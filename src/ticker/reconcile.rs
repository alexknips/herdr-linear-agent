//! The reconciler: the one task that decides. Herdr events, Linear events,
//! effect results and timers only wake it; each pass reads one
//! `session.snapshot` and the run folders and acts on them.
//!
//! A pass, in order:
//!
//! 1. take one snapshot; the rules that read panes or agents run only when it
//!    succeeded
//! 2. apply the Linear events: sessions, sent activities, write failures, and
//!    run reads (close, detach, issue edits, relay)
//! 3. intake from the latest delegated list
//! 4. apply the results of effect tasks (placements, starts) and routing
//!    agents; they are facts and are applied even without a snapshot
//! 5. per active run: watch the coordinator and the workers, launch, nudge
//!    (with a snapshot), then the heartbeat and inbox pruning
//! 6. the write-failure notice, and progress records of gone panes
//!
//! Record writes are field-level read-modify-writes under the run lock, on a
//! blocking thread, after the Herdr request they follow; a record read
//! before an await is never saved whole.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use jiff::{SignedDuration, Timestamp};
use tokio::sync::{Notify, mpsc, watch};

use super::Log;
use crate::config::Config;
use crate::herdr::{Herdr, HerdrError, Link, PaneId, Placed, Snapshot};
use crate::linear::api::{Activity, Content};
use crate::linear::task::{Decline, Levels, LinearEvent, RunQuery};
use crate::outbox::{self, Op};
use crate::paths::Ctx;
use crate::run::{AgentRecord, AgentStatus, Run, RunLock, RunRecord, Status};
use crate::worker::{self, Live, Worker};
use crate::{coordinator, inbox, progress, transcript};

/// A blocked agent is asked about after this long (`worker::BLOCKED_SECS`).
const BLOCKED: SignedDuration = SignedDuration::from_secs(worker::BLOCKED_SECS);
/// A launch whose status stays unknown this long waits on a dialog.
const LAUNCH_DIALOG: SignedDuration = SignedDuration::from_secs(worker::LAUNCH_DIALOG_SECS);
/// An idle coordinator is nudged about unseen inbox items after this long.
pub(super) const COORDINATOR_IDLE: SignedDuration = SignedDuration::from_secs(60);
pub(super) const HEARTBEAT: SignedDuration = SignedDuration::from_secs(20 * 60);
const WRITE_FAILURE_NOTICE: SignedDuration = SignedDuration::from_secs(10 * 60);
/// Half the pane token TTL: a token is refreshed at least this often.
pub(super) const METADATA_REFRESH: SignedDuration = SignedDuration::from_secs(150);
/// The first wait after an unsuccessful placement or start; it doubles with
/// every counted attempt.
pub(super) const LAUNCH_SPACING: SignedDuration = SignedDuration::from_secs(15);
pub(super) const MAX_LAUNCH_ATTEMPTS: u32 = 3;
/// A started agent Herdr has not detected yet is not started again for this
/// long.
pub(super) const DETECTION_GRACE: SignedDuration = SignedDuration::from_secs(60);
/// A recorded pane this ticker has never seen is judged gone only after it
/// has been missing this long (Herdr may answer a placement before its pane
/// list shows the pane).
pub(super) const PANE_GRACE: SignedDuration = SignedDuration::from_secs(30);
const EFFECT_QUEUE: usize = 64;

/// Everything the loop reads from and writes to.
pub struct Inputs<'a, H> {
    pub ctx: &'a Ctx<'a>,
    pub config: &'a Config,
    pub herdr: H,
    /// The configured session's socket path, which keys progress records;
    /// empty until the session is found.
    pub socket: watch::Receiver<String>,
    pub log: Arc<Log>,
    /// Wakes on every Herdr event.
    pub link: watch::Receiver<Link>,
    pub level: watch::Receiver<Levels>,
    pub events: mpsc::Receiver<LinearEvent>,
    /// What the Linear task reads and flushes.
    pub queries: watch::Sender<Vec<RunQuery>>,
    /// The delegations the Linear task answers with a decline.
    pub declines: watch::Sender<Vec<Decline>>,
    /// `notify_one` after queuing outbox requests, so they go out at once.
    /// Each workspace's Linear task.
    pub linear_wake: Vec<Arc<Notify>>,
    /// Becomes `true` when the ticker should exit.
    pub shutdown: watch::Receiver<bool>,
    /// Notified when a subcommand wrote run files the ticker acts on.
    pub poke: Arc<Notify>,
    /// The present; nothing in a pass reads the wall clock.
    pub clock: fn() -> Timestamp,
}

/// One agent of one run: the coordinator, or a worker by id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgentKey {
    pub run: String,
    pub worker: Option<String>,
}

impl AgentKey {
    pub fn coordinator(run: &str) -> Self {
        AgentKey {
            run: run.into(),
            worker: None,
        }
    }

    pub fn worker(run: &str, id: &str) -> Self {
        AgentKey {
            run: run.into(),
            worker: Some(id.into()),
        }
    }

