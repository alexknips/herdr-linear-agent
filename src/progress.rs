//! Progress self-reporting: an agent runs `report` in its own pane, and one
//! JSON record per pane under `<state>/progress/` feeds the ticker. The
//! binding is the pane id Herdr hands every pane shell; there is nothing to
//! mint and no daemon. Outside a Herdr pane the command does nothing.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::herdr_cli::{CALL_TIMEOUT, Herdr};
use crate::paths::{Ctx, Env};
use crate::runner::Runner;

pub const ACTIVITY_COLUMNS: usize = 40;
pub const ACTIVITY_TTL_MS: u64 = 300_000;
pub const WAITING: &str = "Waiting for you";
pub const DONE: &str = "Done";
/// The source Herdr shows for this plugin's pane metadata.
pub const SOURCE: &str = "herdr-linear-agent";

/// One pane's self-report.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Record {
    pub socket: String,
    pub pane_id: String,
    /// Pane ids restart from `w1` after a server restart; the terminal id tells
    /// a stale record from a live pane that reused the id.
    pub terminal_id: String,
    pub activity: String,
    pub percent: Option<u8>,
    /// Unix seconds of the last `report`.
    pub reported_at: i64,
}

impl Record {
    pub fn waiting(&self) -> bool {
        self.reported_at > 0 && self.activity == WAITING
    }

    pub fn done(&self) -> bool {
        self.reported_at > 0 && self.percent == Some(100)
    }
}

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("progress")
}

/// `<pane id>-<short hash of the socket path>.json`: pane ids repeat across sessions.
pub fn path(state_dir: &Path, socket: &str, pane_id: &str) -> PathBuf {
    let hash = &files::sha256_hex(socket.as_bytes())[..8];
    dir(state_dir).join(format!("{}-{hash}.json", pane_id.replace(':', "_")))
}

pub fn load(state_dir: &Path, socket: &str, pane_id: &str) -> Option<Record> {
    files::read_json(&path(state_dir, socket, pane_id))
}

pub fn save(state_dir: &Path, record: &Record) -> Result<()> {
    std::fs::create_dir_all(dir(state_dir))?;
    files::write_json(&path(state_dir, &record.socket, &record.pane_id), record)
}

