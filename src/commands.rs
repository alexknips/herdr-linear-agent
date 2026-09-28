//! The subcommands a coordinator runs. Each is one deterministic mechanic:
//! the coordinator decides whether, what and where; these commands check names
//! against the config, enforce the limits, and queue Linear writes for the
//! ticker. None of them reads the Keychain.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{Config, Limits, Repository};
use crate::coordinator::{self, View};
use crate::herdr::{self, AgentStatus as HerdrStatus, Herdr, Placed, WorkspaceId};
use crate::linear::api::{Activity, Content};
use crate::outbox::{self, Op, StateTarget};
use crate::paths::Ctx;
use crate::process::{Cmd, Runner};
use crate::run::{AgentStatus, Run, RunRecord, Status};
use crate::worker::{self, Group, Worker};
use crate::{files, inbox, names, ticker};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const UNREACHABLE: &str = "the configured Herdr session is not reachable";

/// The configured Herdr session: a client and the socket path its panes see.
pub struct Session<H> {
    pub herdr: H,
    pub socket: String,
}

impl Session<herdr::Client> {
    pub async fn configured(ctx: &Ctx<'_>) -> Result<Self> {
        let config = Config::load(&ctx.config_dir())?;
        let socket = herdr::session_socket(&ctx.env.herdr_bin(), config.herdr.session.as_deref())
            .await
            .context(UNREACHABLE)?;
        Ok(Session {
            socket: socket.to_string_lossy().into_owned(),
            herdr: herdr::Client::new(socket),
        })
    }
}

async fn load_active(ctx: &Ctx<'_>, key: &str) -> Result<(Config, Run, RunRecord)> {
    // Every command an agent runs keeps the ticker alive.
    ticker::start(ctx).await?;
    let config = Config::load(&ctx.config_dir())?;
    let run = Run::load(&ctx.runs_dir(), key)?;
    let record = run.record()?;
    if record.status != Status::Active {
        bail!(
            "run {key} is {:?}; nothing more is done for it",
            record.status
        );
    }
    Ok((config, run, record))
}

fn non_empty(text: &str, what: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("the {what} is empty");
    }
    Ok(())
}

pub fn skill(key: Option<&str>) -> Result<()> {
    print!(
        "{}",
        coordinator::sheet(
            &coordinator::binary_command()?,
            key.unwrap_or("<ISSUE-KEY>")
        )
    );
    Ok(())
}

pub async fn context<H: Herdr>(
    ctx: &Ctx<'_>,
    session: Option<&Session<H>>,
    key: &str,
) -> Result<()> {
    ticker::start(ctx).await?;
    let config = Config::load(&ctx.config_dir())?;
    let run = Run::load(&ctx.runs_dir(), key)?;
    let state_dir = ctx.state_dir();
    let snapshot = match session {
        Some(session) => session.herdr.snapshot().await.ok(),
        None => None,
    };
    let view = snapshot
        .as_ref()
        .zip(session)
        .map(|(snapshot, session)| View {
            snapshot,
            state_dir: &state_dir,
            socket: &session.socket,
            now: jiff::Timestamp::now(),
        });
    let rows = coordinator::worker_rows(&run, view.as_ref());
    let (text, shown) = coordinator::digest(&run, &config, &coordinator::binary_command()?, &rows)?;
    print!("{text}");
    inbox::mark_seen(&run, &shown)
}

pub fn inbox_done(ctx: &Ctx, key: &str, ids: &[String], all: bool) -> Result<()> {
    let run = Run::load(&ctx.runs_dir(), key)?;
    if ids.is_empty() && !all {
        bail!("name the inbox item ids, or pass --all");
    }
    let moved = inbox::done(&run, ids, all)?;
    ticker::poke(&ctx.state_dir());
    println!("{}", done_message(&run, moved));
    Ok(())
}

/// `--all` leaves the items written after the last `context`; the message
/// says how many, so the coordinator reads them before it ends its turn.
fn done_message(run: &Run, moved: usize) -> String {
    let seen = inbox::seen(run);
    let new = inbox::unhandled(run)
        .iter()
        .filter(|item| !seen.contains(&item.id))
        .count();
    if new == 0 {
        format!("{moved} item(s) handled")
    } else {
        format!("{moved} item(s) handled; {new} new item(s) since your last context, run context")
    }
}

pub async fn plan_set(ctx: &Ctx<'_>, key: &str, text: &str) -> Result<()> {
    let (_, run, _) = load_active(ctx, key).await?;
    let plan = outbox::parse_plan(text)?;
    outbox::push(&run, Op::Plan { plan })?;
    ticker::poke(&ctx.state_dir());
    println!("the plan is queued for Linear");
    Ok(())
}

pub async fn say(ctx: &Ctx<'_>, key: &str, text: &str) -> Result<()> {
    non_empty(text, "text")?;
    let (_, run, _) = load_active(ctx, key).await?;
    outbox::push(
        &run,
        Op::Activity {
            activity: Activity::new(Content::Thought {
                body: text.trim().to_string(),
            }),
        },
    )?;
    ticker::poke(&ctx.state_dir());
    println!("queued for the Linear session");
    Ok(())
}

