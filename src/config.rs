//! `$XDG_CONFIG_HOME/herdr-linear-agent/config.toml` and the profile folders
//! next to it, `profiles/<name>/config.toml` with an optional
//! `profiles/<name>/instructions.md`, all written by the user. A profile is a
//! folder of its own so it can be handed to other people as it is.
//!
//! The config is the only place that names Linear teams, repositories, agent
//! profiles (kind, model, effort, approval flags), the routing candidates, limits and
//! the Herdr session. Agents choose among these by name; they never pass a
//! raw flag.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::Digest;

use crate::{agents, routing};

pub const FILE_NAME: &str = "config.toml";
const PROFILES_DIR: &str = "profiles";
const INSTRUCTIONS_FILE: &str = "instructions.md";
pub const DEFAULT_CALLBACK_PORT: u16 = 43871;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The Linear workspaces, by the name runs use for them.
    pub workspaces: BTreeMap<String, Workspace>,
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
    /// Read from the profile folders, never from `config.toml`.
    #[serde(skip)]
    pub profiles: BTreeMap<String, Profile>,
    pub routing: Routing,
}

/// One Linear workspace: its own OAuth application, token and app user.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    /// The private OAuth application's client ID. Not a secret.
    pub client_id: String,
    #[serde(default = "default_callback_port")]
    pub callback_port: u16,
    /// Seconds between two polls of the delegated issues.
    #[serde(default = "default_linear_interval")]
    pub intake_interval_seconds: u64,
    /// Seconds between two reads of the active runs.
    #[serde(default = "default_linear_interval")]
    pub run_read_interval_seconds: u64,
    /// The teams whose delegated issues are picked up, by team key.
    pub teams: BTreeMap<String, Team>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Team {
    /// Linear user IDs whose replies in an Agent Session reach the coordinator.
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    /// The workflow state an issue moves to on `finish`.
    #[serde(default = "default_review_state")]
    pub review_state: String,
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

/// A profile with its base chain resolved: what launch, routing and validation
/// see.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub kind: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Extra agent CLI arguments (permission mode, sandbox), passed unchecked.
    pub args: Vec<String>,
    pub description: String,
    /// The `instructions.md` of the profile and of the profiles it is based
    /// on, from the root to the profile itself; layers without one are left
    /// out. Markdown the plugin adds to the agent's instructions for work
    /// under this profile, after the built-in sheet. Only people write it.
    pub instructions: Vec<Instructions>,
    /// Environment variables for the routing agent's call. Herdr starts
    /// coordinators and workers and cannot pass them any, so only the
    /// routing agent's profile may set this.
    pub env: BTreeMap<String, String>,
}

/// One layer of a profile's instructions: whose `instructions.md` it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Instructions {
    pub profile: String,
    pub text: String,
}

/// A profile's `config.toml` as written. `base` names the profile it
/// inherits from; a field it leaves out comes from there.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    base: Option<String>,
    kind: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    args: Option<Vec<String>>,
    description: Option<String>,
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    /// The profile of the routing agent that picks each issue's coordinator.
    pub agent: String,
    /// The coordinator profiles the routing agent may pick from.
    pub coordinators: Vec<String>,
    /// The coordinator when the routing agent gives no valid answer.
    pub default: String,
    #[serde(default = "default_routing_timeout")]
    pub timeout_seconds: u64,
    /// The profiles a coordinator may start workers with.
    pub workers: Vec<String>,
}

fn default_routing_timeout() -> u64 {
    120
}

impl Config {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    pub fn load(config_dir: &Path) -> Result<Config> {
        let path = Self::path(config_dir);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let profiles = load_profiles(&config_dir.join(PROFILES_DIR))?;
        Self::parse(&text, profiles).with_context(|| format!("{} is not valid", path.display()))
    }

