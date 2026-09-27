//! End-to-end scenarios: the ticker and the coordinator's commands against a
//! fake Herdr (a scripted runner that keeps workspaces, panes and agents) and
//! the fake Linear. Time passes by rewriting recorded timestamps.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::{Value, json};

use crate::commands::{self, WorkerStart};
use crate::linear::api::fake::{FakeLinear, Shared};
use crate::paths::{Ctx, Env};
use crate::run::{AgentStatus, Run, Status};
use crate::runner::Output;
use crate::runner::fake::{FakeRunner, fail, ok};
use crate::steps::{self, Memory};
use crate::ticker::Log;
use crate::worker;

#[derive(Default)]
struct HerdrModel {
    home: PathBuf,
    next: u32,
    panes: Vec<Value>,
    agents: Vec<Value>,
    prompts: Vec<(String, String)>,
    keys: Vec<(String, String)>,
    closed: Vec<String>,
    notifications: Vec<(String, String)>,
    starts: Vec<Vec<String>>,
    /// The next `agent start` fails with this herdr error code.
    start_error: Option<String>,
}

fn flag<'a>(args: &'a [String], name: &str) -> &'a str {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map_or("", String::as_str)
}

fn reply(result: Value) -> Output {
    ok(&json!({ "result": result }).to_string())
}

impl HerdrModel {
    fn new_pane(&mut self, cwd: &str) -> Value {
        self.next += 1;
        let ws = format!("w{}", self.next);
        let pane = json!({ "pane_id": format!("{ws}:p1"), "tab_id": format!("{ws}:t1"), "workspace_id": ws, "cwd": cwd, "terminal_id": format!("term-{}", self.next) });
        self.panes.push(pane.clone());
        pane
    }

    fn handle(&mut self, args: &[String]) -> Output {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["--version"] => ok("herdr 0.9.1\n"),
            ["session", "list", "--json"] => ok(
                r#"{"sessions":[{"name":"work","default":false,"running":true,"socket_path":"/work.sock"}]}"#,
            ),
            ["agent", "list"] => reply(json!({ "agents": self.agents })),
            ["pane", "list"] => reply(json!({ "panes": self.panes })),
            ["workspace", "create", ..] => {
                let pane = self.new_pane(flag(args, "--cwd"));
                reply(json!({ "root_pane": pane }))
            }
            ["worktree", "create", ..] => {
                let path = self
                    .home
                    .join("worktrees")
                    .join(flag(args, "--branch").replace('/', "-"));
                std::fs::create_dir_all(&path).unwrap();
                let path = path.to_string_lossy().into_owned();
                let pane = self.new_pane(&path);
                reply(json!({ "root_pane": pane, "worktree": { "path": path } }))
            }
            ["worktree", "open", ..] => {
                let pane = self.new_pane(flag(args, "--path"));
                reply(json!({ "root_pane": pane, "worktree": { "path": flag(args, "--path") } }))
            }
            ["agent", "start", name, ..] => {
                self.starts.push(args.to_vec());
                let pane_id = flag(args, "--pane");
                let pane = self
                    .panes
                    .iter()
                    .find(|p| p["pane_id"] == pane_id)
                    .cloned()
                    .expect("start in a known pane");
                let mut agent = pane.clone();
                agent["name"] = json!(name);
                agent["agent"] = json!(flag(args, "--kind"));
                agent["agent_status"] = json!("idle");
                agent["agent_session"] = json!({ "value": format!("sess-{name}") });
                match self.start_error.take() {
                    Some(code) => {
                        if code == "agent_not_ready" {
                            agent["agent_status"] = json!("blocked");
                            self.agents.push(agent);
                        }
                        fail(
                            1,
                            &json!({ "error": { "code": code, "message": "startup failed" } })
                                .to_string(),
                        )
                    }
                    None => {
                        self.agents.push(agent.clone());
                        reply(json!({ "agent": agent }))
                    }
                }
            }
            ["agent", "prompt", target, text] => {
                self.prompts.push((target.to_string(), text.to_string()));
                reply(json!({}))
            }
            ["agent", "send-keys", target, key] => {
                self.keys.push((target.to_string(), key.to_string()));
                reply(json!({}))
            }
            ["agent", "rename", pane, name] => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a["pane_id"] == *pane) {
                    agent["name"] = json!(name);
                }
                reply(json!({}))
            }
            ["workspace", "close", ws] => {
                self.closed.push(ws.to_string());
                self.panes.retain(|p| p["workspace_id"] != *ws);
                self.agents.retain(|a| a["workspace_id"] != *ws);
                reply(json!({}))
            }
            ["workspace", "focus", _] => reply(json!({})),
            ["notification", "show", title, "--body", body] => {
                self.notifications
                    .push((title.to_string(), body.to_string()));
                reply(json!({ "shown": true }))
            }
            ["pane", "report-metadata", ..] => ok(""),
            other => fail(1, &format!("fake herdr: unexpected {other:?}")),
        }
    }
}

