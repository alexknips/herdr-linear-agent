//! The ticker's steps, in the order one tick runs them:
//!
//! 1. read Herdr's agent and pane lists once
//! 2. every 30 seconds, pick up delegated issues (`intake`)
//! 3. read every active run's issue state and new prompts (`read_runs`, `relay`)
//! 4. watch coordinators and workers; write inbox items and Linear requests (`watch`)
//! 5. collect finished routing agents (`routing_results`)
//! 6. place and launch agents, deliver launch prompts, nudge (`launch`)
//! 7. keep sessions from going stale; ask about run timeouts (`heartbeat`)
//! 8. send the outboxes (`flush`)
//!
//! Judgment belongs to the agents; claiming, limits and state transitions are
//! done here, deterministically.

use std::collections::HashMap;
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use crate::config::{Config, Size};
use crate::herdr_cli::{Agent, Herdr, Pane, ready_state};
use crate::linear::api::{
    Activity, Content, ExternalUrl, IssueDetail, IssueRef, Linear, Prompt, RunQuery,
};
use crate::linear::transport::Transport;
use crate::outbox::{self, Op, StateTarget};
use crate::paths::Ctx;
use crate::run::{AgentRecord, AgentStatus, Run, RunRecord, Status};
use crate::ticker::Log;
use crate::worker::{self, Group, Live, Worker};
use crate::{agents, coordinator, files, inbox, routing};

pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_SECS: i64 = 20 * 60;
const COORDINATOR_IDLE_SECS: i64 = 60;
const WRITE_FAILURE_NOTICE: Duration = Duration::from_secs(10 * 60);
const TOKEN_TTL_MS: &str = "300000";

pub type LinearClient = Linear<Box<dyn Transport>>;

/// What the ticker keeps between ticks. Everything else is on disk.
#[derive(Default)]
pub struct Memory {
    pub linear: Option<LinearClient>,
    pub app_user: Option<String>,
    pub last_poll: Option<Instant>,
    /// Routing agents this ticker started, by issue key.
    pub routing: HashMap<String, Child>,
    /// The hash of the unseen inbox ids a coordinator was last nudged about.
    pub nudged: HashMap<String, String>,
    pub write_failing_since: Option<Instant>,
    pub write_failure_notified: bool,
    pub linear_error_logged: bool,
}

impl Memory {
    #[cfg(test)]
    pub fn with_linear(transport: Box<dyn Transport>) -> Memory {
        Memory {
            linear: Some(Linear::new(transport)),
            ..Memory::default()
        }
    }

    /// Builds the Linear client from the Keychain credential on first use.
    fn ensure_linear(&mut self, ctx: &Ctx, config: &Config, log: &Log) {
        if self.linear.is_some() {
            return;
        }
        let lock = ctx.state_dir().join("credentials.lock");
        let built = crate::linear::credentials::CredentialManager::production(
            config.linear.client_id.clone(),
            config.linear.callback_port,
            lock,
        )
        .map_err(crate::linear::ApiError::Credential)
        .and_then(crate::linear::transport::HttpsTransport::new);
        match built {
            Ok(transport) => self.linear = Some(Linear::new(Box::new(transport))),
            Err(error) if !self.linear_error_logged => {
                log.line(&format!(
                    "Linear is not available: {error}; run the login action"
                ));
                self.linear_error_logged = true;
            }
            Err(_) => {}
        }
    }
}

/// Everything one tick reads once and shares between steps.
pub struct Tick<'a> {
    pub ctx: &'a Ctx<'a>,
    pub config: &'a Config,
    pub herdr: Herdr<'a>,
    pub socket: String,
    pub agents: Vec<Agent>,
    pub panes: Vec<Pane>,
    pub now: jiff::Timestamp,
    pub log: &'a Log,
    pub bin: String,
}

impl Tick<'_> {
    fn live(&self, record: &AgentRecord) -> Live {
        worker::live_state(
            record,
            &self.agents,
            &self.panes,
            self.now,
            &self.ctx.state_dir(),
            &self.socket,
        )
    }

    fn fail(&self, run: &Run, error: anyhow::Error) {
        self.log.line(&format!("{}: {error:#}", run.key));
    }

    fn notify(&self, title: &str, body: &str) {
        if self.config.notifications.herdr {
            let _ = self.herdr.notification_show(title, body);
        }
    }
}

fn thought(body: impl Into<String>) -> Op {
    Op::Activity {
        activity: Activity::new(Content::Thought { body: body.into() }),
    }
}

fn elicitation(body: impl Into<String>, options: &[(&str, &str)]) -> Op {
    let mut activity = Activity::new(Content::Elicitation { body: body.into() });
    if !options.is_empty() {
        activity.signal = Some("select".into());
        let options: Vec<_> = options
            .iter()
            .map(|(label, value)| json!({ "label": label, "value": value }))
            .collect();
        activity.signal_metadata = Some(json!({ "options": options }));
    }
    Op::Activity { activity }
}

fn error_activity(body: impl Into<String>) -> Op {
    Op::Activity {
        activity: Activity::new(Content::Error { body: body.into() }),
    }
}

/// Coordinators and workers that count against `max_agents`.
pub fn agent_count(runs: &[Run]) -> usize {
    runs.iter()
        .filter_map(|run| run.record().ok().map(|r| (run, r)))
        .filter(|(_, r)| r.status == Status::Active)
        .map(|(run, r)| {
            usize::from(matches!(
                r.coordinator.status,
                AgentStatus::Pending | AgentStatus::Open
            )) + worker::list(run).iter().filter(|w| w.counts()).count()
        })
        .sum()
}

