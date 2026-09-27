//! A task that turns Herdr's pushed events into wakes. It keeps no view of
//! the session: whoever is woken reads a fresh `session.snapshot`, which is
//! newer than every request made before it.

use std::collections::BTreeSet;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{Client, HerdrError, PaneId, Subscription};

const MIN_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// A connection that stayed up this long resets the reconnect backoff.
const STABLE: Duration = Duration::from_secs(30);

// A new workspace also emits `pane_created`, so workspace created and
// updated events add nothing.
const GLOBAL_EVENTS: [&str; 9] = [
    "pane.created",
    "pane.updated",
    "pane.closed",
    "pane.exited",
    "pane.agent_detected",
    "pane.moved",
    "tab.created",
    "tab.closed",
    "workspace.closed",
];

/// What the wake task tells its readers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Whether the global subscription is up.
    pub connected: bool,
    /// When `connected` last changed.
    pub since: Timestamp,
    /// Grows on every pushed event and after every status subscription
    /// replacement. Readers only need to know that it changed.
    pub wakes: u64,
    /// The last error seen, including skipped lines; not cleared on success.
    /// Setting it notifies receivers without growing `wakes`.
    pub last_error: Option<String>,
}

/// Starts following the session behind `client`. The task ends when every
/// receiver is dropped.
pub fn wake(client: Client) -> watch::Receiver<Link> {
    let (tx, rx) = watch::channel(Link {
        connected: false,
        since: Timestamp::now(),
        wakes: 0,
        last_error: None,
    });
    tokio::spawn(run(client, tx));
    rx
}

async fn run(client: Client, tx: watch::Sender<Link>) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let mut connection = Connection {
            client: &client,
            tx: &tx,
            up_since: None,
            status: None,
            followed: None,
            retry: None,
            retry_backoff: MIN_BACKOFF,
        };
        let error = match connection.run().await {
            Ok(()) => return,
            Err(error) => error,
        };
        if connection.up_since.is_some_and(|at| at.elapsed() >= STABLE) {
            backoff = MIN_BACKOFF;
        }
        tx.send_modify(|link| {
            link.last_error = Some(error.to_string());
            if link.connected {
                link.connected = false;
                link.since = Timestamp::now();
            }
        });
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            () = tx.closed() => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

struct Connection<'a> {
    client: &'a Client,
    tx: &'a watch::Sender<Link>,
    up_since: Option<Instant>,
    status: Option<Subscription>,
    /// The panes `status` covers; `None` while no subscription is in place.
    followed: Option<BTreeSet<PaneId>>,
    /// When to retry a failed snapshot or status subscription.
    retry: Option<Instant>,
    retry_backoff: Duration,
}

enum Step {
    Global(Result<Option<super::Event>, HerdrError>),
    Status(Result<Option<super::Event>, HerdrError>),
    Retry,
    Unwatched,
}

