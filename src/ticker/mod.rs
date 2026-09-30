//! The ticker: one background process per state folder. It follows the
//! configured Herdr session, reads and writes Linear, and reconciles every
//! run. `ticker start` (the startup hook, and every agent-facing command)
//! keeps one of this version running; `ticker run` is the process itself.

mod intake;
mod launch;
pub mod reconcile;
#[cfg(test)]
mod scenarios;
mod watching;
#[cfg(test)]
mod world;

use std::fs::{File, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc, watch};

use crate::VERSION;
use crate::config::Config;
use crate::herdr::{self, Link};
use crate::linear::task::{Levels, LinearTask, Links};
use crate::paths::Ctx;

/// The log stays below this size; trimming keeps its newer half.
pub const LOG_CAP: u64 = 1_000_000;
/// The ticker exits once the configured session was unreachable this long.
pub const UNREACHABLE_FOR: SignedDuration = SignedDuration::from_secs(5 * 60);
const STOP_WAIT: Duration = Duration::from_secs(30);
const SUPERVISE_EVERY: Duration = Duration::from_secs(1);
const SOCKET_RETRY: Duration = Duration::from_secs(5);
const EVENT_QUEUE: usize = 256;
/// How often the reload request and the config files are checked.
const CONFIG_POLL: Duration = Duration::from_secs(1);

/// A config whose files changed and that loads, with their fingerprint.
type Reloaded = (Box<Config>, [u8; 32]);

/// Resolves when `serve` should start again with a new config.
type Reload<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Reloaded> + 'a>>;

/// Why `serve` returned.
enum Served {
    Stopped(String),
    Reload(Reloaded),
}

pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.lock")
}

pub fn stop_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.stop")
}

/// Asks the running ticker to load the config again; it removes the file.
pub fn reload_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.reload")
}

pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.log")
}

/// The datagram socket the ticker listens on for pokes.
pub fn poke_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.sock")
}

/// Wakes a running ticker after a subcommand wrote run files it acts on.
/// No ticker running is normal, so every error is ignored.
pub fn poke(state_dir: &Path) {
    if let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() {
        let _ = socket.set_nonblocking(true);
        let _ = socket.send_to(b"!", poke_path(state_dir));
    }
}

/// Listens for pokes and turns each into a wake of the reconciler. Without
/// the socket (a path too long for one, say) the ticker still runs on its
/// other wakes.
fn listen(state_dir: &Path, log: &Log) -> Arc<Notify> {
    let poked = Arc::new(Notify::new());
    let path = poke_path(state_dir);
    let _ = std::fs::remove_file(&path);
    match tokio::net::UnixDatagram::bind(&path) {
        Ok(socket) => {
            let notify = poked.clone();
            tokio::spawn(async move {
                let mut byte = [0u8; 8];
                while socket.recv(&mut byte).await.is_ok() {
                    notify.notify_one();
                }
            });
        }
        Err(error) => log.line(&format!(
            "cannot listen on {}: {error}; subcommands do not wake the ticker",
            path.display()
        )),
    }
    poked
}

/// What the lock holder writes into the lock file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Info {
    pub version: String,
    pub pid: u32,
    pub started: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
    Free,
    Held(Info),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAction {
    Nothing,
    Spawn,
    StopThenSpawn,
}

/// A ticker of another version, or one that is being stopped, is replaced.
pub fn decide_start(lock: &LockState, version: &str, stopping: bool) -> StartAction {
    match lock {
        LockState::Free => StartAction::Spawn,
        LockState::Held(info) if info.version == version && !stopping => StartAction::Nothing,
        LockState::Held(_) => StartAction::StopThenSpawn,
    }
}

fn read_info(file: &mut File) -> Info {
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    serde_json::from_str(&text).unwrap_or_default()
}

/// Probes the lock without taking it for longer than the probe.
pub fn lock_state(state_dir: &Path) -> LockState {
    let Ok(mut file) = File::open(lock_path(state_dir)) else {
        return LockState::Free;
    };
    match file.try_lock_shared() {
        Ok(()) => LockState::Free,
        Err(TryLockError::WouldBlock) => LockState::Held(read_info(&mut file)),
        Err(TryLockError::Error(_)) => LockState::Free,
    }
}

