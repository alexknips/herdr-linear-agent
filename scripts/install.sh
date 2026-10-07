#!/bin/sh
# The plugin's build step. Installs target/release/herdr-linear-agent from the
# GitHub release named by .release-version, checked against its .sha256 file.
# When this platform has no release binary, or it cannot be downloaded, the
# binary is built from source with cargo instead. A binary whose checksum does
# not match is never installed.
set -eu

# Aytza fork: this branch carries changes upstream's release binaries lack, and the fork
# publishes no releases, so the download below fails and the binary is built from source.
repo="alexknips/herdr-linear-agent"
cd "$(dirname "$0")/.."
dest="target/release/herdr-linear-agent"

say() { printf 'herdr-linear-agent install: %s\n' "$*" >&2; }

build_from_source() {
  say "$1; building from source with cargo"
  if ! command -v cargo >/dev/null 2>&1; then
    say "cargo is not installed; install Rust (https://rustup.rs) and run the build again"
    exit 1
  fi
  cargo build --release --locked
  exit 0
}

host_target() {
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64 | Darwin/aarch64) echo aarch64-apple-darwin ;;
    Darwin/x86_64) echo x86_64-apple-darwin ;;
    Linux/x86_64 | Linux/amd64) echo x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) echo aarch64-unknown-linux-musl ;;
    *) return 1 ;;
  esac
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    return 1
  fi
}

version=$(tr -d '[:space:]' < .release-version)
[ -n "$version" ] || build_from_source ".release-version is empty"
target=$(host_target) || build_from_source "no release binary for $(uname -s) $(uname -m)"
command -v curl >/dev/null 2>&1 || build_from_source "curl is not installed"

asset="herdr-linear-agent-$target"
url="https://github.com/$repo/releases/download/v$version/$asset"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/herdr-linear-agent.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM

say "downloading $asset v$version"
if ! curl -fsSL --retry 3 -o "$tmp/$asset" "$url" ||
  ! curl -fsSL --retry 3 -o "$tmp/$asset.sha256" "$url.sha256"; then
  build_from_source "could not download $url"
fi

expected=$(cut -d ' ' -f 1 < "$tmp/$asset.sha256")
actual=$(sha256_of "$tmp/$asset") || build_from_source "no sha256sum or shasum to check the download"
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  say "the checksum of $asset does not match $asset.sha256 (expected ${expected:-nothing}, got $actual); nothing was installed"
  exit 1
fi

mkdir -p target/release
chmod 755 "$tmp/$asset"
mv -f "$tmp/$asset" "$dest.tmp"
mv -f "$dest.tmp" "$dest"
say "installed $dest v$version"