/// One pass. `Err` means Herdr could not be read; everything after that
/// point is per run, and one run's failure never stops the others.
pub fn tick(ctx: &Ctx, memory: &mut Memory, log: &Log) -> Result<()> {
    let config = Config::load(&ctx.config_dir())?;
    let socket = crate::herdr_cli::session_socket(
        &ctx.env.herdr_bin(),
        ctx.runner,
        config.herdr.session.as_deref(),
    )?;
    let herdr = Herdr::new(ctx.env.herdr_bin(), &socket, ctx.runner);
    let agents = herdr.agent_list()?;
    let panes = herdr.pane_list()?;
    let t = Tick {
        ctx,
        config: &config,
        herdr,
        socket: socket.to_string_lossy().into_owned(),
        agents,
        panes,
        now: jiff::Timestamp::now(),
        log,
        bin: coordinator::binary_command()?,
    };

    memory.ensure_linear(ctx, &config, log);
    let poll_due = memory
        .last_poll
        .is_none_or(|at| at.elapsed() >= POLL_INTERVAL);
    if let Some(linear) = memory.linear.as_mut() {
        if poll_due {
            memory.last_poll = Some(Instant::now());
            if let Err(error) = intake(&t, linear, &mut memory.routing) {
                log.line(&format!("intake: {error:#}"));
            }
        }
        if memory.app_user.is_none() {
            memory.app_user = linear.viewer().map(|v| v.id).ok();
        }
        if let Some(app_user) = memory.app_user.clone() {
            read_runs(&t, linear, &app_user);
        }
    }
    let runs = Run::list(&ctx.runs_dir());
    for run in runs
        .iter()
        .filter(|run| run.record().is_ok_and(|r| r.status == Status::Active))
    {
        if let Err(error) = watch(&t, run) {
            t.fail(run, error);
        }
    }
    routing_results(&t, &mut memory.routing);
    for run in runs
        .iter()
        .filter(|run| run.record().is_ok_and(|r| r.status == Status::Active))
    {
        if let Err(error) = launch(&t, run, &mut memory.nudged) {
            t.fail(run, error);
        }
        if let Err(error) = heartbeat(&t, run) {
            t.fail(run, error);
        }
        inbox::prune_done(run);
    }
    if let Some(linear) = memory.linear.as_mut() {
        flush(
            &t,
            linear,
            &runs,
            &mut memory.write_failing_since,
            &mut memory.write_failure_notified,
        );
    }
    crate::progress::prune(
        &ctx.state_dir(),
        &t.socket,
        &t.panes
            .iter()
            .map(|p| p.pane_id.clone())
            .collect::<Vec<_>>(),
    );
    Ok(())
}

// ---------------------------------------------------------------- intake

/// Picks up delegated issues that have no run yet, while `max_runs` and
/// `max_agents` leave room. A run that was detached or closed and is
/// delegated again becomes active again.
pub fn intake(
    t: &Tick,
    linear: &mut LinearClient,
    routing_children: &mut HashMap<String, Child>,
) -> Result<()> {
    let issues = linear.delegated_issues(&t.config.linear.teams)?;
    let runs_dir = t.ctx.runs_dir();
    let paused = t.ctx.state_dir().join("paused").exists();
    for issue in issues {
        if let Ok(run) = Run::load(&runs_dir, &issue.identifier) {
            let record = run.record()?;
            // A claim cut short (no coordinator decided yet) is finished now.
            if record.status == Status::Active
                && record.coordinator.profile.is_empty()
                && record.routing.is_none()
            {
                let detail = linear.issue(&issue.id)?;
                if let Err(error) = finish_claim(t, linear, &run, &detail, routing_children) {
                    t.fail(&run, error);
                }
                continue;
            }
            if record.status != Status::Active {
                run.update(|r| {
                    r.status = Status::Active;
                    // A closed run's coordinator was stopped: bring it back.
                    if r.coordinator.status == AgentStatus::Stopped {
                        r.coordinator.status = AgentStatus::Pending;
                        r.coordinator.resume = !r.coordinator.agent_session.is_empty();
                        r.coordinator.launch_attempts = 0;
                    }
                })?;
                outbox::push(
                    &run,
                    thought("The issue was delegated again; the run continues."),
                )?;
                inbox::write(
                    &run,
                    "issue",
                    "issue",
                    "The issue was delegated to this agent again; the run is active again.",
                )?;
            }
            continue;
        }
        if paused {
            continue;
        }
        let runs = Run::list(&runs_dir);
        let active = runs
            .iter()
            .filter(|r| r.record().is_ok_and(|r| r.status == Status::Active))
            .count();
        if active >= t.config.limits.max_runs as usize
            || agent_count(&runs) + 1 > t.config.limits.max_agents as usize
        {
            break;
        }
        if let Err(error) = claim(t, linear, &issue, routing_children) {
            t.log.line(&format!(
                "{}: could not pick up: {error:#}",
                issue.identifier
            ));
        }
    }
    Ok(())
}

