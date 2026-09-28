//! Launching agents: the coordinator's placement, starts in panes at a shell
//! prompt, and launch prompts. Placements and starts are slow, so they run
//! as effect tasks; their results come back as [`EffectDone`].

use anyhow::Result;
use jiff::Timestamp;

use super::reconcile::{
    AgentKey, DETECTION_GRACE, Deps, Effect, EffectDone, MAX_LAUNCH_ATTEMPTS, Reconciler,
    error_activity, update_run,
};
use crate::config::Config;
use crate::herdr::{Herdr, HerdrError, PaneId, Placed, Snapshot};
use crate::run::{AgentRecord, AgentStatus, Run, RunRecord, Status};
use crate::{agents, claude_trust, coordinator, worker};

/// The agent CLI's arguments: the profile's flags and, for a resume, the
/// session. Codex takes its `resume <id>` words first.
fn start_args(config: &Config, record: &AgentRecord) -> Result<Vec<String>> {
    let mut args = agents::profile_args(config.profile(&record.profile)?);
    if record.resume
        && let Some(resume) = agents::resume_args(&record.kind, &record.agent_session)
    {
        if record.kind == "codex" {
            args.splice(0..0, resume);
        } else {
            args.extend(resume);
        }
    }
    Ok(args)
}

/// Sets a placed coordinator open in its root pane, ready to be started.
fn place_coordinator(record: &mut RunRecord, placed: &Placed) {
    let c = &mut record.coordinator;
    c.placed(placed);
    c.last_state.clear();
    c.last_state_change.clear();
    c.last_state_seq = 0;
    c.blocked_reported = false;
}

/// A pane in the run folder that is empty or holds our agent: a placement
/// whose answer did not arrive. The label is not compared, since a title
/// edit changes it.
fn placed_before(snapshot: &Snapshot, record: &AgentRecord, cwd: &str) -> Option<Placed> {
    let ours = |agent: &crate::herdr::Agent| {
        agent.kind.as_deref() == Some(record.kind.as_str())
            && agent
                .name
                .as_deref()
                .is_none_or(|n| n.is_empty() || n == record.agent_name)
    };
    snapshot
        .panes
        .values()
        .filter(|p| p.cwd.as_deref() == Some(cwd) || p.foreground_cwd.as_deref() == Some(cwd))
        .find(|p| snapshot.agents.iter().filter(|a| a.pane == p.id).all(ours))
        .map(|p| Placed {
            workspace: p.workspace.clone(),
            tab: p.tab.clone(),
            pane: p.id.clone(),
            cwd: cwd.to_string(),
            worktree_path: None,
        })
}

/// A prompt whose answer was lost counts as delivered: a missed one is
/// recovered by the nudge, a doubled one is not.
pub(super) fn delivered(result: Result<(), HerdrError>) -> Result<(), HerdrError> {
    match result {
        Err(HerdrError::OutcomeUnknown(_)) => Ok(()),
        other => other,
    }
}

fn pane_is_empty(snapshot: &Snapshot, pane: &str) -> bool {
    let id = PaneId(pane.to_string());
    snapshot.panes.contains_key(&id) && !snapshot.agents.iter().any(|a| a.pane == id)
}

