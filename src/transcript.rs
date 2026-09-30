//! Agents' own transcripts: which session an agent started, a copy of its
//! transcript kept in the run folder, and a read-only rendering of one. The
//! agent CLIs write their transcripts; the plugin reads them and writes only
//! its copies.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use jiff::Timestamp;
use serde_json::Value;

use crate::paths::Env;
use crate::run::{AgentRecord, Run};
use crate::worker;

/// How long an `opencode` call may take.
const OPENCODE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where each kind keeps its transcripts.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `<dir>/<folder with every non-alphanumeric byte as ->/<session>.jsonl`.
    claude: PathBuf,
    /// `<dir>/YYYY/MM/DD/rollout-*.jsonl`, the folder in the first line.
    codex: PathBuf,
    /// `<dir>/<folder's alphanumeric runs joined by ->/agent-transcripts/<id>/<id>.jsonl`.
    cursor: PathBuf,
    /// The `PATH` `opencode` is looked up in.
    path: Option<String>,
}

impl Roots {
    pub fn from_env(env: &Env) -> Roots {
        let dir = |var: &str, default: &str| {
            env.var(var)
                .filter(|v| Path::new(v).is_absolute())
                .map_or_else(|| env.home.join(default), PathBuf::from)
        };
        Roots {
            claude: dir("CLAUDE_CONFIG_DIR", ".claude").join("projects"),
            codex: dir("CODEX_HOME", ".codex").join("sessions"),
            cursor: env.home.join(".cursor/projects"),
            path: env.var("PATH").map(str::to_string),
        }
    }
}

/// Where a session's transcript is read from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    /// `opencode session export <id>`, run in the folder.
    Export {
        cwd: String,
    },
}

/// One session of an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The id `resume_args` takes.
    pub id: String,
    pub source: Source,
    /// When the session started, or its copy was last written.
    pub at: Option<Timestamp>,
    /// Read from the copy in the run folder.
    pub kept: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sessions {
    /// Oldest first; empty when none is left in `searched`.
    Found {
        sessions: Vec<Session>,
        searched: String,
    },
    /// The kind keeps no transcript the plugin knows.
    Unreadable(&'static str),
}

fn time(at: SystemTime) -> Option<Timestamp> {
    let since = at.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Timestamp::from_second(i64::try_from(since.as_secs()).ok()?).ok()
}

fn modified(path: &Path) -> Option<Timestamp> {
    time(std::fs::metadata(path).ok()?.modified().ok()?)
}

/// When a file or folder was made; its last change where the system keeps
/// no creation time.
fn created(path: &Path) -> Option<Timestamp> {
    let meta = std::fs::metadata(path).ok()?;
    time(meta.created().or_else(|_| meta.modified()).ok()?)
}

fn jsonl_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl") && p.is_file())
        .collect()
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The first line of a file, parsed.
fn first_line(path: &Path) -> Option<Value> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn claude_dir(roots: &Roots, cwd: &str) -> PathBuf {
    let folder: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    roots.claude.join(folder)
}

fn cursor_dir(roots: &Roots, cwd: &str) -> PathBuf {
    let folder = cwd
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    roots.cursor.join(folder).join("agent-transcripts")
}

/// `opencode <args>` in `cwd`: its standard output when it succeeds in time.
fn opencode(roots: &Roots, cwd: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new("opencode");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(path) = &roots.path {
        command.env("PATH", path);
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    // Read while waiting, so a long export cannot fill the pipe and stall.
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        stdout.read_to_string(&mut out).map(|_| out)
    });
    let deadline = Instant::now() + OPENCODE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?.ok()?;
    status.success().then_some(out)
}