struct World {
    home: tempfile::TempDir,
    env: Env,
    runner: FakeRunner,
    herdr: Rc<RefCell<HerdrModel>>,
    linear: Shared,
    memory: Memory,
    log: Log,
}

impl World {
    fn new() -> World {
        Self::with_config(|config| config)
    }

    fn with_config(edit: impl FnOnce(String) -> String) -> World {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_string_lossy().into_owned();
        let env = Env::for_test(
            home.path(),
            &[
                ("XDG_STATE_HOME", &format!("{root}/state")),
                ("XDG_CONFIG_HOME", &format!("{root}/config")),
                ("PATH", "/usr/bin:/bin"),
            ],
        );
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let config = crate::config::tests::SAMPLE
            .replace("path = \"/src/", &format!("path = \"{root}/src/"))
            .replace(
                "[routing.agent]\nprofile = \"router\"\ntimeout_seconds = 60\n",
                "",
            );
        std::fs::write(env.config_dir().join("config.toml"), edit(config)).unwrap();
        let herdr = Rc::new(RefCell::new(HerdrModel {
            home: home.path().to_path_buf(),
            ..HerdrModel::default()
        }));
        let runner = FakeRunner::new();
        let model = herdr.clone();
        runner.on_fn(
            |cmd| cmd.program == "herdr",
            move |cmd| Ok(model.borrow_mut().handle(&cmd.args)),
        );
        runner.on("git -C", fail(128, "not a git repository"));
        runner.on("fetch origin", ok(""));
        let linear = Shared::default();
        let memory = Memory::with_linear(Box::new(linear.clone()));
        let log = Log::new(home.path().join("ticker.log"));
        World {
            home,
            env,
            runner,
            herdr,
            linear,
            memory,
            log,
        }
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            env: &self.env,
            runner: &self.runner,
            detached_ticker: false,
        }
    }

    fn fake(&self) -> std::cell::RefMut<'_, FakeLinear> {
        self.linear.0.borrow_mut()
    }

    fn tick(&mut self) {
        self.memory.last_poll = None;
        let ctx = Ctx {
            env: &self.env,
            runner: &self.runner,
            detached_ticker: false,
        };
        steps::tick(&ctx, &mut self.memory, &self.log).unwrap();
    }

    fn run(&self, key: &str) -> Run {
        Run::load(&self.ctx().runs_dir(), key).unwrap()
    }

    fn set_agent_status(&self, name: &str, status: &str) {
        let mut model = self.herdr.borrow_mut();
        let agent = model
            .agents
            .iter_mut()
            .find(|a| a["name"] == name)
            .expect("agent");
        agent["agent_status"] = json!(status);
    }

    /// Makes the coordinator's recorded state `secs` old.
    fn age_coordinator(&self, key: &str, secs: i64) {
        let past = (jiff::Timestamp::now() - jiff::SignedDuration::from_secs(secs)).to_string();
        self.run(key)
            .update(|r| r.coordinator.last_state_change = past)
            .unwrap();
    }

    fn prompts_to(&self, pane: &str) -> Vec<String> {
        self.herdr
            .borrow()
            .prompts
            .iter()
            .filter(|(p, _)| p == pane)
            .map(|(_, t)| t.clone())
            .collect()
    }

    /// Picks up DATA-1 (estimate 2: size S) and starts its coordinator.
    fn started_run(&mut self) -> String {
        {
            let mut fake = self.fake();
            fake.add_issue("DATA-1", "DATA", "Fix the login");
            fake.issue_mut("DATA-1")["estimate"] = json!(2.0);
        }
        self.tick();
        self.tick();
        self.tick();
        self.run("DATA-1").record().unwrap().coordinator.pane_id
    }

    fn start_worker(&mut self, repo: &str) -> worker::Worker {
        let args = WorkerStart {
            repo: repo.into(),
            profile: "standard".into(),
            title: format!("Change {repo}"),
            task: "Make the change and open a PR.".into(),
        };
        commands::worker_start(&self.ctx(), "DATA-1", &args).unwrap()
    }
}