    /// How the error activity names the agent.
    pub fn role(&self) -> String {
        match &self.worker {
            None => "coordinator".into(),
            Some(id) => format!("worker {id}"),
        }
    }
}

/// The result of one slow Herdr request an effect task made.
#[derive(Debug)]
pub enum EffectDone {
    Placed {
        key: AgentKey,
        result: Result<Placed, HerdrError>,
    },
    Started {
        key: AgentKey,
        pane: String,
        result: Result<(), HerdrError>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    Place,
    Start,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutingDone {
    pub key: String,
    pub choice: crate::routing::Choice,
}

/// What woke the pass, each handled exactly once.
#[derive(Debug, Default)]
pub struct Wake {
    pub events: Vec<LinearEvent>,
    pub effects: Vec<EffectDone>,
    pub routing: Vec<RoutingDone>,
}

/// What a pass borrows from the ticker.
pub struct Deps<'a, H> {
    pub ctx: &'a Ctx<'a>,
    pub config: &'a Config,
    pub herdr: &'a H,
    pub socket: &'a str,
    pub log: &'a Log,
}

impl<H> Deps<'_, H> {
    pub(super) fn fail(&self, key: &str, error: &anyhow::Error) {
        self.log.line(&format!("{key}: {error:#}"));
    }
}

impl<H: Herdr> Deps<'_, H> {
    /// A Herdr notification, when the config allows them.
    pub(super) async fn notify(&self, title: &str, body: &str) {
        if self.config.notifications.herdr {
            let _ = self.herdr.notification_show(title, body).await;
        }
    }
}

/// The reconciler's memory between passes. Everything that must survive a
/// restart is in the run folders.
pub struct Reconciler {
    pub(super) effects: mpsc::Sender<EffectDone>,
    pub(super) routing_done: mpsc::Sender<RoutingDone>,
    pub(super) bin: String,
    /// At most one effect per agent; at most one start per run.
    pub(super) in_flight: BTreeMap<AgentKey, Effect>,
    /// Runs whose routing agent runs.
    pub(super) routing: BTreeSet<String>,
    /// Agents started in a pane where Herdr has not detected them yet.
    pub(super) launched: BTreeMap<AgentKey, (String, Timestamp)>,
    pub(super) seen_panes: BTreeSet<String>,
    pub(super) missing_since: BTreeMap<String, Timestamp>,
    /// Per run, the hash of the unseen inbox ids last nudged about.
    pub(super) nudged: BTreeMap<String, String>,
    /// Per pane, the token last reported and when.
    pub(super) reported: BTreeMap<String, (String, Timestamp)>,
    /// Per issue, when its run last changed status, as the time of the fact
    /// that changed it. A read or a delegated list made earlier is stale.
    pub(super) changed_at: BTreeMap<String, Timestamp>,
    /// Per workspace, the read time of the delegated list intake last used.
    pub(super) intake_read: BTreeMap<String, Timestamp>,
    /// Runs whose outbox is blocked, by issue id, and since when.
    pub(super) failing: BTreeMap<String, Timestamp>,
    pub(super) failure_notified: bool,
    /// Per run, when a heartbeat was queued and no activity was reported
    /// sent since; the flush and its `ActivitySent` may be a pass apart.
    pub(super) heartbeats: BTreeMap<String, Timestamp>,
    /// Runs whose coordinator got its launch prompt in this pass.
    pub(super) prompted: BTreeSet<String>,
    /// Agents whose last placement or start never reached Herdr, and when.
    pub(super) not_sent: BTreeMap<AgentKey, Timestamp>,
    /// Whether this pass's snapshot parsed completely.
    pub(super) trusted: bool,
    /// The count of unparsed snapshot entries last logged.
    pub(super) skipped: usize,
    pub(super) queued: bool,
    pub(super) queries: Vec<RunQuery>,
    /// Per run key, the delegation turned down in the latest delegated list:
    /// the decline its session gets, or `None` when no session tells who
    /// delegated. Each is logged once.
    pub(super) declined: BTreeMap<String, Option<Decline>>,
    /// Transcript copies still running, each giving its failures to log.
    pub(super) keeping: Vec<tokio::task::JoinHandle<Vec<String>>>,
    /// Postmortems being written, by run key.
    pub(super) writing: BTreeMap<
        String,
        (
            crate::postmortem::Stage,
            tokio::task::JoinHandle<super::postmortems::Written>,
        ),
    >,
}

pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| anyhow!("a file task failed: {e}"))?
}

pub(super) async fn update_run(
    run: &Run,
    change: impl FnOnce(&mut RunRecord) + Send + 'static,
) -> Result<RunRecord> {
    let run = run.clone();
    blocking(move || run.update(change)).await
}

pub(super) async fn update_worker(
    run: &Run,
    id: &str,
    change: impl FnOnce(&mut Worker) + Send + 'static,
) -> Result<Worker> {
    let (run, id) = (run.clone(), id.to_string());
    blocking(move || worker::update(&run, &id, change)).await
}

pub(super) async fn inbox_item(
    run: &Run,
    kind: &str,
    subject: &str,
    summary: String,
) -> Result<()> {
    let (run, kind, subject) = (run.clone(), kind.to_string(), subject.to_string());
    blocking(move || inbox::write(&run, &kind, &subject, &summary).map(|_| ())).await
}

