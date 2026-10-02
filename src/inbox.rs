//! The coordinator's inbox: one file per item, `runs/<KEY>/inbox/<id>.md`,
//! holding the item's fields as TOML between two `+++` lines.
//!
//! The ticker writes items (an issue edit, a reply, a worker's new group);
//! `context` shows the unhandled ones and records which ids it showed;
//! `inbox done` moves items to `inbox/done/`. `--all` moves only the items
//! the coordinator has been shown, so an item written after its last
//! `context` is never handled unseen.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::run::Run;

/// Handled items are kept this long.
const DONE_KEPT: Duration = Duration::from_secs(30 * 24 * 3600);

/// The line that opens and closes an item's front matter.
const FENCE: &str = "+++";

/// The field order is the key order in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub created: String,
    pub summary: String,
}

impl Item {
    /// The counter that ends the id: `3` for `...-reply-reply-3`.
    fn counter(&self) -> u64 {
        self.id
            .rsplit('-')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }
}

fn inbox_dir(run: &Run) -> PathBuf {
    run.dir.join("inbox")
}

fn done_dir(run: &Run) -> PathBuf {
    run.dir.join("inbox/done")
}

fn counter_path(run: &Run) -> PathBuf {
    run.state_dir().join("inbox-counter.json")
}

fn seen_path(run: &Run) -> PathBuf {
    run.state_dir().join("inbox-seen.json")
}

fn item_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.md"))
}

/// Ids name files, so one that could leave the folder or hide is refused.
fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || id.starts_with('.') || id.contains('/') || id.contains("..") {
        bail!("`{id}` is not an inbox item id");
    }
    Ok(())
}

/// Keeps an id part to characters that are safe in a file name.
fn id_part(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The file's text: `+++`, one `key = value` line per field, `+++`.
fn render(item: &Item) -> Result<String> {
    let fields = toml::to_string(item).context("an inbox item does not serialize")?;
    Ok(format!("{FENCE}\n{fields}{FENCE}\n"))
}

/// The item a file's text holds, or `None` when it is not in that format.
fn parse(text: &str) -> Option<Item> {
    let inner = text.strip_prefix(FENCE)?.strip_prefix('\n')?;
    let fields = inner.trim_end().strip_suffix(FENCE)?;
    if !fields.ends_with('\n') {
        return None;
    }
    toml::from_str(fields).ok()
}

/// Writes an item and returns its id, `<time>-<kind>-<subject>-<n>`.
pub fn write(run: &Run, kind: &str, subject: &str, summary: &str) -> Result<String> {
    let _lock = run.lock()?;
    write_held(run, &_lock, kind, subject, summary)
}

pub fn write_held(
    run: &Run,
    _lock: &crate::run::RunLock,
    kind: &str,
    subject: &str,
    summary: &str,
) -> Result<String> {
    let n = files::read_json::<u64>(&counter_path(run)).unwrap_or(0) + 1;
    files::write_json(&counter_path(run), &n)?;
    let created = files::now();
    let stamp: String = created
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let item = Item {
        id: format!("{stamp}-{}-{}-{n}", id_part(kind), id_part(subject)),
        kind: kind.to_string(),
        subject: subject.to_string(),
        created,
        summary: summary.to_string(),
    };
    std::fs::create_dir_all(inbox_dir(run))?;
    let text = render(&item)?;
    files::write_atomic(&item_path(&inbox_dir(run), &item.id), text.as_bytes())?;
    Ok(item.id)
}

/// The items in `dir` that parse, oldest first, and the names of the files
/// that do not.
fn read_items(dir: &Path) -> (Vec<Item>, Vec<String>) {
    let (mut items, mut unreadable) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (items, unreadable);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let item = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| parse(&text));
        match item {
            Some(item) if name == format!("{}.md", item.id) => items.push(item),
            _ => unreadable.push(name),
        }
    }
    items.sort_by(|a, b| (&a.created, a.counter(), &a.id).cmp(&(&b.created, b.counter(), &b.id)));
    (items, unreadable)
}