/// Creates the run folder and the session, sends the first thought in this
/// same tick, moves the issue to a started state and decides the coordinator.
fn claim(
    t: &Tick,
    linear: &mut LinearClient,
    issue: &IssueRef,
    routing_children: &mut HashMap<String, Child>,
) -> Result<()> {
    crate::run::validate_key(&issue.identifier)?;
    let detail = linear.issue(&issue.id)?;
    t.ctx.ensure_state_dir()?;
    std::fs::create_dir_all(t.ctx.runs_dir())?;
    let now = files::now();
    let record = RunRecord {
        issue_id: detail.id.clone(),
        identifier: detail.identifier.clone(),
        title: detail.title.clone(),
        url: detail.url.clone(),
        team_key: detail.team.key.clone(),
        labels: detail.labels.iter().map(|l| l.name.clone()).collect(),
        created: now.clone(),
        issue_updated_at: detail.updated_at.clone(),
        issue_hash: crate::run::issue_hash(&detail),
        prompt_cursor: now.clone(),
        last_activity: now.clone(),
        timeout_since: now,
        ..RunRecord::default()
    };
    let run = Run::create(&t.ctx.runs_dir(), record)?;
    files::write_atomic(
        &run.issue_md(),
        crate::run::issue_markdown(&detail).as_bytes(),
    )?;
    t.log.line(&format!("{}: picked up", run.key));
    outbox::push(&run, thought(format!("Picked up {}.", run.key)))?;
    finish_claim(t, linear, &run, &detail, routing_children)
}

/// Attaches the session (the one Linear created on delegation, or a new
/// one), moves the issue to a started state and decides the coordinator. A
/// session that cannot be opened yet is retried when the outbox is sent; it
/// never holds up the rest of the claim.
fn finish_claim(
    t: &Tick,
    linear: &mut LinearClient,
    run: &Run,
    detail: &IssueDetail,
    routing_children: &mut HashMap<String, Child>,
) -> Result<()> {
    if run.record()?.session_id.is_empty() {
        match linear.open_session(&detail.id) {
            Ok(session) => {
                run.update(|r| r.session_id = session)?;
            }
            Err(error) => t.log.line(&format!(
                "{}: could not open the session yet: {error}",
                run.key
            )),
        }
    }
    outbox::push(
        run,
        Op::IssueState {
            target: StateTarget::Started,
        },
    )?;
    route(t, run, detail, routing_children)
}

/// Decides the size from the estimate or a size label, or starts the routing
/// agent and leaves the decision to a later tick.
fn route(
    t: &Tick,
    run: &Run,
    detail: &IssueDetail,
    routing_children: &mut HashMap<String, Child>,
) -> Result<()> {
    if let Some((size, source)) = routing::known_size(t.config, detail) {
        return decide(t, run, size, source);
    }
    let Some(agent) = &t.config.routing.agent else {
        return decide(t, run, Size::Unknown, "default");
    };
    let profile = t.config.profile(&agent.profile)?;
    match routing::spawn(profile, &run.state_dir(), detail, t.ctx.env.var("PATH")) {
        Ok((job, child)) => {
            routing_children.insert(run.key.clone(), child);
            run.update(|r| r.routing = Some(job))?;
            Ok(())
        }
        Err(error) => {
            t.fail(run, error);
            decide(t, run, Size::Unknown, "agent")
        }
    }
}

/// Picks the coordinator profile and reserves the coordinator's launch.
fn decide(t: &Tick, run: &Run, size: Size, source: &str) -> Result<()> {
    let record = run.record()?;
    let labels: Vec<crate::linear::api::Label> = record
        .labels
        .iter()
        .map(|name| crate::linear::api::Label {
            name: name.clone(),
            group: None,
        })
        .collect();
    let name = routing::coordinator_profile(t.config, size, &record.team_key, &labels).to_string();
    let profile = t.config.profile(&name)?;
    let pending = coordinator::pending_record(&record, &name, &profile.kind);
    run.update(|r| {
        r.size = size;
        r.size_source = source.to_string();
        r.routing = None;
        r.coordinator = pending;
    })?;
    let why = match (size, source) {
        (Size::Unknown, "default") => "size unknown".to_string(),
        (Size::Unknown, source) => format!("size unknown after the routing {source}"),
        (size, "agent") => format!("size {size} from the routing agent"),
        (size, source) => format!("size {size} from the {source}"),
    };
    outbox::push(
        run,
        thought(format!(
            "The coordinator uses the `{name}` profile ({why})."
        )),
    )?;
    Ok(())
}

/// Collects routing agents that finished or ran out of time.
pub fn routing_results(t: &Tick, children: &mut HashMap<String, Child>) {
    let timeout = t
        .config
        .routing
        .agent
        .as_ref()
        .map_or(120, |a| a.timeout_seconds) as i64;
    for run in Run::list(&t.ctx.runs_dir()) {
        let Ok(record) = run.record() else { continue };
        let Some(job) = record.routing.clone() else {
            continue;
        };
        let finished = match children.get_mut(&run.key) {
            Some(child) => !matches!(child.try_wait(), Ok(None)),
            // Started by an earlier ticker: it is not our child any more.
            None => !routing::process_alive(job.pid),
        };
        let outcome = if finished {
            children.remove(&run.key);
            let text = std::fs::read_to_string(&job.output).unwrap_or_default();
            Some((routing::parse_output(&text), "agent"))
        } else if files::seconds_since(&job.started, t.now) >= timeout {
            match children.remove(&run.key) {
                Some(mut child) => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                None => routing::kill(job.pid),
            }
            Some((Size::Unknown, "agent (timed out)"))
        } else {
            None
        };
        if let Some((size, source)) = outcome
            && let Err(error) = decide(t, &run, size, source)
        {
            t.fail(&run, error);
        }
    }
}

