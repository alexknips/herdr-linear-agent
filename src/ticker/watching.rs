//! Watching coordinators and workers from a snapshot, and the heartbeat.

use anyhow::Result;
use jiff::Timestamp;

use super::reconcile::{
    COORDINATOR_IDLE, Deps, HEARTBEAT, PANE_GRACE, Reconciler, apply_tracked, error_activity,
    inbox_item, since, update_worker,
};
use crate::agents;
use crate::herdr::{Herdr, PaneId, Snapshot};
use crate::linear::api::{Activity, Content, ExternalUrl};
use crate::outbox::{self, Op};
use crate::run::{AgentRecord, AgentStatus, Recovery, Run, WaitReason};
use crate::worker::{self, Group, Live, Worker};
use crate::{coordinator, files, inbox, transcript};

/// Why a worker waits on a person, for its inbox item.
fn waiting_reason(w: &Worker, live: &Live) -> String {
    let asked = live
        .self_report
        .as_ref()
        .is_some_and(crate::progress::Record::waiting);
    if w.agent.status == AgentStatus::Failed {
        format!("failed: {}", w.agent.error)
    } else if !live.pane_exists {
        "its pane closed before it wrote a report".into()
    } else if asked {
        "it asked a question in its report".into()
    } else if worker::needs_person(&w.agent, live) {
        format!("it waits on a dialog in pane {}", w.agent.pane_id)
    } else {
        live.agent_state
            .map_or_else(|| "no agent".into(), |s| s.as_str().to_string())
    }
}

