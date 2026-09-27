//! Workers: one agent per repository in its own Herdr worktree. Records,
//! briefs, the identity check before acting on a pane, and the group the
//! ticker reports to the coordinator.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::files::{self, seconds_since, write_atomic};
use crate::herdr_cli::{Agent, Pane, ready_state};
use crate::progress;
use crate::run::{AgentRecord, AgentStatus, Run};

pub const BLOCKED_DEBOUNCE_SECS: i64 = 30;
pub const NOT_READY_SECS: i64 = 60;
pub const MAX_LAUNCH_ATTEMPTS: u32 = 3;
pub const MAX_RESTARTS: u32 = 2;
/// The folder inside a worktree that holds a worker's brief and report.
pub const BRIEF_FOLDER: &str = ".herdr-linear-agent";

/// `workers/<id>.toml`. An empty string means "not set".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Worker {
    pub id: String,
    pub title: String,
    /// The repository's catalog name.
    pub repo: String,
    pub repo_path: String,
    pub base: String,
    pub branch: String,
    pub worktree_path: String,
    /// `<worktree>/.herdr-linear-agent/<ISSUE-KEY>-<id>`: brief and report.
    pub brief_dir: String,
    pub created: String,
    pub updated: String,
    pub restarts: u32,
    /// The hash of the worker's report.md as last copied home.
    pub report_hash: String,
    /// The report hash the coordinator was last told about.
    pub announced_report_hash: String,
    pub pr_url: String,
    /// An error activity was sent because the pane closed before a report.
    pub gone_reported: bool,
    /// The "Start worker" action was sent for the current launch.
    pub start_announced: bool,
    pub agent: AgentRecord,
}

impl Worker {
    pub fn report_path(&self) -> PathBuf {
        Path::new(&self.brief_dir).join("report.md")
    }

    /// An open or starting worker counts against the limits.
    pub fn counts(&self) -> bool {
        matches!(self.agent.status, AgentStatus::Pending | AgentStatus::Open)
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    let digits = id.strip_prefix('w').unwrap_or("");
    if digits.is_empty()
        || digits.len() > 4
        || !digits.chars().all(|c| c.is_ascii_digit())
        || digits.starts_with('0')
    {
        bail!("`{id}` is not a worker id (expected the form w1)");
    }
    Ok(())
}

fn record_path(run: &Run, id: &str) -> PathBuf {
    run.workers_dir().join(format!("{id}.toml"))
}

pub fn task_path(run: &Run, id: &str) -> PathBuf {
    run.workers_dir().join(format!("{id}.task.md"))
}

pub fn home_report_path(run: &Run, id: &str) -> PathBuf {
    run.workers_dir().join(format!("{id}.md"))
}

pub fn load(run: &Run, id: &str) -> Result<Worker> {
    validate_id(id)?;
    let path = record_path(run, id);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("{} has no worker `{id}`", run.key))?;
    toml::from_str(&text).with_context(|| format!("{} does not parse", path.display()))
}

/// Every worker of a run, by number.
pub fn list(run: &Run) -> Vec<Worker> {
    let Ok(entries) = std::fs::read_dir(run.workers_dir()) else {
        return Vec::new();
    };
    let mut workers: Vec<Worker> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".toml").map(str::to_string))
        .filter_map(|id| load(run, &id).ok())
        .collect();
    workers.sort_by_key(|w| w.id[1..].parse::<u32>().unwrap_or(0));
    workers
}

fn write_record(run: &Run, worker: &Worker) -> Result<()> {
    write_atomic(
        &record_path(run, &worker.id),
        toml::to_string(worker)?.as_bytes(),
    )
}

/// Read-modify-write under the run lock.
pub fn update(run: &Run, id: &str, change: impl FnOnce(&mut Worker)) -> Result<Worker> {
    let _lock = run.lock()?;
    let mut worker = load(run, id)?;
    change(&mut worker);
    worker.updated = files::now();
    write_record(run, &worker)?;
    Ok(worker)
}

