//! The Linear side of a pass: Linear events, run reads (close, detach,
//! issue edits, relay), intake and claim, and routing.

use std::time::Duration;

use anyhow::{Context, Result};
use jiff::Timestamp;

use super::reconcile::{
    Deps, Reconciler, RoutingDone, blocking, inbox_item, thought, update_run, update_worker,
};
use crate::herdr::{Herdr, Snapshot, WorkspaceId};
use crate::linear::api::{Activity, Content, IssueDetail, IssueRef, Prompt, RunUpdate};
use crate::linear::task::{Decline, Levels, LinearEvent, LinearLevel};
use crate::outbox::{self, Op, StateTarget};
use crate::run::{AgentRecord, AgentStatus, Interrupt, Run, RunRecord, Status, WaitReason};
use crate::{coordinator, files, routing, worker};

/// Whether a prompt created at `created` is newer than the cursor. Both are
/// RFC 3339; text that does not parse is compared as text.
fn after_cursor(created: &str, cursor: &str) -> bool {
    match (created.parse::<Timestamp>(), cursor.parse::<Timestamp>()) {
        (Ok(created), Ok(cursor)) => created > cursor,
        _ => cursor.is_empty() || created > cursor,
    }
}

/// What intake does with an issue's latest delegation.
enum Delegation {
    /// Someone the team allows to delegate delegated it.
    Allowed,
    /// Someone else, or no person, and the app's session was answered after
    /// the delegation: it was declined already.
    Answered,
    /// Someone else, or no person (automation or an agent): the decline for
    /// the app's session, or `None` when there is no session to answer.
    Declined(Option<Decline>),
}

fn delegation(key: &str, issue: &IssueRef, delegators: &[String]) -> Delegation {
    let delegator = issue.delegator.as_ref();
    if delegator
        .and_then(|d| d.user.as_ref())
        .is_some_and(|user| delegators.contains(&user.id))
    {
        return Delegation::Allowed;
    }
    let Some(session) = &issue.session else {
        return Delegation::Declined(None);
    };
    let at = |text: &str| text.parse::<Timestamp>().ok();
    let responded = session.responded_at.as_deref().and_then(at);
    let delegated = delegator.and_then(|d| at(&d.at));
    if responded.is_some() && responded >= delegated {
        Delegation::Answered
    } else {
        Delegation::Declined(Some(Decline {
            key: key.to_string(),
            session_id: session.id.clone(),
        }))
    }
}

/// The session of the issue's delegation, unless it has ended.
fn open_session(issue: &IssueRef) -> Option<String> {
    issue
        .session
        .as_ref()
        .filter(|session| session.status != "complete")
        .map(|session| session.id.clone())
}

/// Logs and notifies a delegation the ticker does not take, with the
/// delegator's ID for the team's `allowed_delegator_ids`.
async fn tell_declined<H: Herdr>(d: &Deps<'_, H>, key: &str, issue: &IssueRef) {
    let why = match issue.delegator.as_ref().map(|d| &d.user) {
        None => "Linear does not tell who delegated it".to_string(),
        Some(None) => "no person delegated it (automation or an agent did)".to_string(),
        Some(Some(user)) => format!(
            "delegated by {} ({}), who is not in allowed_delegator_ids of team {}",
            user.name, user.id, issue.team
        ),
    };
    d.log.line(&format!("{key}: not picked up: {why}"));
    d.notify(&format!("{key} not picked up"), &format!("{why}."))
        .await;
}

fn run_of<H>(d: &Deps<'_, H>, issue_id: &str) -> Option<(Run, RunRecord)> {
    Run::list(&d.ctx.runs_dir()).into_iter().find_map(|run| {
        let record = run.record().ok()?;
        (record.issue_id == issue_id).then_some((run, record))
    })
}