/// At most `columns` columns, no control or bidi characters, trimmed.
pub fn clean(input: &str, columns: usize) -> String {
    let mut width = 0;
    input
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(120)
        .take_while(|c| {
            // East Asian wide characters take two columns; everything else one.
            let wide = ('\u{1100}'..='\u{115f}').contains(c)
                || ('\u{2e80}'..='\u{a4cf}').contains(c)
                || ('\u{ac00}'..='\u{d7a3}').contains(c)
                || ('\u{f900}'..='\u{faff}').contains(c)
                || ('\u{fe30}'..='\u{fe4f}').contains(c)
                || ('\u{ff00}'..='\u{ff60}').contains(c)
                || ('\u{ffe0}'..='\u{ffe6}').contains(c)
                || ('\u{1f300}'..='\u{1f64f}').contains(c)
                || ('\u{1f900}'..='\u{1f9ff}').contains(c)
                || ('\u{20000}'..='\u{3fffd}').contains(c);
            width += if wide { 2 } else { 1 };
            width <= columns
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The calling pane, when this process runs inside a Herdr pane: the id
/// Herdr shows for it and its terminal id. `None` outside Herdr.
pub struct Current {
    pub socket: String,
    pub pane_id: String,
    pub terminal_id: String,
}

pub fn current(env: &Env, runner: &dyn Runner) -> Option<Current> {
    if env.var("HERDR_ENV") != Some("1") {
        return None;
    }
    env.var("HERDR_PANE_ID")?;
    let socket = env.var("HERDR_SOCKET_PATH")?.to_string();
    let herdr = Herdr::new(env.herdr_bin(), &socket, runner);
    let result = herdr
        .call(&["pane", "current", "--current"], CALL_TIMEOUT)
        .ok()?;
    let pane = &result["pane"];
    Some(Current {
        socket,
        pane_id: pane["pane_id"].as_str()?.to_string(),
        terminal_id: pane["terminal_id"].as_str().unwrap_or("").to_string(),
    })
}

/// `report --percent N|--unknown --activity "..."`, run by an agent in its
/// pane. Writes the record and one `hla_activity` pane token with a TTL, so
/// silence clears the activity after five minutes with no daemon.
pub fn report(ctx: &Ctx, percent: Option<u8>, activity: &str) -> Result<()> {
    if percent.is_some_and(|p| p > 100) {
        bail!("--percent must be 0 to 100");
    }
    let Some(pane) = current(ctx.env, ctx.runner) else {
        println!("not inside a Herdr pane; nothing reported");
        return Ok(());
    };
    let activity = if percent == Some(100) {
        DONE.to_string()
    } else {
        clean(activity, ACTIVITY_COLUMNS)
    };
    if activity.is_empty() {
        bail!("--activity is empty");
    }
    let state_dir = ctx.ensure_state_dir()?;
    let record = Record {
        socket: pane.socket.clone(),
        pane_id: pane.pane_id.clone(),
        terminal_id: pane.terminal_id,
        activity: activity.clone(),
        percent,
        reported_at: jiff::Timestamp::now().as_second(),
    };
    save(&state_dir, &record)?;
    let herdr = Herdr::new(ctx.env.herdr_bin(), &pane.socket, ctx.runner);
    let token = format!("hla_activity={activity}");
    let ttl = ACTIVITY_TTL_MS.to_string();
    match herdr.call(
        &[
            "pane",
            "report-metadata",
            &pane.pane_id,
            "--source",
            SOURCE,
            "--token",
            &token,
            "--ttl-ms",
            &ttl,
        ],
        CALL_TIMEOUT,
    ) {
        Ok(_) => println!(
            "recorded: {}{activity}",
            percent.map(|p| format!("{p}% · ")).unwrap_or_default()
        ),
        Err(error) => println!("recorded; the pane token was not set ({error})"),
    }
    Ok(())
}

/// The record for a pane, or nothing when it is missing, was never reported,
/// or describes an earlier pane with the same id.
pub fn self_report(
    state_dir: &Path,
    socket: &str,
    pane_id: &str,
    terminal_id: &str,
) -> Option<Record> {
    let record = load(state_dir, socket, pane_id)?;
    if !terminal_id.is_empty()
        && !record.terminal_id.is_empty()
        && record.terminal_id != terminal_id
    {
        return None;
    }
    (record.reported_at > 0).then_some(record)
}

/// Drops records whose pane is no longer listed in the session they belong to.
/// Only records of `socket` are judged: other sessions' records are theirs.
pub fn prune(state_dir: &Path, socket: &str, live_pane_ids: &[String]) {
    let Ok(entries) = std::fs::read_dir(dir(state_dir)) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(record) = files::read_json::<Record>(&entry.path())
            && record.socket == socket
            && !live_pane_ids.contains(&record.pane_id)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{FakeRunner, ok};

    #[test]
    fn activity_is_cleaned_and_capped_at_forty_columns() {
        assert_eq!(clean("  Reading code\n", 40), "Reading code");
        assert_eq!(clean(&"x".repeat(60), 40).len(), 40);
        assert_eq!(clean("a\u{202e}b\u{7}c", 40), "abc");
        assert_eq!(clean("日本語テキスト", 6), "日本語");
    }

    #[test]
    fn records_round_trip_per_session_and_stale_terminals_are_ignored() {
        let state = tempfile::tempdir().unwrap();
        let record = Record {
            socket: "/a.sock".into(),
            pane_id: "w1:p1".into(),
            terminal_id: "term_1".into(),
            activity: WAITING.into(),
            reported_at: 7,
            ..Record::default()
        };
        save(state.path(), &record).unwrap();
        let other = Record {
            socket: "/b.sock".into(),
            pane_id: "w1:p1".into(),
            terminal_id: "term_9".into(),
            activity: "Testing".into(),
            reported_at: 8,
            ..Record::default()
        };
        save(state.path(), &other).unwrap();
        assert_eq!(load(state.path(), "/a.sock", "w1:p1").unwrap(), record);
        assert!(
            self_report(state.path(), "/a.sock", "w1:p1", "term_1")
                .unwrap()
                .waiting()
        );
        assert!(self_report(state.path(), "/a.sock", "w1:p1", "term_2").is_none());
        assert!(self_report(state.path(), "/a.sock", "w1:p1", "").is_some());
        prune(state.path(), "/a.sock", &["w1:p2".into()]);
        assert!(load(state.path(), "/a.sock", "w1:p1").is_none());
        assert!(load(state.path(), "/b.sock", "w1:p1").is_some());
    }

    #[test]
    fn report_writes_the_record_and_a_token_inside_a_pane() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state");
        let env = Env::for_test(
            home.path(),
            &[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w1:p1"),
                ("HERDR_SOCKET_PATH", "/s.sock"),
                ("XDG_STATE_HOME", state.to_str().unwrap()),
            ],
        );
        let runner = FakeRunner::new();
        runner.on(
            "pane current",
            ok(r#"{"result":{"pane":{"pane_id":"w1:p1","terminal_id":"t1"}}}"#),
        );
        runner.on("pane report-metadata", ok(""));
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: false,
        };
        report(&ctx, None, WAITING).unwrap();
        let record = load(&env.state_dir(), "/s.sock", "w1:p1").unwrap();
        assert!(record.waiting());
        assert_eq!(runner.count("--token hla_activity=Waiting for you"), 1);
        assert!(report(&ctx, Some(101), "x").is_err());

        // Outside a pane nothing is written.
        let outside = Env::for_test(home.path(), &[]);
        let ctx = Ctx {
            env: &outside,
            runner: &runner,
            detached_ticker: false,
        };
        report(&ctx, Some(10), "Reading").unwrap();
    }
}
