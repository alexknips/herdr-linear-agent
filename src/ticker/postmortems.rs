//! Postmortems of the runs whose record marks one due: each is written off
//! the pass, and its comment and labels are queued when it answers.

use anyhow::{Result, anyhow};
use jiff::Timestamp;

use super::reconcile::{Deps, Reconciler, update_run};
use crate::config::Postmortem;
use crate::herdr::Herdr;
use crate::outbox::Op;
use crate::postmortem::{self, Outcome, Stage};
use crate::run::Run;
use crate::transcript::Roots;

/// A postmortem that answered: its method's name, the method, the outcome.
pub(super) type Written = Result<(String, Postmortem, Outcome)>;

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
            let method = d
                .config
                .team(&record.workspace, &record.team_key)
                .ok()
                .and_then(|team| team.postmortem.clone())
                .and_then(|name| Some((d.config.postmortems.get(&name)?.clone(), name)));
            let profile = method
                .as_ref()
                .and_then(|(method, _)| d.config.profile(&method.profile).ok().cloned());
            let (Some((method, name)), Some(profile)) = (method, profile) else {
                // The team writes no postmortem.
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
                let input = tokio::task::spawn_blocking(move || {
                    postmortem::input(&roots, &run, &record, stage)
                })
                .await?;
                let outcome =
                    postmortem::write(&profile, &method, &input, path.as_deref(), &parent).await?;
                Ok((name, method, outcome))
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
        let (name, method, outcome) = match written {
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
        self.guarded(run, move |run, lock| {
            postmortem::keep(run, stage, &name, &method, &outcome, &at)?;
            run.update_held(lock, clear)?;
            let mut ops = vec![Op::Comment {
                body: postmortem::comment(stage, &name, &method, &outcome.summary),
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
            "{}: wrote the {} postmortem",
            run.key,
            stage.word()
        ));
        Ok(())
    }
}
