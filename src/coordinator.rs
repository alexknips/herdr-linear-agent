//! The coordinator's side of a run: the priming files in the run folder, the
//! sheet (`skill`) and the digest it reads every turn (`context`).
//!
//! A coordinator is primed by `AGENTS.md` in its working directory, the run
//! folder. Claude Code reads `CLAUDE.md` (a link to `AGENTS.md`), Codex and
//! Cursor Agent read `AGENTS.md`. Nothing is installed as a skill: the sheet is
//! embedded in the binary, so it always matches the commands it describes.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;

use crate::config::{Config, Instructions};
use crate::files::{self, shell_quote, write_atomic};
use crate::herdr::Snapshot;
use crate::run::{AgentRecord, AgentStatus, Run, RunRecord};
use crate::worker::{self, Group};
use crate::{inbox, names};

/// The subcommands a coordinator may run without a permission prompt. The
/// plugin's own subcommands (`startup`, `action`, `ticker`) are left out.
pub const ALLOWED_SUBCOMMANDS: [&str; 8] = [
    "skill",
    "context",
    "inbox done",
    "plan",
    "say",
    "ask",
    "worker",
    "finish",
];

/// The binary's absolute path, quoted for a shell.
pub fn binary_command() -> Result<String> {
    Ok(shell_quote(&crate::paths::binary()?.to_string_lossy()))
}

pub fn sheet(bin: &str, key: &str) -> String {
    include_str!("../assets/COORDINATOR.md")
        .replace("{bin}", bin)
        .replace("{key}", key)
}

pub fn agents_md(bin: &str, record: &RunRecord, instructions: &[Instructions]) -> String {
    format!(
        "# herdr-linear-agent run {key}\n\n\
         If your working directory is this folder, you are the coordinator of the herdr-linear-agent run for the Linear issue {issue} ({title}).\n\n\
         Run `{bin} skill {key}` now and follow the sheet it prints. Then run `{bin} context {key}` at the start of every turn.\n",
        key = crate::run::run_key(&record.workspace, &record.identifier),
        issue = record.identifier,
        title = record.title.replace('\n', " "),
    ) + &profile_section(&record.coordinator.profile, "the sheet", instructions)
}

/// A profile's `instructions`, after the built-in rules they may not
/// override: one layer as it is, several under a heading each from the most
/// general to the most specific; nothing when there are none.
pub fn profile_section(profile: &str, rules: &str, layers: &[Instructions]) -> String {
    match layers {
        [] => String::new(),
        [one] => format!(
            "\n## Profile instructions\n\n\
             These come from the `{profile}` profile in the plugin's config and add to {rules}. \
             Where they disagree with {rules}, follow {rules}.\n\n{}\n",
            one.text.trim()
        ),
        _ => {
            let mut text = format!(
                "\n## Profile instructions\n\n\
                 These come from the `{profile}` profile and the profiles it is based on, \
                 from the most general to the most specific, and add to {rules}. \
                 Where they disagree with each other, follow the later one. \
                 Where they disagree with {rules}, follow {rules}.\n"
            );
            for layer in layers {
                text += &format!(
                    "\n### From the `{}` profile\n\n{}\n",
                    layer.profile,
                    layer.text.trim()
                );
            }
            text
        }
    }
}

pub fn settings_local(bin: &str) -> serde_json::Value {
    let allow: Vec<String> = ALLOWED_SUBCOMMANDS
        .iter()
        .map(|sub| format!("Bash({bin} {sub}:*)"))
        .collect();
    serde_json::json!({ "permissions": { "allow": allow } })
}

