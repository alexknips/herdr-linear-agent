//! The Herdr actions people use. An action's output only reaches the plugin
//! log, so each action also shows its result as a Herdr notification.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::herdr::{self, Client, Herdr, HerdrError, WorkspaceId};
use crate::linear::client::LinearApi;
use crate::linear::credentials::{CredentialManager, CredentialStatus};
use crate::linear::transport::RateHeaders;
use crate::paths::Ctx;
use crate::process::Cmd;
use crate::run::{AgentStatus, Run, Status};
use crate::{ticker, worker};

const TITLE: &str = "herdr-linear-agent";

/// Runs a synchronous credential or OAuth call off the runtime's threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

/// Prints `body` and shows it as a notification in the invoking session, or
/// in the configured one.
async fn tell(ctx: &Ctx<'_>, body: &str) {
    println!("{body}");
    let session = Config::load(&ctx.config_dir())
        .ok()
        .and_then(|c| c.herdr.session);
    let shown = async {
        // The session the action was invoked from, when there is one.
        let socket = match ctx.env.var("HERDR_SOCKET_PATH").filter(|s| !s.is_empty()) {
            Some(socket) => socket.into(),
            None => herdr::session_socket(&ctx.env.herdr_bin(), session.as_deref()).await?,
        };
        Client::new(socket).notification_show(TITLE, body).await?;
        anyhow::Ok(())
    };
    let _ = shown.await;
}

/// The actions the plugin manifest declares.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Action {
    Login,
    Status,
    OpenIssue,
    FocusRun,
    Pause,
    Resume,
    Doctor,
}

pub async fn run(ctx: &Ctx<'_>, action: Action) -> Result<()> {
    let result = match action {
        Action::Login => login(ctx).await,
        Action::Status => Ok(status_text(ctx)),
        Action::OpenIssue => open_issue(ctx).await,
        Action::FocusRun => focus_run(ctx).await,
        Action::Pause => pause(ctx, true).await,
        Action::Resume => pause(ctx, false).await,
        Action::Doctor => doctor(ctx).await,
    };
    match result {
        Ok(message) => {
            tell(ctx, &message).await;
            Ok(())
        }
        Err(error) => {
            tell(ctx, &format!("{action:?} failed: {error:#}")).await;
            Err(error)
        }
    }
}

fn credential_lock(ctx: &Ctx) -> Result<std::path::PathBuf> {
    Ok(ctx.ensure_state_dir()?.join("credentials.lock"))
}

/// Authorizes the app in the browser, stores the token in the Keychain and
/// checks that the token acts as an app user. A stored credential is revoked
/// and replaced.
async fn login(ctx: &Ctx<'_>) -> Result<String> {
    let config = Config::load(&ctx.config_dir())?;
    let lock = credential_lock(ctx)?;
    let (client_id, port) = (config.linear.client_id.clone(), config.linear.callback_port);
    let login_lock = lock.clone();
    blocking(move || -> Result<()> {
        let mut manager = CredentialManager::production(client_id, port, login_lock)?;
        if manager.status() != CredentialStatus::SignedOut {
            manager
                .logout(true)
                .context("could not revoke the stored credential")?;
        }
        manager.login()?;
        Ok(())
    })
    .await?;
    let linear = crate::linear::client::Client::production(
        config.linear.client_id.clone(),
        config.linear.callback_port,
        lock,
    )
    .await?;
    let viewer = linear.viewer().await?;
    ticker::start(ctx).await?;
    Ok(format!(
        "Logged in to Linear as the app user {}.",
        if viewer.name.is_empty() {
            viewer.id
        } else {
            viewer.name
        }
    ))
}