/// `--option label=value`, or a bare label used as its own value.
pub fn parse_option(text: &str) -> Result<(String, String)> {
    let (label, value) = text.split_once('=').unwrap_or((text, text));
    let (label, value) = (label.trim(), value.trim());
    if label.is_empty() || value.is_empty() {
        bail!("an option looks like `label=value`; got `{text}`");
    }
    Ok((label.to_string(), value.to_string()))
}

pub async fn ask(ctx: &Ctx<'_>, key: &str, text: &str, options: &[(String, String)]) -> Result<()> {
    non_empty(text, "question")?;
    let (_, run, _) = load_active(ctx, key).await?;
    let options: Vec<_> = options
        .iter()
        .map(|(label, value)| (label.as_str(), value.as_str()))
        .collect();
    outbox::push(&run, Op::elicitation(text.trim(), &options))?;
    ticker::poke(&ctx.state_dir());
    println!(
        "the question is queued for the Linear session; end your turn, the answer arrives in your inbox"
    );
    Ok(())
}

/// Posts the final summary and moves the issue to review, once every worker
/// has reported and none is working or waiting.
pub async fn finish<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    text: &str,
) -> Result<()> {
    non_empty(text, "summary")?;
    let (config, run, _) = load_active(ctx, key).await?;
    let snapshot = session.herdr.snapshot().await.context(UNREACHABLE)?;
    let now = jiff::Timestamp::now();
    let state_dir = ctx.state_dir();
    let blocking: Vec<String> = worker::list(&run)
        .into_iter()
        .filter(|w| {
            matches!(
                w.agent.status,
                AgentStatus::Open | AgentStatus::Failed | AgentStatus::Pending
            )
        })
        .map(|w| {
            let live = worker::live_state(&w.agent, &snapshot, now, &state_dir, &session.socket);
            (worker::group(&w, &live), w)
        })
        .filter(|(group, _)| *group != Group::Reported)
        .map(|(group, w)| format!("{} is {}", w.id, group.label()))
        .collect();
    if !blocking.is_empty() {
        bail!(
            "not finished: {}. Every worker must have written a report and be neither working nor waiting",
            blocking.join(", ")
        );
    }
    outbox::push(
        &run,
        Op::Activity {
            activity: Activity::new(Content::Response {
                body: text.trim().to_string(),
            }),
        },
    )?;
    outbox::push(
        &run,
        Op::IssueState {
            target: StateTarget::Review,
        },
    )?;
    run.update(|r| r.finished = true)?;
    ticker::poke(&ctx.state_dir());
    println!(
        "the summary is queued; the issue moves to `{}`",
        config.linear.review_state
    );
    Ok(())
}

/// Adds the brief folder to the repository's shared `info/exclude`, so every
/// worktree of it ignores the worker's brief and report.
pub async fn exclude_from_git(runner: &dyn Runner, cwd: &str) -> Result<()> {
    let out = runner
        .run(&Cmd::new("git", GIT_TIMEOUT).args(["-C", cwd, "rev-parse", "--git-common-dir"]))
        .await?;
    if !out.success() {
        bail!("git rev-parse failed: {}", out.error_text());
    }
    let common = Path::new(out.stdout.trim());
    let exclude = Path::new(cwd).join(common).join("info/exclude");
    let line = format!("{}/", worker::BRIEF_FOLDER);
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    if text.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&line);
    text.push('\n');
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)?;
    }
    files::write_atomic(&exclude, text.as_bytes())
}

async fn fetch(runner: &dyn Runner, repo: &Repository) -> Result<()> {
    let path = repo.path.to_string_lossy();
    let out = runner
        .run(&Cmd::new("git", FETCH_TIMEOUT).args(["-C", &path, "fetch", "origin", &repo.base]))
        .await
        .context("git fetch failed")?;
    if !out.success() {
        bail!("git fetch failed: {}", out.error_text());
    }
    Ok(())
}

/// The path of the worktree that has `branch` checked out, from
/// `git worktree list --porcelain` (blocks of `worktree <path>` and
/// `branch refs/heads/<name>` lines).
fn worktree_of(listing: &str, branch: &str) -> Option<String> {
    let wanted = format!("branch refs/heads/{branch}");
    listing.split("\n\n").find_map(|block| {
        let path = block.lines().find_map(|l| l.strip_prefix("worktree "))?;
        block
            .lines()
            .any(|l| l.trim() == wanted)
            .then(|| path.to_string())
    })
}

async fn find_worktree(runner: &dyn Runner, repo_path: &str, branch: &str) -> Option<String> {
    let out = runner
        .run(&Cmd::new("git", GIT_TIMEOUT).args([
            "-C",
            repo_path,
            "worktree",
            "list",
            "--porcelain",
        ]))
        .await
        .ok()?;
    out.success().then(|| worktree_of(&out.stdout, branch))?
}

