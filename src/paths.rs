//! Where the plugin keeps its config and state, and the process environment
//! it reads them from.
//!
//! Agents run in panes that never see `HERDR_PLUGIN_STATE_DIR` or
//! `HERDR_PLUGIN_CONFIG_DIR`, and they must find the same folders as the
//! ticker, so both come from the XDG variables and `HOME` alone.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

use crate::process::Runner;

const APP: &str = "herdr-linear-agent";

/// A snapshot of the variables the plugin reads. Tests build one by hand so
/// they never depend on the real environment.
#[derive(Debug, Clone)]
pub struct Env {
    pub home: PathBuf,
    vars: HashMap<String, String>,
}

impl Env {
    pub fn from_process() -> Result<Env> {
        let vars: HashMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        let home = match vars.get("HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home),
            _ => bail!("HOME is not set, so the config and state folders are unknown"),
        };
        Ok(Env { home, vars })
    }

    #[cfg(test)]
    pub fn for_test(home: &std::path::Path, vars: &[(&str, &str)]) -> Env {
        let vars = vars.iter().map(|&(k, v)| (k.into(), v.into()));
        Env {
            home: home.into(),
            vars: vars.collect(),
        }
    }

    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// The plugin's folder under `$<var>` when that is absolute, else under
    /// `~/<default>`.
    fn app_dir(&self, var: &str, default: &str) -> PathBuf {
        self.var(var)
            .map(PathBuf::from)
            .filter(|base| base.is_absolute())
            .unwrap_or_else(|| self.home.join(default))
            .join(APP)
    }

    pub fn config_dir(&self) -> PathBuf {
        self.app_dir("XDG_CONFIG_HOME", ".config")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.app_dir("XDG_STATE_HOME", ".local/state")
    }

    pub fn herdr_bin(&self) -> String {
        match self.var("HERDR_BIN_PATH") {
            Some(bin) if !bin.is_empty() => bin.to_string(),
            _ => "herdr".to_string(),
        }
    }
}

/// What a command runs with: the environment, the child-process runner, and
/// whether `ticker start` may spawn a real ticker (never in tests).
pub struct Ctx<'a> {
    pub env: &'a Env,
    pub runner: &'a dyn Runner,
    pub detached_ticker: bool,
}

impl Ctx<'_> {
    pub fn config_dir(&self) -> PathBuf {
        self.env.config_dir()
    }

    pub fn state_dir(&self) -> PathBuf {
        self.env.state_dir()
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.env.state_dir().join("runs")
    }

    /// The private state directory, created when missing.
    pub fn ensure_state_dir(&self) -> Result<PathBuf> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

        let state = self.env.state_dir();

        if let Some(parent) = state.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow!(
                    "cannot create the parent of the state folder {}: {e}",
                    state.display()
                )
            })?;
        }

        match std::fs::DirBuilder::new().mode(0o700).create(&state) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(anyhow!(
                    "cannot create the state folder {}: {error}",
                    state.display()
                ));
            }
        }

        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(&state)
            .map_err(|e| anyhow!("cannot open the state folder {}: {e}", state.display()))?;
        if directory.metadata()?.uid() != unsafe { libc::geteuid() } {
            bail!(
                "the state folder {} is not owned by the current user",
                state.display()
            );
        }
        directory
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|e| anyhow!("cannot restrict the state folder {}: {e}", state.display()))?;

        Ok(state)
    }
}

/// The absolute path of the running executable.
pub fn binary() -> Result<PathBuf> {
    std::env::current_exe().context("could not find the path of this executable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::fake::FakeRunner;
    use std::path::Path;

    #[test]
    fn folders_follow_absolute_xdg_values_and_default_under_home() {
        let home = Path::new("/home/ann");
        // Herdr's plugin folder variables are never read.
        let defaults = Env::for_test(
            home,
            &[
                ("HERDR_PLUGIN_STATE_DIR", "/plugin/state"),
                ("HERDR_PLUGIN_CONFIG_DIR", "/plugin/config"),
            ],
        );
        assert_eq!(
            defaults.config_dir(),
            Path::new("/home/ann/.config/herdr-linear-agent")
        );
        assert_eq!(
            defaults.state_dir(),
            Path::new("/home/ann/.local/state/herdr-linear-agent")
        );

        let set = Env::for_test(
            home,
            &[("XDG_CONFIG_HOME", "/cfg"), ("XDG_STATE_HOME", "/st")],
        );
        assert_eq!(set.config_dir(), Path::new("/cfg/herdr-linear-agent"));
        assert_eq!(set.state_dir(), Path::new("/st/herdr-linear-agent"));

        for bad in ["", "relative/dir", "./x"] {
            let env = Env::for_test(home, &[("XDG_CONFIG_HOME", bad), ("XDG_STATE_HOME", bad)]);
            assert_eq!(env.config_dir(), defaults.config_dir(), "{bad:?}");
            assert_eq!(env.state_dir(), defaults.state_dir(), "{bad:?}");
        }
    }

    #[test]
    fn the_herdr_binary_comes_from_a_non_empty_variable() {
        let home = Path::new("/h");
        assert_eq!(Env::for_test(home, &[]).herdr_bin(), "herdr");
        assert_eq!(
            Env::for_test(home, &[("HERDR_BIN_PATH", "")]).herdr_bin(),
            "herdr"
        );
        assert_eq!(
            Env::for_test(home, &[("HERDR_BIN_PATH", "/opt/herdr/bin/herdr")]).herdr_bin(),
            "/opt/herdr/bin/herdr"
        );
    }

    #[test]
    fn the_context_puts_runs_under_the_state_dir_and_creates_it_on_demand() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: false,
        };
        let state = home.path().join(".local/state/herdr-linear-agent");
        assert_eq!(ctx.runs_dir(), state.join("runs"));
        assert_eq!(
            ctx.config_dir(),
            home.path().join(".config/herdr-linear-agent")
        );
        assert!(!state.exists());
        assert_eq!(ctx.ensure_state_dir().unwrap(), state);
        assert!(state.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn existing_state_directory_is_restricted_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("herdr-linear-agent");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();

        let state_home = home.path().to_str().unwrap();
        let env = Env::for_test(home.path(), &[("XDG_STATE_HOME", state_home)]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: false,
        };

        ctx.ensure_state_dir().unwrap();

        let mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "mode is {mode:04o}");
    }

    #[cfg(unix)]
    #[test]
    fn new_state_directory_is_created_with_owner_only_access() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("herdr-linear-agent");
        let state_home = home.path().to_str().unwrap();
        let env = Env::for_test(home.path(), &[("XDG_STATE_HOME", state_home)]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: false,
        };

        assert_eq!(ctx.ensure_state_dir().unwrap(), state);
        let mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "mode is {mode:04o}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_state_directory_is_rejected_without_changing_its_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("target");
        let state = home.path().join("herdr-linear-agent");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &state).unwrap();

        let state_home = home.path().to_str().unwrap();
        let env = Env::for_test(home.path(), &[("XDG_STATE_HOME", state_home)]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: false,
        };

        assert!(ctx.ensure_state_dir().is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