#[test]
fn a_delegated_issue_becomes_a_run_whose_coordinator_is_started_and_primed() {
    let mut world = World::new();
    world.fake().add_issue("DATA-1", "DATA", "Fix the login");
    world.fake().issue_mut("DATA-1")["estimate"] = json!(2.0);
    world.tick();

    let run = world.run("DATA-1");
    let record = run.record().unwrap();
    assert_eq!(record.size, crate::config::Size::S);
    assert_eq!(record.coordinator.profile, "coordinator-light");
    assert_eq!(
        record.coordinator.status,
        AgentStatus::Open,
        "the workspace is created in the claiming tick"
    );
    assert!(run.issue_md().is_file() && run.dir.join("AGENTS.md").is_file());
    {
        let fake = world.fake();
        let session = fake.session("DATA-1");
        let thoughts: Vec<String> = session
            .sent("thought")
            .iter()
            .map(|a| a["content"]["body"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            thoughts,
            [
                "Picked up DATA-1.",
                "The coordinator uses the `coordinator-light` profile (size S from the estimate)."
            ]
        );
        assert_eq!(fake.issue("DATA-1")["state"]["name"], "In Progress");
    }
    let pane = record.coordinator.pane_id.clone();
    assert_eq!(
        world.herdr.borrow().panes[0]["cwd"],
        json!(run.canonical_dir().to_string_lossy())
    );

    world.tick();
    let starts = world.herdr.borrow().starts.clone();
    assert_eq!(starts.len(), 1);
    assert_eq!(&starts[0][..3], ["agent", "start", "data-1-coordinator"]);
    assert!(starts[0].ends_with(&["--".into(), "--model".into(), "sonnet".into()]));
    world.tick();
    assert_eq!(
        world.prompts_to(&pane),
        ["[herdr-linear-agent ticker] Start DATA-1. Follow AGENTS.md."]
    );
    world.tick();
    assert_eq!(
        world.prompts_to(&pane).len(),
        1,
        "the launch prompt goes out once"
    );
    // A second tick never picks the issue up twice.
    assert_eq!(world.fake().sessions.len(), 1);
}

#[test]
fn a_worker_runs_in_a_worktree_and_its_report_and_pr_reach_linear() {
    let mut world = World::new();
    let coordinator_pane = world.started_run();
    let ctx = world.ctx();
    commands::plan_set(&ctx, "DATA-1", "- [>] Change the API\n- [ ] Finish").unwrap();
    commands::say(&ctx, "DATA-1", "Starting a worker on api.").unwrap();
    let w = world.start_worker("api");
    assert_eq!(w.branch, "herdr-linear-agent/data-1/w1-change-api");
    assert!(world.runner.count("git -C") >= 1);
    assert!(
        std::path::Path::new(&w.brief_dir)
            .join("brief.md")
            .is_file()
    );
    assert!(
        world
            .runner
            .lines("worktree create")
            .iter()
            .any(|l| l.contains("--base origin/main"))
    );

    world.tick();
    let starts = world.herdr.borrow().starts.clone();
    assert!(
        starts.last().unwrap().ends_with(
            &[
                "--model",
                "sonnet",
                "--effort",
                "high",
                "--permission-mode",
                "auto"
            ]
            .map(String::from)
        )
    );
    world.tick();
    assert_eq!(
        world.prompts_to(&w.agent.pane_id),
        ["Read .herdr-linear-agent/DATA-1-w1/brief.md and do what it says."]
    );

    // The worker reports with a pull request and goes idle.
    std::fs::write(
        std::path::Path::new(&w.brief_dir).join("report.md"),
        "PR: https://github.com/acme/api/pull/7\n## Report\nDone.\n## Next\n- Review\n",
    )
    .unwrap();
    world.tick();
    let run = world.run("DATA-1");
    let items = crate::inbox::unhandled(&run);
    assert!(
        items
            .iter()
            .any(|i| i.summary.contains("w1 (api) has a new report")),
        "{items:?}"
    );
    assert!(worker::home_report_path(&run, "w1").is_file());
    {
        let fake = world.fake();
        let session = fake.session("DATA-1");
        let actions: Vec<String> = session
            .sent("action")
            .iter()
            .map(|a| a["content"]["action"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(actions, ["Start worker", "Pull request"]);
        assert_eq!(
            session.external_urls[0].url,
            "https://github.com/acme/api/pull/7"
        );
        assert_eq!(session.plan.as_ref().unwrap()[0]["status"], "inProgress");
        assert!(
            session
                .sent("thought")
                .iter()
                .any(|a| a["content"]["body"] == "Starting a worker on api.")
        );
    }

    // The coordinator is nudged once it has been idle for a minute.
    world.age_coordinator("DATA-1", 120);
    world.tick();
    assert_eq!(
        world.prompts_to(&coordinator_pane).last().unwrap(),
        crate::coordinator::NUDGE_INBOX
    );
    world.tick();
    assert_eq!(
        world
            .prompts_to(&coordinator_pane)
            .iter()
            .filter(|p| **p == crate::coordinator::NUDGE_INBOX)
            .count(),
        1,
        "one nudge per set of items"
    );

    commands::finish(
        &world.ctx(),
        "DATA-1",
        "Opened https://github.com/acme/api/pull/7.",
    )
    .unwrap();
    world.tick();
    let fake = world.fake();
    assert_eq!(fake.issue("DATA-1")["state"]["name"], "In Review");
    assert_eq!(
        fake.session("DATA-1").sent("response")[0]["content"]["body"],
        "Opened https://github.com/acme/api/pull/7."
    );
}

#[test]
fn finish_waits_for_every_worker_and_limits_hold() {
    let mut world = World::with_config(|c| {
        c.replace("[herdr]", "[limits]\nmax_workers_per_run = 1\n\n[herdr]")
    });
    world.started_run();
    world.start_worker("api");
    let error = commands::finish(&world.ctx(), "DATA-1", "Done")
        .unwrap_err()
        .to_string();
    assert!(error.contains("w1 is Working"), "{error}");
    let args = WorkerStart {
        repo: "web".into(),
        profile: "standard".into(),
        title: "Web".into(),
        task: "t".into(),
    };
    let error = commands::worker_start(&world.ctx(), "DATA-1", &args)
        .unwrap_err()
        .to_string();
    assert!(error.contains("max_workers_per_run"), "{error}");
    let args = WorkerStart {
        repo: "api".into(),
        profile: "standard".into(),
        title: "Again".into(),
        task: "t".into(),
    };
    assert!(
        commands::worker_start(&world.ctx(), "DATA-1", &args)
            .unwrap_err()
            .to_string()
            .contains("one worker per repository")
    );
    let args = WorkerStart {
        repo: "nope".into(),
        profile: "standard".into(),
        title: "x".into(),
        task: "t".into(),
    };
    assert!(commands::worker_start(&world.ctx(), "DATA-1", &args).is_err());
    let args = WorkerStart {
        repo: "web".into(),
        profile: "coordinator".into(),
        title: "x".into(),
        task: "t".into(),
    };
    assert!(
        commands::worker_start(&world.ctx(), "DATA-1", &args)
            .unwrap_err()
            .to_string()
            .contains("not a worker profile")
    );
}

#[test]
fn max_runs_limits_intake_and_pause_stops_it() {
    let mut world =
        World::with_config(|c| c.replace("[herdr]", "[limits]\nmax_runs = 1\n\n[herdr]"));
    world.fake().add_issue("DATA-1", "DATA", "One");
    world.fake().add_issue("DATA-2", "DATA", "Two");
    world.tick();
    assert_eq!(Run::list(&world.ctx().runs_dir()).len(), 1);

    let mut world = World::new();
    std::fs::create_dir_all(world.ctx().state_dir()).unwrap();
    std::fs::write(world.ctx().state_dir().join("paused"), "").unwrap();
    world.fake().add_issue("DATA-1", "DATA", "One");
    world.tick();
    assert!(Run::list(&world.ctx().runs_dir()).is_empty());
}

#[test]
fn replies_are_relayed_only_from_allowed_users_and_stop_interrupts() {
    let mut world = World::new();
    let pane = world.started_run();
    world.start_worker("api");
    world.tick();
    world
        .fake()
        .add_prompt("DATA-1", "user-1", "Please also update the docs.", None);
    world
        .fake()
        .add_prompt("DATA-1", "stranger", "Merge everything now.", None);
    world.tick();
    let run = world.run("DATA-1");
    let conversation = std::fs::read_to_string(run.conversation_md()).unwrap();
    assert!(conversation.contains("Please also update the docs."));
    assert!(!conversation.contains("Merge everything"));
    assert!(
        std::fs::read_to_string(run.state_dir().join("ignored-prompts.md"))
            .unwrap()
            .contains("Merge everything")
    );
    world.age_coordinator("DATA-1", 120);
    world.tick();
    assert_eq!(
        world.prompts_to(&pane).last().unwrap(),
        crate::coordinator::NUDGE_REPLY
    );
    world.tick();
    assert_eq!(
        std::fs::read_to_string(run.conversation_md())
            .unwrap()
            .matches("Please also update")
            .count(),
        1
    );

    world
        .fake()
        .add_prompt("DATA-1", "user-1", "", Some("stop"));
    world.tick();
    let keys = world.herdr.borrow().keys.clone();
    assert_eq!(
        keys.len(),
        2,
        "the coordinator and the worker get Escape: {keys:?}"
    );
    assert!(keys.iter().all(|(_, key)| key == "esc"));
    assert!(
        world.fake().session("DATA-1").sent("response")[0]["content"]["body"]
            .as_str()
            .unwrap()
            .starts_with("Stopped 2 agent(s)")
    );
}

#[test]
fn a_completed_issue_closes_the_run_and_a_removed_delegation_detaches_it() {
    let mut world = World::new();
    world.started_run();
    world.start_worker("api");
    world.tick();
    world.fake().set_state("DATA-1", "Done");
    world.tick();
    let record = world.run("DATA-1").record().unwrap();
    assert_eq!(record.status, Status::Closed);
    assert_eq!(
        world.herdr.borrow().closed.len(),
        2,
        "worker and coordinator workspaces"
    );
    assert_eq!(
        worker::load(&world.run("DATA-1"), "w1")
            .unwrap()
            .agent
            .status,
        AgentStatus::Stopped
    );
    let starts = world.herdr.borrow().starts.len();
    world.tick();
    assert_eq!(
        world.herdr.borrow().starts.len(),
        starts,
        "a closed run is left alone"
    );
    // Reopened and still delegated: the coordinator comes back with its session.
    world.fake().set_state("DATA-1", "Todo");
    world.tick();
    world.tick();
    let record = world.run("DATA-1").record().unwrap();
    assert_eq!(record.status, Status::Active);
    assert!(record.coordinator.resume);
    assert!(
        world
            .herdr
            .borrow()
            .starts
            .last()
            .unwrap()
            .ends_with(&["--resume".into(), "sess-data-1-coordinator".into()])
    );

    let mut world = World::new();
    world.started_run();
    world.fake().issue_mut("DATA-1")["delegate"] = Value::Null;
    world.tick();
    assert_eq!(
        world.run("DATA-1").record().unwrap().status,
        Status::Detached
    );
    assert!(world.herdr.borrow().closed.is_empty(), "workspaces stay");
    // Delegated again: the run continues.
    world.fake().issue_mut("DATA-1")["delegate"] =
        json!({ "id": crate::linear::api::fake::APP_USER });
    world.tick();
    assert_eq!(world.run("DATA-1").record().unwrap().status, Status::Active);
}

#[test]
fn a_dialog_in_a_pane_is_reported_once_and_a_lost_coordinator_can_be_resumed() {
    let mut world = World::new();
    world.started_run();
    let w = world.start_worker("api");
    world.herdr.borrow_mut().start_error = Some("agent_not_ready".into());
    world.tick();
    // Blocked for over 30 seconds.
    let past = (jiff::Timestamp::now() - jiff::SignedDuration::from_secs(90)).to_string();
    worker::update(&world.run("DATA-1"), "w1", |w| {
        w.agent.last_state = "blocked".into();
        w.agent.last_state_change = past.clone();
    })
    .unwrap();
    world.tick();
    world.tick();
    let asked = world.fake().session("DATA-1").sent("elicitation");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0]["content"]["body"]
            .as_str()
            .unwrap()
            .contains(&w.agent.pane_id)
    );
    assert!(!world.herdr.borrow().notifications.is_empty());
    assert!(
        crate::inbox::unhandled(&world.run("DATA-1"))
            .iter()
            .any(|i| i.summary.contains("Waiting on you"))
    );

    // The coordinator's workspace disappears.
    let ws = world
        .run("DATA-1")
        .record()
        .unwrap()
        .coordinator
        .workspace_id;
    world
        .herdr
        .borrow_mut()
        .handle(&["workspace".into(), "close".into(), ws]);
    world.tick();
    let record = world.run("DATA-1").record().unwrap();
    assert!(record.coordinator_lost);
    assert_eq!(world.fake().session("DATA-1").sent("elicitation").len(), 2);
    world.fake().add_prompt("DATA-1", "user-1", "resume", None);
    world.tick();
    world.tick();
    let starts = world.herdr.borrow().starts.clone();
    assert!(
        starts
            .last()
            .unwrap()
            .ends_with(&["--resume".into(), "sess-data-1-coordinator".into()]),
        "{starts:?}"
    );
    world.tick();
    let pane = world.run("DATA-1").record().unwrap().coordinator.pane_id;
    assert!(world.prompts_to(&pane)[0].contains("You were restarted"));
}

#[test]
fn quiet_runs_get_a_heartbeat_and_long_runs_ask_to_continue() {
    let mut world =
        World::with_config(|c| c.replace("[herdr]", "[limits]\nrun_timeout_hours = 1\n\n[herdr]"));
    let pane = world.started_run();
    let long_ago = (jiff::Timestamp::now() - jiff::SignedDuration::from_secs(2 * 3600)).to_string();
    world
        .run("DATA-1")
        .update(|r| {
            r.last_activity = long_ago.clone();
            r.timeout_since = long_ago.clone();
        })
        .unwrap();
    world.tick();
    {
        let fake = world.fake();
        let session = fake.session("DATA-1");
        assert!(
            session
                .sent("thought")
                .iter()
                .any(|a| a["ephemeral"] == true),
            "heartbeat"
        );
        assert!(
            session.sent("elicitation")[0]["content"]["body"]
                .as_str()
                .unwrap()
                .contains("going for 1 hours")
        );
    }
    // No prompt reaches the coordinator until someone replies.
    crate::inbox::write(&world.run("DATA-1"), "worker", "w1", "something").unwrap();
    world.age_coordinator("DATA-1", 120);
    world.tick();
    assert_eq!(world.prompts_to(&pane).len(), 1, "only the launch prompt");
    world
        .fake()
        .add_prompt("DATA-1", "user-1", "Continue", None);
    world.tick();
    assert!(!world.run("DATA-1").record().unwrap().timeout_asked);
}

#[test]
fn restarts_switch_profiles_and_are_limited() {
    let mut world = World::new();
    world.started_run();
    world.start_worker("api");
    world.tick();
    let restarted = commands::worker_restart(&world.ctx(), "DATA-1", "w1", Some("deep")).unwrap();
    assert_eq!(
        (restarted.agent.kind.as_str(), restarted.restarts),
        ("codex", 1)
    );
    assert!(restarted.agent.prompt_pending);
    assert_eq!(
        world.herdr.borrow().closed.len(),
        1,
        "the old workspace is closed; the checkout stays"
    );
    assert!(
        std::fs::read_to_string(std::path::Path::new(&restarted.brief_dir).join("brief.md"))
            .unwrap()
            .contains("previous attempt")
    );
    world.tick();
    let starts = world.herdr.borrow().starts.clone();
    assert!(
        starts
            .last()
            .unwrap()
            .contains(&"model_reasoning_effort=xhigh".to_string())
    );
    commands::worker_restart(&world.ctx(), "DATA-1", "w1", None).unwrap();
    assert!(
        commands::worker_restart(&world.ctx(), "DATA-1", "w1", None)
            .unwrap_err()
            .to_string()
            .contains("limit is 2")
    );
}

#[test]
fn a_prompt_to_a_worker_is_refused_while_it_waits_on_a_dialog() {
    let mut world = World::new();
    world.started_run();
    world.start_worker("api");
    world.tick();
    world.tick();
    world.set_agent_status("data-1-w1", "blocked");
    assert!(
        commands::worker_prompt(&world.ctx(), "DATA-1", "w1", "Answer")
            .unwrap_err()
            .to_string()
            .contains("dialog")
    );
    world.set_agent_status("data-1-w1", "working");
    commands::worker_prompt(&world.ctx(), "DATA-1", "w1", "Also add a test.").unwrap();
    let task = std::fs::read_to_string(worker::task_path(&world.run("DATA-1"), "w1")).unwrap();
    assert!(task.contains("## Follow-ups") && task.contains("Also add a test."));
}

#[test]
fn the_routing_agent_decides_an_unsized_issue() {
    let mut world = World::with_config(|c| {
        c + "\n[routing.agent]\nprofile = \"router\"\ntimeout_seconds = 60\n"
    });
    let bin = world.home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("claude");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat >/dev/null\necho '{\"structured_output\":{\"size\":\"XS\"}}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let root = world.home.path().to_string_lossy().into_owned();
    world.env = Env::for_test(
        world.home.path(),
        &[
            ("XDG_STATE_HOME", &format!("{root}/state")),
            ("XDG_CONFIG_HOME", &format!("{root}/config")),
            ("PATH", &format!("{}:/usr/bin:/bin", bin.display())),
        ],
    );
    world.fake().add_issue("DATA-1", "DATA", "Tiny");
    // The fake agent may answer within the tick that starts it.
    world.tick();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while world.run("DATA-1").record().unwrap().routing.is_some()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
        world.tick();
    }
    let record = world.run("DATA-1").record().unwrap();
    assert_eq!(
        (
            record.size,
            record.size_source.as_str(),
            record.coordinator.profile.as_str()
        ),
        (crate::config::Size::XS, "agent", "coordinator-light")
    );
}