impl Reconciler {
    pub(super) async fn apply_event<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        levels: &Levels,
        snap: Option<&Snapshot>,
        event: LinearEvent,
        now: Timestamp,
    ) {
        let result = match event {
            LinearEvent::SessionFound {
                issue_id,
                session_id,
            } => match run_of(d, &issue_id) {
                Some((run, record)) if record.session_id.is_empty() => update_run(&run, move |r| {
                    if r.session_id.is_empty() {
                        r.session_id = session_id;
                    }
                })
                .await
                .map(|_| ()),
                _ => Ok(()),
            },
            LinearEvent::ActivitySent { issue_id, at } => match run_of(d, &issue_id) {
                Some((run, _)) => {
                    if self
                        .heartbeats
                        .get(&run.key)
                        .is_some_and(|queued| *queued <= at)
                    {
                        self.heartbeats.remove(&run.key);
                    }
                    update_run(&run, move |r| r.last_activity = at.to_string())
                        .await
                        .map(|_| ())
                }
                None => Ok(()),
            },
            LinearEvent::WritesFailing { issue_id, since } => {
                self.failing.insert(issue_id, since);
                Ok(())
            }
            LinearEvent::WritesRecovered { issue_id } => {
                self.failing.remove(&issue_id);
                Ok(())
            }
            LinearEvent::RunRead {
                issue_id,
                read_started_at,
                update,
                detail,
            } => {
                let Some((run, record)) = run_of(d, &issue_id) else {
                    return;
                };
                // A read made before the run's last status change (a
                // re-delegation, a reopen) must not undo it.
                let stale = self
                    .changed_at
                    .get(&issue_id)
                    .is_some_and(|at| read_started_at < *at);
                if record.status != Status::Active || stale {
                    return;
                }
                let level = levels.get(&record.workspace).cloned().unwrap_or_default();
                let result = self
                    .apply_read(
                        d,
                        &level,
                        snap,
                        &run,
                        &record,
                        update,
                        detail,
                        read_started_at,
                        now,
                    )
                    .await;
                if let Err(error) = result {
                    d.fail(&run.key, &error);
                }
                return;
            }
        };
        if let Err(error) = result {
            d.log.line(&format!("Linear event: {error:#}"));
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_read<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        level: &LinearLevel,
        snap: Option<&Snapshot>,
        run: &Run,
        record: &RunRecord,
        update: RunUpdate,
        detail: Option<Box<IssueDetail>>,
        read_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        let issue = &update.issue;
        if matches!(issue.state_type.as_str(), "completed" | "canceled") {
            self.changed_at.insert(record.issue_id.clone(), read_at);
            return self.close_run(d, snap, run, &issue.state_name, now).await;
        }
        if level.app_user.is_some() && issue.delegate_id != level.app_user {
            self.changed_at.insert(record.issue_id.clone(), read_at);
            return self.detach_run(d, run).await;
        }
        if let Some(detail) = detail {
            self.refresh_issue(run, record, &detail).await?;
            if record.coordinator.profile.is_empty() && !self.routing.contains(&run.key) {
                self.finish_claim(d, run, &detail).await?;
            }
        }
        self.relay(d, run, &update.prompts, now).await
    }

    /// Rewrites `issue.md` and tells the coordinator when a person edited
    /// the issue. The first write, at the claim, is not an edit.
    async fn refresh_issue(
        &mut self,
        run: &Run,
        record: &RunRecord,
        detail: &IssueDetail,
    ) -> Result<()> {
        files::write_atomic(
            &run.issue_md(),
            crate::run::issue_markdown(detail).as_bytes(),
        )?;
        let hash = crate::run::issue_hash(detail);
        let (updated_at, title) = (detail.updated_at.clone(), detail.title.clone());
        let labels: Vec<String> = detail.labels.iter().map(|l| l.name.clone()).collect();
        let stored = hash.clone();
        update_run(run, move |r| {
            r.issue_updated_at = updated_at;
            r.issue_hash = stored;
            r.title = title;
            r.labels = labels;
        })
        .await?;
        if !record.issue_hash.is_empty() && hash != record.issue_hash {
            inbox_item(
                run,
                "issue",
                "issue",
                "The issue was edited in Linear; issue.md is updated.".into(),
            )
            .await?;
        }
        Ok(())
    }

    /// Replies from allowed users reach the coordinator through
    /// `conversation.md` and the inbox; a stop signal interrupts the run's
    /// agents; anyone else's message is only recorded. The Linear task may
    /// read with an older cursor, so a prompt at or before the record's
    /// cursor was handled already; each prompt moves the cursor in the
    /// critical section that records it.
    async fn relay<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        prompts: &[Prompt],
        now: Timestamp,
    ) -> Result<()> {
        // The run's team decides whose replies count; a team no longer in the
        // config lets nobody's through.
        let record = run.record()?;
        let team = record.team_key.clone();
        let allowed = d
            .config
            .team(&record.workspace, &team)
            .map(|team| team.allowed_user_ids.clone())
            .unwrap_or_default();
        for prompt in prompts {
            let (created, user, body) = (
                prompt.created_at.clone(),
                prompt.user_id.clone(),
                prompt.body.clone(),
            );
            if !after_cursor(&created, &run.record()?.prompt_cursor) {
                continue;
            }
            if !allowed.contains(&prompt.user_id) {
                let recorded = self
                    .guarded(run, move |run, lock| {
                        let new = after_cursor(&created, &run.record()?.prompt_cursor);
                        if new {
                            run.record_ignored_prompt_held(lock, &created, &user, &body)?;
                            run.update_held(lock, |r| r.prompt_cursor = created)?;
                        }
                        Ok((new, Vec::new()))
                    })
                    .await?;
                if recorded {
                    let from = match prompt.user_name.as_str() {
                        "" => prompt.user_id.clone(),
                        name => format!("{name} ({})", prompt.user_id),
                    };
                    let hint =
                        format!("add the id to allowed_user_ids of team {team} to let it through");
                    d.log
                        .line(&format!("{}: ignored a reply from {from}; {hint}", run.key));
                    d.notify(
                        &format!("{} ignored a reply", run.key),
                        &format!("From {from}; {hint}."),
                    )
                    .await;
                }
                continue;
            }
            if prompt.signal.as_deref() == Some("stop") {
                // The keys and the response follow in the first pass with a
                // snapshot, which may be this one.
                update_run(run, move |r| {
                    if after_cursor(&created, &r.prompt_cursor) {
                        r.prompt_cursor = created;
                        r.reply_generation = r.reply_generation.saturating_add(1);
                        r.stopped = true;
                        if let Some(wait) = r.awaiting_reply.take() {
                            r.cleared_wait_id = wait.activity_id;
                        }
                        r.interrupt = Some(Interrupt::Stop);
                    }
                })
                .await?;
                continue;
            }
            // The inbox item comes first: a failure before the cursor moves
            // may repeat it, never the conversation entry.
            inbox_item(
                run,
                "reply",
                "reply",
                format!(
                    "A new reply from user {} is in conversation.md.",
                    prompt.user_id
                ),
            )
            .await?;
            let resume = prompt.body.trim().eq_ignore_ascii_case("resume");
            let restart_window = now.to_string();
            self.guarded(run, move |run, lock| {
                let current = run.record()?;
                if !after_cursor(&created, &current.prompt_cursor) {
                    return Ok((false, Vec::new()));
                }
                run.append_conversation_held(lock, &created, &user, &body)?;
                run.update_held(lock, move |r| {
                    r.prompt_cursor = created;
                    r.reply_generation = r.reply_generation.saturating_add(1);
                    r.stopped = false;
                    r.finished = false;
                    if let Some(wait) = r.awaiting_reply.take() {
                        if wait.reason == WaitReason::RunTimeout {
                            r.timeout_since = restart_window.clone();
                        }
                        r.cleared_wait_id = wait.activity_id;
                    }
                    if r.timeout_asked {
                        r.timeout_asked = false;
                        r.timeout_since = restart_window;
                    }
                    if r.coordinator_lost && resume {
                        r.coordinator_lost = false;
                        r.coordinator.repend();
                    }
                })?;
                Ok((true, Vec::new()))
            })
            .await?;
        }
        Ok(())
    }

    /// Sends the Escape keys a stop or a detach left pending, and after a
    /// stop posts how many agents it reached.
    pub(super) async fn deliver_interrupts<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
    ) {
        for run in Run::list(&d.ctx.runs_dir()) {
            if !run.record().is_ok_and(|r| r.interrupt.is_some()) {
                continue;
            }
            let stopped = self.interrupt_agents(d, Some(snapshot), &run).await;
            let result = self
                .update_and_push(&run, move |r| match r.interrupt.take() {
                    Some(Interrupt::Stop) => vec![Op::activity(Activity::new(Content::Response {
                            body: format!(
                                "Stopped {stopped} agent(s) as asked. Their worktrees are kept; reply here to continue."
                            ),
                        }))],
                    _ => Vec::new(),
                })
                .await;
            if let Err(error) = result {
                d.fail(&run.key, &error);
            }
        }
    }

    /// Escape in the coordinator's and every open worker's pane, for each
    /// agent found by identity. Returns how many were interrupted.
    async fn interrupt_agents<H: Herdr>(
        &self,
        d: &Deps<'_, H>,
        snap: Option<&Snapshot>,
        run: &Run,
    ) -> usize {
        let Some(snapshot) = snap else {
            return 0;
        };
        let mut records: Vec<AgentRecord> = worker::list(run)
            .into_iter()
            .filter(|w| w.agent.status == AgentStatus::Open)
            .map(|w| w.agent)
            .collect();
        if let Ok(record) = run.record() {
            records.insert(0, record.coordinator);
        }
        let mut stopped = 0;
        for record in &records {
            if let Some(agent) = worker::find_agent(record, &snapshot.agents)
                && d.herdr.send_escape(&agent.pane).await.is_ok()
            {
                stopped += 1;
            }
        }
        stopped
    }

    /// The issue was completed or canceled: copy the reports home, stop the
    /// agents and close their workspaces. Checkouts and branches stay.
    async fn close_run<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snap: Option<&Snapshot>,
        run: &Run,
        state: &str,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        self.interrupt_agents(d, snap, run).await;
        // The live workspace, or the recorded one; without a snapshot every
        // open agent's recorded workspace.
        let workspace_of = |agent: &AgentRecord| -> Option<String> {
            if agent.status != AgentStatus::Open {
                return None;
            }
            let Some(snapshot) = snap else {
                return Some(agent.workspace_id.clone()).filter(|w| !w.is_empty());
            };
            if let Some(found) = worker::find_agent(agent, &snapshot.agents) {
                return snapshot
                    .panes
                    .get(&found.pane)
                    .map(|p| p.workspace.0.clone())
                    .or_else(|| Some(agent.workspace_id.clone()));
            }
            let live = worker::live_state(agent, snapshot, now, &d.ctx.state_dir(), d.socket);
            live.pane_exists.then(|| agent.workspace_id.clone())
        };
        let mut workspaces = Vec::new();
        for w in worker::list(run) {
            let _ = worker::copy_report_home(run, &w);
            workspaces.extend(workspace_of(&w.agent));
            update_worker(run, &w.id, |w| w.agent.status = AgentStatus::Stopped).await?;
        }
        workspaces.extend(workspace_of(&record.coordinator));
        for workspace in workspaces {
            if let Err(error) = d
                .herdr
                .workspace_close(&WorkspaceId(workspace.clone()))
                .await
            {
                d.log.line(&format!(
                    "{}: could not close workspace {workspace}: {error}",
                    run.key
                ));
            }
        }
        // Linear shows the session working until a response ends it.
        let body = format!("The issue is {state}; this run is closed.");
        let closed_state = state.to_string();
        self.update_and_push(run, move |r| {
            r.status = Status::Closed;
            r.closed_state = closed_state;
            r.coordinator.status = AgentStatus::Stopped;
            r.postmortem_due = Some(crate::postmortem::Stage::Final);
            if let Some(wait) = r.awaiting_reply.take() {
                r.cleared_wait_id = wait.activity_id;
            }
            if r.session_id.is_empty() {
                return Vec::new();
            }
            vec![Op::activity(Activity::new(Content::Response { body }))]
        })
        .await?;
        d.log
            .line(&format!("{}: closed (the issue is {state})", run.key));
        self.keep_transcripts(d, run);
        Ok(())
    }

    /// The delegation was removed: stop the agents, keep the workspaces.
    /// The keys go out in the first pass with a snapshot.
    async fn detach_run<H: Herdr>(&mut self, d: &Deps<'_, H>, run: &Run) -> Result<()> {
        update_run(run, |r| {
            r.status = Status::Detached;
            r.interrupt.get_or_insert(Interrupt::Detach);
        })
        .await?;
        d.log
            .line(&format!("{}: detached (no longer delegated)", run.key));
        self.keep_transcripts(d, run);
        Ok(())
    }

    /// Picks up delegated issues from a delegated list this pass has not
    /// seen yet, while the limits leave room.
    pub(super) async fn intake<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        levels: &Levels,
        now: Timestamp,
    ) {
        for (workspace, level) in levels {
            self.intake_workspace(d, workspace, level, now).await;
        }
    }

    async fn intake_workspace<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        workspace: &str,
        level: &LinearLevel,
        now: Timestamp,
    ) {
        let Some(delegated) = &level.delegated else {
            return;
        };
        if self
            .intake_read
            .get(workspace)
            .is_some_and(|at| *at >= delegated.read_at)
        {
            return;
        }
        self.intake_read
            .insert(workspace.to_string(), delegated.read_at);
        let runs_dir = d.ctx.runs_dir();
        let paused = d.ctx.state_dir().join("paused").exists();
        let mut declined = std::collections::BTreeMap::new();
        for issue in &delegated.issues {
            let key = crate::run::run_key(workspace, &issue.identifier);
            let run = Run::load(&runs_dir, &key).ok();
            let active = run
                .as_ref()
                .and_then(|run| run.record().ok())
                .is_some_and(|record| record.status == Status::Active);
            // Only a claim or a restart takes a delegation: a running run
            // keeps going whoever opens another session on its issue.
            if !active {
                let delegators = d
                    .config
                    .team(workspace, &issue.team)
                    .map(|team| team.delegators().to_vec())
                    .unwrap_or_default();
                match delegation(&key, issue, &delegators) {
                    Delegation::Allowed => {}
                    Delegation::Answered => continue,
                    Delegation::Declined(decline) => {
                        if self.declined.get(&key) != Some(&decline) {
                            tell_declined(d, &key, issue).await;
                        }
                        declined.insert(key, decline);
                        continue;
                    }
                }
            }
            if let Some(run) = run {
                if let Err(error) = self.reactivate(&run, delegated.read_at).await {
                    d.fail(&run.key, &error);
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
            if active >= d.config.limits.max_runs as usize
                || worker::agent_count(&runs) + 1 > d.config.limits.max_agents as usize
            {
                break;
            }
            if let Err(error) = self
                .claim(d, workspace, issue, delegated.read_at, now)
                .await
            {
                d.log.line(&format!("{key}: could not pick up: {error:#}"));
            }
        }
        self.declined
            .retain(|key, _| crate::run::split_key(key).0 != workspace);
        self.declined.extend(declined);
    }

    /// A detached or closed run whose issue is delegated again, in a list
    /// read after the run stopped, becomes active again.
    async fn reactivate(&mut self, run: &Run, read_at: Timestamp) -> Result<()> {
        let record = run.record()?;
        if record.status == Status::Active
            || self
                .changed_at
                .get(&record.issue_id)
                .is_some_and(|at| read_at <= *at)
        {
            return Ok(());
        }
        self.changed_at.insert(record.issue_id.clone(), read_at);
        self.update_and_push(run, |r| {
            if r.status == Status::Active {
                return Vec::new();
            }
            r.status = Status::Active;
            if r.interrupt == Some(Interrupt::Detach) {
                r.interrupt = None;
            }
            // A closed run's coordinator was stopped: bring it back.
            if r.coordinator.status == AgentStatus::Stopped {
                r.coordinator.repend();
            }
            vec![thought("The issue was delegated again; the run continues.")]
        })
        .await?;
        inbox_item(
            run,
            "issue",
            "issue",
            "The issue was delegated to this agent again; the run is active again.".into(),
        )
        .await
    }

    /// Creates the run and queues the first thought and the started state.
    /// The Linear task opens the session with the flush, and the first run
    /// read brings the detail that writes `issue.md` and routes the run.
    async fn claim<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        workspace: &str,
        issue: &IssueRef,
        read_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        crate::run::validate_issue_key(&issue.identifier)?;
        d.ctx.ensure_state_dir()?;
        std::fs::create_dir_all(d.ctx.runs_dir())?;
        let now = now.to_string();
        let record = RunRecord {
            workspace: workspace.to_string(),
            issue_id: issue.id.clone(),
            identifier: issue.identifier.clone(),
            title: issue.title.clone(),
            url: issue.url.clone(),
            team_key: issue.team.clone(),
            session_id: open_session(issue).unwrap_or_default(),
            created: now.clone(),
            prompt_cursor: now.clone(),
            last_activity: now.clone(),
            timeout_since: now,
            announce_pending: true,
            ..RunRecord::default()
        };
        let runs_dir = d.ctx.runs_dir();
        let run = blocking(move || Run::create(&runs_dir, record)).await?;
        self.changed_at.insert(issue.id.clone(), read_at);
        d.log.line(&format!("{}: picked up", run.key));
        let key = issue.identifier.clone();
        self.update_and_push(&run, move |r| {
            r.announce_pending = false;
            vec![
                thought(format!("Picked up {key}.")),
                Op::IssueState {
                    target: StateTarget::Started,
                },
            ]
        })
        .await
        .map(|_| ())
    }

    /// A run without a decided coordinator: queues the claim's first thought
    /// when a crash cut the claim short, moves the issue to a started state
    /// unless that is queued or done, and routes it.
    async fn finish_claim<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        detail: &IssueDetail,
    ) -> Result<()> {
        let moved = matches!(
            detail.state.r#type.as_str(),
            "started" | "completed" | "canceled"
        );
        let queued = outbox::queued(run)
            .iter()
            .any(|request| matches!(request.op, Op::IssueState { .. }));
        let key = crate::run::split_key(&run.key).1.to_string();
        self.update_and_push(run, move |r| {
            let mut ops = Vec::new();
            if std::mem::take(&mut r.announce_pending) {
                ops.push(thought(format!("Picked up {key}.")));
            }
            if !moved && !queued {
                ops.push(Op::IssueState {
                    target: StateTarget::Started,
                });
            }
            ops
        })
        .await?;
        self.route(d, run, detail).await
    }

    /// Picks the coordinator with the run's routing: at once when it lists
    /// one, else by the routing agent in its own task, whose pick comes back
    /// as a `RoutingDone`.
    async fn route<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        detail: &IssueDetail,
    ) -> Result<()> {
        let record = run.record()?;
        let routing = d.config.routing_of(&record.workspace, &record.team_key)?;
        if let [only] = &routing.coordinators[..] {
            return self
                .decide(d, run, &routing::Choice::Only(only.clone()))
                .await;
        }
        let coordinators = routing::candidates(d.config, &routing.coordinators);
        let agent = routing
            .agent
            .as_deref()
            .context("the routing has no agent to pick with")?;
        let profile = d.config.profile(agent)?.clone();
        let timeout = Duration::from_secs(
            profile
                .timeout_seconds
                .unwrap_or(d.config.limits.routing_agent_timeout_seconds),
        );
        let input = routing::input(detail);
        let path = d.ctx.env.var("PATH").map(str::to_string);
        let parent = std::env::temp_dir();
        let key = run.key.clone();
        let done = self.routing_done.clone();
        self.routing.insert(key.clone());
        tokio::spawn(async move {
            let choice = routing::choose(
                &profile,
                routing::Pick::Coordinator,
                &coordinators,
                &input,
                timeout,
                path.as_deref(),
                &parent,
            )
            .await;
            let _ = done.send(RoutingDone { key, choice }).await;
        });
        Ok(())
    }

    pub(super) async fn apply_routing<H: Herdr>(&mut self, d: &Deps<'_, H>, done: RoutingDone) {
        self.routing.remove(&done.key);
        let Ok(run) = Run::load(&d.ctx.runs_dir(), &done.key) else {
            return;
        };
        if let routing::Choice::Default(_, routing::Fallback::Failed(error)) = &done.choice {
            d.log
                .line(&format!("{}: the routing agent failed: {error}", run.key));
        }
        if let Err(error) = self.decide(d, &run, &done.choice).await {
            d.fail(&run.key, &error);
        }
    }

    /// Records the coordinator profile and reserves the coordinator's launch.
    async fn decide<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        choice: &routing::Choice,
    ) -> Result<()> {
        let record = run.record()?;
        if !record.coordinator.profile.is_empty() || record.status != Status::Active {
            return Ok(());
        }
        let name = choice.profile().to_string();
        let profile = d.config.profile(&name)?;
        let mut pending = coordinator::pending_record(&record, &name, &profile.kind);
        pending.agent_session = record.coordinator.agent_session.clone();
        let source = choice.source();
        self.update_and_push(run, move |r| {
            if !r.coordinator.profile.is_empty() {
                return Vec::new();
            }
            r.routing_source = source.clone();
            r.coordinator = pending;
            vec![thought(format!(
                "The coordinator uses the `{name}` profile ({source})."
            ))]
        })
        .await
        .map(|_| ())
    }
}