async fn next_status(
    status: &mut Option<Subscription>,
) -> Result<Option<super::Event>, HerdrError> {
    match status {
        Some(subscription) => subscription.next().await,
        None => std::future::pending().await,
    }
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

impl Connection<'_> {
    /// One connected period. `Ok` when every receiver is gone, else the error
    /// that ended the global subscription.
    async fn run(&mut self) -> Result<(), HerdrError> {
        let subscriptions: Vec<Value> = GLOBAL_EVENTS.iter().map(|t| json!({"type": t})).collect();
        let mut global = self.client.subscribe(subscriptions).await?;
        self.up_since = Some(Instant::now());
        self.tx.send_modify(|link| {
            link.connected = true;
            link.since = Timestamp::now();
            link.wakes += 1;
        });
        self.refresh().await;
        loop {
            let step = tokio::select! {
                event = global.next() => Step::Global(event),
                event = next_status(&mut self.status) => Step::Status(event),
                () = until(self.retry) => Step::Retry,
                () = self.tx.closed() => Step::Unwatched,
            };
            match step {
                Step::Global(Ok(Some(event))) => {
                    self.wake();
                    if event.changes_panes() {
                        self.refresh().await;
                    }
                }
                Step::Global(Ok(None)) => {
                    return Err(HerdrError::OutcomeUnknown(
                        "Herdr closed the event subscription".into(),
                    ));
                }
                Step::Global(Err(HerdrError::Protocol(detail)))
                | Step::Status(Err(HerdrError::Protocol(detail))) => self.note(detail),
                Step::Global(Err(error)) => return Err(error),
                Step::Status(Ok(Some(_))) => self.wake(),
                Step::Status(ended) => {
                    self.status = None;
                    self.followed = None;
                    self.failed(match ended {
                        Err(error) => error,
                        Ok(_) => HerdrError::OutcomeUnknown(
                            "Herdr closed the status subscription".into(),
                        ),
                    });
                }
                Step::Retry => self.refresh().await,
                Step::Unwatched => return Ok(()),
            }
        }
    }

    /// Reads the pane set and, when it changed, replaces the status
    /// subscription with one for the new set.
    async fn refresh(&mut self) {
        let panes: BTreeSet<PaneId> = match self.client.snapshot().await {
            Ok(snapshot) => snapshot.panes.into_keys().collect(),
            Err(error) => return self.failed(error),
        };
        if self.followed.as_ref() != Some(&panes) {
            let status = if panes.is_empty() {
                None
            } else {
                let subscriptions = panes
                    .iter()
                    .map(|p| json!({"type": "pane.agent_status_changed", "pane_id": p}))
                    .collect();
                // A refusal (a pane closed meanwhile) keeps the old
                // subscription; the retry reads the pane set again.
                match self.client.subscribe(subscriptions).await {
                    Ok(subscription) => Some(subscription),
                    Err(error) => return self.failed(error),
                }
            };
            // The old subscription goes only after the new one's ack. A
            // status event it received but nobody read is lost with it, so
            // readers are woken to see that status in their next snapshot.
            self.status = status;
            self.followed = Some(panes);
            self.wake();
        }
        self.retry = None;
        self.retry_backoff = MIN_BACKOFF;
    }

    fn wake(&self) {
        self.tx.send_modify(|link| link.wakes += 1);
    }

    fn note(&self, detail: String) {
        self.tx.send_modify(|link| link.last_error = Some(detail));
    }

    fn failed(&mut self, error: HerdrError) {
        self.note(error.to_string());
        self.retry = Some(Instant::now() + self.retry_backoff);
        self.retry_backoff = (self.retry_backoff * 2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::AgentStatus;
    use crate::herdr::fake::{FakeHerdrServer, agent_json, pane_json};

    async fn wait(
        rx: &mut watch::Receiver<Link>,
        what: &str,
        f: impl FnMut(&Link) -> bool,
    ) -> Link {
        let link = tokio::time::timeout(Duration::from_secs(1), rx.wait_for(f))
            .await
            .map(|link| link.unwrap().clone());
        link.unwrap_or_else(|_| panic!("timed out waiting for {what}: {:?}", *rx.borrow()))
    }

    fn status_for(pane: &str) -> impl FnMut(&Value) -> bool {
        move |r: &Value| {
            r["method"] == "events.subscribe"
                && r["params"]["subscriptions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["pane_id"] == pane)
        }
    }

    fn status_event(pane: &str, status: &str) -> Value {
        json!({"pane_id": pane, "workspace_id": "w1", "agent": "claude", "agent_status": status})
    }

    #[tokio::test]
    async fn a_pane_created_during_the_bootstrap_snapshot_is_followed() {
        let fake = FakeHerdrServer::start().await;
        fake.set_snapshot(vec![pane_json("w1:p1", "/a")], vec![]);
        let release = fake.hold_snapshot();
        let mut rx = wake(fake.client());
        fake.wait_request(|r| r["method"] == "session.snapshot")
            .await;
        // Written to the global subscription before the snapshot, which
        // still answers the old pane set, is released.
        fake.push("pane_created", json!({"pane": pane_json("w1:p2", "/b")}))
            .await;
        fake.set_snapshot(
            vec![pane_json("w1:p1", "/a"), pane_json("w1:p2", "/b")],
            vec![],
        );
        release.send(()).unwrap();
        fake.wait_request(status_for("w1:p2")).await;
        let link = wait(&mut rx, "connected", |l| l.connected).await;
        assert_eq!(link.last_error, None);
    }

    #[tokio::test]
    async fn a_new_pane_gets_a_status_subscription() {
        let fake = FakeHerdrServer::start().await;
        fake.set_snapshot(vec![pane_json("w1:p1", "/a")], vec![]);
        let mut rx = wake(fake.client());
        fake.wait_request(status_for("w1:p1")).await;
        fake.set_snapshot(
            vec![pane_json("w1:p1", "/a"), pane_json("w1:p2", "/b")],
            vec![],
        );
        fake.push("pane.created", json!({"pane": pane_json("w1:p2", "/b")}))
            .await;
        fake.wait_request(status_for("w1:p2")).await;
        // The replacement wakes once its ack is read; wait for that first.
        let before = wait(&mut rx, "the replacement", |l| l.wakes >= 4)
            .await
            .wakes;
        fake.push(
            "pane.agent_status_changed",
            status_event("w1:p2", "working"),
        )
        .await;
        wait(&mut rx, "a status wake", |l| l.wakes > before).await;
    }

    #[tokio::test]
    async fn a_status_lost_during_a_replacement_wakes_after_the_new_ack() {
        let fake = FakeHerdrServer::start().await;
        fake.set_snapshot(
            vec![pane_json("w1:p1", "/a")],
            vec![agent_json("w1:p1", "claude", "working", Some("c"))],
        );
        let mut rx = wake(fake.client());
        fake.wait_request(status_for("w1:p1")).await;
        wait(&mut rx, "the first status subscription", |l| l.wakes == 2).await;

        let release = fake.hold_status();
        fake.set_snapshot(
            vec![pane_json("w1:p1", "/a"), pane_json("w1:p2", "/b")],
            vec![agent_json("w1:p1", "claude", "working", Some("c"))],
        );
        fake.push("pane_created", json!({"pane": pane_json("w1:p2", "/b")}))
            .await;
        fake.wait_request(status_for("w1:p2")).await;
        // The task now waits for the held ack and reads nothing, so this
        // status reaches only the old subscription and is never read.
        let before = rx.borrow().wakes;
        assert_eq!(before, 3);
        fake.set_snapshot(
            vec![pane_json("w1:p1", "/a"), pane_json("w1:p2", "/b")],
            vec![agent_json("w1:p1", "claude", "done", Some("c"))],
        );
        fake.push("pane.agent_status_changed", status_event("w1:p1", "done"))
            .await;
        assert_eq!(rx.borrow().wakes, before);
        release.send(()).unwrap();
        wait(&mut rx, "the post-ack wake", |l| l.wakes > before).await;
        let snapshot = fake.client().snapshot().await.unwrap();
        assert_eq!(snapshot.agents[0].status, AgentStatus::Done);
    }

    #[tokio::test]
    async fn an_unparseable_event_is_skipped() {
        let fake = FakeHerdrServer::start().await;
        let mut rx = wake(fake.client());
        let link = wait(&mut rx, "connected", |l| l.connected).await;
        fake.push_line("this is not json").await;
        fake.push("workspace_created", json!({"workspace": {}}))
            .await;
        let after = wait(&mut rx, "the next event", |l| l.wakes > link.wakes).await;
        assert!(after.connected);
        assert_eq!(after.since, link.since);
        assert!(
            after
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("this is not json")),
            "{after:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_status_subscription_is_retried_without_disconnecting() {
        let fake = FakeHerdrServer::start().await;
        fake.set_snapshot(vec![pane_json("w1:p1", "/a")], vec![]);
        fake.refuse_status(Some("pane_not_found"));
        let mut rx = wake(fake.client());
        fake.wait_request(status_for("w1:p1")).await;
        let link = wait(&mut rx, "the refusal", |l| l.last_error.is_some()).await;
        assert!(link.connected);
        assert_eq!(
            link.last_error.as_deref(),
            Some("Herdr refused the request (pane_not_found): pane not found")
        );
        fake.refuse_status(None);
        let wakes = link.wakes;
        let after = wait(&mut rx, "the retried subscription", |l| l.wakes > wakes).await;
        assert!(after.connected);
        assert_eq!(after.since, link.since);
        fake.push("pane.agent_status_changed", status_event("w1:p1", "idle"))
            .await;
        wait(&mut rx, "a status wake", |l| l.wakes > after.wakes).await;
    }

    #[tokio::test]
    async fn a_restart_disconnects_and_reconnects() {
        let mut fake = FakeHerdrServer::start().await;
        fake.set_snapshot(vec![pane_json("w1:p1", "/a")], vec![]);
        let mut rx = wake(fake.client());
        let up = wait(&mut rx, "connected", |l| l.connected).await;

        fake.stop().await;
        let down = wait(&mut rx, "disconnected", |l| !l.connected).await;
        assert!(down.since > up.since);
        assert_eq!(
            down.last_error.as_deref(),
            Some("Herdr did not answer the request: Herdr closed the event subscription")
        );

        fake.bind().await;
        let back = wait(&mut rx, "reconnected", |l| l.connected).await;
        assert!(back.since > down.since);
        assert!(back.wakes > up.wakes);
    }
}
