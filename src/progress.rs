//! `report`: a worker tells how far it got, from its own pane. The record is
//! kept under the state folder for the ticker's groups, and the activity is
//! shown in Herdr as a pane token.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::herdr::{self, Herdr, PaneId};
use crate::paths::Env;

/// The activity a worker reports when it waits on the coordinator.
pub const WAITING: &str = "Waiting for you";
/// The metadata source of every pane token the plugin reports.
pub const SOURCE: &str = "herdr-linear-agent";
pub const TOKEN_TTL_MS: u64 = 300_000;
/// The token with an agent's state, on coordinator and worker panes.
pub const STATE_TOKEN: &str = "herdr_linear_agent_state";
/// The token with what a worker reports it is doing.
pub const ACTIVITY_TOKEN: &str = "herdr_linear_agent_activity";
const ACTIVITY_COLUMNS: usize = 40;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub socket: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub activity: String,
    /// `None` for `--unknown`.
    pub percent: Option<u8>,
    /// Seconds since the Unix epoch.
    pub reported_at: i64,
}

impl Record {
    pub fn waiting(&self) -> bool {
        self.activity == WAITING
    }
}

fn is_wide(c: char) -> bool {
    matches!(u32::from(c),
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD)
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Trimmed, without control or bidirectional-override characters, and cut
/// to `columns` display columns (a wide character counts two).
pub fn clean(text: &str, columns: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text
        .trim()
        .chars()
        .filter(|&c| !c.is_control() && !is_bidi_control(c))
    {
        let width = if is_wide(c) { 2 } else { 1 };
        if used + width > columns {
            break;
        }
        used += width;
        out.push(c);
    }
    out.trim_end().to_string()
}

fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("progress")
}

fn path(state_dir: &Path, socket: &str, pane: &str) -> PathBuf {
    let pane: String = pane
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let hash = files::sha256_hex(socket.as_bytes());
    dir(state_dir).join(format!("{pane}-{}.json", &hash[..16]))
}

pub fn save(state_dir: &Path, record: &Record) -> Result<()> {
    std::fs::create_dir_all(dir(state_dir))?;
    files::write_json(&path(state_dir, &record.socket, &record.pane_id), record)
}

pub fn load(state_dir: &Path, socket: &str, pane: &str) -> Option<Record> {
    files::read_json::<Record>(&path(state_dir, socket, pane))
        .filter(|r| r.socket == socket && r.pane_id == pane)
}

/// The pane's record, unless it was written from another terminal than the
/// pane's current one (a new process in a reused pane id).
pub fn self_report(state_dir: &Path, socket: &str, pane: &str, terminal: &str) -> Option<Record> {
    load(state_dir, socket, pane).filter(|r| terminal.is_empty() || r.terminal_id == terminal)
}

