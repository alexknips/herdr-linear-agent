//! Workers: one agent per repository in its own worktree. This module holds
//! the worker record, the names and the brief, and the rules that read a
//! worker's state from a Herdr snapshot. Nothing here talks to Herdr.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::files::{self, write_atomic};
use crate::herdr::{Agent, AgentStatus as HerdrStatus, Snapshot};
use crate::linear::api::{Activity, Content};
use crate::outbox::Op;
use crate::progress;
use crate::run::{AgentRecord, AgentStatus, Run, RunLock, Status};

/// Workers write their brief and report under this folder of the worktree.
pub const BRIEF_FOLDER: &str = ".herdr-linear-agent";
pub const MAX_RESTARTS: u32 = 2;
/// A blocked agent needs a person after this long.
pub const BLOCKED_SECS: i64 = 30;
/// A launch whose status stays unknown this long is stuck on a dialog.
pub const LAUNCH_DIALOG_SECS: i64 = 60;
/// A `Waiting for you` self-report counts for this long.
pub const SELF_REPORT_SECS: i64 = 5 * 60;
/// How long an idle status right after the launch prompt still counts as
/// the agent picking the prompt up.
const JUST_PROMPTED_SECS: i64 = 60;

/// `workers/<id>.toml`. Every field is defaulted, so records written by an
/// older build still load.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Worker {
    pub id: String,
    pub title: String,
    pub repo: String,
    pub repo_path: String,
    pub branch: String,
    pub base: String,
    pub worktree_path: String,
    pub brief_dir: String,
    pub restarts: u32,
    /// The hash of the report the ticker last copied home.
    pub report_hash: String,
    /// The hash of the report the coordinator was told about.
    pub announced_report_hash: String,
    pub pr_url: String,
    /// The "lost its pane before a report" error was sent.
    pub gone_reported: bool,
    /// The activity of the worker's own progress record last sent to
    /// Linear, and when it was first seen.
    pub activity: String,
    pub activity_since: String,
    /// `worker restart` is moving it to a new pane: the watcher leaves it
    /// alone until a snapshot shows the recorded pane.
    pub restarting: bool,
    pub created: String,
    pub updated: String,
    pub agent: AgentRecord,
}

impl Worker {
    /// Whether the worker counts against `max_agents`.
    pub fn counts(&self) -> bool {
        matches!(self.agent.status, AgentStatus::Pending | AgentStatus::Open)
    }
}

/// `w` and a positive number without a leading zero.
pub fn validate_id(id: &str) -> Result<()> {
    let ok = id.strip_prefix('w').is_some_and(|n| {
        !n.is_empty() && !n.starts_with('0') && n.chars().all(|c| c.is_ascii_digit())
    });
    if !ok {
        bail!("`{id}` is not a worker id (expected w1, w2, ...)");
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

fn read(path: &Path) -> Result<Worker> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{} does not parse", path.display()))
}

fn write(run: &Run, worker: &Worker) -> Result<()> {
    let text = toml::to_string(worker).context("a worker record does not serialize")?;
    write_atomic(&record_path(run, &worker.id), text.as_bytes())
}

pub fn load(run: &Run, id: &str) -> Result<Worker> {
    validate_id(id)?;
    let path = record_path(run, id);
    if !path.is_file() {
        bail!("run {} has no worker {id}", run.key);
    }
    read(&path)
}

/// Every worker of the run, by id number.
pub fn list(run: &Run) -> Vec<Worker> {
    let Ok(entries) = std::fs::read_dir(run.workers_dir()) else {
        return Vec::new();
    };
    let mut workers: Vec<Worker> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".toml")?;
            validate_id(id).ok()?;
            read(&e.path()).ok()
        })
        .collect();
    workers.sort_by_key(|w| number(&w.id));
    workers
}

fn number(id: &str) -> u64 {
    id.trim_start_matches('w').parse().unwrap_or(0)
}

/// Gives the next id under the run lock. `check` sees every existing worker
/// and may refuse; `init` fills the new record before it is written.
pub fn allocate(
    run: &Run,
    check: impl FnOnce(&[Worker]) -> Result<()>,
    init: impl FnOnce(&mut Worker),
) -> Result<Worker> {
    let _lock = run.lock()?;
    let workers = list(run);
    check(&workers)?;
    let next = workers.iter().map(|w| number(&w.id)).max().unwrap_or(0) + 1;
    let now = files::now();
    let mut worker = Worker {
        id: format!("w{next}"),
        created: now.clone(),
        updated: now,
        ..Worker::default()
    };
    init(&mut worker);
    write(run, &worker)?;
    Ok(worker)
}

/// Read-modify-write of one record under the run lock; `created` is kept.
pub fn update(run: &Run, id: &str, change: impl FnOnce(&mut Worker)) -> Result<Worker> {
    let lock = run.lock()?;
    update_held(run, &lock, id, change)
}

/// `update` for a caller that holds the run lock.
pub fn update_held(
    run: &Run,
    _lock: &RunLock,
    id: &str,
    change: impl FnOnce(&mut Worker),
) -> Result<Worker> {
    let mut worker = load(run, id)?;
    let created = worker.created.clone();
    change(&mut worker);
    worker.id = id.to_string();
    worker.created = created;
    worker.updated = files::now();
    write(run, &worker)?;
    Ok(worker)
}

pub fn branch_name(key: &str, id: &str, title: &str) -> String {
    let slug = files::slugify(title);
    let key = key.to_lowercase();
    if slug.is_empty() {
        format!("herdr-linear-agent/{key}/{id}")
    } else {
        format!("herdr-linear-agent/{key}/{id}-{slug}")
    }
}

/// The worker's brief folder, `<worktree>/.herdr-linear-agent/<workspace>-<KEY>-<id>`.
pub fn brief_dir(worktree: &str, key: &str, id: &str) -> String {
    format!(
        "{}/{BRIEF_FOLDER}/{}",
        worktree.trim_end_matches('/'),
        brief_name(key, id)
    )
}

fn brief_name(key: &str, id: &str) -> String {
    format!("{}-{id}", key.replace('/', "-"))
}

