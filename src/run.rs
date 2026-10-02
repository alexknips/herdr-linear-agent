//! Runs: one per Linear issue. A run is named by its key,
//! `<workspace>/<ISSUE-KEY>` (for example `acme/DATA-1`), since issue keys of
//! two workspaces may be the same. Its folder under
//! `$XDG_STATE_HOME/herdr-linear-agent/runs/<workspace>/<ISSUE-KEY>/` is the
//! coordinator's working directory and holds everything the ticker needs to
//! continue the run after a restart.
//!
//! ```text
//! runs/<workspace>/<ISSUE-KEY>/
//!   AGENTS.md, CLAUDE.md          the coordinator's priming (CLAUDE.md links to AGENTS.md)
//!   issue.md                      the issue snapshot
//!   conversation.md               replies from allowed users
//!   workers/<id>.toml             worker records
//!   workers/<id>.task.md          the coordinator's task and later prompts
//!   workers/<id>.md               the copy of the worker's report
//!   inbox/                        events for the coordinator
//!   .state/                       the run record, the outbox, the lock
//!   .claude/settings.local.json   the coordinator's allow-list
//! ```

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::files::{self, write_atomic};
use crate::herdr::Placed;
use crate::linear::api::{ExternalUrl, IssueDetail};

pub const SUBDIRS: [&str; 6] = [
    "workers",
    "inbox",
    "inbox/done",
    ".state",
    ".state/outbox",
    ".claude",
];

/// A workspace name from the config: a lower-case letter, then lower-case
/// letters, digits or `-`, at most 16 characters. It is part of run keys,
/// agent names and branch names.
pub fn valid_workspace(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 16
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A Linear team key: an upper-case letter, then upper-case letters or
/// digits, at most 16 characters.
pub fn valid_team_key(team: &str) -> bool {
    team.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && team.len() <= 16
        && team
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// A Linear issue key: `TEAM-123`.
pub fn validate_issue_key(key: &str) -> Result<()> {
    let ok = key.split_once('-').is_some_and(|(team, number)| {
        valid_team_key(team)
            && !number.is_empty()
            && number.len() <= 12
            && number.chars().all(|c| c.is_ascii_digit())
    });
    if !ok {
        bail!("`{key}` is not a Linear issue key (expected the form TEAM-123)");
    }
    Ok(())
}

/// The key of the run of `issue` (`TEAM-123`) in `workspace`.
pub fn run_key(workspace: &str, issue: &str) -> String {
    format!("{workspace}/{issue}")
}

/// A run key: `<workspace>/TEAM-123`.
pub fn validate_key(key: &str) -> Result<()> {
    let ok = key.split_once('/').is_some_and(|(workspace, issue)| {
        valid_workspace(workspace) && validate_issue_key(issue).is_ok()
    });
    if !ok {
        bail!("`{key}` is not a run key (expected the form workspace/TEAM-123)");
    }
    Ok(())
}

/// The workspace and the issue key of a valid run key.
pub fn split_key(key: &str) -> (&str, &str) {
    key.split_once('/').unwrap_or(("", key))
}

/// Where a run is in its life.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The issue is delegated and open: the ticker works on it.
    #[default]
    Active,
    /// The delegation was removed: agents were stopped, workspaces kept.
    Detached,
    /// The issue was completed or canceled: agents stopped, workspaces closed.
    Closed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    CoordinatorQuestion,
    RunTimeout,
    CoordinatorLost,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AwaitingReply {
    pub activity_id: String,
    pub asked_at: String,
    pub reason: WaitReason,
}

/// Where an agent (coordinator or worker) is in its life.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    /// Not placed in a pane yet.
    #[default]
    Pending,
    /// Placed; the ticker launches, prompts and watches it.
    Open,
    /// Placing or launching failed; `error` says why.
    Failed,
    /// Its run ended; nothing is sent to it any more.
    Stopped,
}

/// What the ticker keeps about one agent pane. An agent in Herdr is this
/// agent only when pane id, working directory, kind and name agree (a natively
/// resumed agent may have lost its name and is renamed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct AgentRecord {
    pub status: AgentStatus,
    pub error: String,
    pub profile: String,
    pub kind: String,
    pub agent_name: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub cwd: String,
    /// The launch prompt has not been delivered yet.
    pub prompt_pending: bool,
    /// When the launch prompt went out, and Herdr's `state_change_seq` then.
    pub prompted_at: String,
    pub prompted_seq: u64,
    pub launch_attempts: u32,
    /// The agent's native session, for a resume: the id the plugin gave a
    /// Claude start, or the one found for the other kinds.
    pub agent_session: String,
    /// When the agent was last started: its session is the first one begun
    /// after this.
    pub started_at: String,
    /// The next launch resumes `agent_session`.
    pub resume: bool,
    pub last_state: String,
    pub last_state_change: String,
    /// Herdr's `state_change_seq` when `last_state` was seen, so the same
    /// status in a new episode is still a change.
    pub last_state_seq: u64,
    /// When the last unsuccessful placement or start was made; the next one
    /// waits for the spacing.
    pub last_attempt_at: String,
    pub last_group: String,
    /// A "needs someone in the pane" elicitation was sent for the current episode.
    pub blocked_reported: bool,
}

