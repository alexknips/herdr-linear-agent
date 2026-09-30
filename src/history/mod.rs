//! `history`: past runs, newest first, filtered as you type, with a preview
//! of each run's files and its agents' transcripts.

mod tui;

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::run::{AgentRecord, Run, RunRecord, Status};
use crate::worker::{self, Worker};

pub use tui::run;

/// One run of the list.
pub struct Entry {
    pub run: Run,
    pub record: RunRecord,
    pub workers: Vec<Worker>,
    /// The latest time any of the run's records was changed.
    pub updated: Option<Timestamp>,
}

/// An agent of a run: the coordinator, or a worker by id.
pub struct Agent {
    pub label: String,
    pub record: AgentRecord,
}

fn parse(text: &str) -> Option<Timestamp> {
    text.parse().ok()
}

/// `2026-09-30 05:04` in UTC, or `-`.
pub fn minute(at: Option<Timestamp>) -> String {
    at.map_or_else(
        || "-".into(),
        |at| at.strftime("%Y-%m-%d %H:%M").to_string(),
    )
}

fn status(status: Status) -> &'static str {
    match status {
        Status::Active => "active",
        Status::Detached => "detached",
        Status::Closed => "closed",
    }
}

impl Entry {
    fn load(run: Run) -> Option<Entry> {
        let record = run.record().ok()?;
        let workers = worker::list(&run);
        let mut times = vec![
            &record.created,
            &record.last_activity,
            &record.coordinator.last_state_change,
        ];
        for w in &workers {
            times.extend([&w.updated, &w.agent.last_state_change]);
        }
        let updated = times.into_iter().filter_map(|t| parse(t)).max();
        Some(Entry {
            run,
            record,
            workers,
            updated,
        })
    }

    /// The pull requests the run's workers reported.
    pub fn prs(&self) -> Vec<&str> {
        self.workers
            .iter()
            .map(|w| w.pr_url.as_str())
            .filter(|url| !url.is_empty())
            .collect()
    }

    /// The list line, which is also what the query matches.
    pub fn line(&self) -> String {
        let mut line = format!(
            "{}  {}  {}  {}",
            self.run.key,
            self.record.title,
            status(self.record.status),
            minute(self.updated)
        );
        for pr in self.prs() {
            line.push_str("  ");
            line.push_str(pr);
        }
        line
    }

    /// The run's files: the issue, the conversation, each worker's report,
    /// and the pull requests.
    pub fn preview(&self) -> String {
        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap_or_default();
        let mut text = format!(
            "{}  {}\nStatus {}, updated {}\n{}\n",
            self.run.key,
            self.record.title,
            status(self.record.status),
            minute(self.updated),
            self.record.url
        );
        for pr in self.prs() {
            text.push_str(&format!("PR {pr}\n"));
        }
        text.push('\n');
        text.push_str(read(self.run.issue_md()).trim_end());
        let conversation = read(self.run.conversation_md());
        if !conversation.trim().is_empty() {
            text.push_str("\n\n# Conversation\n\n");
            text.push_str(conversation.trim());
        }
        for w in &self.workers {
            let report = read(worker::home_report_path(&self.run, &w.id));
            let report = match report.trim() {
                "" => "(no report)",
                report => report,
            };
            text.push_str(&format!("\n\n# Worker {}: {}\n\n{report}", w.id, w.title));
        }
        text.push('\n');
        text
    }

    /// The coordinator, then each worker.
    pub fn agents(&self) -> Vec<Agent> {
        let mut coordinator = self.record.coordinator.clone();
        if coordinator.cwd.is_empty() {
            coordinator.cwd = self.run.canonical_dir().to_string_lossy().into_owned();
        }
        let mut agents = vec![Agent {
            label: "coordinator".into(),
            record: coordinator,
        }];
        for w in &self.workers {
            let mut record = w.agent.clone();
            if record.cwd.is_empty() {
                record.cwd.clone_from(&w.worktree_path);
            }
            agents.push(Agent {
                label: w.id.clone(),
                record,
            });
        }
        agents
    }
}

/// Every run of every workspace, the most recently updated first.
pub fn entries(runs_dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Run::list(runs_dir)
        .into_iter()
        .filter_map(Entry::load)
        .collect();
    entries.sort_by(|a, b| b.updated.cmp(&a.updated).then(a.run.key.cmp(&b.run.key)));
    entries
}