/// Allocates the next id under the run lock after `check` accepts the
/// current workers (the limits are checked in the same critical section).
pub fn allocate(
    run: &Run,
    check: impl FnOnce(&[Worker]) -> Result<()>,
    fill: impl FnOnce(&mut Worker),
) -> Result<Worker> {
    let _lock = run.lock()?;
    let workers = list(run);
    check(&workers)?;
    let next = workers
        .iter()
        .filter_map(|w| w.id[1..].parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let mut worker = Worker {
        id: format!("w{next}"),
        created: files::now(),
        ..Worker::default()
    };
    fill(&mut worker);
    worker.updated = worker.created.clone();
    write_record(run, &worker)?;
    Ok(worker)
}

/// `herdr-linear-agent/<issue-key>/<id>-<title slug>`. The lower-case issue
/// key lets Linear's GitHub integration link the pull request to the issue.
pub fn branch_name(issue_key: &str, id: &str, title: &str) -> String {
    let slug = files::slugify(title);
    let key = issue_key.to_ascii_lowercase();
    if slug.is_empty() {
        format!("herdr-linear-agent/{key}/{id}")
    } else {
        format!("herdr-linear-agent/{key}/{id}-{slug}")
    }
}

pub fn brief_dir(cwd: &str, issue_key: &str, id: &str) -> String {
    format!(
        "{}/{BRIEF_FOLDER}/{issue_key}-{id}",
        cwd.trim_end_matches('/')
    )
}

/// The one line a worker is prompted with. Nothing from the issue is ever
/// placed in a prompt.
pub fn launch_prompt(issue_key: &str, id: &str) -> String {
    format!("Read {BRIEF_FOLDER}/{issue_key}-{id}/brief.md and do what it says.")
}

/// Appends a forwarded prompt to the task file with a timestamp, so a
/// restarted worker re-reads it with its task.
pub fn append_follow_up(run: &Run, id: &str, text: &str) -> Result<()> {
    let _lock = run.lock()?;
    let path = task_path(run, id);
    let mut task = std::fs::read_to_string(&path).unwrap_or_default();
    if !task.ends_with('\n') && !task.is_empty() {
        task.push('\n');
    }
    if !task.lines().any(|l| l.trim() == "## Follow-ups") {
        task.push_str("\n## Follow-ups\n");
    }
    task.push_str(&format!("\n### {}\n\n{}\n", files::now(), text.trim()));
    write_atomic(&path, task.as_bytes())
}

pub struct BriefInput<'a> {
    pub issue_key: &'a str,
    pub issue_title: &'a str,
    pub issue_url: &'a str,
    pub worker: &'a Worker,
    pub task: &'a str,
    pub restart: bool,
    /// The binary's absolute path, quoted for a shell.
    pub binary: &'a str,
}

pub fn compose_brief(input: &BriefInput) -> String {
    let w = input.worker;
    let report = w.report_path();
    let mut brief = format!(
        "# Worker brief\n\n- Issue: {} {} ({})\n- Worker: {}\n- Repository: `{}` at `{}`\n- Branch: `{}` (from `origin/{}`)\n- Report: `{}`\n\n",
        input.issue_key,
        input.issue_title,
        input.issue_url,
        w.id,
        w.repo,
        w.worktree_path,
        w.branch,
        w.base,
        report.display()
    );
    brief.push_str(include_str!("../assets/WORKER.md").trim_end());
    brief.push_str("\n\n");
    if input.restart {
        brief.push_str("**A previous attempt at this task exists on this branch.** Read its report at the report path above first, look at what is already on the branch, and continue from there.\n\n");
    }
    brief.push_str(&format!(
        "# Progress\n\nReport progress in this pane with `{} report --percent N --activity '...'` (two to four words; `--unknown` while the scope is unclear): at the start, at milestones, `--activity 'Waiting for you'` when you stop with a question, and `--percent 100` when the whole task is done.\n\n",
        input.binary
    ));
    brief.push_str("# Task\n\n");
    brief.push_str(input.task.trim());
    brief.push('\n');
    brief
}

// ---------------------------------------------------------------- groups

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    WaitingOnYou,
    Reported,
    Working,
    Idle,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::WaitingOnYou => "Waiting on you",
            Group::Reported => "Reported",
            Group::Working => "Working",
            Group::Idle => "Idle",
        }
    }

    pub fn token(self) -> &'static str {
        match self {
            Group::WaitingOnYou => "waiting-on-you",
            Group::Reported => "reported",
            Group::Working => "working",
            Group::Idle => "idle",
        }
    }
}