/// One line per run: its state, the coordinator and the workers.
pub fn status_text(ctx: &Ctx) -> String {
    let mut lines = vec![ticker::describe(&ctx.state_dir())];
    if ctx.state_dir().join("paused").exists() {
        lines.push("paused: new issues are not picked up".into());
    }
    let runs = Run::list(&ctx.runs_dir());
    if runs.is_empty() {
        lines.push("no runs".into());
    }
    for run in runs {
        let Ok(record) = run.record() else { continue };
        let workers: Vec<String> = worker::list(&run)
            .iter()
            .filter(|w| w.agent.status != AgentStatus::Stopped)
            .map(|w| {
                format!(
                    "{} {} ({})",
                    w.id,
                    if w.agent.status == AgentStatus::Failed {
                        "failed"
                    } else if w.agent.last_group.is_empty() {
                        "starting"
                    } else {
                        &w.agent.last_group
                    },
                    w.repo
                )
            })
            .collect();
        let finished = if record.finished { ", finished" } else { "" };
        let coordinator = match record.coordinator.status {
            AgentStatus::Open if record.coordinator_lost => "coordinator gone".to_string(),
            AgentStatus::Open => format!(
                "coordinator {}",
                if record.coordinator.last_state.is_empty() {
                    "starting"
                } else {
                    &record.coordinator.last_state
                }
            ),
            AgentStatus::Pending => "coordinator pending".to_string(),
            AgentStatus::Failed => format!("coordinator failed: {}", record.coordinator.error),
            AgentStatus::Stopped => "coordinator stopped".to_string(),
        };
        let workers = if workers.is_empty() {
            String::new()
        } else {
            format!("; {}", workers.join(", "))
        };
        lines.push(format!(
            "{} {:?}{finished}: {coordinator}{workers}",
            record.identifier, record.status
        ));
    }
    lines.join("\n")
}

async fn pause(ctx: &Ctx<'_>, paused: bool) -> Result<String> {
    let flag = ctx.ensure_state_dir()?.join("paused");
    if paused {
        std::fs::write(&flag, b"")?;
        Ok("Paused: new issues are not picked up. Running runs continue.".into())
    } else {
        let _ = std::fs::remove_file(&flag);
        ticker::start(ctx).await?;
        Ok("Resumed: delegated issues are picked up again.".into())
    }
}

/// The directory of the pane the action was invoked from.
fn invoking_cwd(ctx: &Ctx) -> Option<String> {
    let context: serde_json::Value =
        serde_json::from_str(ctx.env.var("HERDR_PLUGIN_CONTEXT_JSON")?).ok()?;
    context["focused_pane_cwd"]
        .as_str()
        .or_else(|| context["workspace_cwd"].as_str())
        .map(str::to_string)
}

/// The run whose coordinator or worker works in `cwd`.
pub fn run_for_cwd(ctx: &Ctx, cwd: &str) -> Option<Run> {
    let cwd = std::path::Path::new(cwd);
    Run::list(&ctx.runs_dir()).into_iter().find(|run| {
        let coordinator = run
            .record()
            .is_ok_and(|r| !r.coordinator.cwd.is_empty() && cwd.starts_with(&r.coordinator.cwd));
        coordinator
            || worker::list(run)
                .iter()
                .any(|w| !w.worktree_path.is_empty() && cwd.starts_with(&w.worktree_path))
    })
}

async fn open_issue(ctx: &Ctx<'_>) -> Result<String> {
    let cwd = invoking_cwd(ctx).context("this action needs a pane of a run")?;
    let run = run_for_cwd(ctx, &cwd).context("this pane does not belong to a run")?;
    let record = run.record()?;
    let out = ctx
        .runner
        .run(&Cmd::new(crate::files::OPEN_COMMAND, Duration::from_secs(10)).arg(&record.url))
        .await?;
    if !out.success() {
        bail!("could not open {}: {}", record.url, out.error_text());
    }
    Ok(format!("Opened {} in the browser.", record.identifier))
}

/// The issue key in a Linear issue URL.
pub fn key_from_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://linear.app/")?;
    let key = rest.split('/').nth(2)?;
    crate::run::validate_key(key).ok().map(|_| key.to_string())
}

async fn focus_run(ctx: &Ctx<'_>) -> Result<String> {
    let url = ctx
        .env
        .var("HERDR_PLUGIN_CLICKED_URL")
        .context("Ctrl-click a Linear issue link to focus its run")?;
    let key = key_from_url(url).with_context(|| format!("{url} is not a Linear issue URL"))?;
    let run =
        Run::load(&ctx.runs_dir(), &key).with_context(|| format!("there is no run for {key}"))?;
    let record = run.record()?;
    if record.coordinator.workspace_id.is_empty() {
        bail!("{key} has no coordinator workspace yet");
    }
    let config = Config::load(&ctx.config_dir())?;
    let socket =
        herdr::session_socket(&ctx.env.herdr_bin(), config.herdr.session.as_deref()).await?;
    let client = Client::new(socket);
    focus_coordinator(&client, &record.coordinator).await?;
    Ok(format!("Focused the run of {key}."))
}

