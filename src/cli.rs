//! The command line. People use Herdr actions and Linear; the commands here
//! are for the plugin manifest, the ticker and the agents it starts, so every
//! name is spelled out in full.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::actions::{self, Action};
use crate::commands::{self, Session, WorkerStart};
use crate::config::Config;
use crate::files::read_text_arg;
use crate::herdr;
use crate::paths::{Ctx, Env};
use crate::process::RealRunner;
use crate::{progress, ticker};

#[derive(Parser)]
#[command(name = "herdr-linear-agent", version = crate::VERSION, about = "Run a coordinator and per-repository workers for Linear issues delegated to this plugin's Linear app user")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The plugin's startup hook: starts the ticker detached and exits.
    Startup,
    /// A Herdr action.
    Action {
        action: Action,
        /// For `login`: log in to this workspace of the config again.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Manage the background ticker.
    Ticker {
        #[command(subcommand)]
        command: TickerCommand,
    },
    /// Browse past runs, their files and their agents' transcripts.
    History,
    /// Print the coordinator sheet.
    Skill {
        /// The issue key to fill into the commands.
        key: Option<String>,
    },
    /// Print a run's digest: issue, conversation, catalog, profiles, workers and inbox.
    Context { key: String },
    /// Mark inbox items as handled.
    Inbox {
        #[command(subcommand)]
        command: InboxCommand,
    },
    /// Replace the plan shown in the Linear session.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Post a progress note to the Linear session.
    Say(TextArgs),
    /// Ask a person a question in the Linear session.
    Ask {
        #[command(flatten)]
        text: TextArgs,
        /// A choice, as `label=value`; repeatable.
        #[arg(long = "option", value_parser = parse_option)]
        options: Vec<(String, String)>,
    },
    /// Post the final summary and move the issue to review.
    Finish(TextArgs),
    /// Start, prompt or restart workers.
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    /// Report a worker's progress from its own pane.
    Report(ReportArgs),
    /// Tools for checking the plugin against a real Herdr.
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}