pub(super) fn thought(body: impl Into<String>) -> Op {
    Op::Activity {
        activity: Activity::new(Content::Thought { body: body.into() }),
    }
}

pub(super) fn error_activity(body: impl Into<String>) -> Op {
    Op::Activity {
        activity: Activity::new(Content::Error { body: body.into() }),
    }
}

pub(super) fn since(timestamp: &str, now: Timestamp) -> Option<SignedDuration> {
    timestamp
        .parse::<Timestamp>()
        .ok()
        .map(|at| now.duration_since(at))
}

fn after(timestamp: &str, wait: SignedDuration) -> Option<Timestamp> {
    timestamp.parse::<Timestamp>().ok().map(|at| at + wait)
}

/// The wait after `attempts` counted unsuccessful attempts: 15 s, doubling.
pub(super) fn spacing(attempts: u32) -> SignedDuration {
    let doublings = attempts.saturating_sub(1).min(8);
    LAUNCH_SPACING * (1_i32 << doublings)
}

/// When the next placement or start of this agent may be made.
pub(super) fn attempt_due(record: &AgentRecord) -> Option<Timestamp> {
    after(&record.last_attempt_at, spacing(record.launch_attempts))
}

impl Reconciler {
    /// Whether a placement or start of `key` must wait: for the spacing of
    /// its counted attempts, or 15 s after one that never reached Herdr.
    pub(super) fn waits(&self, key: &AgentKey, record: &AgentRecord, now: Timestamp) -> bool {
        attempt_due(record).is_some_and(|at| at > now)
            || self
                .not_sent
                .get(key)
                .is_some_and(|at| *at + LAUNCH_SPACING > now)
    }
}

/// The fields `track` owns, copied only where they changed, so a field
/// another process wrote meanwhile is kept.
pub(super) fn apply_tracked(target: &mut AgentRecord, before: &AgentRecord, after: &AgentRecord) {
    macro_rules! changed {
        ($($field:ident),*) => {$(
            if before.$field != after.$field {
                target.$field = after.$field.clone();
            }
        )*};
    }
    changed!(
        workspace_id,
        tab_id,
        pane_id,
        agent_session,
        last_state,
        last_state_seq,
        last_state_change,
        blocked_reported
    );
}

impl Reconciler {
    pub fn new(
        effects: mpsc::Sender<EffectDone>,
        routing_done: mpsc::Sender<RoutingDone>,
    ) -> Result<Self> {
        Ok(Reconciler {
            effects,
            routing_done,
            bin: coordinator::binary_command()?,
            in_flight: BTreeMap::new(),
            routing: BTreeSet::new(),
            launched: BTreeMap::new(),
            seen_panes: BTreeSet::new(),
            missing_since: BTreeMap::new(),
            nudged: BTreeMap::new(),
            reported: BTreeMap::new(),
            changed_at: BTreeMap::new(),
            intake_read: BTreeMap::new(),
            failing: BTreeMap::new(),
            failure_notified: false,
            heartbeats: BTreeMap::new(),
            prompted: BTreeSet::new(),
            not_sent: BTreeMap::new(),
            trusted: false,
            skipped: 0,
            queued: false,
            queries: Vec::new(),
            declined: BTreeMap::new(),
            keeping: Vec::new(),
            writing: BTreeMap::new(),
        })
    }

    /// Whether an effect task or a routing agent is still running.
    #[cfg(test)]
    pub fn busy(&self) -> bool {
        !self.in_flight.is_empty()
            || !self.routing.is_empty()
            || !self.keeping.is_empty()
            || !self.writing.is_empty()
    }

    /// Waits for the transcript copies and postmortems still running.
    #[cfg(test)]
    pub async fn copies_done(&self) {
        while self.keeping.iter().any(|h| !h.is_finished())
            || self.writing.values().any(|(_, h)| !h.is_finished())
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// Entries per in-memory map.
    #[cfg(test)]
    pub fn remembered(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("launched", self.launched.len()),
            ("seen_panes", self.seen_panes.len()),
            ("missing_since", self.missing_since.len()),
            ("nudged", self.nudged.len()),
            ("reported", self.reported.len()),
            ("changed_at", self.changed_at.len()),
            ("failing", self.failing.len()),
            ("heartbeats", self.heartbeats.len()),
            ("not_sent", self.not_sent.len()),
        ]
    }

    #[cfg(test)]
    pub fn effects_in_flight(&self) -> usize {
        self.in_flight.len()
    }

    #[cfg(test)]
    pub fn routing_in_flight(&self) -> usize {
        self.routing.len()
    }

    /// The queries for the Linear task, as of the last pass.
    pub fn queries(&self) -> &[RunQuery] {
        &self.queries
    }

    /// Copies the transcripts of the run's agents into its folder, off the
    /// pass (see [`transcript::keep_agents`]).
    pub(super) fn keep_transcripts<H>(&mut self, d: &Deps<'_, H>, run: &Run) {
        let (roots, run) = (transcript::Roots::from_env(d.ctx.env), run.clone());
        self.keeping.push(tokio::task::spawn_blocking(move || {
            transcript::keep_agents(&roots, &run, None)
        }));
    }

