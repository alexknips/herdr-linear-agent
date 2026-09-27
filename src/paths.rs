//! Where herdr-linear-agent keeps its files, and the environment it reads.
//!
//! Config and state follow the XDG Base Directory specification on every
//! platform, macOS included: an absolute `$XDG_CONFIG_HOME` or
//! `$XDG_STATE_HOME` wins; an unset or relative one falls back to
//! `~/.config` and `~/.local/state`. Herdr's `HERDR_PLUGIN_*_DIR` variables are
//! deliberately not used: agent panes do not receive them, and an agent must
//! resolve the same paths as the plugin's own commands.
//!
//! Nothing here reads the process environment directly: callers pass an
//! `Env`, so resolution is testable.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::process::Runner;

pub const APP: &str = "herdr-linear-agent";

#[derive(Debug, Clone)]
pub struct Env {
    vars: BTreeMap<String, String>,
    pub home: PathBuf,
}

impl Env {
    pub fn from_process() -> Result<Self> {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        let home = vars
            .get("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(Env { vars, home })
    }

    #[cfg(test)]
    pub fn for_test(home: &Path, vars: &[(&str, &str)]) -> Self {
        Env {
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            home: home.to_path_buf(),
        }
    }

    /// A variable's value; an empty value counts as unset.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    fn xdg(&self, key: &str, fallback: &str) -> PathBuf {
        match self.var(key) {
            Some(value) if Path::new(value).is_absolute() => PathBuf::from(value),
            _ => self.home.join(fallback),
        }
    }

    /// `$XDG_CONFIG_HOME/herdr-linear-agent`: `config.toml`, written by the user.
    pub fn config_dir(&self) -> PathBuf {
        self.xdg("XDG_CONFIG_HOME", ".config").join(APP)
    }

    /// `$XDG_STATE_HOME/herdr-linear-agent`: runs, the ticker's lock and log,
    /// progress records and the credential lock.
    pub fn state_dir(&self) -> PathBuf {
        self.xdg("XDG_STATE_HOME", ".local/state").join(APP)
    }

    /// `HERDR_BIN_PATH` when set, else `herdr` on `PATH`.
    pub fn herdr_bin(&self) -> String {
        self.var("HERDR_BIN_PATH").unwrap_or("herdr").to_string()
    }
}

/// This binary's own path with symbolic links resolved, so a path written into
/// AGENTS.md or a brief survives a link changing.
pub fn binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not find this binary's own path")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// What every subcommand works from: the environment and the runner all
/// external commands go through.
pub struct Ctx<'a> {
    pub env: &'a Env,
    pub runner: &'a dyn Runner,
    /// False in tests, so commands that ensure a ticker never spawn a process.
    pub detached_ticker: bool,
}

impl Ctx<'_> {
    pub fn state_dir(&self) -> PathBuf {
        self.env.state_dir()
    }

    pub fn config_dir(&self) -> PathBuf {
        self.env.config_dir()
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.state_dir().join("runs")
    }

    /// Creates the state directory (mode 0700) when it is missing.
    pub fn ensure_state_dir(&self) -> Result<PathBuf> {
        let dir = self.state_dir();
        if !dir.is_dir() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_absolute_values_win_and_relative_ones_fall_back() {
        let home = Path::new("/h");
        let env = Env::for_test(
            home,
            &[("XDG_CONFIG_HOME", "/cfg"), ("XDG_STATE_HOME", "/st")],
        );
        assert_eq!(env.config_dir(), PathBuf::from("/cfg/herdr-linear-agent"));
        assert_eq!(env.state_dir(), PathBuf::from("/st/herdr-linear-agent"));

        let env = Env::for_test(
            home,
            &[("XDG_CONFIG_HOME", "relative"), ("XDG_STATE_HOME", "")],
        );
        assert_eq!(
            env.config_dir(),
            PathBuf::from("/h/.config/herdr-linear-agent")
        );
        assert_eq!(
            env.state_dir(),
            PathBuf::from("/h/.local/state/herdr-linear-agent")
        );
    }

    #[test]
    fn herdr_bin_prefers_the_variable() {
        assert_eq!(
            Env::for_test(Path::new("/h"), &[("HERDR_BIN_PATH", "/opt/herdr")]).herdr_bin(),
            "/opt/herdr"
        );
        assert_eq!(
            Env::for_test(Path::new("/h"), &[("HERDR_BIN_PATH", "")]).herdr_bin(),
            "herdr"
        );
    }
}