#[derive(Subcommand)]
enum DebugCommand {
    /// Follow the Herdr session and print one JSON line per wake.
    HerdrWatch {
        /// The socket to watch; the configured session's by default.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum TickerCommand {
    /// Start the ticker unless one of this version is running.
    Start,
    /// Ask the running ticker to exit.
    Stop,
    /// Show whether a ticker is running.
    Status,
    /// Run the loop in the foreground (the detached process runs this).
    Run,
}

#[derive(Subcommand)]
enum InboxCommand {
    Done {
        key: String,
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    Set {
        key: String,
        /// A Markdown checklist; `-` reads standard input.
        #[arg(long)]
        file: String,
    },
}

#[derive(Args)]
struct TextArgs {
    key: String,
    /// The text; `-` reads standard input.
    #[arg(long)]
    text_file: String,
}

#[derive(Subcommand)]
enum WorkerCommand {
    /// Start a worker in a new worktree of a catalog repository.
    Start {
        key: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        profile: String,
        #[arg(long)]
        title: String,
        /// The task; `-` reads standard input.
        #[arg(long)]
        task_file: String,
    },
    /// Send a worker a follow-up.
    Prompt {
        key: String,
        id: String,
        #[arg(long)]
        text_file: String,
    },
    /// Start a worker again in its worktree, optionally with another profile.
    Restart {
        key: String,
        id: String,
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Args)]
struct ReportArgs {
    /// Rough progress of the whole task, 0 to 100.
    #[arg(long, conflicts_with = "unknown")]
    percent: Option<u8>,
    /// The scope is not clear yet.
    #[arg(long)]
    unknown: bool,
    /// Two to four words, for example `Testing changes` or `Waiting for you`.
    #[arg(long)]
    activity: String,
}

fn parse_option(text: &str) -> Result<(String, String), String> {
    commands::parse_option(text).map_err(|e| e.to_string())
}

pub async fn run() -> Result<()> {
    let command = Cli::parse().command;
    let env = Env::from_process()?;
    let runner = RealRunner;
    let ctx = Ctx {
        env: &env,
        runner: &runner,
        detached_ticker: true,
    };
    match command {
        Command::Debug {
            command: DebugCommand::HerdrWatch { socket },
        } => herdr_watch(socket).await,
        Command::Startup => ticker::start(&ctx).await,
        Command::History => {
            let (env, runs) = (env.clone(), ctx.runs_dir());
            let session = Config::load(&ctx.config_dir())
                .ok()
                .and_then(|c| c.herdr.session);
            tokio::task::spawn_blocking(move || crate::history::run(&env, &runs, session)).await?
        }
        Command::Action { action, workspace } => {
            actions::run(&ctx, action, workspace.as_deref()).await
        }
        Command::Ticker { command } => match command {
            TickerCommand::Start => ticker::start(&ctx).await,
            TickerCommand::Stop => ticker::stop(&ctx.state_dir()).await,
            TickerCommand::Status => {
                println!("{}", ticker::describe(&ctx.state_dir()));
                Ok(())
            }
            TickerCommand::Run => ticker::run(&ctx).await,
        },
        Command::Skill { key } => commands::skill(key.as_deref()),
        Command::Context { key } => {
            // Without Herdr the digest shows the recorded groups.
            let session = Session::configured(&ctx).await.ok();
            commands::context(&ctx, session.as_ref(), &key).await
        }
        Command::Inbox {
            command: InboxCommand::Done { key, ids, all },
        } => commands::inbox_done(&ctx, &key, &ids, all),
        Command::Plan {
            command: PlanCommand::Set { key, file },
        } => commands::plan_set(&ctx, &key, &read_text_arg(&file)?).await,
        Command::Say(args) => {
            commands::say(&ctx, &args.key, &read_text_arg(&args.text_file)?).await
        }
        Command::Ask { text, options } => {
            commands::ask(&ctx, &text.key, &read_text_arg(&text.text_file)?, &options).await
        }
        Command::Finish(args) => {
            let text = read_text_arg(&args.text_file)?;
            let session = Session::configured(&ctx).await?;
            commands::finish(&ctx, &session, &args.key, &text).await
        }
        Command::Worker { command } => worker(&ctx, command).await,
        Command::Report(args) => {
            progress::report(&env, args.percent.filter(|_| !args.unknown), &args.activity).await
        }
    }
}

async fn worker(ctx: &Ctx<'_>, command: WorkerCommand) -> Result<()> {
    let session = Session::configured(ctx).await?;
    match command {
        WorkerCommand::Start {
            key,
            repo,
            profile,
            title,
            task_file,
        } => {
            let args = WorkerStart {
                repo,
                profile,
                title,
                task: read_text_arg(&task_file)?,
            };
            commands::worker_start(ctx, &session, &key, &args)
                .await
                .map(|_| ())
        }
        WorkerCommand::Prompt { key, id, text_file } => {
            let text = read_text_arg(&text_file)?;
            commands::worker_prompt(ctx, &session, &key, &id, &text).await
        }
        WorkerCommand::Restart { key, id, profile } => {
            commands::worker_restart(ctx, &session, &key, &id, profile.as_deref())
                .await
                .map(|_| ())
        }
    }
}

async fn herdr_watch(socket: Option<PathBuf>) -> Result<()> {
    let socket = match socket {
        Some(socket) => socket,
        None => {
            let env = Env::from_process()?;
            let config = Config::load(&env.config_dir())?;
            herdr::session_socket(&env.herdr_bin(), config.herdr.session.as_deref()).await?
        }
    };
    let client = herdr::Client::new(socket);
    let mut rx = herdr::wake(client.clone());
    let mut printed = None;
    while rx.changed().await.is_ok() {
        let link = rx.borrow_and_update().clone();
        // A new error alone is not a wake.
        if printed == Some((link.connected, link.wakes)) {
            continue;
        }
        printed = Some((link.connected, link.wakes));
        let snapshot = client.snapshot().await.ok();
        let agents: Vec<_> = snapshot
            .iter()
            .flat_map(|s| &s.agents)
            .map(|a| {
                serde_json::json!({
                    "pane": a.pane, "name": a.name, "status": a.status, "seq": a.state_change_seq,
                })
            })
            .collect();
        let line = serde_json::json!({
            "at": jiff::Timestamp::now().to_string(),
            "connected": link.connected,
            "wakes": link.wakes,
            "panes": snapshot.as_ref().map(|s| s.panes.len()),
            "agents": agents,
        });
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_manifest_names_only_existing_actions() {
        let manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
        for action in manifest["actions"].as_array().unwrap() {
            let command: Vec<&str> = action["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let mut argv = vec!["hla"];
            argv.extend(&command[1..]);
            assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?}");
            assert_eq!(command[2], action["id"].as_str().unwrap());
        }
        let panes = manifest["panes"].as_array().unwrap();
        for pane in panes {
            let command: Vec<&str> = pane["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let mut argv = vec!["hla"];
            argv.extend(&command[1..]);
            assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?}");
        }
        assert!(
            panes.iter().any(|p| p["id"].as_str() == Some("history")),
            "the browse action opens the history pane"
        );
        // Herdr refuses a manifest whose link handler has no title (0.9.1).
        for handler in manifest["link_handlers"].as_array().unwrap() {
            assert!(
                handler["title"].as_str().is_some_and(|t| !t.is_empty()),
                "{handler}"
            );
        }
    }

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
        let parsed = Cli::try_parse_from([
            "hla",
            "ask",
            "DATA-1",
            "--text-file",
            "-",
            "--option",
            "Yes=yes",
            "--option",
            "No",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Command::Ask { ref options, .. } if options.len() == 2));
        assert!(Cli::try_parse_from(["hla", "action", "open-issue"]).is_ok());
        assert!(Cli::try_parse_from(["hla", "action", "logout"]).is_err());
        assert!(Cli::try_parse_from(["hla", "debug", "herdr-watch", "--socket", "/s"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "hla",
                "worker",
                "start",
                "DATA-1",
                "--repo",
                "api",
                "--profile",
                "standard",
                "--title",
                "T",
                "--task-file",
                "-"
            ])
            .is_ok()
        );
    }
}