pub fn describe(state_dir: &Path) -> String {
    match lock_state(state_dir) {
        LockState::Free => "ticker not running".into(),
        LockState::Held(info) => format!(
            "ticker {} running since {} (pid {})",
            info.version, info.started, info.pid
        ),
    }
}

/// Starts the ticker detached unless one of this version runs. Without a
/// config there is nothing to run and nothing is created.
pub async fn start(ctx: &Ctx<'_>) -> Result<()> {
    if !Config::path(&ctx.config_dir()).exists() || !ctx.detached_ticker {
        return Ok(());
    }
    let state_dir = ctx.ensure_state_dir()?;
    let stopping = stop_path(&state_dir).exists();
    match decide_start(&lock_state(&state_dir), VERSION, stopping) {
        StartAction::Nothing => Ok(()),
        StartAction::Spawn => spawn(),
        StartAction::StopThenSpawn => {
            stop(&state_dir).await?;
            spawn()
        }
    }
}

/// `<binary> ticker run` in a new session with null stdio. It is not waited
/// for: Herdr runs at most 32 plugin commands at a time, so the hook that
/// starts it must return at once.
fn spawn() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new(crate::paths::binary()?);
    command
        .args(["ticker", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the
    // parent, as `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("could not start the ticker")?;
    Ok(())
}

/// Asks the running ticker to exit and waits until the lock is free. With a
/// free lock, a stale stop file is removed.
pub async fn stop(state_dir: &Path) -> Result<()> {
    let stop = stop_path(state_dir);
    if lock_state(state_dir) != LockState::Free {
        std::fs::write(&stop, b"")
            .with_context(|| format!("could not write {}", stop.display()))?;
        let deadline = tokio::time::Instant::now() + STOP_WAIT;
        // A child forked at the same instant may share the descriptor for a
        // moment, so the lock is polled.
        while lock_state(state_dir) != LockState::Free {
            if tokio::time::Instant::now() >= deadline {
                bail!("the ticker did not stop within {} s", STOP_WAIT.as_secs());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let _ = std::fs::remove_file(&stop);
    Ok(())
}

/// Takes the ticker lock and writes this process's info into it. A lock
/// held by another process is retried for a moment, since a probe holds it
/// briefly.
pub(crate) async fn acquire(state_dir: &Path) -> Result<File> {
    let path = lock_path(state_dir);
    let mut file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    for _ in 0..20 {
        match file.try_lock() {
            Ok(()) => {
                let info = Info {
                    version: VERSION.into(),
                    pid: std::process::id(),
                    started: Timestamp::now().to_string(),
                };
                file.set_len(0)?;
                file.rewind()?;
                file.write_all(serde_json::to_string(&info)?.as_bytes())?;
                return Ok(file);
            }
            Err(TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    bail!("another ticker runs for {}", state_dir.display())
}

/// The ticker's log: one line per event, capped at [`LOG_CAP`].
pub struct Log {
    pub path: PathBuf,
    write: Mutex<()>,
}

impl Log {
    pub fn new(path: PathBuf) -> Log {
        Log {
            path,
            write: Mutex::new(()),
        }
    }

    pub fn line(&self, text: &str) {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let text: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(LOG_CAP as usize / 8)
            .collect();
        let line = format!("{} {text}\n", Timestamp::now());
        let size = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if size + line.len() as u64 > LOG_CAP {
            self.trim();
        }
        if let Ok(mut file) = File::options().create(true).append(true).open(&self.path) {
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Drops the older half of the file at a line boundary.
    fn trim(&self) {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return;
        };
        let half = bytes.len() / 2;
        let start = bytes[half..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| half + i + 1);
        let _ = crate::files::write_atomic(&self.path, &bytes[start..]);
    }
}

/// Whether the session has been down long enough to give up.
pub fn unreachable(link: &Link, now: Timestamp) -> bool {
    !link.connected && now.duration_since(link.since) >= UNREACHABLE_FOR
}

/// `ticker run`: holds the lock until asked to stop or until the configured
/// session has been unreachable for five minutes. A changed config that
/// loads starts the tasks again with it, the lock still held.
pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let config_dir = ctx.config_dir();
    let config = Config::load(&config_dir)?;
    let fingerprint = Config::fingerprint(&config_dir)?;
    let state_dir = ctx.ensure_state_dir()?;
    let _lock = acquire(&state_dir).await?;
    let _ = std::fs::remove_file(stop_path(&state_dir));
    let log = Arc::new(Log::new(log_path(&state_dir)));
    log.line(&format!(
        "ticker {VERSION} started (pid {})",
        std::process::id()
    ));
    let _ = std::fs::remove_file(reload_path(&state_dir));
    let herdr_bin = ctx.env.herdr_bin();
    let notify = |config: &Config, body: String| {
        if !config.notifications.herdr {
            return;
        }
        let (bin, session) = (herdr_bin.clone(), config.herdr.session.clone());
        tokio::spawn(async move {
            use herdr::Herdr as _;
            if let Ok(socket) = herdr::session_socket(&bin, session.as_deref()).await {
                let _ = herdr::Client::new(socket)
                    .notification_show("herdr-linear-agent", &body)
                    .await;
            }
        });
    };
    let served = keep_serving(
        &config_dir,
        &state_dir,
        (Box::new(config), fingerprint),
        &log,
        &notify,
        CONFIG_POLL,
        async |config, reload| serve(ctx, config, &state_dir, &log, reload).await,
    )
    .await;
    match served {
        Ok(reason) => {
            log.line(&format!("ticker stopped: {reason}"));
            Ok(())
        }
        Err(error) => {
            log.line(&format!("ticker failed: {error:#}"));
            Err(error)
        }
    }
}

/// Calls `serve` with the config in use and a watch on its files, again with
/// each config a reload request brings, until `serve` stops for another
/// reason.
async fn keep_serving(
    config_dir: &Path,
    state_dir: &Path,
    (mut config, mut fingerprint): Reloaded,
    log: &Log,
    notify: &dyn Fn(&Config, String),
    poll: Duration,
    mut serve: impl AsyncFnMut(&Config, Reload<'_>) -> Result<Served>,
) -> Result<String> {
    loop {
        let reload = Box::pin(watch_config(
            config_dir,
            state_dir,
            (&config, fingerprint),
            log,
            notify,
            poll,
        ));
        match serve(&config, reload).await? {
            Served::Stopped(reason) => return Ok(reason),
            Served::Reload((new, new_fingerprint)) => {
                log.line("config reloaded; restarting the ticker's tasks");
                (config, fingerprint) = (new, new_fingerprint);
            }
        }
    }
}

/// Resolves with the config once a reload is requested (`reload_path`) and it
/// loads; one that does not load is logged and shown, and the one in use
/// stays. Files that differ from `fingerprint` are only told about, once per
/// fingerprint: the change applies when a reload is requested.
async fn watch_config(
    config_dir: &Path,
    state_dir: &Path,
    (in_use, fingerprint): (&Config, [u8; 32]),
    log: &Log,
    notify: &dyn Fn(&Config, String),
    poll: Duration,
) -> Reloaded {
    let mut told = fingerprint;
    loop {
        tokio::time::sleep(poll).await;
        if std::fs::remove_file(reload_path(state_dir)).is_ok() {
            match (Config::load(config_dir), Config::fingerprint(config_dir)) {
                (Ok(config), Ok(now)) => return (Box::new(config), now),
                (Err(error), _) | (_, Err(error)) => {
                    log.line(&format!("config reload failed: {error:#}"));
                    notify(in_use, format!("The config was not reloaded: {error:#}"));
                }
            }
            continue;
        }
        let Ok(now) = Config::fingerprint(config_dir) else {
            continue;
        };
        if now == told {
            continue;
        }
        told = now;
        let body = match Config::load(config_dir) {
            Ok(_) if now == fingerprint => continue,
            Ok(_) => {
                log.line("config changed; run the reload action to apply it");
                "The config changed. Run herdr-linear-agent: reload the config to apply it."
                    .to_string()
            }
            Err(error) => {
                log.line(&format!("config changed but does not load: {error:#}"));
                format!("The config changed but does not load: {error:#}")
            }
        };
        notify(in_use, body);
    }
}

/// Runs the Linear task and the reconciler at once. Until the configured
/// session's socket is found the Herdr requests are `NotSent`, so the
/// reconciler runs Linear-only passes; the link counts as down since the
/// start, so the 5-minute unreachable exit still applies. `reload` ends it
/// like a stop, so the caller can start it again.
async fn serve(
    ctx: &Ctx<'_>,
    config: &Config,
    state_dir: &Path,
    log: &Arc<Log>,
    reload: Reload<'_>,
) -> Result<Served> {
    let herdr = herdr::Client::default();
    let (link_tx, link) = watch::channel(Link {
        connected: false,
        since: Timestamp::now(),
        wakes: 0,
        last_error: Some("the configured Herdr session is not found yet".into()),
    });
    let (socket_tx, socket) = watch::channel(String::new());
    let finder = tokio::spawn(connect(
        ctx.env.herdr_bin(),
        config.herdr.session.clone(),
        herdr.clone(),
        link_tx,
        socket_tx,
        log.clone(),
    ));
    let (queries_tx, queries_rx) = watch::channel(Vec::new());
    let (level_tx, level_rx) = watch::channel(Levels::new());
    let level_tx = Arc::new(level_tx);
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // One Linear task per workspace, each with its own client and wake.
    let (ended_tx, mut ended) = mpsc::channel::<()>(1);
    let mut linear_wake = Vec::new();
    let mut tasks = Vec::new();
    for (name, workspace) in &config.workspaces {
        let wake = Arc::new(Notify::new());
        linear_wake.push(wake.clone());
        let links = Links {
            queries: queries_rx.clone(),
            level: level_tx.clone(),
            events: events_tx.clone(),
            wake,
        };
        let task = linear(
            name.clone(),
            workspace.clone(),
            state_dir.to_path_buf(),
            links,
            log.clone(),
        );
        let ended_tx = ended_tx.clone();
        tasks.push(tokio::spawn(async move {
            task.await;
            let _ = ended_tx.send(()).await;
        }));
    }
    drop((queries_rx, level_tx, events_tx, ended_tx));
    let linear = async move {
        ended.recv().await;
    };
    let reconciler = reconcile::run(reconcile::Inputs {
        ctx,
        config,
        herdr,
        socket,
        log: log.clone(),
        link: link.clone(),
        level: level_rx,
        events: events_rx,
        queries: queries_tx,
        linear_wake,
        shutdown: shutdown_rx,
        poke: listen(state_dir, log),
        clock: Timestamp::now,
    });
    tokio::pin!(linear, reconciler);
    let served = tokio::select! {
        result = &mut reconciler => {
            finder.abort();
            tasks.iter().for_each(tokio::task::JoinHandle::abort);
            return result.map(|()| Served::Stopped("the reconciler ended".into()));
        }
        reason = supervise(state_dir, link) => Served::Stopped(reason),
        () = &mut linear => Served::Stopped("a Linear task ended".into()),
        reloaded = reload => Served::Reload(reloaded),
    };
    finder.abort();
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
    let _ = shutdown_tx.send(true);
    reconciler.await?;
    let _ = std::fs::remove_file(poke_path(state_dir));
    Ok(served)
}

/// Finds the configured session's socket (`herdr session list` fails while
/// Herdr is down, so it is retried), then hands the client to the
/// reconciler and follows the session's events.
async fn connect(
    herdr_bin: String,
    session: Option<String>,
    herdr: herdr::Client,
    link: watch::Sender<Link>,
    socket_path: watch::Sender<String>,
    log: Arc<Log>,
) {
    let mut logged = false;
    let socket = loop {
        match herdr::session_socket(&herdr_bin, session.as_deref()).await {
            Ok(socket) => break socket,
            Err(error) if !logged => {
                log.line(&format!("Herdr is not reachable yet: {error:#}"));
                logged = true;
            }
            Err(_) => {}
        }
        tokio::time::sleep(SOCKET_RETRY).await;
    };
    herdr.set_socket(socket.clone());
    let _ = socket_path.send(socket.to_string_lossy().into_owned());
    let mut events = herdr::wake(herdr);
    loop {
        let current = events.borrow_and_update().clone();
        if link.send(current).is_err() || events.changed().await.is_err() {
            return;
        }
    }
}

/// Returns why the ticker should exit.
async fn supervise(state_dir: &Path, link: watch::Receiver<Link>) -> String {
    let mut every = tokio::time::interval(SUPERVISE_EVERY);
    loop {
        every.tick().await;
        if stop_path(state_dir).exists() {
            return "asked to stop".into();
        }
        if unreachable(&link.borrow(), Timestamp::now()) {
            return "the configured Herdr session was unreachable for 5 minutes".into();
        }
    }
}

/// Builds the workspace's Linear client from its stored credential, retrying
/// on the intake interval, and runs the Linear task on it.
async fn linear(
    name: String,
    workspace: crate::config::Workspace,
    state_dir: std::path::PathBuf,
    links: Links,
    log: Arc<Log>,
) {
    let retry = Duration::from_secs(workspace.intake_interval_seconds);
    let mut logged = false;
    let client = loop {
        let built = crate::linear::client::Client::production(&name, &workspace, &state_dir).await;
        match built {
            Ok(client) => break client,
            Err(error) => {
                if !logged {
                    log.line(&format!(
                        "{name}: Linear is not available: {error}; run the login action"
                    ));
                    logged = true;
                }
                tokio::select! {
                    () = tokio::time::sleep(retry) => {}
                    () = links.events.closed() => return,
                }
            }
        }
    };
    LinearTask::new(&name, &workspace)
        .run(&client, links, Timestamp::now, |line| log.line(line))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Env;
    use crate::process::fake::FakeRunner;

    const MINE: &str = "0.2.0+b1d";
    const RELEASE_WAIT: Duration = Duration::from_secs(5);

    const POLL: Duration = Duration::from_millis(20);

    fn sample_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::config::tests::write_sample(dir.path(), crate::config::tests::SAMPLE);
        dir
    }

    #[tokio::test]
    async fn a_change_is_told_and_a_reload_request_serves_again_while_the_lock_is_held() {
        let config_dir = sample_dir();
        let state = tempfile::tempdir().unwrap();
        let _lock = acquire(state.path()).await.unwrap();
        let log = Log::new(state.path().join("ticker.log"));
        let config = Config::load(config_dir.path()).unwrap();
        let fingerprint = Config::fingerprint(config_dir.path()).unwrap();
        let standard = config_dir.path().join("profiles/standard/config.toml");
        let notes = std::sync::Mutex::new(Vec::new());
        let notify = |_: &Config, body: String| notes.lock().unwrap().push(body);
        let mut args = Vec::new();
        let stopped = keep_serving(
            config_dir.path(),
            state.path(),
            (Box::new(config), fingerprint),
            &log,
            &notify,
            POLL,
            async |config, mut reload| {
                args.push(config.profile("standard").unwrap().args.clone());
                if args.len() == 2 {
                    assert_eq!(
                        decide_start(&lock_state(state.path()), VERSION, false),
                        StartAction::Nothing,
                        "the lock is held while the tasks start again"
                    );
                    return Ok(Served::Stopped("asked to stop".into()));
                }
                std::fs::write(&standard, "kind = \"claude\"\nargs = [\"--new\"]\n").unwrap();
                let waited = tokio::time::timeout(POLL * 10, &mut reload).await;
                assert!(waited.is_err(), "a change alone does not reload");
                std::fs::write(reload_path(state.path()), "").unwrap();
                Ok(Served::Reload(reload.await))
            },
        )
        .await
        .unwrap();
        assert_eq!(stopped, "asked to stop");
        assert_eq!(
            args,
            [
                vec!["--permission-mode".to_string(), "auto".to_string()],
                vec!["--new".to_string()],
            ]
        );
        assert_eq!(
            *notes.lock().unwrap(),
            ["The config changed. Run herdr-linear-agent: reload the config to apply it."],
            "told once"
        );
        assert!(!reload_path(state.path()).exists(), "the request is taken");
        let text = std::fs::read_to_string(state.path().join("ticker.log")).unwrap();
        assert_eq!(
            text.matches("config reloaded; restarting the ticker's tasks")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_config_that_does_not_load_is_told_once_and_a_request_keeps_the_old_one() {
        let config_dir = sample_dir();
        let state = tempfile::tempdir().unwrap();
        let log = Log::new(state.path().join("ticker.log"));
        let in_use = Config::load(config_dir.path()).unwrap();
        let fingerprint = Config::fingerprint(config_dir.path()).unwrap();
        let notes = std::sync::Mutex::new(Vec::new());
        let notify = |_: &Config, body: String| notes.lock().unwrap().push(body);
        let path = config_dir.path().join("config.toml");
        std::fs::write(&path, "not toml [").unwrap();
        let watch = watch_config(
            config_dir.path(),
            state.path(),
            (&in_use, fingerprint),
            &log,
            &notify,
            POLL,
        );
        tokio::pin!(watch);
        assert!(tokio::time::timeout(POLL * 10, &mut watch).await.is_err());
        std::fs::write(reload_path(state.path()), "").unwrap();
        assert!(
            tokio::time::timeout(POLL * 10, &mut watch).await.is_err(),
            "a request for a config that does not load keeps the old one"
        );
        {
            let notes = notes.lock().unwrap();
            assert_eq!(notes.len(), 2, "{notes:?}");
            assert!(notes[0].starts_with("The config changed but does not load: "));
            assert!(notes[1].starts_with("The config was not reloaded: "));
        }
        let fixed =
            crate::config::tests::SAMPLE.replace("timeout_seconds = 60", "timeout_seconds = 90");
        std::fs::write(&path, fixed).unwrap();
        std::fs::write(reload_path(state.path()), "").unwrap();
        let (config, _) = tokio::time::timeout(POLL * 50, watch).await.unwrap();
        assert_eq!(config.routing.timeout_seconds, 90);
        let text = std::fs::read_to_string(state.path().join("ticker.log")).unwrap();
        assert_eq!(text.matches("config reload failed: ").count(), 1);
    }

    fn held_by(version: &str, pid: u32, started: &str) -> LockState {
        LockState::Held(Info {
            version: version.into(),
            pid,
            started: started.into(),
        })
    }

    /// Takes the ticker lock the way another process would and writes `info`
    /// into it; the lock lasts as long as the returned file.
    fn grab_lock(state_dir: &Path, info: &str) -> File {
        let mut file = File::create(lock_path(state_dir)).unwrap();
        file.try_lock().unwrap();
        file.write_all(info.as_bytes()).unwrap();
        file
    }

    /// Another test may fork while the lock file is open, so the release is
    /// observed by polling.
    async fn released(state_dir: &Path) -> bool {
        let give_up = tokio::time::Instant::now() + RELEASE_WAIT;
        while lock_state(state_dir) != LockState::Free {
            if tokio::time::Instant::now() >= give_up {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    #[test]
    fn start_spawns_leaves_or_replaces_by_the_holders_version() {
        let table = [
            (LockState::Free, false, StartAction::Spawn),
            (LockState::Free, true, StartAction::Spawn),
            (held_by(MINE, 11, ""), false, StartAction::Nothing),
            (
                held_by("0.1.0+a0a", 11, ""),
                false,
                StartAction::StopThenSpawn,
            ),
            (
                held_by("0.1.0+a0a", 11, ""),
                true,
                StartAction::StopThenSpawn,
            ),
            (held_by(MINE, 11, ""), true, StartAction::StopThenSpawn),
        ];
        for (lock, stop_file, expected) in table {
            assert_eq!(
                decide_start(&lock, MINE, stop_file),
                expected,
                "{lock:?} with stop file {stop_file}"
            );
        }
    }

    #[tokio::test]
    async fn without_a_config_start_neither_spawns_nor_creates_the_state_folder() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: true,
        };
        start(&ctx).await.unwrap();
        assert!(!env.state_dir().exists());
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_probe_reports_the_holders_version_pid_and_start() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(lock_state(state.path()), LockState::Free);
        assert_eq!(describe(state.path()), "ticker not running");

        let holder = grab_lock(
            state.path(),
            r#"{"version":"9.9.9+feed","pid":4242,"started":"2026-09-28T09:00:00Z"}"#,
        );
        assert_eq!(
            lock_state(state.path()),
            held_by("9.9.9+feed", 4242, "2026-09-28T09:00:00Z")
        );
        assert_eq!(
            describe(state.path()),
            "ticker 9.9.9+feed running since 2026-09-28T09:00:00Z (pid 4242)"
        );

        drop(holder);
        assert!(released(state.path()).await);
        assert!(describe(state.path()).contains("not running"));
    }

    #[tokio::test]
    async fn stop_without_a_holder_only_clears_a_leftover_stop_file() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(stop_path(state.path()), b"").unwrap();
        stop(state.path()).await.unwrap();
        assert!(!stop_path(state.path()).exists());
        assert_eq!(lock_state(state.path()), LockState::Free);
    }

    #[tokio::test]
    async fn the_running_ticker_holds_the_lock_alone_and_names_this_build() {
        let state = tempfile::tempdir().unwrap();
        let owner = acquire(state.path()).await.unwrap();
        let LockState::Held(info) = lock_state(state.path()) else {
            panic!("the lock is free while a ticker holds it");
        };
        assert_eq!(info.version, VERSION);
        assert_eq!(info.pid, std::process::id());
        assert!(
            info.started.parse::<Timestamp>().is_ok(),
            "{}",
            info.started
        );
        assert_eq!(
            decide_start(&lock_state(state.path()), VERSION, false),
            StartAction::Nothing
        );

        let rival = acquire(state.path()).await.unwrap_err().to_string();
        assert!(rival.contains("another ticker runs"), "{rival}");

        drop(owner);
        assert!(released(state.path()).await);
        drop(acquire(state.path()).await.unwrap());
    }

    #[tokio::test]
    async fn a_ticker_of_an_older_build_is_stopped_before_this_build_takes_over() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().to_path_buf();
        let old = grab_lock(&dir, r#"{"version":"0.0.9+0ld","pid":1}"#);
        // Stands in for the old ticker's supervisor: it exits once asked.
        let old_ticker = tokio::spawn({
            let dir = dir.clone();
            async move {
                while !stop_path(&dir).exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                drop(old);
            }
        });

        let seen = lock_state(&dir);
        assert_eq!(
            decide_start(&seen, VERSION, stop_path(&dir).exists()),
            StartAction::StopThenSpawn
        );
        stop(&dir).await.unwrap();
        old_ticker.await.unwrap();
        assert_eq!(lock_state(&dir), LockState::Free);
        assert!(!stop_path(&dir).exists());

        let successor = acquire(&dir).await.unwrap();
        let LockState::Held(info) = lock_state(&dir) else {
            panic!("the new ticker does not hold the lock");
        };
        assert_eq!(info.version, VERSION);
        drop(successor);
    }

    #[test]
    fn the_log_is_trimmed_to_stay_between_a_quarter_of_its_cap_and_the_cap() {
        let state = tempfile::tempdir().unwrap();
        let log = Log::new(log_path(state.path()));
        let filler = "y".repeat(10_000);
        for n in 0..150 {
            log.line(&format!("entry {n} {filler}"));
        }
        let text = std::fs::read_to_string(&log.path).unwrap();
        let size = text.len() as u64;
        assert!(size <= LOG_CAP, "{size} bytes");
        assert!(size > LOG_CAP / 4, "{size} bytes");
        assert!(text.ends_with(&format!("entry 149 {filler}\n")));
        assert!(!text.contains("entry 0 "));
        assert!(text.lines().all(|l| l.ends_with(filler.as_str())));
    }

    #[test]
    fn a_log_entry_stays_on_one_line() {
        let state = tempfile::tempdir().unwrap();
        let log = Log::new(log_path(state.path()));
        log.line("DATA-1: first\nsecond\tthird");
        let text = std::fs::read_to_string(&log.path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.ends_with(" DATA-1: first second third\n"), "{text}");
    }

    #[test]
    fn the_session_is_given_up_after_five_minutes_down() {
        let went_down: Timestamp = "2026-09-28T10:00:00Z".parse().unwrap();
        let table = [
            (false, 0, false),
            (false, 299, false),
            (false, 300, true),
            (false, 7200, true),
            (true, 7200, false),
        ];
        for (connected, secs, gone) in table {
            let link = Link {
                connected,
                since: went_down,
                wakes: 3,
                last_error: None,
            };
            let now = went_down
                .checked_add(SignedDuration::from_secs(secs))
                .unwrap();
            assert_eq!(
                unreachable(&link, now),
                gone,
                "connected {connected}, {secs} s"
            );
        }
    }
}
