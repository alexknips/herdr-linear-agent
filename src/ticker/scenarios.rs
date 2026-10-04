//! End-to-end behavior of the ticker and the coordinator's commands, run in
//! the World: each scenario settles the ticker and checks what reached
//! Herdr, Linear and the run folder.

use serde_json::{Value, json};

use super::world::World;
use crate::commands::{self, WorkerStart};
use crate::coordinator::{NUDGE_INBOX, NUDGE_REPLY};
use crate::herdr::PaneId;
use crate::linear::api::fake::APP_USER;
use crate::linear::api::{IssueStatus, RunUpdate};
use crate::linear::task::{DECLINED, LinearEvent};
use crate::run::{AgentStatus, Recovery, Status, WaitReason};
use crate::{inbox, worker};

const KEY: &str = "acme/DATA-1";
const PR: &str = "https://github.com/acme/api/pull/7";
const LAUNCH: &str = "[herdr-linear-agent ticker] Start acme/DATA-1. Follow AGENTS.md.";
const RESUMED_WITH: [&str; 2] = ["--resume", "sess-acme-data-1-coordinator"];

fn request(repo: &str, profile: &str, title: &str) -> WorkerStart {
    WorkerStart {
        repo: repo.into(),
        profile: profile.into(),
        title: title.into(),
        task: "t".into(),
    }
}

fn limit(key: &str, value: u64) -> impl FnOnce(String) -> String {
    let table = format!("[limits]\n{key} = {value}\n\n[herdr]");
    move |config| config.replace("[herdr]", &table)
}

fn to(world: &World, pane: &str) -> Vec<String> {
    world.herdr.prompts_to(pane)
}

fn last_args(world: &World) -> Vec<String> {
    world
        .herdr
        .starts()
        .pop()
        .map(|s| s.args)
        .unwrap_or_default()
}

/// A start's arguments without the `--session-id <id>` a new Claude
/// session gets.
fn flags(args: &[String]) -> Vec<String> {
    match args {
        [rest @ .., flag, _] if flag == "--session-id" => rest.to_vec(),
        other => other.to_vec(),
    }
}

fn ends_with(args: &[String], tail: &[&str]) -> bool {
    args.len() >= tail.len() && args[args.len() - tail.len()..].iter().eq(tail)
}

fn count(texts: &[String], wanted: &str) -> usize {
    texts.iter().filter(|t| *t == wanted).count()
}

fn mentions(texts: &[String], part: &str) -> bool {
    texts.iter().any(|t| t.contains(part))
}

const WAITING_TOO_LONG: i64 = 280;
const CONTINUED: &str = "The previous agent process ended while work was in progress. Reread AGENTS.md and the issue/task context, inspect the current workspace and existing changes, then continue the same task from this session. Do not repeat completed work.";

// ---------------------------------------------------------------- claiming

#[tokio::test]
async fn a_delegated_issue_is_claimed_placed_started_and_prompted_once() {
    let mut world = World::sample();
    world.delegate(KEY, "Fix the login", Some(2.0));
    world.settle().await;

    let (run, c) = (world.run(KEY), world.record(KEY));
    assert_eq!(
        (c.routing_source.as_str(), c.coordinator.profile.as_str()),
        ("chosen by the routing agent", "coordinator")
    );
    assert_eq!(
        c.coordinator.status,
        AgentStatus::Open,
        "placed in the claim"
    );
    assert!(run.issue_md().is_file() && run.dir.join("AGENTS.md").is_file());
    let expected = [
        "Picked up DATA-1.",
        "The coordinator uses the `coordinator` profile (chosen by the routing agent).",
    ];
    assert_eq!(world.bodies(KEY, "thought"), expected);
    assert_eq!(world.issue_state(KEY), "In Progress");
    let canonical = run.canonical_dir().to_string_lossy().into_owned();
    let cwds: Vec<Option<String>> = world.herdr.panes().into_iter().map(|p| p.cwd).collect();
    assert_eq!(cwds, [Some(canonical)]);

    // A placement, then one start, then one prompt.
    let order: Vec<String> = world
        .herdr
        .requests()
        .into_iter()
        .filter(|m| m.starts_with("workspace.") || m.starts_with("agent."))
        .collect();
    assert_eq!(order, ["workspace.create", "agent.start", "agent.prompt"]);
    let start = &world.herdr.starts()[0];
    assert_eq!(start.name, "acme-data-1-coordinator");
    // Herdr's report of the session replaces the id afterwards, so the one
    // given to Claude is read from the arguments.
    let session = start.args.last().map_or("", String::as_str);
    assert!(uuid::Uuid::parse_str(session).is_ok(), "{session}");
    assert!(
        ends_with(
            &start.args,
            &[
                "--model",
                "opus",
                "--effort",
                "high",
                "--permission-mode",
                "auto",
                "--session-id",
                session
            ]
        ),
        "{:?}",
        start.args
    );
    assert!(!c.coordinator.started_at.is_empty());
    assert_eq!(to(&world, &c.coordinator.pane_id), [LAUNCH]);

    world.later(5);
    world.settle().await;
    assert_eq!(to(&world, &c.coordinator.pane_id).len(), 1, "prompted once");
    assert_eq!(
        (world.herdr.starts().len(), world.sessions()),
        (1, 1),
        "claimed once"
    );
}

#[tokio::test]
async fn a_crashed_coordinator_resumes_the_same_session_after_a_ticker_restart() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    let session = world.record(KEY).coordinator.agent_session;
    world.herdr.restart();
    world.later(5);
    world.settle().await;
    assert!(matches!(
        world.record(KEY).coordinator.recovery,
        Recovery::Suspected { .. }
    ));

    world.later(30);
    world.settle().await;
    assert!(matches!(
        world.record(KEY).coordinator.recovery,
        Recovery::RetryWait { attempt: 1, .. }
    ));
    world.restart_ticker();
    world.settle().await;
    assert_eq!(
        world.herdr.starts().len(),
        1,
        "the persisted delay has not elapsed"
    );

    world.later(15);
    world.settle().await;
    let starts = world.herdr.starts();
    assert_eq!(starts.len(), 2, "one bounded resume start");
    assert!(
        ends_with(&starts[1].args, &["--resume", &session]),
        "{:?}",
        starts[1].args
    );
    assert!(to(&world, &pane).contains(&CONTINUED.to_string()));
    assert!(matches!(
        world.record(KEY).coordinator.recovery,
        Recovery::Recovered { attempts: 1, .. }
    ));
}

#[tokio::test]
async fn a_recovered_native_session_is_kept_for_a_later_manual_resume() {
    let mut world = World::sample();
    world.running_issue().await;
    let native_session = "opencode-discovered-session";
    let due_at = world.now().to_string();
    world
        .run(KEY)
        .update(|r| {
            r.coordinator.agent_session.clear();
            r.coordinator.recovery = Recovery::RetryWait {
                attempt: 1,
                due_at,
                session: native_session.into(),
            };
        })
        .unwrap();
    world.herdr.report_no_sessions();
    world.herdr.restart();
    world.settle().await;
    assert_eq!(world.record(KEY).coordinator.agent_session, native_session);
    assert!(ends_with(&last_args(&world), &["--resume", native_session]));

    world.herdr.restart();
    world
        .run(KEY)
        .update(|r| {
            r.coordinator.recovery = Recovery::Stale {
                attempts: 3,
                reason: "automatic limit reached".into(),
                reported: true,
            };
            r.coordinator_lost = true;
        })
        .unwrap();
    world.message(KEY, "user-1", "resume", None);
    world.settle().await;
    assert!(ends_with(&last_args(&world), &["--resume", native_session]));
}

#[tokio::test]
async fn an_unknown_resume_answer_is_not_retried_when_the_agent_appears() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    world.herdr.restart();
    world.later(5);
    world.settle().await;
    world.later(30);
    world.settle().await;
    world.herdr.next_start_unknown();
    world.later(15);
    world.settle().await;
    assert_eq!(world.herdr.starts().len(), 2);
    assert!(matches!(
        world.record(KEY).coordinator.recovery,
        Recovery::Recovered { attempts: 1, .. }
    ));
    assert_eq!(count(&to(&world, &pane), CONTINUED), 1);
    world.settle().await;
    assert_eq!(
        world.herdr.starts().len(),
        2,
        "unknown outcome never duplicates the start"
    );
}

#[tokio::test]
async fn a_resume_request_not_sent_consumes_one_attempt_and_uses_the_next_backoff() {
    let mut world = World::sample();
    world.running_issue().await;
    world.herdr.restart();
    world.later(5);
    world.settle().await;
    world.later(30);
    world.settle().await;
    world.herdr.next_start_not_sent();
    world.later(15);
    world.settle().await;
    assert!(matches!(
        world.record(KEY).coordinator.recovery,
        Recovery::RetryWait { attempt: 2, .. }
    ));
    assert_eq!(
        world.herdr.starts().len(),
        1,
        "NotSent did not start an agent"
    );
    world.later(30);
    world.settle().await;
    assert_eq!(
        world.herdr.starts().len(),
        2,
        "the second attempt follows its backoff"
    );
}

#[tokio::test]
async fn stale_recovery_waits_for_an_existing_answer_before_asking_to_resume() {
    let mut world = World::sample();
    world.running_issue().await;
    commands::ask(&world.ctx(), KEY, "Should I continue?", &[])
        .await
        .unwrap();
    world.settle().await;
    world
        .run(KEY)
        .update(|r| {
            r.coordinator.recovery = Recovery::Stale {
                attempts: 3,
                reason: "test stale state".into(),
                reported: false,
            }
        })
        .unwrap();
    world.settle().await;
    let record = world.record(KEY);
    assert_eq!(
        record.awaiting_reply.as_ref().unwrap().reason,
        WaitReason::CoordinatorQuestion
    );
    assert!(matches!(
        record.coordinator.recovery,
        Recovery::Stale {
            reported: false,
            ..
        }
    ));

    world.message(KEY, "user-1", "Continue", None);
    world.settle().await;
    let record = world.record(KEY);
    assert_eq!(
        record.awaiting_reply.as_ref().unwrap().reason,
        WaitReason::CoordinatorLost
    );
    assert!(matches!(
        record.coordinator.recovery,
        Recovery::Stale { reported: true, .. }
    ));
    assert!(mentions(
        &world.bodies(KEY, "elicitation"),
        "Reply `resume`"
    ));
}