// ---------------------------------------------------------------- reading runs

/// Reads every active run's issue and new prompts in one request, and falls
/// back to one request per run when the batch fails.
pub fn read_runs(t: &Tick, linear: &mut LinearClient, app_user: &str) {
    let runs: Vec<(Run, RunRecord)> = Run::list(&t.ctx.runs_dir())
        .into_iter()
        .filter_map(|run| run.record().ok().map(|r| (run, r)))
        .filter(|(_, r)| r.status == Status::Active && !r.session_id.is_empty())
        .collect();
    let query = |r: &RunRecord| RunQuery {
        issue_id: r.issue_id.clone(),
        session_id: r.session_id.clone(),
        cursor: r.prompt_cursor.clone(),
    };
    let queries: Vec<RunQuery> = runs.iter().map(|(_, r)| query(r)).collect();
    let updates = match linear.run_updates(&queries) {
        Ok(updates) => updates.into_iter().map(Ok).collect(),
        Err(_) => queries
            .iter()
            .map(|q| {
                linear
                    .run_updates(std::slice::from_ref(q))
                    .map(|mut u| u.remove(0))
            })
            .collect::<Vec<_>>(),
    };
    for ((run, record), update) in runs.iter().zip(updates) {
        let result = update.map_err(anyhow::Error::from).and_then(|update| {
            let issue = &update.issue;
            if matches!(issue.state_type.as_str(), "completed" | "canceled") {
                return close_run(t, run, &issue.state_name);
            }
            if issue.delegate_id.as_deref() != Some(app_user) {
                return detach_run(t, run);
            }
            if issue.updated_at != record.issue_updated_at {
                refresh_issue(run, linear)?;
            }
            relay(t, run, &update.prompts)
        });
        if let Err(error) = result {
            t.fail(run, error);
        }
    }
}

/// Rewrites `issue.md` and tells the coordinator when a person edited the issue.
fn refresh_issue(run: &Run, linear: &mut LinearClient) -> Result<()> {
    let detail = linear.issue(&run.record()?.issue_id)?;
    files::write_atomic(
        &run.issue_md(),
        crate::run::issue_markdown(&detail).as_bytes(),
    )?;
    let hash = crate::run::issue_hash(&detail);
    let before = run.record()?.issue_hash;
    run.update(|r| {
        r.issue_updated_at = detail.updated_at.clone();
        r.issue_hash = hash.clone();
        r.title = detail.title.clone();
        r.labels = detail.labels.iter().map(|l| l.name.clone()).collect();
    })?;
    if hash != before {
        inbox::write(
            run,
            "issue",
            "issue",
            "The issue was edited in Linear; issue.md is updated.",
        )?;
    }
    Ok(())
}

/// A person's messages: replies from allowed users reach the coordinator
/// through `conversation.md` and the inbox; a stop signal interrupts the run's
/// agents; anyone else's message is only recorded.
pub fn relay(t: &Tick, run: &Run, prompts: &[Prompt]) -> Result<()> {
    let Some(last) = prompts.last() else {
        return Ok(());
    };
    for prompt in prompts {
        if !t.config.linear.allowed_user_ids.contains(&prompt.user_id) {
            run.record_ignored_prompt(&prompt.created_at, &prompt.user_id, &prompt.body)?;
            continue;
        }
        if prompt.signal.as_deref() == Some("stop") {
            let stopped = interrupt_agents(t, run);
            run.update(|r| r.stopped = true)?;
            outbox::push(
                run,
                Op::Activity {
                    activity: Activity::new(Content::Response {
                        body: format!(
                            "Stopped {stopped} agent(s) as asked. Their worktrees are kept; reply here to continue."
                        ),
                    }),
                },
            )?;
            continue;
        }
        run.append_conversation(&prompt.created_at, &prompt.user_id, &prompt.body)?;
        inbox::write(
            run,
            "reply",
            "reply",
            &format!(
                "A new reply from user {} is in conversation.md.",
                prompt.user_id
            ),
        )?;
        let resume = prompt.body.trim().eq_ignore_ascii_case("resume");
        run.update(|r| {
            r.stopped = false;
            if r.timeout_asked {
                r.timeout_asked = false;
                r.timeout_since = files::now();
            }
            if r.coordinator_lost && resume {
                r.coordinator_lost = false;
                r.coordinator.status = AgentStatus::Pending;
                r.coordinator.resume = !r.coordinator.agent_session.is_empty();
                r.coordinator.launch_attempts = 0;
            }
        })?;
    }
    let cursor = last.created_at.clone();
    run.update(|r| r.prompt_cursor = cursor)?;
    Ok(())
}

/// Escape in the coordinator's and every open worker's pane. Returns how many
/// agents were interrupted.
fn interrupt_agents(t: &Tick, run: &Run) -> usize {
    let mut records: Vec<AgentRecord> = worker::list(run)
        .into_iter()
        .filter(|w| w.agent.status == AgentStatus::Open)
        .map(|w| w.agent)
        .collect();
    if let Ok(record) = run.record() {
        records.insert(0, record.coordinator);
    }
    records
        .iter()
        .filter_map(|record| worker::find_agent(record, &t.agents))
        .filter(|agent| t.herdr.agent_interrupt(&agent.pane_id).is_ok())
        .count()
}

