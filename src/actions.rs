//! The Herdr actions people use. An action's output only reaches the plugin
//! log, so each action also shows its result as a Herdr notification.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::herdr::{self, Client, Herdr, HerdrError, WorkspaceId};
use crate::linear::client::LinearApi;
use crate::linear::credentials::{self, CredentialManager, CredentialStatus};
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
        let socket = herdr::invoking_socket(ctx.env, session.as_deref()).await?;
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
    Reload,
    Browse,
}

/// `workspace` names the one workspace `login` logs in to again; the other
/// actions take none.
pub async fn run(ctx: &Ctx<'_>, action: Action, workspace: Option<&str>) -> Result<()> {
    if workspace.is_some() && !matches!(action, Action::Login) {
        bail!("--workspace applies only to the login action");
    }
    let result = match action {
        Action::Login => login(ctx, workspace).await,
        Action::Status => Ok(status_text(ctx)),
        Action::OpenIssue => open_issue(ctx).await,
        Action::FocusRun => focus_run(ctx).await,
        Action::Pause => pause(ctx, true).await,
        Action::Resume => pause(ctx, false).await,
        Action::Doctor => doctor(ctx).await,
        Action::Reload => reload(ctx).await,
        // The pane is the answer; only a failure is told.
        Action::Browse => match browse(ctx).await {
            Ok(()) => return Ok(()),
            Err(error) => Err(error),
        },
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

/// Opens the plugin's `history` pane over the focused one.
async fn browse(ctx: &Ctx<'_>) -> Result<()> {
    let session = Config::load(&ctx.config_dir())?.herdr.session;
    let socket = herdr::invoking_socket(ctx.env, session.as_deref()).await?;
    let params = serde_json::json!({ "plugin_id": TITLE, "entrypoint": "history", "focus": true });
    Client::new(socket)
        .call::<serde_json::Value>("plugin.pane.open", params)
        .await?;
    Ok(())
}

/// The workspace's stored credential, read without the network.
async fn credential_status(
    ctx: &Ctx<'_>,
    name: &str,
) -> Result<CredentialStatus, credentials::CredentialError> {
    let (name, lock) = (
        name.to_string(),
        credentials::lock_path(&ctx.state_dir(), name),
    );
    blocking(move || CredentialManager::production_status(&name, lock).map(|mut m| m.status()))
        .await
}

fn stored(status: &Result<CredentialStatus, credentials::CredentialError>) -> bool {
    matches!(
        status,
        Ok(CredentialStatus::Ready | CredentialStatus::ExpiredOrRefreshNeeded)
    )
}

/// Logs in to `only`, or else to every workspace without a stored
/// credential, or to all of them again when each has one. Each login opens
/// the browser in turn.
async fn login(ctx: &Ctx<'_>, only: Option<&str>) -> Result<String> {
    let config = Config::load(&ctx.config_dir())?;
    let names: Vec<String> = match only {
        Some(name) => {
            config.workspace(name)?;
            vec![name.to_string()]
        }
        None => {
            let mut missing = Vec::new();
            for name in config.workspaces.keys() {
                if !stored(&credential_status(ctx, name).await) {
                    missing.push(name.clone());
                }
            }
            if missing.is_empty() {
                config.workspaces.keys().cloned().collect()
            } else {
                missing
            }
        }
    };
    let mut lines = Vec::new();
    for name in &names {
        let line = login_workspace(ctx, name, config.workspace(name)?)
            .await
            .with_context(|| format!("workspace `{name}`"))?;
        lines.push(line);
    }
    ticker::start(ctx).await?;
    Ok(lines.join("\n"))
}

/// Authorizes the workspace's app in the browser, stores the token and checks
/// that the token acts as an app user. A stored credential is revoked and
/// replaced.
async fn login_workspace(
    ctx: &Ctx<'_>,
    name: &str,
    workspace: &crate::config::Workspace,
) -> Result<String> {
    let state_dir = ctx.ensure_state_dir()?;
    let lock = credentials::lock_path(&state_dir, name);
    let (account, client_id, port) = (
        name.to_string(),
        workspace.client_id.clone(),
        workspace.callback_port,
    );
    blocking(move || -> Result<()> {
        let mut manager = CredentialManager::production(&account, client_id, port, lock)?;
        if manager.status() != CredentialStatus::SignedOut {
            manager
                .logout(true)
                .context("could not revoke the stored credential")?;
        }
        manager.login()?;
        Ok(())
    })
    .await?;
    let linear = crate::linear::client::Client::production(name, workspace, &state_dir).await?;
    let viewer = linear.viewer().await?;
    Ok(format!(
        "Logged in to the Linear workspace `{name}` as the app user {}.",
        if viewer.name.is_empty() {
            viewer.id
        } else {
            viewer.name
        }
    ))
}

/// Checks the config and asks the running ticker to load it again, which
/// starts its tasks over and reads the credentials again; without a running
/// ticker, starts one, which loads it anyway.
async fn reload(ctx: &Ctx<'_>) -> Result<String> {
    Config::load(&ctx.config_dir())?;
    let state_dir = ctx.state_dir();
    if matches!(ticker::lock_state(&state_dir), ticker::LockState::Free) {
        ticker::start(ctx).await?;
        return Ok("The ticker was not running; it started with the config.".into());
    }
    let request = ticker::reload_path(&state_dir);
    std::fs::write(&request, b"")
        .with_context(|| format!("could not write {}", request.display()))?;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !request.exists() {
            return Ok("Reloaded the config: the ticker's tasks start again with it.".into());
        }
    }
    Ok("Asked the ticker to reload the config; it has not taken the request yet.".into())
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
            run.key, record.status
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

/// The organization's URL key and the issue key in a Linear issue URL.
pub fn issue_from_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://linear.app/")?;
    let mut parts = rest.split('/');
    let (organization, kind, key) = (parts.next()?, parts.next()?, parts.next()?);
    (kind == "issue" && !organization.is_empty() && crate::run::validate_issue_key(key).is_ok())
        .then(|| (organization.to_string(), key.to_string()))
}

async fn focus_run(ctx: &Ctx<'_>) -> Result<String> {
    let url = ctx
        .env
        .var("HERDR_PLUGIN_CLICKED_URL")
        .context("Ctrl-click a Linear issue link to focus its run")?;
    let issue = issue_from_url(url).with_context(|| format!("{url} is not a Linear issue URL"))?;
    // Issue keys repeat across workspaces; the organization in the URL tells
    // them apart.
    let (run, record) = Run::list(&ctx.runs_dir())
        .into_iter()
        .filter_map(|run| run.record().ok().map(|record| (run, record)))
        .find(|(_, record)| issue_from_url(&record.url).as_ref() == Some(&issue))
        .with_context(|| format!("there is no run for {}", issue.1))?;
    let key = run.key;
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
async fn linear_budget(
    ctx: &Ctx<'_>,
    name: &str,
    workspace: &crate::config::Workspace,
) -> Option<String> {
    let linear = crate::linear::client::Client::production(name, workspace, &ctx.state_dir())
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
        for (name, workspace) in &config.workspaces {
            let status = credential_status(ctx, name).await;
            match &status {
                Ok(CredentialStatus::Ready | CredentialStatus::ExpiredOrRefreshNeeded) => {
                    ok.push(format!("Linear `{name}`: credential stored"))
                }
                Ok(other) => problems.push(format!(
                    "Linear `{name}`: credential is {other}; run the login action"
                )),
                Err(error) => problems.push(format!("Linear `{name}`: credential: {error}")),
            }
            let budget = if stored(&status) {
                linear_budget(ctx, name, workspace).await
            } else {
                None
            };
            ok.push(format!(
                "Linear `{name}`: budget {}",
                budget.as_deref().unwrap_or("unknown")
            ));
        }
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
    fn issue_urls_give_their_organization_and_key() {
        let issue = |org: &str, key: &str| Some((org.to_string(), key.to_string()));
        assert_eq!(
            issue_from_url("https://linear.app/acme/issue/DATA-12/fix-login"),
            issue("acme", "DATA-12")
        );
        assert_eq!(
            issue_from_url("https://linear.app/beta/issue/DATA-12"),
            issue("beta", "DATA-12")
        );
        assert_eq!(issue_from_url("https://linear.app/acme/project/x"), None);
        assert_eq!(
            issue_from_url("https://evil.example/acme/issue/DATA-12"),
            None
        );
    }

    #[tokio::test]
    async fn reload_checks_the_config_then_asks_the_running_ticker() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: false,
        };
        let config_dir = ctx.config_dir();
        crate::config::tests::write_sample(&config_dir, "not toml [");
        let state_dir = ctx.ensure_state_dir().unwrap();
        let _lock = ticker::acquire(&state_dir).await.unwrap();
        assert!(reload(&ctx).await.is_err());
        assert!(
            !ticker::reload_path(&state_dir).exists(),
            "nothing is asked of the ticker for a config that does not load"
        );

        crate::config::tests::write_sample(&config_dir, crate::config::tests::SAMPLE);
        let request = ticker::reload_path(&state_dir);
        let taker = tokio::spawn(async move {
            while std::fs::remove_file(&request).is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        assert_eq!(
            reload(&ctx).await.unwrap(),
            "Reloaded the config: the ticker's tasks start again with it."
        );
        taker.await.unwrap();
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
                workspace: "acme".into(),
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
        assert_eq!(run_for_cwd(&ctx, "/wt/api/src").unwrap().key, "acme/DATA-1");
        assert_eq!(
            run_for_cwd(&ctx, "/state/runs/DATA-1").unwrap().key,
            "acme/DATA-1"
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
            agent_name: "acme-data-1-coordinator".into(),
            pane_id: "w1:p1".into(),
            // Recorded before Herdr renumbered the workspace.
            workspace_id: "w9".into(),
            cwd: "/run".into(),
            ..Default::default()
        };
        focus_coordinator(&herdr, &coordinator).await.unwrap();
        herdr
            .agent_start("acme-data-1-coordinator", "claude", &placed.pane, &[])
            .await
            .unwrap();
        focus_coordinator(&herdr, &coordinator).await.unwrap();
        assert_eq!(
            herdr.focused(),
            [WorkspaceId("w9".into()), placed.workspace]
        );
    }
}