/// The items not yet done, oldest first, each summary on one line.
pub fn unhandled(run: &Run) -> Vec<Item> {
    let (mut items, _) = read_items(&inbox_dir(run));
    for item in &mut items {
        item.summary = item.summary.replace("\r\n", " ").replace('\n', " ");
    }
    items
}

/// Records the ids `context` showed the coordinator, replacing the last set:
/// a digest shows every unhandled item, so nothing shown earlier is lost.
/// The file is a pretty JSON array with no final newline.
pub fn mark_seen(run: &Run, ids: &[String]) -> Result<()> {
    let _lock = run.lock()?;
    let text = serde_json::to_string_pretty(ids)?;
    files::write_atomic(&seen_path(run), text.as_bytes())
}

/// The ids the last `context` showed.
pub fn seen(run: &Run) -> Vec<String> {
    files::read_json(&seen_path(run)).unwrap_or_default()
}

/// Moves the named items and, with `all`, every item the coordinator has
/// been shown, to `inbox/done/`. Returns how many moved.
pub fn done(run: &Run, ids: &[String], all: bool) -> Result<usize> {
    for id in ids {
        check_id(id)?;
    }
    let _lock = run.lock()?;
    let mut targets: Vec<String> = ids.to_vec();
    if all {
        targets.extend(seen(run).into_iter().filter(|id| check_id(id).is_ok()));
    }
    targets.sort();
    targets.dedup();
    std::fs::create_dir_all(done_dir(run))?;
    let mut moved = 0;
    for id in &targets {
        let from = item_path(&inbox_dir(run), id);
        match std::fs::rename(&from, item_path(&done_dir(run), id)) {
            Ok(()) => moved += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("could not move {}", from.display())),
        }
    }
    Ok(moved)
}