/// The issue was completed or canceled: copy the reports home, stop the agents
/// and close their workspaces. Checkouts and branches stay.
fn close_run(t: &Tick, run: &Run, state: &str) -> Result<()> {
    let record = run.record()?;
    interrupt_agents(t, run);
    let mut workspaces = Vec::new();
    for w in worker::list(run) {
        let _ = worker::copy_report_home(run, &w);
        if w.agent.status == AgentStatus::Open && t.live(&w.agent).pane_exists {
            workspaces.push(
                worker::find_agent(&w.agent, &t.agents)
                    .map_or(w.agent.workspace_id.clone(), |a| a.workspace_id.clone()),
            );
        }
        worker::update(run, &w.id, |w| w.agent.status = AgentStatus::Stopped)?;
    }
    if record.coordinator.status == AgentStatus::Open && t.live(&record.coordinator).pane_exists {
        workspaces.push(
            worker::find_agent(&record.coordinator, &t.agents)
                .map_or(record.coordinator.workspace_id.clone(), |a| {
                    a.workspace_id.clone()
                }),
        );
    }
    for workspace in workspaces {
        if let Err(error) = t.herdr.workspace_close(&workspace) {
            t.log.line(&format!(
                "{}: could not close workspace {workspace}: {error}",
                run.key
            ));
        }
    }
    run.update(|r| {
        r.status = Status::Closed;
        r.coordinator.status = AgentStatus::Stopped;
    })?;
    t.log
        .line(&format!("{}: closed (the issue is {state})", run.key));
    Ok(())
}

/// The delegation was removed: stop the agents, keep the workspaces.
fn detach_run(t: &Tick, run: &Run) -> Result<()> {
    interrupt_agents(t, run);
    run.update(|r| r.status = Status::Detached)?;
    t.log
        .line(&format!("{}: detached (no longer delegated)", run.key));
    Ok(())
}

// ---------------------------------------------------------------- watching

/// Keeps an agent record in step with Herdr: a renumbered pane, a lost name,
/// the state and how long it has lasted, the native session id.
fn track(t: &Tick, record: &AgentRecord, live: &Live) -> AgentRecord {
    let mut next = record.clone();
    if let Some((workspace, tab, pane)) = &live.moved_to {
        next.workspace_id = workspace.clone();
        next.tab_id = tab.clone();
        next.pane_id = pane.clone();
    }
    if let Some(agent) = worker::find_agent(record, &t.agents) {
        if worker::needs_rename(record, agent) {
            let _ = t.herdr.agent_rename(&agent.pane_id, &record.agent_name);
        }
        if !agent.session_id().is_empty() {
            next.agent_session = agent.session_id().to_string();
        }
    }
    let state = live.agent_state.clone().unwrap_or_default();
    if state != record.last_state {
        next.last_state = state;
        next.last_state_change = files::now();
    }
    if !live.needs_person(record) {
        next.blocked_reported = false;
    }
    next
}

fn report_pane(t: &Tick, pane: &str, display: &str, state: &str) {
    let token = format!("hla_state={state}");
    let _ = t.herdr.call(
        &[
            "pane",
            "report-metadata",
            pane,
            "--source",
            crate::progress::SOURCE,
            "--display-agent",
            display,
            "--token",
            &token,
            "--ttl-ms",
            TOKEN_TTL_MS,
        ],
        crate::herdr_cli::CALL_TIMEOUT,
    );
}

/// Asks a person, once per episode, to answer a dialog in a Herdr pane.
fn ask_for_person(t: &Tick, run: &Run, who: &str, record: &AgentRecord) -> Result<()> {
    let label = coordinator::workspace_label(&run.record()?);
    let body = format!(
        "{who} needs someone in Herdr: it is waiting on a dialog in pane `{}` (session `{}`, run {label}). Answer it there.",
        record.pane_id,
        t.config.herdr.session.as_deref().unwrap_or("default")
    );
    outbox::push(run, elicitation(body.clone(), &[]))?;
    t.notify(&format!("{} needs you", run.key), &body);
    Ok(())
}

pub fn watch(t: &Tick, run: &Run) -> Result<()> {
    let record = run.record()?;
    let c = &record.coordinator;
    if c.status == AgentStatus::Open {
        let live = t.live(c);
        let mut next = track(t, c, &live);
        if live.needs_person(c) && !c.blocked_reported {
            ask_for_person(t, run, "The coordinator", &next)?;
            next.blocked_reported = true;
        }
        let lost = !live.pane_exists && !record.coordinator_lost;
        if lost {
            let resumable = !next.agent_session.is_empty()
                && agents::resume_args(&next.kind, &next.agent_session).is_some();
            let how = if resumable {
                "Reply `resume` to start it again with its previous session."
            } else {
                "Reply `resume` to start a new coordinator."
            };
            outbox::push(
                run,
                elicitation(
                    format!("The coordinator's pane for {} is gone. {how}", run.key),
                    &[("Resume", "resume")],
                ),
            )?;
            t.notify(&format!("{} coordinator is gone", run.key), how);
        } else if live.pane_exists {
            let state = if live.needs_person(c) {
                "needs you"
            } else {
                live.agent_state.as_deref().unwrap_or("starting")
            };
            report_pane(
                t,
                &next.pane_id,
                &format!("{} · coordinator", run.key),
                state,
            );
        }
        if next != *c || lost {
            run.update(|r| {
                r.coordinator = next;
                r.coordinator_lost |= lost;
            })?;
        }
    }
    for w in worker::list(run)
        .into_iter()
        .filter(|w| w.agent.status == AgentStatus::Open || w.agent.status == AgentStatus::Failed)
    {
        watch_worker(t, run, &w)?;
    }
    Ok(())
}

