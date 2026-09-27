//! A stand-in Herdr server for tests that behaves like the measured one: one
//! response per request connection, then close; a subscription answers its
//! acknowledgement and then pushes events; a second subscribe on a
//! subscription connection closes it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::Client;

pub fn pane_json(pane: &str, cwd: &str) -> Value {
    let workspace = pane.split(':').next().unwrap();
    json!({
        "pane_id": pane, "terminal_id": format!("term-{pane}"), "workspace_id": workspace,
        "tab_id": format!("{workspace}:t1"), "focused": false, "cwd": cwd,
        "foreground_cwd": cwd, "agent_status": "unknown", "revision": 0
    })
}

pub fn agent_json(pane: &str, agent: &str, status: &str, name: Option<&str>) -> Value {
    let mut value = pane_json(pane, "/");
    value["agent"] = json!(agent);
    value["agent_status"] = json!(status);
    value["name"] = json!(name);
    value
}

/// A line to write and a signal that it was written.
type Push = (String, oneshot::Sender<()>);

#[derive(Default)]
struct State {
    snapshot: Value,
    errors: BTreeMap<String, (String, String)>,
    /// Results for methods other than the snapshot and notifications.
    answers: BTreeMap<String, Value>,
    /// Refuses subscriptions that name panes with this code.
    refuse_status: Option<String>,
    hold_snapshot: Option<oneshot::Receiver<()>>,
    hold_status: Option<oneshot::Receiver<()>>,
    subscribers: Vec<(Vec<Value>, mpsc::UnboundedSender<Push>)>,
}

struct Shared {
    state: Mutex<State>,
    requests: watch::Sender<Vec<Value>>,
}

pub struct FakeHerdrServer {
    _dir: tempfile::TempDir,
    pub socket: PathBuf,
    shared: Arc<Shared>,
    server: Option<JoinHandle<()>>,
}

impl FakeHerdrServer {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = Self {
            socket: dir.path().join("herdr.sock"),
            _dir: dir,
            shared: Arc::new(Shared {
                state: Mutex::default(),
                requests: watch::Sender::new(Vec::new()),
            }),
            server: None,
        };
        fake.set_snapshot(Vec::new(), Vec::new());
        fake.bind().await;
        fake
    }

    pub fn client(&self) -> Client {
        Client::new(&self.socket)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap()
    }

    pub fn set_snapshot(&self, panes: Vec<Value>, agents: Vec<Value>) {
        self.state().snapshot = json!({
            "version": "0.9.1", "protocol": 22, "workspaces": [], "tabs": [],
            "layouts": [], "panes": panes, "agents": agents
        });
    }

    pub fn set_raw_snapshot(&self, snapshot: Value) {
        self.state().snapshot = snapshot;
    }

    pub fn fail(&self, method: &str, code: &str, message: &str) {
        self.state()
            .errors
            .insert(method.into(), (code.into(), message.into()));
    }

    pub fn answer(&self, method: &str, result: Value) {
        self.state().answers.insert(method.into(), result);
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<Value> {
        self.shared.requests.borrow().clone()
    }

    pub fn refuse_status(&self, code: Option<&str>) {
        self.state().refuse_status = code.map(Into::into);
    }

    /// Holds the next snapshot answer, with the content it had when asked,
    /// until the returned sender fires.
    pub fn hold_snapshot(&self) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        self.state().hold_snapshot = Some(rx);
        tx
    }

    /// Holds the next subscription that names panes: it is neither
    /// registered nor acknowledged until the returned sender fires.
    pub fn hold_status(&self) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        self.state().hold_status = Some(rx);
        tx
    }

    /// Waits until a request matching `f` has arrived.
    pub async fn wait_request(&self, mut f: impl FnMut(&Value) -> bool) {
        let mut rx = self.shared.requests.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rx.wait_for(|requests| requests.iter().any(&mut f)),
        )
        .await
        .expect("the request never arrived")
        .unwrap();
    }

    /// Pushes an event to every subscription that asked for it and returns
    /// once it was written to each.
    pub async fn push(&self, event: &str, data: Value) {
        let name = event.replace('.', "_");
        let wanted = |subscriptions: &[Value]| {
            subscriptions.iter().any(|s| {
                s["type"]
                    .as_str()
                    .is_some_and(|t| t.replace('.', "_") == name)
                    && s.get("pane_id").is_none_or(|p| *p == data["pane_id"])
            })
        };
        let line = json!({"event": event, "data": data}).to_string();
        self.send(&line, wanted).await;
    }

    /// Writes a raw line to every subscription.
    pub async fn push_line(&self, line: &str) {
        self.send(line, |_| true).await;
    }

    async fn send(&self, line: &str, wanted: impl Fn(&[Value]) -> bool) {
        let written: Vec<oneshot::Receiver<()>> = {
            let mut state = self.state();
            let mut written = Vec::new();
            state.subscribers.retain(|(subscriptions, tx)| {
                if !wanted(subscriptions) {
                    return true;
                }
                let (done, rx) = oneshot::channel();
                written.push(rx);
                tx.send((format!("{line}\n"), done)).is_ok()
            });
            written
        };
        for rx in written {
            let _ = rx.await;
        }
    }

    /// Like `herdr server stop`: every connection ends and the socket goes away.
    pub async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
        self.state().subscribers.clear();
        let _ = std::fs::remove_file(&self.socket);
    }

    pub async fn bind(&mut self) {
        let listener = UnixListener::bind(&self.socket).unwrap();
        self.server = Some(tokio::spawn(serve(listener, self.shared.clone())));
    }
}

