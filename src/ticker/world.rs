//! A ticker in a box for the scenario tests: the reconciler and the Linear
//! task's `step`, with the agent subcommands a test runs, against the
//! trait-level Herdr fake and the fake Linear. Time is a clock the test
//! moves; nothing ages records by rewriting them.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::Log;
use super::reconcile::{Deps, EffectDone, Reconciler, RoutingDone, Wake};
use crate::commands::{self, Session, WorkerStart};
use crate::config::Config;
use crate::herdr::FakeHerdr;
use crate::linear::api::fake::FakeLinear;
use crate::linear::task::{LinearEvent, LinearLevel, LinearTask, RunQuery};
use crate::paths::{Ctx, Env};
use crate::process::fake::{FakeRunner, fail, ok};
use crate::run::{Run, RunRecord};
use crate::worker::Worker;

pub const SOCKET: &str = "/tmp/hla-world/work.sock";
/// Rounds of Linear step and pass before `settle` gives up.
const ROUNDS: usize = 50;
/// Requests that are not effects: reading the state and refreshing tokens.
const READS: [&str; 2] = ["session.snapshot", "pane.report_metadata"];

pub struct World {
    pub home: tempfile::TempDir,
    pub env: Env,
    pub runner: FakeRunner,
    pub herdr: FakeHerdr,
    pub linear: Mutex<FakeLinear>,
    pub config: Config,
    task: LinearTask,
    reconciler: Reconciler,
    effects: mpsc::Receiver<EffectDone>,
    routing: mpsc::Receiver<RoutingDone>,
    log: Log,
    now: Timestamp,
    injected: Vec<LinearEvent>,
    /// Every pass is followed by a second one with nothing new, which must
    /// change nothing.
    pub every_pass_twice: bool,
    /// The Linear task steps with the queries of this many rounds before
    /// the latest, as a task that reads while the reconciler moves on.
    pub query_lag: usize,
    published: VecDeque<Vec<RunQuery>>,
    /// `ActivitySent` reaches the reconciler one round after its flush, so
    /// a pass runs between the two.
    pub hold_activity_sent: bool,
    held: Vec<LinearEvent>,
    /// The events of a round go to one pass with the level the reconciler
    /// had before, and the new level to the next pass.
    pub split_level: bool,
    seen_level: LinearLevel,
}

/// Every file under the runs folder with its bytes.
fn files_under(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => folders.push(path),
                Ok(_) => {
                    let bytes = std::fs::read(&path).unwrap_or_default();
                    found.insert(path, bytes);
                }
                Err(_) => {}
            }
        }
    }
    found
}

/// The script body of a fake `claude` routing agent that reads its input
/// and picks `name`, in Claude Code's JSON result shape.
pub fn answer_with(name: &str) -> String {
    format!("cat > /dev/null\necho '{{\"structured_output\":{{\"coordinator\":\"{name}\"}}}}'\n")
}

fn write_router(bin: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin).unwrap();
    let path = bin.join("claude");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn env_in(home: &Path, path: &str) -> Env {
    let root = home.to_string_lossy().into_owned();
    Env::for_test(
        home,
        &[
            ("XDG_STATE_HOME", &format!("{root}/state")),
            ("XDG_CONFIG_HOME", &format!("{root}/config")),
            ("PATH", path),
        ],
    )
}

fn channels() -> (
    Reconciler,
    mpsc::Receiver<EffectDone>,
    mpsc::Receiver<RoutingDone>,
) {
    let (effects_tx, effects) = mpsc::channel(64);
    let (routing_tx, routing) = mpsc::channel(64);
    let reconciler = Reconciler::new(effects_tx, routing_tx).unwrap();
    (reconciler, effects, routing)
}

impl World {
    /// The sample config, unchanged apart from the World's paths.
    pub fn sample() -> World {
        World::with(|config| config)
    }