#[test]
fn context_shows_the_digest_and_marks_items_seen() {
    let mut world = World::new();
    world.started_run();
    let run = world.run("DATA-1");
    let id = crate::inbox::write(&run, "worker", "w1", "item").unwrap();
    commands::context(&world.ctx(), "DATA-1").unwrap();
    assert!(crate::inbox::seen(&run).contains(&id));
    commands::inbox_done(&world.ctx(), "DATA-1", &[], true).unwrap();
    assert!(crate::inbox::unhandled(&run).is_empty());
}

#[test]
fn the_session_linear_created_on_delegation_is_used() {
    let mut world = World::new();
    world.fake().add_issue("DATA-1", "DATA", "Delegated");
    let session = world.fake().delegate_session("DATA-1");
    world.tick();
    assert_eq!(world.run("DATA-1").record().unwrap().session_id, session);
    let fake = world.fake();
    assert_eq!(fake.sessions.len(), 1, "no second session");
    assert!(!fake.sessions[0].sent("thought").is_empty());
}

#[test]
fn a_claim_without_a_session_still_decides_its_coordinator() {
    let mut world = World::new();
    world.fake().add_issue("DATA-1", "DATA", "Early");
    world.fake().sessions_disabled = true;
    world.tick();
    let record = world.run("DATA-1").record().unwrap();
    assert!(record.session_id.is_empty());
    assert_eq!(
        record.coordinator.profile, "coordinator",
        "the claim went on"
    );

    // A run left without a coordinator decision (an older build) is finished.
    world
        .run("DATA-1")
        .update(|r| r.coordinator = Default::default())
        .unwrap();
    world.fake().sessions_disabled = false;
    world.tick();
    let record = world.run("DATA-1").record().unwrap();
    assert_eq!(record.coordinator.profile, "coordinator");
    assert!(!record.session_id.is_empty());
    let fake = world.fake();
    assert_eq!(fake.sessions.len(), 1);
    assert_eq!(fake.issue("DATA-1")["state"]["name"], "In Progress");
    assert!(
        fake.sessions[0]
            .sent("thought")
            .iter()
            .any(|a| a["content"]["body"] == "Picked up DATA-1.")
    );
}

#[test]
fn the_trust_dialog_is_accepted_only_when_enabled() {
    let trusted = |world: &World| -> Vec<String> {
        let config: Value = serde_json::from_str(
            &std::fs::read_to_string(world.home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        config["projects"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    };

    let mut world = World::new();
    std::fs::write(world.home.path().join(".claude.json"), "{}").unwrap();
    world.started_run();
    assert!(trusted(&world).is_empty(), "off by default");

    let mut world = World::with_config(|c| c + "\n[claude]\nauto_accept_trust_dialog = true\n");
    std::fs::write(world.home.path().join(".claude.json"), "{}").unwrap();
    world.started_run();
    let run_dir = world
        .run("DATA-1")
        .canonical_dir()
        .to_string_lossy()
        .into_owned();
    assert_eq!(trusted(&world), std::slice::from_ref(&run_dir));
    let w = world.start_worker("api");
    world.tick();
    let all = trusted(&world);
    assert!(
        all.contains(&w.agent.cwd) && all.contains(&w.repo_path),
        "{all:?}"
    );
}