impl Reconciler {
    /// Keeps an agent record in step with Herdr: a renumbered pane, a lost
    /// name, the native session, and the status with when its episode began.
    /// The same status with a new `state_change_seq` is a new episode.
    async fn track<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        key: &super::reconcile::AgentKey,
        record: &AgentRecord,
        now: Timestamp,
    ) -> (AgentRecord, Live) {
        let seen = self.live(d, snapshot, record, now);
        let may_resume = run.record().is_ok_and(|r| {
            r.status == crate::run::Status::Active
                && !r.stopped
                && !r.finished
                && r.awaiting_reply.is_none()
        }) && match &key.worker {
            None => !worker::needs_person(record, &seen),
            Some(id) => worker::load(run, id).is_ok_and(|w| {
                !w.restarting
                    && w.report_hash.is_empty()
                    && worker::report_hash(&w).is_none()
                    && w.agent.last_group != "waiting_on_you"
                    && seen.self_report.as_ref().is_none_or(|r| !r.waiting())
            }),
        };
        let mut next = record.clone();
        if let Some((workspace, tab, pane)) = &seen.moved_to {
            next.workspace_id = workspace.clone();
            next.tab_id = tab.clone();
            next.pane_id = pane.clone();
        }
        if let Some(agent) = &seen.agent {
            if may_resume
                && let Recovery::Starting {
                    attempt, session, ..
                } = &record.recovery
                && agent.interactive_ready
                && agent.status.is_idle()
            {
                let prompt = "The previous agent process ended while work was in progress. Reread AGENTS.md and the issue/task context, inspect the current workspace and existing changes, then continue the same task from this session. Do not repeat completed work.";
                let still_current = run.record().is_ok_and(|r| {
                    r.status == crate::run::Status::Active
                        && !r.stopped
                        && !r.finished
                        && r.awaiting_reply.is_none()
                }) && self.agent_record(run, key).is_ok_and(|current| {
                    current.recovery == record.recovery && current.pane_id == record.pane_id
                });
                if still_current
                    && super::launch::delivered(d.herdr.agent_prompt(&agent.pane, prompt).await)
                        .is_ok()
                {
                    next.recovery = Recovery::Recovered {
                        attempts: *attempt,
                        session: session.clone(),
                    };
                }
            }
            if agent.name.as_deref().is_none_or(str::is_empty) && !record.agent_name.is_empty() {
                let _ = d.herdr.agent_rename(&agent.pane, &record.agent_name).await;
            }
            if let Some(session) = agent.session.as_deref().filter(|s| !s.is_empty()) {
                next.agent_session = session.to_string();
            }
        }
        let state = seen
            .agent_state
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let seq = seen.agent.as_ref().map_or(0, |a| a.state_change_seq);
        if state != record.last_state || seq != record.last_state_seq {
            next.last_state = state;
            next.last_state_seq = seq;
            next.last_state_change = now.to_string();
            next.blocked_reported = false;
        }
        let live = self.live(d, snapshot, &next, now);
        if !worker::needs_person(&next, &live) {
            next.blocked_reported = false;
        }
        (next, live)
    }

    /// The elicitation that asks a person to answer a dialog in a Herdr
    /// pane, and the notification that goes with it.
    fn person_needed<H>(
        d: &Deps<'_, H>,
        run: &Run,
        label: &str,
        who: &str,
        record: &AgentRecord,
    ) -> (Op, (String, String)) {
        let body = format!(
            "{who} needs someone in Herdr: it is waiting on a dialog in pane `{}` (session `{}`, run {label}). Answer it there.",
            record.pane_id,
            d.config.herdr.session.as_deref().unwrap_or("default")
        );
        let notice = (format!("{} needs you", run.key), body.clone());
        (super::reconcile::thought(body), notice)
    }

    pub(super) async fn watch<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        let label = coordinator::workspace_label(&record);
        let c = &record.coordinator;
        if c.status == AgentStatus::Open && self.judged(d, snapshot, c, now) {
            let (mut next, live) = self
                .track(
                    d,
                    snapshot,
                    run,
                    &super::reconcile::AgentKey::coordinator(&run.key),
                    c,
                    now,
                )
                .await;
            let needs = worker::needs_person(&next, &live);
            next.recovery = self
                .recovery_state(
                    d,
                    &next,
                    &live,
                    !record.stopped
                        && !record.finished
                        && record.awaiting_reply.is_none()
                        && !needs,
                    now,
                )
                .await;
            if record.stopped || record.finished || record.status != crate::run::Status::Active {
                next.recovery = Recovery::None;
            }
            let mut stale_notice = None;
            if let Recovery::Stale {
                attempts,
                reason,
                reported: false,
            } = next.recovery.clone()
            {
                let expected = next.recovery.clone();
                let body = format!(
                    "Automatic resume stopped after {attempts} attempts. {reason}. Reply `resume` when ready."
                );
                let reported = self
                    .update_and_push(run, move |r| {
                        if r.status != crate::run::Status::Active
                            || r.stopped
                            || r.finished
                            || r.awaiting_reply.is_some()
                            || r.coordinator.recovery != expected
                        {
                            return Vec::new();
                        }
                        r.coordinator.recovery = Recovery::Stale {
                            attempts,
                            reason,
                            reported: true,
                        };
                        r.coordinator_lost = true;
                        vec![Op::awaiting_reply(
                            body,
                            &[("Resume", "resume")],
                            WaitReason::CoordinatorLost,
                        )]
                    })
                    .await?;
                if let Recovery::Stale { reported: true, .. } = reported.coordinator.recovery {
                    next.recovery = reported.coordinator.recovery;
                    stale_notice = Some(format!("{} coordinator recovery stopped", run.key));
                }
            }
            let mut ops = Vec::new();
            let mut notices = Vec::new();
            if let Some(notice) = stale_notice {
                notices.push((notice, "Reply `resume` to continue the run.".into()));
            }
            if needs && !next.blocked_reported {
                let (op, notice) = Self::person_needed(d, run, &label, "The coordinator", &next);
                ops.push(op);
                notices.push(notice);
                next.blocked_reported = true;
            }
            let lost = !live.pane_exists && !record.coordinator_lost;
            let ask_to_resume = !live.pane_exists
                && (lost || (record.coordinator_lost && record.awaiting_reply.is_none()));
            // A kind that picks its own session id is looked up once it is
            // needed: when the pane is gone, a resume can continue it.
            if lost
                && next.agent_session.is_empty()
                && let Ok(since) = next.started_at.parse::<Timestamp>()
            {
                let roots = transcript::Roots::from_env(d.ctx.env);
                let (kind, cwd) = (next.kind.clone(), next.cwd.clone());
                let found = tokio::task::spawn_blocking(move || {
                    transcript::session_since(&roots, &kind, &cwd, since)
                })
                .await;
                if let Ok(Some(found)) = found {
                    next.agent_session = found;
                }
            }
            if ask_to_resume {
                let resumable = !next.agent_session.is_empty()
                    && agents::resume_args(&next.kind, &next.agent_session).is_some();
                let how = if resumable {
                    "Reply `resume` to start it again with its previous session."
                } else {
                    "Reply `resume` to start a new coordinator."
                };
                ops.push(Op::awaiting_reply(
                    format!("The coordinator's pane for {} is gone. {how}", run.key),
                    &[("Resume", "resume")],
                    WaitReason::CoordinatorLost,
                ));
                notices.push((format!("{} coordinator is gone", run.key), how.to_string()));
            } else if live.pane_exists {
                let state = if needs {
                    "needs you"
                } else {
                    live.agent_state.map_or("starting", |s| s.as_str())
                };
                let display = format!("{} \u{b7} coordinator", run.key);
                self.report_pane(d, snapshot, &next.pane_id, &display, state, now)
                    .await;
            }
            let back = live.pane_exists
                && live.agent.is_some()
                && record.coordinator_lost
                && !matches!(next.recovery, Recovery::Stale { .. });
            if next != *c || lost || ask_to_resume || back {
                let before = c.clone();
                let recovery_stale = matches!(&next.recovery, Recovery::Stale { .. });
                self.update_and_push(run, move |r| {
                    apply_tracked(&mut r.coordinator, &before, &next);
                    r.coordinator_lost = (r.coordinator_lost || lost) && !back;
                    if recovery_stale {
                        r.coordinator_lost = true;
                    }
                    if back
                        && r.awaiting_reply
                            .as_ref()
                            .is_some_and(|wait| wait.reason == WaitReason::CoordinatorLost)
                        && let Some(wait) = r.awaiting_reply.take()
                    {
                        r.cleared_wait_id = wait.activity_id;
                    }
                    ops
                })
                .await?;
            }
            for (title, body) in notices {
                d.notify(&title, &body).await;
            }
        }
        for mut w in worker::list(run)
            .into_iter()
            .filter(|w| matches!(w.agent.status, AgentStatus::Open | AgentStatus::Failed))
        {
            // A worker between two panes is neither gone nor anywhere yet.
            if w.agent.status == AgentStatus::Open && w.agent.pane_id.is_empty() {
                continue;
            }
            if w.restarting {
                if !self.restart_landed(snapshot, &w.agent.pane_id, now) {
                    continue;
                }
                let pane = w.agent.pane_id.clone();
                w = update_worker(run, &w.id, move |r| {
                    if r.agent.pane_id == pane {
                        r.restarting = false;
                    }
                })
                .await?;
                if w.restarting {
                    continue;
                }
            }
            if self.judged(d, snapshot, &w.agent, now) {
                self.watch_worker(d, snapshot, run, &label, &w, now).await?;
            }
        }
        Ok(())
    }

    /// Whether this pass may watch the agent: always with a whole snapshot;
    /// with a partly parsed one only when our agent is found in it, since
    /// its absence may be an entry that did not parse.
    fn judged<H>(
        &self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        record: &AgentRecord,
        now: Timestamp,
    ) -> bool {
        self.trusted
            || worker::live_state(record, snapshot, now, &d.ctx.state_dir(), d.socket)
                .agent
                .is_some()
    }

    /// Whether a restarted worker's new pane is in a snapshot taken after
    /// the restart, or has been missing longer than the pane grace.
    fn restart_landed(&mut self, snapshot: &Snapshot, pane: &str, now: Timestamp) -> bool {
        if snapshot.panes.contains_key(&PaneId(pane.to_string())) {
            return true;
        }
        if !self.trusted {
            return false;
        }
        let first = *self.missing_since.entry(pane.to_string()).or_insert(now);
        now.duration_since(first) >= PANE_GRACE
    }

    /// The Linear writes of a worker are queued in the critical section
    /// that stores the fields guarding them; inbox items follow.
    async fn watch_worker<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        label: &str,
        w: &Worker,
        now: Timestamp,
    ) -> Result<()> {
        let run_record = run.record()?;
        let (tracked, live) = self
            .track(
                d,
                snapshot,
                run,
                &super::reconcile::AgentKey::worker(&run.key, &w.id),
                &w.agent,
                now,
            )
            .await;
        let mut next = w.clone();
        next.agent = tracked;
        let group_before = worker::group(w, &live);
        next.agent.recovery = self
            .recovery_state(
                d,
                &next.agent,
                &live,
                !w.restarting
                    && w.report_hash.is_empty()
                    && worker::report_hash(w).is_none()
                    && group_before != Group::WaitingOnYou
                    && live.self_report.as_ref().is_none_or(|r| !r.waiting()),
                now,
            )
            .await;
        if run_record.stopped
            || run_record.finished
            || run_record.status != crate::run::Status::Active
        {
            next.agent.recovery = Recovery::None;
        }
        let mut notices = Vec::new();
        if let Recovery::Stale {
            attempts,
            reason,
            reported: false,
        } = next.agent.recovery.clone()
        {
            let id = w.id.clone();
            let expected = next.agent.recovery.clone();
            let summary = format!(
                "Automatic resume stopped for worker {} after {attempts} attempts. {reason}. Restart it manually to continue.",
                w.id
            );
            let inbox_summary = summary.clone();
            let report_reason = reason.clone();
            let (state_dir, socket) = (d.ctx.state_dir(), d.socket.to_string());
            let applied = self
                .guarded(run, move |run, lock| {
                    let record = run.record()?;
                    if record.status != crate::run::Status::Active
                        || record.stopped
                        || record.finished
                        || record.awaiting_reply.is_some()
                    {
                        return Ok((false, Vec::new()));
                    }
                    let current = worker::load(run, &id)?;
                    if current.agent.recovery != expected
                        || current.restarting
                        || !current.report_hash.is_empty()
                        || worker::report_hash(&current).is_some()
                        || crate::progress::load(&state_dir, &socket, &current.agent.pane_id)
                            .is_some_and(|r| r.waiting())
                    {
                        return Ok((false, Vec::new()));
                    }
                    worker::update_held(run, lock, &id, |w| {
                        w.agent.recovery = Recovery::Stale {
                            attempts,
                            reason: report_reason,
                            reported: true,
                        }
                    })?;
                    inbox::write_held(run, lock, "worker", &id, &inbox_summary)?;
                    Ok((true, vec![error_activity(summary)]))
                })
                .await?;
            if applied {
                next.agent.recovery = Recovery::Stale {
                    attempts,
                    reason,
                    reported: true,
                };
                notices.push((
                    format!("{} worker {} recovery stopped", run.key, w.id),
                    "See the coordinator inbox for the manual restart instruction.".into(),
                ));
            }
        }
        let mut pull_request = None;
        let mut ops = Vec::new();

        // A report written in this pass counts for the group at once.
        if let Some(hash) = worker::report_hash(w).filter(|h| *h != w.report_hash) {
            worker::copy_report_home(run, w)?;
            next.report_hash = hash;
            let report =
                std::fs::read_to_string(worker::home_report_path(run, &w.id)).unwrap_or_default();
            if let Some(url) = worker::pr_line(&report).filter(|url| *url != w.pr_url) {
                ops.push(Op::activity(Activity::new(Content::Action {
                    action: "Pull request".into(),
                    parameter: format!("{url} (worker {}, repo {})", w.id, w.repo),
                    result: None,
                })));
                pull_request = Some(ExternalUrl {
                    label: format!("{} {} PR", w.id, w.repo),
                    url: url.clone(),
                });
                next.pr_url = url;
            }
        }
        if worker::needs_person(&next.agent, &live) && !next.agent.blocked_reported {
            let who = format!("Worker {} ({})", w.id, w.repo);
            let (op, notice) = Self::person_needed(d, run, label, &who, &next.agent);
            ops.push(op);
            notices.push(notice);
            next.agent.blocked_reported = true;
        }
        if w.agent.status == AgentStatus::Open
            && !live.pane_exists
            && next.report_hash.is_empty()
            && !w.gone_reported
        {
            ops.push(error_activity(format!(
                "Worker {} ({}) lost its pane before it wrote a report.",
                w.id, w.repo
            )));
            next.gone_reported = true;
        }
        // What the worker says it is doing, while it does it: a new activity
        // replaces the last one in the session.
        if let Some(report) = live
            .self_report
            .as_ref()
            .filter(|r| !r.waiting() && !r.activity.trim().is_empty())
            && report.activity != w.activity
        {
            let mut activity = Activity::new(Content::Thought {
                body: format!("{} ({}): {}", w.id, w.repo, report.activity),
            });
            activity.ephemeral = true;
            ops.push(Op::activity(activity));
            next.activity = report.activity.clone();
            next.activity_since = now.to_string();
        }
        let guarded = next.clone();
        if guarded != *w {
            let before = w.clone();
            let id = w.id.clone();
            self.guarded(run, move |run, lock| {
                worker::update_held(run, lock, &id, |r| {
                    apply_tracked(&mut r.agent, &before.agent, &guarded.agent);
                    macro_rules! changed {
                        ($($field:ident).+) => {
                            if before.$($field).+ != guarded.$($field).+ {
                                r.$($field).+ = guarded.$($field).+.clone();
                            }
                        };
                    }
                    changed!(report_hash);
                    changed!(pr_url);
                    changed!(gone_reported);
                    changed!(activity);
                    changed!(activity_since);
                })?;
                if let Some(added) = pull_request {
                    let urls = run
                        .update_held(lock, |r| {
                            if !r.external_urls.iter().any(|u| u.url == added.url) {
                                r.external_urls.push(added);
                            }
                        })?
                        .external_urls;
                    // The action first, then the list that names it.
                    ops.insert(1, Op::ExternalUrls { urls });
                }
                Ok(((), ops))
            })
            .await?;
        }
        for (title, body) in notices {
            d.notify(&title, &body).await;
        }
        let group = worker::group(&next, &live);
        let mut milestones = Vec::new();
        if group.token() != w.agent.last_group {
            let summary = match group {
                Group::WaitingOnYou => {
                    let reason = waiting_reason(&next, &live);
                    // A dialog already asked for a person in the session.
                    if !worker::needs_person(&next.agent, &live) {
                        milestones.push(Op::activity(Activity::new(Content::Action {
                            action: "Worker waiting".into(),
                            parameter: format!("{} ({}): {reason}", w.id, w.repo),
                            result: None,
                        })));
                    }
                    Some(format!(
                        "{} ({}) is Waiting on you: {reason}.",
                        w.id, w.repo
                    ))
                }
                Group::Idle => Some(format!(
                    "{} ({}) is idle without a report; check its pane {}.",
                    w.id, w.repo, next.agent.pane_id
                )),
                Group::Reported | Group::Working => None,
            };
            if let Some(summary) = summary {
                inbox_item(run, "worker", &w.id, summary).await?;
            }
            next.agent.last_group = group.token().to_string();
        }
        if group == Group::Reported && next.report_hash != w.announced_report_hash {
            inbox_item(
                run,
                "worker",
                &w.id,
                format!(
                    "{} ({}) has a new report: workers/{}.md",
                    w.id, w.repo, w.id
                ),
            )
            .await?;
            let report =
                std::fs::read_to_string(worker::home_report_path(run, &w.id)).unwrap_or_default();
            // A thought, since Linear renders its body as Markdown and an
            // action's result as code.
            let section = worker::report_section(&report);
            let body = if section.is_empty() {
                format!("{} ({}) wrote a report.", w.id, w.repo)
            } else {
                format!("{} ({}) reported:\n\n{section}", w.id, w.repo)
            };
            milestones.push(Op::activity(Activity::new(Content::Thought { body })));
            next.announced_report_hash = next.report_hash.clone();
        }
        if live.pane_exists {
            let display = format!("{} \u{b7} {} {}", run.key, w.id, w.title);
            self.report_pane(
                d,
                snapshot,
                &next.agent.pane_id,
                &display,
                group.label(),
                now,
            )
            .await;
        }
        let before = w.clone();
        if next.agent.last_group != before.agent.last_group
            || next.announced_report_hash != before.announced_report_hash
        {
            let id = w.id.clone();
            self.guarded(run, move |run, lock| {
                worker::update_held(run, lock, &id, |r| {
                    if before.agent.last_group != next.agent.last_group {
                        r.agent.last_group = next.agent.last_group.clone();
                    }
                    if before.announced_report_hash != next.announced_report_hash {
                        r.announced_report_hash = next.announced_report_hash.clone();
                    }
                })?;
                Ok(((), milestones))
            })
            .await?;
        }
        Ok(())
    }

    async fn recovery_state<H: Herdr>(
        &self,
        d: &Deps<'_, H>,
        agent: &AgentRecord,
        live: &Live,
        eligible: bool,
        now: Timestamp,
    ) -> Recovery {
        if !self.trusted
            || !eligible
            || agent.status != AgentStatus::Open
            || agent.prompt_pending
            || agent.prompted_at.is_empty()
            || !live.pane_exists
            || live.agent.is_some()
        {
            if live.agent.is_some()
                && let Recovery::Suspected {
                    session, attempts, ..
                } = &agent.recovery
            {
                return Recovery::Recovered {
                    attempts: *attempts,
                    session: if session.is_empty() {
                        agent.agent_session.clone()
                    } else {
                        session.clone()
                    },
                };
            }
            return agent.recovery.clone();
        }
        let (since, session, attempts) = match &agent.recovery {
            Recovery::None => (now, agent.agent_session.clone(), 0),
            Recovery::Recovered { attempts, session } => (now, session.clone(), *attempts),
            Recovery::Suspected {
                since,
                session,
                attempts,
            } => (since.parse().unwrap_or(now), session.clone(), *attempts),
            Recovery::Starting {
                attempt,
                session,
                since,
                ..
            } => {
                let at = since.parse::<Timestamp>().unwrap_or(now);
                if now.duration_since(at) < super::reconcile::DETECTION_GRACE {
                    return agent.recovery.clone();
                }
                if *attempt >= 3 {
                    return Recovery::Stale {
                        attempts: *attempt,
                        reason: "resume did not appear in Herdr".into(),
                        reported: false,
                    };
                }
                return Recovery::RetryWait {
                    attempt: *attempt + 1,
                    due_at: (now
                        + jiff::SignedDuration::from_secs(if *attempt == 1 { 30 } else { 60 }))
                    .to_string(),
                    session: session.clone(),
                };
            }
            _ => return agent.recovery.clone(),
        };
        let confirmed = !matches!(agent.recovery, Recovery::None | Recovery::Recovered { .. })
            && now.duration_since(since) >= PANE_GRACE;
        if !confirmed {
            return Recovery::Suspected {
                since: since.to_string(),
                session,
                attempts,
            };
        }
        let found = if !session.is_empty() {
            Some(session)
        } else if let Ok(at) = agent.started_at.parse::<Timestamp>() {
            let roots = transcript::Roots::from_env(d.ctx.env);
            let (kind, cwd) = (agent.kind.clone(), agent.cwd.clone());
            tokio::task::spawn_blocking(move || transcript::session_since(&roots, &kind, &cwd, at))
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let Some(session) = found.filter(|s| agents::resume_args(&agent.kind, s).is_some()) else {
            return Recovery::Stale {
                attempts,
                reason: "native session could not be found".into(),
                reported: false,
            };
        };
        if attempts >= 3 {
            return Recovery::Stale {
                attempts,
                reason: "automatic resume limit reached".into(),
                reported: false,
            };
        }
        let attempt = attempts + 1;
        Recovery::RetryWait {
            attempt,
            due_at: (now
                + jiff::SignedDuration::from_secs(match attempt {
                    1 => 15,
                    2 => 30,
                    _ => 60,
                }))
            .to_string(),
            session,
        }
    }

    /// An ephemeral thought after 10 minutes without an activity says what
    /// each open worker is doing, and for how long; past `ask_to_continue_after_hours` a person is asked
    /// whether to go on. Neither goes out without a session or while stopped.
    pub(super) async fn heartbeat<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snap: Option<&Snapshot>,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        if record.session_id.is_empty()
            || record.stopped
            || record.finished
            || record.awaiting_reply.is_some()
        {
            return Ok(());
        }
        let hours = d.config.limits.ask_to_continue_after_hours;
        let limit = i64::try_from(hours.saturating_mul(3600)).unwrap_or(i64::MAX);
        let timeout_due = files::seconds_since(&record.timeout_since, now) >= limit;
        let quiet = since(&record.last_activity, now).is_some_and(|d| d >= HEARTBEAT);
        // A heartbeat already flushed whose `ActivitySent` has not arrived
        // must not be followed by a second one.
        let recent = self
            .heartbeats
            .get(&run.key)
            .is_some_and(|at| now.duration_since(*at) < HEARTBEAT);
        if !timeout_due && quiet && !recent && outbox::is_empty(run) {
            let mut parts = Vec::new();
            for w in worker::list(run)
                .iter()
                .filter(|w| w.agent.status == AgentStatus::Open)
            {
                let group = match snap {
                    Some(snapshot) => {
                        Some(worker::group(w, &self.live(d, snapshot, &w.agent, now)))
                    }
                    None => Group::from_token(&w.agent.last_group),
                };
                let Some(group) = group else { continue };
                let doing = match since(&w.activity_since, now) {
                    Some(lasted) if group == Group::Working && !w.activity.is_empty() => {
                        format!("{}, for {} min", w.activity, (lasted.as_secs() / 60).max(1))
                    }
                    _ => group.label().to_lowercase(),
                };
                parts.push(format!("{} ({}): {doing}", w.id, w.repo));
            }
            let body = if parts.is_empty() {
                "Still on it: no workers.".to_string()
            } else {
                format!("Still on it. {}.", parts.join("; "))
            };
            let mut activity = Activity::new(Content::Thought { body });
            activity.ephemeral = true;
            if self.push(run, Op::activity(activity)).await? {
                self.heartbeats.insert(run.key.clone(), now);
            }
        }
        if timeout_due {
            self.update_and_push(run, move |r| {
                    if r.awaiting_reply.is_some() || r.stopped || r.finished {
                        return Vec::new();
                    }
                    vec![Op::awaiting_reply(
                        format!(
                            "This run has been going for {hours} hours. Reply to let it continue; until then the coordinator gets no prompts."
                        ),
                        &[("Continue", "continue")],
                        WaitReason::RunTimeout,
                    )]
                })
                .await?;
            d.notify(
                &format!("{} ran {hours} hours", run.key),
                "Reply in the Linear session to let it continue.",
            )
            .await;
        }
        Ok(())
    }

    /// Prompts an idle coordinator once per set of unseen inbox items.
    pub(super) async fn nudge<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        let c = &record.coordinator;
        if c.status != AgentStatus::Open
            || self.prompted.contains(&run.key)
            || record.coordinator_lost
            || c.prompt_pending
            || record.stopped
            || record.awaiting_reply.is_some()
            || record.finished
        {
            return Ok(());
        }
        let Some(agent) = worker::find_agent(c, &snapshot.agents) else {
            return Ok(());
        };
        let idle_for = since(&c.last_state_change, now).unwrap_or_default();
        if !agent.status.is_idle()
            || c.last_state != agent.status.as_str()
            || idle_for < COORDINATOR_IDLE
        {
            return Ok(());
        }
        let seen = inbox::seen(run);
        let unseen: Vec<inbox::Item> = inbox::unhandled(run)
            .into_iter()
            .filter(|item| !seen.contains(&item.id))
            .collect();
        if unseen.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = unseen.iter().map(|i| i.id.as_str()).collect();
        let hash = files::sha256_hex(ids.join("\n").as_bytes());
        if self.nudged.get(&run.key) == Some(&hash) {
            return Ok(());
        }
        let text = if unseen.iter().any(|i| i.kind == "reply") {
            coordinator::NUDGE_REPLY
        } else {
            coordinator::NUDGE_INBOX
        };
        if super::launch::delivered(d.herdr.agent_prompt(&agent.pane, text).await).is_ok() {
            self.nudged.insert(run.key.clone(), hash);
        }
        Ok(())
    }
}