#[tokio::test]
async fn a_crashed_worker_resumes_without_restarting_its_coordinator() {
    let mut world = World::sample();
    world.running_issue().await;
    let worker = world.start_worker("api").await;
    world.settle().await;
    let session = worker::load(&world.run(KEY), "w1")
        .unwrap()
        .agent
        .agent_session;
    world.herdr.remove_agent("acme-data-1-w1");
    world.later(5);
    world.settle().await;
    assert!(matches!(
        worker::load(&world.run(KEY), "w1").unwrap().agent.recovery,
        Recovery::Suspected { .. }
    ));

    world.later(30);
    world.settle().await;
    world.restart_ticker();
    world.settle().await;
    assert_eq!(
        world.herdr.starts().len(),
        2,
        "only the original coordinator and worker starts"
    );
    world.later(15);
    world.settle().await;
    let starts = world.herdr.starts();
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2].name, "acme-data-1-w1");
    assert!(
        ends_with(&starts[2].args, &["--resume", &session]),
        "{:?}",
        starts[2].args
    );
    assert!(to(&world, &worker.agent.pane_id).contains(&CONTINUED.to_string()));
    assert!(matches!(
        worker::load(&world.run(KEY), "w1").unwrap().agent.recovery,
        Recovery::Recovered { attempts: 1, .. }
    ));

    for (attempt, delay) in [(2, 30), (3, 60)] {
        world.herdr.remove_agent("acme-data-1-w1");
        world.later(5);
        world.settle().await;
        world.later(35);
        world.settle().await;
        let recovery = worker::load(&world.run(KEY), "w1").unwrap().agent.recovery;
        assert!(
            matches!(
                recovery,
                Recovery::RetryWait { attempt: found, .. } if found == attempt
            ),
            "{recovery:?}"
        );
        world.later(delay);
        world.settle().await;
        assert!(matches!(
            worker::load(&world.run(KEY), "w1").unwrap().agent.recovery,
            Recovery::Recovered { attempts: found, .. } if found == attempt
        ));
    }
    let starts_before_stale = world.herdr.starts().len();
    world.herdr.remove_agent("acme-data-1-w1");
    world.later(5);
    world.settle().await;
    world.later(35);
    world.settle().await;
    assert!(matches!(
        worker::load(&world.run(KEY), "w1").unwrap().agent.recovery,
        Recovery::Stale {
            attempts: 3,
            reported: true,
            ..
        }
    ));
    assert_eq!(
        world.herdr.starts().len(),
        starts_before_stale,
        "three resumes is the cap"
    );
}

#[tokio::test]
async fn a_stale_worker_recovery_is_written_to_the_coordinator_inbox_once() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    worker::update(&world.run(KEY), "w1", |w| {
        w.agent.recovery = Recovery::Stale {
            attempts: 3,
            reason: "test stale state".into(),
            reported: false,
        }
    })
    .unwrap();
    world.settle().await;
    let items = inbox::unhandled(&world.run(KEY));
    assert_eq!(items.len(), 1);
    assert!(
        items[0]
            .summary
            .contains("Automatic resume stopped for worker w1 after 3 attempts")
    );
    assert!(matches!(
        worker::load(&world.run(KEY), "w1").unwrap().agent.recovery,
        Recovery::Stale { reported: true, .. }
    ));
}

#[tokio::test]
async fn a_worker_report_written_after_a_crash_suppresses_automatic_resume() {
    let mut world = World::sample();
    world.running_issue().await;
    let worker = world.start_worker("api").await;
    world.settle().await;
    world.herdr.remove_agent("acme-data-1-w1");
    world.later(5);
    world.settle().await;
    world.report(&worker, "## Report\n\nThe work is complete.\n");
    world.later(35);
    world.settle().await;
    assert_eq!(
        world.herdr.starts().len(),
        2,
        "do not restart a worker that reported"
    );
    assert!(worker::report_hash(&worker::load(&world.run(KEY), "w1").unwrap()).is_some());
    assert!(matches!(
        worker::load(&world.run(KEY), "w1").unwrap().agent.recovery,
        Recovery::Suspected { .. }
    ));
}

#[tokio::test]
async fn stopped_finished_and_awaiting_runs_do_not_resume_a_missing_agent() {
    for gate in ["stopped", "finished", "awaiting_reply"] {
        let mut world = World::sample();
        world.running_issue().await;
        if gate == "awaiting_reply" {
            commands::ask(&world.ctx(), KEY, "Should I continue?", &[])
                .await
                .unwrap();
            world.settle().await;
        } else {
            world
                .run(KEY)
                .update(|r| match gate {
                    "stopped" => r.stopped = true,
                    _ => r.finished = true,
                })
                .unwrap();
        }
        world.herdr.restart();
        world.later(5);
        world.settle().await;
        world.later(120);
        world.settle().await;
        assert_eq!(world.herdr.starts().len(), 1, "{gate} run was not resumed");
        if gate == "stopped" {
            assert_eq!(world.record(KEY).coordinator.recovery, Recovery::None);
        }
    }
}

/// A second workspace whose `DATA` team has its own allowed user and review
/// state.
const BETA: &str = r#"
[workspaces.beta]
client_id = "client-456"

[workspaces.beta.teams.DATA]
allowed_user_ids = ["user-2"]
review_state = "Ready for review"
routing = "default"
"#;

#[tokio::test]
async fn two_workspaces_with_the_same_issue_key_run_apart_under_their_own_team_rules() {
    let mut world = World::with(|c| c + BETA);
    world.delegate("acme/DATA-1", "Fix the login", Some(2.0));
    world.delegate("beta/DATA-1", "Fix the export", Some(2.0));
    world.settle().await;

    let keys: Vec<String> = crate::run::Run::list(&world.ctx().runs_dir())
        .into_iter()
        .map(|run| run.key)
        .collect();
    assert_eq!(keys, ["acme/DATA-1", "beta/DATA-1"]);
    let mut names: Vec<String> = world.herdr.starts().into_iter().map(|s| s.name).collect();
    names.sort();
    assert_eq!(
        names,
        ["acme-data-1-coordinator", "beta-data-1-coordinator"]
    );
    for key in ["acme/DATA-1", "beta/DATA-1"] {
        assert_eq!(
            world.bodies(key, "thought")[0],
            "Picked up DATA-1.",
            "{key}"
        );
        assert_eq!(world.issue_state(key), "In Progress", "{key}");
    }
    assert_eq!(
        world.record("beta/DATA-1").url,
        "https://linear.app/beta/issue/DATA-1/x"
    );

    // Each run's team decides whose replies count.
    world.message("beta/DATA-1", "user-1", "From acme's user.", None);
    world.message("beta/DATA-1", "user-2", "From beta's user.", None);
    world.settle().await;
    let relayed = world.text("beta/DATA-1", "conversation.md");
    assert!(
        relayed.contains("From beta's user.") && !relayed.contains("From acme's user."),
        "{relayed}"
    );

    // `finish` moves the issue to its own team's review state.
    world.fake_for("beta/DATA-1").issue_mut("DATA-1")["team"]["states"]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "state-ready", "name": "Ready for review", "type": "started", "position": 3.5 }));
    commands::finish(&world.ctx(), &world.session(), "beta/DATA-1", "Done.")
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(world.issue_state("beta/DATA-1"), "Ready for review");
    assert_eq!(world.issue_state("acme/DATA-1"), "In Progress");
}

#[tokio::test]
async fn after_a_config_reload_the_next_worker_starts_with_the_new_profile() {
    let mut world = World::sample();
    world.running_issue().await;
    std::fs::write(
        world.env.config_dir().join("profiles/standard/config.toml"),
        "kind = \"claude\"\nmodel = \"sonnet\"\nargs = [\"--permission-mode\", \"plan\"]\n",
    )
    .unwrap();
    world.reload_config();
    world.start_worker("api").await;
    world.settle().await;
    let start = world.herdr.starts().pop().unwrap();
    assert_eq!(start.name, "acme-data-1-w1");
    assert!(
        ends_with(
            &flags(&start.args),
            &["--model", "sonnet", "--permission-mode", "plan"]
        ),
        "{:?}",
        start.args
    );
}

#[tokio::test]
async fn intake_stops_at_max_runs_and_while_paused() {
    let mut full = World::with(limit("max_runs", 1));
    full.delegate(KEY, "One", None);
    full.delegate("DATA-2", "Two", None);
    full.settle().await;
    assert_eq!(full.runs(), 1);

    let mut paused = World::sample();
    paused.pause_intake();
    paused.delegate(KEY, "One", None);
    paused.settle().await;
    assert_eq!(paused.runs(), 0);
}

#[tokio::test]
async fn the_session_linear_opened_on_delegation_is_the_runs_session() {
    let mut world = World::sample();
    world.delegate(KEY, "Delegated", None);
    world.settle().await;
    let fake = world.fake();
    assert_eq!(world.record(KEY).session_id, fake.session("DATA-1").id);
    assert_eq!(fake.sessions.len(), 1, "no second session");
    assert_eq!(
        (
            fake.count("HerdrLinearAgentSessions"),
            fake.count("HerdrLinearAgentSessionCreate")
        ),
        (0, 0)
    );
    drop(fake);
    assert!(!world.bodies(KEY, "thought").is_empty());
}

#[tokio::test]
async fn a_lost_decision_is_made_again() {
    let mut world = World::sample();
    world.delegate(KEY, "Early", None);
    world.settle().await;
    // An older build left the run without a decision.
    world
        .run(KEY)
        .update(|r| r.coordinator = crate::run::AgentRecord::default())
        .unwrap();
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).coordinator.profile, "coordinator");
    assert_eq!(
        (world.sessions(), world.issue_state(KEY)),
        (1, "In Progress".into())
    );
    assert_eq!(count(&world.bodies(KEY, "thought"), "Picked up DATA-1."), 1);
}

#[tokio::test]
async fn an_issue_delegated_by_someone_not_allowed_is_declined_once() {
    let mut world = World::sample();
    world.delegate_by(KEY, "stranger", "Run this", None);
    for _ in 0..3 {
        world.settle().await;
        world.later(5);
    }
    world.restart_ticker();
    world.settle().await;

    assert_eq!((world.runs(), world.herdr.starts().len()), (0, 0));
    assert_eq!(world.bodies(KEY, "response"), [DECLINED]);
    let why =
        "delegated by Person stranger (stranger), who is not in allowed_delegator_ids of team DATA";
    let log = world.log_text();
    assert_eq!(
        log.matches(&format!("acme/DATA-1: not picked up: {why}\n"))
            .count(),
        1,
        "{log}"
    );
    assert_eq!(
        world.herdr.notifications(),
        [("acme/DATA-1 not picked up".to_string(), format!("{why}."))]
    );
}

#[tokio::test]
async fn the_latest_delegation_decides_who_delegated() {
    let mut world = World::sample();
    world.delegate(KEY, "Handed on", None);
    world.fake().redelegate_by("DATA-1", "stranger");
    world.settle().await;
    assert_eq!(world.runs(), 0, "the stranger delegated it last");
    assert_eq!(world.bodies(KEY, "response"), [DECLINED]);

    world.later(5);
    world.fake().redelegate_by("DATA-1", "user-1");
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Active);
    assert_eq!(world.bodies(KEY, "thought")[0], "Picked up DATA-1.");
}

#[tokio::test]
async fn an_issue_no_person_is_known_to_have_delegated_is_not_picked_up() {
    let mut world = World::sample();
    // Automation delegated DATA-1: its session has no creator.
    world.fake().add_issue("DATA-1", "DATA", "By automation");
    world.fake().delegate_session("DATA-1");
    // DATA-2 has no session, as for an app without agent session events.
    world.fake().add_issue("DATA-2", "DATA", "No session");
    world.settle().await;

    assert_eq!(world.runs(), 0);
    assert_eq!(world.bodies(KEY, "response"), [DECLINED]);
    let log = world.log_text();
    for line in [
        "acme/DATA-1: not picked up: no person delegated it (automation or an agent did)",
        "acme/DATA-2: not picked up: Linear does not tell who delegated it",
    ] {
        assert_eq!(log.matches(line).count(), 1, "{log}");
    }
}