/// Removes this socket's records of panes that no longer exist.
pub fn prune(state_dir: &Path, socket: &str, live: &[String]) {
    let Ok(entries) = std::fs::read_dir(dir(state_dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(record) = files::read_json::<Record>(&entry.path()) else {
            continue;
        };
        if record.socket == socket && !live.contains(&record.pane_id) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `report`: outside a Herdr pane it does nothing.
pub async fn report(env: &Env, percent: Option<u8>, activity: &str) -> Result<()> {
    if percent.is_some_and(|p| p > 100) {
        bail!("--percent is at most 100");
    }
    let var = |key| env.var(key).filter(|v| !v.is_empty());
    let (Some("1"), Some(pane), Some(socket)) = (
        var("HERDR_ENV"),
        var("HERDR_PANE_ID"),
        var("HERDR_SOCKET_PATH"),
    ) else {
        return Ok(());
    };
    let client = herdr::Client::new(socket);
    report_in_pane(
        &env.state_dir(),
        &client,
        socket,
        &PaneId(pane.into()),
        percent,
        activity,
        jiff::Timestamp::now().as_second(),
    )
    .await
}

pub async fn report_in_pane<H: Herdr>(
    state_dir: &Path,
    herdr: &H,
    socket: &str,
    caller: &PaneId,
    percent: Option<u8>,
    activity: &str,
    now: i64,
) -> Result<()> {
    let activity = clean(activity, ACTIVITY_COLUMNS);
    let pane = herdr
        .pane_current(caller)
        .await
        .context("Herdr does not know this pane")?;
    save(
        state_dir,
        &Record {
            socket: socket.into(),
            pane_id: pane.id.0.clone(),
            terminal_id: pane.terminal,
            activity: activity.clone(),
            percent,
            reported_at: now,
        },
    )?;
    crate::ticker::poke(state_dir);
    // The record is what the ticker reads; a token that did not reach Herdr
    // only leaves the pane's label stale.
    let _ = herdr
        .report_metadata(
            &pane.id,
            SOURCE,
            "",
            &[(ACTIVITY_TOKEN.into(), activity)],
            TOKEN_TTL_MS,
        )
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::FakeHerdr;

    const WORK_SOCKET: &str = "/tmp/herdr-work.sock";

    #[test]
    fn token_names_fit_herdrs_limits() {
        // Herdr takes names matching `^[A-Za-z0-9_-]{1,32}$`.
        for name in [STATE_TOKEN, ACTIVITY_TOKEN] {
            assert!(
                (1..=32).contains(&name.len())
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{name}"
            );
        }
    }
    const SIDE_SOCKET: &str = "/tmp/herdr-side.sock";

    fn progress_of(socket: &str, pane: &str, terminal: &str) -> Record {
        Record {
            socket: socket.into(),
            pane_id: pane.into(),
            terminal_id: terminal.into(),
            activity: "Writing the handler".into(),
            percent: Some(55),
            reported_at: 1_790_000_000,
        }
    }

    #[test]
    fn an_activity_is_trimmed_stripped_and_cut_to_its_columns() {
        let table = [
            ("  Running the suite \n", 40, "Running the suite"),
            ("Fix\u{7}ing\u{1b} the build", 40, "Fixing the build"),
            ("left\u{202E}right\u{2066}\u{200F}", 40, "leftright"),
            ("日本語テキスト", 6, "日本語"),
            ("日本語テキスト", 7, "日本語"),
            ("abc def", 4, "abc"),
        ];
        for (raw, columns, expected) in table {
            assert_eq!(clean(raw, columns), expected, "{raw:?} at {columns}");
        }
        assert_eq!(clean(&"q".repeat(55), ACTIVITY_COLUMNS), "q".repeat(40));
        assert_eq!(clean(&"字".repeat(33), ACTIVITY_COLUMNS), "字".repeat(20));
    }

    #[test]
    fn records_are_kept_per_socket_and_pane() {
        let state = tempfile::tempdir().unwrap();
        let mine = progress_of(WORK_SOCKET, "w1:p1", "term-1");
        save(state.path(), &mine).unwrap();
        assert_eq!(load(state.path(), WORK_SOCKET, "w1:p1"), Some(mine));
        assert_eq!(load(state.path(), SIDE_SOCKET, "w1:p1"), None);
        assert_eq!(load(state.path(), WORK_SOCKET, "w1:p2"), None);
        assert!(state.path().join("progress").is_dir());
    }

    #[test]
    fn a_record_from_an_earlier_terminal_of_the_pane_is_not_its_self_report() {
        let state = tempfile::tempdir().unwrap();
        let mine = progress_of(WORK_SOCKET, "w2:p1", "term-7");
        save(state.path(), &mine).unwrap();
        let table = [("term-7", true), ("", true), ("term-8", false)];
        for (terminal, counts) in table {
            let found = self_report(state.path(), WORK_SOCKET, "w2:p1", terminal);
            assert_eq!(found.is_some(), counts, "terminal {terminal:?}");
        }
    }

    #[test]
    fn pruning_touches_only_gone_panes_of_the_same_socket() {
        let state = tempfile::tempdir().unwrap();
        for (socket, pane) in [
            (WORK_SOCKET, "w1:p1"),
            (WORK_SOCKET, "w2:p1"),
            (SIDE_SOCKET, "w2:p1"),
        ] {
            save(state.path(), &progress_of(socket, pane, "term-1")).unwrap();
        }
        prune(state.path(), WORK_SOCKET, &["w1:p1".to_string()]);
        assert!(load(state.path(), WORK_SOCKET, "w1:p1").is_some());
        assert!(load(state.path(), WORK_SOCKET, "w2:p1").is_none());
        assert!(load(state.path(), SIDE_SOCKET, "w2:p1").is_some());
    }

    #[test]
    fn only_the_exact_waiting_activity_waits() {
        assert_eq!(WAITING, "Waiting for you");
        let mut record = progress_of(WORK_SOCKET, "w1:p1", "term-1");
        assert!(!record.waiting());
        record.activity = "Waiting for you".into();
        assert!(record.waiting());
        record.activity = "Waiting for you to merge".into();
        assert!(!record.waiting());
    }

    #[tokio::test]
    async fn a_report_in_a_pane_saves_its_record_and_sets_the_activity_token() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state");
        let herdr = FakeHerdr::new(home.path());
        let pane = herdr.add_pane("/wt/api");

        report_in_pane(
            &state,
            &herdr,
            WORK_SOCKET,
            &pane,
            Some(40),
            " Running tests\u{7} ",
            1_790_000_123,
        )
        .await
        .unwrap();

        assert_eq!(
            load(&state, WORK_SOCKET, &pane.0),
            Some(Record {
                socket: WORK_SOCKET.into(),
                pane_id: "w1:p1".into(),
                terminal_id: "term-1".into(),
                activity: "Running tests".into(),
                percent: Some(40),
                reported_at: 1_790_000_123,
            })
        );
        assert_eq!(herdr.requests(), ["pane.current", "pane.report_metadata"]);
        let [token] = herdr.metadata().try_into().unwrap();
        assert_eq!(token.pane, pane);
        assert_eq!(token.source, "herdr-linear-agent");
        assert_eq!(
            token.tokens,
            [(
                "herdr_linear_agent_activity".to_string(),
                "Running tests".to_string()
            )]
        );
        assert_eq!(token.ttl_ms, 300_000);

        report_in_pane(
            &state,
            &herdr,
            WORK_SOCKET,
            &pane,
            None,
            "Scoping",
            1_790_000_200,
        )
        .await
        .unwrap();
        let unknown = load(&state, WORK_SOCKET, &pane.0).unwrap();
        assert_eq!(
            (unknown.percent, unknown.activity.as_str()),
            (None, "Scoping")
        );
    }

    #[tokio::test]
    async fn outside_a_pane_nothing_is_written_and_a_percent_over_a_hundred_fails() {
        let home = tempfile::tempdir().unwrap();
        let partial: [&[(&str, &str)]; 3] = [
            &[],
            &[
                ("HERDR_ENV", "0"),
                ("HERDR_PANE_ID", "w1:p1"),
                ("HERDR_SOCKET_PATH", WORK_SOCKET),
            ],
            &[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p1")],
        ];
        for vars in partial {
            let env = Env::for_test(home.path(), vars);
            report(&env, Some(10), "Reading").await.unwrap();
            assert!(!env.state_dir().exists(), "{vars:?}");
        }
        let env = Env::for_test(home.path(), &[]);
        let refused = report(&env, Some(101), "Reading").await.unwrap_err();
        assert!(refused.to_string().contains("at most 100"), "{refused}");
        report(&env, Some(100), "Done").await.unwrap();
    }
}
