//! Typed calls to the herdr CLI. Differences between herdr versions stay here.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::runner::{Cmd, Runner};

pub const MIN_VERSION: Version = Version(0, 9, 1);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);
pub const AGENT_START_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Parses `herdr 0.9.1` and `herdr 0.9.2-preview.3`; a pre-release suffix is ignored.
pub fn parse_version(text: &str) -> Option<Version> {
    let token = text
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let core = token.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some(Version(parts.next()??, parts.next()??, parts.next()??))
}

/// A herdr command that does not talk to a server (`--version`, `session list`).
fn bare(bin: &str) -> Cmd {
    Cmd::new(bin, CALL_TIMEOUT)
}

pub fn version(bin: &str, runner: &dyn Runner) -> Result<Version> {
    let out = runner.run(&bare(bin).arg("--version"))?;
    if !out.success() {
        bail!("`{bin} --version` failed: {}", out.error_text());
    }
    parse_version(&out.stdout)
        .with_context(|| format!("could not read a version from `{}`", out.stdout.trim()))
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub running: bool,
    pub socket_path: PathBuf,
}

pub fn session_list(bin: &str, runner: &dyn Runner) -> Result<Vec<SessionInfo>> {
    #[derive(Deserialize)]
    struct Reply {
        sessions: Vec<SessionInfo>,
    }
    let out = runner.run(&bare(bin).args(["session", "list", "--json"]))?;
    if !out.success() {
        bail!("`{bin} session list` failed: {}", out.error_text());
    }
    let reply: Reply =
        serde_json::from_str(&out.stdout).context("`herdr session list --json` output changed")?;
    Ok(reply.sessions)
}

/// The socket of the session named in the config, or of herdr's default
/// session. Asked from herdr (`session list --json`), never guessed.
pub fn session_socket(bin: &str, runner: &dyn Runner, name: Option<&str>) -> Result<PathBuf> {
    let sessions = session_list(bin, runner)?;
    let found = match name {
        Some(name) => sessions.into_iter().find(|s| s.name == name),
        None => sessions.into_iter().find(|s| s.default),
    };
    match (found, name) {
        (Some(session), _) => Ok(session.socket_path),
        (None, Some(name)) => {
            bail!("herdr has no session named `{name}`; start it with `herdr --session {name}`")
        }
        (None, None) => bail!("herdr lists no default session"),
    }
}

/// herdr bound to one session's socket.
pub struct Herdr<'a> {
    bin: String,
    socket: PathBuf,
    runner: &'a dyn Runner,
}

/// A herdr call that failed. `code` is herdr's own error code (for example
/// `agent_not_ready` or `pane_not_found`), or `timeout` / `unreachable` /
/// `failed` when herdr never answered with one.
#[derive(Debug, Clone, PartialEq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "herdr: {} ({})", self.message, self.code)
    }
}

impl std::error::Error for HerdrError {}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Pane {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub foreground_cwd: String,
    /// Stable across pane moves; restarts with the server.
    #[serde(default)]
    pub terminal_id: String,
}

/// The native session reference an official integration reported.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct AgentSession {
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Agent {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub agent_status: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub foreground_cwd: String,
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
}

impl Agent {
    pub fn session_id(&self) -> &str {
        self.agent_session
            .as_ref()
            .map(|s| s.value.as_str())
            .unwrap_or("")
    }
}

/// The one "ready for a prompt" predicate: state `idle` or `done`.
pub fn ready_state(state: &str) -> bool {
    matches!(state, "idle" | "done")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
}

impl<'a> Herdr<'a> {
    pub fn new(bin: impl Into<String>, socket: impl Into<PathBuf>, runner: &'a dyn Runner) -> Self {
        Herdr {
            bin: bin.into(),
            socket: socket.into(),
            runner,
        }
    }

    /// `HERDR_SESSION` is removed so an inherited value can never compete with
    /// the socket this binary resolved.
    pub fn cmd(&self, timeout: Duration) -> Cmd {
        Cmd::new(&self.bin, timeout)
            .env("HERDR_SOCKET_PATH", self.socket.to_string_lossy())
            .env_remove("HERDR_SESSION")
    }