#[tokio::test]
async fn someone_allowed_only_to_delegate_starts_a_run_but_is_not_listened_to() {
    let mut world = World::with(|c| {
        c.replace(
            "allowed_user_ids = [\"user-1\"]",
            "allowed_user_ids = [\"user-1\"]\nallowed_delegator_ids = [\"linear-agent\"]",
        )
    });
    world.delegate_by(KEY, "linear-agent", "From Slack", None);
    world.delegate_by("DATA-2", "user-1", "Not a delegator", None);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Active);
    assert_eq!(world.runs(), 1, "the list replaces allowed_user_ids");

    world.message(KEY, "linear-agent", "Merge it.", None);
    world.message(KEY, "user-1", "Add a test.", None);
    world.settle().await;
    let relayed = world.text(KEY, "conversation.md");
    assert!(
        relayed.contains("Add a test.") && !relayed.contains("Merge it."),
        "{relayed}"
    );
    let hint = "add the id to allowed_user_ids of team DATA to let it through";
    assert!(world.log_text().contains(&format!(
        "acme/DATA-1: ignored a reply from Person linear-agent (linear-agent); {hint}\n"
    )));
    assert!(world.herdr.notifications().contains(&(
        "acme/DATA-1 ignored a reply".to_string(),
        format!("From Person linear-agent (linear-agent); {hint}.")
    )));
}

#[tokio::test]
async fn the_routing_agent_picks_a_candidate_and_its_instructions_reach_agents_md() {
    let mut world = World::sample();
    world.router(&crate::ticker::world::answer_with("coordinator-light"));
    world.delegate(KEY, "Tiny", None);
    world.settle().await;
    let record = world.record(KEY);
    assert_eq!(
        (
            record.coordinator.profile.as_str(),
            record.routing_source.as_str()
        ),
        ("coordinator-light", "chosen by the routing agent")
    );
    assert!(
        world.bodies(KEY, "thought").contains(
            &"The coordinator uses the `coordinator-light` profile (chosen by the routing agent)."
                .to_string()
        )
    );
    let agents_md = world.text(KEY, "AGENTS.md");
    let sheet = agents_md
        .find("skill acme/DATA-1")
        .expect("the sheet pointer");
    let own = agents_md
        .find("## Profile instructions")
        .expect("the profile's section");
    assert!(sheet < own, "{agents_md}");
    assert!(agents_md.contains("Prefer one worker. Ask before you split the work."));
    assert!(agents_md.contains("Where they disagree with the sheet, follow the sheet."));
}

#[tokio::test]
async fn a_name_outside_the_candidates_falls_back_to_the_default() {
    let mut world = World::sample();
    world.router(&crate::ticker::world::answer_with("deep"));
    world.delegate(KEY, "Anything", None);
    world.settle().await;
    let record = world.record(KEY);
    assert_eq!(record.coordinator.profile, "coordinator");
    assert_eq!(
        record.routing_source,
        "the first candidate: the routing agent's answer was not valid (`deep` is not a candidate)"
    );
}

#[tokio::test]
async fn a_routing_job_an_older_build_recorded_is_left_alone_and_routed_again() {
    let mut world = World::sample();
    world.delegate(KEY, "Old", None);
    world.refuse_sessions(true);
    world.settle().await;
    let run = world.run(KEY);
    run.update(|r| r.coordinator = crate::run::AgentRecord::default())
        .unwrap();
    // The field an older build wrote; nothing reads it now.
    let path = run.state_dir().join("run.json");
    let mut stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stored["routing"] = json!({"pid": 4242, "started": "2026-01-01T00:00:00Z", "output": ""});
    std::fs::write(&path, stored.to_string()).unwrap();

    world.restart_ticker();
    world.settle().await;
    assert_eq!(world.record(KEY).coordinator.profile, "coordinator");
}

// ---------------------------------------------------------------- workers

#[tokio::test]
async fn a_worker_report_with_a_pr_reaches_the_inbox_and_linear() {
    let mut world = World::sample();
    let coordinator = world.running_issue().await;
    let plan = "- [>] Change the API\n- [ ] Finish";
    commands::plan_set(&world.ctx(), KEY, plan).await.unwrap();
    commands::say(&world.ctx(), KEY, "Starting a worker on api.")
        .await
        .unwrap();
    let w = world.start_worker("api").await;
    assert_eq!(w.branch, "herdr-linear-agent/acme/data-1/w1-change-api");
    assert!(world.runner.count("git -C") > 0);
    assert!(!world.brief(&w).is_empty(), "brief.md is written");
    assert_eq!(world.herdr.worktrees()[0].2, "origin/main", "--base");

    world.settle().await;
    let flags = [
        "--model",
        "sonnet",
        "--effort",
        "high",
        "--permission-mode",
        "auto",
    ];
    assert!(ends_with(&self::flags(&last_args(&world)), &flags));
    assert_eq!(
        to(&world, &w.agent.pane_id),
        ["Read .herdr-linear-agent/acme-DATA-1-w1/brief.md and do what it says."]
    );

    world.report(
        &w,
        &format!("PR: {PR}\n## Report\nDone.\n## Next\n- Review\n"),
    );
    world.settle().await;
    let items = world.inbox(KEY);
    assert!(mentions(&items, "w1 (api) has a new report"), "{items:?}");
    assert!(worker::home_report_path(&world.run(KEY), "w1").is_file());
    assert_eq!(world.actions(KEY), ["Start worker", "Pull request"]);
    assert_eq!(
        world.sent(KEY, "action")[1]["content"]["parameter"],
        "https://linear.review/acme/api/pull/7 (worker w1, repo api)"
    );
    assert_eq!(
        count(&world.bodies(KEY, "thought"), "w1 (api) reported:\n\nDone."),
        1
    );
    {
        let linear = world.fake();
        let session = linear.session("DATA-1");
        let urls: Vec<&str> = session
            .external_urls
            .iter()
            .map(|u| u.url.as_str())
            .collect();
        assert_eq!(urls, ["https://linear.review/acme/api/pull/7"]);
        assert_eq!(
            session.plan.clone().unwrap_or_default()[0]["status"],
            "inProgress"
        );
    }
    assert_eq!(
        count(&world.bodies(KEY, "thought"), "Starting a worker on api."),
        1
    );

    // Idle for two minutes: one nudge for the unseen items.
    world.later(120);
    world.settle().await;
    assert_eq!(
        to(&world, &coordinator).last().map(String::as_str),
        Some(NUDGE_INBOX)
    );
    world.later(5);
    world.settle().await;
    assert_eq!(
        count(&to(&world, &coordinator), NUDGE_INBOX),
        1,
        "one nudge per set"
    );

    let summary = format!("Opened {PR}.");
    commands::finish(&world.ctx(), &world.session(), KEY, &summary)
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(world.issue_state(KEY), "In Review");
    assert_eq!(world.bodies(KEY, "response"), [summary]);
}

#[tokio::test]
async fn finish_and_worker_start_hold_their_limits() {
    let mut world = World::with(limit("max_workers_per_run", 1));
    world.running_issue().await;
    world.start_worker("api").await;
    let (ctx, session) = (world.ctx(), world.session());
    let refused = commands::finish(&ctx, &session, KEY, "Done")
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("w1 is Working"), "{refused}");
    let refusals = [
        (request("web", "standard", "Web"), "max_workers_per_run"),
        (
            request("api", "standard", "Again"),
            "one worker per repository",
        ),
        (request("web", "coordinator", "x"), "not a worker profile"),
        (
            request("nope", "standard", "x"),
            "not in the repository catalog",
        ),
    ];
    for (start, expected) in refusals {
        let refused = commands::worker_start(&ctx, &session, KEY, &start)
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains(expected), "{refused}");
    }
}

#[tokio::test]
async fn a_restart_switches_the_profile_and_stops_at_two() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    let again = commands::worker_restart(&world.ctx(), &world.session(), KEY, "w1", Some("deep"))
        .await
        .unwrap();
    assert_eq!(
        (
            again.agent.kind.as_str(),
            again.restarts,
            again.agent.prompt_pending
        ),
        ("codex", 1, true)
    );
    assert_eq!(
        world.herdr.closed().len(),
        1,
        "the old workspace; the checkout stays"
    );
    assert!(world.brief(&again).contains("previous attempt"));

    world.settle().await;
    assert!(
        last_args(&world)
            .iter()
            .any(|a| a == "model_reasoning_effort=xhigh")
    );
    let (ctx, session) = (world.ctx(), world.session());
    let restart = || commands::worker_restart(&ctx, &session, KEY, "w1", None);
    restart().await.unwrap();
    let third = restart().await.unwrap_err().to_string();
    assert!(third.contains("limit is 2"), "{third}");
}

#[tokio::test]
async fn a_worker_waiting_on_a_dialog_refuses_a_prompt() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    let (ctx, session) = (world.ctx(), world.session());
    world.herdr.set_status("acme-data-1-w1", "blocked");
    let refused = commands::worker_prompt(&ctx, &session, KEY, "w1", "Answer")
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("dialog"), "{refused}");
    world.herdr.set_status("acme-data-1-w1", "working");
    commands::worker_prompt(&ctx, &session, KEY, "w1", "Also add a test.")
        .await
        .unwrap();
    let task = world.text(KEY, "workers/w1.task.md");
    assert!(task.contains("## Follow-ups"), "{task}");
    assert!(task.contains("Also add a test."), "{task}");
}

#[tokio::test]
async fn the_trust_dialog_is_accepted_only_when_the_config_says_so() {
    let mut off = World::sample();
    std::fs::write(off.home_file(".claude.json"), "{}").unwrap();
    off.running_issue().await;
    assert_eq!(off.trusted(), Vec::<String>::new(), "off by default");

    let mut on = World::with(|c| c + "\n[claude]\nauto_accept_trust_dialog = true\n");
    std::fs::write(on.home_file(".claude.json"), "{}").unwrap();
    on.running_issue().await;
    let run_dir = on.run(KEY).canonical_dir().to_string_lossy().into_owned();
    assert_eq!(on.trusted(), [run_dir]);
    let w = on.start_worker("api").await;
    on.settle().await;
    let folders = on.trusted();
    for folder in [&w.agent.cwd, &w.repo_path] {
        assert!(folders.contains(folder), "{folder} in {folders:?}");
    }
}

// ---------------------------------------------------------------- Linear

#[tokio::test]
async fn only_allowed_users_reach_the_coordinator_and_stop_interrupts_everyone() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    world.message(KEY, "user-1", "Please also update the docs.", None);
    world.message(KEY, "stranger", "Merge everything now.", None);
    world.settle().await;
    let relayed = world.text(KEY, "conversation.md");
    assert!(
        relayed.contains("Please also update the docs.") && !relayed.contains("Merge everything")
    );
    assert!(
        world
            .text(KEY, ".state/ignored-prompts.md")
            .contains("Merge everything")
    );

    world.later(120);
    world.settle().await;
    assert_eq!(
        to(&world, &pane).last().map(String::as_str),
        Some(NUDGE_REPLY)
    );
    world.later(5);
    world.settle().await;
    let relayed = world.text(KEY, "conversation.md");
    assert_eq!(
        relayed.matches("Please also update").count(),
        1,
        "relayed once"
    );

    world.message(KEY, "user-1", "", Some("stop"));
    world.settle().await;
    let keys: Vec<String> = world.herdr.keys().into_iter().map(|(_, k)| k).collect();
    assert_eq!(keys, ["esc", "esc"], "the coordinator and the worker");
    let answer = world.bodies(KEY, "response");
    assert!(answer[0].starts_with("Stopped 2 agent(s)"), "{answer:?}");
}

