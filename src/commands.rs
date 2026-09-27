//! The subcommands a coordinator runs. Each is one deterministic mechanic:
//! the coordinator decides whether, what and where; these commands check names
//! against the config, enforce the limits, and queue Linear writes for the
//! ticker. None of them reads the Keychain.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::herdr_cli::{self, Agent, Herdr, Pane};
use crate::linear::api::{Activity, Content};
use crate::outbox::{self, Op, StateTarget};
use crate::paths::Ctx;
use crate::run::{AgentStatus, Run, RunRecord, Status};
use crate::runner::{Cmd, Runner};
use crate::worker::{self, Group, Worker};
use crate::{coordinator, files, inbox, names, steps, ticker};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

fn load_active(ctx: &Ctx, key: &str) -> Result<(Config, Run, RunRecord)> {
    // Every command an agent runs keeps the ticker alive.
    ticker::start(ctx)?;
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

/// The configured session's lists, for commands that check live state.
struct View<'a> {
    herdr: Herdr<'a>,
    socket: String,
    agents: Vec<Agent>,
    panes: Vec<Pane>,
}

fn view<'a>(ctx: &'a Ctx, config: &Config) -> Result<View<'a>> {
    let socket = herdr_cli::session_socket(
        &ctx.env.herdr_bin(),
        ctx.runner,
        config.herdr.session.as_deref(),
    )?;
    let herdr = Herdr::new(ctx.env.herdr_bin(), &socket, ctx.runner);
    let agents = herdr
        .agent_list()
        .context("the configured Herdr session is not reachable")?;
    let panes = herdr.pane_list()?;
    Ok(View {
        herdr,
        socket: socket.to_string_lossy().into_owned(),
        agents,
        panes,
    })
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

pub fn context(ctx: &Ctx, key: &str) -> Result<()> {
    ticker::start(ctx)?;
    let config = Config::load(&ctx.config_dir())?;
    let run = Run::load(&ctx.runs_dir(), key)?;
    let state_dir = ctx.state_dir();
    let rows = match view(ctx, &config) {
        Ok(v) => coordinator::worker_rows(&run, Some((&v.agents, &v.panes, &state_dir, &v.socket))),
        Err(_) => coordinator::worker_rows(&run, None),
    };
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
    println!("{moved} item(s) handled");
    Ok(())
}

pub fn plan_set(ctx: &Ctx, key: &str, text: &str) -> Result<()> {
    let (_, run, _) = load_active(ctx, key)?;
    let plan = outbox::parse_plan(text)?;
    outbox::push(&run, Op::Plan { plan })?;
    println!("the plan is queued for Linear");
    Ok(())
}

