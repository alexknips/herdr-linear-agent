//! Small helpers shared by every module: atomic file writes, hashing,
//! timestamps, slugs and shell quoting.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use jiff::Timestamp;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

/// The command that opens a URL in the user's browser.
#[cfg(target_os = "macos")]
pub const OPEN_COMMAND: &str = "open";
#[cfg(not(target_os = "macos"))]
pub const OPEN_COMMAND: &str = "xdg-open";

/// Writes the whole file or nothing: the bytes go to a hidden sibling first,
/// which is then renamed over `path`. The parent folder must exist.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let tmp: PathBuf = path.with_file_name(format!(
        ".{}.{}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("could not write {}", path.display()))
}

/// Pretty JSON with a final newline, written atomically.
pub fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

/// The file's value, or `None` when it is missing or does not parse.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The current time, RFC 3339 in UTC to the second.
pub fn now() -> String {
    Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Whole seconds from an RFC 3339 timestamp to `now`; `0` when the
/// timestamp does not parse.
pub fn seconds_since(timestamp: &str, now: Timestamp) -> i64 {
    timestamp
        .parse::<Timestamp>()
        .map(|then| now.duration_since(then).as_secs())
        .unwrap_or(0)
}

/// Lower-case ASCII words joined by single `-`, at most 40 characters.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(40);
    slug.trim_end_matches('-').to_string()
}

/// A word a POSIX shell reads back unchanged: bare when it holds only ASCII
/// letters, digits and `/._-`, single-quoted otherwise.
pub fn shell_quote(text: &str) -> String {
    let bare = |c: char| c.is_ascii_alphanumeric() || "/._-".contains(c);
    if !text.is_empty() && text.chars().all(bare) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The text of a `--file` style argument: standard input for `-`.
pub fn read_text_arg(path: &str) -> Result<String> {
    if path == "-" {
        return std::io::read_to_string(std::io::stdin()).context("could not read standard input");
    }
    std::fs::read_to_string(path).with_context(|| format!("could not read {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_short_lower_case_words() {
        assert_eq!(slugify("Fix the $(login) bug!"), "fix-the-login-bug");
        assert_eq!(slugify("???"), "");
        assert_eq!(slugify("  Émile's  API v2 "), "mile-s-api-v2");
        let long = slugify("one two three four five six seven eight nine ten");
        assert_eq!(long, "one-two-three-four-five-six-seven-eight");
        assert!(long.len() <= 40);
        assert_eq!(slugify(&"x".repeat(39)).len(), 39);
        assert_eq!(slugify(&format!("{} y", "x".repeat(39))), "x".repeat(39));
    }

    #[test]
    fn words_are_quoted_only_when_needed() {
        assert_eq!(
            shell_quote("/bin/herdr-linear-agent"),
            "/bin/herdr-linear-agent"
        );
        assert_eq!(shell_quote("a-b_c.d/E9"), "a-b_c.d/E9");
        assert_eq!(shell_quote("/my dir/it's"), r"'/my dir/it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm)"), "'$(rm)'");
        assert_eq!(shell_quote("naïve"), "'naïve'");
    }

    #[test]
    fn atomic_writes_replace_the_file_and_leave_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["record.json"]);

        let missing = dir.path().join("nowhere/record.json");
        assert!(write_atomic(&missing, b"x").is_err());
        assert!(!dir.path().join("nowhere").exists());
    }

    #[test]
    fn json_round_trips_and_unreadable_files_read_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.json");
        write_json(&path, &serde_json::json!({"a": [1, 2]})).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"a\": [\n    1,\n    2\n  ]\n}\n"
        );
        assert_eq!(read_json::<serde_json::Value>(&path).unwrap()["a"][1], 2);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_json::<u64>(&path), None);
        assert_eq!(read_json::<u64>(&dir.path().join("missing")), None);
    }

    #[test]
    fn hashes_are_lower_case_hex() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn seconds_since_counts_whole_seconds_or_is_zero() {
        let now: Timestamp = "2026-09-28T12:00:00Z".parse().unwrap();
        assert_eq!(seconds_since("2026-09-28T11:58:30Z", now), 90);
        assert_eq!(seconds_since("2026-09-28T13:00:00+02:00", now), 3600);
        assert_eq!(seconds_since("2026-09-28T11:59:59.900Z", now), 0);
        assert_eq!(seconds_since("2026-09-28T12:00:05Z", now), -5);
        assert_eq!(seconds_since("", now), 0);
        assert_eq!(seconds_since("yesterday", now), 0);
    }

    #[test]
    fn now_is_utc_rfc3339_to_the_second() {
        let text = now();
        assert_eq!(text.len(), "2026-09-28T12:00:00Z".len(), "{text}");
        assert!(text.ends_with('Z'));
        assert!(seconds_since(&text, Timestamp::now()) <= 1);
    }

    #[test]
    fn text_arguments_read_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.md");
        std::fs::write(&path, "hello\n").unwrap();
        assert_eq!(read_text_arg(path.to_str().unwrap()).unwrap(), "hello\n");
        let error = read_text_arg("/nonexistent/t.md").unwrap_err();
        assert_eq!(error.to_string(), "could not read /nonexistent/t.md");
    }
}