/// What Herdr shows for an agent's pane right now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Live {
    pub pane_exists: bool,
    /// `None` when no agent is detected in the pane.
    pub agent_state: Option<String>,
    /// How long the agent has been in that state.
    pub state_secs: i64,
    pub self_report: Option<progress::Record>,
    pub report_age_secs: i64,
    /// The agent was found in another pane (Herdr renumbered it): its ids.
    pub moved_to: Option<(String, String, String)>,
}

impl Live {
    /// The agent said it is waiting for someone and has not started working since.
    pub fn self_waiting(&self) -> bool {
        self.self_report.as_ref().is_some_and(|r| r.waiting())
            && self.agent_state.as_deref() != Some("working")
    }

    pub fn self_working(&self) -> bool {
        self.self_report
            .as_ref()
            .is_some_and(|r| !r.done() && !r.waiting())
            && self.report_age_secs < (progress::ACTIVITY_TTL_MS / 1000) as i64
    }

    /// Herdr shows a question or permission prompt for 30 seconds, or a
    /// launch has been stuck on a dialog for 60: only a person in the pane
    /// can move it on.
    pub fn needs_person(&self, record: &AgentRecord) -> bool {
        let state = self.agent_state.as_deref();
        let blocked = state == Some("blocked") && self.state_secs >= BLOCKED_DEBOUNCE_SECS;
        let stuck_launch = record.prompt_pending
            && state.is_some_and(|s| !ready_state(s))
            && self.state_secs >= NOT_READY_SECS;
        blocked || stuck_launch
    }
}

fn works_in(agent: &Agent, cwd: &str) -> bool {
    !cwd.is_empty() && (agent.cwd == cwd || agent.foreground_cwd == cwd)
}

fn same_agent(record: &AgentRecord, agent: &Agent) -> bool {
    works_in(agent, &record.cwd) && (agent.agent.is_empty() || agent.agent == record.kind)
}

/// The record's agent: in the recorded pane with the same working directory
/// and kind, named as recorded or unnamed (Herdr's native resume starts it
/// again without a name); or, when Herdr renumbered panes after a restart, the
/// live agent with the recorded name, working directory and kind.
pub fn find_agent<'a>(record: &AgentRecord, agents: &'a [Agent]) -> Option<&'a Agent> {
    if record.pane_id.is_empty() {
        return None;
    }
    agents
        .iter()
        .find(|a| {
            a.pane_id == record.pane_id
                && same_agent(record, a)
                && (a.name.is_empty() || a.name == record.agent_name)
        })
        .or_else(|| {
            agents.iter().find(|a| {
                !record.agent_name.is_empty()
                    && a.name == record.agent_name
                    && same_agent(record, a)
            })
        })
}

/// Our agent running without a name: re-apply it.
pub fn needs_rename(record: &AgentRecord, agent: &Agent) -> bool {
    !record.agent_name.is_empty() && agent.name.is_empty()
}

/// Live state from one `agent list` and one `pane list`, plus the agent's
/// own report. `record.last_state_change` supplies how long the state lasted
/// when the live state equals the recorded one.
pub fn live_state(
    record: &AgentRecord,
    agents: &[Agent],
    panes: &[Pane],
    now: jiff::Timestamp,
    state_dir: &Path,
    socket: &str,
) -> Live {
    let agent = find_agent(record, agents);
    let pane = panes.iter().find(|p| {
        p.pane_id == record.pane_id && (p.cwd == record.cwd || p.foreground_cwd == record.cwd)
    });
    // A pane with our id that holds someone else's agent is not ours.
    let foreign = agent.is_none() && agents.iter().any(|a| a.pane_id == record.pane_id);
    let agent_state = agent.map(|a| a.agent_status.clone());
    let state_secs = match &agent_state {
        Some(state) if *state == record.last_state => seconds_since(&record.last_state_change, now),
        _ => 0,
    };
    let mut live = Live {
        pane_exists: (agent.is_some() || pane.is_some()) && !foreign,
        agent_state,
        state_secs,
        moved_to: agent
            .filter(|a| a.pane_id != record.pane_id)
            .map(|a| (a.workspace_id.clone(), a.tab_id.clone(), a.pane_id.clone())),
        ..Live::default()
    };
    let terminal = agent
        .map(|a| a.terminal_id.as_str())
        .or(pane.map(|p| p.terminal_id.as_str()))
        .unwrap_or("");
    let pane_id = agent.map(|a| a.pane_id.as_str()).unwrap_or(&record.pane_id);
    if live.pane_exists
        && let Some(report) = progress::self_report(state_dir, socket, pane_id, terminal)
    {
        live.report_age_secs = now.as_second() - report.reported_at;
        live.self_report = Some(report);
    }
    live
}