/// `worktree.create`, except that a creation whose answer was lost is
/// found by its branch and opened instead of failing or creating a second.
async fn create_worktree<H: Herdr>(
    runner: &dyn Runner,
    herdr: &H,
    repo_path: &str,
    branch: &str,
    base: &str,
) -> Result<Placed, herdr::HerdrError> {
    match herdr.worktree_create(repo_path, branch, base).await {
        Err(herdr::HerdrError::OutcomeUnknown(detail)) => {
            match find_worktree(runner, repo_path, branch).await {
                Some(path) => herdr.worktree_open(repo_path, &path).await,
                None => Err(herdr::HerdrError::OutcomeUnknown(detail)),
            }
        }
        other => other,
    }
}

pub struct WorkerStart {
    pub repo: String,
    pub profile: String,
    pub title: String,
    pub task: String,
}

/// Workers that are not stopped count for one worker per repository and for
/// `max_workers_per_run`.
fn check_room(workers: &[Worker], repo: &str, limits: Limits) -> Result<()> {
    let live: Vec<&Worker> = workers
        .iter()
        .filter(|w| w.agent.status != AgentStatus::Stopped)
        .collect();
    if let Some(other) = live.iter().find(|w| w.repo == repo) {
        bail!(
            "{} already works on `{repo}`: one worker per repository; prompt or restart it instead",
            other.id
        );
    }
    if live.len() >= limits.max_workers_per_run as usize {
        bail!(
            "the run already has {} worker(s); max_workers_per_run is {}",
            live.len(),
            limits.max_workers_per_run
        );
    }
    Ok(())
}

fn check_agents(ctx: &Ctx, limits: Limits) -> Result<()> {
    let count = worker::agent_count(&Run::list(&ctx.runs_dir()));
    if count + 1 > limits.max_agents as usize {
        bail!("the limit of {} agents is reached", limits.max_agents);
    }
    Ok(())
}

/// Places a worker in a new worktree of a catalog repository. The ticker
/// starts its agent in a later pass.
pub async fn worker_start<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    args: &WorkerStart,
) -> Result<Worker> {
    let (config, run, record) = load_active(ctx, key).await?;
    let repo = config.repository(&args.repo)?.clone();
    let profile = config.worker_profile(&args.profile)?.clone();
    let limits = config.limits;
    check_room(&worker::list(&run), &args.repo, limits)?;
    check_agents(ctx, limits)?;
    fetch(ctx.runner, &repo).await?;
    let repo_path = repo.path.to_string_lossy().into_owned();
    let worker = worker::allocate(
        &run,
        |workers| {
            check_room(workers, &args.repo, limits)?;
            check_agents(ctx, limits)
        },
        |w| {
            w.title = args.title.clone();
            w.repo = args.repo.clone();
            w.repo_path = repo_path.clone();
            w.base = repo.base.clone();
            w.branch = worker::branch_name(key, &w.id, &args.title);
            w.agent.status = AgentStatus::Pending;
            w.agent.profile = args.profile.clone();
            w.agent.kind = profile.kind.clone();
            w.agent.agent_name = names::agent_name(key, &record.issue_id, &w.id);
        },
    )?;
    let base = format!("origin/{}", repo.base);
    let placed = match create_worktree(
        ctx.runner,
        &session.herdr,
        &repo_path,
        &worker.branch,
        &base,
    )
    .await
    {
        Ok(placed) => placed,
        Err(error) => {
            worker::update(&run, &worker.id, |w| {
                w.agent.status = AgentStatus::Failed;
                w.agent.error = format!("could not create the worktree: {error}");
            })?;
            ticker::poke(&ctx.state_dir());
            bail!("could not create the worktree for {}: {error}", worker.id);
        }
    };
    let instructions = profile.instructions.as_deref();
    let worker = place(
        ctx,
        &run,
        &record,
        worker,
        &placed,
        args.task.trim(),
        false,
        instructions,
    )
    .await?;
    ticker::poke(&ctx.state_dir());
    println!(
        "{} is placed in {} on branch {}; herdr-linear-agent starts its agent shortly",
        worker.id, worker.worktree_path, worker.branch
    );
    Ok(worker)
}