/// Removes handled items older than 30 days, and moves the files in `inbox/`
/// that do not parse as items (`<id>.md` with the front matter) to
/// `inbox/done/`, so the caller logs each of them once. Returns their names.
pub fn prune_done(run: &Run) -> Vec<String> {
    let Ok(_lock) = run.lock() else {
        return Vec::new();
    };
    let now = SystemTime::now();
    if let Ok(entries) = std::fs::read_dir(done_dir(run)) {
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age > DONE_KEPT);
            if old && entry.file_type().is_ok_and(|t| t.is_file()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let (_, unreadable) = read_items(&inbox_dir(run));
    let _ = std::fs::create_dir_all(done_dir(run));
    unreadable
        .into_iter()
        .filter(|name| std::fs::rename(inbox_dir(run).join(name), done_dir(run).join(name)).is_ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::RunRecord;

    /// Two items as the released build wrote them, byte for byte.
    const REPLY_ID: &str = "20260928T014109Z-reply-reply-3";
    const REPLY: &str = "+++
id = \"20260928T014109Z-reply-reply-3\"
kind = \"reply\"
subject = \"reply\"
created = \"2026-09-28T01:41:10Z\"
summary = \"A new reply from user 6b7b1cde-f3cc-4886-8339-660f4851e476 is in conversation.md.\"
+++
";
    const WORKER_ID: &str = "20260928T020313Z-worker-w1-1";
    const WORKER: &str = "+++
id = \"20260928T020313Z-worker-w1-1\"
kind = \"worker\"
subject = \"w1\"
created = \"2026-09-28T02:03:14Z\"
summary = \"w1 (testing) is idle without a report; check its pane wH:p1.\"
+++
";

    fn run() -> (tempfile::TempDir, Run) {
        let home = tempfile::tempdir().unwrap();
        let run = Run::create(
            home.path(),
            RunRecord {
                workspace: "acme".into(),
                identifier: "DATA-7".into(),
                issue_id: "issue-7".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        (home, run)
    }

    fn ids(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    fn read(run: &Run, rel: &str) -> String {
        std::fs::read_to_string(run.dir.join(rel)).unwrap()
    }

    #[test]
    fn items_written_by_the_released_build_are_read_and_handled() {
        let (_home, run) = run();
        std::fs::write(run.dir.join(format!("inbox/{REPLY_ID}.md")), REPLY).unwrap();
        std::fs::write(run.dir.join(format!("inbox/{WORKER_ID}.md")), WORKER).unwrap();
        std::fs::write(run.state_dir().join("inbox-counter.json"), "3\n").unwrap();

        assert!(prune_done(&run).is_empty(), "both parse as items");
        let items = unhandled(&run);
        assert_eq!(
            items,
            [
                Item {
                    id: REPLY_ID.into(),
                    kind: "reply".into(),
                    subject: "reply".into(),
                    created: "2026-09-28T01:41:10Z".into(),
                    summary: "A new reply from user 6b7b1cde-f3cc-4886-8339-660f4851e476 \
                              is in conversation.md."
                        .into(),
                },
                Item {
                    id: WORKER_ID.into(),
                    kind: "worker".into(),
                    subject: "w1".into(),
                    created: "2026-09-28T02:03:14Z".into(),
                    summary: "w1 (testing) is idle without a report; check its pane wH:p1.".into(),
                },
            ],
            "ordered by created before the counter"
        );

        mark_seen(&run, &[REPLY_ID.to_string()]).unwrap();
        assert_eq!(
            read(&run, ".state/inbox-seen.json"),
            "[\n  \"20260928T014109Z-reply-reply-3\"\n]"
        );
        assert_eq!(done(&run, &[], true).unwrap(), 1);
        assert_eq!(ids(&unhandled(&run)), [WORKER_ID]);
        assert_eq!(read(&run, &format!("inbox/done/{REPLY_ID}.md")), REPLY);

        let next = write(&run, "worker", "w1", "next").unwrap();
        assert!(next.ends_with("-worker-w1-4"), "{next}");
        assert_eq!(read(&run, ".state/inbox-counter.json"), "4\n");
    }

    #[test]
    fn an_item_is_written_in_the_released_format() {
        let (_home, run) = run();
        let id = write(&run, "worker", "w1", "w1 (api) has a new report").unwrap();
        let item = &unhandled(&run)[0];
        assert_eq!(
            read(&run, &format!("inbox/{id}.md")),
            format!(
                "+++\nid = \"{id}\"\nkind = \"worker\"\nsubject = \"w1\"\n\
                 created = \"{}\"\nsummary = \"w1 (api) has a new report\"\n+++\n",
                item.created
            )
        );
        assert_eq!(item.id, id);
    }

    #[test]
    fn items_of_one_second_order_by_their_counter_as_a_number() {
        let (_home, run) = run();
        for (id, n) in [
            ("T-worker-w1-10", 10),
            ("T-worker-w1-9", 9),
            ("T-reply-reply-9", 9),
        ] {
            let item = Item {
                id: id.into(),
                kind: "worker".into(),
                subject: format!("{n}"),
                created: "2026-09-28T01:00:00Z".into(),
                summary: "same second".into(),
            };
            let path = item_path(&inbox_dir(&run), id);
            std::fs::write(path, render(&item).unwrap()).unwrap();
        }
        assert_eq!(
            ids(&unhandled(&run)),
            ["T-reply-reply-9", "T-worker-w1-9", "T-worker-w1-10"]
        );
    }

    #[test]
    fn items_are_listed_oldest_first_with_one_line_summaries() {
        let (_home, run) = run();
        let first = write(&run, "worker", "w1", "first line\nsecond line").unwrap();
        let second = write(&run, "reply", "reply", "A new reply.").unwrap();
        let third = write(&run, "worker", "w1", "again").unwrap();
        assert!(first.ends_with("-worker-w1-1"), "{first}");
        assert!(second.ends_with("-reply-reply-2"), "{second}");
        assert!(third.ends_with("-worker-w1-3"), "{third}");
        assert_ne!(first, third);

        let items = unhandled(&run);
        assert_eq!(ids(&items), [&first, &second, &third]);
        assert_eq!(items[0].summary, "first line second line");
        assert_eq!(items[0].kind, "worker");
        assert_eq!(items[0].subject, "w1");
        assert_eq!(items[1].summary, "A new reply.");
        assert!(run.dir.join(format!("inbox/{first}.md")).is_file());
    }

    #[test]
    fn done_all_moves_only_the_items_context_showed() {
        let (_home, run) = run();
        let shown = write(&run, "worker", "w1", "shown").unwrap();
        assert_eq!(done(&run, &[], true).unwrap(), 0, "nothing was shown yet");

        mark_seen(&run, std::slice::from_ref(&shown)).unwrap();
        assert_eq!(seen(&run), std::slice::from_ref(&shown));
        let later = write(&run, "worker", "w2", "written after context").unwrap();
        assert_eq!(done(&run, &[], true).unwrap(), 1);
        assert_eq!(ids(&unhandled(&run)), [&later]);
        assert!(run.dir.join(format!("inbox/done/{shown}.md")).is_file());

        assert_eq!(done(&run, &[], true).unwrap(), 0, "already moved");
        mark_seen(&run, std::slice::from_ref(&later)).unwrap();
        assert_eq!(
            seen(&run),
            std::slice::from_ref(&later),
            "the last context replaces the set"
        );
    }

    #[test]
    fn named_ids_move_whether_or_not_they_were_shown() {
        let (_home, run) = run();
        let a = write(&run, "issue", "issue", "edited").unwrap();
        let b = write(&run, "worker", "w1", "report").unwrap();
        let c = write(&run, "worker", "w2", "report").unwrap();
        mark_seen(&run, std::slice::from_ref(&a)).unwrap();
        assert_eq!(done(&run, std::slice::from_ref(&b), false).unwrap(), 1);
        assert_eq!(ids(&unhandled(&run)), [&a, &c]);
        assert_eq!(
            done(&run, &[c.clone(), "no-such-item".into()], true).unwrap(),
            2
        );
        assert!(unhandled(&run).is_empty());
    }

    #[test]
    fn ids_that_could_leave_the_folder_are_refused() {
        let (_home, run) = run();
        let kept = write(&run, "worker", "w1", "kept").unwrap();
        for bad in ["", "../run.json", "a/b", ".hidden", "x..y"] {
            let error = done(&run, &[kept.clone(), bad.into()], false).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("`{bad}` is not an inbox item id")
            );
        }
        assert_eq!(ids(&unhandled(&run)), [&kept], "nothing moved on a refusal");
    }

    #[test]
    fn pruning_drops_old_done_items_and_sets_aside_files_that_do_not_parse() {
        let (_home, run) = run();
        let old = write(&run, "worker", "w1", "old").unwrap();
        let recent = write(&run, "worker", "w1", "recent").unwrap();
        done(&run, &[old.clone(), recent.clone()], false).unwrap();
        let old_path = run.dir.join(format!("inbox/done/{old}.md"));
        let month_ago = SystemTime::now() - Duration::from_secs(31 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&old_path)
            .unwrap()
            .set_modified(month_ago)
            .unwrap();
        let open = write(&run, "reply", "reply", "open").unwrap();
        let inbox = run.dir.join("inbox");
        std::fs::write(inbox.join("plain.md"), "no front matter").unwrap();
        std::fs::write(inbox.join("renamed.md"), REPLY).unwrap();
        std::fs::write(inbox.join("unclosed.md"), &REPLY[..REPLY.len() - 4]).unwrap();
        std::fs::write(inbox.join("item.json"), r#"{"id":"item"}"#).unwrap();

        let mut set_aside = prune_done(&run);
        set_aside.sort();
        assert_eq!(
            set_aside,
            ["item.json", "plain.md", "renamed.md", "unclosed.md"]
        );
        assert!(!old_path.exists());
        assert!(run.dir.join(format!("inbox/done/{recent}.md")).exists());
        assert!(run.dir.join("inbox/done/plain.md").exists());
        assert_eq!(ids(&unhandled(&run)), [&open]);
        assert!(
            prune_done(&run).is_empty(),
            "each file that does not parse is reported once"
        );
    }
}