pub fn launch_prompt(key: &str, id: &str) -> String {
    format!(
        "Read {BRIEF_FOLDER}/{}/brief.md and do what it says.",
        brief_name(key, id)
    )
}

/// The `Start worker` action, queued when the worker's launch prompt is
/// delivered.
pub fn start_action(worker: &Worker) -> Op {
    Op::activity(Activity::new(Content::Action {
        action: "Start worker".into(),
        parameter: format!("{} {}: {}", worker.id, worker.repo, worker.title),
        result: None,
    }))
}

/// Adds the text as a follow-up to the worker's task file.
pub fn append_follow_up(run: &Run, id: &str, text: &str) -> Result<()> {
    let _lock = run.lock()?;
    let path = task_path(run, id);
    let mut task = std::fs::read_to_string(&path).unwrap_or_default();
    if !task.contains("\n## Follow-ups\n") {
        task = format!("{}\n\n## Follow-ups\n", task.trim_end());
    }
    task.push_str(&format!("\n### {}\n\n{}\n", files::now(), text.trim()));
    write_atomic(&path, task.as_bytes())
}

fn report_path(worker: &Worker) -> PathBuf {
    Path::new(&worker.brief_dir).join("report.md")
}

/// The report's bytes, never through a symbolic link.
fn read_report(worker: &Worker) -> Option<Vec<u8>> {
    let path = report_path(worker);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    std::fs::read(&path).ok()
}

pub fn report_hash(worker: &Worker) -> Option<String> {
    read_report(worker).map(|bytes| files::sha256_hex(&bytes))
}

/// Copies the worker's report to `workers/<id>.md` and returns its text;
/// `None` when there is no report.
pub fn copy_report_home(run: &Run, worker: &Worker) -> Result<Option<String>> {
    let Some(bytes) = read_report(worker) else {
        return Ok(None);
    };
    write_atomic(&home_report_path(run, &worker.id), &bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The report's `## Report` section as written, without its heading, in
/// whole lines of at most 1,500 characters together; empty when there is
/// none.
pub fn report_section(report: &str) -> String {
    let mut text = String::new();
    for line in report
        .lines()
        .skip_while(|l| l.trim() != "## Report")
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
    {
        if text.chars().count() + line.chars().count() > 1500 {
            text.push_str("…\n");
            break;
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text.trim_matches('\n').to_string()
}

/// The pull request of the report's first `PR:` line, when it names one.
pub fn pr_line(report: &str) -> Option<String> {
    let url = report
        .lines()
        .find_map(|l| l.trim().strip_prefix("PR:"))?
        .trim();
    let rest = url.strip_prefix("https://github.com/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let [owner, repo, "pull", number] = parts.as_slice() else {
        return None;
    };
    let name = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let valid = name(owner)
        && name(repo)
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit());
    valid.then(|| url.to_string())
}

pub struct BriefInput<'a> {
    pub issue_key: &'a str,
    pub issue_title: &'a str,
    pub issue_url: &'a str,
    pub worker: &'a Worker,
    pub task: &'a str,
    pub restart: bool,
    pub binary: &'a str,
    /// The worker profile's instruction layers, added after the built-in rules.
    pub instructions: &'a [crate::config::Instructions],
}

pub fn compose_brief(input: &BriefInput) -> String {
    let w = input.worker;
    let mut text = format!(
        "# Worker brief\n\n\
         - Issue: {key} {title}\n\
         - URL: {url}\n\
         - Repository: {repo}\n\
         - Worktree: {worktree}\n\
         - Branch: {branch}\n\
         - Base: {base}\n\
         - Report: {report}\n\n",
        key = input.issue_key,
        title = input.issue_title.replace('\n', " "),
        url = input.issue_url,
        repo = w.repo,
        worktree = w.worktree_path,
        branch = w.branch,
        base = w.base,
        report = report_path(w).display(),
    );
    if input.restart {
        text.push_str(
            "This is a restart. A previous attempt already worked in this worktree: read the current state of the worktree and of the report before you go on.\n\n",
        );
    }
    text.push_str(include_str!("../assets/WORKER.md").trim_end());
    text.push_str(&crate::coordinator::profile_section(
        &w.agent.profile,
        "the rules above",
        input.instructions,
    ));
    text.push_str(&format!(
        "\n\n## Progress\n\n\
         Tell the plugin how far you are whenever your activity changes:\n\n\
         ```sh\n{bin} report --percent N --activity 'Two to four words'\n```\n\n\
         Use `--unknown` instead of `--percent N` while the scope is unclear, and `--activity '{waiting}'` when you stop with a question.\n\n\
         ## Task\n\n{task}\n",
        bin = input.binary,
        waiting = progress::WAITING,
        task = input.task.trim(),
    ));
    text
}

/// Coordinators and workers of active runs that count against `max_agents`.
pub fn agent_count(runs: &[Run]) -> usize {
    runs.iter()
        .filter_map(|run| run.record().ok().map(|r| (run, r)))
        .filter(|(_, r)| r.status == Status::Active)
        .map(|(run, r)| {
            usize::from(matches!(
                r.coordinator.status,
                AgentStatus::Pending | AgentStatus::Open
            )) + list(run).iter().filter(|w| w.counts()).count()
        })
        .sum()
}

/// Herdr's agent in the recorded pane, when it is the recorded agent: pane,
/// working directory and kind agree, and the name does too or is empty (a
/// natively resumed agent loses its name).
pub fn find_agent<'a>(record: &AgentRecord, agents: &'a [Agent]) -> Option<&'a Agent> {
    agents.iter().find(|a| {
        a.pane.0 == record.pane_id
            && same_place(record, a)
            && a.name
                .as_deref()
                .is_none_or(|n| n.is_empty() || n == record.agent_name)
    })
}

/// Herdr reports an agent it is still launching without a kind, and reports
/// directories with symlinks resolved, so neither may count as a mismatch.
fn same_place(record: &AgentRecord, agent: &Agent) -> bool {
    let kind = agent.kind.as_deref().is_none_or(|k| k == record.kind);
    let resolved = std::fs::canonicalize(&record.cwd).ok();
    let here = |dir: &String| {
        *dir == record.cwd
            || resolved
                .as_ref()
                .is_some_and(|r| std::fs::canonicalize(dir).ok().as_ref() == Some(r))
    };
    kind && [&agent.cwd, &agent.foreground_cwd]
        .into_iter()
        .flatten()
        .any(here)
}

