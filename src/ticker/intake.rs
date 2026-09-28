//! The Linear side of a pass: Linear events, run reads (close, detach,
//! issue edits, relay), intake and claim, and routing.

use anyhow::Result;
use jiff::Timestamp;

use super::reconcile::{
    Deps, Reconciler, RoutingDone, blocking, inbox_item, thought, update_run, update_worker,
};
use crate::herdr::{Herdr, Snapshot, WorkspaceId};
use crate::linear::api::{Activity, Content, IssueDetail, IssueRef, Prompt, RunUpdate};
use crate::linear::task::{LinearEvent, LinearLevel};
use crate::outbox::{self, Op, StateTarget};
use crate::run::{AgentRecord, AgentStatus, Interrupt, Run, RunRecord, Status};
use crate::{coordinator, files, routing, worker};

/// Whether a prompt created at `created` is newer than the cursor. Both are
/// RFC 3339; text that does not parse is compared as text.
fn after_cursor(created: &str, cursor: &str) -> bool {
    match (created.parse::<Timestamp>(), cursor.parse::<Timestamp>()) {
        (Ok(created), Ok(cursor)) => created > cursor,
        _ => cursor.is_empty() || created > cursor,
    }
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
        level: &LinearLevel,
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
                let result = self
                    .apply_read(
                        d,
                        level,
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
        for prompt in prompts {
            let (created, user, body) = (
                prompt.created_at.clone(),
                prompt.user_id.clone(),
                prompt.body.clone(),
            );
            if !after_cursor(&created, &run.record()?.prompt_cursor) {
                continue;
            }
            if !d.config.linear.allowed_user_ids.contains(&prompt.user_id) {
                self.guarded(run, move |run, lock| {
                    if after_cursor(&created, &run.record()?.prompt_cursor) {
                        run.record_ignored_prompt_held(lock, &created, &user, &body)?;
                        run.update_held(lock, |r| r.prompt_cursor = created)?;
                    }
                    Ok(((), Vec::new()))
                })
                .await?;
                continue;
            }
            if prompt.signal.as_deref() == Some("stop") {
                // The keys and the response follow in the first pass with a
                // snapshot, which may be this one.
                update_run(run, move |r| {
                    if after_cursor(&created, &r.prompt_cursor) {
                        r.prompt_cursor = created;
                        r.stopped = true;
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
                if !after_cursor(&created, &run.record()?.prompt_cursor) {
                    return Ok(((), Vec::new()));
                }
                run.append_conversation_held(lock, &created, &user, &body)?;
                run.update_held(lock, move |r| {
                    r.prompt_cursor = created;
                    r.stopped = false;
                    if r.timeout_asked {
                        r.timeout_asked = false;
                        r.timeout_since = restart_window;
                    }
                    if r.coordinator_lost && resume {
                        r.coordinator_lost = false;
                        r.coordinator.repend();
                    }
                })?;
                Ok(((), Vec::new()))
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
                    Some(Interrupt::Stop) => vec![Op::Activity {
                        activity: Activity::new(Content::Response {
                            body: format!(
                                "Stopped {stopped} agent(s) as asked. Their worktrees are kept; reply here to continue."
                            ),
                        }),
                    }],
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
        update_run(run, |r| {
            r.status = Status::Closed;
            r.coordinator.status = AgentStatus::Stopped;
        })
        .await?;
        d.log
            .line(&format!("{}: closed (the issue is {state})", run.key));
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
        Ok(())
    }

    /// Picks up delegated issues from a delegated list this pass has not
    /// seen yet, while the limits leave room.
    pub(super) async fn intake<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        level: &LinearLevel,
        now: Timestamp,
    ) {
        let Some(delegated) = &level.delegated else {
            return;
        };
        if self.intake_read.is_some_and(|at| at >= delegated.read_at) {
            return;
        }
        self.intake_read = Some(delegated.read_at);
        let runs_dir = d.ctx.runs_dir();
        let paused = d.ctx.state_dir().join("paused").exists();
        for issue in &delegated.issues {
            if let Ok(run) = Run::load(&runs_dir, &issue.identifier) {
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
            if let Err(error) = self.claim(d, issue, delegated.read_at, now).await {
                d.log.line(&format!(
                    "{}: could not pick up: {error:#}",
                    issue.identifier
                ));
            }
        }
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
        issue: &IssueRef,
        read_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        crate::run::validate_key(&issue.identifier)?;
        d.ctx.ensure_state_dir()?;
        std::fs::create_dir_all(d.ctx.runs_dir())?;
        let now = now.to_string();
        let record = RunRecord {
            issue_id: issue.id.clone(),
            identifier: issue.identifier.clone(),
            title: issue.title.clone(),
            url: issue.url.clone(),
            team_key: issue.team.clone(),
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
        let key = run.key.clone();
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
        let key = run.key.clone();
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

    /// Starts the routing agent in its own task; its choice comes back as a
    /// `RoutingDone`.
    async fn route<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        detail: &IssueDetail,
    ) -> Result<()> {
        let routing = &d.config.routing;
        let profile = d.config.profile(&routing.agent)?.clone();
        let candidates = routing::candidates(d.config);
        let timeout = std::time::Duration::from_secs(routing.timeout_seconds);
        let issue = detail.clone();
        let path = d.ctx.env.var("PATH").map(str::to_string);
        let parent = std::env::temp_dir();
        let key = run.key.clone();
        let done = self.routing_done.clone();
        self.routing.insert(key.clone());
        tokio::spawn(async move {
            let choice = routing::choose(
                &profile,
                &candidates,
                &issue,
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
        if let routing::Choice::Default(routing::Fallback::Failed(error)) = &done.choice {
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
        let name = choice.profile(d.config).to_string();
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
