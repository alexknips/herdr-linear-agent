mod actions;
mod agents;
mod claude_trust;
mod cli;
mod commands;
mod config;
mod coordinator;
mod files;
mod herdr;
mod inbox;
mod linear;
mod names;
mod outbox;
mod paths;
mod process;
mod progress;
mod routing;
mod run;
mod ticker;
mod worker;

/// The release version plus a build identifier (short git hash and build
/// time), so a rebuilt binary always differs from the one a running ticker was
/// started from.
pub const VERSION: &str = concat!(env!("HLA_RELEASE_VERSION"), "+", env!("HLA_BUILD_ID"));

/// A Herdr server that was not started from a login shell hands its plugins a
/// minimal `PATH`, so `git` or an agent CLI may be missing for the ticker
/// although they work in the user's terminal. The usual install folders are
/// appended (never prepended: what the user's `PATH` resolves still wins).
fn extend_path() {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<std::path::PathBuf> = std::env::split_paths(&current).collect();
    let mut extra: Vec<std::path::PathBuf> =
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
            .iter()
            .map(Into::into)
            .collect();
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        extra.push(home.join(".local/bin"));
        extra.push(home.join(".cargo/bin"));
    }
    for dir in extra {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    if let Ok(joined) = std::env::join_paths(dirs) {
        // SAFETY: first thing in `main`, before any thread exists.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

fn main() {
    // `PATH` is changed before the runtime starts its threads.
    extend_path();
    let result = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(cli::run()));
    if let Err(error) = result {
        eprintln!("herdr-linear-agent: {error:#}");
        std::process::exit(1);
    }
}