    /// Logs the failures of the copies that finished.
    async fn log_kept<H>(&mut self, d: &Deps<'_, H>) {
        let (done, running) = std::mem::take(&mut self.keeping)
            .into_iter()
            .partition::<Vec<_>, _>(|h| h.is_finished());
        self.keeping = running;
        for handle in done {
            for line in handle.await.unwrap_or_default() {
                d.log.line(&line);
            }
        }
    }

    /// The declines for the Linear task, as of the last pass.
    pub fn declines(&self) -> Vec<Decline> {
        self.declined.values().flatten().cloned().collect()
    }

    /// Whether the last pass queued outbox requests; clears the flag.
    pub fn take_queued(&mut self) -> bool {
        std::mem::take(&mut self.queued)
    }

    pub(super) async fn push(&mut self, run: &Run, op: Op) -> Result<()> {
        let run = run.clone();
        blocking(move || outbox::push(&run, op).map(|_| ())).await?;
        self.queued = true;
        Ok(())
    }

    /// One critical section under the run lock: `work` writes the fields
    /// that guard its Linear writes and returns those writes, which are
    /// queued before the lock is released. A failure after a write is
    /// queued can then never make the write go out twice.
    pub(super) async fn guarded<T: Send + 'static>(
        &mut self,
        run: &Run,
        work: impl FnOnce(&Run, &RunLock) -> Result<(T, Vec<Op>)> + Send + 'static,
    ) -> Result<T> {
        let run = run.clone();
        let (value, queued) = blocking(move || {
            let lock = run.lock()?;
            let (value, ops) = work(&run, &lock)?;
            let queued = !ops.is_empty();
            for op in ops {
                outbox::push_held(&run, &lock, op)?;
            }
            Ok((value, queued))
        })
        .await?;
        self.queued |= queued;
        Ok(value)
    }

    /// `update_run` whose change also returns the requests it guards.
    pub(super) async fn update_and_push(
        &mut self,
        run: &Run,
        change: impl FnOnce(&mut RunRecord) -> Vec<Op> + Send + 'static,
    ) -> Result<RunRecord> {
        self.guarded(run, move |run, lock| {
            let mut ops = Vec::new();
            let record = run.update_held(lock, |r| ops = change(r))?;
            Ok((record, ops))
        })
        .await
    }

    pub async fn pass<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        levels: &Levels,
        wake: Wake,
        now: Timestamp,
    ) {
        self.log_kept(d).await;
        self.postmortems(d, now).await;
        let snapshot = d.herdr.snapshot().await.ok();
        let snap = snapshot.as_ref();
        if let Some(snapshot) = snap {
            for pane in snapshot.panes.keys() {
                self.seen_panes.insert(pane.0.clone());
                self.missing_since.remove(&pane.0);
            }
            if snapshot.skipped != self.skipped {
                self.skipped = snapshot.skipped;
                d.log.line(&format!(
                    "the Herdr snapshot has {} entries that do not parse; panes it lacks are not judged gone or empty",
                    snapshot.skipped
                ));
            }
        }
        // A partly parsed snapshot may lack a pane that exists: it serves
        // the rules that find agents, never those that judge a pane gone
        // or empty or prune what belongs to a pane.
        self.trusted = snap.is_some_and(|s| s.skipped == 0);
        let whole = snap.filter(|s| s.skipped == 0);
        for event in wake.events {
            self.apply_event(d, levels, whole, event, now).await;
        }
        self.intake(d, levels, now).await;
        if let Some(snapshot) = snap {
            self.deliver_interrupts(d, snapshot).await;
        }
        self.prompted.clear();
        for effect in wake.effects {
            self.apply_effect(d, effect, now).await;
        }
        for done in wake.routing {
            self.apply_routing(d, done).await;
        }
        for run in Run::list(&d.ctx.runs_dir()) {
            if !run.record().is_ok_and(|r| r.status == Status::Active) {
                continue;
            }
            if let Some(snapshot) = snap {
                if let Err(error) = self.watch(d, snapshot, &run, now).await {
                    d.fail(&run.key, &error);
                }
                if let Err(error) = self.launch(d, snapshot, &run, now).await {
                    d.fail(&run.key, &error);
                }
                if let Err(error) = self.nudge(d, snapshot, &run, now).await {
                    d.fail(&run.key, &error);
                }
            }
            if let Err(error) = self.heartbeat(d, whole, &run, now).await {
                d.fail(&run.key, &error);
            }
            let pruned = run.clone();
            let set_aside = blocking(move || Ok(inbox::prune_done(&pruned)))
                .await
                .unwrap_or_default();
            for name in set_aside {
                d.log.line(&format!(
                    "{}: moved inbox/{name} to inbox/done: not an inbox item of this build",
                    run.key
                ));
            }
        }
        self.write_failure_notice(d, now).await;
        if let Some(snapshot) = whole {
            let live: Vec<String> = snapshot.panes.keys().map(|p| p.0.clone()).collect();
            progress::prune(&d.ctx.state_dir(), d.socket, &live);
        }
        self.forget(d, whole);
        self.queries = self.compute_queries(d);
    }

    /// Drops what the in-memory maps hold for runs that are no longer active
    /// and, with a whole snapshot, for panes that are gone and no active
    /// agent records. A run's status change is kept until a delegated list
    /// read after it was handled, since only an older read or list could
    /// undo it.
    fn forget<H>(&mut self, d: &Deps<'_, H>, whole: Option<&Snapshot>) {
        let mut active = BTreeSet::new();
        let mut issues = BTreeSet::new();
        let mut recorded = BTreeSet::new();
        for run in Run::list(&d.ctx.runs_dir()) {
            let Ok(record) = run.record() else { continue };
            issues.insert(record.issue_id.clone());
            if record.status != Status::Active {
                continue;
            }
            recorded.insert(record.coordinator.pane_id.clone());
            recorded.extend(worker::list(&run).into_iter().map(|w| w.agent.pane_id));
            active.insert(run.key);
        }
        self.launched.retain(|key, _| active.contains(&key.run));
        self.not_sent.retain(|key, _| active.contains(&key.run));
        self.nudged.retain(|key, _| active.contains(key));
        self.heartbeats.retain(|key, _| active.contains(key));
        // Kept until every workspace read a delegated list after it: the
        // oldest of their reads, none while a workspace has not read one.
        let read = (self.intake_read.len() == d.config.workspaces.len())
            .then(|| self.intake_read.values().min().copied())
            .flatten();
        self.changed_at.retain(|issue_id, at| {
            issues.contains(issue_id) && read.is_none_or(|read| *at >= read)
        });
        if let Some(snapshot) = whole {
            let live = |pane: &String| snapshot.panes.contains_key(&PaneId(pane.clone()));
            self.reported.retain(|pane, _| live(pane));
            self.seen_panes
                .retain(|pane| live(pane) || recorded.contains(pane));
            self.missing_since.retain(|pane, _| recorded.contains(pane));
        }
    }

    async fn write_failure_notice<H: Herdr>(&mut self, d: &Deps<'_, H>, now: Timestamp) {
        let active: BTreeSet<String> = Run::list(&d.ctx.runs_dir())
            .iter()
            .filter_map(|run| run.record().ok())
            .filter(|r| r.status == Status::Active)
            .map(|r| r.issue_id)
            .collect();
        self.failing.retain(|issue_id, _| active.contains(issue_id));
        let Some(since) = self.failing.values().min().copied() else {
            self.failure_notified = false;
            return;
        };
        if !self.failure_notified && now.duration_since(since) >= WRITE_FAILURE_NOTICE {
            let _ = d
                .herdr
                .notification_show(
                    "herdr-linear-agent",
                    "Linear has not accepted writes for 10 minutes. They are kept and retried; see the ticker log.",
                )
                .await;
            self.failure_notified = true;
        }
    }

    /// Active runs, and runs of any status with queued requests (the flush
    /// covers detached and closed runs too). A run whose coordinator is not
    /// decided and whose routing does not run asks for the issue detail.
    fn compute_queries<H>(&self, d: &Deps<'_, H>) -> Vec<RunQuery> {
        Run::list(&d.ctx.runs_dir())
            .into_iter()
            .filter_map(|run| {
                let record = run.record().ok()?;
                let active = record.status == Status::Active;
                if !active && outbox::is_empty(&run) {
                    return None;
                }
                let undecided = active
                    && record.coordinator.profile.is_empty()
                    && !self.routing.contains(&run.key);
                // A team no longer in the config keeps the default review state.
                let review_state = d
                    .config
                    .team(&record.workspace, &record.team_key)
                    .map_or_else(|_| "In Review".to_string(), |t| t.review_state.clone());
                Some(RunQuery {
                    key: run.key.clone(),
                    review_state,
                    issue_id: record.issue_id,
                    run_dir: run.dir.clone(),
                    session_id: Some(record.session_id).filter(|s| !s.is_empty()),
                    prompt_cursor: record.prompt_cursor,
                    issue_updated_at: (!undecided).then_some(record.issue_updated_at),
                })
            })
            .collect()
    }

    /// The earliest time a time-based rule may become due, at most
    /// `METADATA_REFRESH` away. Only future times count: a rule that is due
    /// but held by another condition waits for the next wake.
    pub fn next_deadline<H>(&self, d: &Deps<'_, H>, now: Timestamp) -> Timestamp {
        let mut times: Vec<Timestamp> = Vec::new();
        // A postmortem's answer wakes nothing: it is looked for every 5 s.
        if !self.writing.is_empty() {
            times.push(now + SignedDuration::from_secs(5));
        }
        let hours = i64::try_from(d.config.limits.run_timeout_hours).unwrap_or(i64::MAX / 7200);
        let timeout = SignedDuration::from_hours(hours);
        for run in Run::list(&d.ctx.runs_dir()) {
            let Ok(record) = run.record() else { continue };
            if record.status != Status::Active {
                continue;
            }
            let mut agents = vec![record.coordinator.clone()];
            agents.extend(worker::list(&run).into_iter().map(|w| w.agent));
            for agent in &agents {
                agent_deadlines(agent, &mut times);
                // A `Waiting for you` self-report stops counting after 5 min.
                if agent.status == AgentStatus::Open
                    && let Some(report) =
                        progress::load(&d.ctx.state_dir(), d.socket, &agent.pane_id)
                    && report.waiting()
                {
                    times.extend(Timestamp::from_second(
                        report.reported_at + worker::SELF_REPORT_SECS,
                    ));
                }
            }
            if matches!(record.coordinator.last_state.as_str(), "idle" | "done") {
                times.extend(after(
                    &record.coordinator.last_state_change,
                    COORDINATOR_IDLE,
                ));
            }
            if !record.session_id.is_empty() && !record.stopped {
                times.extend(after(&record.last_activity, HEARTBEAT));
                if !record.timeout_asked {
                    times.extend(after(&record.timeout_since, timeout));
                }
            }
        }
        times.extend(self.launched.values().map(|(_, at)| *at + DETECTION_GRACE));
        times.extend(self.not_sent.values().map(|at| *at + LAUNCH_SPACING));
        times.extend(self.missing_since.values().map(|at| *at + PANE_GRACE));
        times.extend(self.reported.values().map(|(_, at)| *at + METADATA_REFRESH));
        if !self.failure_notified {
            times.extend(self.failing.values().map(|at| *at + WRITE_FAILURE_NOTICE));
        }
        times
            .into_iter()
            .filter(|at| *at > now)
            .min()
            .unwrap_or(now + METADATA_REFRESH)
            .min(now + METADATA_REFRESH)
    }

    /// Reports a pane's state token when it changed or is due for a refresh.
    pub(super) async fn report_pane<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        pane: &str,
        display: &str,
        state: &str,
        now: Timestamp,
    ) {
        let pane_id = PaneId(pane.to_string());
        if !snapshot.panes.contains_key(&pane_id) {
            return;
        }
        let token = format!("{display}\n{state}");
        if let Some((last, at)) = self.reported.get(pane)
            && *last == token
            && now.duration_since(*at) < METADATA_REFRESH
        {
            return;
        }
        let _ = d
            .herdr
            .report_metadata(
                &pane_id,
                progress::SOURCE,
                display,
                &[("hla_state".into(), state.into())],
                progress::TOKEN_TTL_MS,
            )
            .await;
        self.reported.insert(pane.to_string(), (token, now));
    }

    /// What the snapshot shows of an agent. A recorded pane this ticker has
    /// never seen counts as present (and empty) until `PANE_GRACE` passed.
    pub(super) fn live<H>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        record: &AgentRecord,
        now: Timestamp,
    ) -> Live {
        let mut live = worker::live_state(record, snapshot, now, &d.ctx.state_dir(), d.socket);
        let pane = &record.pane_id;
        if !live.pane_exists
            && !pane.is_empty()
            && !snapshot.panes.contains_key(&PaneId(pane.clone()))
            && !self.seen_panes.contains(pane)
        {
            let first = *self.missing_since.entry(pane.clone()).or_insert(now);
            live.pane_exists = now.duration_since(first) < PANE_GRACE;
        }
        live
    }
}