/// Writes `AGENTS.md`, the `CLAUDE.md` link and the allow-list. Rewritten at
/// every launch, so an updated binary's path is what the coordinator sees.
pub fn write_priming(
    run: &Run,
    record: &RunRecord,
    bin: &str,
    instructions: &[Instructions],
) -> Result<()> {
    write_atomic(
        &run.dir.join("AGENTS.md"),
        agents_md(bin, record, instructions).as_bytes(),
    )?;
    let claude = run.dir.join("CLAUDE.md");
    if std::fs::read_link(&claude).ok().as_deref() != Some(Path::new("AGENTS.md")) {
        let _ = std::fs::remove_file(&claude);
        std::os::unix::fs::symlink("AGENTS.md", &claude)?;
    }
    files::write_json(
        &run.dir.join(".claude/settings.local.json"),
        &settings_local(bin),
    )?;
    std::fs::create_dir_all(run.dir.join(".cursor"))?;
    files::write_json(&run.dir.join(".cursor/cli.json"), &cursor_cli(bin))
}

/// Cursor Agent's project permissions: the coordinator may run the plugin's
/// binary without asking. Cursor matches the command as typed, so the entry
/// is the path the sheet tells the coordinator to run.
pub fn cursor_cli(bin: &str) -> serde_json::Value {
    serde_json::json!({ "permissions": { "allow": [format!("Shell({bin})")], "deny": [] } })
}

/// The coordinator record before its workspace exists.
pub fn pending_record(record: &RunRecord, profile: &str, kind: &str) -> AgentRecord {
    AgentRecord {
        status: AgentStatus::Pending,
        profile: profile.to_string(),
        kind: kind.to_string(),
        agent_name: names::agent_name(
            &crate::run::run_key(&record.workspace, &record.identifier),
            &record.issue_id,
            "coordinator",
        ),
        ..AgentRecord::default()
    }
}

pub fn workspace_label(record: &RunRecord) -> String {
    let title: String = record
        .title
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    format!(
        "{} {title}",
        crate::run::run_key(&record.workspace, &record.identifier)
    )
}

pub fn launch_prompt(key: &str, resume: bool) -> String {
    if resume {
        format!(
            "[herdr-linear-agent ticker] You were restarted as the coordinator of {key}. Run context."
        )
    } else {
        format!("[herdr-linear-agent ticker] Start {key}. Follow AGENTS.md.")
    }
}

pub const NUDGE_REPLY: &str =
    "[herdr-linear-agent ticker] There is a new reply in Linear. Run context.";
pub const NUDGE_INBOX: &str = "[herdr-linear-agent ticker] There are new inbox items. Run context.";

/// A Herdr snapshot and what reading a worker's live state needs besides.
pub struct View<'a> {
    pub snapshot: &'a Snapshot,
    pub state_dir: &'a Path,
    pub socket: &'a str,
    pub now: Timestamp,
}

/// One worker line of the digest.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerRow {
    pub id: String,
    pub title: String,
    pub repo: String,
    /// `Stopped`, `Starting`, or the group label.
    pub state: &'static str,
    pub pr_url: String,
}

/// The workers' groups, computed live from `view`; without one (Herdr is
/// unreachable) they come from each record's `last_group`.
pub fn worker_rows(run: &Run, view: Option<&View>) -> Vec<WorkerRow> {
    worker::list(run)
        .into_iter()
        .map(|w| {
            let group = match (w.agent.status, view) {
                (AgentStatus::Stopped, _) => None,
                (_, Some(v)) => Some(worker::group(
                    &w,
                    &worker::live_state(&w.agent, v.snapshot, v.now, v.state_dir, v.socket),
                )),
                (AgentStatus::Failed, None) => Some(Group::WaitingOnYou),
                (_, None) => Group::from_token(&w.agent.last_group),
            };
            let state = match (w.agent.status, group) {
                (AgentStatus::Stopped, _) => "Stopped",
                (_, Some(group)) => group.label(),
                (_, None) => "Starting",
            };
            WorkerRow {
                id: w.id,
                title: w.title,
                repo: w.repo,
                state,
                pr_url: w.pr_url,
            }
        })
        .collect()
}

