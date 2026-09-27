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
use crate::linear::task::{LinearLevel, LinearTask, Links};
use crate::paths::Ctx;

/// The log stays below this size; trimming keeps its newer half.
pub const LOG_CAP: u64 = 1_000_000;
/// The ticker exits once the configured session was unreachable this long.
pub const UNREACHABLE_FOR: SignedDuration = SignedDuration::from_secs(5 * 60);
const STOP_WAIT: Duration = Duration::from_secs(30);
const SUPERVISE_EVERY: Duration = Duration::from_secs(1);
const SOCKET_RETRY: Duration = Duration::from_secs(5);
const EVENT_QUEUE: usize = 256;

pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.lock")
}

pub fn stop_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.stop")
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
async fn acquire(state_dir: &Path) -> Result<File> {
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
/// session has been unreachable for five minutes.
pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let config = Config::load(&ctx.config_dir())?;
    let state_dir = ctx.ensure_state_dir()?;
    let _lock = acquire(&state_dir).await?;
    let _ = std::fs::remove_file(stop_path(&state_dir));
    let log = Arc::new(Log::new(log_path(&state_dir)));
    log.line(&format!(
        "ticker {VERSION} started (pid {})",
        std::process::id()
    ));
    match serve(ctx, &config, &state_dir, &log).await {
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

/// Runs the Linear task and the reconciler at once. Until the configured
/// session's socket is found the Herdr requests are `NotSent`, so the
/// reconciler runs Linear-only passes; the link counts as down since the
/// start, so the 5-minute unreachable exit still applies.
async fn serve(ctx: &Ctx<'_>, config: &Config, state_dir: &Path, log: &Arc<Log>) -> Result<String> {
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
    let (level_tx, level_rx) = watch::channel(LinearLevel::default());
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
    let linear_wake = Arc::new(Notify::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let linear = linear(
        config,
        state_dir,
        Links {
            queries: queries_rx,
            level: level_tx,
            events: events_tx,
            wake: linear_wake.clone(),
        },
        log.clone(),
    );
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
    let reason = tokio::select! {
        result = &mut reconciler => {
            finder.abort();
            return result.map(|()| "the reconciler ended".into());
        }
        reason = supervise(state_dir, link) => reason,
        () = &mut linear => "the Linear task ended".into(),
    };
    finder.abort();
    let _ = shutdown_tx.send(true);
    reconciler.await?;
    let _ = std::fs::remove_file(poke_path(state_dir));
    Ok(reason)
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

/// Builds the Linear client from the stored credential, retrying on the
/// intake interval, and runs the Linear task on it.
async fn linear(config: &Config, state_dir: &Path, links: Links, log: Arc<Log>) {
    let lock = state_dir.join("credentials.lock");
    let retry = Duration::from_secs(config.linear.intake_interval_seconds);
    let mut logged = false;
    let client = loop {
        let built = crate::linear::client::Client::production(
            config.linear.client_id.clone(),
            config.linear.callback_port,
            lock.clone(),
        )
        .await;
        match built {
            Ok(client) => break client,
            Err(error) => {
                if !logged {
                    log.line(&format!(
                        "Linear is not available: {error}; run the login action"
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
    LinearTask::new(&config.linear)
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
