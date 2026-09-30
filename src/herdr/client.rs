//! Requests and subscriptions over the session socket. Herdr closes a request
//! connection after its one response, so every call opens its own.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{Instant, timeout_at};

use super::{Agent, Event, HerdrError, Pane, PaneId, parse_event};

pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// A client of one session's socket. The ticker makes it before the
/// session is found and fills the socket in later; until then every request
/// is `NotSent`, so callers run as while Herdr is down.
#[derive(Debug, Clone, Default)]
pub struct Client {
    socket: Arc<OnceLock<PathBuf>>,
}

/// `session.snapshot`: the complete view a decision reads. Entries that do
/// not parse are left out and counted in `skipped`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub version: String,
    pub protocol: u32,
    pub panes: BTreeMap<PaneId, Pane>,
    pub agents: Vec<Agent>,
    pub skipped: usize,
}

/// An `events.subscribe` connection after its acknowledgement. Herdr closes
/// a connection that subscribes twice, so each subscription owns one.
pub struct Subscription {
    lines: Lines<BufReader<OwnedReadHalf>>,
    // Dropping the write half would half-close the connection.
    _write: OwnedWriteHalf,
}

fn request_line(method: &str, params: Value) -> String {
    let id = format!("hla-{}", uuid::Uuid::new_v4().simple());
    let mut line = json!({"id": id, "method": method, "params": params}).to_string();
    line.push('\n');
    line
}

fn parse_response<R: DeserializeOwned>(line: &str) -> Result<R, HerdrError> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| HerdrError::Protocol(format!("{e}: {line}")))?;
    if let Some(error) = value.get("error") {
        let text = |key: &str| {
            error
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        return Err(HerdrError::Api {
            code: text("code"),
            message: text("message"),
        });
    }
    let result = value
        .get("result")
        .cloned()
        .ok_or_else(|| HerdrError::Protocol(format!("no result: {line}")))?;
    serde_json::from_value(result).map_err(|e| HerdrError::Protocol(format!("{e}: {line}")))
}

fn unknown(error: impl std::fmt::Display) -> HerdrError {
    HerdrError::OutcomeUnknown(error.to_string())
}

/// Keeps the entries of `values` that parse as `T`, counting the others.
fn lenient<T: DeserializeOwned>(values: Vec<Value>, skipped: &mut usize) -> Vec<T> {
    values
        .into_iter()
        .filter_map(|v| {
            let parsed = serde_json::from_value(v).ok();
            *skipped += usize::from(parsed.is_none());
            parsed
        })
        .collect()
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        let client = Self::default();
        client.set_socket(socket.into());
        client
    }

    /// Fills in the socket of a client made without one; the first call wins.
    pub fn set_socket(&self, socket: PathBuf) {
        let _ = self.socket.set(socket);
    }

    /// Connects and writes one request, then reads its first line, all
    /// before `deadline`.
    async fn open(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<(String, Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf), HerdrError> {
        let socket = self
            .socket
            .get()
            .ok_or_else(|| HerdrError::NotSent("the Herdr session was not found yet".into()))?;
        let stream = timeout_at(deadline, UnixStream::connect(socket))
            .await
            .map_err(|_| HerdrError::NotSent("connecting timed out".into()))?
            .map_err(|e| HerdrError::NotSent(e.to_string()))?;
        let (read, mut write) = stream.into_split();
        timeout_at(
            deadline,
            write.write_all(request_line(method, params).as_bytes()),
        )
        .await
        .map_err(|_| unknown("writing timed out"))?
        .map_err(unknown)?;
        let mut lines = BufReader::new(read).lines();
        let line = timeout_at(deadline, lines.next_line())
            .await
            .map_err(|_| unknown("no answer in time"))?
            .map_err(unknown)?
            .ok_or_else(|| unknown("the connection closed without an answer"))?;
        Ok((line, lines, write))
    }

    pub async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<R, HerdrError> {
        self.call_with_timeout(method, params, DEFAULT_TIMEOUT)
            .await
    }

    pub async fn call_with_timeout<R: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<R, HerdrError> {
        let (line, _, _) = self.open(method, params, Instant::now() + timeout).await?;
        parse_response(&line)
    }

    /// Opens `events.subscribe` on a new connection and waits for its
    /// acknowledgement, so no event pushed after this returns is missed.
    pub async fn subscribe(&self, subscriptions: Vec<Value>) -> Result<Subscription, HerdrError> {
        let params = json!({"subscriptions": subscriptions});
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        let (line, lines, write) = self.open("events.subscribe", params, deadline).await?;
        let ack: Value = parse_response(&line)?;
        if ack["type"] != "subscription_started" {
            return Err(HerdrError::Protocol(format!(
                "not an acknowledgement: {ack}"
            )));
        }
        Ok(Subscription {
            lines,
            _write: write,
        })
    }

    pub async fn snapshot(&self) -> Result<Snapshot, HerdrError> {
        #[derive(Deserialize)]
        struct Answer {
            snapshot: Raw,
        }
        #[derive(Deserialize)]
        struct Raw {
            version: String,
            protocol: u32,
            panes: Vec<Value>,
            agents: Vec<Value>,
        }
        let Answer { snapshot } = self.call("session.snapshot", json!({})).await?;
        let mut skipped = 0;
        let panes = lenient::<Pane>(snapshot.panes, &mut skipped)
            .into_iter()
            .map(|pane| (pane.id.clone(), pane))
            .collect();
        let agents = lenient(snapshot.agents, &mut skipped);
        Ok(Snapshot {
            version: snapshot.version,
            protocol: snapshot.protocol,
            panes,
            agents,
            skipped,
        })
    }

    /// Only the snapshot's `version`, for telling an old Herdr whose snapshot
    /// no longer parses.
    pub async fn version(&self) -> Result<String, HerdrError> {
        #[derive(Deserialize)]
        struct Answer {
            snapshot: Version,
        }
        #[derive(Deserialize)]
        struct Version {
            version: String,
        }
        let Answer { snapshot } = self.call("session.snapshot", json!({})).await?;
        Ok(snapshot.version)
    }
}