impl AgentRecord {
    /// Open in the pane Herdr placed it in, its launch prompt due.
    pub fn placed(&mut self, placed: &Placed) {
        self.status = AgentStatus::Open;
        self.error.clear();
        self.workspace_id = placed.workspace.0.clone();
        self.tab_id = placed.tab.clone();
        self.pane_id = placed.pane.0.clone();
        self.cwd = placed.cwd.clone();
        self.prompt_pending = true;
        self.launch_attempts = 0;
        self.last_attempt_at.clear();
    }

    /// Pending again, to be placed anew and resumed when it has a session.
    pub fn repend(&mut self) {
        self.status = AgentStatus::Pending;
        self.resume = !self.agent_session.is_empty();
        self.launch_attempts = 0;
        self.last_attempt_at.clear();
    }
}

/// Escape keys the run still owes its agents: a stop or a detach decided
/// while no snapshot showed where they run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Interrupt {
    /// Posts `Stopped <n> agent(s) ...` once the keys went out.
    Stop,
    Detach,
}

/// `.state/run.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct RunRecord {
    /// The config's name for the issue's Linear workspace.
    pub workspace: String,
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub team_key: String,
    /// The issue's label names, for the routing rules.
    pub labels: Vec<String>,
    pub session_id: String,
    pub status: Status,
    pub created: String,
    /// The issue's `updatedAt` when `issue.md` was last written.
    pub issue_updated_at: String,
    /// A hash of the snapshot parts a person edits (not the state).
    pub issue_hash: String,
    /// Where the coordinator profile came from: the routing agent or the
    /// default, with the reason.
    pub routing_source: String,
    pub coordinator: AgentRecord,
    /// Prompts created after this timestamp have not been read yet.
    pub prompt_cursor: String,
    /// When the ticker last sent an activity.
    pub last_activity: String,
    /// `finish` was accepted.
    pub finished: bool,
    pub external_urls: Vec<ExternalUrl>,
    /// When the run timeout was last reset (the start, or a reply to the timeout question).
    pub timeout_since: String,
    /// The run timeout question was asked and not answered yet.
    /// Compatibility input for records written before `awaiting_reply`.
    pub timeout_asked: bool,
    /// The one Linear-native question whose answer can resume the run.
    pub awaiting_reply: Option<AwaitingReply>,
    /// Prevents a cleared, still-queued question from being restored after a
    /// restart while its unknown Linear write is being reconciled.
    #[serde(default)]
    pub cleared_wait_id: String,
    /// Advances only for a newly accepted allowed prompt; it distinguishes
    /// a pending question from one answered while its write was uncertain.
    pub reply_generation: u64,
    /// The coordinator's pane is gone and a resume question was asked.
    pub coordinator_lost: bool,
    /// A person pressed stop: no prompt or heartbeat goes out until they
    /// reply again. Inbox items are still written.
    pub stopped: bool,
    /// The claim's `Picked up <KEY>.` thought is not queued yet. A record
    /// of an older build lacks the field and reads as announced.
    pub announce_pending: bool,
    /// Sent by the next pass that has a snapshot.
    pub interrupt: Option<Interrupt>,
    /// A postmortem the ticker writes next: interim after `finish`, final
    /// when the run closes (which replaces an interim one not written yet).
    pub postmortem_due: Option<crate::postmortem::Stage>,
    /// The workflow state the issue was in when the run closed.
    pub closed_state: String,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub dir: PathBuf,
    pub key: String,
}