/// The indexes of the entries whose line matches `query`, best match first;
/// all of them in their order when the query is blank.
pub fn filter(entries: &[Entry], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..entries.len()).collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let line = e.line();
            pattern
                .score(Utf32Str::new(&line, &mut buf), &mut matcher)
                .map(|score| (i, score))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::RunRecord;

    fn run(runs: &Path, workspace: &str, key: &str, title: &str, at: &str) -> Run {
        let record = RunRecord {
            workspace: workspace.into(),
            identifier: key.into(),
            title: title.into(),
            url: format!("https://linear.app/{workspace}/issue/{key}"),
            created: at.into(),
            last_activity: at.into(),
            ..RunRecord::default()
        };
        Run::create(runs, record).unwrap()
    }

    fn keys(entries: &[Entry], picked: &[usize]) -> Vec<String> {
        picked.iter().map(|&i| entries[i].run.key.clone()).collect()
    }

    #[test]
    fn runs_are_listed_newest_first_and_filtered_as_typed() {
        let dir = tempfile::tempdir().unwrap();
        run(
            dir.path(),
            "acme",
            "DATA-1",
            "Fix the login",
            "2026-09-01T00:00:00Z",
        );
        run(
            dir.path(),
            "acme",
            "DATA-2",
            "Export to CSV",
            "2026-09-03T00:00:00Z",
        );
        run(
            dir.path(),
            "beta",
            "OPS-7",
            "Rotate the login keys",
            "2026-09-02T00:00:00Z",
        );
        let entries = entries(dir.path());
        let all = filter(&entries, "");
        assert_eq!(
            keys(&entries, &all),
            ["acme/DATA-2", "beta/OPS-7", "acme/DATA-1"]
        );
        assert_eq!(
            entries[0].line(),
            "acme/DATA-2  Export to CSV  active  2026-09-03 00:00"
        );
        let login = filter(&entries, "login");
        assert_eq!(keys(&entries, &login), ["beta/OPS-7", "acme/DATA-1"]);
        assert_eq!(
            keys(&entries, &filter(&entries, "beta ops")),
            ["beta/OPS-7"]
        );
        assert!(filter(&entries, "zzz").is_empty());
    }

    #[test]
    fn the_preview_shows_the_issue_the_conversation_the_reports_and_the_prs() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(
            dir.path(),
            "acme",
            "DATA-1",
            "Fix the login",
            "2026-09-01T00:00:00Z",
        );
        std::fs::write(
            run.issue_md(),
            "# DATA-1 Fix the login\n\nThe form fails.\n",
        )
        .unwrap();
        std::fs::write(run.conversation_md(), "## user-1\n\nAlso add a test.\n").unwrap();
        let worker = Worker {
            id: "w1".into(),
            title: "Fix the form".into(),
            pr_url: "https://github.com/acme/api/pull/7".into(),
            updated: "2026-09-02T00:00:00Z".into(),
            ..Worker::default()
        };
        std::fs::write(
            run.workers_dir().join("w1.toml"),
            toml::to_string(&worker).unwrap(),
        )
        .unwrap();
        std::fs::write(
            worker::home_report_path(&run, "w1"),
            "PR: https://github.com/acme/api/pull/7\n\n## Report\n\nDone.\n",
        )
        .unwrap();
        let entry = &entries(dir.path())[0];
        assert_eq!(
            entry.preview(),
            "acme/DATA-1  Fix the login\nStatus active, updated 2026-09-02 00:00\nhttps://linear.app/acme/issue/DATA-1\nPR https://github.com/acme/api/pull/7\n\n# DATA-1 Fix the login\n\nThe form fails.\n\n# Conversation\n\n## user-1\n\nAlso add a test.\n\n# Worker w1: Fix the form\n\nPR: https://github.com/acme/api/pull/7\n\n## Report\n\nDone.\n"
        );
        assert!(
            entry
                .line()
                .ends_with("  https://github.com/acme/api/pull/7")
        );
        let labels: Vec<String> = entry.agents().into_iter().map(|a| a.label).collect();
        assert_eq!(labels, ["coordinator", "w1"]);
    }
}
