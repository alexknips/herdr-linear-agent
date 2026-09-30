mod actions;
mod agents;
mod claude_trust;
mod cli;
mod commands;
mod config;
mod coordinator;
mod files;
mod herdr;
mod history;
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

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The release version plus a build identifier (short git hash and build
/// time), so a rebuilt binary always differs from the one a running ticker was
/// started from.
pub const VERSION: &str = concat!(env!("HLA_RELEASE_VERSION"), "+", env!("HLA_BUILD_ID"));

/// `current` with each existing folder it lacks appended, or `None` when
/// nothing is missing. Herdr may start plugins with a bare `PATH` when it was
/// not launched from a login shell, so the folders where `git` and the agent
/// CLIs are usually installed are added at the end: the user's own order
/// still wins.
fn extended_path(
    current: &OsStr,
    home: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<OsString> {
    let home_folders = [".local/bin", ".cargo/bin", ".local/share/mise/shims"]
        .into_iter()
        .filter_map(|sub| home.map(|h| h.join(sub)));
    let system_folders = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/home/linuxbrew/.linuxbrew/bin",
    ]
    .map(PathBuf::from);
    let present: Vec<PathBuf> = std::env::split_paths(current).collect();
    let added: Vec<PathBuf> = home_folders
        .chain(system_folders)
        .filter(|dir| exists(dir) && !present.contains(dir))
        .collect();
    if added.is_empty() {
        return None;
    }
    let mut path = current.to_os_string();
    if !path.is_empty() {
        path.push(":");
    }
    path.push(std::env::join_paths(&added).ok()?);
    Some(path)
}

fn extend_path() {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let current = std::env::var_os("PATH").unwrap_or_default();
    if let Some(path) = extended_path(&current, home.as_deref(), Path::is_dir) {
        // SAFETY: called first thing in `main`, before any other thread exists.
        unsafe { std::env::set_var("PATH", path) };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_install_folders_are_appended_once_in_order() {
        let home = Path::new("/home/ann");
        let all = |_: &Path| true;
        let path = extended_path(OsStr::new("/usr/bin:/usr/local/bin"), Some(home), all).unwrap();
        assert_eq!(
            path,
            "/usr/bin:/usr/local/bin:/home/ann/.local/bin:/home/ann/.cargo/bin:\
             /home/ann/.local/share/mise/shims:/opt/homebrew/bin:/home/linuxbrew/.linuxbrew/bin"
        );

        let some =
            |p: &Path| p == Path::new("/opt/homebrew/bin") || p.starts_with("/home/ann/.cargo");
        assert_eq!(
            extended_path(OsStr::new("/bin"), Some(home), some).unwrap(),
            "/bin:/home/ann/.cargo/bin:/opt/homebrew/bin"
        );
        assert_eq!(
            extended_path(OsStr::new(""), None, some).unwrap(),
            "/opt/homebrew/bin"
        );
    }

    #[test]
    fn the_version_is_the_release_version_plus_a_build_id() {
        let release = include_str!("../.release-version").trim();
        let (head, build) = VERSION.split_once('+').unwrap();
        assert_eq!(head, release);
        let (hash, time) = build.split_once('.').unwrap();
        assert!(
            hash == "nogit" || hash.chars().all(|c| c.is_ascii_hexdigit()),
            "{hash}"
        );
        assert!(time.parse::<u64>().unwrap() > 1_700_000_000, "{time}");
    }

    #[test]
    fn nothing_changes_when_every_folder_is_present_or_missing() {
        let none = |_: &Path| false;
        assert_eq!(
            extended_path(OsStr::new("/bin"), Some(Path::new("/h")), none),
            None
        );
        let brew = |p: &Path| p == Path::new("/opt/homebrew/bin");
        assert_eq!(
            extended_path(OsStr::new("/opt/homebrew/bin:/bin"), None, brew),
            None
        );
    }
}