/// Held while reading and rewriting anything under `workers/`, `inbox/` or
/// `.state/`. Never held across a Herdr, git or Linear call.
pub struct RunLock {
    _file: File,
}

impl Run {
    fn at(runs_dir: &Path, key: &str) -> Result<Run> {
        validate_key(key)?;
        Ok(Run {
            dir: runs_dir.join(key),
            key: key.to_string(),
        })
    }

    pub fn load(runs_dir: &Path, key: &str) -> Result<Run> {
        let run = Self::at(runs_dir, key)?;
        if !run.record_path().is_file() {
            bail!("there is no run for {key}");
        }
        Ok(run)
    }

    /// Every run folder with a record, sorted by key.
    pub fn list(runs_dir: &Path) -> Vec<Run> {
        let names = |dir: &Path| -> Vec<String> {
            std::fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut runs: Vec<Run> = names(runs_dir)
            .into_iter()
            .flat_map(|workspace| {
                names(&runs_dir.join(&workspace))
                    .into_iter()
                    .map(move |issue| run_key(&workspace, &issue))
            })
            .filter_map(|key| Run::load(runs_dir, &key).ok())
            .collect();
        runs.sort_by(|a, b| a.key.cmp(&b.key));
        runs
    }

    /// Creates the run folder and its first record.
    pub fn create(runs_dir: &Path, record: RunRecord) -> Result<Run> {
        let run = Self::at(runs_dir, &run_key(&record.workspace, &record.identifier))?;
        if run.record_path().exists() {
            bail!("a run for {} already exists", run.key);
        }
        for sub in SUBDIRS {
            std::fs::create_dir_all(run.dir.join(sub))
                .with_context(|| format!("could not create {}", run.dir.join(sub).display()))?;
        }
        files::write_json(&run.record_path(), &record)?;
        Ok(run)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir.join(".state")
    }

    fn record_path(&self) -> PathBuf {
        self.state_dir().join("run.json")
    }

    pub fn issue_md(&self) -> PathBuf {
        self.dir.join("issue.md")
    }

    pub fn conversation_md(&self) -> PathBuf {
        self.dir.join("conversation.md")
    }

    pub fn workers_dir(&self) -> PathBuf {
        self.dir.join("workers")
    }

    /// Where the plugin keeps copies of an agent's transcripts: the
    /// coordinator's under `coordinator`, a worker's under its id.
    pub fn transcripts_dir(&self, agent: &str) -> PathBuf {
        self.state_dir().join("transcripts").join(agent)
    }

    /// The run folder with symbolic links resolved: Herdr reports a pane's
    /// physical working directory, and records compare against it.
    pub fn canonical_dir(&self) -> PathBuf {
        std::fs::canonicalize(&self.dir).unwrap_or_else(|_| self.dir.clone())
    }

    pub fn lock(&self) -> Result<RunLock> {
        let path = self.state_dir().join("lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("run {} is gone ({})", self.key, path.display()))?;
        file.lock()?;
        Ok(RunLock { _file: file })
    }

    pub fn record(&self) -> Result<RunRecord> {
        let path = self.record_path();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let mut record: RunRecord = serde_json::from_str(&text)
            .with_context(|| format!("{} does not parse", path.display()))?;
        if record.awaiting_reply.is_none() {
            if record.timeout_asked {
                record.awaiting_reply = Some(AwaitingReply {
                    activity_id: String::new(),
                    asked_at: record.timeout_since.clone(),
                    reason: WaitReason::RunTimeout,
                });
                record.timeout_asked = false;
            } else if record.status == Status::Active && !record.stopped && !record.finished {
                record.awaiting_reply = self
                    .queued_wait(record.reply_generation)?
                    .filter(|wait| wait.activity_id != record.cleared_wait_id);
            }
        }
        Ok(record)
    }

    fn queued_wait(&self, reply_generation: u64) -> Result<Option<AwaitingReply>> {
        let dir = self.state_dir().join("outbox");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(None);
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        for path in paths.into_iter().rev() {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let (Some(activity_id), Some(asked_at), Some(reason)) = (
                request["id"].as_str(),
                request["created"].as_str(),
                request["wait_reason"].as_str(),
            ) else {
                continue;
            };
            let generation = request["wait_generation"].as_u64().unwrap_or_default();
            let Ok(reason) = serde_json::from_value(serde_json::Value::String(reason.into()))
            else {
                continue;
            };
            if generation != reply_generation {
                continue;
            }
            return Ok(Some(AwaitingReply {
                activity_id: activity_id.into(),
                asked_at: asked_at.into(),
                reason,
            }));
        }
        Ok(None)
    }

    /// Read-modify-write of the record under the lock: `change` touches only
    /// the fields its step owns.
    pub fn update(&self, change: impl FnOnce(&mut RunRecord)) -> Result<RunRecord> {
        let lock = self.lock()?;
        self.update_held(&lock, change)
    }

    /// `update` for a caller that holds the lock, so several writes form one
    /// critical section.
    pub fn update_held(
        &self,
        _lock: &RunLock,
        change: impl FnOnce(&mut RunRecord),
    ) -> Result<RunRecord> {
        let mut record = self.record()?;
        change(&mut record);
        files::write_json(&self.record_path(), &record)?;
        Ok(record)
    }

    /// Appends one allowed reply to `conversation.md`.
    pub fn append_conversation_held(
        &self,
        _lock: &RunLock,
        created: &str,
        user_id: &str,
        body: &str,
    ) -> Result<()> {
        let path = self.conversation_md();
        let mut text = std::fs::read_to_string(&path).unwrap_or_else(|_| "# Conversation\n\nReplies from allowed users in the issue's Agent Session, oldest first.\n".to_string());
        text.push_str(&format!(
            "\n## {created} (user {user_id})\n\n{}\n",
            body.trim()
        ));
        write_atomic(&path, text.as_bytes())
    }

    /// Records a reply from a user who is not allowed; it never reaches the coordinator.
    pub fn record_ignored_prompt_held(
        &self,
        _lock: &RunLock,
        created: &str,
        user_id: &str,
        body: &str,
    ) -> Result<()> {
        let path = self.state_dir().join("ignored-prompts.md");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&format!(
            "\n## {created} (user {user_id})\n\n{}\n",
            body.trim()
        ));
        write_atomic(&path, text.as_bytes())
    }
}

/// The parts of an issue a person edits, hashed to tell a real edit from an
/// `updatedAt` change the plugin's own writes caused. Session comments are
/// left out: the agent's activities show as comments, and replies are
/// relayed on their own.
pub fn issue_hash(issue: &IssueDetail) -> String {
    let labels: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
    let comments: Vec<String> = issue
        .comments
        .iter()
        .filter(|c| !c.in_session)
        .map(|c| format!("{}\n{}\n{}", c.author, c.created_at, c.body))
        .collect();
    files::sha256_hex(
        format!(
            "{}\n{}\n{}\n{}",
            issue.title,
            issue.description,
            labels.join(","),
            comments.join("\n")
        )
        .as_bytes(),
    )
}

/// `issue.md`: the issue as data for the coordinator. The description and
/// comments are what was asked, never instructions to the plugin.
pub fn issue_markdown(issue: &IssueDetail) -> String {
    let mut out = format!("# {} {}\n\n", issue.identifier, issue.title);
    out.push_str(&format!(
        "- URL: {}\n- Team: {} ({})\n- State: {} ({})\n",
        issue.url, issue.team.name, issue.team.key, issue.state.name, issue.state.r#type
    ));
    if let Some(estimate) = issue.estimate {
        out.push_str(&format!("- Estimate: {estimate}\n"));
    }
    if !issue.labels.is_empty() {
        let labels: Vec<String> = issue
            .labels
            .iter()
            .map(|l| match &l.group {
                Some(group) => format!("{group}/{}", l.name),
                None => l.name.clone(),
            })
            .collect();
        out.push_str(&format!("- Labels: {}\n", labels.join(", ")));
    }
    out.push_str(&format!(
        "- Updated: {}\n\n## Description\n\n",
        issue.updated_at
    ));
    out.push_str(if issue.description.trim().is_empty() {
        "(no description)"
    } else {
        issue.description.trim()
    });
    out.push_str("\n\n## Comments\n");
    if issue.comments.is_empty() {
        out.push_str("\n(no comments)\n");
    }
    for comment in &issue.comments {
        out.push_str(&format!(
            "\n### {} at {}\n\n{}\n",
            comment.author,
            comment.created_at,
            comment.body.trim()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_validated_before_any_path_is_built() {
        for good in ["acme/DATA-1", "a-1/A1B-123456"] {
            assert!(validate_key(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "DATA-1",
            "acme/data-1",
            "acme/DATA",
            "acme/DATA-",
            "acme/-1",
            "../DATA-1",
            "acme/../DATA-1",
            "acme/DATA-1/x",
            "acme/DATA-1a",
            "Acme/DATA-1",
            "1acme/DATA-1",
            "acme-with-a-long-name/DATA-1",
        ] {
            assert!(validate_key(bad).is_err(), "{bad}");
        }
        assert_eq!(
            validate_key("DATA-1").unwrap_err().to_string(),
            "`DATA-1` is not a run key (expected the form workspace/TEAM-123)"
        );
        assert_eq!(split_key("acme/DATA-1"), ("acme", "DATA-1"));
    }

    #[test]
    fn runs_of_two_workspaces_with_the_same_issue_key_are_apart() {
        let dir = tempfile::tempdir().unwrap();
        for workspace in ["beta", "acme"] {
            Run::create(
                dir.path(),
                RunRecord {
                    workspace: workspace.into(),
                    identifier: "DATA-1".into(),
                    ..RunRecord::default()
                },
            )
            .unwrap();
        }
        let keys: Vec<String> = Run::list(dir.path()).into_iter().map(|r| r.key).collect();
        assert_eq!(keys, ["acme/DATA-1", "beta/DATA-1"]);
        assert!(dir.path().join("acme/DATA-1/.state/run.json").is_file());
    }

    #[test]
    fn a_record_with_an_older_builds_routing_fields_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let record = RunRecord {
            workspace: "acme".into(),
            identifier: "DATA-1".into(),
            ..RunRecord::default()
        };
        let run = Run::create(dir.path(), record).unwrap();
        std::fs::write(
            run.record_path(),
            r#"{"identifier":"DATA-1","size":"S","size_source":"agent",
                "routing":{"pid":4242,"started":"2026-01-01T00:00:00Z","output":""}}"#,
        )
        .unwrap();
        let record = run.record().unwrap();
        assert_eq!(
            (record.identifier.as_str(), record.routing_source.as_str()),
            ("DATA-1", "")
        );
    }

    #[test]
    fn runs_are_created_listed_and_updated_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let record = RunRecord {
            workspace: "acme".into(),
            identifier: "DATA-1".into(),
            title: "First".into(),
            ..RunRecord::default()
        };
        let run = Run::create(dir.path(), record.clone()).unwrap();
        assert!(Run::create(dir.path(), record).is_err());
        for sub in SUBDIRS {
            assert!(run.dir.join(sub).is_dir(), "{sub}");
        }
        run.update(|r| r.session_id = "s".into()).unwrap();
        run.update(|r| r.finished = true).unwrap();
        let loaded = Run::load(dir.path(), "acme/DATA-1")
            .unwrap()
            .record()
            .unwrap();
        assert_eq!(
            (
                loaded.session_id.as_str(),
                loaded.finished,
                loaded.title.as_str()
            ),
            ("s", true, "First")
        );
        assert_eq!(Run::list(dir.path()).len(), 1);
        assert!(Run::load(dir.path(), "DATA-2").is_err());

        let lock = run.lock().unwrap();
        run.append_conversation_held(
            &lock,
            "2026-09-25T00:00:01Z",
            "user-1",
            "Please also fix B.\n",
        )
        .unwrap();
        run.append_conversation_held(&lock, "2026-09-25T00:00:02Z", "user-1", "Thanks")
            .unwrap();
        drop(lock);
        let text = std::fs::read_to_string(run.conversation_md()).unwrap();
        assert!(text.starts_with("# Conversation"));
        assert!(text.find("Please also fix B.").unwrap() < text.find("Thanks").unwrap());
    }
}
