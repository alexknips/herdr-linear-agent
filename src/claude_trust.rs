//! Marks folders as trusted in Claude Code's own config, so an interactive
//! `claude` started there does not stop at its workspace trust dialog.
//!
//! Claude Code keeps that decision per folder in `~/.claude.json`
//! (`$CLAUDE_CONFIG_DIR/.claude.json` when set), under
//! `projects.<absolute path>.hasTrustDialogAccepted`. The layout is not
//! documented, so this is best effort: when the file is missing or its shape
//! changes, nothing is written and the dialog shows as before, where the
//! ticker's "someone needs to answer in Herdr" request covers it.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::paths::Env;

fn config_path(env: &Env) -> PathBuf {
    match env.var("CLAUDE_CONFIG_DIR") {
        Some(dir) => Path::new(dir).join(".claude.json"),
        None => env.home.join(".claude.json"),
    }
}

/// Adds `hasTrustDialogAccepted = true` for each folder that lacks it,
/// keeping every other key, the key order and the file's permissions.
/// Returns whether the file was changed.
pub fn trust(env: &Env, dirs: &[&str]) -> Result<bool> {
    let path = config_path(env);
    let Ok(text) = std::fs::read_to_string(&path) else {
        // Claude Code has not been set up here; do not create its config.
        return Ok(false);
    };
    let mut config: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} does not parse", path.display()))?;
    let Some(projects) = config
        .as_object_mut()
        .map(|c| c.entry("projects").or_insert_with(|| json!({})))
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };
    let mut changed = false;
    for dir in dirs.iter().filter(|d| !d.is_empty()) {
        let project = projects.entry(dir.to_string()).or_insert_with(|| json!({}));
        let Some(project) = project.as_object_mut() else {
            continue;
        };
        if project.get("hasTrustDialogAccepted") != Some(&Value::Bool(true)) {
            project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
            changed = true;
        }
    }
    if changed {
        write_preserving_mode(&path, serde_json::to_string_pretty(&config)?.as_bytes())?;
    }
    Ok(changed)
}

/// A temporary file with the original's mode, then a rename, so Claude Code
/// never reads a half-written config.
fn write_preserving_mode(path: &Path, contents: &[u8]) -> Result<()> {
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let tmp = path.with_file_name(format!(
        ".claude.json.herdr-linear-agent.{}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_is_added_once_and_everything_else_is_kept() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let path = home.path().join(".claude.json");
        std::fs::write(&path, r#"{"zeta":1,"projects":{"/other":{"allowedTools":[],"hasTrustDialogAccepted":false}},"alpha":{"x":2}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(trust(&env, &["/runs/DATA-1", "/other", ""]).unwrap());
        let config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            config["projects"]["/runs/DATA-1"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(config["projects"]["/other"]["hasTrustDialogAccepted"], true);
        assert_eq!(config["projects"]["/other"]["allowedTools"], json!([]));
        assert_eq!(config["alpha"]["x"], 2);
        let keys: Vec<&String> = config.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zeta", "projects", "alpha"], "key order is kept");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        assert!(
            !trust(&env, &["/runs/DATA-1"]).unwrap(),
            "nothing to change"
        );
    }

    #[test]
    fn a_missing_or_unexpected_config_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        assert!(!trust(&env, &["/runs/DATA-1"]).unwrap());
        assert!(!home.path().join(".claude.json").exists());

        std::fs::write(home.path().join(".claude.json"), "[]").unwrap();
        assert!(!trust(&env, &["/runs/DATA-1"]).unwrap());

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".claude.json"), "{}").unwrap();
        let env = Env::for_test(
            home.path(),
            &[("CLAUDE_CONFIG_DIR", dir.path().to_str().unwrap())],
        );
        assert!(trust(&env, &["/runs/DATA-1"]).unwrap());
        assert!(
            std::fs::read_to_string(dir.path().join(".claude.json"))
                .unwrap()
                .contains("/runs/DATA-1")
        );
    }
}