#[tokio::test]
async fn a_stop_holds_every_prompt_until_the_next_reply() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    world.message(KEY, "user-1", "", Some("stop"));
    world.settle().await;
    assert!(world.record(KEY).stopped);

    let item = "w1 is idle without a report";
    inbox::write(&world.run(KEY), "worker", "w1", item).unwrap();
    world.later(120);
    world.settle().await;
    assert_eq!(to(&world, &pane), [LAUNCH], "nothing more while stopped");

    world.message(KEY, "user-1", "Go on.", None);
    world.settle().await;
    world.later(120);
    world.settle().await;
    assert!(!world.record(KEY).stopped);
    assert_eq!(
        to(&world, &pane).last().map(String::as_str),
        Some(NUDGE_REPLY)
    );
}

#[tokio::test]
async fn quiet_runs_get_a_heartbeat_and_long_ones_ask_to_go_on() {
    let mut world = World::with(limit("ask_to_continue_after_hours", 1));
    let pane = world.running_issue().await;
    world.later(10 * 60);
    world.settle().await;
    let ephemeral: Vec<Value> = world
        .sent(KEY, "thought")
        .into_iter()
        .filter(|a| a["ephemeral"] == json!(true))
        .collect();
    assert_eq!(ephemeral.len(), 1, "one heartbeat");
    world.later(2 * 3600);
    world.settle().await;
    assert_eq!(
        world.record(KEY).awaiting_reply.as_ref().map(|w| w.reason),
        Some(WaitReason::RunTimeout)
    );
    assert_eq!(world.fake().session("DATA-1").status, "awaitingInput");
    assert!(mentions(
        &world.bodies(KEY, "elicitation"),
        "going for 1 hours"
    ));

    inbox::write(&world.run(KEY), "worker", "w1", "something").unwrap();
    world.later(120);
    world.settle().await;
    assert_eq!(
        to(&world, &pane),
        [LAUNCH],
        "no nudge while the question is open"
    );
    world.message(KEY, "user-1", "Continue", None);
    world.settle().await;
    assert!(!world.record(KEY).timeout_asked);
}

#[tokio::test]
async fn an_explicit_question_stays_the_latest_activity_past_the_heartbeat_deadline() {
    let mut world = World::sample();
    world.running_issue().await;
    commands::ask(&world.ctx(), KEY, "Should I proceed?", &[])
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(
        world.sent(KEY, "elicitation").last().unwrap()["content"]["body"],
        "Should I proceed?"
    );

    world.later(10 * 60);
    world.settle().await;

    assert_eq!(
        world
            .fake()
            .session("DATA-1")
            .sent_types()
            .last()
            .map(String::as_str),
        Some("elicitation"),
        "no heartbeat or worker activity should replace the outstanding question"
    );
    assert_eq!(world.fake().session("DATA-1").status, "awaitingInput");
}

#[tokio::test]
async fn a_worker_report_is_saved_locally_while_linear_waits_for_an_answer() {
    let mut world = World::sample();
    world.running_issue().await;
    let worker = world.start_worker("api").await;
    world.settle().await;
    commands::ask(&world.ctx(), KEY, "Should I proceed?", &[])
        .await
        .unwrap();
    world.settle().await;

    world.report(&worker, "## Report\n\nThe tests pass.\n");
    world.settle().await;

    assert!(crate::worker::home_report_path(&world.run(KEY), "w1").is_file());
    assert_eq!(
        world
            .fake()
            .session("DATA-1")
            .sent_types()
            .last()
            .map(String::as_str),
        Some("elicitation"),
        "saving the worker report must not publish progress over the question"
    );
    assert_eq!(world.fake().session("DATA-1").status, "awaitingInput");
}

#[tokio::test]
async fn a_new_allowed_reply_clears_the_wait_once_and_resumes_progress() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    commands::ask(&world.ctx(), KEY, "Should I proceed?", &[])
        .await
        .unwrap();
    world.settle().await;
    let wait = world.record(KEY).awaiting_reply.unwrap();
    let reloaded = crate::run::Run::load(&world.ctx().runs_dir(), KEY).unwrap();
    assert_eq!(
        reloaded.record().unwrap().awaiting_reply,
        Some(wait.clone())
    );

    world.message(KEY, "user-1", "Proceed", None);
    world.settle().await;
    world.later(120);
    world.settle().await;
    assert!(world.record(KEY).awaiting_reply.is_none());
    assert_eq!(world.fake().session("DATA-1").status, "active");
    assert_eq!(
        std::fs::read_to_string(world.run(KEY).conversation_md())
            .unwrap()
            .matches("Proceed")
            .count(),
        1
    );
    assert!(wait.asked_at < world.record(KEY).prompt_cursor);

    commands::say(&world.ctx(), KEY, "Continuing now.")
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(world.fake().session("DATA-1").status, "active");
    assert_eq!(
        world
            .fake()
            .session("DATA-1")
            .sent_types()
            .last()
            .map(String::as_str),
        Some("thought")
    );
    assert_eq!(
        to(&world, &pane).last().map(String::as_str),
        Some(NUDGE_REPLY)
    );
}

#[tokio::test]
async fn a_fresh_reply_is_accepted_even_when_its_server_time_predates_local_question_time() {
    let mut world = World::sample();
    world.running_issue().await;
    commands::ask(&world.ctx(), KEY, "Should I proceed?", &[])
        .await
        .unwrap();
    world.settle().await;
    let wait = world.record(KEY).awaiting_reply.unwrap();
    let future = (world.now() + jiff::SignedDuration::from_secs(60)).to_string();
    world
        .run(KEY)
        .update(|r| r.awaiting_reply.as_mut().unwrap().asked_at = future.clone())
        .unwrap();

    world.message(KEY, "user-1", "Context before the question", None);
    world.settle().await;
    let record = world.record(KEY);
    assert!(record.awaiting_reply.is_none());
    assert_eq!(record.cleared_wait_id, wait.activity_id);
    let conversation = std::fs::read_to_string(world.run(KEY).conversation_md()).unwrap();
    assert!(conversation.contains("Context before the question"));
    assert!(
        record.prompt_cursor < future,
        "the test models local/server clock skew"
    );
    assert_eq!(world.fake().session("DATA-1").status, "active");
}

#[tokio::test]
async fn a_disallowed_reply_does_not_clear_a_linear_wait() {
    let mut world = World::sample();
    world.running_issue().await;
    commands::ask(&world.ctx(), KEY, "Should I proceed?", &[])
        .await
        .unwrap();
    world.settle().await;
    let wait = world.record(KEY).awaiting_reply.unwrap();

    world.message(KEY, "unapproved-user", "Proceed", None);
    world.settle().await;
    world.later(120);
    world.settle().await;

    assert_eq!(world.record(KEY).awaiting_reply, Some(wait));
    assert!(
        !world
            .text(KEY, "conversation.md")
            .contains("unapproved-user")
    );
    assert!(
        world
            .text(KEY, ".state/ignored-prompts.md")
            .contains("unapproved-user")
    );
}

#[tokio::test]
async fn a_lost_coordinator_asks_for_resume_after_an_existing_question_is_answered() {
    let mut world = World::sample();
    world.running_issue().await;
    commands::ask(&world.ctx(), KEY, "Should I proceed?", &[])
        .await
        .unwrap();
    world.settle().await;
    let workspace = world.record(KEY).coordinator.workspace_id;
    world.herdr.remove_workspace(&workspace);
    world.settle().await;
    assert!(world.record(KEY).coordinator_lost);
    assert_eq!(world.bodies(KEY, "elicitation").len(), 1);
    assert_eq!(
        world.record(KEY).awaiting_reply.unwrap().reason,
        WaitReason::CoordinatorQuestion
    );

    world.message(KEY, "user-1", "I will look at it; keep going.", None);
    world.settle().await;
    world.later(120);
    world.settle().await;
    assert_eq!(world.bodies(KEY, "elicitation").len(), 2);
    assert_eq!(
        world.record(KEY).awaiting_reply.unwrap().reason,
        WaitReason::CoordinatorLost
    );
}

#[tokio::test]
async fn a_completed_issue_closes_its_run_and_a_reopened_one_resumes() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    world.move_issue(KEY, "Done");
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Closed);
    assert_eq!(
        world.bodies(KEY, "response").last().map(String::as_str),
        Some("The issue is Done; this run is closed."),
        "the session ends"
    );
    assert_eq!(
        world.herdr.closed().len(),
        2,
        "the worker's and the coordinator's"
    );
    assert_eq!(world.worker(KEY, "w1").agent.status, AgentStatus::Stopped);
    let before = world.herdr.starts().len();
    world.later(5);
    world.settle().await;
    assert_eq!(
        world.herdr.starts().len(),
        before,
        "nothing is started for it"
    );

    world.move_issue(KEY, "Todo");
    world.later(5);
    world.settle().await;
    let reopened = world.record(KEY);
    assert_eq!(
        (reopened.status, reopened.coordinator.resume),
        (Status::Active, true)
    );
    assert!(
        ends_with(&last_args(&world), &RESUMED_WITH),
        "{:?}",
        last_args(&world)
    );
}

#[tokio::test]
async fn a_removed_delegation_detaches_the_run_and_keeps_its_workspaces() {
    let mut world = World::sample();
    world.running_issue().await;
    world.set_delegate(KEY, Value::Null);
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Detached);
    assert_eq!(world.herdr.closed(), [], "workspaces stay");

    world.set_delegate(KEY, json!({ "id": APP_USER }));
    world.later(5);
    world.settle().await;
    assert_eq!(
        world.record(KEY).status,
        Status::Active,
        "the run continues"
    );
}

// ---------------------------------------------------------------- people in Herdr

#[tokio::test]
async fn a_dialog_is_reported_in_progress_and_a_gone_coordinator_asks_for_resume() {
    let mut world = World::sample();
    world.running_issue().await;
    let w = world.start_worker("api").await;
    world.herdr.fail_next_start("agent_not_ready");
    world.settle().await;
    // Blocked for a minute and a half.
    world.later(90);
    world.settle().await;
    world.later(5);
    world.settle().await;
    let questions = world.bodies(KEY, "elicitation");
    assert!(
        questions.is_empty(),
        "pane-only dialogs do not ask in Linear"
    );
    assert!(mentions(&world.bodies(KEY, "thought"), &w.agent.pane_id));
    assert_eq!(world.herdr.notifications()[0].0, "acme/DATA-1 needs you");
    assert!(mentions(&world.inbox(KEY), "Waiting on you"));

    // A person closes the coordinator's workspace.
    let workspace = world.record(KEY).coordinator.workspace_id;
    world.herdr.remove_workspace(&workspace);
    world.settle().await;
    assert!(world.record(KEY).coordinator_lost);
    assert_eq!(world.bodies(KEY, "elicitation").len(), 1);

    world.message(KEY, "user-1", "resume", None);
    world.settle().await;
    assert!(
        ends_with(&last_args(&world), &RESUMED_WITH),
        "{:?}",
        last_args(&world)
    );
    let pane = world.record(KEY).coordinator.pane_id;
    assert!(to(&world, &pane)[0].contains("You were restarted"));
}