/// Every session of `kind` that ran in `cwd`, with when it started.
fn started(roots: &Roots, kind: &str, cwd: &str) -> Vec<(String, Source, Option<Timestamp>)> {
    match kind {
        "claude" => jsonl_in(&claude_dir(roots, cwd))
            .into_iter()
            .map(|p| (stem(&p), Source::File(p.clone()), created(&p)))
            .collect(),
        "codex" => {
            let mut found = Vec::new();
            let mut folders = vec![roots.codex.clone()];
            while let Some(folder) = folders.pop() {
                let Ok(entries) = std::fs::read_dir(&folder) else {
                    continue;
                };
                for path in entries.flatten().map(|e| e.path()) {
                    if path.is_dir() {
                        folders.push(path);
                    } else if path.extension().is_some_and(|e| e == "jsonl")
                        && let Some(meta) = first_line(&path)
                        && meta["type"] == "session_meta"
                        && meta["payload"]["cwd"] == cwd
                    {
                        let payload = &meta["payload"];
                        let at = payload["timestamp"]
                            .as_str()
                            .and_then(|t| t.parse().ok())
                            .or_else(|| created(&path));
                        let id = payload["id"].as_str().unwrap_or("").to_string();
                        found.push((id, Source::File(path), at));
                    }
                }
            }
            found
        }
        "cursor" => std::fs::read_dir(cursor_dir(roots, cwd))
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|entry| {
                let at = created(&entry.path());
                jsonl_in(&entry.path())
                    .into_iter()
                    .map(move |p| (stem(&p), Source::File(p), at))
            })
            .collect(),
        "opencode" => {
            let listed = opencode(roots, cwd, &["session", "list", "--format", "json"]);
            let listed: Vec<Value> = listed
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
            listed
                .iter()
                .filter(|s| s["directory"] == cwd)
                .filter_map(|s| {
                    let id = s["id"].as_str()?.to_string();
                    let at = s["created"]
                        .as_i64()
                        .and_then(|ms| Timestamp::from_millisecond(ms).ok());
                    Some((id, Source::Export { cwd: cwd.into() }, at))
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The session the agent launched at `since` began: the first of `kind` to
/// start in `cwd` at or after it. Claude's is chosen at the launch.
pub fn session_since(roots: &Roots, kind: &str, cwd: &str, since: Timestamp) -> Option<String> {
    started(roots, kind, cwd)
        .into_iter()
        .filter(|(_, _, at)| at.is_some_and(|at| at >= since))
        .min_by_key(|(_, _, at)| *at)
        .map(|(id, _, _)| id)
}

/// The sessions an agent of `kind` left in `cwd`, oldest first.
pub fn sessions(roots: &Roots, kind: &str, cwd: &str) -> Sessions {
    let searched = match kind {
        "claude" => claude_dir(roots, cwd).display().to_string(),
        "codex" => roots.codex.display().to_string(),
        "cursor" => cursor_dir(roots, cwd).display().to_string(),
        "opencode" => format!("`opencode session list` in {cwd}"),
        _ => return Sessions::Unreadable("this agent kind keeps no transcript the plugin knows"),
    };
    let mut sessions: Vec<Session> = started(roots, kind, cwd)
        .into_iter()
        .map(|(id, source, at)| Session {
            id,
            source,
            at,
            kept: false,
        })
        .collect();
    sessions.sort_by_key(|s| s.at);
    Sessions::Found { sessions, searched }
}

/// Session `id` of an agent in `cwd`, where the agent keeps it.
pub fn original(roots: &Roots, kind: &str, cwd: &str, id: &str) -> Option<Session> {
    let Sessions::Found { sessions, .. } = sessions(roots, kind, cwd) else {
        return None;
    };
    sessions.into_iter().find(|s| s.id == id)
}

/// The copies kept under `dir`, oldest first.
pub fn kept(dir: &Path) -> Vec<Session> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<Session> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "jsonl" || e == "json"))
        .map(|p| Session {
            id: stem(&p),
            at: modified(&p),
            source: Source::File(p),
            kept: true,
        })
        .collect();
    sessions.sort_by_key(|s| s.at);
    sessions
}

fn read(roots: &Roots, session: &Session) -> std::io::Result<Vec<u8>> {
    match &session.source {
        Source::File(path) => std::fs::read(path),
        Source::Export { cwd } => opencode(roots, cwd, &["session", "export", &session.id])
            .map(String::into_bytes)
            .ok_or_else(|| std::io::Error::other("`opencode session export` failed")),
    }
}

/// Copies session `id`'s transcript to `<dir>/<id>.jsonl` (OpenCode's export
/// to `<id>.json`) when there is no copy yet or the original grew. A copy
/// whose original is gone stays. Returns whether it wrote.
pub fn keep(roots: &Roots, dir: &Path, kind: &str, cwd: &str, id: &str) -> std::io::Result<bool> {
    let Some(session) = original(roots, kind, cwd, id) else {
        return Ok(false);
    };
    let bytes = read(roots, &session)?;
    let ext = if kind == "opencode" { "json" } else { "jsonl" };
    let copy = dir.join(format!("{id}.{ext}"));
    if std::fs::metadata(&copy).is_ok_and(|m| m.len() >= bytes.len() as u64) {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    let partial = dir.join(format!(".{id}.{ext}.partial"));
    std::fs::write(&partial, &bytes)?;
    std::fs::rename(&partial, &copy)?;
    Ok(true)
}

/// One step of a transcript.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Said { who: &'static str, text: String },
    Call { tool: String, input: String },
    Result { text: String, error: bool },
}

const LINE: usize = 200;
const RESULT_LINES: usize = 5;

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// A tool input on one line: its JSON, or its text.
fn input(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), LINE)
}