    /// A SHA-256 over the files `load` reads, by sorted path and content:
    /// `config.toml` and each profile folder's `config.toml` and
    /// `instructions.md`. Rewriting a file with the same content keeps it.
    pub fn fingerprint(config_dir: &Path) -> Result<[u8; 32]> {
        let path = Self::path(config_dir);
        let mut files = vec![path];
        if let Ok(entries) = std::fs::read_dir(config_dir.join(PROFILES_DIR)) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                files.push(entry.path().join(FILE_NAME));
                files.push(entry.path().join(INSTRUCTIONS_FILE));
            }
        }
        files.sort();
        let mut hash = sha2::Sha256::new();
        for file in files {
            match std::fs::read(&file) {
                Ok(bytes) => {
                    hash.update(file.as_os_str().as_encoded_bytes());
                    hash.update([0]);
                    hash.update((bytes.len() as u64).to_le_bytes());
                    hash.update(&bytes);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && file != Self::path(config_dir) => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("could not read {}", file.display()));
                }
            }
        }
        Ok(hash.finalize().into())
    }

    fn parse(text: &str, profiles: BTreeMap<String, Profile>) -> Result<Config> {
        let table: toml::Table = toml::from_str(text)?;
        ensure!(
            !table.contains_key("profiles"),
            "profiles are not set here: put each one in {PROFILES_DIR}/<name>/{FILE_NAME} next to this file"
        );
        let mut config: Config = table.try_into()?;
        config.profiles = profiles;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.workspaces.is_empty(), "no workspace is configured");
        for (name, workspace) in &self.workspaces {
            ensure!(
                crate::run::valid_workspace(name),
                "workspace name `{name}` may use only lower-case letters, digits and `-`, start with a letter, and have at most 16 characters"
            );
            ensure!(
                !workspace.client_id.trim().is_empty(),
                "workspaces.{name}.client_id is empty"
            );
            ensure!(
                workspace.callback_port != 0,
                "workspaces.{name}.callback_port may not be 0"
            );
            ensure!(
                !workspace.teams.is_empty(),
                "workspaces.{name}.teams lists no team"
            );
            for (key, team) in &workspace.teams {
                ensure!(
                    crate::run::valid_team_key(key),
                    "workspaces.{name}.teams.{key}: `{key}` is not a Linear team key"
                );
                ensure!(
                    !team.review_state.trim().is_empty(),
                    "workspaces.{name}.teams.{key}.review_state is empty"
                );
            }
            for (key, seconds) in [
                ("intake_interval_seconds", workspace.intake_interval_seconds),
                (
                    "run_read_interval_seconds",
                    workspace.run_read_interval_seconds,
                ),
            ] {
                ensure!(
                    (1..=MAX_LINEAR_INTERVAL).contains(&seconds),
                    "workspaces.{name}.{key} must be between 1 and {MAX_LINEAR_INTERVAL}"
                );
            }
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
                "profile `{name}`: kind `{}` is not a Herdr agent kind",
                profile.kind
            );
            if profile.effort.is_some() && !agents::has_effort_flag(&profile.kind) {
                bail!(
                    "profile `{name}`: the `{}` CLI has no effort flag; put the effort in the model ID instead",
                    profile.kind
                );
            }
        }

        let routing = &self.routing;
        for (name, profile) in &self.profiles {
            for key in profile.env.keys() {
                ensure!(
                    !key.is_empty() && !key.contains('='),
                    "profile `{name}`: env has an invalid variable name `{key}`"
                );
            }
            let used = routing.coordinators.contains(name) || routing.workers.contains(name);
            ensure!(
                profile.env.is_empty() || !used,
                "profile `{name}`: env applies only to the routing agent, but `{name}` is also a coordinator or worker profile"
            );
        }
        let agent = self.profile(&routing.agent).context("routing.agent")?;
        ensure!(
            routing::registered(&agent.kind),
            "routing.agent: the `{}` kind cannot be a routing agent",
            agent.kind
        );
        ensure!(
            !routing.coordinators.is_empty(),
            "routing.coordinators lists no profile"
        );
        for name in &routing.coordinators {
            self.profile(name).context("routing.coordinators")?;
        }
        self.profile(&routing.default).context("routing.default")?;
        ensure!(
            routing.timeout_seconds >= 1,
            "routing.timeout_seconds must be at least 1"
        );
        ensure!(
            !routing.workers.is_empty(),
            "routing.workers lists no profile"
        );
        for name in &routing.workers {
            self.profile(name).context("routing.workers")?;
        }
        Ok(())
    }

    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.profiles.get(name).with_context(|| {
            format!("no profile named `{name}`: add {PROFILES_DIR}/{name}/{FILE_NAME}")
        })
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

    pub fn workspace(&self, name: &str) -> Result<&Workspace> {
        self.workspaces
            .get(name)
            .with_context(|| format!("no workspace named `{name}` in the config"))
    }

    /// The team a run belongs to: its workspace's entry for the team key.
    pub fn team(&self, workspace: &str, team_key: &str) -> Result<&Team> {
        self.workspace(workspace)?
            .teams
            .get(team_key)
            .with_context(|| {
                format!("team `{team_key}` is not configured in workspace `{workspace}`")
            })
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

/// Reads every profile folder under `dir`: the folder's name is the profile's
/// name. Entries starting with `.` are skipped, symbolic links are followed,
/// and files other than the two a profile has are left alone. A missing `dir`
/// gives no profiles. Each profile is then resolved against its base chain.
fn load_profiles(dir: &Path) -> Result<BTreeMap<String, Profile>> {
    let mut files = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", dir.display()));
        }
    };
    for entry in entries {
        let path = entry
            .with_context(|| format!("could not read {}", dir.display()))?
            .path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name.starts_with('.') {
            continue;
        }
        ensure!(
            path.is_dir(),
            "{} is not a folder: each profile is a folder with a {FILE_NAME}",
            path.display()
        );
        files.insert(name, read_profile(&path)?);
    }
    files
        .keys()
        .map(|name| Ok((name.clone(), resolve(name, &files)?)))
        .collect()
}