impl Drop for FakeHerdrServer {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

async fn serve(listener: UnixListener, shared: Arc<Shared>) {
    // Dropping the set when this task is aborted ends every connection.
    let mut connections = JoinSet::new();
    while let Ok((stream, _)) = listener.accept().await {
        connections.spawn(connection(stream, shared.clone()));
    }
}

async fn connection(stream: UnixStream, shared: Arc<Shared>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let request: Value = serde_json::from_str(&line).unwrap();
    let log = |request: &Value| shared.requests.send_modify(|r| r.push(request.clone()));
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or_default().to_string();
    let error = |code: &str, message: String| json!({"id": id, "error": {"code": code, "message": message}});
    if method == "events.subscribe" {
        let subscriptions = request["params"]["subscriptions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let names_panes = subscriptions.iter().any(|s| s.get("pane_id").is_some());
        let (refused, hold) = {
            let mut state = shared.state.lock().unwrap();
            let refused = state.refuse_status.clone().filter(|_| names_panes);
            let hold = if names_panes {
                state.hold_status.take()
            } else {
                None
            };
            (refused, hold)
        };
        log(&request);
        if let Some(code) = refused {
            let response = error(&code, "pane not found".into());
            let _ = write.write_all(format!("{response}\n").as_bytes()).await;
            return;
        }
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Push>();
        // Registered before the ack, so a test that saw the ack can push.
        shared
            .state
            .lock()
            .unwrap()
            .subscribers
            .push((subscriptions, tx));
        let ack = json!({"id": id, "result": {"type": "subscription_started"}});
        if write
            .write_all(format!("{ack}\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
        loop {
            tokio::select! {
                push = rx.recv() => match push {
                    Some((line, done)) if write.write_all(line.as_bytes()).await.is_ok() => {
                        let _ = done.send(());
                    }
                    _ => return,
                },
                // A second subscribe, or the client going away, ends it.
                _ = lines.next_line() => return,
            }
        }
    }
    log(&request);
    let (response, hold) = {
        let mut state = shared.state.lock().unwrap();
        let ok = |result: Value| json!({"id": id, "result": result});
        match (state.errors.get(&method), method.as_str()) {
            (Some((code, message)), _) => (error(code, message.clone()), None),
            (None, "session.snapshot") => (
                ok(json!({"type": "session_snapshot", "snapshot": state.snapshot})),
                state.hold_snapshot.take(),
            ),
            (None, "notification.show") => (
                ok(json!({"type": "notification_show", "shown": true, "reason": "shown"})),
                None,
            ),
            (None, other) => match state.answers.get(other) {
                Some(result) => (ok(result.clone()), None),
                None => (
                    error("unknown_method", format!("unknown method {other}")),
                    None,
                ),
            },
        }
    };
    if let Some(hold) = hold {
        let _ = hold.await;
    }
    let _ = write.write_all(format!("{response}\n").as_bytes()).await;
}