/// Writes `lines` records of a Claude transcript for session `id` of an
/// agent that ran in `cwd`, where Claude keeps it.
fn claude_transcript(world: &World, cwd: &str, id: &str, lines: usize) -> std::path::PathBuf {
    let folder: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = world
        .home_file(".claude/projects")
        .join(folder)
        .join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = (1..=lines)
        .map(|n| {
            format!(
                "{}\n",
                json!({"type": "user", "message": {"content": format!("line {n}")}})
            )
        })
        .collect();
    std::fs::write(&path, text).unwrap();
    path
}

fn lines_of(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |t| t.lines().count())
}

#[tokio::test]
async fn without_herdr_reports_the_plugin_names_the_claude_session_and_resumes_it() {
    let mut world = World::sample();
    world.herdr.report_no_sessions();
    world.running_issue().await;
    let args = world.herdr.starts()[0].args.clone();
    let id = args.last().unwrap().clone();
    assert_eq!(args[args.len() - 2], "--session-id");
    assert_eq!(world.record(KEY).coordinator.agent_session, id);

    let workspace = world.record(KEY).coordinator.workspace_id;
    world.herdr.remove_workspace(&workspace);
    world.settle().await;
    let asked = world.bodies(KEY, "elicitation");
    assert!(
        asked.last().unwrap().contains("with its previous session"),
        "{asked:?}"
    );
    world.message(KEY, "user-1", "resume", None);
    world.settle().await;
    let resumed = last_args(&world);
    assert!(ends_with(&resumed, &["--resume", &id]), "{resumed:?}");
    assert!(!resumed.contains(&"--session-id".to_string()));
    assert_eq!(world.record(KEY).coordinator.agent_session, id);
}

#[tokio::test]
async fn a_closed_runs_transcripts_are_kept_and_outlive_their_originals() {
    let mut world = World::sample();
    world.herdr.report_no_sessions();
    world.running_issue().await;
    let c = world.record(KEY).coordinator;
    let original = claude_transcript(&world, &c.cwd, &c.agent_session, 1);
    let copy = world
        .run(KEY)
        .transcripts_dir("coordinator")
        .join(format!("{}.jsonl", c.agent_session));
    let close_and_reopen = async |world: &mut World| {
        world.move_issue(KEY, "Canceled");
        world.later(5);
        world.settle().await;
        assert_eq!(world.record(KEY).status, Status::Closed);
        world.move_issue(KEY, "Todo");
        for _ in 0..2 {
            world.later(5);
            world.settle().await;
        }
    };
    close_and_reopen(&mut world).await;
    assert_eq!(lines_of(&copy), 1, "kept when the run closed");
    claude_transcript(&world, &c.cwd, &c.agent_session, 3);
    close_and_reopen(&mut world).await;
    assert_eq!(lines_of(&copy), 3, "the grown original replaced it");
    std::fs::remove_file(&original).unwrap();
    close_and_reopen(&mut world).await;
    assert_eq!(lines_of(&copy), 3, "left when the original is gone");
    assert_eq!(
        world.record(KEY).coordinator.agent_session,
        c.agent_session,
        "the run resumed its session each time"
    );
}

#[tokio::test]
async fn a_detached_run_and_a_restarted_worker_keep_their_transcripts() {
    let mut world = World::sample();
    world.herdr.report_no_sessions();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    let w = world.worker(KEY, "w1").agent;
    claude_transcript(&world, &w.cwd, &w.agent_session, 2);
    commands::worker_restart(&world.ctx(), &world.session(), KEY, "w1", None)
        .await
        .unwrap();
    let kept = world.run(KEY).transcripts_dir("w1");
    assert_eq!(
        lines_of(&kept.join(format!("{}.jsonl", w.agent_session))),
        2
    );
    world.settle().await;
    let restarted = world.worker(KEY, "w1").agent;
    assert_ne!(restarted.agent_session, w.agent_session, "a new session");
    assert!(uuid::Uuid::parse_str(&restarted.agent_session).is_ok());

    let c = world.record(KEY).coordinator;
    claude_transcript(&world, &c.cwd, &c.agent_session, 1);
    world.set_delegate(KEY, json!(null));
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Detached);
    let copy = world
        .run(KEY)
        .transcripts_dir("coordinator")
        .join(format!("{}.jsonl", c.agent_session));
    assert_eq!(lines_of(&copy), 1);
}

/// Adds the profile `name` (Claude, with a method in its `instructions.md`)
/// as a postmortem candidate of the `default` routing.
fn postmortem_profile(world: &World, name: &str) {
    let config = world.env.config_dir();
    let folder = config.join("profiles").join(name);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("config.toml"),
        "kind = \"claude\"\nmodel = \"haiku\"\ndescription = \"reviews runs\"\n",
    )
    .unwrap();
    std::fs::write(
        folder.join("instructions.md"),
        "Say what went well and what to change.\n",
    )
    .unwrap();
    let text = std::fs::read_to_string(config.join("config.toml")).unwrap();
    let text = if text.contains("postmortems = [") {
        text.replacen(
            "postmortems = [",
            &format!("postmortems = [\"{name}\", "),
            1,
        )
    } else {
        text.replacen(
            "workers = [\"standard\", \"deep\"]",
            &format!("workers = [\"standard\", \"deep\"]\npostmortems = [\"{name}\"]"),
            1,
        )
    };
    std::fs::write(config.join("config.toml"), text).unwrap();
}

/// Gives the `default` routing the postmortem profile `review` (labels
/// `Improvement` and `postmortem/rework`), and a fake `claude` that answers
/// routing with `routing` (a JSON object) and a postmortem with `answer`,
/// or fails it when `answer` is empty.
/// A `review` postmortem profile, and a routing agent that picks the
/// `coordinator` profile and the `pick` postmortem profile.
fn postmortem_method(world: &mut World, pick: &str, answer: &str) {
    postmortem_profile(world, "review");
    world.reload_config();
    let postmortem = if answer.is_empty() {
        "exit 1".to_string()
    } else {
        format!("echo '{{\"structured_output\":{answer}}}'")
    };
    world.router(&format!(
        "cat > /dev/null\ncase \"$*\" in\n  *'# Method'*) {postmortem} ;;\n  *'postmortem profile'*) echo '{{\"structured_output\":{{\"postmortem\":\"{pick}\"}}}}' ;;\n  *) echo '{{\"structured_output\":{{\"coordinator\":\"coordinator\"}}}}' ;;\nesac\n"
    ));
    world.fake().labels = vec!["Improvement".into(), "postmortem/rework".into()];
}

fn version_of(world: &World, name: &str) -> String {
    crate::postmortem::Method::new(
        name,
        world.config.profile(name).unwrap(),
        world.config.limits.postmortem_agent_timeout_seconds,
    )
    .version
}