fn agent_deadlines(agent: &AgentRecord, times: &mut Vec<Timestamp>) {
    if matches!(agent.status, AgentStatus::Pending) || agent.prompt_pending {
        times.extend(attempt_due(agent));
    }
    if agent.status != AgentStatus::Open || agent.blocked_reported {
        return;
    }
    match agent.last_state.as_str() {
        "blocked" => times.extend(after(&agent.last_state_change, BLOCKED)),
        "unknown" if agent.prompt_pending => {
            times.extend(after(&agent.last_state_change, LAUNCH_DIALOG));
        }
        _ => {}
    }
}

/// Runs passes until shutdown. Each wake drains every ready message, then
/// runs one pass; between wakes it sleeps until the next deadline.
pub async fn run<H: Herdr + Clone + 'static>(mut inputs: Inputs<'_, H>) -> Result<()> {
    let (effects_tx, mut effects) = mpsc::channel(EFFECT_QUEUE);
    let (routing_tx, mut routing) = mpsc::channel(EFFECT_QUEUE);
    let mut reconciler = Reconciler::new(effects_tx, routing_tx)
        .context("the reconciler could not find its binary")?;
    let mut deadline = tokio::time::Instant::now();
    loop {
        let mut wake = Wake::default();
        tokio::select! {
            biased;
            changed = inputs.shutdown.changed() => {
                if changed.is_err() || *inputs.shutdown.borrow() {
                    return Ok(());
                }
            }
            changed = inputs.link.changed() => {
                if changed.is_err() {
                    bail!("the Herdr wake task ended");
                }
            }
            changed = inputs.level.changed() => {
                if changed.is_err() {
                    bail!("the Linear task ended");
                }
            }
            event = inputs.events.recv() => match event {
                Some(event) => wake.events.push(event),
                None => bail!("the Linear task ended"),
            },
            // The reconciler holds a sender of both, so they never close.
            Some(done) = effects.recv() => wake.effects.push(done),
            Some(done) = routing.recv() => wake.routing.push(done),
            () = inputs.poke.notified() => {}
            () = tokio::time::sleep_until(deadline) => {}
        }
        wake.events
            .extend(std::iter::from_fn(|| inputs.events.try_recv().ok()));
        wake.effects
            .extend(std::iter::from_fn(|| effects.try_recv().ok()));
        wake.routing
            .extend(std::iter::from_fn(|| routing.try_recv().ok()));
        drop(inputs.link.borrow_and_update());
        let current = inputs.level.borrow_and_update().clone();
        let now = (inputs.clock)();
        let socket = inputs.socket.borrow().clone();
        let deps = Deps {
            ctx: inputs.ctx,
            config: inputs.config,
            herdr: &inputs.herdr,
            socket: &socket,
            log: &inputs.log,
        };
        reconciler.pass(&deps, &current, wake, now).await;
        if *inputs.queries.borrow() != reconciler.queries() {
            inputs.queries.send_replace(reconciler.queries().to_vec());
        }
        let declines = reconciler.declines();
        let declined = *inputs.declines.borrow() != declines;
        if declined {
            inputs.declines.send_replace(declines);
        }
        if reconciler.take_queued() || declined {
            inputs.linear_wake.iter().for_each(|wake| wake.notify_one());
        }
        let wait = reconciler.next_deadline(&deps, now).duration_since(now);
        deadline =
            tokio::time::Instant::now() + std::time::Duration::try_from(wait).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::FakeHerdr;
    use crate::linear::api::IssueRef;
    use crate::linear::task::{Delegated, LinearLevel};
    use crate::paths::Env;
    use crate::process::fake::FakeRunner;

    const T0: &str = "2026-09-28T09:00:00Z";

    fn t0() -> Timestamp {
        T0.parse().unwrap()
    }

    fn delegated(key: &str) -> Levels {
        Levels::from([(
            "acme".to_string(),
            LinearLevel {
                app_user: Some("app".into()),
                delegated: Some(Delegated {
                    read_at: t0(),
                    issues: vec![IssueRef {
                        id: format!("id-{key}"),
                        identifier: key.into(),
                        title: "Loop".into(),
                        url: format!("https://linear.app/acme/issue/{key}"),
                        updated_at: T0.into(),
                        state: "unstarted".into(),
                        team: "DATA".into(),
                        delegator: Some(crate::linear::api::Delegator {
                            user: Some(crate::linear::api::User {
                                id: "user-1".into(),
                                name: "User One".into(),
                            }),
                            at: T0.into(),
                        }),
                        session: Some(crate::linear::api::SessionRef {
                            id: format!("session-{key}"),
                            status: "pending".into(),
                            responded_at: None,
                        }),
                    }],
                }),
            },
        )])
    }

    struct Rig {
        home: tempfile::TempDir,
        env: Env,
        runner: FakeRunner,
        config: Config,
    }

    fn rig() -> Rig {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_string_lossy().into_owned();
        let env = Env::for_test(
            home.path(),
            &[
                ("XDG_STATE_HOME", &format!("{root}/state")),
                ("XDG_CONFIG_HOME", &format!("{root}/config")),
            ],
        );
        Rig {
            config: crate::config::tests::sample(),
            runner: FakeRunner::new(),
            env,
            home,
        }
    }

    #[tokio::test]
    async fn a_wake_runs_a_pass_that_publishes_queries_and_wakes_the_linear_task() {
        let rig = rig();
        let ctx = Ctx {
            env: &rig.env,
            runner: &rig.runner,
            detached_ticker: false,
        };
        let herdr = FakeHerdr::new(rig.home.path());
        let (_link_tx, link) = watch::channel(Link {
            connected: true,
            since: t0(),
            wakes: 0,
            last_error: None,
        });
        let (level_tx, level) = watch::channel(Levels::new());
        let (events_tx, events) = mpsc::channel(8);
        let (queries, mut published) = watch::channel(Vec::new());
        let linear_wake = Arc::new(Notify::new());
        let (shutdown_tx, shutdown) = watch::channel(false);
        let log = Arc::new(Log::new(rig.home.path().join("ticker.log")));
        let reconciler = run(Inputs {
            ctx: &ctx,
            config: &rig.config,
            herdr: herdr.clone(),
            socket: watch::channel("/tmp/loop.sock".to_string()).1,
            log,
            link,
            level,
            events,
            queries,
            declines: watch::channel(Vec::new()).0,
            linear_wake: vec![linear_wake.clone()],
            shutdown,
            poke: Arc::new(Notify::new()),
            clock: t0,
        });
        let driver = async {
            level_tx.send(delegated("DATA-7")).unwrap();
            published.changed().await.unwrap();
            let queries = published.borrow_and_update().clone();
            assert_eq!(queries.len(), 1);
            assert_eq!(queries[0].issue_id, "id-DATA-7");
            assert_eq!(
                queries[0].issue_updated_at, None,
                "the claim asks for the detail"
            );
            linear_wake.notified().await;
            shutdown_tx.send(true).unwrap();
            drop(events_tx);
        };
        let (result, ()) = tokio::join!(reconciler, driver);
        result.unwrap();
        let run = Run::load(&ctx.runs_dir(), "acme/DATA-7").unwrap();
        assert_eq!(
            outbox::pending(&run).len(),
            2,
            "the thought and the started state"
        );
        assert!(herdr.requests().contains(&"session.snapshot".to_string()));
    }

    #[tokio::test]
    async fn a_poke_wakes_a_pass() {
        let rig = rig();
        let ctx = Ctx {
            env: &rig.env,
            runner: &rig.runner,
            detached_ticker: false,
        };
        let herdr = FakeHerdr::new(rig.home.path());
        let (_link_tx, link) = watch::channel(Link {
            connected: true,
            since: t0(),
            wakes: 0,
            last_error: None,
        });
        let (_level_tx, level) = watch::channel(Levels::new());
        let (events_tx, events) = mpsc::channel(8);
        let (queries, _published) = watch::channel(Vec::new());
        let (shutdown_tx, shutdown) = watch::channel(false);
        let poke = Arc::new(Notify::new());
        let passes = || {
            herdr
                .requests()
                .iter()
                .filter(|m| *m == "session.snapshot")
                .count()
        };
        let reconciler = run(Inputs {
            ctx: &ctx,
            config: &rig.config,
            herdr: herdr.clone(),
            socket: watch::channel("/tmp/loop.sock".to_string()).1,
            log: Arc::new(Log::new(rig.home.path().join("ticker.log"))),
            link,
            level,
            events,
            queries,
            declines: watch::channel(Vec::new()).0,
            linear_wake: vec![Arc::new(Notify::new())],
            shutdown,
            poke: poke.clone(),
            clock: t0,
        });
        let driver = async {
            while passes() == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            poke.notify_one();
            let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while passes() < 2 && tokio::time::Instant::now() < give_up {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let woken = passes() >= 2;
            shutdown_tx.send(true).unwrap();
            drop(events_tx);
            woken
        };
        let (result, woken) = tokio::join!(reconciler, driver);
        result.unwrap();
        assert!(woken, "the poke ran a pass");
    }

    #[tokio::test]
    async fn a_linear_task_that_ends_ends_the_loop_with_an_error() {
        let rig = rig();
        let ctx = Ctx {
            env: &rig.env,
            runner: &rig.runner,
            detached_ticker: false,
        };
        let (_link_tx, link) = watch::channel(Link {
            connected: false,
            since: t0(),
            wakes: 0,
            last_error: None,
        });
        let (_level_tx, level) = watch::channel(Levels::new());
        let (events_tx, events) = mpsc::channel(8);
        let (queries, _published) = watch::channel(Vec::new());
        let (_shutdown_tx, shutdown) = watch::channel(false);
        drop(events_tx);
        let error = run(Inputs {
            ctx: &ctx,
            config: &rig.config,
            herdr: FakeHerdr::new(rig.home.path()),
            socket: watch::channel("/tmp/loop.sock".to_string()).1,
            log: Arc::new(Log::new(rig.home.path().join("ticker.log"))),
            link,
            level,
            events,
            queries,
            declines: watch::channel(Vec::new()).0,
            linear_wake: vec![Arc::new(Notify::new())],
            shutdown,
            poke: Arc::new(Notify::new()),
            clock: t0,
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "the Linear task ended");
    }

    #[test]
    fn attempts_are_spaced_fifteen_seconds_doubling() {
        let table = [(0, 15), (1, 15), (2, 30), (3, 60), (4, 120)];
        for (attempts, seconds) in table {
            assert_eq!(
                spacing(attempts),
                SignedDuration::from_secs(seconds),
                "{attempts}"
            );
        }
        let record = AgentRecord {
            launch_attempts: 2,
            last_attempt_at: T0.into(),
            ..AgentRecord::default()
        };
        assert_eq!(
            attempt_due(&record),
            Some(t0() + SignedDuration::from_secs(30))
        );
        assert_eq!(attempt_due(&AgentRecord::default()), None, "never tried");
    }

    #[test]
    fn a_tracked_update_keeps_fields_another_process_wrote() {
        let before = AgentRecord {
            last_state: "idle".into(),
            prompt_pending: true,
            ..AgentRecord::default()
        };
        let mut after = before.clone();
        after.last_state = "working".into();
        after.last_state_seq = 4;
        // Meanwhile `worker restart` changed the profile and the pane.
        let mut stored = before.clone();
        stored.profile = "deep".into();
        stored.pane_id = "w9:p1".into();
        apply_tracked(&mut stored, &before, &after);
        assert_eq!(
            (stored.last_state.as_str(), stored.last_state_seq),
            ("working", 4)
        );
        assert_eq!(
            (stored.profile.as_str(), stored.pane_id.as_str()),
            ("deep", "w9:p1")
        );
    }
}