/// A profile folder's `config.toml` and its `instructions.md`, `None` when
/// that is missing or blank.
fn read_profile(folder: &Path) -> Result<(ProfileFile, Option<String>)> {
    let path = folder.join(FILE_NAME);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let file: ProfileFile =
        toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
    let path = folder.join(INSTRUCTIONS_FILE);
    let instructions = match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => None,
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    Ok((file, instructions))
}

/// Lays `name`'s base chain from its root to `name`: a later layer replaces
/// `model`, `effort`, `args` and `env` whole, may not change `kind`, and
/// adds its instructions; `description` is `name`'s own.
fn resolve(name: &str, files: &BTreeMap<String, (ProfileFile, Option<String>)>) -> Result<Profile> {
    let mut chain = vec![name];
    while let Some(base) = files[*chain.last().unwrap()].0.base.as_deref() {
        if chain.contains(&base) {
            chain.push(base);
            bail!(
                "profile `{name}`: its base chain loops: {}",
                chain.join(" \u{2192} ")
            );
        }
        ensure!(
            files.contains_key(base),
            "profile `{}`: base `{base}` is not a profile",
            chain.last().unwrap()
        );
        chain.push(base);
    }
    let mut kind: Option<(&str, &str)> = None;
    let (mut model, mut effort, mut args, mut env) = (None, None, Vec::new(), BTreeMap::new());
    let mut instructions = Vec::new();
    for layer in chain.iter().rev() {
        let (file, text) = &files[*layer];
        if let Some(own) = file.kind.as_deref() {
            match kind {
                Some((inherited, from)) if inherited != own => bail!(
                    "profile `{layer}`: kind `{own}` differs from its base `{from}` (`{inherited}`)"
                ),
                Some(_) => {}
                None => kind = Some((own, layer)),
            }
        }
        model = file.model.clone().or(model);
        effort = file.effort.clone().or(effort);
        args = file.args.clone().unwrap_or(args);
        env = file.env.clone().unwrap_or(env);
        if let Some(text) = text {
            instructions.push(Instructions {
                profile: layer.to_string(),
                text: text.clone(),
            });
        }
    }
    let Some((kind, _)) = kind else {
        bail!("profile `{name}`: kind is set neither here nor in a base");
    };
    Ok(Profile {
        kind: kind.to_string(),
        model,
        effort,
        args,
        description: files[name].0.description.clone().unwrap_or_default(),
        instructions,
        env,
    })
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

    /// A config with every section, used across the test suite. Its profiles
    /// are [`SAMPLE_PROFILES`].
    pub const SAMPLE: &str = r#"
[workspaces.acme]
client_id = "client-123"

[workspaces.acme.teams.DATA]
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

[routing]
agent = "router"
coordinators = ["coordinator", "coordinator-light"]
default = "coordinator"
timeout_seconds = 60
workers = ["standard", "deep"]
"#;

    /// The sample's profile folders: name, `config.toml`, `instructions.md`.
    pub const SAMPLE_PROFILES: [(&str, &str, Option<&str>); 5] = [
        (
            "coordinator",
            "kind = \"claude\"\nmodel = \"opus\"\neffort = \"high\"\nargs = [\"--permission-mode\", \"auto\"]\ndescription = \"default coordinator\"\n",
            None,
        ),
        (
            "coordinator-light",
            "kind = \"claude\"\nmodel = \"sonnet\"\ndescription = \"small issues\"\n",
            Some("Prefer one worker. Ask before you split the work.\n"),
        ),
        ("router", "kind = \"claude\"\nmodel = \"haiku\"\n", None),
        (
            "standard",
            "kind = \"claude\"\nmodel = \"sonnet\"\neffort = \"high\"\nargs = [\"--permission-mode\", \"auto\"]\ndescription = \"scoped features and fixes\"\n",
            None,
        ),
        (
            "deep",
            "kind = \"codex\"\nmodel = \"gpt-6-sol\"\neffort = \"xhigh\"\nargs = [\"-s\", \"workspace-write\"]\ndescription = \"cross-module changes\"\n",
            None,
        ),
    ];

    /// Writes `config.toml` with `text` and the sample's profile folders.
    pub fn write_sample(config_dir: &Path, text: &str) {
        std::fs::create_dir_all(config_dir).unwrap();
        std::fs::write(config_dir.join(FILE_NAME), text).unwrap();
        for (name, config, instructions) in SAMPLE_PROFILES {
            let folder = config_dir.join(PROFILES_DIR).join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(FILE_NAME), config).unwrap();
            if let Some(text) = instructions {
                std::fs::write(folder.join(INSTRUCTIONS_FILE), text).unwrap();
            }
        }
    }

    /// The sample config as the ticker loads it.
    pub fn sample() -> Config {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        Config::load(dir.path()).unwrap()
    }

    /// Loads the sample with `text` as `config.toml` and `profiles` replacing
    /// or adding profile folders' `config.toml`. The error is the full chain.
    fn load_with(text: &str, profiles: &[(&str, &str)]) -> Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), text);
        for (name, config) in profiles {
            let folder = dir.path().join(PROFILES_DIR).join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(FILE_NAME), config).unwrap();
        }
        Config::load(dir.path()).map_err(|e| format!("{e:#}"))
    }

    #[test]
    fn the_sample_parses_with_defaults() {
        let config = sample();
        let acme = config.workspace("acme").unwrap();
        assert_eq!(acme.callback_port, DEFAULT_CALLBACK_PORT);
        assert_eq!(
            config.team("acme", "DATA").unwrap().review_state,
            "In Review"
        );
        assert!(
            config
                .team("acme", "WEB")
                .unwrap_err()
                .to_string()
                .contains("team `WEB` is not configured in workspace `acme`")
        );
        assert_eq!(config.limits, Limits::default());
        assert!(config.notifications.herdr);
        assert_eq!(
            config.routing.coordinators,
            ["coordinator", "coordinator-light"]
        );
        assert_eq!(
            config.profiles.keys().collect::<Vec<_>>(),
            [
                "coordinator",
                "coordinator-light",
                "deep",
                "router",
                "standard"
            ]
        );
        assert_eq!(
            config.profile("coordinator-light").unwrap().instructions,
            [Instructions {
                profile: "coordinator-light".into(),
                text: "Prefer one worker. Ask before you split the work.\n".into(),
            }]
        );
        assert_eq!(config.profile("coordinator").unwrap().instructions, []);
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
            load_with(&text, &[]).unwrap_err()
        };
        bad(
            "[workspaces.acme.teams.DATA]",
            "[workspaces.acme.teams.data]",
        );
        bad("client_id = \"client-123\"", "client_id = \" \"");
        bad("[workspaces.acme]", "[workspaces.Acme]");
        bad(
            "[workspaces.acme.teams.DATA]",
            "[workspaces.acme.teams.DATA]\nreview_state = \"\"",
        );
        bad("default = \"coordinator\"", "default = \"missing\"");
        bad(
            "workers = [\"standard\", \"deep\"]",
            "workers = [\"missing\"]",
        );
        bad("path = \"/src/api\"", "path = \"src/api\"");
        bad("agent = \"router\"", "agent = \"deep-x\"");
        bad(
            "coordinators = [\"coordinator\", \"coordinator-light\"]",
            "coordinators = [\"coordinator\", \"missing\"]",
        );
        bad(
            "coordinators = [\"coordinator\", \"coordinator-light\"]",
            "coordinators = []",
        );
        bad("timeout_seconds = 60", "timeout_seconds = 0");
        // The size-based routing is gone.
        bad(
            "default = \"coordinator\"",
            "default = \"coordinator\"\nsize_label_group = \"size\"",
        );
        bad("[herdr]", "[herdr]\nunknown = 1");
        assert!(
            load_with(SAMPLE, &[("deep", "kind = \"chatgpt\"\n")])
                .unwrap_err()
                .ends_with("profile `deep`: kind `chatgpt` is not a Herdr agent kind")
        );
        // Effort only for kinds with an effort flag.
        assert!(
            load_with(
                SAMPLE,
                &[("router", "kind = \"cursor\"\neffort = \"high\"\n")]
            )
            .unwrap_err()
            .ends_with("profile `router`: the `cursor` CLI has no effort flag; put the effort in the model ID instead")
        );
    }

    #[test]
    fn profiles_come_from_their_folders() {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        let profiles = dir.path().join(PROFILES_DIR);
        std::fs::write(profiles.join(".DS_Store"), "").unwrap();
        std::fs::write(profiles.join("router").join("README.md"), "Shared router.").unwrap();
        std::fs::write(profiles.join("standard").join(INSTRUCTIONS_FILE), " \n").unwrap();
        let shared = tempfile::tempdir().unwrap();
        std::fs::write(shared.path().join(FILE_NAME), "kind = \"codex\"\n").unwrap();
        std::fs::write(shared.path().join(INSTRUCTIONS_FILE), "Run `make check`.\n").unwrap();
        std::os::unix::fs::symlink(shared.path(), profiles.join("shared")).unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert_eq!(
            config.profiles.keys().collect::<Vec<_>>(),
            [
                "coordinator",
                "coordinator-light",
                "deep",
                "router",
                "shared",
                "standard"
            ]
        );
        assert_eq!(config.profile("standard").unwrap().instructions, []);
        let shared = config.profile("shared").unwrap();
        assert_eq!(shared.kind, "codex");
        assert_eq!(shared.instructions[0].text, "Run `make check`.\n");
    }

    #[test]
    fn a_profile_inherits_from_its_base_chain() {
        let config = load_with(
            SAMPLE,
            &[
                (
                    "root",
                    "kind = \"claude\"\nmodel = \"opus\"\neffort = \"high\"\nargs = [\"--permission-mode\", \"auto\"]\ndescription = \"the root\"\nenv = { A = \"1\" }\n",
                ),
                ("mid", "base = \"root\"\nmodel = \"sonnet\"\nargs = [\"--x\"]\n"),
                ("leaf", "base = \"mid\"\neffort = \"low\"\ndescription = \"the leaf\"\n"),
            ],
        )
        .unwrap();
        let leaf = config.profile("leaf").unwrap();
        assert_eq!(
            (
                leaf.kind.as_str(),
                leaf.model.as_deref(),
                leaf.effort.as_deref(),
                leaf.args.as_slice(),
                leaf.description.as_str(),
            ),
            (
                "claude",
                Some("sonnet"),
                Some("low"),
                &["--x".to_string()][..],
                "the leaf"
            )
        );
        assert_eq!(
            leaf.env,
            BTreeMap::from([("A".to_string(), "1".to_string())])
        );
        let mid = config.profile("mid").unwrap();
        assert_eq!(
            (mid.effort.as_deref(), mid.description.as_str()),
            (Some("high"), ""),
            "the description is not inherited"
        );
    }

    #[test]
    fn a_base_chain_must_end_at_a_profile_keep_its_kind_and_not_loop() {
        let error = |profiles: &[(&str, &str)]| load_with(SAMPLE, profiles).unwrap_err();
        assert!(
            error(&[
                ("b", "kind = \"claude\"\n"),
                ("a", "base = \"b\"\nkind = \"codex\"\n")
            ])
            .ends_with("profile `a`: kind `codex` differs from its base `b` (`claude`)")
        );
        assert!(
            error(&[("a", "base = \"a\"\nkind = \"claude\"\n")])
                .ends_with("profile `a`: its base chain loops: a \u{2192} a")
        );
        assert!(
            error(&[
                ("a", "base = \"b\"\nkind = \"claude\"\n"),
                ("b", "base = \"a\"\n")
            ])
            .ends_with("profile `a`: its base chain loops: a \u{2192} b \u{2192} a")
        );
        assert!(
            error(&[("a", "base = \"x\"\nkind = \"claude\"\n")])
                .ends_with("profile `a`: base `x` is not a profile")
        );
        assert!(
            error(&[("a", "base = \"b\"\n"), ("b", "model = \"opus\"\n")])
                .ends_with("profile `a`: kind is set neither here nor in a base")
        );
    }

    #[test]
    fn env_inherited_by_a_worker_profile_is_refused() {
        let error = load_with(
            SAMPLE,
            &[
                ("with-env", "kind = \"claude\"\nenv = { X = \"1\" }\n"),
                ("standard", "base = \"with-env\"\n"),
            ],
        )
        .unwrap_err();
        assert!(error.ends_with(
            "profile `standard`: env applies only to the routing agent, but `standard` is also a coordinator or worker profile"
        ), "{error}");
    }

    #[test]
    fn instructions_are_the_layers_that_have_them_from_the_root() {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        let profiles = dir.path().join(PROFILES_DIR);
        for (name, config, instructions) in [
            ("root", "kind = \"claude\"\n", Some("Root rule.\n")),
            ("mid", "base = \"root\"\n", None),
            ("leaf", "base = \"mid\"\n", Some("Leaf rule.\n")),
        ] {
            std::fs::create_dir_all(profiles.join(name)).unwrap();
            std::fs::write(profiles.join(name).join(FILE_NAME), config).unwrap();
            if let Some(text) = instructions {
                std::fs::write(profiles.join(name).join(INSTRUCTIONS_FILE), text).unwrap();
            }
        }
        let config = Config::load(dir.path()).unwrap();
        let layer = |profile: &str, text: &str| Instructions {
            profile: profile.into(),
            text: text.into(),
        };
        assert_eq!(
            config.profile("leaf").unwrap().instructions,
            [layer("root", "Root rule.\n"), layer("leaf", "Leaf rule.\n")]
        );
        assert_eq!(
            config.profile("mid").unwrap().instructions,
            [layer("root", "Root rule.\n")]
        );
    }

    #[test]
    fn the_fingerprint_follows_the_content_of_the_files_load_reads() {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        let first = Config::fingerprint(dir.path()).unwrap();
        write_sample(dir.path(), SAMPLE);
        assert_eq!(
            Config::fingerprint(dir.path()).unwrap(),
            first,
            "rewritten with the same content"
        );
        let instructions = dir
            .path()
            .join(PROFILES_DIR)
            .join("deep")
            .join(INSTRUCTIONS_FILE);
        std::fs::write(&instructions, "Run the slow tests too.\n").unwrap();
        let with_instructions = Config::fingerprint(dir.path()).unwrap();
        assert_ne!(with_instructions, first, "a new instructions.md");
        std::fs::write(&instructions, "Run the fast tests only.\n").unwrap();
        assert_ne!(Config::fingerprint(dir.path()).unwrap(), with_instructions);
        std::fs::write(dir.path().join(PROFILES_DIR).join(".notes"), "x").unwrap();
        std::fs::write(
            dir.path().join(PROFILES_DIR).join("deep").join("README.md"),
            "x",
        )
        .unwrap();
        std::fs::remove_file(&instructions).unwrap();
        assert_eq!(
            Config::fingerprint(dir.path()).unwrap(),
            first,
            "files load does not read leave it alone"
        );
    }

    #[test]
    fn profiles_live_only_in_their_folders() {
        let in_config = SAMPLE.to_string() + "\n[profiles.extra]\nkind = \"claude\"\n";
        assert!(load_with(&in_config, &[]).unwrap_err().ends_with(
            "profiles are not set here: put each one in profiles/<name>/config.toml next to this file"
        ));
        let instructions = load_with(
            SAMPLE,
            &[("router", "kind = \"claude\"\ninstructions = \"x\"\n")],
        )
        .unwrap_err();
        assert!(
            instructions.contains("profiles/router/config.toml is not valid"),
            "{instructions}"
        );
        assert!(
            instructions.contains("unknown field `instructions`"),
            "{instructions}"
        );

        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        let profiles = dir.path().join(PROFILES_DIR);
        std::fs::write(profiles.join("extra.toml"), "kind = \"claude\"\n").unwrap();
        assert!(
            format!("{:#}", Config::load(dir.path()).unwrap_err()).ends_with(
                "extra.toml is not a folder: each profile is a folder with a config.toml"
            )
        );
        std::fs::remove_file(profiles.join("extra.toml")).unwrap();
        std::fs::create_dir(profiles.join("empty")).unwrap();
        assert!(format!("{:#}", Config::load(dir.path()).unwrap_err()).contains("could not read "));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), SAMPLE).unwrap();
        assert!(
            format!("{:#}", Config::load(dir.path()).unwrap_err()).ends_with(
                "routing.agent: no profile named `router`: add profiles/router/config.toml"
            )
        );
    }

    #[test]
    fn linear_intervals_default_to_five_seconds_and_are_checked() {
        let intervals = |config: &Config| {
            let acme = config.workspace("acme").unwrap();
            (acme.intake_interval_seconds, acme.run_read_interval_seconds)
        };
        assert_eq!(intervals(&sample()), (5, 5));
        let set = |lines: &str| {
            load_with(
                &SAMPLE.replacen(
                    "client_id = \"client-123\"",
                    &format!("client_id = \"client-123\"\n{lines}"),
                    1,
                ),
                &[],
            )
        };
        let config = set("intake_interval_seconds = 30\nrun_read_interval_seconds = 10").unwrap();
        assert_eq!(intervals(&config), (30, 10));
        assert!(
            set("intake_interval_seconds = 0")
                .unwrap_err()
                .ends_with("workspaces.acme.intake_interval_seconds must be between 1 and 3600")
        );
        assert!(
            set("run_read_interval_seconds = 3601")
                .unwrap_err()
                .ends_with("workspaces.acme.run_read_interval_seconds must be between 1 and 3600")
        );
    }

    #[test]
    fn workspaces_and_their_teams_have_their_own_settings() {
        let text = SAMPLE.to_string()
            + "\n[workspaces.beta]\nclient_id = \"client-456\"\ncallback_port = 43872\n\n[workspaces.beta.teams.DATA]\nallowed_user_ids = [\"user-2\"]\nreview_state = \"Review\"\n\n[workspaces.beta.teams.OPS]\n";
        let config = load_with(&text, &[]).unwrap();
        assert_eq!(
            config.workspaces.keys().collect::<Vec<_>>(),
            ["acme", "beta"]
        );
        assert_eq!(config.workspace("beta").unwrap().callback_port, 43872);
        let beta = config.team("beta", "DATA").unwrap();
        assert_eq!(
            (beta.allowed_user_ids.as_slice(), beta.review_state.as_str()),
            (&["user-2".to_string()][..], "Review")
        );
        assert_eq!(
            config.team("acme", "DATA").unwrap().allowed_user_ids,
            ["user-1"]
        );
        let ops = config.team("beta", "OPS").unwrap();
        assert!(ops.allowed_user_ids.is_empty());
        assert_eq!(ops.review_state, "In Review");

        let no_teams =
            SAMPLE.to_string() + "\n[workspaces.beta]\nclient_id = \"client-456\"\nteams = {}\n";
        assert!(
            load_with(&no_teams, &[])
                .unwrap_err()
                .ends_with("workspaces.beta.teams lists no team")
        );
        let none = SAMPLE
            .replace(
                "[workspaces.acme.teams.DATA]\nallowed_user_ids = [\"user-1\"]\n",
                "",
            )
            .replace("[workspaces.acme]\nclient_id = \"client-123\"\n", "");
        assert!(
            load_with(&none, &[])
                .unwrap_err()
                .contains("missing field `workspaces`")
        );
    }

    #[test]
    fn only_the_routing_agents_profile_may_set_env() {
        let with_env = |profile: &str| {
            let (_, config, _) = SAMPLE_PROFILES
                .iter()
                .find(|(n, _, _)| *n == profile)
                .unwrap();
            load_with(
                SAMPLE,
                &[(
                    profile,
                    &format!("{config}env = {{ OPENCODE_CONFIG_DIR = \"/tmp/empty\" }}\n"),
                )],
            )
        };
        let config = with_env("router").unwrap();
        assert_eq!(
            config
                .profile("router")
                .unwrap()
                .env
                .get("OPENCODE_CONFIG_DIR")
                .map(String::as_str),
            Some("/tmp/empty")
        );
        assert!(with_env("standard").unwrap_err().ends_with(
            "profile `standard`: env applies only to the routing agent, but `standard` is also a coordinator or worker profile"
        ));
    }

    #[test]
    fn a_routing_agent_must_be_a_registered_kind() {
        assert!(
            load_with(SAMPLE, &[("router", "kind = \"gemini\"\n")])
                .unwrap_err()
                .ends_with("routing.agent: the `gemini` kind cannot be a routing agent")
        );
    }
}