fn posted(world: &World) -> Vec<String> {
    world.fake().issue("DATA-1")["posted"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| c["body"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn a_finish_and_a_close_each_get_one_postmortem_comment() {
    let mut world = World::sample();
    postmortem_method(
        &mut world,
        "review",
        r#"{"summary":"It went well.","labels":["Improvement","Bogus"]}"#,
    );
    world.running_issue().await;
    commands::finish(&world.ctx(), &world.session(), KEY, "Done.")
        .await
        .unwrap();
    world.settle().await;
    let version = version_of(&world, "review");
    assert_eq!(
        posted(&world),
        [format!(
            "**Postmortem (interim)**, method `review` version `{version}`\n\nIt went well."
        )]
    );
    let labels: Vec<String> = world.fake().issue("DATA-1")["labels"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        labels,
        ["Improvement"],
        "a label outside the method's list is dropped"
    );

    world.move_issue(KEY, "Canceled");
    world.later(5);
    world.settle().await;
    world.later(5);
    world.settle().await;
    let comments = posted(&world);
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[1].starts_with("**Postmortem (final)**"),
        "{comments:?}"
    );
    assert_eq!(world.record(KEY).closed_state, "Canceled");
    assert_eq!(world.record(KEY).postmortem_due, None);
    let kept = std::fs::read_dir(world.run(KEY).state_dir().join("postmortems"))
        .unwrap()
        .count();
    assert_eq!(kept, 2);
    let log = world.log_text();
    assert!(
        log.contains("acme/DATA-1: wrote the interim postmortem"),
        "{log}"
    );
    assert!(
        log.contains(
            "acme/DATA-1: wrote the final postmortem with the `review` profile (the only candidate)"
        ),
        "{log}"
    );
}

#[tokio::test]
async fn a_routing_with_one_candidate_each_asks_no_routing_agent() {
    let mut world = World::sample();
    postmortem_profile(&world, "review");
    let config = world.env.config_dir().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap()
        + "\n[routing.solo]\ncoordinators = [\"coordinator-light\"]\npostmortems = [\"review\"]\nworkers = [\"standard\"]\n";
    let text = text.replacen("routing = \"default\"", "routing = \"solo\"", 1);
    std::fs::write(&config, text).unwrap();
    world.reload_config();
    // A routing call fails; only the postmortem itself answers.
    world.router(
        "cat > /dev/null\ncase \"$*\" in\n  *'# Method'*) echo '{\"structured_output\":{\"summary\":\"Fine.\",\"labels\":[]}}' ;;\n  *) exit 1 ;;\nesac\n",
    );
    world.delegate(KEY, "Small", None);
    world.settle().await;
    let record = world.record(KEY);
    assert_eq!(
        (
            record.coordinator.profile.as_str(),
            record.routing_source.as_str()
        ),
        ("coordinator-light", "the only candidate")
    );
    assert_eq!(
        world.bodies(KEY, "thought")[1..],
        ["The coordinator uses the `coordinator-light` profile (the only candidate)."]
    );
    commands::finish(&world.ctx(), &world.session(), KEY, "Done.")
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(posted(&world).len(), 1);
    let log = world.log_text();
    assert!(
        log.contains("acme/DATA-1: wrote the interim postmortem with the `review` profile (the only candidate)"),
        "{log}"
    );
}

#[tokio::test]
async fn the_routing_agent_picks_among_postmortem_profiles_and_that_one_writes() {
    let mut world = World::sample();
    postmortem_profile(&world, "quick");
    postmortem_method(&mut world, "quick", r#"{"summary":"Short.","labels":[]}"#);
    world.running_issue().await;
    commands::finish(&world.ctx(), &world.session(), KEY, "Done.")
        .await
        .unwrap();
    world.settle().await;
    let version = version_of(&world, "quick");
    assert_eq!(
        posted(&world),
        [format!(
            "**Postmortem (interim)**, method `quick` version `{version}`\n\nShort."
        )]
    );
    let log = world.log_text();
    assert!(
        log.contains(
            "wrote the interim postmortem with the `quick` profile (chosen by the routing agent)"
        ),
        "{log}"
    );
}

#[tokio::test]
async fn a_failed_postmortem_is_logged_and_not_tried_again() {
    let mut world = World::sample();
    postmortem_method(&mut world, "review", "");
    world.running_issue().await;
    commands::finish(&world.ctx(), &world.session(), KEY, "Done.")
        .await
        .unwrap();
    world.settle().await;
    world.later(5);
    world.settle().await;
    assert!(posted(&world).is_empty());
    assert_eq!(world.record(KEY).postmortem_due, None);
    let log = world.log_text();
    assert_eq!(
        log.matches("acme/DATA-1: the interim postmortem failed: it exited with")
            .count(),
        1,
        "{log}"
    );
}

#[tokio::test]
async fn context_prints_the_digest_and_marks_its_items_seen() {
    let mut world = World::sample();
    world.running_issue().await;
    let id = inbox::write(&world.run(KEY), "worker", "w1", "item").unwrap();
    commands::context(&world.ctx(), Some(&world.session()), KEY)
        .await
        .unwrap();
    assert!(inbox::seen(&world.run(KEY)).contains(&id));
    commands::inbox_done(&world.ctx(), KEY, &[], true).unwrap();
    assert_eq!(world.inbox(KEY), Vec::<String>::new());
}

// ---------------------------------------------------------------- event-driven

#[tokio::test]
async fn a_ticker_started_while_herdr_is_down_judges_nothing_lost() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    let items = world.inbox(KEY).len();

    world.herdr.set_down(true);
    world.restart_ticker();
    world.message(KEY, "user-1", "Status?", None);
    world.later(60);
    world.settle().await;
    assert!(!world.record(KEY).coordinator_lost);
    assert!(world.bodies(KEY, "error").is_empty());
    assert!(
        world.text(KEY, "conversation.md").contains("Status?"),
        "Linear goes on"
    );
    assert_eq!(world.inbox(KEY).len(), items + 1, "only the reply");

    world.herdr.set_down(false);
    world.settle().await;
    let back = world.record(KEY);
    assert_eq!(
        (back.coordinator_lost, back.coordinator.pane_id),
        (false, pane)
    );
    assert!(world.bodies(KEY, "error").is_empty());
    assert!(world.bodies(KEY, "elicitation").is_empty());
}

#[tokio::test]
async fn a_worker_pane_that_shows_up_late_is_not_lost() {
    let mut world = World::sample();
    world.running_issue().await;
    world.herdr.delay_panes(3);
    let w = world.start_worker("api").await;
    for _ in 0..4 {
        world.settle().await;
    }
    assert!(world.bodies(KEY, "error").is_empty());
    assert!(!mentions(&world.inbox(KEY), "closed before"));
    let starts: Vec<PaneId> = world
        .herdr
        .starts()
        .into_iter()
        .filter(|s| s.name == "acme-data-1-w1")
        .map(|s| s.pane)
        .collect();
    assert_eq!(starts, [PaneId(w.agent.pane_id.clone())]);
    assert_eq!(to(&world, &w.agent.pane_id).len(), 1);
}

#[tokio::test]
async fn an_agent_herdr_detects_late_is_started_once() {
    let mut world = World::sample();
    world.herdr.delay_detection(3);
    world.delegate(KEY, "Fix the login", Some(2.0));
    for _ in 0..4 {
        world.settle().await;
    }
    assert_eq!(world.asked("agent.start"), 1);
    let pane = world.record(KEY).coordinator.pane_id;
    assert_eq!(to(&world, &pane), [LAUNCH]);
}

#[tokio::test]
async fn a_placement_without_an_answer_is_adopted_not_repeated() {
    let mut world = World::sample();
    world.herdr.next_placement_unknown();
    world.delegate(KEY, "Fix the login", Some(2.0));
    world.settle().await;
    let panes = world.herdr.panes();
    assert_eq!((world.asked("workspace.create"), panes.len()), (1, 1));
    let c = world.record(KEY).coordinator;
    assert_eq!(
        (c.status, c.launch_attempts, c.pane_id),
        (AgentStatus::Open, 0, panes[0].id.0.clone())
    );
    assert_eq!(world.asked("agent.start"), 1);
}

#[tokio::test]
async fn delegating_again_and_reopening_settle_without_a_loop() {
    let mut world = World::sample();
    world.running_issue().await;
    let issue_id = world.record(KEY).issue_id;

    world.set_delegate(KEY, Value::Null);
    world.later(5);
    world.settle().await;
    let detached_at = world.now();
    world.set_delegate(KEY, json!({ "id": APP_USER }));
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Active);

    // A read made before the delegation came back must not detach it again.
    world.inject(LinearEvent::RunRead {
        issue_id,
        read_started_at: detached_at,
        update: RunUpdate {
            issue: IssueStatus {
                updated_at: "2026-09-25T00:00:00.000Z".into(),
                state_type: "unstarted".into(),
                state_name: "Todo".into(),
                delegate_id: None,
            },
            prompts: Vec::new(),
        },
        detail: None,
    });
    world.settle().await;
    assert_eq!(
        world.record(KEY).status,
        Status::Active,
        "the stale read is ignored"
    );

    world.move_issue(KEY, "Canceled");
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Closed);
    world.move_issue(KEY, "Todo");
    for _ in 0..2 {
        world.later(5);
        world.settle().await;
    }
    assert_eq!(world.record(KEY).status, Status::Active);
    let again = "The issue was delegated again; the run continues.";
    assert_eq!(
        count(&world.bodies(KEY, "thought"), again),
        2,
        "once per return"
    );
    let log = world.log_text();
    assert_eq!(
        log.matches("detached (no longer delegated)").count(),
        1,
        "{log}"
    );
    assert_eq!(
        log.matches("closed (the issue is Canceled)").count(),
        1,
        "{log}"
    );
}

/// DATA-1 running, then canceled: its run is closed.
async fn closed_run(world: &mut World) {
    world.running_issue().await;
    world.move_issue(KEY, "Canceled");
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Closed);
}

#[tokio::test]
async fn a_closed_run_delegated_again_by_someone_allowed_continues() {
    let mut world = World::sample();
    closed_run(&mut world).await;
    let session = world.record(KEY).session_id;
    world.fake().redelegate_by("DATA-1", "user-1");
    world.move_issue(KEY, "Todo");
    for _ in 0..2 {
        world.later(5);
        world.settle().await;
    }
    let record = world.record(KEY);
    assert_eq!(
        (record.status, record.session_id),
        (Status::Active, session)
    );
    let again = "The issue was delegated again; the run continues.";
    assert_eq!(count(&world.bodies(KEY, "thought"), again), 1);
}

#[tokio::test]
async fn a_closed_run_delegated_again_by_someone_not_allowed_stays_closed() {
    let mut world = World::sample();
    closed_run(&mut world).await;
    world.later(5);
    world.fake().redelegate_by("DATA-1", "stranger");
    world.move_issue(KEY, "Todo");
    for _ in 0..3 {
        world.later(5);
        world.settle().await;
    }
    world.restart_ticker();
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Closed);
    assert_eq!(count(&world.bodies(KEY, "response"), DECLINED), 1);
    let again = "The issue was delegated again; the run continues.";
    assert_eq!(count(&world.bodies(KEY, "thought"), again), 0);
    let log = world.log_text();
    assert_eq!(
        log.matches("acme/DATA-1: not picked up").count(),
        1,
        "{log}"
    );
}

#[tokio::test]
async fn every_pass_repeated_at_once_changes_nothing() {
    let mut world = World::sample();
    world.every_pass_twice = true;
    let coordinator = world.running_issue().await;
    let w = world.start_worker("api").await;
    world.settle().await;
    world.report(&w, &format!("PR: {PR}\n"));
    world.settle().await;
    world.later(120);
    world.settle().await;
    world.message(KEY, "user-1", "Thanks.", None);
    world.settle().await;
    assert_eq!(world.herdr.starts().len(), 2);
    assert_eq!(to(&world, &w.agent.pane_id).len(), 1);
    assert_eq!(count(&to(&world, &coordinator), NUDGE_INBOX), 1);
    assert_eq!(world.actions(KEY), ["Start worker", "Pull request"]);
    assert_eq!(
        count(&world.bodies(KEY, "thought"), "w1 (api) wrote a report."),
        1
    );
}

#[tokio::test]
async fn a_blocked_episode_missed_between_passes_is_still_a_new_episode() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    world.herdr.set_status("acme-data-1-w1", "blocked");
    world.settle().await;
    let blocked_at = world.now();
    assert_eq!(
        world.deadline(),
        blocked_at + jiff::SignedDuration::from_secs(30)
    );
    world.later(30);
    world.settle().await;
    assert!(world.bodies(KEY, "elicitation").is_empty());
    assert!(mentions(&world.bodies(KEY, "thought"), "Worker w1"));
    assert!(!world.actions(KEY).iter().any(|a| a == "Worker waiting"));

    // Answered and blocked again with no pass in between: the snapshot
    // shows the same status with a newer sequence.
    world.herdr.set_status("acme-data-1-w1", "idle");
    world.herdr.set_status("acme-data-1-w1", "blocked");
    world.later(5);
    world.settle().await;
    assert!(
        world.bodies(KEY, "elicitation").is_empty(),
        "pane-only waiting never opens a Linear question"
    );
    assert!(mentions(&world.bodies(KEY, "thought"), "Worker w1"));
    world.later(30);
    world.settle().await;
    assert!(
        world.bodies(KEY, "elicitation").is_empty(),
        "a repeated pane dialog remains a local instruction"
    );
}

#[tokio::test]
async fn a_list_read_with_the_detaching_read_does_not_bring_the_run_back() {
    let mut world = World::sample();
    world.running_issue().await;
    world.later(5);
    // Linear dropped the delegation between the two reads of one round: the
    // delegated list still has the issue, the run read does not.
    world.inject(LinearEvent::RunRead {
        issue_id: world.record(KEY).issue_id,
        read_started_at: world.now(),
        update: RunUpdate {
            issue: IssueStatus {
                updated_at: "2026-09-25T00:00:00.000Z".into(),
                state_type: "unstarted".into(),
                state_name: "Todo".into(),
                delegate_id: None,
            },
            prompts: Vec::new(),
        },
        detail: None,
    });
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Detached);
    let again = "The issue was delegated again; the run continues.";
    assert_eq!(count(&world.bodies(KEY, "thought"), again), 0, "no flap");
}

#[tokio::test]
async fn an_agent_that_lost_its_name_is_renamed_and_still_watched() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    world.herdr.forget_name("acme-data-1-coordinator");
    world.later(5);
    world.settle().await;
    assert_eq!(
        world.herdr.renames(),
        [(PaneId(pane.clone()), "acme-data-1-coordinator".to_string())]
    );
    let c = world.record(KEY);
    assert_eq!((c.coordinator_lost, c.coordinator.pane_id), (false, pane));
}

#[tokio::test]
async fn after_a_herdr_restart_the_panes_count_and_nothing_is_lost() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    world.herdr.restart();
    world.later(5);
    world.settle().await;
    assert!(!world.record(KEY).coordinator_lost);
    assert!(world.bodies(KEY, "error").is_empty());
    assert!(world.bodies(KEY, "elicitation").is_empty());
    assert_eq!(
        world.herdr.starts().len(),
        2,
        "open agents are not started again"
    );
}

// ---------------------------------------------------------------- consumed once