/// The recorded agent in another pane: Herdr renumbered it.
fn find_moved<'a>(record: &AgentRecord, agents: &'a [Agent]) -> Option<&'a Agent> {
    if record.agent_name.is_empty() {
        return None;
    }
    agents.iter().find(|a| {
        a.pane.0 != record.pane_id
            && same_place(record, a)
            && a.name.as_deref() == Some(record.agent_name.as_str())
    })
}

/// What a snapshot shows of one recorded agent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Live {
    /// Our agent is in its pane or moved, or its pane waits at a shell prompt.
    pub pane_exists: bool,
    /// `(workspace, tab, pane)` when Herdr moved our agent to another pane.
    pub moved_to: Option<(String, String, String)>,
    /// The agent found by the identity rule.
    pub agent: Option<Agent>,
    pub agent_state: Option<HerdrStatus>,
    /// Seconds in the current status, when it is the recorded one.
    pub state_secs: i64,
    /// The worker's own progress record, when it is recent.
    pub self_report: Option<progress::Record>,
    /// The launch prompt went out less than a minute ago and the agent has
    /// not changed state since, so an idle status is from before the prompt.
    pub just_prompted: bool,
}

pub fn live_state(
    record: &AgentRecord,
    snapshot: &Snapshot,
    now: Timestamp,
    state_dir: &Path,
    socket: &str,
) -> Live {
    let mut live = Live::default();
    let pane = if let Some(agent) = find_agent(record, &snapshot.agents) {
        live.agent = Some(agent.clone());
        Some(agent.pane.clone())
    } else if let Some(agent) = find_moved(record, &snapshot.agents) {
        live.agent = Some(agent.clone());
        if let Some(pane) = snapshot.panes.get(&agent.pane) {
            live.moved_to = Some((
                pane.workspace.0.clone(),
                pane.tab.clone(),
                pane.id.0.clone(),
            ));
        }
        Some(agent.pane.clone())
    } else {
        let id = crate::herdr::PaneId(record.pane_id.clone());
        let empty = !snapshot.agents.iter().any(|a| a.pane == id);
        (empty && snapshot.panes.contains_key(&id)).then_some(id)
    };
    let Some(pane) = pane else {
        return live;
    };
    live.pane_exists = true;
    live.agent_state = live.agent.as_ref().map(|a| a.status);
    live.just_prompted = live.agent.as_ref().is_some_and(|a| {
        !record.prompted_at.is_empty()
            && a.state_change_seq == record.prompted_seq
            && files::seconds_since(&record.prompted_at, now) < JUST_PROMPTED_SECS
    });
    if let Some(status) = live.agent_state
        && status.as_str() == record.last_state
    {
        live.state_secs = files::seconds_since(&record.last_state_change, now);
    }
    let terminal = snapshot
        .panes
        .get(&pane)
        .map_or("", |p| p.terminal.as_str());
    live.self_report = progress::self_report(state_dir, socket, &pane.0, terminal)
        .filter(|r| now.as_second() - r.reported_at < SELF_REPORT_SECS);
    live
}

/// Blocked for 30 s, or a launch stuck on a dialog for 60 s.
pub fn needs_person(record: &AgentRecord, live: &Live) -> bool {
    match live.agent_state {
        Some(HerdrStatus::Blocked) => live.state_secs >= BLOCKED_SECS,
        Some(HerdrStatus::Unknown) => {
            record.prompt_pending && live.state_secs >= LAUNCH_DIALOG_SECS
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    WaitingOnYou,
    Working,
    Idle,
    Reported,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::WaitingOnYou => "Waiting on you",
            Group::Working => "Working",
            Group::Idle => "Idle",
            Group::Reported => "Reported",
        }
    }

    /// The value stored in `last_group`.
    pub fn token(self) -> &'static str {
        match self {
            Group::WaitingOnYou => "waiting_on_you",
            Group::Working => "working",
            Group::Idle => "idle",
            Group::Reported => "reported",
        }
    }

    /// An unknown stored value reads as none, so the next group is a change.
    pub fn from_token(token: &str) -> Option<Group> {
        [
            Group::WaitingOnYou,
            Group::Working,
            Group::Idle,
            Group::Reported,
        ]
        .into_iter()
        .find(|g| g.token() == token)
    }
}