/// The text of a content that is a string or a list of text parts.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn claude_items(record: &Value, items: &mut Vec<Item>) {
    let content = &record["message"]["content"];
    match (record["type"].as_str(), content) {
        (Some("user"), _) if record["isMeta"] == true => {}
        (Some("user"), Value::String(text)) => items.push(Item::Said {
            who: "user",
            text: text.clone(),
        }),
        (Some(kind @ ("user" | "assistant")), Value::Array(parts)) => {
            let who = if kind == "user" { "user" } else { "assistant" };
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => items.push(Item::Said {
                        who,
                        text: part["text"].as_str().unwrap_or("").to_string(),
                    }),
                    Some("tool_use") => items.push(Item::Call {
                        tool: part["name"].as_str().unwrap_or("?").to_string(),
                        input: input(&part["input"]),
                    }),
                    Some("tool_result") => items.push(Item::Result {
                        text: text_of(&part["content"]),
                        error: part["is_error"] == true,
                    }),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn codex_items(record: &Value, items: &mut Vec<Item>) {
    if record["type"] != "response_item" {
        return;
    }
    let item = &record["payload"];
    match item["type"].as_str() {
        Some("message") => {
            let who = match item["role"].as_str() {
                Some("user") => "user",
                Some("assistant") => "assistant",
                _ => return,
            };
            items.push(Item::Said {
                who,
                text: text_of(&item["content"]),
            });
        }
        Some("function_call") => items.push(Item::Call {
            tool: item["name"].as_str().unwrap_or("?").to_string(),
            input: input(&item["arguments"]),
        }),
        Some("custom_tool_call") => items.push(Item::Call {
            tool: item["name"].as_str().unwrap_or("?").to_string(),
            input: input(&item["input"]),
        }),
        Some("function_call_output" | "custom_tool_call_output") => items.push(Item::Result {
            text: text_of(&item["output"]),
            error: false,
        }),
        _ => {}
    }
}

fn cursor_items(record: &Value, items: &mut Vec<Item>) {
    let who = match record["role"].as_str() {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return,
    };
    for part in record["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
    {
        match part["type"].as_str() {
            Some("text") => items.push(Item::Said {
                who,
                text: part["text"].as_str().unwrap_or("").to_string(),
            }),
            Some("tool_use") => items.push(Item::Call {
                tool: part["name"].as_str().unwrap_or("?").to_string(),
                input: input(&part["input"]),
            }),
            _ => {}
        }
    }
}

fn opencode_items(export: &Value, items: &mut Vec<Item>) {
    for message in export["messages"].as_array().into_iter().flatten() {
        match message["type"].as_str() {
            Some("user") => items.push(Item::Said {
                who: "user",
                text: message["text"].as_str().unwrap_or("").to_string(),
            }),
            Some("assistant") => {
                for part in message["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str() {
                        Some("text") => items.push(Item::Said {
                            who: "assistant",
                            text: part["text"].as_str().unwrap_or("").to_string(),
                        }),
                        Some("tool") => {
                            let state = &part["state"];
                            items.push(Item::Call {
                                tool: part["name"].as_str().unwrap_or("?").to_string(),
                                input: input(&state["input"]),
                            });
                            items.push(Item::Result {
                                text: text_of(&state["content"]),
                                error: state["status"] == "error",
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn format(items: &[Item]) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            Item::Said { who, text } => {
                let text = text.trim();
                if !text.is_empty() {
                    out.push_str(&format!("── {who}\n{text}\n\n"));
                }
            }
            Item::Call { tool, input } => out.push_str(&format!("→ {tool} {input}\n")),
            Item::Result { text, error } => {
                let lines: Vec<&str> = text.trim().lines().collect();
                let mark = if *error { "error" } else { "result" };
                match lines.as_slice() {
                    [] => out.push_str(&format!("← {mark}: (empty)\n\n")),
                    lines => {
                        out.push_str(&format!("← {mark}:\n"));
                        for line in lines.iter().take(RESULT_LINES) {
                            out.push_str(&format!("  {}\n", clip(line, LINE)));
                        }
                        if lines.len() > RESULT_LINES {
                            out.push_str(&format!(
                                "  … {} more lines\n",
                                lines.len() - RESULT_LINES
                            ));
                        }
                        out.push('\n');
                    }
                }
            }
        }
    }
    out
}

/// The agent's session: the recorded one, or else the first begun after its
/// last start, which is then recorded.
pub fn find_session(roots: &Roots, run: &Run, label: &str, agent: &AgentRecord) -> Option<String> {
    if !agent.agent_session.is_empty() {
        return Some(agent.agent_session.clone());
    }
    let since = agent.started_at.parse().ok()?;
    let found = session_since(roots, &agent.kind, &agent.cwd, since)?;
    let record = |a: &mut AgentRecord| {
        if a.agent_session.is_empty() && a.started_at == agent.started_at {
            a.agent_session.clone_from(&found);
        }
    };
    let _ = match label {
        "coordinator" => run.update(|r| record(&mut r.coordinator)).map(|_| ()),
        id => worker::update(run, id, |w| record(&mut w.agent)).map(|_| ()),
    };
    Some(found)
}

/// Keeps copies of the transcripts of the run's coordinator and workers, or
/// of `only` (`coordinator` or a worker id). Returns a line per failure.
pub fn keep_agents(roots: &Roots, run: &Run, only: Option<&str>) -> Vec<String> {
    let Ok(record) = run.record() else {
        return Vec::new();
    };
    let mut agents = vec![("coordinator".to_string(), record.coordinator)];
    agents.extend(worker::list(run).into_iter().map(|w| (w.id, w.agent)));
    let mut failures = Vec::new();
    for (label, agent) in agents {
        if only.is_some_and(|only| only != label) || agent.cwd.is_empty() {
            continue;
        }
        let Some(id) = find_session(roots, run, &label, &agent) else {
            continue;
        };
        let dir = run.transcripts_dir(&label);
        if let Err(error) = keep(roots, &dir, &agent.kind, &agent.cwd, &id) {
            failures.push(format!(
                "{}: could not keep the {label} transcript {id}: {error}",
                run.key
            ));
        }
    }
    failures
}

/// A session of `kind` as readable text. Tabs become spaces: a terminal
/// cell shows no tab.
pub fn render(roots: &Roots, kind: &str, session: &Session) -> std::io::Result<String> {
    let text = String::from_utf8_lossy(&read(roots, session)?).into_owned();
    let mut items = Vec::new();
    if kind == "opencode" {
        let export: Value = serde_json::from_str(&text).map_err(std::io::Error::other)?;
        opencode_items(&export, &mut items);
    } else {
        let parse: fn(&Value, &mut Vec<Item>) = match kind {
            "claude" => claude_items,
            "codex" => codex_items,
            _ => cursor_items,
        };
        for line in text.lines() {
            if let Ok(record) = serde_json::from_str::<Value>(line) {
                parse(&record, &mut items);
            }
        }
    }
    Ok(format(&items).replace('\t', "    "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roots(home: &Path) -> Roots {
        Roots::from_env(&Env::for_test(home, &[]))
    }

    fn write(path: &Path, lines: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(path, text.join("\n") + "\n").unwrap();
    }

    fn found(sessions: Sessions) -> Vec<Session> {
        match sessions {
            Sessions::Found { sessions, .. } => sessions,
            Sessions::Unreadable(why) => panic!("unreadable: {why}"),
        }
    }

    const CWD: &str = "/home/me/.local/state/herdr-linear-agent/runs/acme/DATA-1";

    #[test]
    fn a_claude_transcript_is_found_by_its_folder_and_shown_as_steps() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(
            ".claude/projects/-home-me--local-state-herdr-linear-agent-runs-acme-DATA-1/s-1.jsonl",
        );
        let long = (1..=7)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            &path,
            &[
                json!({"type": "mode", "mode": "x"}),
                json!({"type": "user", "message": {"role": "user", "content": "[ticker] Start acme/DATA-1."}}),
                json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "<local-command-caveat>"}}),
                json!({"type": "assistant", "message": {"content": [
                    {"type": "thinking", "thinking": "hidden"},
                    {"type": "text", "text": "Reading the issue."},
                    {"type": "tool_use", "name": "Bash", "input": {"command": "herdr-linear-agent context acme/DATA-1"}}
                ]}}),
                json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "content": long.replace("line 1", "1\tline 1")}
                ]}}),
                json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "is_error": true, "content": [{"type": "text", "text": "denied"}]}
                ]}}),
            ],
        );
        let roots = roots(home.path());
        let claude = found(sessions(&roots, "claude", CWD));
        assert_eq!(
            claude.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s-1"]
        );
        assert_eq!(
            render(&roots, "claude", &claude[0]).unwrap(),
            "── user\n[ticker] Start acme/DATA-1.\n\n── assistant\nReading the issue.\n\n→ Bash {\"command\":\"herdr-linear-agent context acme/DATA-1\"}\n← result:\n  1    line 1\n  line 2\n  line 3\n  line 4\n  line 5\n  … 2 more lines\n\n← error:\n  denied\n\n"
        );
    }

    #[test]
    fn a_codex_session_is_the_first_one_begun_in_the_folder_after_the_start() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex/sessions/2026/09/30");
        let meta = |id: &str, at: &str, cwd: &str| json!({"type": "session_meta", "payload": {"id": id, "timestamp": at, "cwd": cwd}});
        write(
            &dir.join("rollout-a.jsonl"),
            &[
                meta("019a-1", "2026-09-30T10:00:05Z", CWD),
                json!({"type": "response_item", "payload": {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]}}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Start acme/DATA-1."}]}}),
                json!({"type": "response_item", "payload": {"type": "reasoning", "summary": []}}),
                json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{\"cmd\": \"ls\"}"}}),
                json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "a.txt\nb.txt"}}),
                json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch"}}),
                json!({"type": "response_item", "payload": {"type": "custom_tool_call_output", "output": [{"type": "input_text", "text": "Success."}]}}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}}),
            ],
        );
        write(
            &dir.join("rollout-b.jsonl"),
            &[meta("019a-0", "2026-09-30T09:00:00Z", CWD)],
        );
        write(
            &dir.join("rollout-c.jsonl"),
            &[meta("019a-2", "2026-09-30T11:00:00Z", CWD)],
        );
        write(
            &dir.join("rollout-d.jsonl"),
            &[meta("other", "2026-09-30T10:00:01Z", "/elsewhere")],
        );
        let roots = roots(home.path());
        let since: Timestamp = "2026-09-30T10:00:00Z".parse().unwrap();
        assert_eq!(
            session_since(&roots, "codex", CWD, since).as_deref(),
            Some("019a-1"),
            "the first one at or after the start, in the folder"
        );
        let session = original(&roots, "codex", CWD, "019a-1").unwrap();
        assert_eq!(
            render(&roots, "codex", &session).unwrap(),
            "── user\nStart acme/DATA-1.\n\n→ shell {\"cmd\": \"ls\"}\n← result:\n  a.txt\n  b.txt\n\n→ apply_patch *** Begin Patch *** End Patch\n← result:\n  Success.\n\n── assistant\nDone.\n\n"
        );
    }

    #[test]
    fn a_cursor_transcript_has_no_results_and_an_unknown_kind_is_not_read() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(
            ".cursor/projects/home-me-local-state-herdr-linear-agent-runs-acme-DATA-1/agent-transcripts/c-1/c-1.jsonl",
        );
        write(
            &path,
            &[
                json!({"role": "user", "message": {"content": [{"type": "text", "text": "Start."}]}}),
                json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "Shell", "input": {"command": "ls"}}]}}),
            ],
        );
        let roots = roots(home.path());
        let cursor = found(sessions(&roots, "cursor", CWD));
        assert_eq!(
            render(&roots, "cursor", &cursor[0]).unwrap(),
            "── user\nStart.\n\n→ Shell {\"command\":\"ls\"}\n"
        );
        assert!(matches!(
            sessions(&roots, "gemini", CWD),
            Sessions::Unreadable(_)
        ));
        assert_eq!(
            sessions(&roots, "claude", "/gone"),
            Sessions::Found {
                sessions: Vec::new(),
                searched: home
                    .path()
                    .join(".claude/projects/-gone")
                    .display()
                    .to_string(),
            }
        );
    }

    /// A fake `opencode` that lists two sessions of `cwd` and exports one.
    fn fake_opencode(home: &Path, cwd: &Path) -> Roots {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let list = json!([
            {"id": "ses_new", "created": 1_790_000_100_000_i64, "directory": cwd},
            {"id": "ses_old", "created": 1_790_000_000_000_i64, "directory": cwd},
            {"id": "ses_else", "created": 1_790_000_200_000_i64, "directory": "/elsewhere"}
        ]);
        let export = json!({"info": {"id": "ses_new"}, "messages": [
            {"type": "user", "text": "Start acme/DATA-1."},
            {"type": "assistant", "content": [
                {"type": "tool", "name": "shell", "state": {"status": "completed", "input": {"command": "ls"}, "content": [{"type": "text", "text": "a.txt\n"}]}},
                {"type": "tool", "name": "read", "state": {"status": "error", "input": {"path": "x"}, "content": [{"type": "text", "text": "no such file"}]}}
            ]},
            {"type": "assistant", "content": [{"type": "text", "text": "Done."}]},
            {"type": "idle"}
        ]});
        let script = format!(
            "#!/bin/sh\ncase \"$2\" in\n  list) printf '%s\\n' '{list}' ;;\n  export) [ \"$3\" = ses_new ] && printf '%s\\n' '{export}' || exit 1 ;;\nesac\n"
        );
        let path = bin.join("opencode");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path_var = format!("{}:/usr/bin:/bin", bin.display());
        Roots::from_env(&Env::for_test(home, &[("PATH", &path_var)]))
    }

    #[test]
    fn opencode_sessions_come_from_its_list_and_their_export_is_shown() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("run");
        std::fs::create_dir_all(&cwd).unwrap();
        let roots = fake_opencode(home.path(), &cwd);
        let cwd = cwd.to_string_lossy().into_owned();
        let since = Timestamp::from_millisecond(1_790_000_050_000).unwrap();
        assert_eq!(
            session_since(&roots, "opencode", &cwd, since).as_deref(),
            Some("ses_new")
        );
        let session = original(&roots, "opencode", &cwd, "ses_new").unwrap();
        assert_eq!(session.source, Source::Export { cwd: cwd.clone() });
        assert_eq!(
            render(&roots, "opencode", &session).unwrap(),
            "── user\nStart acme/DATA-1.\n\n→ shell {\"command\":\"ls\"}\n← result:\n  a.txt\n\n→ read {\"path\":\"x\"}\n← error:\n  no such file\n\n── assistant\nDone.\n\n"
        );
        let dir = home.path().join("kept");
        assert!(keep(&roots, &dir, "opencode", &cwd, "ses_new").unwrap());
        assert!(dir.join("ses_new.json").is_file());
    }

    #[test]
    fn a_copy_is_written_again_only_when_the_original_grew_and_outlives_it() {
        let home = tempfile::tempdir().unwrap();
        let roots = roots(home.path());
        let original = home.path().join(
            ".claude/projects/-home-me--local-state-herdr-linear-agent-runs-acme-DATA-1/s-1.jsonl",
        );
        let line = json!({"type": "user", "message": {"content": "one"}});
        write(&original, std::slice::from_ref(&line));
        let dir = home.path().join("run/.state/transcripts/coordinator");
        assert!(
            keep(&roots, &dir, "claude", CWD, "s-1").unwrap(),
            "first copy"
        );
        assert!(
            !keep(&roots, &dir, "claude", CWD, "s-1").unwrap(),
            "unchanged"
        );
        write(
            &original,
            &[
                line.clone(),
                json!({"type": "user", "message": {"content": "two"}}),
            ],
        );
        assert!(keep(&roots, &dir, "claude", CWD, "s-1").unwrap(), "grew");
        std::fs::remove_file(&original).unwrap();
        assert!(!keep(&roots, &dir, "claude", CWD, "s-1").unwrap(), "gone");
        let copies = kept(&dir);
        assert_eq!(copies.len(), 1);
        assert!(copies[0].kept);
        assert_eq!(
            render(&roots, "claude", &copies[0]).unwrap(),
            "── user\none\n\n── user\ntwo\n\n"
        );
    }
}