fn read_or(path: &Path, missing: &str) -> String {
    std::fs::read_to_string(path)
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|_| missing.to_string())
}

/// What the coordinator reads at the start of every turn, and the inbox ids
/// it shows.
pub fn digest(
    run: &Run,
    config: &Config,
    bin: &str,
    rows: &[WorkerRow],
) -> Result<(String, Vec<String>)> {
    let record = run.record()?;
    let mut text = format!(
        "# Run {} ({:?}{})\n\n## Issue\n\n{}\n\n## Conversation\n\n{}\n\n## Repositories\n\n",
        run.key,
        record.status,
        if record.finished { ", finished" } else { "" },
        read_or(&run.issue_md(), "(issue.md is not written yet)"),
        read_or(&run.conversation_md(), "(no replies yet)"),
    );
    if config.repositories.is_empty() {
        text.push_str("(none)\n");
    }
    for (name, repo) in &config.repositories {
        let _ = write!(
            text,
            "- {name}: {} (base {})",
            repo.path.display(),
            repo.base
        );
        if !repo.description.is_empty() {
            let _ = write!(text, " \u{2014} {}", repo.description);
        }
        text.push('\n');
    }
    text.push_str("\n## Worker profiles\n\n");
    // A team no longer in the config has no worker profiles.
    let workers = config
        .routing_of(&record.workspace, &record.team_key)
        .map(|r| r.workers.clone())
        .unwrap_or_default();
    for name in &workers {
        let Ok(profile) = config.profile(name) else {
            continue;
        };
        let _ = write!(text, "- {name}: {}", profile.kind);
        if let Some(model) = &profile.model {
            let _ = write!(text, " {model}");
        }
        if let Some(effort) = &profile.effort {
            let _ = write!(text, ", effort {effort}");
        }
        if !profile.description.is_empty() {
            let _ = write!(text, " \u{2014} {}", profile.description);
        }
        text.push('\n');
    }
    text.push_str("\n## Workers\n\n");
    if rows.is_empty() {
        let _ = writeln!(
            text,
            "(none yet; start one with `{bin} worker start {} --repo <name> ...`)",
            run.key
        );
    }
    for row in rows {
        let _ = write!(
            text,
            "- {} [{}] {} (repo {})",
            row.id, row.state, row.title, row.repo
        );
        if !row.pr_url.is_empty() {
            let _ = write!(text, ", PR {}", row.pr_url);
        }
        text.push('\n');
    }
    text.push_str("\n## Inbox\n\n");
    let items = inbox::unhandled(run);
    if items.is_empty() {
        text.push_str("(empty)\n");
    }
    for item in &items {
        let _ = writeln!(
            text,
            "- {} [{}] {}: {}",
            item.id, item.kind, item.subject, item.summary
        );
    }
    if !items.is_empty() {
        let _ = writeln!(
            text,
            "\nWhen you have handled them, run `{bin} inbox done {} --all` (or name the ids).",
            run.key
        );
    }
    Ok((text, items.into_iter().map(|i| i.id).collect()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::herdr::{Agent, AgentStatus as Seen, Pane, PaneId, WorkspaceId};

    const BIN: &str = "/opt/hla/bin/herdr-linear-agent";

    struct Folder {
        _home: tempfile::TempDir,
        run: Run,
        record: RunRecord,
    }

    fn folder(title: &str) -> Folder {
        let home = tempfile::tempdir().unwrap();
        let record = RunRecord {
            workspace: "acme".into(),
            issue_id: "0b7c6c1e-issue".into(),
            identifier: "DATA-1".into(),
            title: title.into(),
            url: "https://linear.app/acme/issue/DATA-1".into(),
            team_key: "DATA".into(),
            ..RunRecord::default()
        };
        let run = Run::create(&home.path().join("runs"), record.clone()).unwrap();
        Folder {
            _home: home,
            run,
            record,
        }
    }

    fn sample_config() -> Config {
        crate::config::tests::sample()
    }

    /// Each needle appears after the previous one.
    fn assert_in_order(text: &str, needles: &[&str]) {
        let mut from = 0;
        for needle in needles {
            let Some(at) = text[from..].find(needle) else {
                panic!("`{needle}` is missing after byte {from} of:\n{text}");
            };
            from += at + needle.len();
        }
    }

    #[test]
    fn several_instruction_layers_get_a_heading_each_from_the_most_general() {
        let layer = |profile: &str, text: &str| Instructions {
            profile: profile.into(),
            text: text.into(),
        };
        assert_eq!(
            profile_section(
                "leaf",
                "the sheet",
                &[
                    layer("root", "Root rule.\n"),
                    layer("leaf", " Leaf rule. \n")
                ]
            ),
            "\n## Profile instructions\n\n\
             These come from the `leaf` profile and the profiles it is based on, from the most general \
             to the most specific, and add to the sheet. Where they disagree with each other, follow \
             the later one. Where they disagree with the sheet, follow the sheet.\n\n\
             ### From the `root` profile\n\nRoot rule.\n\n\
             ### From the `leaf` profile\n\nLeaf rule.\n"
        );
        assert_eq!(profile_section("leaf", "the sheet", &[]), "");
        assert_eq!(
            profile_section("leaf", "the sheet", &[layer("root", "Root rule.\n")]),
            "\n## Profile instructions\n\n\
             These come from the `leaf` profile in the plugin's config and add to the sheet. \
             Where they disagree with the sheet, follow the sheet.\n\nRoot rule.\n"
        );
    }

    #[test]
    fn priming_points_the_coordinator_at_the_binary_and_allows_only_agent_commands() {
        let f = folder("Fix the\nlogin");
        std::os::unix::fs::symlink("elsewhere.md", f.run.dir.join("CLAUDE.md")).unwrap();
        write_priming(&f.run, &f.record, BIN, &[]).unwrap();

        let agents = std::fs::read_to_string(f.run.dir.join("AGENTS.md")).unwrap();
        let expected = [
            "# herdr-linear-agent run acme/DATA-1",
            "",
            "If your working directory is this folder, you are the coordinator of the \
             herdr-linear-agent run for the Linear issue DATA-1 (Fix the login).",
            "",
            "Run `/opt/hla/bin/herdr-linear-agent skill acme/DATA-1` now and follow the sheet it \
             prints. Then run `/opt/hla/bin/herdr-linear-agent context acme/DATA-1` at the start \
             of every turn.",
        ];
        assert_eq!(agents, format!("{}\n", expected.join("\n")));
        assert_eq!(
            std::fs::read_link(f.run.dir.join("CLAUDE.md")).unwrap(),
            Path::new("AGENTS.md")
        );

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(f.run.dir.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        let allowed: Vec<String> = [
            "skill",
            "context",
            "inbox done",
            "plan",
            "say",
            "ask",
            "worker",
            "finish",
        ]
        .iter()
        .map(|sub| format!("Bash(/opt/hla/bin/herdr-linear-agent {sub}:*)"))
        .collect();
        assert_eq!(
            settings,
            serde_json::json!({"permissions": {"allow": allowed}})
        );
        for plugin_only in ["startup", "action", "ticker"] {
            assert!(!settings.to_string().contains(plugin_only), "{plugin_only}");
        }
        let cursor: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(f.run.dir.join(".cursor/cli.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            cursor,
            serde_json::json!({"permissions": {
                "allow": ["Shell(/opt/hla/bin/herdr-linear-agent)"],
                "deny": []
            }})
        );

        // A second placement keeps the link and rewrites the text.
        write_priming(&f.run, &f.record, "/usr/local/bin/hla", &[]).unwrap();
        assert!(
            std::fs::read_to_string(f.run.dir.join("CLAUDE.md"))
                .unwrap()
                .contains("Run `/usr/local/bin/hla skill acme/DATA-1`")
        );
    }

    #[test]
    fn the_sheet_fills_in_the_binary_and_the_key() {
        let filled = sheet("/bin/hla", "DATA-7");
        assert!(
            filled.contains("`/bin/hla worker start DATA-7 --repo "),
            "{filled}"
        );
        assert!(!filled.contains("{bin}") && !filled.contains("{key}"));
        assert!(sheet("/bin/hla", "<ISSUE-KEY>").contains("worker start <ISSUE-KEY> --repo"));
    }

    #[test]
    fn prompts_labels_and_the_pending_record_follow_the_key() {
        assert_eq!(
            launch_prompt("DATA-1", false),
            "[herdr-linear-agent ticker] Start DATA-1. Follow AGENTS.md."
        );
        assert_eq!(
            launch_prompt("DATA-1", true),
            "[herdr-linear-agent ticker] You were restarted as the coordinator of DATA-1. \
             Run context."
        );
        assert_eq!(
            NUDGE_REPLY,
            "[herdr-linear-agent ticker] There is a new reply in Linear. Run context."
        );
        assert_eq!(
            NUDGE_INBOX,
            "[herdr-linear-agent ticker] There are new inbox items. Run context."
        );

        let f = folder(&format!("Tab\there {}", "z".repeat(80)));
        let label = workspace_label(&f.record);
        assert_eq!(label, format!("acme/DATA-1 Tabhere {}", "z".repeat(52)));

        let pending = pending_record(&f.record, "coordinator-light", "claude");
        assert_eq!(pending.status, AgentStatus::Pending);
        assert_eq!(pending.profile, "coordinator-light");
        assert_eq!(pending.kind, "claude");
        assert_eq!(pending.agent_name, "acme-data-1-coordinator");
    }

    #[test]
    fn the_digest_lists_issue_catalog_worker_profiles_workers_and_inbox_in_order() {
        let f = folder("Fix login");
        std::fs::write(
            f.run.issue_md(),
            "# DATA-1 Fix login\n\nThe login page loops.\n",
        )
        .unwrap();
        std::fs::write(
            f.run.conversation_md(),
            "# Conversation\n\nuser-1: please keep the old URL\n",
        )
        .unwrap();
        let item = inbox::write(
            &f.run,
            "worker",
            "w1",
            "w1 (api) has a new report:\nworkers/w1.md",
        )
        .unwrap();
        let rows = [
            WorkerRow {
                id: "w1".into(),
                title: "Change API".into(),
                repo: "api".into(),
                state: "Reported",
                pr_url: "https://github.com/acme/api/pull/7".into(),
            },
            WorkerRow {
                id: "w2".into(),
                title: "Update the page".into(),
                repo: "web".into(),
                state: "Working",
                pr_url: String::new(),
            },
        ];

        let (text, shown) = digest(&f.run, &sample_config(), "/bin/hla", &rows).unwrap();
        assert_in_order(
            &text,
            &[
                "## Issue",
                "The login page loops.",
                "## Conversation",
                "please keep the old URL",
                "## Repositories",
                "- api: /src/api (base main) \u{2014} The API server\n",
                "- web: /src/web (base develop)\n",
                "## Worker profiles",
                "- standard: claude sonnet, effort high \u{2014} scoped features and fixes",
                "- deep: codex gpt-6-sol, effort xhigh \u{2014} cross-module changes",
                "## Workers",
                "- w1 [Reported] Change API",
                "PR https://github.com/acme/api/pull/7",
                "- w2 [Working] Update the page",
                "## Inbox",
                &item,
                "[worker] w1: w1 (api) has a new report: workers/w1.md",
            ],
        );
        for coordinator_profile in ["- coordinator:", "- coordinator-light:", "- router:"] {
            assert!(!text.contains(coordinator_profile), "{coordinator_profile}");
        }
        assert_eq!(shown, [item]);

        inbox::mark_seen(&f.run, &shown).unwrap();
        inbox::done(&f.run, &[], true).unwrap();
        let (_, shown) = digest(&f.run, &sample_config(), "/bin/hla", &[]).unwrap();
        assert!(shown.is_empty());
    }

    fn pane_row(pane: &str) -> (PaneId, Pane) {
        let workspace = pane.split(':').next().unwrap();
        let entry = Pane {
            id: PaneId(pane.into()),
            workspace: WorkspaceId(workspace.into()),
            tab: format!("{workspace}:t1"),
            terminal: format!("term-{workspace}"),
            cwd: None,
            foreground_cwd: None,
            label: None,
        };
        (entry.id.clone(), entry)
    }

    fn claude_in(pane: &str, name: &str, cwd: &str, status: Seen) -> Agent {
        Agent {
            pane: PaneId(pane.into()),
            kind: Some("claude".into()),
            name: Some(name.into()),
            status,
            session: None,
            cwd: Some(cwd.into()),
            foreground_cwd: None,
            terminal: "term-x".into(),
            interactive_ready: true,
            launch_pending: false,
            state_change_seq: 1,
            state_labels: BTreeMap::new(),
        }
    }

    #[test]
    fn rows_read_the_snapshot_when_there_is_one_and_the_stored_group_otherwise() {
        let f = folder("Fix login");
        let setups: [(AgentStatus, &str, &str); 5] = [
            (AgentStatus::Open, "working", ""),
            (AgentStatus::Failed, "", ""),
            (AgentStatus::Stopped, "reported", "h3"),
            (AgentStatus::Open, "", ""),
            (AgentStatus::Open, "reported", "h5"),
        ];
        for (n, (status, stored, hash)) in setups.into_iter().enumerate() {
            let n = n + 1;
            worker::allocate(
                &f.run,
                |_| Ok(()),
                |w| {
                    w.title = format!("Task {n}");
                    w.repo = format!("repo{n}");
                    w.report_hash = hash.into();
                    w.pr_url = if n == 5 {
                        "https://github.com/acme/repo5/pull/5".into()
                    } else {
                        String::new()
                    };
                    w.agent = AgentRecord {
                        status,
                        kind: "claude".into(),
                        agent_name: format!("data-1-w{n}"),
                        pane_id: format!("p{n}:1"),
                        cwd: format!("/wt/repo{n}"),
                        last_group: stored.into(),
                        ..AgentRecord::default()
                    };
                },
            )
            .unwrap();
        }
        let states = |rows: Vec<WorkerRow>| rows.into_iter().map(|r| r.state).collect::<Vec<_>>();

        let offline = worker_rows(&f.run, None);
        assert_eq!(offline[0].title, "Task 1");
        assert_eq!(offline[0].repo, "repo1");
        assert_eq!(offline[4].pr_url, "https://github.com/acme/repo5/pull/5");
        assert_eq!(
            states(offline),
            [
                "Working",
                "Waiting on you",
                "Stopped",
                "Starting",
                "Reported"
            ]
        );

        // Only w1's pane is alive, with its agent idle.
        let snapshot = Snapshot {
            version: "0.9.1".into(),
            protocol: 22,
            panes: BTreeMap::from([pane_row("p1:1")]),
            agents: vec![claude_in("p1:1", "data-1-w1", "/wt/repo1", Seen::Idle)],
            skipped: 0,
        };
        let state = tempfile::tempdir().unwrap();
        let view = View {
            snapshot: &snapshot,
            state_dir: state.path(),
            socket: "/tmp/herdr-work.sock",
            now: "2026-09-28T12:00:00Z".parse().unwrap(),
        };
        assert_eq!(
            states(worker_rows(&f.run, Some(&view))),
            [
                "Idle",
                "Waiting on you",
                "Stopped",
                "Waiting on you",
                "Reported"
            ]
        );
    }
}