/// Focuses the coordinator's live workspace, or else the recorded one.
async fn focus_coordinator<H: Herdr>(
    herdr: &H,
    coordinator: &crate::run::AgentRecord,
) -> Result<()> {
    let snapshot = herdr.snapshot().await?;
    let workspace = worker::find_agent(coordinator, &snapshot.agents)
        .and_then(|a| snapshot.panes.get(&a.pane))
        .map_or_else(
            || WorkspaceId(coordinator.workspace_id.clone()),
            |pane| pane.workspace.clone(),
        );
    herdr.workspace_focus(&workspace).await?;
    Ok(())
}

/// The executable Herdr starts for an agent kind.
fn executable(kind: &str) -> &str {
    match kind {
        "cursor" => "cursor-agent",
        other => other,
    }
}

/// Whether `program` resolves from this process's `PATH`, which is what the
/// ticker and the agents it starts see.
fn on_path(program: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// The version the session reports, and the error that kept its snapshot
/// from being read. An old Herdr whose snapshot no longer parses still gives
/// its version.
async fn herdr_version(client: Client) -> (Option<String>, Option<HerdrError>) {
    match client.snapshot().await {
        Ok(snapshot) => (Some(snapshot.version), None),
        Err(error @ HerdrError::Protocol(_)) => (client.version().await.ok(), Some(error)),
        Err(error) => (None, Some(error)),
    }
}

/// The budget one viewer read reports; `None` when the read fails or a
/// value is missing.
async fn linear_budget(ctx: &Ctx<'_>, config: &Config) -> Option<String> {
    let linear = crate::linear::client::Client::production(
        config.linear.client_id.clone(),
        config.linear.callback_port,
        ctx.state_dir().join("credentials.lock"),
    )
    .await
    .ok()?;
    linear.viewer().await.ok()?;
    let mut budget = RateHeaders::default();
    for headers in linear.take_headers() {
        budget.observe(&headers);
    }
    budget.describe()
}

/// Checks the setup and lists every problem found.
async fn doctor(ctx: &Ctx<'_>) -> Result<String> {
    let mut ok = Vec::new();
    let mut problems = Vec::new();
    let config = Config::load(&ctx.config_dir());
    // Without a config this is the default session.
    let session = config.as_ref().ok().and_then(|c| c.herdr.session.clone());
    let socket = herdr::session_socket(&ctx.env.herdr_bin(), session.as_deref()).await;
    let (version, error) = match &socket {
        Ok(socket) => herdr_version(Client::new(socket)).await,
        Err(_) => (None, None),
    };
    match (&version, &error, &socket) {
        (Some(v), _, _) if !herdr::version_at_least(v, herdr::MIN_VERSION) => {
            problems.push(format!("herdr {v} is older than {}", herdr::MIN_VERSION))
        }
        (_, Some(error), _) => problems.push(format!("herdr: {error}")),
        (Some(v), None, _) => ok.push(format!("herdr {v}")),
        // With a config the session line below reports this.
        (None, None, Err(error)) if config.is_err() => problems.push(format!("herdr: {error:#}")),
        (None, None, _) => {}
    }
    let config = match config {
        Ok(config) => {
            ok.push(format!(
                "config {}",
                Config::path(&ctx.config_dir()).display()
            ));
            Some(config)
        }
        Err(error) => {
            problems.push(format!("{error:#}"));
            None
        }
    };
    if let Some(config) = &config {
        match (&socket, &version) {
            (Err(error), _) => problems.push(format!("{error:#}")),
            (Ok(_), Some(_)) => ok.push(format!(
                "Herdr session `{}` is reachable",
                session.as_deref().unwrap_or("default")
            )),
            (Ok(_), None) => problems.push("the configured Herdr session does not answer".into()),
        }
        let mut kinds: Vec<&str> = config
            .profiles
            .values()
            .map(|p| executable(&p.kind))
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        for program in kinds.into_iter().chain(["git"]) {
            if on_path(program) {
                ok.push(format!("`{program}` found"));
            } else {
                problems.push(format!("`{program}` is not on the ticker's PATH"));
            }
        }
        for (name, repo) in &config.repositories {
            if !repo.path.join(".git").exists() {
                problems.push(format!(
                    "repository `{name}`: {} is not a git checkout",
                    repo.path.display()
                ));
            }
        }
        let lock = ctx.state_dir().join("credentials.lock");
        let status =
            blocking(move || CredentialManager::production_status(lock).map(|mut m| m.status()))
                .await;
        let stored = matches!(
            status,
            Ok(CredentialStatus::Ready | CredentialStatus::ExpiredOrRefreshNeeded)
        );
        match status {
            Ok(CredentialStatus::Ready | CredentialStatus::ExpiredOrRefreshNeeded) => {
                ok.push("Linear credential stored".into())
            }
            Ok(other) => problems.push(format!(
                "Linear credential is {other}; run the login action"
            )),
            Err(error) => problems.push(format!("Linear credential: {error}")),
        }
        let budget = if stored {
            linear_budget(ctx, config).await
        } else {
            None
        };
        ok.push(format!(
            "Linear budget: {}",
            budget.as_deref().unwrap_or("unknown")
        ));
    }
    match ticker::lock_state(&ctx.state_dir()) {
        ticker::LockState::Held(info) if info.version == crate::VERSION => {
            ok.push("ticker running".into())
        }
        ticker::LockState::Held(info) => problems.push(format!(
            "the ticker runs {}, this binary is {}",
            info.version,
            crate::VERSION
        )),
        ticker::LockState::Free => problems.push("the ticker is not running".into()),
    }
    let active = Run::list(&ctx.runs_dir())
        .iter()
        .filter(|r| r.record().is_ok_and(|r| r.status == Status::Active))
        .count();
    ok.push(format!("{active} active run(s)"));
    let mut text = if problems.is_empty() {
        "All checks passed.".to_string()
    } else {
        format!(
            "{} problem(s):\n- {}",
            problems.len(),
            problems.join("\n- ")
        )
    };
    text.push_str(&format!("\nOK:\n- {}", ok.join("\n- ")));
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::FakeHerdr;
    use crate::paths::Env;
    use crate::process::fake::FakeRunner;
    use crate::run::RunRecord;

    #[test]
    fn issue_urls_give_their_key() {
        assert_eq!(
            key_from_url("https://linear.app/acme/issue/DATA-12/fix-login"),
            Some("DATA-12".into())
        );
        assert_eq!(
            key_from_url("https://linear.app/acme/issue/DATA-12"),
            Some("DATA-12".into())
        );
        assert_eq!(key_from_url("https://linear.app/acme/project/x"), None);
        assert_eq!(
            key_from_url("https://evil.example/acme/issue/DATA-12"),
            None
        );
    }

    #[tokio::test]
    async fn status_lists_runs_and_panes_map_to_their_run() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: false,
        };
        assert!(status_text(&ctx).contains("no runs"));
        std::fs::create_dir_all(ctx.runs_dir()).unwrap();
        let run = Run::create(
            &ctx.runs_dir(),
            RunRecord {
                identifier: "DATA-1".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        run.update(|r| {
            r.coordinator.status = AgentStatus::Open;
            r.coordinator.cwd = "/state/runs/DATA-1".into();
            r.coordinator.last_state = "idle".into();
        })
        .unwrap();
        worker::allocate(
            &run,
            |_| Ok(()),
            |w| {
                w.repo = "api".into();
                w.worktree_path = "/wt/api".into();
                w.agent.status = AgentStatus::Open;
                w.agent.last_group = "working".into();
            },
        )
        .unwrap();
        let text = status_text(&ctx);
        assert!(
            text.contains("DATA-1 Active: coordinator idle; w1 working (api)"),
            "{text}"
        );
        assert_eq!(run_for_cwd(&ctx, "/wt/api/src").unwrap().key, "DATA-1");
        assert_eq!(
            run_for_cwd(&ctx, "/state/runs/DATA-1").unwrap().key,
            "DATA-1"
        );
        assert!(run_for_cwd(&ctx, "/elsewhere").is_none());

        pause(&ctx, true).await.unwrap();
        assert!(status_text(&ctx).contains("paused"));
        pause(&ctx, false).await.unwrap();
        assert!(!status_text(&ctx).contains("paused"));
    }

    #[tokio::test]
    async fn focus_prefers_the_coordinators_live_workspace() {
        let home = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(home.path());
        let placed = herdr.workspace_create("/run", "DATA-1 x").await.unwrap();
        let coordinator = crate::run::AgentRecord {
            status: AgentStatus::Open,
            kind: "claude".into(),
            agent_name: "data-1-coordinator".into(),
            pane_id: "w1:p1".into(),
            // Recorded before Herdr renumbered the workspace.
            workspace_id: "w9".into(),
            cwd: "/run".into(),
            ..Default::default()
        };
        focus_coordinator(&herdr, &coordinator).await.unwrap();
        herdr
            .agent_start("data-1-coordinator", "claude", &placed.pane, &[])
            .await
            .unwrap();
        focus_coordinator(&herdr, &coordinator).await.unwrap();
        assert_eq!(
            herdr.focused(),
            [WorkspaceId("w9".into()), placed.workspace]
        );
    }
}
