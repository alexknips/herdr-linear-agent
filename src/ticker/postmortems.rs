//! Postmortems of the runs whose record marks one due: each is written off
//! the pass, and its comment and labels are queued when it answers.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use jiff::Timestamp;

use super::reconcile::{Deps, Reconciler, update_run};
use crate::config::{Config, Profile};
use crate::herdr::Herdr;
use crate::outbox::Op;
use crate::postmortem::{self, Method, Outcome, Stage};
use crate::routing::{self, Choice};
use crate::run::{Run, RunRecord};
use crate::transcript::Roots;

/// A postmortem that answered: its method, how its profile was picked, and
/// the outcome.
pub(super) type Written = Result<(Method, String, Outcome)>;

/// What writing a run's postmortem needs, taken from the config when it
/// starts: the routing's candidates with their profiles, its agent with its
/// timeout, and the postmortem agents' default timeout.
struct Plan {
    candidates: Vec<routing::Candidate>,
    profiles: BTreeMap<String, Profile>,
    agent: Option<(Profile, Duration)>,
    postmortem_timeout: u64,
}

/// The run's routing's postmortem plan; none when it lists no profile.
fn plan(config: &Config, record: &RunRecord) -> Option<Plan> {
    let routing = config
        .routing_of(&record.workspace, &record.team_key)
        .ok()?;
    let profiles: BTreeMap<String, Profile> = routing
        .postmortems
        .iter()
        .map(|name| Some((name.clone(), config.profile(name).ok()?.clone())))
        .collect::<Option<_>>()?;
    if profiles.is_empty() {
        return None;
    }
    let agent = routing.agent.as_deref().and_then(|name| {
        let agent = config.profile(name).ok()?.clone();
        let seconds = agent
            .timeout_seconds
            .unwrap_or(config.limits.routing_agent_timeout_seconds);
        Some((agent, Duration::from_secs(seconds)))
    });
    Some(Plan {
        candidates: routing::candidates(config, &routing.postmortems),
        profiles,
        agent,
        postmortem_timeout: config.limits.postmortem_agent_timeout_seconds,
    })
}

/// Picks the profile, with the routing agent when there are several, then
/// has it write the postmortem.
async fn write(
    plan: Plan,
    brief: String,
    input: String,
    path: Option<String>,
    parent: std::path::PathBuf,
) -> Written {
    let choice = match (&plan.agent, &plan.candidates[..]) {
        (Some((agent, timeout)), [_, _, ..]) => {
            routing::choose(
                agent,
                routing::Pick::Postmortem,
                &plan.candidates,
                &brief,
                *timeout,
                path.as_deref(),
                &parent,
            )
            .await
        }
        _ => Choice::Only(plan.candidates[0].name.clone()),
    };
    let name = choice.profile();
    let profile = &plan.profiles[name];
    let timeout = profile.timeout_seconds.unwrap_or(plan.postmortem_timeout);
    let method = Method::new(name, profile, timeout);
    let outcome = postmortem::write(&method, &input, path.as_deref(), &parent).await?;
    Ok((method, choice.source(), outcome))
}

impl Reconciler {
    /// Posts the postmortems that answered and starts the ones due.
    pub(super) async fn postmortems<H: Herdr>(&mut self, d: &Deps<'_, H>, now: Timestamp) {
        let done: Vec<String> = self
            .writing
            .iter()
            .filter(|(_, (_, handle))| handle.is_finished())
            .map(|(key, _)| key.clone())
            .collect();
        for key in done {
            let Some((stage, handle)) = self.writing.remove(&key) else {
                continue;
            };
            let written = handle
                .await
                .unwrap_or_else(|e| Err(anyhow!("the postmortem task failed: {e}")));
            let Ok(run) = Run::load(&d.ctx.runs_dir(), &key) else {
                continue;
            };
            if let Err(error) = self.post(d, &run, stage, written, now).await {
                d.fail(&key, &error);
            }
        }
        for run in Run::list(&d.ctx.runs_dir()) {
            let Ok(record) = run.record() else {
                continue;
            };
            let Some(stage) = record.postmortem_due else {
                continue;
            };
            if self.writing.contains_key(&run.key) {
                continue;
            }
            let Some(plan) = plan(d.config, &record) else {
                // The routing writes no postmortem.
                let cleared = update_run(&run, move |r| {
                    if r.postmortem_due == Some(stage) {
                        r.postmortem_due = None;
                    }
                });
                if let Err(error) = cleared.await {
                    d.fail(&run.key, &error);
                }
                continue;
            };
            let roots = Roots::from_env(d.ctx.env);
            let path = d.ctx.env.var("PATH").map(str::to_string);
            let parent = std::env::temp_dir();
            let key = run.key.clone();
            let task = tokio::spawn(async move {
                let (brief, input) = tokio::task::spawn_blocking(move || {
                    let brief = postmortem::brief(&run, &record, stage);
                    let input = postmortem::input(&roots, &run, &record, &brief);
                    (brief, input)
                })
                .await?;
                write(plan, brief, input, path, parent).await
            });
            self.writing.insert(key, (stage, task));
        }
    }

    /// Keeps the outcome and queues its comment and labels, or logs why there
    /// is none; either way the postmortem of `stage` is no longer due.
    async fn post<H>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        stage: Stage,
        written: Written,
        now: Timestamp,
    ) -> Result<()> {
        let clear = move |r: &mut crate::run::RunRecord| {
            if r.postmortem_due == Some(stage) {
                r.postmortem_due = None;
            }
        };
        let (method, picked, outcome) = match written {
            Ok(written) => written,
            Err(error) => {
                d.log.line(&format!(
                    "{}: the {} postmortem failed: {error:#}",
                    run.key,
                    stage.word()
                ));
                update_run(run, clear).await?;
                return Ok(());
            }
        };
        let at = now.to_string();
        let told = format!("with the `{}` profile ({picked})", method.name);
        self.guarded(run, move |run, lock| {
            postmortem::keep(run, stage, &method, &picked, &outcome, &at)?;
            run.update_held(lock, clear)?;
            let mut ops = vec![Op::Comment {
                body: postmortem::comment(stage, &method, &outcome.summary),
            }];
            if !outcome.labels.is_empty() {
                ops.push(Op::Labels {
                    names: outcome.labels.clone(),
                });
            }
            Ok(((), ops))
        })
        .await?;
        d.log.line(&format!(
            "{}: wrote the {} postmortem {told}",
            run.key,
            stage.word()
        ));
        Ok(())
    }
}