/// Writes the task and the brief into the new pane's worktree and records
/// the placement. The agent's own fields are reset for a fresh launch.
#[allow(clippy::too_many_arguments)]
async fn place(
    ctx: &Ctx<'_>,
    run: &Run,
    record: &RunRecord,
    mut worker: Worker,
    placed: &Placed,
    task: &str,
    restart: bool,
    instructions: Option<&str>,
) -> Result<Worker> {
    let worktree = placed
        .worktree_path
        .clone()
        .unwrap_or_else(|| placed.cwd.clone());
    // Only the brief folder goes unignored without it; the worker still works.
    let _ = exclude_from_git(ctx.runner, &worktree).await;
    worker.worktree_path = worktree.clone();
    worker.brief_dir = worker::brief_dir(&worktree, &run.key, &worker.id);
    if !restart {
        files::write_atomic(
            &worker::task_path(run, &worker.id),
            format!("{task}\n").as_bytes(),
        )?;
    }
    std::fs::create_dir_all(&worker.brief_dir)
        .with_context(|| format!("could not create {}", worker.brief_dir))?;
    let brief = worker::compose_brief(&worker::BriefInput {
        issue_key: &run.key,
        issue_title: &record.title,
        issue_url: &record.url,
        worker: &worker,
        task,
        restart,
        binary: &coordinator::binary_command()?,
        instructions,
    });
    files::write_atomic(
        &Path::new(&worker.brief_dir).join("brief.md"),
        brief.as_bytes(),
    )?;
    worker::update(run, &worker.id, |w| {
        w.worktree_path = worker.worktree_path.clone();
        w.brief_dir = worker.brief_dir.clone();
        w.agent.placed(placed);
    })
}

/// Sends a worker a follow-up and records it in its task file.
pub async fn worker_prompt<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    id: &str,
    text: &str,
) -> Result<()> {
    non_empty(text, "text")?;
    let (_, run, _) = load_active(ctx, key).await?;
    let w = worker::load(&run, id)?;
    if w.agent.status != AgentStatus::Open || w.agent.pane_id.is_empty() {
        bail!("worker {id} is not running");
    }
    let snapshot = session.herdr.snapshot().await.context(UNREACHABLE)?;
    let live = worker::live_state(
        &w.agent,
        &snapshot,
        jiff::Timestamp::now(),
        &ctx.state_dir(),
        &session.socket,
    );
    let Some(agent) = live.agent.as_ref().filter(|_| live.pane_exists) else {
        bail!("worker {id} is not running");
    };
    if agent.status == HerdrStatus::Blocked {
        bail!(
            "worker {id} waits on a dialog in pane {}; a person must answer it in Herdr first",
            agent.pane
        );
    }
    worker::append_follow_up(&run, id, text)?;
    session
        .herdr
        .agent_prompt(&agent.pane, text.trim())
        .await
        .with_context(|| format!("could not prompt worker {id}"))?;
    ticker::poke(&ctx.state_dir());
    println!("the follow-up is sent to {id}");
    Ok(())
}

/// The restarted worker's new pane: its kept worktree opened again, or a
/// worktree placed from its base when it never had one.
async fn reopen<H: Herdr>(
    ctx: &Ctx<'_>,
    config: &Config,
    session: &Session<H>,
    w: &Worker,
) -> Result<Placed> {
    let id = &w.id;
    if !w.worktree_path.is_empty() {
        if !w.agent.workspace_id.is_empty() {
            // The checkout stays; a workspace that is already gone is fine.
            let _ = session
                .herdr
                .workspace_close(&WorkspaceId(w.agent.workspace_id.clone()))
                .await;
        }
        return session
            .herdr
            .worktree_open(&w.repo_path, &w.worktree_path)
            .await
            .with_context(|| format!("could not open the worktree of {id}"));
    }
    let repo = config.repository(&w.repo)?.clone();
    let repo_path = repo.path.to_string_lossy().into_owned();
    match find_worktree(ctx.runner, &repo_path, &w.branch).await {
        // A creation whose answer was lost left the checkout behind.
        Some(path) => session
            .herdr
            .worktree_open(&repo_path, &path)
            .await
            .with_context(|| format!("could not open the worktree of {id}")),
        // The worktree was never created: place it again from its base.
        None => {
            fetch(ctx.runner, &repo).await?;
            let base = format!("origin/{}", repo.base);
            create_worktree(ctx.runner, &session.herdr, &repo_path, &w.branch, &base)
                .await
                .with_context(|| format!("could not create the worktree for {id}"))
        }
    }
}

