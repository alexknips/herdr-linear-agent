//! `$XDG_CONFIG_HOME/herdr-linear-agent/config.toml`, written by the user.
//!
//! The config is the only place that names Linear teams, repositories, agent
//! profiles (kind, model, effort, approval flags), routing rules, limits and
//! the Herdr session. Agents choose among these by name; they never pass a
//! raw flag.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::agents;

pub const FILE_NAME: &str = "config.toml";
pub const DEFAULT_CALLBACK_PORT: u16 = 43871;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub linear: Linear,
    #[serde(default)]
    pub herdr: Herdr,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub notifications: Notifications,
    #[serde(default)]
    pub claude: Claude,
    #[serde(default)]
    pub repositories: BTreeMap<String, Repository>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    pub routing: Routing,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Linear {
    /// The private OAuth application's client ID. Not a secret.
    pub client_id: String,
    #[serde(default = "default_callback_port")]
    pub callback_port: u16,
    /// Keys of the teams whose delegated issues are picked up.
    pub teams: Vec<String>,
    /// Linear user IDs whose replies in an Agent Session reach the coordinator.
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    /// The workflow state an issue moves to on `finish`.
    #[serde(default = "default_review_state")]
    pub review_state: String,
    /// Seconds between two polls of the delegated issues.
    #[serde(default = "default_linear_interval")]
    pub intake_interval_seconds: u64,
    /// Seconds between two reads of the active runs.
    #[serde(default = "default_linear_interval")]
    pub run_read_interval_seconds: u64,
}

fn default_callback_port() -> u16 {
    DEFAULT_CALLBACK_PORT
}

fn default_review_state() -> String {
    "In Review".into()
}

fn default_linear_interval() -> u64 {
    5
}

/// The longest Linear interval accepted: one hour.
const MAX_LINEAR_INTERVAL: u64 = 3600;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Herdr {
    /// The Herdr session runs are started in; herdr's default session when unset.
    pub session: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub max_runs: u32,
    pub max_workers_per_run: u32,
    pub max_agents: u32,
    pub run_timeout_hours: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_runs: 2,
            max_workers_per_run: 4,
            max_agents: 8,
            run_timeout_hours: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Notifications {
    /// Also show a Herdr notification when a run needs a person.
    pub herdr: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications { herdr: true }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Claude {
    /// Accept Claude Code's workspace trust dialog ahead of time: before
    /// starting Claude Code in a run folder or a worktree, set
    /// `hasTrustDialogAccepted` for the folder (and the worktree's main
    /// checkout) in `~/.claude.json`.
    pub auto_accept_trust_dialog: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    /// The main checkout, an absolute path.
    pub path: PathBuf,
    /// The branch workers start from. Never guessed.
    pub base: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub kind: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Extra agent CLI arguments (permission mode, sandbox), passed unchecked.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    /// The coordinator profile when no rule matches.
    pub default: String,
    /// The label group whose label names are sizes.
    #[serde(default)]
    pub size_label_group: Option<String>,
    /// The profiles a coordinator may start workers with.
    pub workers: Vec<String>,
    #[serde(default)]
    pub agent: Option<RoutingAgent>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoutingAgent {
    pub profile: String,
    #[serde(default = "default_routing_timeout")]
    pub timeout_seconds: u64,
}

fn default_routing_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default)]
    pub sizes: Vec<Size>,
    #[serde(default)]
    pub teams: Vec<String>,
    #[serde(default)]
    pub labels_any: Vec<String>,
    pub coordinator: String,
}

/// An issue's size, from its estimate, a size label or the routing agent.
/// The variants are T-shirt sizes, spelled as people write them.
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum Size {
    XS,
    S,
    M,
    L,
    XL,
    XXL,
    XXXL,
    #[default]
    #[serde(rename = "unknown")]
    Unknown,
}