impl Subscription {
    /// The next pushed event; `Ok(None)` when Herdr closed the connection. A
    /// line that is not an event is a `Protocol` error and the subscription
    /// stays usable.
    pub async fn next(&mut self) -> Result<Option<Event>, HerdrError> {
        // `next_line` is cancel safe, so this can sit in a `select!`.
        match self.lines.next_line().await.map_err(unknown)? {
            Some(line) => parse_event(&line).map(Some),
            None => Ok(None),
        }
    }
}

/// The socket of the session a Herdr action or pane runs in
/// (`HERDR_SOCKET_PATH`), else of `session`.
pub async fn invoking_socket(env: &crate::paths::Env, session: Option<&str>) -> Result<PathBuf> {
    match env.var("HERDR_SOCKET_PATH").filter(|s| !s.is_empty()) {
        Some(socket) => Ok(socket.into()),
        None => session_socket(&env.herdr_bin(), session).await,
    }
}

/// Asks `herdr session list --json` for a session's socket path.
pub async fn session_socket(herdr_bin: &str, session: Option<&str>) -> Result<PathBuf> {
    let output = tokio::time::timeout(
        DEFAULT_TIMEOUT,
        tokio::process::Command::new(herdr_bin)
            .args(["session", "list", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("`herdr session list` did not finish in time")?
    .with_context(|| format!("could not run {herdr_bin}"))?;
    if !output.status.success() {
        bail!(
            "`herdr session list` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    socket_in_list(&output.stdout, session)
}

fn socket_in_list(json: &[u8], session: Option<&str>) -> Result<PathBuf> {
    #[derive(Deserialize)]
    struct List {
        sessions: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        name: String,
        #[serde(default)]
        default: bool,
        socket_path: PathBuf,
    }
    let list: List =
        serde_json::from_slice(json).context("`herdr session list --json` is not a list")?;
    list.sessions
        .into_iter()
        .find(|s| match session {
            Some(name) => s.name == name,
            None => s.default,
        })
        .map(|s| s.socket_path)
        .with_context(|| match session {
            Some(name) => format!("there is no Herdr session `{name}`"),
            None => "Herdr lists no default session".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::net::UnixListener;

    use super::*;
    use crate::herdr::fake::{FakeHerdrServer, agent_json, pane_json};
    use crate::herdr::{AgentStatus, Herdr};

    #[test]
    fn a_session_socket_comes_from_the_session_list() {
        let list = br#"{"sessions":[
            {"default":true,"name":"default","running":true,"socket_path":"/h/herdr.sock"},
            {"default":false,"name":"work","running":true,"socket_path":"/h/sessions/work/herdr.sock"}]}"#;
        assert_eq!(
            socket_in_list(list, None).unwrap(),
            Path::new("/h/herdr.sock")
        );
        assert_eq!(
            socket_in_list(list, Some("work")).unwrap(),
            Path::new("/h/sessions/work/herdr.sock")
        );
        assert_eq!(
            socket_in_list(list, Some("other")).unwrap_err().to_string(),
            "there is no Herdr session `other`"
        );
    }

    #[tokio::test]
    async fn an_error_response_becomes_an_api_error() {
        let fake = FakeHerdrServer::start().await;
        fake.fail("session.snapshot", "server_busy", "try again later");
        let error = fake.client().snapshot().await.unwrap_err();
        assert_eq!(
            error,
            HerdrError::Api {
                code: "server_busy".into(),
                message: "try again later".into(),
            }
        );
    }

    #[tokio::test]
    async fn a_client_is_not_sent_until_its_session_is_found() {
        let fake = FakeHerdrServer::start().await;
        let client = Client::default();
        let error = client.snapshot().await.unwrap_err();
        assert_eq!(
            error,
            HerdrError::NotSent("the Herdr session was not found yet".into())
        );
        client.clone().set_socket(fake.socket.clone());
        assert_eq!(client.snapshot().await.unwrap().panes.len(), 0);
        assert_eq!(
            fake.requests().len(),
            1,
            "only the request after the socket was set"
        );
    }

    #[tokio::test]
    async fn a_failed_connect_is_not_sent_and_a_hang_up_is_outcome_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let client = Client::new(&socket);
        let error = client.notification_show("t", "b").await.unwrap_err();
        assert!(matches!(error, HerdrError::NotSent(_)), "{error:?}");

        // A server that reads the whole request and closes without answering.
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            lines.next_line().await.unwrap().unwrap()
        });
        let error = client.notification_show("t", "b").await.unwrap_err();
        assert_eq!(
            error,
            HerdrError::OutcomeUnknown("the connection closed without an answer".into())
        );
        let request: Value = serde_json::from_str(&server.await.unwrap()).unwrap();
        assert_eq!(request["method"], "notification.show");
    }

    #[tokio::test]
    async fn a_call_times_out_as_outcome_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
            drop(stream);
        });
        let error = Client::new(&socket)
            .call_with_timeout::<Value>("agent.start", json!({}), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            HerdrError::OutcomeUnknown("no answer in time".into())
        );
    }

    #[tokio::test]
    async fn a_malformed_snapshot_entry_is_skipped() {
        let fake = FakeHerdrServer::start().await;
        let mut broken = agent_json("w1:p2", "codex", "idle", None);
        broken.as_object_mut().unwrap().remove("terminal_id");
        fake.set_snapshot(
            vec![
                pane_json("w1:p1", "/a"),
                pane_json("w1:p2", "/b"),
                json!({"pane_id": 3}),
            ],
            vec![
                agent_json("w1:p1", "claude", "blocked", Some("coordinator")),
                broken,
            ],
        );
        let snapshot = fake.client().snapshot().await.unwrap();
        assert_eq!(snapshot.skipped, 2);
        assert_eq!(
            snapshot
                .panes
                .keys()
                .map(|p| p.0.as_str())
                .collect::<Vec<_>>(),
            ["w1:p1", "w1:p2"]
        );
        assert_eq!(
            snapshot.panes[&PaneId("w1:p2".into())].cwd.as_deref(),
            Some("/b")
        );
        let [agent] = snapshot.agents.as_slice() else {
            panic!("{:?}", snapshot.agents);
        };
        assert_eq!(agent.pane, PaneId("w1:p1".into()));
        assert_eq!(agent.name.as_deref(), Some("coordinator"));
        assert_eq!(agent.status, AgentStatus::Blocked);
        assert_eq!(
            (snapshot.version.as_str(), snapshot.protocol),
            ("0.9.1", 22)
        );
    }

    #[tokio::test]
    async fn an_old_snapshot_still_gives_its_version() {
        let fake = FakeHerdrServer::start().await;
        fake.set_raw_snapshot(json!({"version": "0.8.0", "protocol": 20, "panes": {}}));
        let client = fake.client();
        assert!(matches!(
            client.snapshot().await,
            Err(HerdrError::Protocol(_))
        ));
        assert_eq!(client.version().await.unwrap(), "0.8.0");
    }
}
