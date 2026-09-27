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

use super::api::{self, IssueDetail, IssueRef, IssueStatus, RunUpdate};
use super::client::LinearApi;
use crate::config;
use crate::outbox;
use crate::run::Run;

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
        LinearTask {
            teams: linear.teams.clone(),
            review_state: linear.review_state.clone(),
            intake_interval: seconds(linear.intake_interval_seconds),
            run_read_interval: seconds(linear.run_read_interval_seconds),
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

    /// When the next interval is due.
    pub fn next_due(&self, now: Timestamp) -> Timestamp {
        let intake = self.last_intake.map_or(now, |at| at + self.intake_interval);
        let read = self.last_read.map_or(now, |at| at + self.run_read_interval);
        intake.min(read)
    }

    /// One round: the viewer while unknown, the delegated poll and the run
    /// read when their intervals are due, then the flush. Each event goes to
    /// `events` as soon as it happened, so the reconciler sees a run's flush
    /// as soon as it finishes. It neither sleeps nor reads the clock.
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

        // The viewer is retried on the run-read interval; run reads wait for it.
        let read_due = due(self.last_read, self.run_read_interval, now);
        if read_due {
            self.last_read = Some(now);
            if self.level.app_user.is_none()
                && let Ok(viewer) = client.viewer().await
            {
                self.level.app_user = Some(viewer.id);
            }
        }
        if due(self.last_intake, self.intake_interval, now) {
            self.last_intake = Some(now);
            match client.delegated_issues(&self.teams).await {
                Ok(issues) => {
                    self.level.delegated = Some(Delegated {
                        read_at: now,
                        issues,
                    })
                }
                Err(error) => self.log.push(format!("intake: {error}")),
            }
        }
        if read_due && self.level.app_user.is_some() {
            self.read_runs(client, queries, now, events).await;
        }
        self.flush(client, queries, now, events).await;
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
        let updates = client.read_runs(&batch).await;
        for ((query, _), update) in with_session.iter().zip(updates) {
            let update = match update {
                Ok(update) => update,
                Err(error) => {
                    self.log.push(format!("{}: {error}", query.key()));
                    continue;
                }
            };
            let stale = query
                .issue_updated_at
                .as_deref()
                .is_none_or(|at| at.is_empty() || at != update.issue.updated_at);
            let detail = if stale {
                self.detail(client, query).await
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
            if !query.issue_updated_at.as_deref().unwrap_or("").is_empty() {
                continue;
            }
            let Some(detail) = self.detail(client, query).await else {
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
    }

    async fn detail(
        &mut self,
        client: &impl LinearApi,
        query: &RunQuery,
    ) -> Option<Box<IssueDetail>> {
        match client.issue(&query.issue_id).await {
            Ok(detail) => Some(Box::new(detail)),
            Err(error) => {
                self.log.push(format!("{}: {error}", query.key()));
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
            let key = query.key();
            let run = Run {
                dir: query.run_dir.clone(),
                key: key.clone(),
            };
            let blocked = if outbox::pending(&run).is_empty() {
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
                self.log.push(format!(
                    "{}: could not create the session: {error}",
                    query.key()
                ));
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
    use crate::linear::api::fake::FakeLinear;
    use crate::linear::api::{Activity, Content};
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
            ["DATA-1: Linear write failed, will retry: Linear rate-limited the request"]
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
}