impl Reconciler {
    pub(super) async fn apply_effect<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        done: EffectDone,
        now: Timestamp,
    ) {
        let key = match &done {
            EffectDone::Placed { key, .. } | EffectDone::Started { key, .. } => key.clone(),
        };
        self.in_flight.remove(&key);
        let Ok(run) = Run::load(&d.ctx.runs_dir(), &key.run) else {
            return;
        };
        let result = match done {
            EffectDone::Placed { result, .. } => self.placed(d, &run, &key, result, now).await,
            EffectDone::Started { pane, result, .. } => {
                self.started(&run, &key, pane, result, now).await
            }
        };
        if let Err(error) = result {
            d.fail(&run.key, &error);
        }
    }

    async fn placed<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        key: &AgentKey,
        result: Result<Placed, HerdrError>,
        now: Timestamp,
    ) -> Result<()> {
        match result {
            Ok(placed) => {
                let found = placed.clone();
                // Decided under the lock: the run may have moved on while
                // the workspace was created.
                let applied = self
                    .guarded(run, move |run, lock| {
                        let record = run.record()?;
                        if record.status != Status::Active
                            || record.coordinator.status != AgentStatus::Pending
                        {
                            return Ok((false, Vec::new()));
                        }
                        run.update_held(lock, |r| place_coordinator(r, &found))?;
                        Ok((true, Vec::new()))
                    })
                    .await?;
                if !applied {
                    let _ = d.herdr.workspace_close(&placed.workspace).await;
                }
                Ok(())
            }
            // The next snapshot shows whether it was placed.
            Err(HerdrError::OutcomeUnknown(_)) => {
                let at = now.to_string();
                update_run(run, move |r| {
                    if r.status == Status::Active && r.coordinator.status == AgentStatus::Pending {
                        r.coordinator.last_attempt_at = at;
                    }
                })
                .await?;
                Ok(())
            }
            // Herdr down is not an attempt; the next one waits 15 s.
            Err(HerdrError::NotSent(_)) => {
                self.not_sent.insert(key.clone(), now);
                Ok(())
            }
            Err(error) => {
                self.attempt_failed(run, key, None, &error.to_string(), now)
                    .await
            }
        }
    }

    async fn started(
        &mut self,
        run: &Run,
        key: &AgentKey,
        pane: String,
        result: Result<(), HerdrError>,
        now: Timestamp,
    ) -> Result<()> {
        // A result for a pane the agent left (a restart, a close) is dropped.
        let current = self
            .agent_record(run, key)
            .is_ok_and(|a| a.status == AgentStatus::Open && a.pane_id == pane)
            && run.record().is_ok_and(|r| r.status == Status::Active);
        if !current {
            return Ok(());
        }
        match result {
            // Herdr may not show the agent yet: it is not started again while
            // the grace lasts.
            Ok(()) | Err(HerdrError::OutcomeUnknown(_)) => {
                self.launched.insert(key.clone(), (pane, now));
                Ok(())
            }
            Err(HerdrError::NotSent(_)) => {
                self.not_sent.insert(key.clone(), now);
                Ok(())
            }
            Err(error) => {
                self.attempt_failed(run, key, Some(pane), &error.to_string(), now)
                    .await
            }
        }
    }

    /// Counts a failed placement (`pane` none: the agent must still be
    /// pending) or start (the agent must still be open in `pane`) of an
    /// active run; the third fails the agent. Decided under the lock.
    async fn attempt_failed(
        &mut self,
        run: &Run,
        key: &AgentKey,
        pane: Option<String>,
        error: &str,
        now: Timestamp,
    ) -> Result<()> {
        let (message, at) = (error.to_string(), now.to_string());
        let activity = error_activity(format!("Could not start the {} agent: {error}", key.role()));
        let worker_id = key.worker.clone();
        self.guarded(run, move |run, lock| {
            let record = run.record()?;
            let agent = match &worker_id {
                None => record.coordinator.clone(),
                Some(id) => worker::load(run, id)?.agent,
            };
            let current = match &pane {
                None => agent.status == AgentStatus::Pending,
                Some(pane) => agent.status == AgentStatus::Open && agent.pane_id == *pane,
            };
            if record.status != Status::Active || !current {
                return Ok(((), Vec::new()));
            }
            let count = |a: &mut AgentRecord| {
                a.launch_attempts += 1;
                a.last_attempt_at = at;
                if a.launch_attempts >= MAX_LAUNCH_ATTEMPTS {
                    a.status = AgentStatus::Failed;
                    a.error = message;
                }
            };
            let failed = match &worker_id {
                None => {
                    run.update_held(lock, |r| count(&mut r.coordinator))?
                        .coordinator
                }
                Some(id) => worker::update_held(run, lock, id, |w| count(&mut w.agent))?.agent,
            }
            .status
                == AgentStatus::Failed;
            Ok(((), if failed { vec![activity] } else { Vec::new() }))
        })
        .await
    }

    /// A start Herdr has not detected once the grace ended counts as an
    /// unsuccessful attempt, so a start that never shows ends after three.
    async fn expire_undetected(
        &mut self,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let expired: Vec<(AgentKey, String)> = self
            .launched
            .iter()
            .filter(|(key, (_, at))| {
                key.run == run.key && now.duration_since(*at) >= DETECTION_GRACE
            })
            .map(|(key, (pane, _))| (key.clone(), pane.clone()))
            .collect();
        for (key, pane) in expired {
            self.launched.remove(&key);
            if pane_is_empty(snapshot, &pane) {
                let grace = DETECTION_GRACE.as_secs();
                let error = format!("Herdr did not detect the agent within {grace} s");
                self.attempt_failed(run, &key, Some(pane), &error, now)
                    .await?;
            }
        }
        Ok(())
    }

    fn agent_record(&self, run: &Run, key: &AgentKey) -> Result<AgentRecord> {
        Ok(match &key.worker {
            None => run.record()?.coordinator,
            Some(id) => worker::load(run, id)?.agent,
        })
    }

    fn start_in_flight(&self, run: &str) -> bool {
        self.in_flight
            .iter()
            .any(|(key, effect)| key.run == run && *effect == Effect::Start)
    }

    pub(super) async fn launch<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        if record.status != Status::Active {
            return Ok(());
        }
        // Placing and starting judge panes absent or empty.
        let trusted = self.trusted;
        if trusted {
            self.expire_undetected(snapshot, run, now).await?;
        }
        let record = run.record()?;
        let coordinator_key = AgentKey::coordinator(&run.key);
        let c = &record.coordinator;
        if trusted
            && c.status == AgentStatus::Pending
            && !c.profile.is_empty()
            && !self.in_flight.contains_key(&coordinator_key)
        {
            self.place(d, snapshot, run, &record, now).await?;
        }

        let record = run.record()?;
        let workers = worker::list(run);
        let mut candidates: Vec<(AgentKey, AgentRecord, Vec<String>)> = vec![(
            coordinator_key,
            record.coordinator.clone(),
            vec![run.canonical_dir().to_string_lossy().into_owned()],
        )];
        candidates.extend(workers.iter().map(|w| {
            (
                AgentKey::worker(&run.key, &w.id),
                w.agent.clone(),
                vec![w.agent.cwd.clone(), w.repo_path.clone()],
            )
        }));
        if trusted && !self.start_in_flight(&run.key) {
            for (key, agent, trusted) in &candidates {
                if self.start(d, snapshot, key, agent, trusted, now)? {
                    break;
                }
            }
        }
        self.prompt_coordinator(d, snapshot, run, &record, now)
            .await?;
        for w in &workers {
            self.prompt_worker(d, snapshot, run, &record, w, now)
                .await?;
        }
        Ok(())
    }

    /// Adopts a workspace whose creation was not answered, or creates one.
    async fn place<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        record: &RunRecord,
        now: Timestamp,
    ) -> Result<()> {
        // Rewritten at every placement, so an updated binary's path is what
        // the coordinator sees.
        coordinator::write_priming(run, record, &self.bin)?;
        let cwd = run.canonical_dir().to_string_lossy().into_owned();
        let label = coordinator::workspace_label(record);
        if let Some(found) = placed_before(snapshot, &record.coordinator, &cwd) {
            update_run(run, move |r| {
                if r.coordinator.status == AgentStatus::Pending {
                    place_coordinator(r, &found);
                }
            })
            .await?;
            return Ok(());
        }
        let key = AgentKey::coordinator(&run.key);
        if self.waits(&key, &record.coordinator, now) {
            return Ok(());
        }
        self.not_sent.remove(&key);
        self.in_flight.insert(key.clone(), Effect::Place);
        let (herdr, done) = (d.herdr.clone(), self.effects.clone());
        tokio::spawn(async move {
            let result = herdr.workspace_create(&cwd, &label).await;
            let _ = done.send(EffectDone::Placed { key, result }).await;
        });
        Ok(())
    }

    /// Starts an open agent whose launch prompt is pending in its pane, when
    /// the pane is at its shell prompt. Returns whether a start went out.
    fn start<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        key: &AgentKey,
        agent: &AgentRecord,
        trusted: &[String],
        now: Timestamp,
    ) -> Result<bool> {
        if agent.status != AgentStatus::Open
            || !agent.prompt_pending
            || self.in_flight.contains_key(key)
            || !pane_is_empty(snapshot, &agent.pane_id)
            || self.waits(key, agent, now)
        {
            return Ok(false);
        }
        if let Some((pane, at)) = self.launched.get(key)
            && *pane == agent.pane_id
            && now.duration_since(*at) < DETECTION_GRACE
        {
            return Ok(false);
        }
        let args = start_args(d.config, agent)?;
        if d.config.claude.auto_accept_trust_dialog && agent.kind == "claude" {
            let dirs: Vec<&str> = trusted.iter().map(String::as_str).collect();
            if let Err(error) = claude_trust::trust(d.ctx.env, &dirs) {
                d.fail(&key.run, &error);
            }
        }
        self.launched.remove(key);
        self.not_sent.remove(key);
        self.in_flight.insert(key.clone(), Effect::Start);
        let (herdr, done) = (d.herdr.clone(), self.effects.clone());
        let (key, name, kind, pane) = (
            key.clone(),
            agent.agent_name.clone(),
            agent.kind.clone(),
            agent.pane_id.clone(),
        );
        tokio::spawn(async move {
            let result = herdr
                .agent_start(&name, &kind, &PaneId(pane.clone()), &args)
                .await;
            let _ = done.send(EffectDone::Started { key, pane, result }).await;
        });
        Ok(true)
    }

    async fn prompt_coordinator<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        record: &RunRecord,
        now: Timestamp,
    ) -> Result<()> {
        let c = &record.coordinator;
        if c.status != AgentStatus::Open
            || !c.prompt_pending
            || record.stopped
            || record.timeout_asked
        {
            return Ok(());
        }
        let Some(agent) = worker::find_agent(c, &snapshot.agents).filter(|a| a.status.is_idle())
        else {
            return Ok(());
        };
        self.launched.remove(&AgentKey::coordinator(&run.key));
        let text = coordinator::launch_prompt(&run.key, c.resume);
        delivered(d.herdr.agent_prompt(&agent.pane, &text).await)?;
        self.prompted.insert(run.key.clone());
        let seq = agent.state_change_seq;
        update_run(run, move |r| {
            r.coordinator.prompt_pending = false;
            r.coordinator.prompted_at = now.to_string();
            r.coordinator.prompted_seq = seq;
        })
        .await?;
        Ok(())
    }

    /// The worker's launch prompt; the `Start worker` action goes to Linear
    /// once it is delivered.
    async fn prompt_worker<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        record: &RunRecord,
        w: &worker::Worker,
        now: Timestamp,
    ) -> Result<()> {
        if w.agent.status != AgentStatus::Open || !w.agent.prompt_pending || record.stopped {
            return Ok(());
        }
        let Some(agent) =
            worker::find_agent(&w.agent, &snapshot.agents).filter(|a| a.status.is_idle())
        else {
            return Ok(());
        };
        self.launched.remove(&AgentKey::worker(&run.key, &w.id));
        let prompt = worker::launch_prompt(&run.key, &w.id);
        delivered(d.herdr.agent_prompt(&agent.pane, &prompt).await)?;
        let seq = agent.state_change_seq;
        let id = w.id.clone();
        self.guarded(run, move |run, lock| {
            let w = worker::update_held(run, lock, &id, |w| {
                w.agent.prompt_pending = false;
                w.agent.prompted_at = now.to_string();
                w.agent.prompted_seq = seq;
            })?;
            Ok(((), vec![worker::start_action(&w)]))
        })
        .await
    }
}