/// A worker's group. First matching row wins; one function, so the commands
/// and the ticker always agree.
pub fn group(worker: &Worker, live: &Live) -> Group {
    let agent = &worker.agent;
    let state = live.agent_state.as_deref();
    let has_report = !worker.report_hash.is_empty();
    if agent.status == AgentStatus::Failed
        || (!live.pane_exists && !has_report && agent.status == AgentStatus::Open)
    {
        return Group::WaitingOnYou;
    }
    if live.needs_person(agent) || live.self_waiting() {
        return Group::WaitingOnYou;
    }
    // The harness showing `working` wins over a report: a report written
    // mid-run is not a result.
    if has_report && !agent.prompt_pending && state != Some("working") {
        return Group::Reported;
    }
    if matches!(state, Some("working") | Some("blocked"))
        || agent.prompt_pending
        || agent.status == AgentStatus::Pending
        || live.self_working()
    {
        return Group::Working;
    }
    Group::Idle
}

/// The hash of a worker's report when it is a regular file inside a real brief
/// folder. Cheap enough to run every tick.
pub fn report_hash(worker: &Worker) -> Option<String> {
    let dir = Path::new(&worker.brief_dir);
    if worker.brief_dir.is_empty() || !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let report = worker.report_path();
    let regular = std::fs::symlink_metadata(&report).is_ok_and(|m| m.is_file());
    regular
        .then(|| std::fs::read(&report).ok())
        .flatten()
        .map(|bytes| files::sha256_hex(&bytes))
}

/// Copies a worker's report into the run folder. Returns the report's hash,
/// or `None` when there is no regular report file.
pub fn copy_report_home(run: &Run, worker: &Worker) -> Result<Option<String>> {
    report_hash(worker).map_or(Ok(None), |hash| {
        let bytes = std::fs::read(worker.report_path())?;
        let _lock = run.lock()?;
        write_atomic(&home_report_path(run, &worker.id), &bytes)?;
        Ok(Some(hash))
    })
}