impl Size {
    pub const KNOWN: [Size; 7] = [
        Size::XS,
        Size::S,
        Size::M,
        Size::L,
        Size::XL,
        Size::XXL,
        Size::XXXL,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Size::XS => "XS",
            Size::S => "S",
            Size::M => "M",
            Size::L => "L",
            Size::XL => "XL",
            Size::XXL => "XXL",
            Size::XXXL => "XXXL",
            Size::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Option<Size> {
        Self::KNOWN
            .into_iter()
            .chain([Size::Unknown])
            .find(|s| s.name() == text)
    }
}

impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl Config {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    pub fn load(config_dir: &Path) -> Result<Config> {
        let path = Self::path(config_dir);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("{} is not valid", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let linear = &self.linear;
        ensure!(
            !linear.client_id.trim().is_empty(),
            "linear.client_id is empty"
        );
        ensure!(
            linear.callback_port != 0,
            "linear.callback_port may not be 0"
        );
        ensure!(!linear.teams.is_empty(), "linear.teams lists no team");
        ensure!(
            !linear.review_state.trim().is_empty(),
            "linear.review_state is empty"
        );
        for (key, seconds) in [
            ("intake_interval_seconds", linear.intake_interval_seconds),
            (
                "run_read_interval_seconds",
                linear.run_read_interval_seconds,
            ),
        ] {
            ensure!(
                (1..=MAX_LINEAR_INTERVAL).contains(&seconds),
                "linear.{key} must be between 1 and {MAX_LINEAR_INTERVAL}"
            );
        }
        let limits = &self.limits;
        ensure!(
            limits.max_runs >= 1 && limits.max_workers_per_run >= 1 && limits.max_agents >= 2,
            "limits must allow one run with one worker"
        );
        ensure!(
            limits.run_timeout_hours >= 1,
            "limits.run_timeout_hours must be at least 1"
        );

        for (name, repo) in &self.repositories {
            ensure!(
                valid_name(name),
                "repository name `{name}` may use only letters, digits, `.`, `_` and `-`"
            );
            ensure!(
                repo.path.is_absolute(),
                "repositories.{name}.path must be an absolute path"
            );
            ensure!(
                !repo.base.trim().is_empty() && !repo.base.starts_with('-'),
                "repositories.{name}.base is not a branch name"
            );
        }
        for (name, profile) in &self.profiles {
            ensure!(
                valid_name(name),
                "profile name `{name}` may use only letters, digits, `.`, `_` and `-`"
            );
            ensure!(
                agents::is_kind(&profile.kind),
                "profiles.{name}.kind `{}` is not a Herdr agent kind",
                profile.kind
            );
            if profile.effort.is_some() && !agents::has_effort_flag(&profile.kind) {
                bail!(
                    "profiles.{name}: the `{}` CLI has no effort flag; put the effort in the model ID instead",
                    profile.kind
                );
            }
        }

        let routing = &self.routing;
        self.profile(&routing.default).context("routing.default")?;
        ensure!(
            !routing.workers.is_empty(),
            "routing.workers lists no profile"
        );
        for name in &routing.workers {
            self.profile(name).context("routing.workers")?;
        }
        for (n, rule) in routing.rules.iter().enumerate() {
            self.profile(&rule.coordinator)
                .with_context(|| format!("routing.rules[{n}].coordinator"))?;
        }
        if let Some(agent) = &routing.agent {
            let profile = self
                .profile(&agent.profile)
                .context("routing.agent.profile")?;
            ensure!(
                matches!(profile.kind.as_str(), "claude" | "codex"),
                "routing.agent.profile must be a `claude` or `codex` profile: only they return schema-checked JSON headless"
            );
            ensure!(
                agent.timeout_seconds >= 1,
                "routing.agent.timeout_seconds must be at least 1"
            );
        }
        Ok(())
    }

    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.profiles
            .get(name)
            .with_context(|| format!("no profile named `{name}` in [profiles]"))
    }

    /// A profile a coordinator may start a worker with.
    pub fn worker_profile(&self, name: &str) -> Result<&Profile> {
        if !self.routing.workers.iter().any(|w| w == name) {
            bail!(
                "`{name}` is not a worker profile; routing.workers lists {}",
                self.routing.workers.join(", ")
            );
        }
        self.profile(name)
    }

    pub fn repository(&self, name: &str) -> Result<&Repository> {
        self.repositories.get(name).with_context(|| {
            let known: Vec<&str> = self.repositories.keys().map(String::as_str).collect();
            format!(
                "`{name}` is not in the repository catalog ({})",
                if known.is_empty() {
                    "empty".to_string()
                } else {
                    known.join(", ")
                }
            )
        })
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.starts_with(['-', '.'])
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A config with every section, used across the test suite.
    pub const SAMPLE: &str = r#"
[linear]
client_id = "client-123"
teams = ["DATA"]
allowed_user_ids = ["user-1"]

[herdr]
session = "work"

[repositories.api]
path = "/src/api"
base = "main"
description = "The API server"

[repositories.web]
path = "/src/web"
base = "develop"

[profiles.coordinator]
kind = "claude"
model = "opus"
effort = "high"
args = ["--permission-mode", "auto"]
description = "default coordinator"

[profiles.coordinator-light]
kind = "claude"
model = "sonnet"
description = "small issues"

[profiles.router]
kind = "claude"
model = "haiku"

[profiles.standard]
kind = "claude"
model = "sonnet"
effort = "high"
args = ["--permission-mode", "auto"]
description = "scoped features and fixes"

[profiles.deep]
kind = "codex"
model = "gpt-6-sol"
effort = "xhigh"
args = ["-s", "workspace-write"]
description = "cross-module changes"

[routing]
default = "coordinator"
size_label_group = "size"
workers = ["standard", "deep"]

[routing.agent]
profile = "router"
timeout_seconds = 60

[[routing.rules]]
sizes = ["XS", "S"]
coordinator = "coordinator-light"
"#;

    #[test]
    fn the_sample_parses_with_defaults() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.linear.callback_port, DEFAULT_CALLBACK_PORT);
        assert_eq!(config.linear.review_state, "In Review");
        assert_eq!(config.limits, Limits::default());
        assert!(config.notifications.herdr);
        assert_eq!(config.routing.rules[0].sizes, [Size::XS, Size::S]);
        assert_eq!(config.worker_profile("deep").unwrap().kind, "codex");
        assert!(config.worker_profile("coordinator").is_err());
        assert!(
            config
                .repository("nope")
                .unwrap_err()
                .to_string()
                .contains("api, web")
        );
    }

