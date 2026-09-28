//! The Linear task of the event-driven ticker. It owns the Linear client and
//! only reads Linear and sends the outboxes; the reconciler owns every
//! decision and every run record.
//!
//! Level state (the app user and the delegated list) is published on
//! a `watch`; everything that happened (a run was read, a session was found,
//! an activity was sent, writes started or stopped failing) is a
//! [`LinearEvent`], reported once.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use tokio::sync::{Notify, mpsc, watch};

use super::ApiError;
use super::api::{self, IssueDetail, IssueRef, IssueStatus, RunUpdate};
use super::client::LinearApi;
use super::transport::RateHeaders;
use crate::config;
use crate::outbox;
use crate::run::Run;

/// Runs per batch of the run read until a batch has measured one run's cost.
const FIRST_BATCH: usize = 10;
/// A batch of the run read is kept under this many points, half the
/// per-query limit.
const BATCH_POINTS: u64 = 5_000;
/// The longest pause after a rate limit that gave no reset time.
const MAX_PAUSE: SignedDuration = SignedDuration::from_secs(300);

/// What one read cost the last time it ran.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cost {
    requests: u64,
    points: u64,
}

impl Cost {
    /// Assumed for a read that has not been measured.
    const UNMEASURED: Cost = Cost {
        requests: 1,
        points: 0,
    };

    /// The cost of the responses a read got; `None` when none arrived.
    fn of(headers: &[RateHeaders]) -> Option<Cost> {
        (!headers.is_empty()).then(|| Cost {
            requests: headers.len() as u64,
            points: headers.iter().filter_map(|h| h.cost).sum(),
        })
    }
}

/// The parts of a step, which run in an order that depends on the budget.
#[derive(Clone, Copy)]
enum Did {
    Viewer,
    Intake,
    RunRead,
    Flush,
}