    /// Runs one herdr command and returns the `result` object of its JSON reply.
    pub fn call(&self, args: &[&str], timeout: Duration) -> Result<serde_json::Value, HerdrError> {
        let cmd = self.cmd(timeout).args(args.iter().copied());
        let out = self.runner.run(&cmd).map_err(|e| HerdrError {
            code: "unreachable".into(),
            message: format!("{e:#}"),
        })?;
        if out.timed_out {
            return Err(HerdrError {
                code: "timeout".into(),
                message: format!("`herdr {}` timed out", args.join(" ")),
            });
        }
        // herdr prints one JSON object; on failure it carries `error`, and
        // which stream it lands on is not something to depend on.
        let reply = [&out.stdout, &out.stderr]
            .into_iter()
            .find_map(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok());
        if let Some(reply) = reply {
            if let Some(error) = reply.get("error") {
                return Err(HerdrError {
                    code: error["code"].as_str().unwrap_or("failed").to_string(),
                    message: error["message"].as_str().unwrap_or("").to_string(),
                });
            }
            if out.success() {
                return Ok(reply
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
        } else if out.success() && out.stdout.trim().is_empty() {
            // Metadata writes acknowledge success with the exit status alone.
            return Ok(serde_json::Value::Null);
        }
        Err(HerdrError {
            code: "failed".into(),
            message: format!("`herdr {}`: {}", args.join(" "), out.error_text()),
        })
    }

    fn call_as<T: serde::de::DeserializeOwned>(
        &self,
        args: &[&str],
        field: &str,
    ) -> Result<T, HerdrError> {
        let result = self.call(args, CALL_TIMEOUT)?;
        serde_json::from_value(result[field].clone()).map_err(|e| HerdrError {
            code: "failed".into(),
            message: format!("`herdr {}` reply changed: {e}", args.join(" ")),
        })
    }

    pub fn pane_list(&self) -> Result<Vec<Pane>, HerdrError> {
        self.call_as(&["pane", "list"], "panes")
    }

    pub fn agent_list(&self) -> Result<Vec<Agent>, HerdrError> {
        self.call_as(&["agent", "list"], "agents")
    }

    fn created(result: &serde_json::Value) -> Result<Created, HerdrError> {
        let pane = &result["root_pane"];
        match (
            pane["workspace_id"].as_str(),
            pane["tab_id"].as_str(),
            pane["pane_id"].as_str(),
        ) {
            (Some(w), Some(t), Some(p)) => Ok(Created {
                workspace_id: w.into(),
                tab_id: t.into(),
                pane_id: p.into(),
            }),
            _ => Err(HerdrError {
                code: "failed".into(),
                message: "herdr's create reply has no root_pane ids".into(),
            }),
        }
    }

    /// A new workspace whose root pane is a shell in `cwd`, never focused.
    pub fn workspace_create(&self, cwd: &Path, label: &str) -> Result<Created, HerdrError> {
        let cwd = cwd.to_string_lossy();
        let result = self.call(
            &[
                "workspace",
                "create",
                "--cwd",
                &cwd,
                "--label",
                label,
                "--no-focus",
            ],
            CALL_TIMEOUT,
        )?;
        Self::created(&result)
    }

    /// Closes a workspace's Herdr state. A worktree checkout stays on disk.
    pub fn workspace_close(&self, workspace: &str) -> Result<(), HerdrError> {
        self.call(&["workspace", "close", workspace], CALL_TIMEOUT)
            .map(|_| ())
    }

    pub fn workspace_focus(&self, workspace: &str) -> Result<(), HerdrError> {
        self.call(&["workspace", "focus", workspace], CALL_TIMEOUT)
            .map(|_| ())
    }

    /// Creates a worktree-backed workspace. Returns the ids, the checkout path
    /// and the root pane's working directory as herdr reports them.
    pub fn worktree_create(
        &self,
        repo: &Path,
        branch: &str,
        base: &str,
        label: &str,
    ) -> Result<(Created, String, String), HerdrError> {
        let repo = repo.to_string_lossy();
        let result = self.call(
            &[
                "worktree",
                "create",
                "--cwd",
                &repo,
                "--branch",
                branch,
                "--base",
                base,
                "--label",
                label,
                "--no-focus",
            ],
            Duration::from_secs(20),
        )?;
        Self::worktree_reply(&result)
    }

    /// `--cwd <repo>` is required: without it herdr answers `worktree_not_found`
    /// even for a path git lists (checked on 0.9.1).
    pub fn worktree_open(
        &self,
        repo: &Path,
        path: &str,
        label: &str,
    ) -> Result<(Created, String, String), HerdrError> {
        let repo = repo.to_string_lossy();
        let result = self.call(
            &[
                "worktree",
                "open",
                "--cwd",
                &repo,
                "--path",
                path,
                "--label",
                label,
                "--no-focus",
            ],
            Duration::from_secs(20),
        )?;
        Self::worktree_reply(&result)
    }

    fn worktree_reply(result: &serde_json::Value) -> Result<(Created, String, String), HerdrError> {
        let created = Self::created(result)?;
        let path = result["worktree"]["path"]
            .as_str()
            .or_else(|| result["workspace"]["worktree"]["checkout_path"].as_str())
            .unwrap_or_default()
            .to_string();
        let cwd = result["root_pane"]["cwd"]
            .as_str()
            .unwrap_or(&path)
            .to_string();
        if path.is_empty() {
            return Err(HerdrError {
                code: "failed".into(),
                message: "herdr's worktree reply has no path".into(),
            });
        }
        Ok((created, path, cwd))
    }

    /// Starts an agent in a pane that is at a shell prompt. Success means herdr
    /// detected the agent and it is ready for input.
    pub fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane: &str,
        agent_args: &[String],
    ) -> Result<Agent, HerdrError> {
        let timeout_ms = AGENT_START_TIMEOUT.as_millis().to_string();
        let mut args = vec![
            "agent",
            "start",
            name,
            "--kind",
            kind,
            "--pane",
            pane,
            "--timeout",
            &timeout_ms,
        ];
        if !agent_args.is_empty() {
            args.push("--");
            args.extend(agent_args.iter().map(String::as_str));
        }
        // herdr enforces the timeout itself; the outer deadline only guards a hang.
        let result = self.call(&args, AGENT_START_TIMEOUT + Duration::from_secs(5))?;
        serde_json::from_value(result["agent"].clone()).map_err(|e| HerdrError {
            code: "failed".into(),
            message: format!("`herdr agent start` reply changed: {e}"),
        })
    }

    /// Submits a prompt. herdr's parser takes positionals first and options
    /// after them, and has no `--` separator here; text in the second
    /// position is accepted even when it starts with a dash (checked on 0.9.1).
    pub fn agent_prompt(&self, target: &str, text: &str) -> Result<(), HerdrError> {
        self.call(&["agent", "prompt", target, text], CALL_TIMEOUT)
            .map(|_| ())
    }

    /// Escape in an agent's pane: the harness's own interrupt.
    pub fn agent_interrupt(&self, target: &str) -> Result<(), HerdrError> {
        self.call(&["agent", "send-keys", target, "esc"], CALL_TIMEOUT)
            .map(|_| ())
    }

    /// Names an already detected agent (after Herdr's native resume leaves a
    /// restored pane unnamed).
    pub fn agent_rename(&self, pane: &str, name: &str) -> Result<(), HerdrError> {
        self.call(&["agent", "rename", pane, name], CALL_TIMEOUT)
            .map(|_| ())
    }

    pub fn notification_show(&self, title: &str, body: &str) -> Result<(), HerdrError> {
        self.call(
            &["notification", "show", title, "--body", body],
            CALL_TIMEOUT,
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{FakeRunner, fail, ok};

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("herdr 0.9.0\n"), Some(Version(0, 9, 0)));
        assert_eq!(
            parse_version("herdr 0.9.2-preview.3"),
            Some(Version(0, 9, 2))
        );
        assert_eq!(parse_version("0.10.0"), Some(Version(0, 10, 0)));
        assert_eq!(parse_version("herdr"), None);
        assert!(Version(0, 9, 0) < MIN_VERSION);
        assert!(Version(0, 10, 0) > MIN_VERSION);
    }

    const SESSIONS: &str = r#"{"sessions":[
        {"default":true,"name":"default","running":true,"socket_path":"/h/.config/herdr/herdr.sock"},
        {"default":false,"name":"work","running":true,"socket_path":"/h/.config/herdr/sessions/work/herdr.sock"}]}"#;

    #[test]
    fn session_sockets_come_from_herdr() {
        let runner = FakeRunner::new();
        runner.on("session list --json", ok(SESSIONS));
        assert_eq!(
            session_socket("herdr", &runner, None).unwrap(),
            PathBuf::from("/h/.config/herdr/herdr.sock")
        );
        assert_eq!(
            session_socket("herdr", &runner, Some("work")).unwrap(),
            PathBuf::from("/h/.config/herdr/sessions/work/herdr.sock")
        );
        assert!(session_socket("herdr", &runner, Some("nope")).is_err());
    }

    #[test]
    fn errors_carry_herdrs_code_and_calls_carry_the_socket() {
        let runner = FakeRunner::new();
        runner.on(
            "agent start",
            fail(
                1,
                r#"{"error":{"code":"agent_not_ready","message":"blocked during startup"}}"#,
            ),
        );
        runner.on("pane report-metadata", ok(""));
        let herdr = Herdr::new("herdr", "/s.sock", &runner);
        let error = herdr
            .agent_start("x", "claude", "w1:p1", &["--model".into(), "opus".into()])
            .unwrap_err();
        assert_eq!(error.code, "agent_not_ready");
        assert!(
            runner
                .lines("agent start")
                .iter()
                .any(|l| l.ends_with("-- --model opus"))
        );
        assert!(
            herdr
                .call(&["pane", "report-metadata", "w1:p1"], CALL_TIMEOUT)
                .is_ok()
        );
        let call = runner.calls.borrow()[0].clone();
        assert!(
            call.env
                .contains(&("HERDR_SOCKET_PATH".into(), "/s.sock".into()))
        );
        assert!(call.env_remove.contains(&"HERDR_SESSION".into()));
    }
}
