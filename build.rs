//! Sets the two halves of the binary's version:
//!
//! - `HLA_RELEASE_VERSION`: the contents of `.release-version`;
//! - `HLA_BUILD_ID`: the short git hash (`nogit` outside a checkout) and the
//!   build time in Unix seconds, joined by `.`.
//!
//! No `rerun-if-changed` line is printed on purpose: Cargo then runs this
//! again whenever a file of the package changes, so every rebuilt binary gets
//! a new build id and a running ticker of an older build is replaced.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let release = std::fs::read_to_string(".release-version")
        .expect("cannot read .release-version")
        .trim()
        .to_string();
    assert!(!release.is_empty(), ".release-version is empty");

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|hash| !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| "nogit".to_string());
    let built = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!("cargo:rustc-env=HLA_RELEASE_VERSION={release}");
    println!("cargo:rustc-env=HLA_BUILD_ID={hash}.{built}");
}