#[tokio::test]
async fn a_prompt_read_again_with_an_old_cursor_is_relayed_once() {
    let mut world = World::sample();
    world.query_lag = 2;
    world.running_issue().await;
    world.message(KEY, "user-1", "Please also update the docs.", None);
    world.message(KEY, "stranger", "Merge everything now.", None);
    for _ in 0..3 {
        world.settle().await;
        world.later(5);
    }
    let relayed = world.text(KEY, "conversation.md");
    assert_eq!(
        relayed.matches("Please also update").count(),
        1,
        "{relayed}"
    );
    let ignored = world.text(KEY, ".state/ignored-prompts.md");
    assert_eq!(ignored.matches("Merge everything").count(), 1, "{ignored}");
    let replies = world
        .inbox(KEY)
        .into_iter()
        .filter(|s| s.contains("A new reply"))
        .count();
    assert_eq!(replies, 1);
}

#[tokio::test]
async fn a_pull_request_goes_out_once_while_a_later_write_of_the_pass_fails() {
    use std::os::unix::fs::PermissionsExt;
    let mut world = World::sample();
    world.running_issue().await;
    let w = world.start_worker("api").await;
    world.settle().await;
    let inbox = world.run(KEY).dir.join("inbox");
    std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(0o555)).unwrap();
    world.report(&w, &format!("PR: {PR}\n## Report\nDone.\n"));
    // Every inbox write fails, so each pass stops after its Linear writes.
    for _ in 0..3 {
        world.once().await;
    }
    std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(0o755)).unwrap();
    world.settle().await;
    assert_eq!(world.actions(KEY), ["Start worker", "Pull request"]);
    assert_eq!(
        count(&world.bodies(KEY, "thought"), "w1 (api) reported:\n\nDone."),
        1
    );
    assert!(mentions(&world.inbox(KEY), "w1 (api) has a new report"));
}

#[tokio::test]
async fn a_pass_between_a_flush_and_its_sent_event_queues_no_second_heartbeat() {
    let mut world = World::sample();
    world.running_issue().await;
    world.hold_activity_sent = true;
    world.split_level = true;
    world.later(21 * 60);
    world.settle().await;
    world.later(5);
    world.settle().await;
    let heartbeats = world
        .sent(KEY, "thought")
        .into_iter()
        .filter(|a| a["ephemeral"] == json!(true))
        .count();
    assert_eq!(heartbeats, 1);
}

#[tokio::test]
async fn a_claim_cut_short_before_its_first_thought_still_announces_it() {
    let mut world = World::sample();
    let issue_id = world.delegate(KEY, "Fix the login", Some(2.0));
    // A ticker that crashed right after creating the run folder.
    std::fs::create_dir_all(world.ctx().runs_dir()).unwrap();
    let at = world.now().to_string();
    crate::run::Run::create(
        &world.ctx().runs_dir(),
        crate::run::RunRecord {
            workspace: "acme".into(),
            issue_id,
            identifier: "DATA-1".into(),
            title: "Fix the login".into(),
            team_key: "DATA".into(),
            created: at.clone(),
            prompt_cursor: at.clone(),
            last_activity: at.clone(),
            timeout_since: at,
            announce_pending: true,
            ..crate::run::RunRecord::default()
        },
    )
    .unwrap();
    world.settle().await;
    let thoughts = world.bodies(KEY, "thought");
    assert_eq!(count(&thoughts, "Picked up DATA-1."), 1, "{thoughts:?}");
    assert_eq!(thoughts[0], "Picked up DATA-1.");
    assert!(!world.record(KEY).announce_pending);
}

#[tokio::test]
async fn a_failing_outbox_of_a_run_that_ended_raises_no_notice() {
    let mut world = World::sample();
    world.running_issue().await;
    let issue_id = world.record(KEY).issue_id;
    world.inject(LinearEvent::WritesFailing {
        issue_id,
        since: world.now(),
    });
    world.move_issue(KEY, "Done");
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Closed);
    world.later(11 * 60);
    world.settle().await;
    let notices: Vec<String> = world
        .herdr
        .notifications()
        .into_iter()
        .map(|(title, _)| title)
        .filter(|t| t == "herdr-linear-agent")
        .collect();
    assert_eq!(notices, Vec::<String>::new());
}

// ---------------------------------------------------------------- Herdr effects

#[tokio::test]
async fn a_prompt_whose_answer_is_lost_counts_as_delivered() {
    let mut world = World::sample();
    world.herdr.next_prompt_unknown();
    let pane = world.running_issue().await;
    world.later(5);
    world.settle().await;
    assert_eq!(to(&world, &pane), [LAUNCH]);

    world.herdr.next_prompt_unknown();
    let w = world.start_worker("api").await;
    world.settle().await;
    world.later(5);
    world.settle().await;
    assert_eq!(to(&world, &w.agent.pane_id).len(), 1);
    assert_eq!(world.actions(KEY), ["Start worker"]);

    inbox::write(&world.run(KEY), "worker", "w1", "item").unwrap();
    world.herdr.next_prompt_unknown();
    world.later(120);
    world.settle().await;
    world.later(5);
    world.settle().await;
    assert_eq!(count(&to(&world, &pane), NUDGE_INBOX), 1);
}

#[tokio::test]
async fn a_coordinator_prompted_in_a_pass_is_not_nudged_in_it() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    world.later(120);
    world.settle().await;
    // A coordinator idle for two minutes whose launch prompt is due again,
    // with an unseen item.
    world
        .run(KEY)
        .update(|r| r.coordinator.prompt_pending = true)
        .unwrap();
    inbox::write(&world.run(KEY), "worker", "w1", "item").unwrap();
    world.once().await;
    assert_eq!(to(&world, &pane), [LAUNCH, LAUNCH]);
}

#[tokio::test]
async fn a_stop_while_herdr_is_down_interrupts_once_herdr_is_back() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    world.herdr.set_down(true);
    world.message(KEY, "user-1", "", Some("stop"));
    world.settle().await;
    assert!(world.record(KEY).stopped);
    assert!(world.bodies(KEY, "response").is_empty(), "no count yet");

    world.herdr.set_down(false);
    world.later(5);
    world.settle().await;
    let keys: Vec<String> = world.herdr.keys().into_iter().map(|(_, k)| k).collect();
    assert_eq!(keys, ["esc", "esc"]);
    let answer = world.bodies(KEY, "response");
    assert_eq!(answer.len(), 1);
    assert!(answer[0].starts_with("Stopped 2 agent(s)"), "{answer:?}");
}

#[tokio::test]
async fn a_detach_while_herdr_is_down_interrupts_once_herdr_is_back() {
    let mut world = World::sample();
    world.running_issue().await;
    world.herdr.set_down(true);
    world.set_delegate(KEY, Value::Null);
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Detached);
    world.herdr.set_down(false);
    world.later(5);
    world.settle().await;
    let keys: Vec<String> = world.herdr.keys().into_iter().map(|(_, k)| k).collect();
    assert_eq!(keys, ["esc"]);
    assert!(world.bodies(KEY, "response").is_empty());
}

#[tokio::test]
async fn a_start_herdr_never_detects_counts_as_an_attempt() {
    let mut world = World::sample();
    world.herdr.delay_detection(1_000_000);
    world.delegate(KEY, "Fix the login", Some(2.0));
    world.settle().await;
    for _ in 0..8 {
        world.later(61);
        world.settle().await;
    }
    assert_eq!(world.asked("agent.start"), 3);
    let c = world.record(KEY).coordinator;
    assert_eq!((c.status, c.launch_attempts), (AgentStatus::Failed, 3));
    let errors = world.bodies(KEY, "error");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].starts_with("Could not start the coordinator agent: "),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_start_herdr_never_received_waits_fifteen_seconds_whatever_the_attempts() {
    let mut world = World::sample();
    world.herdr.fail_next_start("startup_failed");
    world.delegate(KEY, "Fix the login", Some(2.0));
    world.settle().await;
    world.later(15);
    world.herdr.fail_next_start("startup_failed");
    world.settle().await;
    assert_eq!(world.record(KEY).coordinator.launch_attempts, 2);
    world.later(30);
    world.herdr.next_start_not_sent();
    world.settle().await;
    assert_eq!(
        world.herdr.starts().len(),
        2,
        "the dropped start is not seen"
    );
    world.later(15);
    world.settle().await;
    assert_eq!(world.herdr.starts().len(), 3);
    assert_eq!(world.record(KEY).coordinator.launch_attempts, 2);
}

#[tokio::test]
async fn a_placement_without_an_answer_is_adopted_after_a_title_edit() {
    let mut world = World::sample();
    world.herdr.next_placement_unknown();
    world.delegate(KEY, "Fix the login", Some(2.0));
    while world.asked("workspace.create") == 0 {
        world.once().await;
    }
    // The title changes while the answer is lost, and with it the label.
    world
        .run(KEY)
        .update(|r| r.title = "Fix the login page".into())
        .unwrap();
    world.settle().await;
    let panes = world.herdr.panes();
    assert_eq!((world.asked("workspace.create"), panes.len()), (1, 1));
    let c = world.record(KEY).coordinator;
    assert_eq!(
        (c.status, c.pane_id),
        (AgentStatus::Open, panes[0].id.0.clone())
    );
    assert_eq!(to(&world, &panes[0].id.0), [LAUNCH]);
}

// ---------------------------------------------------------------- races

#[tokio::test]
async fn a_restart_between_a_snapshot_and_its_pass_keeps_the_new_pane() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    let again = world.restart_worker_mid_pass("w1").await;
    world.settle().await;
    world.later(5);
    world.settle().await;
    assert!(
        world.bodies(KEY, "error").is_empty(),
        "{:?}",
        world.bodies(KEY, "error")
    );
    assert!(!mentions(&world.inbox(KEY), "closed before"));
    let w = world.worker(KEY, "w1");
    assert_eq!(w.agent.pane_id, again.agent.pane_id);
    assert_eq!(
        to(&world, &w.agent.pane_id).len(),
        1,
        "started and prompted"
    );
}

#[tokio::test]
async fn a_worker_without_a_pane_is_never_judged_gone() {
    let mut world = World::sample();
    world.running_issue().await;
    let w = world.start_worker("api").await;
    world.settle().await;
    // A restart has cleared the pane and closed the workspace; the new one
    // is not open yet.
    world.run(KEY).update(|_| ()).unwrap();
    crate::worker::update(&world.run(KEY), "w1", |w| {
        w.agent.pane_id.clear();
        w.agent.prompt_pending = true;
    })
    .unwrap();
    world.herdr.remove_workspace(&w.agent.workspace_id);
    world.later(5);
    world.settle().await;
    assert!(world.bodies(KEY, "error").is_empty());
    assert!(!mentions(&world.inbox(KEY), "closed before"));
}

#[tokio::test]
async fn a_start_result_for_a_pane_the_worker_left_is_dropped() {
    let mut world = World::sample();
    world.running_issue().await;
    let first = world.start_worker("api").await;
    world.herdr.fail_next_start("startup_failed");
    world.once().await;
    // The start in the first pane is out when the coordinator restarts it.
    let again = commands::worker_restart(&world.ctx(), &world.session(), KEY, "w1", None)
        .await
        .unwrap();
    assert_ne!(again.agent.pane_id, first.agent.pane_id);
    world.settle().await;
    let w = world.worker(KEY, "w1");
    assert_eq!(w.agent.launch_attempts, 0);
    let in_new: Vec<PaneId> = world
        .herdr
        .starts()
        .into_iter()
        .map(|s| s.pane)
        .filter(|p| p.0 == again.agent.pane_id)
        .collect();
    assert_eq!(in_new.len(), 1);
}