    #[test]
    fn invalid_configs_are_refused() {
        let bad = |from: &str, to: &str| {
            let text = SAMPLE.replacen(from, to, 1);
            assert_ne!(text, SAMPLE, "{from}");
            Config::parse(&text).unwrap_err().to_string()
        };
        bad("teams = [\"DATA\"]", "teams = []");
        bad("kind = \"codex\"", "kind = \"chatgpt\"");
        bad("default = \"coordinator\"", "default = \"missing\"");
        bad(
            "workers = [\"standard\", \"deep\"]",
            "workers = [\"missing\"]",
        );
        bad("path = \"/src/api\"", "path = \"src/api\"");
        bad("profile = \"router\"", "profile = \"deep-x\"");
        bad("[herdr]", "[herdr]\nunknown = 1");
        // Effort only for kinds with an effort flag.
        let cursor = SAMPLE.replace(
            "[profiles.router]\nkind = \"claude\"",
            "[profiles.router]\nkind = \"cursor\"\neffort = \"high\"",
        );
        assert!(Config::parse(&cursor).is_err());
    }

    #[test]
    fn linear_intervals_default_to_five_seconds_and_are_checked() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(
            (
                config.linear.intake_interval_seconds,
                config.linear.run_read_interval_seconds
            ),
            (5, 5)
        );
        let set = |lines: &str| {
            Config::parse(&SAMPLE.replacen(
                "teams = [\"DATA\"]",
                &format!("teams = [\"DATA\"]\n{lines}"),
                1,
            ))
        };
        let config = set("intake_interval_seconds = 30\nrun_read_interval_seconds = 10").unwrap();
        assert_eq!(
            (
                config.linear.intake_interval_seconds,
                config.linear.run_read_interval_seconds
            ),
            (30, 10)
        );
        assert_eq!(
            set("intake_interval_seconds = 0").unwrap_err().to_string(),
            "linear.intake_interval_seconds must be between 1 and 3600"
        );
        assert_eq!(
            set("run_read_interval_seconds = 3601")
                .unwrap_err()
                .to_string(),
            "linear.run_read_interval_seconds must be between 1 and 3600"
        );
    }

    #[test]
    fn sizes_parse_and_print() {
        assert_eq!(Size::parse("XXL"), Some(Size::XXL));
        assert_eq!(Size::parse("unknown"), Some(Size::Unknown));
        assert_eq!(Size::parse("huge"), None);
        assert_eq!(Size::M.to_string(), "M");
    }
}