    /// A World whose config is the shared sample with its repositories under
    /// the World's home and `edit` applied. Its routing agent is a fake
    /// `claude` that picks `coordinator`; [`World::router`] replaces it.
    pub fn with(edit: impl FnOnce(String) -> String) -> World {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_string_lossy().into_owned();
        let bin = home.path().join("bin");
        let env = env_in(home.path(), &format!("{}:/usr/bin:/bin", bin.display()));
        let text = crate::config::tests::SAMPLE
            .replace("path = \"/src/", &format!("path = \"{root}/src/"));
        let text = edit(text);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("config.toml"), &text).unwrap();
        let config = Config::parse(&text).unwrap();
        write_router(&bin, &answer_with("coordinator"));
        let runner = FakeRunner::new();
        runner.on("git -C", fail(128, "not a git repository"));
        runner.on("fetch origin", ok(""));
        let (reconciler, effects, routing) = channels();
        World {
            herdr: FakeHerdr::new(home.path()),
            linear: Mutex::new(FakeLinear::default()),
            task: LinearTask::new(&config.linear),
            log: Log::new(home.path().join("ticker.log")),
            now: Timestamp::now().round(jiff::Unit::Second).unwrap(),
            injected: Vec::new(),
            every_pass_twice: false,
            query_lag: 0,
            published: VecDeque::new(),
            hold_activity_sent: false,
            held: Vec::new(),
            split_level: false,
            seen_level: LinearLevel::default(),
            home,
            env,
            runner,
            config,
            reconciler,
            effects,
            routing,
        }
    }

    /// The same World after a ticker restart: nothing kept in memory.
    pub fn restart_ticker(&mut self) {
        let (reconciler, effects, routing) = channels();
        self.reconciler = reconciler;
        self.effects = effects;
        self.routing = routing;
        self.task = LinearTask::new(&self.config.linear);
        self.published.clear();
        self.held.clear();
        self.seen_level = LinearLevel::default();
    }

    pub fn ctx(&self) -> Ctx<'_> {
        Ctx {
            env: &self.env,
            runner: &self.runner,
            detached_ticker: false,
        }
    }

    pub fn session(&self) -> Session<FakeHerdr> {
        Session {
            herdr: self.herdr.clone(),
            socket: SOCKET.into(),
        }
    }

    pub fn now(&self) -> Timestamp {
        self.now
    }

    pub fn later(&mut self, seconds: i64) {
        self.now += SignedDuration::from_secs(seconds);
    }

    pub fn fake(&self) -> MutexGuard<'_, FakeLinear> {
        self.linear.lock().unwrap()
    }

    /// Hands the next pass an event as if the Linear task had sent it.
    pub fn inject(&mut self, event: LinearEvent) {
        self.injected.push(event);
    }

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(self.home.path().join("ticker.log")).unwrap_or_default()
    }

    /// Runs Linear step and pass, with both Linear intervals due, until a
    /// round changes no file of any run, makes no Herdr request other than
    /// reads and leaves the Linear queries as they were, with nothing in
    /// flight.
    pub async fn settle(&mut self) {
        for _ in 0..ROUNDS {
            let before = files_under(&self.ctx().runs_dir());
            let asked = self.herdr.requests().len();
            let queries = self.reconciler.queries().to_vec();
            self.once().await;
            let effects = self.herdr.requests()[asked..]
                .iter()
                .filter(|method| !READS.contains(&method.as_str()))
                .count();
            if effects == 0
                && !self.reconciler.busy()
                && self.held.is_empty()
                && self.published.len() > self.query_lag
                && self
                    .published
                    .iter()
                    .all(|q| q == self.reconciler.queries())
                && queries == self.reconciler.queries()
                && before == files_under(&self.ctx().runs_dir())
            {
                return;
            }
        }
        panic!("the ticker did not settle within {ROUNDS} rounds");
    }

    /// One round: a Linear step and the pass it wakes.
    pub async fn once(&mut self) {
        let wake = self.round().await;
        if self.split_level {
            let before = self.seen_level.clone();
            self.pass(wake, &before).await;
            let level = self.task.level().clone();
            self.pass(Wake::default(), &level).await;
        } else {
            let level = self.task.level().clone();
            self.pass(wake, &level).await;
        }
        self.seen_level = self.task.level().clone();
    }

    /// The queries the Linear task steps with, `query_lag` rounds old.
    fn queries_for_step(&mut self) -> Vec<RunQuery> {
        self.published.push_back(self.reconciler.queries().to_vec());
        while self.published.len() > self.query_lag + 1 {
            self.published.pop_front();
        }
        self.published.front().cloned().unwrap_or_default()
    }

    /// One Linear step, then the results of effect tasks and routing agents
    /// that are still out.
    async fn round(&mut self) -> Wake {
        self.task.force_due();
        self.fake().present = Some(self.now);
        let queries = self.queries_for_step();
        let mut events = std::mem::take(&mut self.injected);
        events.append(&mut self.held);
        let (sender, mut sent) = mpsc::channel(1024);
        self.task
            .step_into(&self.linear, &queries, self.now, &sender)
            .await;
        for event in std::iter::from_fn(|| sent.try_recv().ok()) {
            if self.hold_activity_sent && matches!(event, LinearEvent::ActivitySent { .. }) {
                self.held.push(event);
            } else {
                events.push(event);
            }
        }
        for line in self.task.take_log() {
            self.log.line(&line);
        }
        let mut wake = Wake {
            events,
            ..Wake::default()
        };
        while wake.effects.len() < self.reconciler.effects_in_flight() {
            let done = tokio::time::timeout(Duration::from_secs(5), self.effects.recv())
                .await
                .expect("an effect task did not report")
                .expect("the effect channel closed");
            wake.effects.push(done);
        }
        while wake.routing.len() < self.reconciler.routing_in_flight() {
            let done = tokio::time::timeout(Duration::from_secs(20), self.routing.recv())
                .await
                .expect("the routing agent did not finish")
                .expect("the routing channel closed");
            wake.routing.push(done);
        }
        wake
    }

    async fn pass(&mut self, wake: Wake, level: &LinearLevel) {
        let ctx = Ctx {
            env: &self.env,
            runner: &self.runner,
            detached_ticker: false,
        };
        let deps = Deps {
            ctx: &ctx,
            config: &self.config,
            herdr: &self.herdr,
            socket: SOCKET,
            log: &self.log,
        };
        self.reconciler.pass(&deps, level, wake, self.now).await;
        if !self.every_pass_twice {
            return;
        }
        // An effect task still out may change Herdr between the two passes;
        // then only the absence of a second effect is checked.
        let quiet = self.reconciler.effects_in_flight() == 0;
        let before = files_under(&ctx.runs_dir());
        let asked = self.herdr.requests().len();
        let in_flight = self.reconciler.effects_in_flight();
        self.reconciler
            .pass(&deps, level, Wake::default(), self.now)
            .await;
        assert_eq!(
            in_flight,
            self.reconciler.effects_in_flight(),
            "a repeated pass started an effect"
        );
        if quiet {
            let repeated: Vec<String> = self.herdr.requests()[asked..]
                .iter()
                .filter(|m| !READS.contains(&m.as_str()))
                .cloned()
                .collect();
            assert!(
                repeated.is_empty(),
                "a repeated pass asked Herdr {repeated:?}"
            );
            assert!(
                before == files_under(&ctx.runs_dir()),
                "a repeated pass changed a run folder"
            );
        }
    }

    /// One round whose pass takes its snapshot, then waits while
    /// `worker restart DATA-1 <id>` runs, and applies the older snapshot to
    /// the newer records.
    pub async fn restart_worker_mid_pass(&mut self, id: &str) -> Worker {
        let (taken, go) = self.herdr.hold_next_snapshot();
        let wake = self.round().await;
        let level = self.task.level().clone();
        let ctx = Ctx {
            env: &self.env,
            runner: &self.runner,
            detached_ticker: false,
        };
        let deps = Deps {
            ctx: &ctx,
            config: &self.config,
            herdr: &self.herdr,
            socket: SOCKET,
            log: &self.log,
        };
        let session = Session {
            herdr: self.herdr.clone(),
            socket: SOCKET.into(),
        };
        let pass = self.reconciler.pass(&deps, &level, wake, self.now);
        let restart = async {
            taken.await.expect("the pass took no snapshot");
            let restarted = commands::worker_restart(&ctx, &session, "DATA-1", id, None)
                .await
                .unwrap();
            go.send(()).unwrap();
            restarted
        };
        let ((), restarted) = tokio::join!(pass, restart);
        self.seen_level = level;
        restarted
    }

    /// A pass with nothing from the Linear task.
    pub async fn pass_alone(&mut self) {
        let level = self.task.level().clone();
        self.pass(Wake::default(), &level).await;
    }

    /// Entries per in-memory map of the reconciler.
    pub fn remembered(&self) -> Vec<(&'static str, usize)> {
        self.reconciler.remembered()
    }

    /// The deadline the reconciler would sleep until after the last pass.
    pub fn deadline(&self) -> Timestamp {
        let ctx = self.ctx();
        let deps = Deps {
            ctx: &ctx,
            config: &self.config,
            herdr: &self.herdr,
            socket: SOCKET,
            log: &self.log,
        };
        self.reconciler.next_deadline(&deps, self.now)
    }

    /// Replaces the fake routing agent with a script body (after its shebang).
    pub fn router(&self, body: &str) {
        write_router(&self.home.path().join("bin"), body);
    }

    pub fn pause_intake(&self) {
        let state = self.ctx().state_dir();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("paused"), "").unwrap();
    }

    pub fn runs(&self) -> usize {
        Run::list(&self.ctx().runs_dir()).len()
    }

    pub fn sessions(&self) -> usize {
        self.fake().sessions.len()
    }

    /// Linear refuses agent sessions, as for an app without the webhook.
    pub fn refuse_sessions(&self, refuse: bool) {
        self.fake().sessions_disabled = refuse;
    }

    pub fn move_issue(&self, key: &str, state: &str) {
        self.fake().set_state(key, state);
    }

    pub fn set_delegate(&self, key: &str, delegate: Value) {
        self.fake().issue_mut(key)["delegate"] = delegate;
    }

    /// Writes the worker's report in its worktree.
    pub fn report(&self, w: &Worker, text: &str) {
        std::fs::write(Path::new(&w.brief_dir).join("report.md"), text).unwrap();
    }

    pub fn brief(&self, w: &Worker) -> String {
        std::fs::read_to_string(Path::new(&w.brief_dir).join("brief.md")).unwrap_or_default()
    }

    /// A text file of the run folder, empty when missing.
    pub fn text(&self, key: &str, relative: &str) -> String {
        std::fs::read_to_string(self.run(key).dir.join(relative)).unwrap_or_default()
    }

    /// The summaries of the run's unhandled inbox items.
    pub fn inbox(&self, key: &str) -> Vec<String> {
        crate::inbox::unhandled(&self.run(key))
            .into_iter()
            .map(|i| i.summary)
            .collect()
    }

    pub fn home_file(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    /// The folders Claude Code's config marks as trusted.
    pub fn trusted(&self) -> Vec<String> {
        let text = std::fs::read_to_string(self.home_file(".claude.json")).unwrap_or_default();
        let config: Value = serde_json::from_str(&text).unwrap_or_default();
        config["projects"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn run(&self, key: &str) -> Run {
        Run::load(&self.ctx().runs_dir(), key).unwrap()
    }

    pub fn record(&self, key: &str) -> RunRecord {
        self.run(key).record().unwrap()
    }

    pub fn worker(&self, key: &str, id: &str) -> Worker {
        crate::worker::load(&self.run(key), id).unwrap()
    }

    /// An issue of the DATA team delegated to the app user.
    pub fn delegate(&self, key: &str, title: &str, estimate: Option<f64>) -> String {
        let mut fake = self.fake();
        let id = fake.add_issue(key, "DATA", title);
        fake.issue_mut(key)["estimate"] = json!(estimate);
        id
    }

    /// A person's message in the issue's session, stamped after the clock.
    pub fn message(&self, key: &str, user: &str, body: &str, signal: Option<&str>) {
        let mut fake = self.fake();
        fake.present = Some(self.now);
        fake.add_prompt(key, user, body, signal);
    }

    /// DATA-1, estimate 2 (size S), claimed and its coordinator prompted.
    /// Returns the coordinator's pane.
    pub async fn running_issue(&mut self) -> String {
        self.delegate("DATA-1", "Fix the login", Some(2.0));
        self.settle().await;
        self.record("DATA-1").coordinator.pane_id
    }

    /// `worker start DATA-1` on `repo` with the `standard` profile.
    pub async fn start_worker(&self, repo: &str) -> Worker {
        let args = WorkerStart {
            repo: repo.into(),
            profile: "standard".into(),
            title: format!("Change {repo}"),
            task: "Make the change and open a PR.".into(),
        };
        commands::worker_start(&self.ctx(), &self.session(), "DATA-1", &args)
            .await
            .unwrap()
    }

    /// The action names the plugin sent, oldest first.
    pub fn actions(&self, key: &str) -> Vec<String> {
        self.sent(key, "action")
            .iter()
            .filter_map(|a| a["content"]["action"].as_str().map(str::to_string))
            .collect()
    }

    /// Activities of one type the plugin sent to the issue's session.
    pub fn sent(&self, key: &str, kind: &str) -> Vec<Value> {
        self.fake().session(key).sent(kind)
    }

    /// The bodies of the sent activities of one type.
    pub fn bodies(&self, key: &str, kind: &str) -> Vec<String> {
        self.sent(key, kind)
            .iter()
            .map(|a| {
                a["content"]["body"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    pub fn issue_state(&self, key: &str) -> String {
        self.fake().issue(key)["state"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Herdr requests of one method so far.
    pub fn asked(&self, method: &str) -> usize {
        self.herdr
            .requests()
            .iter()
            .filter(|m| m.as_str() == method)
            .count()
    }
}
