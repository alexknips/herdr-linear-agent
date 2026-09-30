//! Agents' own transcripts, found by the folder the agent ran in and shown
//! read-only: what each side said, each tool call, and the start of each
//! result. Herdr reports no session id without its agent integrations, so
//! the folder is the only link from a run to them.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jiff::Timestamp;
use serde_json::Value;

use crate::paths::Env;

/// Where each kind keeps its transcripts.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `<dir>/<folder with every non-alphanumeric byte as ->/<session>.jsonl`.
    claude: PathBuf,
    /// `<dir>/YYYY/MM/DD/rollout-*.jsonl`, the folder in the first line.
    codex: PathBuf,
    /// `<dir>/<folder's alphanumeric runs joined by ->/agent-transcripts/<id>/<id>.jsonl`.
    cursor: PathBuf,
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
        }
    }
}

/// One transcript file of an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The id `resume_args` takes.
    pub id: String,
    pub path: PathBuf,
    pub modified: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sessions {
    /// Oldest first; empty when none is left in `searched`.
    Found {
        sessions: Vec<Session>,
        searched: PathBuf,
    },
    /// The kind keeps no transcript the plugin can read.
    Unreadable(&'static str),
}

fn modified(path: &Path) -> Option<Timestamp> {
    let time = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = time.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Timestamp::from_second(i64::try_from(since.as_secs()).ok()?).ok()
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

fn session(id: String, path: PathBuf) -> Session {
    Session {
        id,
        modified: modified(&path),
        path,
    }
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

/// The transcripts an agent of `kind` left for `cwd`.
pub fn sessions(roots: &Roots, kind: &str, cwd: &str) -> Sessions {
    let (mut sessions, searched) = match kind {
        "claude" => {
            let folder: String = cwd
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let dir = roots.claude.join(folder);
            let found = jsonl_in(&dir)
                .into_iter()
                .map(|p| session(stem(&p), p))
                .collect();
            (found, dir)
        }
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
                        let id = meta["payload"]["id"].as_str().unwrap_or("").to_string();
                        found.push(session(id, path));
                    }
                }
            }
            (found, roots.codex.clone())
        }
        "cursor" => {
            let folder = cwd
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("-");
            let dir = roots.cursor.join(folder).join("agent-transcripts");
            let mut found = Vec::new();
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                for path in jsonl_in(&entry.path()) {
                    found.push(session(stem(&path), path));
                }
            }
            (found, dir)
        }
        "opencode" => {
            return Sessions::Unreadable(
                "OpenCode keeps its sessions in a SQLite database, which this plugin does not read",
            );
        }
        _ => return Sessions::Unreadable("this agent kind keeps no transcript the plugin knows"),
    };
    sessions.sort_by(|a, b| a.modified.cmp(&b.modified).then(a.path.cmp(&b.path)));
    Sessions::Found { sessions, searched }
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