/// The last successful poll of the delegated issues.
#[derive(Debug, Clone, PartialEq)]
pub struct Delegated {
    pub read_at: Timestamp,
    pub issues: Vec<IssueRef>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinearLevel {
    pub app_user: Option<String>,
    pub delegated: Option<Delegated>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinearEvent {
    /// A run's issue state and new prompts, and the issue detail when the
    /// query's `issue_updated_at` was empty or differs from the read.
    RunRead {
        issue_id: String,
        read_started_at: Timestamp,
        update: RunUpdate,
        detail: Option<Box<IssueDetail>>,
    },
    /// The session found (Linear's auto-created one) or opened for a run
    /// whose query has none.
    SessionFound {
        issue_id: String,
        session_id: String,
    },
    /// An activity of the run went out.
    ActivitySent { issue_id: String, at: Timestamp },
    /// The run's outbox is blocked since `since`.
    WritesFailing { issue_id: String, since: Timestamp },
    /// The run's outbox is no longer blocked.
    WritesRecovered { issue_id: String },
}

/// What the reconciler asks the Linear task to read and send for one run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunQuery {
    pub issue_id: String,
    pub run_dir: PathBuf,
    pub session_id: Option<String>,
    /// Prompts created after this RFC 3339 timestamp are read.
    pub prompt_cursor: String,
    /// The issue's `updatedAt` when `issue.md` was last written; empty for a
    /// fresh claim.
    pub issue_updated_at: Option<String>,
}

impl RunQuery {
    /// The run's issue key: the name of its folder.
    fn key(&self) -> String {
        self.run_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// The channels the task talks through.
pub struct Links {
    pub queries: watch::Receiver<Vec<RunQuery>>,
    pub level: watch::Sender<LinearLevel>,
    pub events: mpsc::Sender<LinearEvent>,
    pub wake: Arc<Notify>,
}

pub struct LinearTask {
    teams: Vec<String>,
    review_state: String,
    intake_interval: SignedDuration,
    run_read_interval: SignedDuration,
    /// The intervals in effect: the configured ones, stretched while the
    /// budget is short.
    intake_every: SignedDuration,
    read_every: SignedDuration,
    /// Linear's budget as the latest responses reported it.
    budget: RateHeaders,
    intake_cost: Cost,
    read_cost: Cost,
    /// Points one run adds to a batch of the run read, once measured.
    run_points: Option<u64>,
    /// Fewer than 20% of the requests or of the points remain.
    short: bool,
    /// No read or write goes out before this.
    paused_until: Option<Timestamp>,
    /// Rate limits in a row that gave no reset time.
    pauses: u32,
    /// A request of this step was rate-limited.
    limited: bool,
    level: LinearLevel,
    last_intake: Option<Timestamp>,
    last_read: Option<Timestamp>,
    /// Sessions found or opened for runs whose queries do not show one yet.
    sessions: BTreeMap<String, String>,
    /// Runs whose outbox is blocked, and since when.
    failing: BTreeMap<String, Timestamp>,
    log: Vec<String>,
}

fn seconds(value: u64) -> SignedDuration {
    SignedDuration::from_secs(i64::try_from(value).unwrap_or(i64::MAX))
}

fn due(last: Option<Timestamp>, interval: SignedDuration, now: Timestamp) -> bool {
    last.is_none_or(|at| now.duration_since(at) >= interval)
}

impl LinearTask {
    pub fn new(linear: &config::Linear) -> Self {
        let intake_interval = seconds(linear.intake_interval_seconds);
        let run_read_interval = seconds(linear.run_read_interval_seconds);
        LinearTask {
            teams: linear.teams.clone(),
            review_state: linear.review_state.clone(),
            intake_interval,
            run_read_interval,
            intake_every: intake_interval,
            read_every: run_read_interval,
            budget: RateHeaders::default(),
            intake_cost: Cost::UNMEASURED,
            read_cost: Cost::UNMEASURED,
            run_points: None,
            short: false,
            paused_until: None,
            pauses: 0,
            limited: false,
            level: LinearLevel::default(),
            last_intake: None,
            last_read: None,
            sessions: BTreeMap::new(),
            failing: BTreeMap::new(),
            log: Vec::new(),
        }
    }

    /// Makes both intervals due, so the next `step_into` polls and reads.
    #[cfg(test)]
    pub fn force_due(&mut self) {
        self.last_intake = None;
        self.last_read = None;
    }

    #[cfg(test)]
    pub fn level(&self) -> &LinearLevel {
        &self.level
    }

    /// Log lines written since the last take, oldest first.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    #[cfg(test)]
    pub fn budget(&self) -> &RateHeaders {
        &self.budget
    }

    /// When the next interval is due, or the pause ends.
    pub fn next_due(&self, now: Timestamp) -> Timestamp {
        let intake = self.last_intake.map_or(now, |at| at + self.intake_every);
        let read = self.last_read.map_or(now, |at| at + self.read_every);
        let next = intake.min(read);
        self.paused_until.map_or(next, |until| next.max(until))
    }

    /// One round: the viewer while unknown, the delegated poll and the run
    /// read when their intervals are due, then the flush; while the budget
    /// is short the flush goes first and the run read before the delegated
    /// poll. A rate limit ends the round and pauses every read and write.
    /// Each event goes to `events` as soon as it happened, so the reconciler
    /// sees a run's flush as soon as it finishes. It neither sleeps nor reads
    /// the clock.
    pub async fn step_into(
        &mut self,
        client: &impl LinearApi,
        queries: &[RunQuery],
        now: Timestamp,
        events: &mpsc::Sender<LinearEvent>,
    ) {
        self.sessions.retain(|issue_id, _| {
            queries
                .iter()
                .any(|q| &q.issue_id == issue_id && q.session_id.is_none())
        });
        // A run that left the queries is no longer flushed: its failure ends.
        let (kept, gone): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut self.failing)
            .into_iter()
            .partition(|(issue_id, _)| queries.iter().any(|q| &q.issue_id == issue_id));
        self.failing = kept;
        for issue_id in gone.into_keys() {
            let _ = events.send(LinearEvent::WritesRecovered { issue_id }).await;
        }

        if self.paused_until.is_some_and(|until| now < until) {
            return;
        }
        self.paused_until = None;
        self.limited = false;
        let order = if self.short {
            [Did::Flush, Did::Viewer, Did::RunRead, Did::Intake]
        } else {
            [Did::Viewer, Did::Intake, Did::RunRead, Did::Flush]
        };
        // The viewer is retried on the run-read interval; run reads wait for it.
        let read_due = due(self.last_read, self.read_every, now);
        for op in order {
            if self.limited {
                break;
            }
            match op {
                Did::Viewer if read_due && self.level.app_user.is_none() => {
                    self.last_read = Some(now);
                    let viewer = client.viewer().await;
                    self.observe(client);
                    match viewer {
                        Ok(viewer) => self.level.app_user = Some(viewer.id),
                        Err(error) => {
                            self.hit(&error);
                        }
                    }
                }
                Did::Intake if due(self.last_intake, self.intake_every, now) => {
                    self.last_intake = Some(now);
                    self.intake(client, now).await;
                }
                Did::RunRead if read_due && self.level.app_user.is_some() => {
                    self.last_read = Some(now);
                    self.read_runs(client, queries, now, events).await;
                }
                Did::Flush => self.flush(client, queries, now, events).await,
                _ => {}
            }
        }
        self.observe(client);
        if self.limited {
            self.pause(now);
        } else {
            self.pauses = 0;
        }
        self.retime(now);
    }

    /// Takes the headers of the responses since the last call into the
    /// budget.
    fn observe(&mut self, client: &impl LinearApi) -> Vec<RateHeaders> {
        let headers = client.take_headers();
        for h in &headers {
            self.budget.observe(h);
        }
        headers
    }

    /// Whether `error` is a rate limit, which ends the step.
    fn hit(&mut self, error: &ApiError) -> bool {
        let limited = *error == ApiError::RateLimited;
        self.limited |= limited;
        limited
    }

    async fn intake(&mut self, client: &impl LinearApi, now: Timestamp) {
        let result = client.delegated_issues(&self.teams).await;
        let spent = self.observe(client);
        match result {
            Ok(issues) => {
                self.intake_cost = Cost::of(&spent).unwrap_or(self.intake_cost);
                self.level.delegated = Some(Delegated {
                    read_at: now,
                    issues,
                });
            }
            Err(error) => {
                if !self.hit(&error) {
                    self.log.push(format!("intake: {error}"));
                }
            }
        }
    }

    /// Pauses every read and write until the latest reset known, or else
    /// for the run-read interval doubled on each rate limit in a row, at
    /// most 5 minutes.
    fn pause(&mut self, now: Timestamp) {
        let until = match self.budget.reset().filter(|reset| *reset > now) {
            Some(reset) => {
                self.pauses = 0;
                reset
            }
            None => {
                let wait = self
                    .run_read_interval
                    .checked_mul(1 << self.pauses.min(20))
                    .unwrap_or(MAX_PAUSE)
                    .min(MAX_PAUSE);
                self.pauses += 1;
                now + wait
            }
        };
        self.paused_until = Some(until);
        self.log.push(format!(
            "Linear rate-limited the requests ({}): every read and write waits {:#}, until {:.0}",
            self.budget.summary(),
            until.duration_since(now),
            until
        ));
    }

    /// Stretches the read intervals while fewer than 20% of the requests or
    /// of the points remain, so that the reads until the reset, at their
    /// measured cost, use at most 90% of the remainder; never past that
    /// reset. Logs when the budget becomes short and when it recovers.
    fn retime(&mut self, now: Timestamp) {
        let rate = |cost: fn(&Cost) -> u64| {
            cost(&self.intake_cost) as f64 / self.intake_interval.as_secs_f64()
                + cost(&self.read_cost) as f64 / self.run_read_interval.as_secs_f64()
        };
        let dimensions = [
            (self.budget.requests, rate(|c| c.requests)),
            (self.budget.complexity, rate(|c| c.points)),
        ];
        let mut factor: f64 = 1.0;
        let mut reset: Option<Timestamp> = None;
        for (allowance, per_second) in dimensions {
            let (Some(limit), Some(remaining), Some(at)) =
                (allowance.limit, allowance.remaining, allowance.reset)
            else {
                continue;
            };
            if at <= now || remaining.saturating_mul(5) >= limit {
                continue;
            }
            let usable = (remaining - remaining / 10) as f64;
            let needed = per_second * at.duration_since(now).as_secs_f64();
            factor = factor.max(if usable > 0.0 {
                needed / usable
            } else {
                f64::INFINITY
            });
            reset = reset.max(Some(at));
        }
        let longest = reset.map_or(SignedDuration::MAX, |at| at.duration_since(now));
        let stretch = |interval: SignedDuration| {
            let millis = (interval.as_millis() as f64 * factor).round() as i64;
            SignedDuration::from_millis(millis)
                .min(longest)
                .max(interval)
        };
        self.intake_every = stretch(self.intake_interval);
        self.read_every = stretch(self.run_read_interval);
        let short = reset.is_some();
        if short != self.short {
            self.short = short;
            self.log.push(format!(
                "Linear budget is {} ({}): the intake poll runs every {:#} and the run read every {:#}",
                if short { "short" } else { "no longer short" },
                self.budget.summary(),
                self.intake_every,
                self.read_every
            ));
        }
    }

    /// Runs per batch of the run read: as many as keep the batch under
    /// 5,000 points by the last measurement.
    fn batch_size(&self) -> usize {
        self.run_points.map_or(FIRST_BATCH, |points| {
            usize::try_from((BATCH_POINTS - 1) / points.max(1))
                .unwrap_or(usize::MAX)
                .max(1)
        })
    }

    /// Takes a measured cost per run, and logs it when it changed.
    fn measure_run(&mut self, points: u64) {
        if self.run_points == Some(points) {
            return;
        }
        self.run_points = Some(points);
        self.log.push(format!(
            "Linear run read costs {points} points per run; up to {} runs per query",
            self.batch_size()
        ));
    }

    fn session_of(&self, query: &RunQuery) -> Option<String> {
        query
            .session_id
            .clone()
            .or_else(|| self.sessions.get(&query.issue_id).cloned())
    }

    async fn read_runs(
        &mut self,
        client: &impl LinearApi,
        queries: &[RunQuery],
        now: Timestamp,
        events: &mpsc::Sender<LinearEvent>,
    ) {
        let (with_session, without): (Vec<_>, Vec<_>) = queries
            .iter()
            .map(|query| (query, self.session_of(query)))
            .partition(|(_, session)| session.is_some());
        let batch: Vec<api::RunQuery> = with_session
            .iter()
            .map(|(query, session)| api::RunQuery {
                issue_id: query.issue_id.clone(),
                session_id: session.clone().unwrap_or_default(),
                cursor: query.prompt_cursor.clone(),
            })
            .collect();
        let mut spent = Vec::new();
        let mut updates = Vec::with_capacity(batch.len());
        let mut rest = &batch[..];
        while !rest.is_empty() && !self.limited {
            let (part, later) = rest.split_at(self.batch_size().min(rest.len()));
            rest = later;
            let results = client.read_runs(part).await;
            let headers = self.observe(client);
            // Only a batch answered by one request measured its runs.
            if let [one] = headers[..]
                && let Some(points) = one.cost
                && results.iter().all(Result::is_ok)
            {
                self.measure_run(points.div_ceil(part.len() as u64));
            }
            spent.extend(headers);
            for result in &results {
                if let Err(error) = result {
                    self.hit(error);
                }
            }
            updates.extend(results);
        }
        for ((query, _), update) in with_session.iter().zip(updates) {
            let update = match update {
                Ok(update) => update,
                Err(ApiError::RateLimited) => continue,
                Err(error) => {
                    self.log.push(format!("{}: {error}", query.key()));
                    continue;
                }
            };
            let stale = query
                .issue_updated_at
                .as_deref()
                .is_none_or(|at| at.is_empty() || at != update.issue.updated_at);
            let detail = if stale && !self.limited {
                let detail = self.detail(client, query).await;
                spent.extend(self.observe(client));
                detail
            } else {
                None
            };
            let _ = events
                .send(LinearEvent::RunRead {
                    issue_id: query.issue_id.clone(),
                    read_started_at: now,
                    update,
                    detail,
                })
                .await;
        }
        // A fresh claim without a session still needs the detail for
        // `issue.md` and routing; its state comes from the detail.
        for (query, _) in without {
            if self.limited || !query.issue_updated_at.as_deref().unwrap_or("").is_empty() {
                continue;
            }
            let detail = self.detail(client, query).await;
            spent.extend(self.observe(client));
            let Some(detail) = detail else {
                continue;
            };
            let update = RunUpdate {
                issue: IssueStatus {
                    updated_at: detail.updated_at.clone(),
                    state_type: detail.state.r#type.clone(),
                    state_name: detail.state.name.clone(),
                    delegate_id: detail.delegate_id.clone(),
                },
                prompts: Vec::new(),
            };
            let _ = events
                .send(LinearEvent::RunRead {
                    issue_id: query.issue_id.clone(),
                    read_started_at: now,
                    update,
                    detail: Some(detail),
                })
                .await;
        }
        if !self.limited {
            self.read_cost = Cost::of(&spent).unwrap_or(self.read_cost);
        }
    }

    async fn detail(
        &mut self,
        client: &impl LinearApi,
        query: &RunQuery,
    ) -> Option<Box<IssueDetail>> {
        match client.issue(&query.issue_id).await {
            Ok(detail) => Some(Box::new(detail)),
            Err(error) => {
                if !self.hit(&error) {
                    self.log.push(format!("{}: {error}", query.key()));
                }
                None
            }
        }
    }

    /// Sends every run's queued requests in order. A run without a session
    /// gets the one Linear created on delegation, or a new one.
    async fn flush(
        &mut self,
        client: &impl LinearApi,
        queries: &[RunQuery],
        now: Timestamp,
        events: &mpsc::Sender<LinearEvent>,
    ) {
        for query in queries {
            if self.limited {
                break;
            }
            let key = query.key();
            let run = Run {
                dir: query.run_dir.clone(),
                key: key.clone(),
            };
            let pending = !outbox::pending(&run).is_empty();
            let blocked = if !pending {
                false
            } else if let Some(session) = self.session_for_flush(client, query, events).await {
                let sent =
                    outbox::send(&run, &session, &query.issue_id, &self.review_state, client).await;
                if sent.activity_sent {
                    let _ = events
                        .send(LinearEvent::ActivitySent {
                            issue_id: query.issue_id.clone(),
                            at: now,
                        })
                        .await;
                }
                for (request, error) in &sent.refused {
                    self.log.push(format!(
                        "{key}: Linear refused request {}: {error}",
                        request.id
                    ));
                }
                match &sent.blocked {
                    Some(error) => {
                        self.hit(error);
                        self.log
                            .push(format!("{key}: Linear write failed, will retry: {error}"));
                        true
                    }
                    None => false,
                }
            } else {
                true
            };
            match (blocked, self.failing.contains_key(&query.issue_id)) {
                (true, false) => {
                    self.failing.insert(query.issue_id.clone(), now);
                    let _ = events
                        .send(LinearEvent::WritesFailing {
                            issue_id: query.issue_id.clone(),
                            since: now,
                        })
                        .await;
                }
                (false, true) => {
                    self.failing.remove(&query.issue_id);
                    let _ = events
                        .send(LinearEvent::WritesRecovered {
                            issue_id: query.issue_id.clone(),
                        })
                        .await;
                }
                _ => {}
            }
        }
    }

    async fn session_for_flush(
        &mut self,
        client: &impl LinearApi,
        query: &RunQuery,
        events: &mpsc::Sender<LinearEvent>,
    ) -> Option<String> {
        if let Some(session) = self.session_of(query) {
            return Some(session);
        }
        let found = match client.find_session(&query.issue_id).await {
            Ok(Some(session)) => Ok(session),
            Ok(None) => client.create_session(&query.issue_id).await,
            Err(error) => Err(error),
        };
        match found {
            Ok(session) => {
                self.sessions
                    .insert(query.issue_id.clone(), session.clone());
                let _ = events
                    .send(LinearEvent::SessionFound {
                        issue_id: query.issue_id.clone(),
                        session_id: session.clone(),
                    })
                    .await;
                Some(session)
            }
            Err(error) => {
                if !self.hit(&error) {
                    self.log.push(format!(
                        "{}: could not create the session: {error}",
                        query.key()
                    ));
                }
                None
            }
        }
    }

    /// Runs `step_into` whenever an interval is due or the reconciler calls
    /// `notify_one`, until the event channel closes.
    pub async fn run(
        mut self,
        client: &impl LinearApi,
        links: Links,
        clock: impl Fn() -> Timestamp,
        log: impl Fn(&str),
    ) {
        loop {
            let queries = links.queries.borrow().clone();
            // Events go out as they happen and before the level, so a pass
            // woken by the level has already seen what led to it.
            self.step_into(client, &queries, clock(), &links.events)
                .await;
            for line in self.take_log() {
                log(&line);
            }
            if links.events.is_closed() {
                return;
            }
            // The latest level is always stored, but it wakes the
            // reconciler only when the app user or the delegated issues
            // changed: a new read time alone is no news.
            links.level.send_if_modified(|level| {
                let issues = |l: &LinearLevel| l.delegated.as_ref().map(|d| d.issues.clone());
                let news =
                    level.app_user != self.level.app_user || issues(level) != issues(&self.level);
                *level = self.level.clone();
                news
            });
            let now = clock();
            let wait = std::time::Duration::try_from(self.next_due(now).duration_since(now))
                .unwrap_or_default();
            tokio::select! {
                _ = links.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::ApiError;
    use crate::linear::api::fake::{self, FakeLinear};
    use crate::linear::api::{Activity, Content};
    use crate::linear::transport::Allowance;
    use crate::outbox::Op;
    use crate::run::RunRecord;
    use std::sync::Mutex;

    const T0: &str = "2026-09-28T00:00:00Z";

    fn at(seconds: i64) -> Timestamp {
        T0.parse::<Timestamp>().unwrap() + SignedDuration::from_secs(seconds)
    }

    /// One `step_into`, and the events it sent.
    async fn step(
        task: &mut LinearTask,
        linear: &impl LinearApi,
        queries: &[RunQuery],
        now: Timestamp,
    ) -> Vec<LinearEvent> {
        let (events, mut sent) = mpsc::channel(64);
        task.step_into(linear, queries, now, &events).await;
        std::iter::from_fn(|| sent.try_recv().ok()).collect()
    }

    /// The headers of an account with `requests` of 5,000 requests and
    /// `points` of 2,000,000 points left until `reset`. The delegated poll
    /// costs 100 points, each run of a run-read batch `per_run`, anything
    /// else 1.
    fn account(requests: u64, points: u64, reset: Timestamp, per_run: u64) -> fake::Headers {
        Box::new(move |operation, variables| {
            let allowance = |limit, remaining| Allowance {
                limit: Some(limit),
                remaining: Some(remaining),
                reset: Some(reset),
            };
            RateHeaders {
                requests: allowance(5_000, requests),
                complexity: allowance(2_000_000, points),
                cost: Some(match operation {
                    "HlaDelegatedIssues" => 100,
                    "HlaRuns" => per_run * fake::runs(variables) as u64,
                    _ => 1,
                }),
            }
        })
    }

    fn task() -> LinearTask {
        let config = config::Config::parse(config::tests::SAMPLE).unwrap();
        LinearTask::new(&config.linear)
    }

    fn thought(body: &str) -> Op {
        Op::Activity {
            activity: Activity::new(Content::Thought { body: body.into() }),
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        runs: Vec<Run>,
        issues: Vec<String>,
        linear: Mutex<FakeLinear>,
    }

    /// One run per key, each with an issue in the fake and, when `sessions`,
    /// Linear's auto-created session.
    fn setup(keys: &[&str], sessions: bool) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = FakeLinear::default();
        let mut runs = Vec::new();
        let mut issues = Vec::new();
        for key in keys {
            issues.push(fake.add_issue(key, "DATA", "Title"));
            if sessions {
                fake.delegate_session(key);
            }
            let record = RunRecord {
                identifier: key.to_string(),
                ..RunRecord::default()
            };
            runs.push(Run::create(dir.path(), record).unwrap());
        }
        Setup {
            _dir: dir,
            runs,
            issues,
            linear: Mutex::new(fake),
        }
    }

    impl Setup {
        fn query(&self, n: usize, session: Option<&str>, updated_at: Option<&str>) -> RunQuery {
            RunQuery {
                issue_id: self.issues[n].clone(),
                run_dir: self.runs[n].dir.clone(),
                session_id: session.map(str::to_string),
                prompt_cursor: "2026-09-24T00:00:00Z".into(),
                issue_updated_at: updated_at.map(str::to_string),
            }
        }

        fn fake(&self) -> std::sync::MutexGuard<'_, FakeLinear> {
            self.linear.lock().unwrap()
        }

        /// Calls of the three read operations: viewer, delegated, runs.
        fn reads(&self) -> (usize, usize, usize) {
            let fake = self.fake();
            (
                fake.count("HlaViewer"),
                fake.count("HlaDelegatedIssues"),
                fake.count("HlaRuns"),
            )
        }

        /// The number of runs in each `HlaRuns` request, oldest first.
        fn batches(&self) -> Vec<usize> {
            self.fake()
                .calls
                .iter()
                .filter(|(name, _, _)| name == "HlaRuns")
                .map(|(_, variables, _)| fake::runs(variables))
                .collect()
        }

        /// The operations called since the first `from` calls, in order.
        fn calls_since(&self, from: usize) -> Vec<String> {
            self.fake().calls[from..]
                .iter()
                .map(|(name, _, _)| name.clone())
                .collect()
        }

        fn failed(&self, n: usize) -> usize {
            self.runs[n]
                .state_dir()
                .join("outbox/failed")
                .read_dir()
                .map_or(0, |entries| entries.count())
        }
    }

    const UPDATED: &str = "2026-09-25T00:00:00.000Z";

    #[tokio::test]
    async fn the_delegated_poll_and_the_run_read_run_on_their_own_intervals() {
        let s = setup(&["DATA-1"], true);
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();

        let events = step(&mut task, &s.linear, &queries, at(0)).await;
        assert_eq!(events.len(), 1);
        assert_eq!(task.level().app_user.as_deref(), Some("app-user-1"));
        let delegated = task.level().delegated.clone().unwrap();
        assert_eq!(delegated.read_at, at(0));
        assert_eq!(delegated.issues[0].identifier, "DATA-1");
        assert_eq!(s.reads(), (1, 1, 1));
        assert_eq!(task.next_due(at(0)), at(5));

        let calls = s.fake().calls.len();
        assert_eq!(step(&mut task, &s.linear, &queries, at(3)).await, []);
        assert_eq!(s.fake().calls.len(), calls, "nothing is read before 5 s");

        let events = step(&mut task, &s.linear, &queries, at(5)).await;
        assert!(matches!(
            &events[..],
            [LinearEvent::RunRead { read_started_at, detail: None, .. }] if *read_started_at == at(5)
        ));
        assert_eq!(task.level().delegated.as_ref().unwrap().read_at, at(5));
        assert_eq!(s.reads(), (1, 2, 2));
    }

    fn detail_of(event: &LinearEvent) -> Option<&str> {
        match event {
            LinearEvent::RunRead { detail, .. } => detail.as_ref().map(|d| d.updated_at.as_str()),
            other => panic!("not a run read: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_issue_detail_is_read_for_a_fresh_claim_and_after_an_edit() {
        let s = setup(&["DATA-1", "DATA-2"], true);
        let mut task = task();
        let queries = [
            s.query(0, Some("session-1"), None),
            s.query(1, Some("session-2"), Some(UPDATED)),
        ];
        let events = step(&mut task, &s.linear, &queries, at(0)).await;
        assert_eq!(events.len(), 2);
        assert_eq!(detail_of(&events[0]), Some(UPDATED));
        assert_eq!(detail_of(&events[1]), None);
        assert_eq!(s.fake().count("HlaIssue"), 1);

        s.fake().issue_mut("DATA-2")["updatedAt"] = "2026-09-27T00:00:00.000Z".into();
        let queries = [
            s.query(0, Some("session-1"), Some(UPDATED)),
            s.query(1, Some("session-2"), Some(UPDATED)),
        ];
        let events = step(&mut task, &s.linear, &queries, at(5)).await;
        assert_eq!(detail_of(&events[0]), None);
        assert_eq!(detail_of(&events[1]), Some("2026-09-27T00:00:00.000Z"));
        assert_eq!(s.fake().count("HlaIssue"), 2);
    }

    #[tokio::test]
    async fn a_rate_limited_write_stays_queued_and_a_refusal_is_set_aside() {
        let s = setup(&["DATA-1"], true);
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();
        step(&mut task, &s.linear, &queries, at(0)).await;

        let refused = outbox::push(&s.runs[0], thought("refused")).unwrap();
        s.fake().fail_next = Some(ApiError::Graphql("Entity not found".into()));
        assert_eq!(step(&mut task, &s.linear, &queries, at(1)).await, []);
        assert!(outbox::pending(&s.runs[0]).is_empty());
        assert_eq!(s.failed(0), 1);
        assert_eq!(
            task.take_log(),
            [format!(
                "DATA-1: Linear refused request {refused}: Linear reported an error: Entity not found"
            )]
        );

        outbox::push(&s.runs[0], thought("limited")).unwrap();
        s.fake().fail_next = Some(ApiError::RateLimited);
        let events = step(&mut task, &s.linear, &queries, at(2)).await;
        assert_eq!(
            events,
            [LinearEvent::WritesFailing {
                issue_id: s.issues[0].clone(),
                since: at(2)
            }]
        );
        let pending = outbox::pending(&s.runs[0]);
        assert_eq!(pending.len(), 1);
        assert!(pending[0].1.attempted);
        assert_eq!(s.failed(0), 1);
        assert_eq!(
            task.take_log(),
            [
                "DATA-1: Linear write failed, will retry: Linear rate-limited the request",
                "Linear rate-limited the requests (the budget is unknown): every read and write waits 5s, until 2026-09-28T00:00:07Z"
            ]
        );
        assert!(s.fake().sessions[0].sent("thought").is_empty());
    }

    #[tokio::test]
    async fn a_lost_response_is_confirmed_by_the_read_back_and_not_sent_twice() {
        let s = setup(&["DATA-1"], true);
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();
        step(&mut task, &s.linear, &queries, at(0)).await;

        outbox::push(&s.runs[0], thought("once")).unwrap();
        s.fake().lose_next_response = true;
        step(&mut task, &s.linear, &queries, at(1)).await;
        assert!(outbox::pending(&s.runs[0])[0].1.attempted);

        let events = step(&mut task, &s.linear, &queries, at(2)).await;
        assert!(outbox::pending(&s.runs[0]).is_empty());
        assert_eq!(s.fake().sessions[0].sent("thought").len(), 1);
        assert_eq!(s.fake().count("HlaActivityCreate"), 1);
        assert_eq!(s.fake().count("HlaActivityFind"), 1);
        assert_eq!(
            events,
            [
                LinearEvent::ActivitySent {
                    issue_id: s.issues[0].clone(),
                    at: at(2)
                },
                LinearEvent::WritesRecovered {
                    issue_id: s.issues[0].clone()
                }
            ]
        );
    }

    #[tokio::test]
    async fn a_run_without_a_session_gets_the_auto_created_one_once() {
        let s = setup(&["DATA-1"], true);
        outbox::push(&s.runs[0], thought("Picked up DATA-1.")).unwrap();
        let queries = [s.query(0, None, None)];
        let mut task = task();

        let events = step(&mut task, &s.linear, &queries, at(0)).await;
        assert_eq!(events.len(), 3);
        match &events[0] {
            LinearEvent::RunRead {
                issue_id,
                read_started_at,
                update,
                detail,
            } => {
                assert_eq!(issue_id, &s.issues[0]);
                assert_eq!(*read_started_at, at(0));
                assert_eq!(update.issue.state_type, "unstarted");
                assert_eq!(update.issue.delegate_id.as_deref(), Some("app-user-1"));
                assert!(update.prompts.is_empty());
                assert_eq!(detail.as_ref().unwrap().identifier, "DATA-1");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            events[1..],
            [
                LinearEvent::SessionFound {
                    issue_id: s.issues[0].clone(),
                    session_id: "session-1".into()
                },
                LinearEvent::ActivitySent {
                    issue_id: s.issues[0].clone(),
                    at: at(0)
                }
            ]
        );
        assert_eq!(
            s.fake().sessions[0].sent("thought")[0]["content"]["body"],
            "Picked up DATA-1."
        );

        // The reconciler has not persisted the session yet: no second one.
        outbox::push(&s.runs[0], thought("second")).unwrap();
        let events = step(&mut task, &s.linear, &queries, at(1)).await;
        assert_eq!(
            events,
            [LinearEvent::ActivitySent {
                issue_id: s.issues[0].clone(),
                at: at(1)
            }]
        );
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        outbox::push(&s.runs[0], thought("third")).unwrap();
        step(&mut task, &s.linear, &queries, at(2)).await;
        assert_eq!(s.fake().sessions.len(), 1);
        assert_eq!(s.fake().count("HlaSessions"), 1);
        assert_eq!(s.fake().count("HlaSessionCreate"), 0);
        assert_eq!(s.fake().sessions[0].sent("thought").len(), 3);
    }

    #[tokio::test]
    async fn write_failures_are_reported_once_and_so_is_the_recovery() {
        let s = setup(&["DATA-1"], true);
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();
        step(&mut task, &s.linear, &queries, at(0)).await;
        outbox::push(&s.runs[0], thought("late")).unwrap();

        s.fake().fail_next = Some(ApiError::HttpStatus(503));
        let events = step(&mut task, &s.linear, &queries, at(1)).await;
        assert_eq!(
            events,
            [LinearEvent::WritesFailing {
                issue_id: s.issues[0].clone(),
                since: at(1)
            }]
        );
        s.fake().fail_next = Some(ApiError::RequestFailed);
        assert_eq!(step(&mut task, &s.linear, &queries, at(2)).await, []);

        let events = step(&mut task, &s.linear, &queries, at(3)).await;
        assert_eq!(
            events,
            [
                LinearEvent::ActivitySent {
                    issue_id: s.issues[0].clone(),
                    at: at(3)
                },
                LinearEvent::WritesRecovered {
                    issue_id: s.issues[0].clone()
                }
            ]
        );
        assert_eq!(step(&mut task, &s.linear, &queries, at(4)).await, []);
        assert_eq!(s.fake().sessions[0].sent("thought").len(), 1);
    }

    #[tokio::test]
    async fn a_failing_run_that_leaves_the_queries_is_reported_recovered() {
        let s = setup(&["DATA-1"], true);
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();
        step(&mut task, &s.linear, &queries, at(0)).await;
        outbox::push(&s.runs[0], thought("late")).unwrap();
        s.fake().fail_next = Some(ApiError::HttpStatus(503));
        step(&mut task, &s.linear, &queries, at(1)).await;
        let events = step(&mut task, &s.linear, &[], at(2)).await;
        assert_eq!(
            events,
            [LinearEvent::WritesRecovered {
                issue_id: s.issues[0].clone()
            }]
        );
        assert_eq!(step(&mut task, &s.linear, &[], at(3)).await, []);
    }

    #[tokio::test]
    async fn a_new_read_time_alone_does_not_wake_the_reconciler() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let s = Arc::new(setup(&["DATA-1"], true));
        let (_queries_tx, queries) =
            watch::channel(vec![s.query(0, Some("session-1"), Some(UPDATED))]);
        let (level, mut level_rx) = watch::channel(LinearLevel::default());
        let (events, mut events_rx) = mpsc::channel(16);
        let wake = Arc::new(Notify::new());
        let links = Links {
            queries,
            level,
            events,
            wake: wake.clone(),
        };
        let clock = Arc::new(AtomicI64::new(0));
        let (shared, time) = (s.clone(), clock.clone());
        let running = tokio::spawn(async move {
            task()
                .run(
                    &shared.linear,
                    links,
                    move || at(time.load(Ordering::SeqCst)),
                    |_| {},
                )
                .await;
        });
        level_rx.changed().await.unwrap();
        assert_eq!(
            level_rx
                .borrow_and_update()
                .delegated
                .as_ref()
                .unwrap()
                .read_at,
            at(0)
        );
        events_rx.recv().await.unwrap();

        clock.store(10, Ordering::SeqCst);
        wake.notify_one();
        events_rx.recv().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!level_rx.has_changed().unwrap(), "only read_at changed");
        assert_eq!(
            level_rx.borrow().delegated.as_ref().unwrap().read_at,
            at(10)
        );

        s.fake().add_issue("DATA-2", "DATA", "Second");
        clock.store(20, Ordering::SeqCst);
        wake.notify_one();
        level_rx.changed().await.unwrap();
        assert_eq!(
            level_rx.borrow().delegated.as_ref().unwrap().issues.len(),
            2
        );
        drop(events_rx);
        wake.notify_one();
        running.await.unwrap();
    }

    #[tokio::test]
    async fn run_publishes_the_level_and_flushes_when_woken() {
        let s = Arc::new(setup(&["DATA-1"], true));
        let (queries_tx, queries) =
            watch::channel(vec![s.query(0, Some("session-1"), Some(UPDATED))]);
        let (level, mut level_rx) = watch::channel(LinearLevel::default());
        let (events, mut events_rx) = mpsc::channel(16);
        let wake = Arc::new(Notify::new());
        let links = Links {
            queries,
            level,
            events,
            wake: wake.clone(),
        };
        let shared = s.clone();
        let running = tokio::spawn(async move {
            // A fixed clock: only `notify_one` wakes the task again.
            task().run(&shared.linear, links, || at(0), |_| {}).await;
        });

        level_rx.changed().await.unwrap();
        assert_eq!(level_rx.borrow().app_user.as_deref(), Some("app-user-1"));
        assert!(matches!(
            events_rx.recv().await,
            Some(LinearEvent::RunRead { .. })
        ));

        outbox::push(&s.runs[0], thought("woken")).unwrap();
        wake.notify_one();
        assert_eq!(
            events_rx.recv().await,
            Some(LinearEvent::ActivitySent {
                issue_id: s.issues[0].clone(),
                at: at(0)
            })
        );
        drop(events_rx);
        drop(queries_tx);
        wake.notify_one();
        running.await.unwrap();
    }

    #[tokio::test]
    async fn reads_keep_their_intervals_with_plenty_of_budget() {
        let s = setup(&["DATA-1"], true);
        s.fake().headers = Some(account(4_900, 1_900_000, at(3600), 10));
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();

        step(&mut task, &s.linear, &queries, at(0)).await;
        assert_eq!(
            s.calls_since(0),
            ["HlaViewer", "HlaDelegatedIssues", "HlaRuns"]
        );
        assert_eq!(task.budget().requests.remaining, Some(4_900));
        assert_eq!(task.budget().complexity.remaining, Some(1_900_000));
        assert_eq!(task.next_due(at(0)), at(5));
        step(&mut task, &s.linear, &queries, at(3)).await;
        assert_eq!(s.calls_since(3), Vec::<String>::new());
        step(&mut task, &s.linear, &queries, at(5)).await;
        assert_eq!(s.calls_since(3), ["HlaDelegatedIssues", "HlaRuns"]);
        assert_eq!(task.next_due(at(5)), at(10));
        assert_eq!(
            task.take_log(),
            ["Linear run read costs 10 points per run; up to 499 runs per query"]
        );
    }

    #[tokio::test]
    async fn at_19_percent_the_reads_are_stretched_and_writes_go_first() {
        let s = setup(&["DATA-1"], true);
        // 380,000 of 2,000,000 points is 19%; 90% of it is 342,000. Normal
        // reads until the reset would cost (100 + 1,800) / 5 s * 1,800 s =
        // 684,000, twice that, so both 5 s intervals become 10 s.
        s.fake().headers = Some(account(4_000, 380_000, at(1800), 1_800));
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();

        step(&mut task, &s.linear, &queries, at(0)).await;
        assert_eq!(task.next_due(at(0)), at(10));
        assert_eq!(
            task.take_log(),
            [
                "Linear run read costs 1800 points per run; up to 2 runs per query",
                "Linear budget is short (4000/5000 requests, 380000/2000000 points, resets 2026-09-28T00:30:00Z): the intake poll runs every 10s and the run read every 10s"
            ]
        );
        let calls = s.fake().calls.len();
        step(&mut task, &s.linear, &queries, at(5)).await;
        assert_eq!(s.calls_since(calls), Vec::<String>::new());

        outbox::push(&s.runs[0], thought("due")).unwrap();
        step(&mut task, &s.linear, &queries, at(10)).await;
        assert_eq!(
            s.calls_since(calls),
            ["HlaActivityCreate", "HlaRuns", "HlaDelegatedIssues"]
        );
        assert_eq!(task.take_log(), Vec::<String>::new(), "no line per read");

        s.fake().headers = Some(account(4_000, 1_900_000, at(1800), 1_800));
        step(&mut task, &s.linear, &queries, at(20)).await;
        assert_eq!(task.next_due(at(20)), at(25));
        assert_eq!(
            task.take_log(),
            [
                "Linear budget is no longer short (4000/5000 requests, 1900000/2000000 points, resets 2026-09-28T00:30:00Z): the intake poll runs every 5s and the run read every 5s"
            ]
        );
    }

    #[tokio::test]
    async fn a_rate_limit_with_a_reset_pauses_reads_and_writes_until_the_reset() {
        let s = setup(&["DATA-1"], true);
        s.fake().headers = Some(account(4_000, 1_900_000, at(60), 10));
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();
        step(&mut task, &s.linear, &queries, at(0)).await;
        task.take_log();

        outbox::push(&s.runs[0], thought("held")).unwrap();
        s.fake().fail_next = Some(ApiError::RateLimited);
        let calls = s.fake().calls.len();
        step(&mut task, &s.linear, &queries, at(5)).await;
        assert_eq!(
            s.calls_since(calls),
            ["HlaDelegatedIssues"],
            "the rate limit ends the step"
        );
        assert_eq!(task.next_due(at(5)), at(60));
        assert_eq!(
            task.take_log(),
            [
                "Linear rate-limited the requests (4000/5000 requests, 1900000/2000000 points, resets 2026-09-28T00:01:00Z): every read and write waits 55s, until 2026-09-28T00:01:00Z"
            ]
        );

        let calls = s.fake().calls.len();
        for t in [10, 30, 59] {
            step(&mut task, &s.linear, &queries, at(t)).await;
        }
        assert_eq!(s.fake().calls.len(), calls, "nothing before the reset");
        assert_eq!(outbox::pending(&s.runs[0]).len(), 1);

        step(&mut task, &s.linear, &queries, at(60)).await;
        assert_eq!(
            s.calls_since(calls),
            ["HlaDelegatedIssues", "HlaRuns", "HlaActivityCreate"]
        );
        assert!(outbox::pending(&s.runs[0]).is_empty());
        assert_eq!(task.next_due(at(60)), at(65));
    }

    #[tokio::test]
    async fn a_rate_limit_without_a_reset_doubles_the_pause_up_to_5_minutes() {
        let s = setup(&["DATA-1"], true);
        let queries = [s.query(0, Some("session-1"), Some(UPDATED))];
        let mut task = task();
        step(&mut task, &s.linear, &queries, at(0)).await;

        let mut now = at(5);
        for wait in [5, 10, 20, 40, 80, 160, 300, 300] {
            s.fake().fail_next = Some(ApiError::RateLimited);
            let calls = s.fake().calls.len();
            step(&mut task, &s.linear, &queries, now).await;
            assert_eq!(s.calls_since(calls), ["HlaDelegatedIssues"]);
            let until = now + SignedDuration::from_secs(wait);
            assert_eq!(task.next_due(now), until, "after waiting {wait}s");
            now = until;
        }
        assert_eq!(
            task.take_log().last().unwrap(),
            "Linear rate-limited the requests (the budget is unknown): every read and write waits 5m, until 2026-09-28T00:15:20Z"
        );

        let calls = s.fake().calls.len();
        step(&mut task, &s.linear, &queries, now).await;
        assert_eq!(s.calls_since(calls), ["HlaDelegatedIssues", "HlaRuns"]);
        let later = now + SignedDuration::from_secs(5);
        s.fake().fail_next = Some(ApiError::RateLimited);
        step(&mut task, &s.linear, &queries, later).await;
        assert_eq!(
            task.next_due(later),
            later + SignedDuration::from_secs(5),
            "a read in between starts over at the interval"
        );
    }

    #[tokio::test]
    async fn the_run_read_is_split_to_stay_under_5000_points() {
        let keys: Vec<String> = (1..=12).map(|n| format!("DATA-{n}")).collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let s = setup(&keys, true);
        s.fake().headers = Some(account(4_900, 1_900_000, at(3600), 1_200));
        let sessions: Vec<String> = (1..=12).map(|n| format!("session-{n}")).collect();
        let queries: Vec<RunQuery> = (0..12)
            .map(|n| s.query(n, Some(&sessions[n]), Some(UPDATED)))
            .collect();
        let mut task = task();

        let events = step(&mut task, &s.linear, &queries, at(0)).await;
        assert_eq!(events.len(), 12);
        assert_eq!(s.batches(), [10, 2], "10 runs before a measurement");
        assert_eq!(
            task.take_log(),
            ["Linear run read costs 1200 points per run; up to 4 runs per query"]
        );

        let events = step(&mut task, &s.linear, &queries, at(5)).await;
        assert_eq!(events.len(), 12);
        assert_eq!(s.batches()[2..], [4, 4, 4], "4 x 1,200 = 4,800 points");
        assert_eq!(
            task.take_log(),
            Vec::<String>::new(),
            "the cost is unchanged"
        );
    }
}