/// The pull request URL on a report's first line (`PR: <url>`), when it is a
/// GitHub pull request URL.
pub fn pr_line(report: &str) -> Option<String> {
    let url = report.lines().next()?.strip_prefix("PR:")?.trim();
    let rest = url.strip_prefix("https://github.com/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let valid = parts.len() == 4
        && parts[2] == "pull"
        && parts[3].chars().all(|c| c.is_ascii_digit())
        && !parts[3].is_empty()
        && parts[..2].iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        });
    valid.then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::RunRecord;

    fn now() -> jiff::Timestamp {
        "2026-09-25T12:00:00Z".parse().unwrap()
    }

    fn ago(secs: i64) -> String {
        (now() - jiff::SignedDuration::from_secs(secs)).to_string()
    }

    fn open_worker() -> Worker {
        Worker {
            id: "w1".into(),
            agent: AgentRecord {
                status: AgentStatus::Open,
                kind: "claude".into(),
                agent_name: "data-1-w1".into(),
                pane_id: "w2:p1".into(),
                cwd: "/wt".into(),
                last_state: "idle".into(),
                last_state_change: ago(45),
                ..AgentRecord::default()
            },
            ..Worker::default()
        }
    }

    fn live(state: Option<&str>, secs: i64) -> Live {
        Live {
            pane_exists: true,
            agent_state: state.map(str::to_string),
            state_secs: secs,
            ..Live::default()
        }
    }

    #[test]
    fn groups_follow_the_rows_in_order() {
        let w = open_worker();
        let failed = Worker {
            agent: AgentRecord {
                status: AgentStatus::Failed,
                ..w.agent.clone()
            },
            ..w.clone()
        };
        assert_eq!(
            group(&failed, &live(Some("working"), 0)),
            Group::WaitingOnYou
        );
        assert_eq!(
            group(&w, &Live::default()),
            Group::WaitingOnYou,
            "pane gone without a report"
        );
        assert_eq!(group(&w, &live(Some("blocked"), 30)), Group::WaitingOnYou);
        assert_eq!(
            group(&w, &live(Some("blocked"), 29)),
            Group::Working,
            "a quickly answered prompt never shows"
        );
        let pending = Worker {
            agent: AgentRecord {
                prompt_pending: true,
                ..w.agent.clone()
            },
            ..w.clone()
        };
        assert_eq!(
            group(&pending, &live(Some("unknown"), 60)),
            Group::WaitingOnYou,
            "a launch stuck on a dialog"
        );
        assert_eq!(group(&pending, &live(None, 0)), Group::Working);

        let reported = Worker {
            report_hash: "h".into(),
            ..w.clone()
        };
        assert_eq!(group(&reported, &live(Some("idle"), 0)), Group::Reported);
        assert_eq!(group(&reported, &live(Some("working"), 0)), Group::Working);
        assert_eq!(
            group(&reported, &Live::default()),
            Group::Reported,
            "a closed pane after the report is finished work"
        );
        assert_eq!(group(&w, &live(Some("idle"), 0)), Group::Idle);

        let waiting = progress::Record {
            activity: progress::WAITING.into(),
            reported_at: 1,
            ..Default::default()
        };
        let asked = Live {
            self_report: Some(waiting),
            ..live(Some("idle"), 0)
        };
        assert_eq!(group(&reported, &asked), Group::WaitingOnYou);
    }

    fn agent(pane: &str, name: &str, cwd: &str, kind: &str) -> Agent {
        Agent {
            pane_id: pane.into(),
            workspace_id: "w2".into(),
            tab_id: "w2:t1".into(),
            name: name.into(),
            agent: kind.into(),
            agent_status: "idle".into(),
            cwd: cwd.into(),
            ..Agent::default()
        }
    }

    #[test]
    fn identity_is_pane_cwd_kind_and_name() {
        let w = open_worker();
        let rec = &w.agent;
        assert!(find_agent(rec, &[agent("w2:p1", "data-1-w1", "/wt", "claude")]).is_some());
        assert!(
            find_agent(rec, &[agent("w2:p1", "", "/wt", "claude")]).is_some(),
            "natively resumed, unnamed"
        );
        assert!(find_agent(rec, &[agent("w2:p1", "other", "/wt", "claude")]).is_none());
        assert!(find_agent(rec, &[agent("w2:p1", "data-1-w1", "/other", "claude")]).is_none());
        assert!(find_agent(rec, &[agent("w2:p1", "data-1-w1", "/wt", "codex")]).is_none());
        // Renumbered after a restart: found by name, cwd and kind.
        let moved = live_state(
            rec,
            &[agent("w9:p3", "data-1-w1", "/wt", "claude")],
            &[],
            now(),
            Path::new("/nonexistent"),
            "/s",
        );
        assert!(moved.pane_exists);
        assert_eq!(moved.moved_to.unwrap().2, "w9:p3");
        // Someone else's agent in our pane: treated as gone.
        let foreign = live_state(
            rec,
            &[agent("w2:p1", "other", "/wt", "claude")],
            &[],
            now(),
            Path::new("/nonexistent"),
            "/s",
        );
        assert!(!foreign.pane_exists);
        assert_eq!(
            live_state(
                rec,
                &[agent("w2:p1", "data-1-w1", "/wt", "claude")],
                &[],
                now(),
                Path::new("/nonexistent"),
                "/s"
            )
            .state_secs,
            45
        );
    }

    #[test]
    fn ids_branches_briefs_and_pr_lines() {
        assert!(validate_id("w1").is_ok() && validate_id("w12").is_ok());
        for bad in ["", "w", "w0", "w01", "x1", "../w1", "w1a"] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
        assert_eq!(
            branch_name("DATA-12", "w1", "Fix the $(login) bug!"),
            "herdr-linear-agent/data-12/w1-fix-the-login-bug"
        );
        assert_eq!(
            branch_name("DATA-12", "w2", "???"),
            "herdr-linear-agent/data-12/w2"
        );
        assert_eq!(
            brief_dir("/wt/", "DATA-12", "w1"),
            "/wt/.herdr-linear-agent/DATA-12-w1"
        );
        assert_eq!(
            launch_prompt("DATA-12", "w1"),
            "Read .herdr-linear-agent/DATA-12-w1/brief.md and do what it says."
        );

        assert_eq!(
            pr_line("PR: https://github.com/o/r/pull/12\n## Report"),
            Some("https://github.com/o/r/pull/12".into())
        );
        for bad in [
            "## Report",
            "PR: https://evil.example/o/r/pull/1",
            "PR: https://github.com/o/r/issues/1",
            "PR: https://github.com/o/r/pull/1x",
        ] {
            assert_eq!(pr_line(bad), None, "{bad}");
        }

        let w = Worker {
            id: "w1".into(),
            repo: "api".into(),
            branch: "b".into(),
            base: "main".into(),
            worktree_path: "/wt".into(),
            brief_dir: "/wt/.herdr-linear-agent/DATA-12-w1".into(),
            ..Worker::default()
        };
        let brief = compose_brief(&BriefInput {
            issue_key: "DATA-12",
            issue_title: "Title",
            issue_url: "https://linear.app/x",
            worker: &w,
            task: "Do it.",
            restart: true,
            binary: "/bin/hla",
        });
        let pos = |needle: &str| {
            brief
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        assert!(pos("# Worker brief") < pos("previous attempt"));
        assert!(pos("previous attempt") < pos("/bin/hla report --percent N"));
        assert!(pos("/bin/hla report") < pos("Do it."));
        assert!(brief.contains("/wt/.herdr-linear-agent/DATA-12-w1/report.md"));
    }

    #[test]
    fn allocation_follow_ups_and_report_copies() {
        let dir = tempfile::tempdir().unwrap();
        let run = Run::create(
            dir.path(),
            RunRecord {
                identifier: "DATA-1".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        let a = allocate(&run, |_| Ok(()), |w| w.repo = "api".into()).unwrap();
        let b = allocate(&run, |_| Ok(()), |_| {}).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("w1", "w2"));
        assert!(
            allocate(
                &run,
                |workers| if workers.len() >= 2 {
                    bail!("full")
                } else {
                    Ok(())
                },
                |_| {}
            )
            .is_err()
        );
        update(&run, "w1", |w| w.title = "T".into()).unwrap();
        assert_eq!(load(&run, "w1").unwrap().repo, "api");

        std::fs::write(task_path(&run, "w1"), "The task.").unwrap();
        append_follow_up(&run, "w1", "Also do Y.\n").unwrap();
        append_follow_up(&run, "w1", "And Z.").unwrap();
        let text = std::fs::read_to_string(task_path(&run, "w1")).unwrap();
        assert!(
            text.starts_with("The task.\n\n## Follow-ups\n\n### 20"),
            "{text}"
        );
        assert_eq!(text.matches("## Follow-ups").count(), 1);

        let work = tempfile::tempdir().unwrap();
        let brief = work.path().join(".herdr-linear-agent/DATA-1-w1");
        std::fs::create_dir_all(&brief).unwrap();
        let w = Worker {
            brief_dir: brief.to_string_lossy().into_owned(),
            ..load(&run, "w1").unwrap()
        };
        assert_eq!(copy_report_home(&run, &w).unwrap(), None);
        std::fs::write(
            brief.join("report.md"),
            "PR: https://github.com/o/r/pull/1\n",
        )
        .unwrap();
        assert!(copy_report_home(&run, &w).unwrap().is_some());
        assert!(home_report_path(&run, "w1").is_file());
        // A symbolic link is never read.
        std::fs::remove_file(brief.join("report.md")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", brief.join("report.md")).unwrap();
        assert_eq!(report_hash(&w), None);
    }
}