// ---------------------------------------------------------------- trust and lifetime

#[tokio::test]
async fn panes_left_out_of_a_partly_parsed_snapshot_are_not_judged() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    let w = world.start_worker("api").await;
    world.settle().await;
    let state = world.ctx().state_dir();
    self_report(&world, &w, "Reading");
    world.herdr.unparsed(&pane, true);
    world.herdr.unparsed(&w.agent.pane_id, true);
    world.later(5);
    world.settle().await;
    assert!(!world.record(KEY).coordinator_lost);
    assert!(world.bodies(KEY, "elicitation").is_empty());
    assert!(world.bodies(KEY, "error").is_empty());
    assert!(!mentions(&world.inbox(KEY), "closed before"));
    assert!(crate::progress::load(&state, super::world::SOCKET, &w.agent.pane_id).is_some());

    world.herdr.unparsed(&pane, false);
    world.herdr.unparsed(&w.agent.pane_id, false);
    world.later(5);
    world.settle().await;
    assert!(!world.record(KEY).coordinator_lost);
    assert_eq!(world.herdr.starts().len(), 2, "nothing is started again");
}

#[tokio::test]
async fn an_agent_whose_entry_does_not_parse_is_not_started_again() {
    let mut world = World::sample();
    world.running_issue().await;
    let w = world.start_worker("api").await;
    // The started agent is there, but its one entry does not parse.
    world.herdr.unparsed_agent(&w.agent.pane_id, true);
    world.settle().await;
    for _ in 0..3 {
        world.later(61);
        world.settle().await;
    }
    let starts = |world: &World| {
        world
            .herdr
            .starts()
            .into_iter()
            .filter(|s| s.name == "acme-data-1-w1")
            .count()
    };
    assert_eq!(starts(&world), 1);
    assert_eq!(world.worker(KEY, "w1").agent.launch_attempts, 0);
    assert!(
        to(&world, &w.agent.pane_id).is_empty(),
        "not found, not prompted"
    );

    world.herdr.unparsed_agent(&w.agent.pane_id, false);
    world.later(5);
    world.settle().await;
    assert_eq!(starts(&world), 1);
    assert_eq!(to(&world, &w.agent.pane_id).len(), 1);
}

fn self_report(world: &World, w: &worker::Worker, activity: &str) {
    crate::progress::save(
        &world.ctx().state_dir(),
        &crate::progress::Record {
            socket: super::world::SOCKET.into(),
            pane_id: w.agent.pane_id.clone(),
            // The fake's pane `w<n>:p1` runs terminal `term-<n>`.
            terminal_id: w.agent.pane_id.replace(":p1", "").replace('w', "term-"),
            activity: activity.into(),
            reported_at: world.now().as_second(),
            ..crate::progress::Record::default()
        },
    )
    .unwrap();
}

fn ephemeral_thoughts(world: &World) -> Vec<String> {
    world
        .sent(KEY, "thought")
        .iter()
        .filter(|a| a["ephemeral"] == true)
        .map(|a| a["content"]["body"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_workers_activity_goes_to_linear_once_and_the_heartbeat_says_how_long() {
    let mut world = World::sample();
    world.running_issue().await;
    let w = world.start_worker("api").await;
    world.settle().await;
    world.herdr.set_status("acme-data-1-w1", "working");
    self_report(&world, &w, "Running tests");
    world.settle().await;
    world.later(30);
    world.settle().await;
    assert_eq!(ephemeral_thoughts(&world), ["w1 (api): Running tests"]);

    world.later(12 * 60);
    world.settle().await;
    assert_eq!(
        ephemeral_thoughts(&world),
        [
            "w1 (api): Running tests",
            "Still on it. w1 (api): Running tests, for 12 min."
        ]
    );

    self_report(&world, &w, crate::progress::WAITING);
    world.herdr.set_status("acme-data-1-w1", "idle");
    world.settle().await;
    assert_eq!(
        ephemeral_thoughts(&world).len(),
        2,
        "no activity for a question"
    );
    assert_eq!(
        world.sent(KEY, "action").last().unwrap()["content"]["parameter"],
        "w1 (api): it asked a question in its report"
    );
    assert_eq!(world.actions(KEY), ["Start worker", "Worker waiting"]);
}

#[tokio::test]
async fn a_waiting_self_report_wakes_the_ticker_when_it_expires() {
    let mut world = World::sample();
    world.running_issue().await;
    let w = world.start_worker("api").await;
    world.settle().await;
    world.later(120);
    world.settle().await;
    crate::progress::save(
        &world.ctx().state_dir(),
        &crate::progress::Record {
            socket: super::world::SOCKET.into(),
            pane_id: w.agent.pane_id.clone(),
            activity: crate::progress::WAITING.into(),
            reported_at: world.now().as_second() - WAITING_TOO_LONG,
            ..crate::progress::Record::default()
        },
    )
    .unwrap();
    world.settle().await;
    let expiry = worker::SELF_REPORT_SECS - WAITING_TOO_LONG;
    assert_eq!(
        world.deadline(),
        world.now() + jiff::SignedDuration::from_secs(expiry)
    );
}

#[tokio::test]
async fn a_routing_agent_that_never_reads_its_input_times_out() {
    let mut world = World::with(limit("routing_agent_timeout_seconds", 1));
    world.router("sleep 30\n");
    world.delegate(KEY, "Huge", None);
    world.fake().issue_mut("DATA-1")["description"] = json!("x".repeat(1 << 20));
    world.settle().await;
    let routed = world.record(KEY);
    assert_eq!(
        (
            routed.coordinator.profile.as_str(),
            routed.routing_source.as_str()
        ),
        (
            "coordinator",
            "the first candidate: the routing agent timed out"
        )
    );
}

#[tokio::test]
async fn the_reconciler_forgets_what_ended_runs_and_gone_panes_left() {
    let mut world = World::sample();
    world.running_issue().await;
    world.start_worker("api").await;
    world.settle().await;
    inbox::write(&world.run(KEY), "worker", "w1", "item").unwrap();
    world.later(120);
    world.settle().await;
    world.move_issue(KEY, "Done");
    world.later(5);
    world.settle().await;
    world.later(5);
    world.settle().await;
    assert_eq!(world.record(KEY).status, Status::Closed);
    let kept: Vec<(&str, usize)> = world
        .remembered()
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .collect();
    assert_eq!(kept, []);
}

#[tokio::test]
async fn the_reconciler_leaves_an_unreadable_outbox_file_to_the_linear_task() {
    let mut world = World::sample();
    world.running_issue().await;
    world.move_issue(KEY, "Done");
    world.later(5);
    world.settle().await;
    // A closed run's outbox is listed to decide whether it is flushed.
    let outbox = world.run(KEY).state_dir().join("outbox");
    std::fs::write(outbox.join("9999999999.json"), "not json").unwrap();
    world.pass_alone().await;
    assert!(outbox.join("9999999999.json").is_file());
    world.settle().await;
    assert!(outbox.join("failed/9999999999.json").is_file());
}

#[tokio::test]
async fn a_run_under_every_lag_knob_writes_each_fact_once() {
    let mut world = World::sample();
    world.query_lag = 2;
    world.hold_activity_sent = true;
    world.split_level = true;
    world.every_pass_twice = true;
    let coordinator = world.running_issue().await;
    let w = world.start_worker("api").await;
    world.settle().await;
    world.report(&w, &format!("PR: {PR}\n"));
    world.settle().await;
    world.message(KEY, "user-1", "Thanks.", None);
    world.settle().await;
    world.later(21 * 60);
    world.settle().await;
    world.later(5);
    world.settle().await;
    assert_eq!(to(&world, &coordinator)[0], LAUNCH);
    assert_eq!(count(&to(&world, &coordinator), LAUNCH), 1);
    assert_eq!(to(&world, &w.agent.pane_id).len(), 1);
    assert_eq!(world.actions(KEY), ["Start worker", "Pull request"]);
    assert_eq!(
        count(&world.bodies(KEY, "thought"), "w1 (api) wrote a report."),
        1
    );
    assert_eq!(count(&world.bodies(KEY, "thought"), "Picked up DATA-1."), 1);
    let relayed = world.text(KEY, "conversation.md");
    assert_eq!(relayed.matches("Thanks.").count(), 1);
    let heartbeats = world
        .sent(KEY, "thought")
        .into_iter()
        .filter(|a| a["ephemeral"] == json!(true))
        .count();
    assert_eq!(heartbeats, 1);
    assert_eq!(world.sessions(), 1);
}

#[tokio::test]
async fn a_coordinator_seen_again_in_its_pane_is_no_longer_lost_and_gets_its_reply() {
    let mut world = World::sample();
    let pane = world.running_issue().await;
    world
        .run(KEY)
        .update(|r| r.coordinator_lost = true)
        .unwrap();

    world.message(KEY, "user-1", "Try again, please.", None);
    world.later(120);
    world.settle().await;
    assert!(!world.record(KEY).coordinator_lost);
    assert_eq!(
        to(&world, &pane).last().map(String::as_str),
        Some(NUDGE_REPLY)
    );
}

#[tokio::test]
async fn a_worker_idle_right_after_its_launch_prompt_is_not_reported_idle() {
    let mut world = World::sample();
    world.running_issue().await;
    let w = world.start_worker("api").await;
    world.settle().await;
    assert_eq!(
        to(&world, &w.agent.pane_id),
        ["Read .herdr-linear-agent/acme-DATA-1-w1/brief.md and do what it says."]
    );
    let idle = |world: &World| mentions(&world.inbox(KEY), "is idle without a report");
    assert!(!idle(&world), "the prompt has not been picked up yet");

    world.later(30);
    world.settle().await;
    assert!(!idle(&world), "still inside the minute after the prompt");

    world.later(31);
    world.settle().await;
    assert!(idle(&world), "a minute without any change is idle");
}

#[tokio::test]
async fn only_a_person_editing_the_issue_writes_an_issue_item() {
    let mut world = World::sample();
    world.running_issue().await;
    let issue_items = |world: &World| {
        world
            .inbox(KEY)
            .iter()
            .filter(|i| i.contains("The issue was edited in Linear"))
            .count()
    };
    assert_eq!(
        issue_items(&world),
        0,
        "the agent's activities are not edits"
    );

    world.message(KEY, "user-1", "Please also add a README.", None);
    world.later(5);
    world.settle().await;
    assert!(world.text(KEY, "conversation.md").contains("add a README"));
    assert_eq!(
        issue_items(&world),
        0,
        "a reply in the session is not an edit"
    );
    assert!(
        world
            .text(KEY, "issue.md")
            .contains("Please also add a README."),
        "issue.md still lists every comment"
    );

    world
        .fake()
        .add_comment("DATA-1", "someone", "Note the API change in #12.");
    world.later(5);
    world.settle().await;
    assert_eq!(issue_items(&world), 1, "a person's comment on the issue is");

    world.fake().issue_mut("DATA-1")["description"] = json!("A new description.");
    world.fake().issue_mut("DATA-1")["updatedAt"] = json!("2027-01-01T00:00:00.000Z");
    world.later(5);
    world.settle().await;
    assert_eq!(issue_items(&world), 2, "an edited description is");
}