/// The first row that holds, in order.
pub fn group(worker: &Worker, live: &Live) -> Group {
    let reported = !worker.report_hash.is_empty();
    let asked = live
        .self_report
        .as_ref()
        .is_some_and(progress::Record::waiting)
        && live.agent_state != Some(HerdrStatus::Working);
    if worker.agent.status == AgentStatus::Failed {
        Group::WaitingOnYou
    } else if !live.pane_exists {
        if reported {
            Group::Reported
        } else {
            Group::WaitingOnYou
        }
    } else if needs_person(&worker.agent, live) || asked {
        Group::WaitingOnYou
    } else if !live.agent_state.is_some_and(HerdrStatus::is_idle) {
        Group::Working
    } else if reported {
        Group::Reported
    } else if worker.agent.prompt_pending
        || live.just_prompted
        || live
            .self_report
            .as_ref()
            .is_some_and(|r| r.percent != Some(100))
    {
        // Idle without a report, but not stuck: the prompt has not reached
        // the agent yet, or its own progress record says it is under way.
        Group::Working
    } else {
        Group::Idle
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use jiff::SignedDuration;

    use super::*;
    use crate::herdr::{Pane, PaneId, WorkspaceId};
    use crate::run::RunRecord;

    const SOCKET: &str = "/tmp/herdr-work.sock";
    const WORKTREE: &str = "/wt/api";

    fn noon() -> Timestamp {
        "2026-09-28T12:00:00Z".parse().unwrap()
    }

    fn secs_before_noon(secs: i64) -> String {
        noon()
            .checked_sub(SignedDuration::from_secs(secs))
            .unwrap()
            .to_string()
    }

    struct Runs {
        home: tempfile::TempDir,
    }

    impl Runs {
        fn new() -> Self {
            Runs {
                home: tempfile::tempdir().unwrap(),
            }
        }

        fn dir(&self) -> PathBuf {
            self.home.path().join("runs")
        }

        fn add(&self, key: &str, status: Status, coordinator: AgentStatus) -> Run {
            let record = RunRecord {
                workspace: "acme".into(),
                issue_id: format!("uuid-{key}"),
                identifier: key.into(),
                title: "Fix login".into(),
                status,
                coordinator: AgentRecord {
                    status: coordinator,
                    ..AgentRecord::default()
                },
                ..RunRecord::default()
            };
            Run::create(&self.dir(), record).unwrap()
        }
    }

    fn api_worker_agent(pane: &str) -> AgentRecord {
        AgentRecord {
            status: AgentStatus::Open,
            profile: "standard".into(),
            kind: "claude".into(),
            agent_name: "acme-data-1-w1".into(),
            pane_id: pane.into(),
            cwd: WORKTREE.into(),
            ..AgentRecord::default()
        }
    }

    /// A pane as `session.snapshot` lists it; its workspace is the id's
    /// prefix and its terminal `term-<pane>`.
    fn listed_pane(id: &str) -> (PaneId, Pane) {
        let workspace = id.split(':').next().unwrap();
        let pane = Pane {
            id: PaneId(id.into()),
            workspace: WorkspaceId(workspace.into()),
            tab: format!("{workspace}:t1"),
            terminal: format!("term-{id}"),
            cwd: Some(WORKTREE.into()),
            foreground_cwd: None,
            label: None,
        };
        (pane.id.clone(), pane)
    }

    fn herdr_sees(
        pane: &str,
        name: Option<&str>,
        cwd: &str,
        kind: &str,
        status: HerdrStatus,
    ) -> Agent {
        Agent {
            pane: PaneId(pane.into()),
            kind: Some(kind.into()),
            name: name.map(Into::into),
            status,
            session: Some("sess-w1".into()),
            cwd: Some(cwd.into()),
            foreground_cwd: None,
            terminal: format!("term-{pane}"),
            interactive_ready: true,
            launch_pending: false,
            state_change_seq: 2,
            state_labels: BTreeMap::new(),
        }
    }

    fn ours_in(pane: &str, status: HerdrStatus) -> Agent {
        herdr_sees(pane, Some("acme-data-1-w1"), WORKTREE, "claude", status)
    }

    fn session_with(panes: &[&str], agents: Vec<Agent>) -> Snapshot {
        Snapshot {
            version: "0.9.1".into(),
            protocol: 22,
            panes: panes.iter().map(|p| listed_pane(p)).collect(),
            agents,
            skipped: 0,
        }
    }

    fn api_worker(status: AgentStatus, report_hash: &str, prompt_pending: bool) -> Worker {
        Worker {
            id: "w1".into(),
            title: "Change API".into(),
            repo: "api".into(),
            report_hash: report_hash.into(),
            agent: AgentRecord {
                status,
                prompt_pending,
                ..api_worker_agent("w1:p1")
            },
            ..Worker::default()
        }
    }

    fn in_pane(status: Option<HerdrStatus>, secs: i64) -> Live {
        Live {
            pane_exists: true,
            agent_state: status,
            state_secs: secs,
            ..Live::default()
        }
    }

    #[test]
    fn validate_id_accepts_w_and_a_number_without_leading_zero() {
        for good in ["w1", "w12", "w307"] {
            assert!(validate_id(good).is_ok(), "{good}");
        }
        for bad in ["", "w", "w0", "w01", "x1", "../w1", "w1a", "W1", "w-1"] {
            assert!(validate_id(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            validate_id("w0").unwrap_err().to_string(),
            "`w0` is not a worker id (expected w1, w2, ...)"
        );
    }

    #[test]
    fn branches_briefs_and_prompts_are_named_from_the_key_and_id() {
        let names = [
            (
                "acme/DATA-1",
                "w1",
                "Change API",
                "herdr-linear-agent/acme/data-1/w1-change-api",
            ),
            (
                "beta/DATA-12",
                "w3",
                "Fix the $(login) bug!",
                "herdr-linear-agent/beta/data-12/w3-fix-the-login-bug",
            ),
            (
                "acme/DATA-1",
                "w2",
                "???",
                "herdr-linear-agent/acme/data-1/w2",
            ),
        ];
        for (key, id, title, branch) in names {
            assert_eq!(branch_name(key, id, title), branch, "{title}");
        }
        for worktree in ["/wt/api", "/wt/api/", "/wt/api//"] {
            assert_eq!(
                brief_dir(worktree, "acme/DATA-1", "w1"),
                "/wt/api/.herdr-linear-agent/acme-DATA-1-w1"
            );
        }
        assert_eq!(
            launch_prompt("acme/DATA-1", "w2"),
            "Read .herdr-linear-agent/acme-DATA-1-w2/brief.md and do what it says."
        );

        let Op::Activity { activity, .. } = start_action(&api_worker(AgentStatus::Open, "", true))
        else {
            panic!("Start worker is an activity");
        };
        let Content::Action {
            action,
            parameter,
            result,
        } = activity.content
        else {
            panic!("Start worker is an action");
        };
        assert_eq!(action, "Start worker");
        assert_eq!(parameter, "w1 api: Change API");
        assert_eq!(result, None);
    }

    #[test]
    fn a_brief_puts_heading_restart_note_rules_report_command_and_task_in_order() {
        let worker = Worker {
            repo: "api".into(),
            branch: "herdr-linear-agent/acme/data-1/w1-change-api".into(),
            base: "main".into(),
            worktree_path: WORKTREE.into(),
            brief_dir: brief_dir(WORKTREE, "acme/DATA-1", "w1"),
            ..api_worker(AgentStatus::Open, "", true)
        };
        let brief = |restart| {
            compose_brief(&BriefInput {
                issue_key: "DATA-1",
                issue_title: "Fix\nlogin",
                issue_url: "https://linear.app/acme/issue/DATA-1",
                worker: &worker,
                task: "\n  Change the session handler.  \n",
                restart,
                binary: "/bin/herdr-linear-agent",
                instructions: &[],
            })
        };
        let rules = include_str!("../assets/WORKER.md").lines().next().unwrap();
        let restarted = brief(true);
        let mut from = 0;
        for needle in [
            "# Worker brief",
            "DATA-1 Fix login",
            "https://linear.app/acme/issue/DATA-1",
            "api",
            WORKTREE,
            "herdr-linear-agent/acme/data-1/w1-change-api",
            "main",
            "/wt/api/.herdr-linear-agent/acme-DATA-1-w1/report.md",
            "previous attempt",
            rules,
            "/bin/herdr-linear-agent report --percent N",
            "--activity",
            "Change the session handler.",
        ] {
            let at = restarted[from..]
                .find(needle)
                .unwrap_or_else(|| panic!("`{needle}` is not after byte {from}"));
            from += at + needle.len();
        }
        assert!(restarted.ends_with("Change the session handler.\n"));
        assert!(!brief(false).contains("previous attempt"));
        assert!(!restarted.contains("## Profile instructions"));

        let with_own = compose_brief(&BriefInput {
            issue_key: "DATA-1",
            issue_title: "Fix login",
            issue_url: "https://linear.app/acme/issue/DATA-1",
            worker: &worker,
            task: "Change the session handler.",
            restart: false,
            binary: "/bin/herdr-linear-agent",
            instructions: &[crate::config::Instructions {
                profile: "standard".into(),
                text: "Run `make check` before you commit.\n".into(),
            }],
        });
        let last_rule = include_str!("../assets/WORKER.md")
            .trim_end()
            .lines()
            .last()
            .unwrap();
        let rules_end = with_own.find(last_rule).unwrap() + last_rule.len();
        let own = with_own.find("## Profile instructions").unwrap();
        let task = with_own.find("## Task").unwrap();
        assert!(rules_end < own && own < task, "{with_own}");
        assert!(with_own.contains(
            "These come from the `standard` profile in the plugin's config and add to the rules above. \
             Where they disagree with the rules above, follow the rules above.\n\nRun `make check` before you commit.\n"
        ));
    }

    #[test]
    fn several_instruction_layers_go_between_the_rules_and_the_progress_section() {
        let worker = Worker {
            brief_dir: brief_dir(WORKTREE, "acme/DATA-1", "w1"),
            ..api_worker(AgentStatus::Open, "", true)
        };
        let layers = [
            crate::config::Instructions {
                profile: "base".into(),
                text: "Run `make check`.\n".into(),
            },
            crate::config::Instructions {
                profile: "standard".into(),
                text: "Keep the change small.\n".into(),
            },
        ];
        let brief = compose_brief(&BriefInput {
            issue_key: "DATA-1",
            issue_title: "Fix login",
            issue_url: "https://linear.app/acme/issue/DATA-1",
            worker: &worker,
            task: "Change the session handler.",
            restart: false,
            binary: "/bin/herdr-linear-agent",
            instructions: &layers,
        });
        let last_rule = include_str!("../assets/WORKER.md")
            .trim_end()
            .lines()
            .last()
            .unwrap();
        let rules_end = brief.find(last_rule).unwrap() + last_rule.len();
        let (root, own) = (
            brief.find("### From the `base` profile").unwrap(),
            brief.find("### From the `standard` profile").unwrap(),
        );
        let progress = brief.find("## Progress").unwrap();
        assert!(rules_end < root && root < own && own < progress, "{brief}");
    }

    #[test]
    fn a_pr_counts_only_on_the_first_pr_line_naming_a_github_pull_request() {
        let pull = Some("https://github.com/acme/api/pull/7");
        let table = [
            ("Done.\nPR: https://github.com/acme/api/pull/7\n", pull),
            ("   PR:   https://github.com/acme/api/pull/7   ", pull),
            (
                "PR: https://github.com/my-org/api.v2/pull/1234",
                Some("https://github.com/my-org/api.v2/pull/1234"),
            ),
            ("PR: https://gitlab.com/acme/api/pull/7", None),
            ("PR: http://github.com/acme/api/pull/7", None),
            ("PR: https://github.com/acme/api/issues/7", None),
            ("PR: https://github.com/acme/api/pull/7/files", None),
            ("PR: https://github.com/acme/api/pull/7 (draft)", None),
            ("PR: https://github.com/acme/api/pull/", None),
            ("PR: https://github.com/acme/api/pull/x7", None),
            ("See PR: https://github.com/acme/api/pull/7", None),
            ("PR: none yet\nPR: https://github.com/acme/api/pull/8", None),
            ("No pull request.", None),
        ];
        for (report, expected) in table {
            assert_eq!(pr_line(report).as_deref(), expected, "{report:?}");
        }
    }

    #[test]
    fn the_report_section_keeps_its_lines_and_stops_at_the_next_heading() {
        let long = format!("## Report\n- {}\n- {}\n", "a".repeat(900), "b".repeat(900));
        let table = [
            (
                "PR: x\n## Report\n\n- Changed the login.\n  - and its test\n\nCI passed.  \n## Next\n- Review\n",
                "- Changed the login.\n  - and its test\n\nCI passed.".to_string(),
            ),
            ("## Report\nDone.\n## Next\n- Review\n", "Done.".into()),
            ("## Report\n## Next\n- Review\n", String::new()),
            ("No sections.", String::new()),
            (&long, format!("- {}\n…", "a".repeat(900))),
        ];
        for (report, expected) in table {
            assert_eq!(report_section(report), expected, "{report:?}");
        }
    }

    #[test]
    fn allocation_hands_out_ids_and_a_refusal_writes_nothing() {
        let runs = Runs::new();
        let run = runs.add("DATA-1", Status::Active, AgentStatus::Open);
        let first = allocate(
            &run,
            |existing| {
                assert!(existing.is_empty());
                Ok(())
            },
            |w| w.repo = "api".into(),
        )
        .unwrap();
        assert_eq!((first.id.as_str(), first.repo.as_str()), ("w1", "api"));
        assert!(!first.created.is_empty());
        assert_eq!(first.created, first.updated);
        assert!(run.dir.join("workers/w1.toml").is_file());

        let second = allocate(
            &run,
            |existing| {
                assert_eq!(existing.len(), 1);
                Ok(())
            },
            |w| w.repo = "web".into(),
        )
        .unwrap();
        assert_eq!(second.id, "w2");

        let refused = allocate(
            &run,
            |_| bail!("one worker per repository"),
            |_| panic!("init runs only after the check passed"),
        );
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("one worker per repository")
        );
        assert_eq!(list(&run).len(), 2);
        assert_eq!(load(&run, "w2").unwrap().repo, "web");
        assert_eq!(
            load(&run, "w9").unwrap_err().to_string(),
            "run acme/DATA-1 has no worker w9"
        );
        assert!(load(&run, "../w1").is_err());
    }

    #[test]
    fn ids_sort_by_number_and_updates_keep_id_and_created() {
        let runs = Runs::new();
        let run = runs.add("DATA-1", Status::Active, AgentStatus::Open);
        for _ in 0..10 {
            allocate(&run, |_| Ok(()), |_| {}).unwrap();
        }
        let ids: Vec<String> = list(&run).into_iter().map(|w| w.id).collect();
        assert_eq!(ids[8..], ["w9", "w10"]);

        let before = load(&run, "w3").unwrap();
        let after = update(&run, "w3", |w| {
            w.pr_url = "https://github.com/acme/api/pull/3".into();
            w.id = "w99".into();
            w.created = "1999-01-01T00:00:00Z".into();
        })
        .unwrap();
        assert_eq!(after.id, "w3");
        assert_eq!(after.created, before.created);
        assert_eq!(load(&run, "w3").unwrap(), after);
        assert!(load(&run, "w99").is_err());
    }

    #[test]
    fn follow_ups_share_one_heading_in_the_task_file() {
        let runs = Runs::new();
        let run = runs.add("DATA-1", Status::Active, AgentStatus::Open);
        assert_eq!(task_path(&run, "w1"), run.dir.join("workers/w1.task.md"));
        assert_eq!(home_report_path(&run, "w1"), run.dir.join("workers/w1.md"));

        std::fs::write(task_path(&run, "w1"), "Change the handler.\n").unwrap();
        append_follow_up(&run, "w1", "  Also cover the redirect.  ").unwrap();
        append_follow_up(&run, "w1", "Rename the flag.").unwrap();
        let task = std::fs::read_to_string(task_path(&run, "w1")).unwrap();
        assert!(
            task.starts_with("Change the handler.\n\n## Follow-ups\n"),
            "{task}"
        );
        assert_eq!(task.matches("## Follow-ups").count(), 1);
        assert_eq!(task.matches("\n### ").count(), 2);
        let first = task.find("\n\nAlso cover the redirect.\n").unwrap();
        let second = task.find("\n\nRename the flag.\n").unwrap();
        assert!(first < second);
        assert!(task.ends_with("Rename the flag.\n"));
    }

    #[test]
    fn reports_are_hashed_and_copied_home_never_through_a_link() {
        let runs = Runs::new();
        let run = runs.add("DATA-1", Status::Active, AgentStatus::Open);
        let brief = runs.home.path().join("wt/.herdr-linear-agent/DATA-1-w1");
        std::fs::create_dir_all(&brief).unwrap();
        let worker = Worker {
            brief_dir: brief.to_string_lossy().into_owned(),
            ..api_worker(AgentStatus::Open, "", false)
        };
        let home = home_report_path(&run, "w1");

        assert_eq!(report_hash(&worker), None);
        assert_eq!(copy_report_home(&run, &worker).unwrap(), None);
        assert!(!home.exists());

        let text = "Done.\nPR: https://github.com/acme/api/pull/7\n";
        std::fs::write(brief.join("report.md"), text).unwrap();
        let hash = report_hash(&worker).unwrap();
        assert_eq!(hash, files::sha256_hex(text.as_bytes()));
        assert_eq!(hash.len(), 64);
        assert_eq!(
            copy_report_home(&run, &worker).unwrap().as_deref(),
            Some(text)
        );
        assert_eq!(std::fs::read_to_string(&home).unwrap(), text);

        let secret = runs.home.path().join("secret.txt");
        std::fs::write(&secret, "not a report").unwrap();
        std::fs::remove_file(brief.join("report.md")).unwrap();
        std::fs::remove_file(&home).unwrap();
        std::os::unix::fs::symlink(&secret, brief.join("report.md")).unwrap();
        assert_eq!(report_hash(&worker), None);
        assert_eq!(copy_report_home(&run, &worker).unwrap(), None);
        assert!(!home.exists());
    }

    #[test]
    fn pending_and_open_agents_of_active_runs_count_against_max_agents() {
        for (status, counts) in [
            (AgentStatus::Pending, true),
            (AgentStatus::Open, true),
            (AgentStatus::Failed, false),
            (AgentStatus::Stopped, false),
        ] {
            assert_eq!(api_worker(status, "", false).counts(), counts, "{status:?}");
        }

        let runs = Runs::new();
        let busy = runs.add("DATA-1", Status::Active, AgentStatus::Open);
        for status in [AgentStatus::Open, AgentStatus::Failed, AgentStatus::Pending] {
            allocate(&busy, |_| Ok(()), |w| w.agent.status = status).unwrap();
        }
        let detached = runs.add("DATA-2", Status::Detached, AgentStatus::Open);
        allocate(
            &detached,
            |_| Ok(()),
            |w| w.agent.status = AgentStatus::Open,
        )
        .unwrap();
        runs.add("DATA-3", Status::Active, AgentStatus::Stopped);
        runs.add("DATA-4", Status::Active, AgentStatus::Pending);
        assert_eq!(agent_count(&Run::list(&runs.dir())), 4);
    }

    #[test]
    fn records_written_before_this_build_still_load() {
        let runs = Runs::new();
        let run = runs.add("DATA-1", Status::Active, AgentStatus::Open);
        let stored = r#"id = "w1"
title = "Change API"
repo = "api"
repo_path = "/src/api"
branch = "herdr-linear-agent/acme/data-1/w1-change-api"
base = "main"
worktree_path = "/wt/api"
brief_dir = "/wt/api/.herdr-linear-agent/DATA-1-w1"
restarts = 1
report_hash = "5e1f"
pr_url = "https://github.com/acme/api/pull/7"
created = "2026-05-01T08:00:00Z"
updated = "2026-05-01T09:30:00Z"

[agent]
status = "open"
profile = "standard"
kind = "claude"
agent_name = "acme-data-1-w1"
pane_id = "w1:p1"
cwd = "/wt/api"
last_group = "waiting"
"#;
        std::fs::write(run.dir.join("workers/w1.toml"), stored).unwrap();
        let worker = load(&run, "w1").unwrap();
        assert_eq!(
            worker.branch,
            "herdr-linear-agent/acme/data-1/w1-change-api"
        );
        assert_eq!(worker.restarts, 1);
        assert_eq!(worker.pr_url, "https://github.com/acme/api/pull/7");
        assert_eq!(worker.announced_report_hash, "");
        assert!(!worker.gone_reported);
        assert_eq!(worker.agent.status, AgentStatus::Open);
        assert_eq!(worker.agent.pane_id, "w1:p1");
        assert_eq!(Group::from_token(&worker.agent.last_group), None);
        assert_eq!(list(&run), std::slice::from_ref(&worker));

        let rewritten = update(&run, "w1", |w| w.agent.last_group = "working".into()).unwrap();
        assert_eq!(rewritten.created, "2026-05-01T08:00:00Z");
        assert_eq!(load(&run, "w1").unwrap(), rewritten);
    }

    #[test]
    fn the_recorded_agent_needs_pane_cwd_kind_and_name_to_agree() {
        let record = api_worker_agent("w1:p1");
        let table = [
            (
                "all agree",
                herdr_sees(
                    "w1:p1",
                    Some("acme-data-1-w1"),
                    WORKTREE,
                    "claude",
                    HerdrStatus::Idle,
                ),
                true,
            ),
            (
                "resumed with an empty name",
                herdr_sees("w1:p1", Some(""), WORKTREE, "claude", HerdrStatus::Idle),
                true,
            ),
            (
                "resumed without a name",
                herdr_sees("w1:p1", None, WORKTREE, "claude", HerdrStatus::Idle),
                true,
            ),
            (
                "another name",
                herdr_sees(
                    "w1:p1",
                    Some("acme-data-1-w2"),
                    WORKTREE,
                    "claude",
                    HerdrStatus::Idle,
                ),
                false,
            ),
            (
                "another cwd",
                herdr_sees(
                    "w1:p1",
                    Some("acme-data-1-w1"),
                    "/wt/web",
                    "claude",
                    HerdrStatus::Idle,
                ),
                false,
            ),
            (
                "another kind",
                herdr_sees(
                    "w1:p1",
                    Some("acme-data-1-w1"),
                    WORKTREE,
                    "codex",
                    HerdrStatus::Idle,
                ),
                false,
            ),
            (
                "another pane",
                herdr_sees(
                    "w4:p1",
                    Some("acme-data-1-w1"),
                    WORKTREE,
                    "claude",
                    HerdrStatus::Idle,
                ),
                false,
            ),
            (
                "still launching, kind not detected yet",
                Agent {
                    kind: None,
                    launch_pending: true,
                    ..ours_in("w1:p1", HerdrStatus::Unknown)
                },
                true,
            ),
            (
                "only the foreground cwd is the worktree",
                Agent {
                    cwd: Some("/elsewhere".into()),
                    foreground_cwd: Some(WORKTREE.into()),
                    ..ours_in("w1:p1", HerdrStatus::Working)
                },
                true,
            ),
        ];
        for (case, seen, ours) in table {
            let found = find_agent(&record, std::slice::from_ref(&seen));
            assert_eq!(found.is_some(), ours, "{case}");
        }
    }

    #[test]
    fn a_cwd_herdr_reports_with_symlinks_resolved_is_the_same_place() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("run");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let record = AgentRecord {
            cwd: link.to_string_lossy().into_owned(),
            ..api_worker_agent("w1:p1")
        };
        let seen = Agent {
            cwd: Some(real.canonicalize().unwrap().to_string_lossy().into_owned()),
            ..ours_in("w1:p1", HerdrStatus::Idle)
        };
        assert!(find_agent(&record, std::slice::from_ref(&seen)).is_some());
    }

    #[test]
    fn a_snapshot_shows_where_the_agent_sits_and_how_long_its_status_held() {
        let state = tempfile::tempdir().unwrap();
        let record = AgentRecord {
            last_state: "blocked".into(),
            last_state_change: secs_before_noon(45),
            ..api_worker_agent("w1:p1")
        };
        let read =
            |snapshot: Snapshot| live_state(&record, &snapshot, noon(), state.path(), SOCKET);

        let home = read(session_with(
            &["w1:p1"],
            vec![ours_in("w1:p1", HerdrStatus::Blocked)],
        ));
        assert!(home.pane_exists);
        assert_eq!(home.moved_to, None);
        assert_eq!(home.agent_state, Some(HerdrStatus::Blocked));
        assert_eq!(home.state_secs, 45);
        assert_eq!(home.agent.unwrap().name.as_deref(), Some("acme-data-1-w1"));

        let changed = read(session_with(
            &["w1:p1"],
            vec![ours_in("w1:p1", HerdrStatus::Working)],
        ));
        assert_eq!(changed.agent_state, Some(HerdrStatus::Working));
        assert_eq!(changed.state_secs, 0);

        let moved = read(session_with(
            &["w3:p2"],
            vec![ours_in("w3:p2", HerdrStatus::Blocked)],
        ));
        assert!(moved.pane_exists);
        assert_eq!(
            moved.moved_to,
            Some(("w3".into(), "w3:t1".into(), "w3:p2".into()))
        );
        assert_eq!(moved.state_secs, 45);

        let stranger = herdr_sees(
            "w1:p1",
            Some("someone-else"),
            WORKTREE,
            "claude",
            HerdrStatus::Idle,
        );
        let foreign = read(session_with(&["w1:p1"], vec![stranger]));
        assert!(!foreign.pane_exists);
        assert_eq!(foreign.agent_state, None);
        assert_eq!(foreign.agent, None);

        let at_prompt = read(session_with(&["w1:p1"], vec![]));
        assert!(at_prompt.pane_exists);
        assert_eq!(at_prompt.agent_state, None);

        assert_eq!(read(session_with(&[], vec![])), Live::default());
    }

    #[test]
    fn a_waiting_self_report_counts_while_recent_and_not_working() {
        let table = [
            (
                60,
                "term-w1:p1",
                HerdrStatus::Idle,
                true,
                Group::WaitingOnYou,
            ),
            (
                299,
                "term-w1:p1",
                HerdrStatus::Idle,
                true,
                Group::WaitingOnYou,
            ),
            (300, "term-w1:p1", HerdrStatus::Idle, false, Group::Idle),
            (60, "term-before", HerdrStatus::Idle, false, Group::Idle),
            (60, "term-w1:p1", HerdrStatus::Working, true, Group::Working),
        ];
        for (age, terminal, status, recent, expected) in table {
            let state = tempfile::tempdir().unwrap();
            progress::save(
                state.path(),
                &progress::Record {
                    socket: SOCKET.into(),
                    pane_id: "w1:p1".into(),
                    terminal_id: terminal.into(),
                    activity: progress::WAITING.into(),
                    percent: None,
                    reported_at: noon().as_second() - age,
                },
            )
            .unwrap();
            let worker = api_worker(AgentStatus::Open, "", false);
            let snapshot = session_with(&["w1:p1"], vec![ours_in("w1:p1", status)]);
            let seen = live_state(&worker.agent, &snapshot, noon(), state.path(), SOCKET);
            let case = format!("{age} s old from {terminal}, {status:?}");
            assert_eq!(seen.self_report.is_some(), recent, "{case}");
            assert_eq!(group(&worker, &seen), expected, "{case}");
        }
    }

    #[test]
    fn the_first_matching_row_decides_the_group() {
        use AgentStatus::{Failed, Open};
        use HerdrStatus::{Blocked, Done, Idle, Unknown, Working};
        let gone = Live::default();
        let asked = Live {
            self_report: Some(progress::Record {
                activity: progress::WAITING.into(),
                ..progress::Record::default()
            }),
            ..in_pane(Some(Idle), 5)
        };
        let table = [
            (
                "failed beats a report",
                api_worker(Failed, "h", false),
                in_pane(Some(Idle), 0),
                Group::WaitingOnYou,
            ),
            (
                "pane gone after a report",
                api_worker(Open, "h", false),
                gone.clone(),
                Group::Reported,
            ),
            (
                "pane gone before a report",
                api_worker(Open, "", false),
                gone,
                Group::WaitingOnYou,
            ),
            (
                "blocked for 30 s",
                api_worker(Open, "", false),
                in_pane(Some(Blocked), 30),
                Group::WaitingOnYou,
            ),
            (
                "blocked for 29 s",
                api_worker(Open, "", false),
                in_pane(Some(Blocked), 29),
                Group::Working,
            ),
            (
                "launch dialog for 60 s",
                api_worker(Open, "", true),
                in_pane(Some(Unknown), 60),
                Group::WaitingOnYou,
            ),
            (
                "launch dialog for 59 s",
                api_worker(Open, "", true),
                in_pane(Some(Unknown), 59),
                Group::Working,
            ),
            (
                "unknown after the prompt",
                api_worker(Open, "", false),
                in_pane(Some(Unknown), 600),
                Group::Working,
            ),
            (
                "asked in its self-report",
                api_worker(Open, "h", false),
                asked,
                Group::WaitingOnYou,
            ),
            (
                "working",
                api_worker(Open, "h", false),
                in_pane(Some(Working), 900),
                Group::Working,
            ),
            (
                "no agent yet",
                api_worker(Open, "", true),
                in_pane(None, 0),
                Group::Working,
            ),
            (
                "idle with a report",
                api_worker(Open, "h", false),
                in_pane(Some(Idle), 0),
                Group::Reported,
            ),
            (
                "done with a report",
                api_worker(Open, "h", false),
                in_pane(Some(Done), 0),
                Group::Reported,
            ),
            (
                "idle before its launch prompt went out",
                api_worker(Open, "", true),
                in_pane(Some(Idle), 0),
                Group::Working,
            ),
            (
                "idle right after its launch prompt",
                api_worker(Open, "", false),
                Live {
                    just_prompted: true,
                    ..in_pane(Some(Idle), 0)
                },
                Group::Working,
            ),
            (
                "idle with a recent self-report under way",
                api_worker(Open, "", false),
                Live {
                    self_report: Some(progress::Record {
                        activity: "Writing tests".into(),
                        percent: Some(40),
                        ..progress::Record::default()
                    }),
                    ..in_pane(Some(Idle), 0)
                },
                Group::Working,
            ),
            (
                "idle with a self-report at 100 percent",
                api_worker(Open, "", false),
                Live {
                    self_report: Some(progress::Record {
                        activity: "Done".into(),
                        percent: Some(100),
                        ..progress::Record::default()
                    }),
                    ..in_pane(Some(Idle), 0)
                },
                Group::Idle,
            ),
            (
                "idle without a report",
                api_worker(Open, "", false),
                in_pane(Some(Idle), 0),
                Group::Idle,
            ),
            (
                "done without a report",
                api_worker(Open, "", false),
                in_pane(Some(Done), 0),
                Group::Idle,
            ),
        ];
        for (case, worker, seen, expected) in table {
            assert_eq!(group(&worker, &seen), expected, "{case}");
        }
    }

    #[test]
    fn group_labels_and_tokens_round_trip() {
        let table = [
            (Group::WaitingOnYou, "Waiting on you", "waiting_on_you"),
            (Group::Working, "Working", "working"),
            (Group::Idle, "Idle", "idle"),
            (Group::Reported, "Reported", "reported"),
        ];
        for (group, label, token) in table {
            assert_eq!(group.label(), label);
            assert_eq!(group.token(), token);
            assert_eq!(Group::from_token(token), Some(group));
        }
        for unknown in ["", "waiting", "Working", "blocked"] {
            assert_eq!(Group::from_token(unknown), None, "{unknown:?}");
        }
    }
}