pub fn say(ctx: &Ctx, key: &str, text: &str) -> Result<()> {
    non_empty(text, "text")?;
    let (_, run, _) = load_active(ctx, key)?;
    outbox::push(
        &run,
        Op::Activity {
            activity: Activity::new(Content::Thought {
                body: text.trim().to_string(),
            }),
        },
    )?;
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

pub fn ask(ctx: &Ctx, key: &str, text: &str, options: &[(String, String)]) -> Result<()> {
    non_empty(text, "question")?;
    let (_, run, _) = load_active(ctx, key)?;
    let mut activity = Activity::new(Content::Elicitation {
        body: text.trim().to_string(),
    });
    if !options.is_empty() {
        activity.signal = Some("select".into());
        let options: Vec<_> = options
            .iter()
            .map(|(label, value)| serde_json::json!({ "label": label, "value": value }))
            .collect();
        activity.signal_metadata = Some(serde_json::json!({ "options": options }));
    }
    outbox::push(&run, Op::Activity { activity })?;
    println!(
        "the question is queued for the Linear session; end your turn, the answer arrives in your inbox"
    );
    Ok(())
}

/// Posts the final summary and moves the issue to review, once every worker
/// has reported and none is working or waiting.
pub fn finish(ctx: &Ctx, key: &str, text: &str) -> Result<()> {
    non_empty(text, "summary")?;
    let (config, run, _) = load_active(ctx, key)?;
    let v = view(ctx, &config)?;
    let now = jiff::Timestamp::now();
    let blocking: Vec<String> = worker::list(&run)
        .into_iter()
        .filter(|w| {
            matches!(
                w.agent.status,
                AgentStatus::Open | AgentStatus::Failed | AgentStatus::Pending
            )
        })
        .map(|w| {
            (
                worker::group(
                    &w,
                    &worker::live_state(
                        &w.agent,
                        &v.agents,
                        &v.panes,
                        now,
                        &ctx.state_dir(),
                        &v.socket,
                    ),
                ),
                w,
            )
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
    println!(
        "the summary is queued; the issue moves to `{}`",
        config.linear.review_state
    );
    Ok(())
}

// ---------------------------------------------------------------- workers

pub struct WorkerStart {
    pub repo: String,
    pub profile: String,
    pub title: String,
    pub task: String,
}

pub fn worker_start(ctx: &Ctx, key: &str, args: &WorkerStart) -> Result<Worker> {
    non_empty(&args.title, "title")?;
    non_empty(&args.task, "task")?;
    let (config, run, record) = load_active(ctx, key)?;
    let repo = config.repository(&args.repo)?.clone();
    let profile = config.worker_profile(&args.profile)?.clone();
    let agents_now = steps::agent_count(&Run::list(&ctx.runs_dir()));
    let limits = config.limits;
    let worker = worker::allocate(
        &run,
        |workers| {
            if let Some(w) = workers.iter().find(|w| w.counts() && w.repo == args.repo) {
                bail!(
                    "{} already works on `{}`; start at most one worker per repository (use `worker prompt` or `worker restart`)",
                    w.id,
                    args.repo
                );
            }
            let open = workers.iter().filter(|w| w.counts()).count();
            if open >= limits.max_workers_per_run as usize {
                bail!(
                    "this run already has {open} workers; max_workers_per_run is {}",
                    limits.max_workers_per_run
                );
            }
            if agents_now >= limits.max_agents as usize {
                bail!(
                    "{agents_now} agents are running; max_agents is {}",
                    limits.max_agents
                );
            }
            Ok(())
        },
        |w| {
            w.title = args.title.trim().to_string();
            w.repo = args.repo.clone();
            w.repo_path = repo.path.to_string_lossy().into_owned();
            w.base = repo.base.clone();
            w.agent.profile = args.profile.clone();
            w.agent.kind = profile.kind.clone();
        },
    )?;
    let id = worker.id.clone();
    worker::update(&run, &id, |w| {
        w.agent.agent_name = names::worker(&record.identifier, &record.issue_id, &id)
    })?;
    {
        let _lock = run.lock()?;
        files::write_atomic(&worker::task_path(&run, &id), args.task.as_bytes())?;
    }
    let v = view(ctx, &config)?;
    match place(ctx, &v.herdr, &run, &record, &id, false) {
        Ok(worker) => {
            println!(
                "started {id} in {} on branch {}; the ticker launches its agent within 15 seconds",
                worker.worktree_path, worker.branch
            );
            Ok(worker)
        }
        Err(error) => {
            let message = format!("{error:#}");
            let _ = worker::update(&run, &id, |w| {
                w.agent.status = AgentStatus::Failed;
                w.agent.error = message.clone();
            });
            Err(error.context(format!(
                "{id} failed to start; `worker restart {key} {id}` retries"
            )))
        }
    }
}

fn git(runner: &dyn Runner, dir: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let out = runner.run(
        &Cmd::new("git", timeout)
            .args(["-C", dir])
            .args(args.iter().copied()),
    )?;
    if !out.success() {
        bail!("git {}: {}", args.join(" "), out.error_text());
    }
    Ok(out.stdout.trim().to_string())
}

/// Fetches the base, creates the worktree with Herdr, writes the brief and
/// hands the worker to the ticker's launch step.
fn place(
    ctx: &Ctx,
    herdr: &Herdr,
    run: &Run,
    record: &RunRecord,
    id: &str,
    restart: bool,
) -> Result<Worker> {
    let w = worker::load(run, id)?;
    if let Err(error) = git(
        ctx.runner,
        &w.repo_path,
        &["fetch", "origin", &w.base],
        FETCH_TIMEOUT,
    ) {
        eprintln!("warning: {error:#}");
    }
    let branch = worker::branch_name(&record.identifier, id, &w.title);
    let label = format!("{} {id} {}", record.identifier, w.title);
    let (created, path, cwd) = herdr.worktree_create(
        Path::new(&w.repo_path),
        &branch,
        &format!("origin/{}", w.base),
        &label,
    )?;
    // Recorded at once, so a command killed midway leaves a record `worker
    // restart` can act on.
    worker::update(run, id, |w| {
        w.branch = branch;
        w.worktree_path = path;
        w.agent.cwd = cwd;
        w.agent.workspace_id = created.workspace_id;
        w.agent.tab_id = created.tab_id;
        w.agent.pane_id = created.pane_id;
    })?;
    brief_and_hand_over(ctx, run, record, id, restart)
}

fn brief_and_hand_over(
    ctx: &Ctx,
    run: &Run,
    record: &RunRecord,
    id: &str,
    restart: bool,
) -> Result<Worker> {
    let w = worker::load(run, id)?;
    let dir = worker::brief_dir(&w.agent.cwd, &record.identifier, id);
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {dir}"))?;
    exclude_from_git(ctx.runner, &w.agent.cwd)?;
    let with_dir = Worker {
        brief_dir: dir.clone(),
        ..w
    };
    let task = std::fs::read_to_string(worker::task_path(run, id)).unwrap_or_default();
    let brief = worker::compose_brief(&worker::BriefInput {
        issue_key: &record.identifier,
        issue_title: &record.title,
        issue_url: &record.url,
        worker: &with_dir,
        task: &task,
        restart,
        binary: &coordinator::binary_command()?,
    });
    files::write_atomic(&Path::new(&dir).join("brief.md"), brief.as_bytes())?;
    worker::update(run, id, |w| {
        w.brief_dir = dir;
        w.agent.status = AgentStatus::Open;
        w.agent.error.clear();
        w.agent.prompt_pending = true;
        w.agent.launch_attempts = 0;
        w.agent.last_state.clear();
        w.agent.last_state_change = files::now();
        w.agent.last_group.clear();
        w.agent.blocked_reported = false;
        w.start_announced = false;
        w.gone_reported = false;
    })
}

/// Adds `.herdr-linear-agent/` to the repository's shared `info/exclude`, so
/// nothing in a brief folder is ever committed.
pub fn exclude_from_git(runner: &dyn Runner, cwd: &str) -> Result<()> {
    let Ok(path) = git(
        runner,
        cwd,
        &["rev-parse", "--git-path", "info/exclude"],
        GIT_TIMEOUT,
    ) else {
        return Ok(());
    };
    let path = Path::new(cwd).join(path);
    let pattern = format!("{}/", worker::BRIEF_FOLDER);
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = current;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&pattern);
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("could not update {}", path.display()))
}

/// Sends a worker a follow-up and appends it to its task file.
pub fn worker_prompt(ctx: &Ctx, key: &str, id: &str, text: &str) -> Result<()> {
    non_empty(text, "text")?;
    let (config, run, _) = load_active(ctx, key)?;
    let w = worker::load(&run, id)?;
    if w.agent.status != AgentStatus::Open {
        bail!(
            "{id} is {:?}; `worker restart {key} {id}` brings it back",
            w.agent.status
        );
    }
    if w.agent.prompt_pending {
        bail!("{id} has not received its brief yet; try again once it has started");
    }
    let v = view(ctx, &config)?;
    let agent = worker::find_agent(&w.agent, &v.agents).with_context(|| format!("no agent runs in {id}'s pane; text is never typed at a shell prompt (try `worker restart`)"))?;
    match agent.agent_status.as_str() {
        "blocked" => bail!(
            "{id} waits on a dialog in its pane {}; a person must answer it first",
            agent.pane_id
        ),
        "unknown" => bail!("{id}'s agent state is unknown; not sending"),
        _ => {}
    }
    v.herdr
        .agent_prompt(&agent.pane_id, text.trim())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    worker::append_follow_up(&run, id, text)?;
    println!("sent to {id} (it was {})", agent.agent_status);
    Ok(())
}

/// Starts a worker again in its worktree, optionally with another profile.
pub fn worker_restart(ctx: &Ctx, key: &str, id: &str, profile: Option<&str>) -> Result<Worker> {
    let (config, run, record) = load_active(ctx, key)?;
    let w = worker::load(&run, id)?;
    if w.restarts >= worker::MAX_RESTARTS {
        bail!(
            "{id} was restarted {} times already; the limit is {}",
            w.restarts,
            worker::MAX_RESTARTS
        );
    }
    if let Some(name) = profile {
        let kind = config.worker_profile(name)?.kind.clone();
        worker::update(&run, id, |w| {
            w.agent.profile = name.to_string();
            w.agent.kind = kind;
        })?;
    }
    let v = view(ctx, &config)?;
    let live = worker::live_state(
        &w.agent,
        &v.agents,
        &v.panes,
        jiff::Timestamp::now(),
        &ctx.state_dir(),
        &v.socket,
    );
    worker::update(&run, id, |w| w.restarts += 1)?;
    let placed = if w.worktree_path.is_empty() {
        place(ctx, &v.herdr, &run, &record, id, true)?
    } else {
        // The old agent goes with its workspace; the checkout stays.
        if live.pane_exists {
            let workspace = worker::find_agent(&w.agent, &v.agents)
                .map_or(w.agent.workspace_id.clone(), |a| a.workspace_id.clone());
            v.herdr
                .workspace_close(&workspace)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        let label = format!("{} {id} {}", record.identifier, w.title);
        let (created, path, cwd) =
            v.herdr
                .worktree_open(Path::new(&w.repo_path), &w.worktree_path, &label)?;
        worker::update(&run, id, |w| {
            w.worktree_path = path;
            w.agent.cwd = cwd;
            w.agent.workspace_id = created.workspace_id;
            w.agent.tab_id = created.tab_id;
            w.agent.pane_id = created.pane_id;
        })?;
        brief_and_hand_over(ctx, &run, &record, id, true)?
    };
    println!(
        "restarted {id} with the `{}` profile; the ticker launches its agent within 15 seconds",
        placed.agent.profile
    );
    Ok(placed)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn exclude_is_added_once_to_the_shared_file() {
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
        exclude_from_git(&crate::runner::RealRunner, &cwd).unwrap();
        exclude_from_git(&crate::runner::RealRunner, &cwd).unwrap();
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
}
