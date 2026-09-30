//! Herdr over its unix socket: a request client, `session.snapshot` as the
//! view every decision reads, and a task that turns pushed events into wakes.

mod client;
#[cfg(test)]
mod fake;
#[cfg(test)]
mod memory;
mod requests;
mod wake;

pub use client::{Client, Snapshot, Subscription, invoking_socket, session_socket};
#[cfg(test)]
pub use memory::FakeHerdr;
pub use requests::{Herdr, Placed};
pub use wake::{Link, wake};

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The oldest Herdr this client was written against (protocol 22).
pub const MIN_VERSION: &str = "0.9.1";

/// Whether the first `X.Y.Z` of `version` is at least `minimum`'s, so
/// `0.9.2-preview.3` counts as 0.9.2. An unreadable version is not.
pub fn version_at_least(version: &str, minimum: &str) -> bool {
    fn triple(text: &str) -> Option<(u64, u64, u64)> {
        let end = text
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(text.len());
        let mut parts = text[..end].split('.').map(|p| p.parse::<u64>().ok());
        Some((parts.next()??, parts.next()??, parts.next()??))
    }
    match (triple(version), triple(minimum)) {
        (Some(version), Some(minimum)) => version >= minimum,
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

impl From<&str> for PaneId {
    fn from(id: &str) -> Self {
        PaneId(id.to_string())
    }
}

impl From<&str> for WorkspaceId {
    fn from(id: &str) -> Self {
        WorkspaceId(id.to_string())
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }

    /// Ready for input: Herdr shows `idle` after `working` as `done`.
    pub fn is_idle(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }
}

impl<'de> Deserialize<'de> for AgentStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

/// An entry of the snapshot's `panes`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pane {
    #[serde(rename = "pane_id")]
    pub id: PaneId,
    #[serde(rename = "workspace_id")]
    pub workspace: WorkspaceId,
    #[serde(rename = "tab_id")]
    pub tab: String,
    #[serde(rename = "terminal_id")]
    pub terminal: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// An entry of the snapshot's `agents`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Agent {
    #[serde(rename = "pane_id")]
    pub pane: PaneId,
    /// Herdr's `agent`, for example `claude`.
    #[serde(rename = "agent", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "agent_status")]
    pub status: AgentStatus,
    /// `agent_session.value`.
    #[serde(rename = "agent_session", default, deserialize_with = "session_value")]
    pub session: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(rename = "terminal_id")]
    pub terminal: String,
    #[serde(default)]
    pub interactive_ready: bool,
    #[serde(default)]
    pub launch_pending: bool,
    /// Grows with every state change, so an equal status with a higher
    /// sequence is a new episode.
    #[serde(default)]
    pub state_change_seq: u64,
    /// Herdr's `state_labels`, status to label.
    #[serde(default)]
    pub state_labels: BTreeMap<String, String>,
}

fn session_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    struct Session {
        value: String,
    }
    Ok(Option::<Session>::deserialize(deserializer)?.map(|s| s.value))
}

/// A pushed event, read only far enough to know whether the pane set may
/// have changed. The data is not kept: decisions read a fresh snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The event name in the underscore spelling, for example `pane_closed`.
    pub name: String,
}

impl Event {
    /// Whether a pane may have appeared, gone, or changed its id.
    pub fn changes_panes(&self) -> bool {
        matches!(
            self.name.as_str(),
            "pane_created" | "pane_closed" | "pane_moved" | "tab_closed" | "workspace_closed"
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrError {
    /// The request never reached Herdr (socket missing, connect refused or
    /// timed out), so retrying it is safe.
    NotSent(String),
    /// The connection ended or timed out after the request was written:
    /// Herdr may have acted on it.
    OutcomeUnknown(String),
    /// A line that is not JSON or not of the expected shape.
    Protocol(String),
    /// Herdr answered with an error.
    Api { code: String, message: String },
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(detail) => write!(f, "could not reach the Herdr socket: {detail}"),
            Self::OutcomeUnknown(detail) => {
                write!(f, "Herdr did not answer the request: {detail}")
            }
            Self::Protocol(detail) => write!(f, "unexpected answer from Herdr: {detail}"),
            Self::Api { code, message } => {
                write!(f, "Herdr refused the request ({code}): {message}")
            }
        }
    }
}

impl std::error::Error for HerdrError {}

/// Parses one pushed `{"event", "data"}` line.
fn parse_event(line: &str) -> Result<Event, HerdrError> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| HerdrError::Protocol(format!("{e}: {line}")))?;
    let name = value
        .get("event")
        .and_then(Value::as_str)
        .ok_or_else(|| HerdrError::Protocol(format!("not an event: {line}")))?;
    // Herdr pushes global events with underscores and subscription events
    // dotted; accept either spelling for every name.
    Ok(Event {
        name: name.replace('.', "_"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_compare_by_their_first_triple() {
        assert!(version_at_least("0.9.1", MIN_VERSION));
        assert!(version_at_least("0.9.2-preview.3", MIN_VERSION));
        assert!(version_at_least("0.10.0", MIN_VERSION));
        assert!(!version_at_least("0.9.0", MIN_VERSION));
        assert!(!version_at_least("0.9", MIN_VERSION));
        assert!(!version_at_least("dev", MIN_VERSION));
    }

    #[test]
    fn events_parse_in_both_spellings() {
        for name in [
            "pane_closed",
            "pane.closed",
            "tab.closed",
            "workspace_closed",
        ] {
            let line = json!({"event": name, "data": {"pane_id": "w1:p1"}}).to_string();
            assert!(parse_event(&line).unwrap().changes_panes(), "{name}");
        }
        let line = json!({"event": "pane.agent_status_changed", "data": {}}).to_string();
        let event = parse_event(&line).unwrap();
        assert_eq!(event.name, "pane_agent_status_changed");
        assert!(!event.changes_panes());
        assert!(matches!(
            parse_event("{\"data\": {}}"),
            Err(HerdrError::Protocol(_))
        ));
        assert!(matches!(parse_event("nope"), Err(HerdrError::Protocol(_))));
    }

    #[test]
    fn an_agent_carries_its_session_labels_and_sequence() {
        let agent: Agent = serde_json::from_value(json!({
            "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "terminal_id": "t-1",
            "cwd": "/src", "agent": "claude", "agent_status": "napping", "name": "coordinator",
            "agent_session": {"agent": "claude", "kind": "id", "source": "x", "value": "s-1"},
            "state_labels": {"working": "Testing"}, "state_change_seq": 7,
            "interactive_ready": true, "launch_pending": false, "focused": false, "revision": 3
        }))
        .unwrap();
        assert_eq!(
            agent,
            Agent {
                pane: PaneId("w1:p1".into()),
                kind: Some("claude".into()),
                name: Some("coordinator".into()),
                status: AgentStatus::Unknown,
                session: Some("s-1".into()),
                cwd: Some("/src".into()),
                foreground_cwd: None,
                terminal: "t-1".into(),
                interactive_ready: true,
                launch_pending: false,
                state_change_seq: 7,
                state_labels: BTreeMap::from([("working".into(), "Testing".into())]),
            }
        );
    }
}