/// A transcript of `kind` as readable text. Tabs become spaces: a terminal
/// cell shows no tab.
pub fn render(kind: &str, path: &Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    let parse: fn(&Value, &mut Vec<Item>) = match kind {
        "claude" => claude_items,
        "codex" => codex_items,
        _ => cursor_items,
    };
    let mut items = Vec::new();
    for line in text.lines() {
        if let Ok(record) = serde_json::from_str::<Value>(line) {
            parse(&record, &mut items);
        }
    }
    Ok(format(&items).replace('\t', "    "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(home: &Path) -> Roots {
        Roots::from_env(&Env::for_test(home, &[]))
    }

    fn write(path: &Path, lines: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(path, text.join("\n") + "\n").unwrap();
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
                serde_json::json!({"type": "mode", "mode": "x"}),
                serde_json::json!({"type": "user", "message": {"role": "user", "content": "[ticker] Start acme/DATA-1."}}),
                serde_json::json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "<local-command-caveat>"}}),
                serde_json::json!({"type": "assistant", "message": {"content": [
                    {"type": "thinking", "thinking": "hidden"},
                    {"type": "text", "text": "Reading the issue."},
                    {"type": "tool_use", "name": "Bash", "input": {"command": "herdr-linear-agent context acme/DATA-1"}}
                ]}}),
                serde_json::json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "content": long.replace("line 1", "1\tline 1")}
                ]}}),
                serde_json::json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "is_error": true, "content": [{"type": "text", "text": "denied"}]}
                ]}}),
            ],
        );
        let Sessions::Found { sessions, .. } = sessions(&roots(home.path()), "claude", CWD) else {
            panic!("claude is readable");
        };
        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s-1"]
        );
        assert_eq!(
            render("claude", &sessions[0].path).unwrap(),
            "── user\n[ticker] Start acme/DATA-1.\n\n── assistant\nReading the issue.\n\n→ Bash {\"command\":\"herdr-linear-agent context acme/DATA-1\"}\n← result:\n  1    line 1\n  line 2\n  line 3\n  line 4\n  line 5\n  … 2 more lines\n\n← error:\n  denied\n\n"
        );
    }

    #[test]
    fn a_codex_transcript_is_found_by_the_folder_in_its_first_line() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex/sessions/2026/09/30");
        write(
            &dir.join("rollout-a.jsonl"),
            &[
                serde_json::json!({"type": "session_meta", "payload": {"id": "019a-1", "cwd": CWD}}),
                serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]}}),
                serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Start acme/DATA-1."}]}}),
                serde_json::json!({"type": "response_item", "payload": {"type": "reasoning", "summary": []}}),
                serde_json::json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{\"cmd\": \"ls\"}"}}),
                serde_json::json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "a.txt\nb.txt"}}),
                serde_json::json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch"}}),
                serde_json::json!({"type": "response_item", "payload": {"type": "custom_tool_call_output", "output": [{"type": "input_text", "text": "Success."}]}}),
                serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}}),
            ],
        );
        write(
            &dir.join("rollout-b.jsonl"),
            &[
                serde_json::json!({"type": "session_meta", "payload": {"id": "other", "cwd": "/elsewhere"}}),
            ],
        );
        let Sessions::Found { sessions, .. } = sessions(&roots(home.path()), "codex", CWD) else {
            panic!("codex is readable");
        };
        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["019a-1"]
        );
        assert_eq!(
            render("codex", &sessions[0].path).unwrap(),
            "── user\nStart acme/DATA-1.\n\n→ shell {\"cmd\": \"ls\"}\n← result:\n  a.txt\n  b.txt\n\n→ apply_patch *** Begin Patch *** End Patch\n← result:\n  Success.\n\n── assistant\nDone.\n\n"
        );
    }

    #[test]
    fn a_cursor_transcript_has_no_results_and_opencode_is_not_read() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(
            ".cursor/projects/home-me-local-state-herdr-linear-agent-runs-acme-DATA-1/agent-transcripts/c-1/c-1.jsonl",
        );
        write(
            &path,
            &[
                serde_json::json!({"role": "user", "message": {"content": [{"type": "text", "text": "Start."}]}}),
                serde_json::json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "Shell", "input": {"command": "ls"}}]}}),
            ],
        );
        let roots = roots(home.path());
        let Sessions::Found {
            sessions: found, ..
        } = sessions(&roots, "cursor", CWD)
        else {
            panic!("cursor is readable");
        };
        assert_eq!(
            render("cursor", &found[0].path).unwrap(),
            "── user\nStart.\n\n→ Shell {\"command\":\"ls\"}\n"
        );
        assert!(matches!(
            sessions(&roots, "opencode", CWD),
            Sessions::Unreadable(why) if why.contains("SQLite")
        ));
        assert_eq!(
            sessions(&roots, "claude", "/gone"),
            Sessions::Found {
                sessions: Vec::new(),
                searched: home.path().join(".claude/projects/-gone"),
            }
        );
    }
}