fn watch_worker(t: &Tick, run: &Run, w: &Worker) -> Result<()> {
    let live = t.live(&w.agent);
    let mut next = w.clone();
    next.agent = track(t, &w.agent, &live);

    // A report written this tick counts for the group at once.
    if let Some(hash) = worker::report_hash(w).filter(|h| *h != w.report_hash) {
        worker::copy_report_home(run, w)?;
        next.report_hash = hash;
        let report =
            std::fs::read_to_string(worker::home_report_path(run, &w.id)).unwrap_or_default();
        if let Some(url) = worker::pr_line(&report).filter(|url| *url != w.pr_url) {
            outbox::push(
                run,
                Op::Activity {
                    activity: Activity::new(Content::Action {
                        action: "Pull request".into(),
                        parameter: format!("{} (worker {}, repo {})", url, w.id, w.repo),
                        result: None,
                    }),
                },
            )?;
            let urls = run.update(|r| {
                if !r.external_urls.iter().any(|u| u.url == url) {
                    r.external_urls.push(ExternalUrl {
                        label: format!("{} {} PR", w.id, w.repo),
                        url: url.clone(),
                    });
                }
            })?;
            outbox::push(
                run,
                Op::ExternalUrls {
                    urls: urls.external_urls,
                },
            )?;
            next.pr_url = url;
        }
    }

    let group = worker::group(&next, &live);
    if group.token() != w.agent.last_group {
        let reason = if w.agent.status == AgentStatus::Failed {
            format!("failed: {}", w.agent.error)
        } else if !live.pane_exists {
            "its pane closed before it wrote a report".to_string()
        } else if live.self_waiting() {
            "it asked a question in its report".to_string()
        } else if live.needs_person(&w.agent) {
            format!("it waits on a dialog in pane {}", w.agent.pane_id)
        } else {
            live.agent_state
                .clone()
                .unwrap_or_else(|| "no agent".into())
        };
        match group {
            Group::WaitingOnYou => {
                inbox::write(
                    run,
                    "worker",
                    &w.id,
                    &format!("{} ({}) is Waiting on you: {reason}.", w.id, w.repo),
                )?;
            }
            Group::Idle => {
                inbox::write(
                    run,
                    "worker",
                    &w.id,
                    &format!(
                        "{} ({}) is idle without a report; check its pane {}.",
                        w.id, w.repo, w.agent.pane_id
                    ),
                )?;
            }
            Group::Reported | Group::Working => {}
        }
        next.agent.last_group = group.token().to_string();
    }
    if group == Group::Reported && next.report_hash != w.announced_report_hash {
        inbox::write(
            run,
            "worker",
            &w.id,
            &format!(
                "{} ({}) has a new report: workers/{}.md",
                w.id, w.repo, w.id
            ),
        )?;
        next.announced_report_hash = next.report_hash.clone();
    }
    if live.needs_person(&w.agent) && !w.agent.blocked_reported {
        ask_for_person(
            t,
            run,
            &format!("Worker {} ({})", w.id, w.repo),
            &next.agent,
        )?;
        next.agent.blocked_reported = true;
    }
    if w.agent.status == AgentStatus::Open
        && !live.pane_exists
        && next.report_hash.is_empty()
        && !w.gone_reported
    {
        outbox::push(
            run,
            error_activity(format!(
                "Worker {} ({}) lost its pane before it wrote a report.",
                w.id, w.repo
            )),
        )?;
        next.gone_reported = true;
    }
    if live.pane_exists {
        report_pane(
            t,
            &next.agent.pane_id,
            &format!("{} · {} {}", run.key, w.id, w.title),
            group.label(),
        );
    }
    if next != *w {
        let changed = next;
        worker::update(run, &w.id, |r| {
            let (created, updated) = (r.created.clone(), r.updated.clone());
            *r = changed;
            r.created = created;
            r.updated = updated;
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------- launching

/// Places a pending coordinator, launches agents at a shell prompt, delivers
/// launch prompts to agents that are ready, and nudges an idle coordinator.
/// At most one `agent start` per run per tick, and never a start and a prompt
/// for the same pane in one tick.
pub fn launch(t: &Tick, run: &Run, nudged: &mut HashMap<String, String>) -> Result<()> {
    let mut may_start = true;
    let record = run.record()?;
    let c = record.coordinator.clone();
    if c.status == AgentStatus::Pending && !c.profile.is_empty() && record.routing.is_none() {
        place_coordinator(t, run, &record)?;
    } else if c.status == AgentStatus::Open
        && let Some(next) = launch_agent(
            t,
            run,
            &c,
            &coordinator::launch_prompt(&run.key, c.resume),
            "the coordinator",
            &[&c.cwd],
            &mut may_start,
        )?
    {
        run.update(|r| r.coordinator = next)?;
    }
    for w in worker::list(run)
        .into_iter()
        .filter(|w| w.agent.status == AgentStatus::Open)
    {
        let before = w.agent.clone();
        if let Some(next) = launch_agent(
            t,
            run,
            &w.agent,
            &worker::launch_prompt(&run.key, &w.id),
            &format!("worker {}", w.id),
            // Claude Code checks a worktree's trust at its main checkout.
            &[&w.agent.cwd, &w.repo_path],
            &mut may_start,
        )? {
            let started =
                next.launch_attempts > before.launch_attempts && next.status == AgentStatus::Open;
            let announce = started && !w.start_announced;
            if announce {
                outbox::push(
                    run,
                    Op::Activity {
                        activity: Activity::new(Content::Action {
                            action: "Start worker".into(),
                            parameter: format!(
                                "{}: repo `{}`, profile `{}`",
                                w.id, w.repo, w.agent.profile
                            ),
                            result: None,
                        }),
                    },
                )?;
            }
            worker::update(run, &w.id, |r| {
                r.agent = next;
                r.start_announced |= announce;
            })?;
        }
    }
    nudge(t, run, nudged)
}

fn place_coordinator(t: &Tick, run: &Run, record: &RunRecord) -> Result<()> {
    coordinator::write_priming(run, record, &t.bin)?;
    let cwd = run.canonical_dir();
    match t
        .herdr
        .workspace_create(&cwd, &coordinator::workspace_label(record))
    {
        Ok(created) => {
            run.update(|r| {
                let c = &mut r.coordinator;
                c.status = AgentStatus::Open;
                c.workspace_id = created.workspace_id.clone();
                c.tab_id = created.tab_id.clone();
                c.pane_id = created.pane_id.clone();
                c.cwd = cwd.to_string_lossy().into_owned();
                c.prompt_pending = true;
                c.launch_attempts = 0;
                c.last_state.clear();
                c.last_state_change = files::now();
                c.blocked_reported = false;
            })?;
            Ok(())
        }
        Err(error) => {
            let attempts = run
                .update(|r| r.coordinator.launch_attempts += 1)?
                .coordinator
                .launch_attempts;
            if attempts >= worker::MAX_LAUNCH_ATTEMPTS {
                run.update(|r| {
                    r.coordinator.status = AgentStatus::Failed;
                    r.coordinator.error = error.to_string();
                })?;
                outbox::push(
                    run,
                    error_activity(format!(
                        "Could not open a Herdr workspace for the coordinator: {error}"
                    )),
                )?;
            }
            Err(error.into())
        }
    }
}

/// One agent's launch step. Returns the changed record, if anything changed.
fn launch_agent(
    t: &Tick,
    run: &Run,
    record: &AgentRecord,
    prompt: &str,
    who: &str,
    trust_dirs: &[&str],
    may_start: &mut bool,
) -> Result<Option<AgentRecord>> {
    let live = t.live(record);
    let mut next = record.clone();
    match live.agent_state.as_deref() {
        // Ready: deliver the launch prompt once.
        Some(state) if record.prompt_pending && ready_state(state) => {
            let pane = worker::find_agent(record, &t.agents)
                .map_or(record.pane_id.clone(), |a| a.pane_id.clone());
            t.herdr
                .agent_prompt(&pane, prompt)
                .with_context(|| format!("launch prompt to {who}"))?;
            next.prompt_pending = false;
            next.resume = false;
        }
        Some(_) => return Ok(None),
        // A shell prompt in our pane: start the agent.
        None if live.pane_exists && (record.prompt_pending || record.resume) => {
            if record.launch_attempts >= worker::MAX_LAUNCH_ATTEMPTS {
                next.status = AgentStatus::Failed;
                next.error = format!(
                    "no `{}` agent appeared after {} launch attempts",
                    record.kind,
                    worker::MAX_LAUNCH_ATTEMPTS
                );
                outbox::push(
                    run,
                    error_activity(format!("Could not start {who}: {}.", next.error)),
                )?;
                return Ok(Some(next));
            }
            if !std::mem::take(may_start) {
                return Ok(None);
            }
            let mut args = t
                .config
                .profile(&record.profile)
                .map(agents::profile_args)
                .unwrap_or_default();
            if record.resume
                && let Some(resume) = agents::resume_args(&record.kind, &record.agent_session)
            {
                args.extend(resume);
            }
            next.launch_attempts += 1;
            next.last_state_change = files::now();
            if record.kind == "claude"
                && t.config.claude.auto_accept_trust_dialog
                && let Err(error) = crate::claude_trust::trust(t.ctx.env, trust_dirs)
            {
                t.log.line(&format!(
                    "{}: could not accept Claude Code's trust dialog for {who}: {error:#}",
                    run.key
                ));
            }
            match t
                .herdr
                .agent_start(&record.agent_name, &record.kind, &record.pane_id, &args)
            {
                // A dialog (trust, login) blocks startup; `watch` asks a person.
                Err(error) if error.code == "agent_not_ready" => {}
                Err(error) => {
                    next.error = error.to_string();
                    t.log.line(&format!("{}: starting {who}: {error}", run.key));
                }
                Ok(agent) => {
                    if !agent.session_id().is_empty() {
                        next.agent_session = agent.session_id().to_string();
                    }
                }
            }
        }
        None => return Ok(None),
    }
    Ok(Some(next))
}

/// Prompts an idle coordinator once per set of unseen inbox items. The prompt
/// is a fixed line; what happened is in the inbox. No prompt goes out while
/// the run timeout question is open.
fn nudge(t: &Tick, run: &Run, nudged: &mut HashMap<String, String>) -> Result<()> {
    let record = run.record()?;
    let c = &record.coordinator;
    if c.status != AgentStatus::Open || c.prompt_pending || record.timeout_asked || record.stopped {
        return Ok(());
    }
    let seen = inbox::seen(run);
    let unseen: Vec<inbox::Item> = inbox::unhandled(run)
        .into_iter()
        .filter(|i| !seen.contains(&i.id))
        .collect();
    if unseen.is_empty() {
        return Ok(());
    }
    let hash = files::sha256_hex(
        unseen
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    );
    if nudged.get(&run.key) == Some(&hash) {
        return Ok(());
    }
    let Some(agent) = worker::find_agent(c, &t.agents) else {
        return Ok(());
    };
    let idle_long = ready_state(&agent.agent_status)
        && c.last_state == agent.agent_status
        && files::seconds_since(&c.last_state_change, t.now) >= COORDINATOR_IDLE_SECS;
    if !idle_long {
        return Ok(());
    }
    let text = if unseen.iter().any(|i| i.kind == "reply") {
        coordinator::NUDGE_REPLY
    } else {
        coordinator::NUDGE_INBOX
    };
    t.herdr.agent_prompt(&agent.pane_id, text)?;
    nudged.insert(run.key.clone(), hash);
    Ok(())
}

// ---------------------------------------------------------------- heartbeat

/// An ephemeral thought after 20 minutes without an activity keeps the session
/// from going stale; past `run_timeout_hours` a person is asked whether to go on.
pub fn heartbeat(t: &Tick, run: &Run) -> Result<()> {
    let record = run.record()?;
    if record.session_id.is_empty() || record.stopped {
        return Ok(());
    }
    if files::seconds_since(&record.last_activity, t.now) >= HEARTBEAT_SECS
        && outbox::pending(run).is_empty()
    {
        let mut counts: Vec<(Group, usize)> = Vec::new();
        for w in worker::list(run)
            .iter()
            .filter(|w| w.agent.status == AgentStatus::Open)
        {
            let group = worker::group(w, &t.live(&w.agent));
            match counts.iter_mut().find(|(g, _)| *g == group) {
                Some((_, n)) => *n += 1,
                None => counts.push((group, 1)),
            }
        }
        let summary = if counts.is_empty() {
            "no workers".to_string()
        } else {
            counts
                .iter()
                .map(|(g, n)| format!("{n} {}", g.label().to_lowercase()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut activity = Activity::new(Content::Thought {
            body: format!("Still on it: {summary}."),
        });
        activity.ephemeral = true;
        outbox::push(run, Op::Activity { activity })?;
    }
    let limit = t.config.limits.run_timeout_hours as i64 * 3600;
    if !record.timeout_asked && files::seconds_since(&record.timeout_since, t.now) >= limit {
        let hours = t.config.limits.run_timeout_hours;
        outbox::push(
            run,
            elicitation(
                format!(
                    "This run has been going for {hours} hours. Reply to let it continue; until then the coordinator gets no prompts."
                ),
                &[("Continue", "continue")],
            ),
        )?;
        run.update(|r| r.timeout_asked = true)?;
        t.notify(
            &format!("{} ran {hours} hours", run.key),
            "Reply in the Linear session to let it continue.",
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- outbox

/// Sends every run's outbox; creates a missing session first. After ten
/// minutes in which Linear accepted no write, a Herdr notification says so.
pub fn flush(
    t: &Tick,
    linear: &mut LinearClient,
    runs: &[Run],
    failing_since: &mut Option<Instant>,
    notified: &mut bool,
) {
    let mut blocked = false;
    for run in runs {
        let Ok(record) = run.record() else { continue };
        if outbox::pending(run).is_empty() {
            continue;
        }
        let session = if record.session_id.is_empty() {
            match linear.open_session(&record.issue_id) {
                Ok(id) => {
                    let _ = run.update(|r| r.session_id = id.clone());
                    id
                }
                Err(error) => {
                    t.log.line(&format!(
                        "{}: could not create the session: {error}",
                        run.key
                    ));
                    blocked = true;
                    continue;
                }
            }
        } else {
            record.session_id.clone()
        };
        let sent = outbox::send(
            run,
            &session,
            &record.issue_id,
            &t.config.linear.review_state,
            linear,
        );
        if sent.activity_sent {
            let _ = run.update(|r| r.last_activity = files::now());
        }
        for (request, error) in &sent.refused {
            t.log.line(&format!(
                "{}: Linear refused request {}: {error}",
                run.key, request.id
            ));
        }
        if let Some(error) = sent.blocked {
            t.log.line(&format!(
                "{}: Linear write failed, will retry: {error}",
                run.key
            ));
            blocked = true;
        }
    }
    if !blocked {
        *failing_since = None;
        *notified = false;
        return;
    }
    let since = *failing_since.get_or_insert_with(Instant::now);
    if since.elapsed() >= WRITE_FAILURE_NOTICE && !*notified {
        let _ = t.herdr.notification_show("herdr-linear-agent", "Linear has not accepted writes for 10 minutes. They are kept and retried; see the ticker log.");
        *notified = true;
    }
}