/// Starts a worker again in its worktree, optionally with another profile.
pub async fn worker_restart<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    id: &str,
    profile: Option<&str>,
) -> Result<Worker> {
    let (config, run, record) = load_active(ctx, key).await?;
    let w = worker::load(&run, id)?;
    if w.restarts >= worker::MAX_RESTARTS {
        bail!(
            "worker {id} was restarted {} times; the limit is {}",
            w.restarts,
            worker::MAX_RESTARTS
        );
    }
    let (profile_name, profile) = match profile {
        Some(name) => (name.to_string(), config.worker_profile(name)?.clone()),
        None => (
            w.agent.profile.clone(),
            config.profile(&w.agent.profile)?.clone(),
        ),
    };
    if !w.counts() {
        check_agents(ctx, config.limits)?;
    }
    // Before the old workspace closes, so a pass woken by the close finds
    // a worker the watcher leaves alone rather than one whose pane is gone.
    worker::update(&run, id, |w| {
        w.restarting = true;
        w.gone_reported = false;
        w.agent.status = AgentStatus::Open;
        w.agent.workspace_id.clear();
        w.agent.tab_id.clear();
        w.agent.pane_id.clear();
        w.agent.prompt_pending = true;
        w.agent.last_group.clear();
        w.agent.blocked_reported = false;
    })?;
    let placed = match reopen(ctx, &config, session, &w).await {
        Ok(placed) => placed,
        Err(error) => {
            let message = format!("{error:#}");
            worker::update(&run, id, |w| {
                w.restarting = false;
                w.agent.status = AgentStatus::Failed;
                w.agent.error = message;
            })?;
            ticker::poke(&ctx.state_dir());
            return Err(error);
        }
    };
    let reset = worker::update(&run, id, |w| {
        w.restarts += 1;
        w.report_hash.clear();
        w.gone_reported = false;
        w.agent.profile = profile_name.clone();
        w.agent.kind = profile.kind.clone();
        w.agent.last_group.clear();
        w.agent.last_state.clear();
        w.agent.last_state_change.clear();
        w.agent.last_state_seq = 0;
        w.agent.blocked_reported = false;
        w.agent.resume = false;
    })?;
    let task = std::fs::read_to_string(worker::task_path(&run, id)).unwrap_or_default();
    let instructions = profile.instructions.as_deref();
    let worker = place(
        ctx,
        &run,
        &record,
        reset,
        &placed,
        task.trim(),
        true,
        instructions,
    )
    .await?;
    ticker::poke(&ctx.state_dir());
    println!(
        "{id} restarts in {} with the `{profile_name}` profile",
        worker.worktree_path
    );
    Ok(worker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::SAMPLE;
    use crate::herdr::{FakeHerdr, PaneId};
    use crate::paths::Env;
    use crate::process::fake::{FakeRunner, fail, ok};

    fn pane(record: &crate::run::AgentRecord) -> PaneId {
        PaneId(record.pane_id.clone())
    }

    #[test]
    fn options_parse_as_label_and_value() {
        assert_eq!(
            parse_option("Yes=yes").unwrap(),
            ("Yes".into(), "yes".into())
        );
        assert_eq!(
            parse_option("Maybe").unwrap(),
            ("Maybe".into(), "Maybe".into())
        );
        assert!(parse_option("=x").is_err());
        assert!(parse_option("x=").is_err());
    }

    #[tokio::test]
    async fn exclude_is_added_once_to_the_shared_file() {
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        let cwd = repo.path().to_string_lossy().into_owned();
        let runner = crate::process::RealRunner;
        exclude_from_git(&runner, &cwd).await.unwrap();
        exclude_from_git(&runner, &cwd).await.unwrap();
        let text = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(".herdr-linear-agent/").count(), 1);
        std::fs::create_dir_all(repo.path().join(".herdr-linear-agent/DATA-1-w1")).unwrap();
        std::fs::write(
            repo.path().join(".herdr-linear-agent/DATA-1-w1/report.md"),
            "r",
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&run(&["status", "--porcelain"]).stdout).is_empty());
    }

    struct Setup {
        home: tempfile::TempDir,
        env: Env,
        runner: FakeRunner,
        session: Session<FakeHerdr>,
    }

    impl Setup {
        fn new(edit: impl FnOnce(String) -> String) -> Setup {
            let home = tempfile::tempdir().unwrap();
            let root = home.path().to_string_lossy().into_owned();
            let env = Env::for_test(
                home.path(),
                &[
                    ("XDG_STATE_HOME", &format!("{root}/state")),
                    ("XDG_CONFIG_HOME", &format!("{root}/config")),
                ],
            );
            std::fs::create_dir_all(env.config_dir()).unwrap();
            let config = SAMPLE.replace("path = \"/src/", &format!("path = \"{root}/src/"));
            std::fs::write(env.config_dir().join("config.toml"), edit(config)).unwrap();
            std::fs::create_dir_all(env.state_dir().join("runs")).unwrap();
            let runner = FakeRunner::new();
            runner.on("git -C", fail(128, "not a git repository"));
            runner.on("fetch origin", ok(""));
            let session = Session {
                herdr: FakeHerdr::new(home.path()),
                socket: "/work.sock".into(),
            };
            let setup = Setup {
                home,
                env,
                runner,
                session,
            };
            Run::create(
                &setup.ctx().runs_dir(),
                RunRecord {
                    identifier: "DATA-1".into(),
                    issue_id: "issue-1".into(),
                    title: "Fix the login".into(),
                    url: "https://linear.app/acme/issue/DATA-1".into(),
                    ..RunRecord::default()
                },
            )
            .unwrap();
            setup
        }

        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                env: &self.env,
                runner: &self.runner,
                detached_ticker: false,
            }
        }

        fn run(&self) -> Run {
            Run::load(&self.ctx().runs_dir(), "DATA-1").unwrap()
        }

        async fn start(&self, repo: &str, profile: &str) -> Result<Worker> {
            let args = WorkerStart {
                repo: repo.into(),
                profile: profile.into(),
                title: format!("Change {repo}"),
                task: "Make the change and open a PR.".into(),
            };
            worker_start(&self.ctx(), &self.session, "DATA-1", &args).await
        }
    }

    #[tokio::test]
    async fn a_worker_is_placed_in_a_worktree_with_its_brief() {
        let setup = Setup::new(|c| c);
        let w = setup.start("api", "standard").await.unwrap();
        assert_eq!(w.branch, "herdr-linear-agent/data-1/w1-change-api");
        let root = setup.home.path().to_string_lossy();
        assert_eq!(
            setup.session.herdr.worktrees(),
            [(
                format!("{root}/src/api"),
                "herdr-linear-agent/data-1/w1-change-api".into(),
                "origin/main".into()
            )]
        );
        assert_eq!(
            setup.runner.lines("fetch"),
            [format!("git -C {root}/src/api fetch origin main")]
        );
        // The exclusion failed (not a git repository) and the start went on.
        assert_eq!(setup.runner.count("rev-parse --git-common-dir"), 1);
        assert_eq!(w.agent.status, AgentStatus::Open);
        assert!(w.agent.prompt_pending);
        assert_eq!(w.agent.agent_name, "data-1-w1");
        assert_eq!(w.agent.kind, "claude");
        assert_eq!(
            (w.agent.pane_id.as_str(), w.agent.workspace_id.as_str()),
            ("w1:p1", "w1")
        );
        assert_eq!(w.agent.cwd, w.worktree_path);
        assert!(w.brief_dir.ends_with(".herdr-linear-agent/DATA-1-w1"));
        let brief = std::fs::read_to_string(Path::new(&w.brief_dir).join("brief.md")).unwrap();
        assert!(brief.contains("Make the change and open a PR."));
        assert!(!brief.contains("previous attempt"));
        let task = std::fs::read_to_string(worker::task_path(&setup.run(), "w1")).unwrap();
        assert_eq!(task, "Make the change and open a PR.\n");
        assert!(
            outbox::pending(&setup.run()).is_empty(),
            "`Start worker` is queued when the launch prompt goes out"
        );
        assert_eq!(worker::load(&setup.run(), "w1").unwrap(), w);
    }

    #[tokio::test]
    async fn limits_and_names_are_checked_before_anything_is_placed() {
        let setup =
            Setup::new(|c| c.replace("[herdr]", "[limits]\nmax_workers_per_run = 1\n\n[herdr]"));
        setup.start("api", "standard").await.unwrap();
        let error = setup
            .start("web", "standard")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("max_workers_per_run"), "{error}");
        let error = setup
            .start("api", "standard")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("one worker per repository"), "{error}");
        assert!(setup.start("nope", "standard").await.is_err());
        let error = setup
            .start("web", "coordinator")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a worker profile"), "{error}");
        assert_eq!(setup.session.herdr.worktrees().len(), 1);
        assert_eq!(worker::list(&setup.run()).len(), 1);

        // A stopped worker frees its repository and its place.
        worker::update(&setup.run(), "w1", |w| {
            w.agent.status = AgentStatus::Stopped
        })
        .unwrap();
        assert_eq!(setup.start("api", "deep").await.unwrap().id, "w2");
    }

    #[tokio::test]
    async fn the_agent_limit_counts_every_active_run() {
        let setup = Setup::new(|c| c.replace("[herdr]", "[limits]\nmax_agents = 2\n\n[herdr]"));
        setup
            .run()
            .update(|r| r.coordinator.status = AgentStatus::Open)
            .unwrap();
        setup.start("api", "standard").await.unwrap();
        let error = setup
            .start("web", "standard")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "the limit of 2 agents is reached");
    }

    #[tokio::test]
    async fn a_failed_fetch_writes_no_worker() {
        let setup = Setup::new(|c| c);
        setup.runner.on(
            "fetch origin",
            fail(128, "fatal: couldn't find remote ref main\nmore"),
        );
        let error = setup
            .start("api", "standard")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "git fetch failed: fatal: couldn't find remote ref main"
        );
        assert!(worker::list(&setup.run()).is_empty());
        assert!(setup.session.herdr.worktrees().is_empty());
    }

    #[tokio::test]
    async fn a_failed_placement_leaves_a_failed_worker() {
        let setup = Setup::new(|c| c);
        setup.session.herdr.set_down(true);
        assert!(setup.start("api", "standard").await.is_err());
        let w = worker::load(&setup.run(), "w1").unwrap();
        assert_eq!(w.agent.status, AgentStatus::Failed);
        assert!(
            w.agent.error.contains("could not create the worktree"),
            "{}",
            w.agent.error
        );
        // A restart places it again from its base.
        setup.session.herdr.set_down(false);
        let w = worker_restart(&setup.ctx(), &setup.session, "DATA-1", "w1", None)
            .await
            .unwrap();
        assert_eq!((w.agent.status, w.restarts), (AgentStatus::Open, 1));
        assert_eq!(setup.session.herdr.worktrees().len(), 1);
    }

    /// `git worktree list --porcelain` naming the fake's worktree of `branch`.
    fn listing(setup: &Setup, branch: &str) -> String {
        let path = setup
            .home
            .path()
            .join("worktrees")
            .join(branch.replace('/', "-"));
        format!(
            "worktree {}/src/api\nHEAD 1111\nbranch refs/heads/main\n\nworktree {}\nHEAD 2222\nbranch refs/heads/{branch}\n\n",
            setup.home.path().display(),
            path.display()
        )
    }

    #[tokio::test]
    async fn a_worktree_created_without_an_answer_is_opened_not_failed() {
        let setup = Setup::new(|c| c);
        let branch = "herdr-linear-agent/data-1/w1-change-api";
        setup
            .runner
            .on("worktree list --porcelain", ok(&listing(&setup, branch)));
        setup.session.herdr.next_placement_unknown();
        let w = setup.start("api", "standard").await.unwrap();
        assert_eq!(w.agent.status, AgentStatus::Open);
        assert_eq!(setup.session.herdr.worktrees().len(), 1);
        let opened: Vec<String> = setup
            .session
            .herdr
            .requests()
            .into_iter()
            .filter(|m| m.starts_with("worktree."))
            .collect();
        assert_eq!(opened, ["worktree.create", "worktree.open"]);
        assert_eq!(w.agent.pane_id, "w2:p1");
        assert!(
            w.worktree_path
                .ends_with("herdr-linear-agent-data-1-w1-change-api")
        );
    }

    #[tokio::test]
    async fn a_restart_opens_the_worktree_a_lost_answer_left_behind() {
        let setup = Setup::new(|c| c);
        setup.session.herdr.next_placement_unknown();
        assert!(setup.start("api", "standard").await.is_err());
        let failed = worker::load(&setup.run(), "w1").unwrap();
        assert_eq!(failed.agent.status, AgentStatus::Failed);
        setup.runner.on(
            "worktree list --porcelain",
            ok(&listing(&setup, &failed.branch)),
        );
        let w = worker_restart(&setup.ctx(), &setup.session, "DATA-1", "w1", None)
            .await
            .unwrap();
        assert_eq!(
            setup.session.herdr.worktrees().len(),
            1,
            "not created again"
        );
        assert_eq!(
            (w.agent.status, w.agent.pane_id.as_str()),
            (AgentStatus::Open, "w2:p1")
        );
    }

    #[tokio::test]
    async fn restarts_switch_profiles_and_are_limited() {
        let setup = Setup::new(|c| c);
        setup.start("api", "standard").await.unwrap();
        worker::update(&setup.run(), "w1", |w| {
            w.report_hash = "h".into();
            w.announced_report_hash = "h".into();
            w.agent.last_group = "reported".into();
            w.agent.launch_attempts = 2;
        })
        .unwrap();
        let restarted = worker_restart(&setup.ctx(), &setup.session, "DATA-1", "w1", Some("deep"))
            .await
            .unwrap();
        assert_eq!(
            (restarted.agent.kind.as_str(), restarted.restarts),
            ("codex", 1)
        );
        assert!(restarted.agent.prompt_pending);
        assert_eq!(restarted.agent.profile, "deep");
        assert_eq!(restarted.agent.pane_id, "w2:p1");
        assert_eq!(
            (
                restarted.report_hash.as_str(),
                restarted.announced_report_hash.as_str(),
                restarted.agent.last_group.as_str(),
                restarted.agent.launch_attempts
            ),
            ("", "h", "", 0)
        );
        assert_eq!(
            setup.session.herdr.closed(),
            [WorkspaceId("w1".into())],
            "the old workspace is closed; the checkout stays"
        );
        assert!(
            std::fs::read_to_string(Path::new(&restarted.brief_dir).join("brief.md"))
                .unwrap()
                .contains("previous attempt")
        );
        let error = worker_restart(
            &setup.ctx(),
            &setup.session,
            "DATA-1",
            "w1",
            Some("coordinator"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("not a worker profile"), "{error}");
        worker_restart(&setup.ctx(), &setup.session, "DATA-1", "w1", None)
            .await
            .unwrap();
        let error = worker_restart(&setup.ctx(), &setup.session, "DATA-1", "w1", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("limit is 2"), "{error}");
    }

    #[tokio::test]
    async fn a_prompt_to_a_worker_is_refused_while_it_waits_on_a_dialog() {
        let setup = Setup::new(|c| c);
        let w = setup.start("api", "standard").await.unwrap();
        let herdr = &setup.session.herdr;
        let error = worker_prompt(&setup.ctx(), &setup.session, "DATA-1", "w1", "Hi")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error, "worker w1 is not running",
            "no agent in the pane yet"
        );
        herdr
            .agent_start(&w.agent.agent_name, "claude", &pane(&w.agent), &[])
            .await
            .unwrap();
        herdr.set_status("data-1-w1", "blocked");
        let error = worker_prompt(&setup.ctx(), &setup.session, "DATA-1", "w1", "Answer")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("dialog"), "{error}");
        herdr.set_status("data-1-w1", "working");
        worker_prompt(
            &setup.ctx(),
            &setup.session,
            "DATA-1",
            "w1",
            "Also add a test.\n",
        )
        .await
        .unwrap();
        assert_eq!(herdr.prompts_to("w1:p1"), ["Also add a test."]);
        let task = std::fs::read_to_string(worker::task_path(&setup.run(), "w1")).unwrap();
        assert!(task.contains("## Follow-ups") && task.contains("Also add a test."));
    }

    #[tokio::test]
    async fn finish_waits_for_every_worker() {
        let setup = Setup::new(|c| c);
        let w = setup.start("api", "standard").await.unwrap();
        let error = finish(&setup.ctx(), &setup.session, "DATA-1", "Done")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "not finished: w1 is Working. Every worker must have written a report and be neither working nor waiting"
        );
        let herdr = &setup.session.herdr;
        herdr
            .agent_start(&w.agent.agent_name, "claude", &pane(&w.agent), &[])
            .await
            .unwrap();
        worker::update(&setup.run(), "w1", |w| w.report_hash = "h".into()).unwrap();
        finish(&setup.ctx(), &setup.session, "DATA-1", " Opened the PR. ")
            .await
            .unwrap();
        let ops: Vec<Op> = outbox::pending(&setup.run())
            .into_iter()
            .map(|(_, r)| r.op)
            .collect();
        assert_eq!(
            ops,
            [
                Op::Activity {
                    activity: Activity::new(Content::Response {
                        body: "Opened the PR.".into()
                    })
                },
                Op::IssueState {
                    target: StateTarget::Review
                }
            ]
        );
        assert!(setup.run().record().unwrap().finished);

        herdr.set_down(true);
        let error = finish(&setup.ctx(), &setup.session, "DATA-1", "Again")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, UNREACHABLE);
    }

    #[tokio::test]
    async fn commands_that_write_run_files_poke_the_ticker() {
        let setup = Setup::new(|c| c);
        let listener =
            std::os::unix::net::UnixDatagram::bind(ticker::poke_path(&setup.ctx().state_dir()))
                .unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let poked = || {
            let mut byte = [0u8; 8];
            listener.recv(&mut byte).is_ok()
        };
        say(&setup.ctx(), "DATA-1", "Working on it.").await.unwrap();
        assert!(poked(), "say");
        plan_set(&setup.ctx(), "DATA-1", "- [ ] One").await.unwrap();
        assert!(poked(), "plan set");
        setup.start("api", "standard").await.unwrap();
        assert!(poked(), "worker start");
        inbox_done(&setup.ctx(), "DATA-1", &[], true).unwrap();
        assert!(poked(), "inbox done");
    }

    #[tokio::test]
    async fn a_poke_without_a_ticker_is_ignored() {
        let setup = Setup::new(|c| c);
        say(&setup.ctx(), "DATA-1", "Nobody listens.")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn done_all_leaves_the_items_written_after_the_last_context() {
        let setup = Setup::new(|c| c);
        let run = setup.run();
        inbox::write(&run, "worker", "w1", "shown").unwrap();
        context(&setup.ctx(), Some(&setup.session), "DATA-1")
            .await
            .unwrap();
        let later = inbox::write(&run, "worker", "w2", "later").unwrap();
        inbox_done(&setup.ctx(), "DATA-1", &[], true).unwrap();
        let left: Vec<String> = inbox::unhandled(&run).into_iter().map(|i| i.id).collect();
        assert_eq!(left, std::slice::from_ref(&later));
        assert_eq!(
            done_message(&run, 1),
            "1 item(s) handled; 1 new item(s) since your last context, run context"
        );
        inbox_done(&setup.ctx(), "DATA-1", std::slice::from_ref(&later), false).unwrap();
        assert_eq!(done_message(&run, 1), "1 item(s) handled");
    }

    #[tokio::test]
    async fn commands_need_an_active_run_and_context_marks_items_seen() {
        let setup = Setup::new(|c| c);
        let run = setup.run();
        let id = inbox::write(&run, "worker", "w1", "item").unwrap();
        context(&setup.ctx(), Some(&setup.session), "DATA-1")
            .await
            .unwrap();
        assert!(inbox::seen(&run).contains(&id));
        context::<FakeHerdr>(&setup.ctx(), None, "DATA-1")
            .await
            .unwrap();
        inbox_done(&setup.ctx(), "DATA-1", &[], true).unwrap();
        assert!(inbox::unhandled(&run).is_empty());
        assert!(inbox_done(&setup.ctx(), "DATA-1", &[], false).is_err());

        run.update(|r| r.status = Status::Detached).unwrap();
        let error = say(&setup.ctx(), "DATA-1", "x")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "run DATA-1 is Detached; nothing more is done for it");
        assert_eq!(
            say(&setup.ctx(), "DATA-1", " ")
                .await
                .unwrap_err()
                .to_string(),
            "the text is empty"
        );
    }
}
