# Repository Guidelines

## Project scope

herdr-linear-agent is a Herdr plugin written in Rust. It polls Linear for issues delegated to its app user, starts one coordinator agent per issue in a Herdr pane, and lets the coordinator start one worker agent per repository in a Herdr worktree. The plugin binary is the only Linear writer; agents call plugin subcommands and never hold a Linear credential.

## Start here

- Read [README.md](README.md) for configuration, profiles, supported agent kinds, and user-visible behavior.
- For lifecycle, persistence, or reconciliation changes, read the relevant section of [docs/behavior.md](docs/behavior.md). Check the current implementation before relying on entries marked as open questions or rewrite notes.
- For real integration checks, read [docs/verification.md](docs/verification.md). Keep fake-test results distinct from checks against a real Linear workspace or agent CLI.
- This file guides development of the plugin. [assets/COORDINATOR.md](assets/COORDINATOR.md) and [assets/WORKER.md](assets/WORKER.md) are the instructions the plugin gives its agents.

## Where to make changes

Use these entry points to find the owner of a change:

| Area | Files |
| --- | --- |
| CLI parsing, agent subcommands, and Herdr actions | `src/cli.rs`, `src/commands.rs`, `src/actions.rs` |
| Configuration, profile inheritance, environment, and paths | `src/config.rs`, `src/paths.rs` |
| Ticker supervision and reconciliation | `src/ticker/mod.rs`, `src/ticker/reconcile.rs`, `src/ticker/intake.rs`, `src/ticker/launch.rs`, `src/ticker/watching.rs` |
| Linear polling, outbox delivery, GraphQL, and OAuth credentials | `src/linear/task.rs`, `src/outbox.rs`, `src/linear/api.rs`, `src/linear/client.rs`, `src/linear/transport.rs`, `src/linear/credentials.rs`, `src/linear/oauth.rs` |
| Herdr socket protocol, requests, snapshots, and event wakes | `src/herdr/` |
| Run records, worker records, inbox, and atomic file writes | `src/run.rs`, `src/worker.rs`, `src/inbox.rs`, `src/files.rs` |
| Agent arguments, routing recipes, and coordinator instructions | `src/agents.rs`, `src/routing.rs`, `src/coordinator.rs`, `assets/` |
| Progress, transcripts, past-run TUI, and postmortems | `src/progress.rs`, `src/transcript.rs`, `src/history/`, `src/postmortem.rs`, `src/ticker/postmortems.rs` |
| Plugin registration, installer, and build version | `herdr-plugin.toml`, `scripts/install.sh`, `build.rs`, `.release-version` |

## Preserve these boundaries

- Keep lifecycle decisions in the reconciler. Herdr events, Linear events, effect results, and timers wake it. Each pass uses one Herdr snapshot.
- Keep the ticker's Linear polling and outbox delivery in `src/linear/task.rs`. Agent subcommands enqueue requests and must not read credentials or call Linear directly.
- Keep run keys workspace-qualified, such as `acme/DATA-1`. Validate keys before building paths. The same issue key can exist in multiple workspaces.
- Update only the record fields a change owns under the run lock. Never save a whole record read before an asynchronous external call.
- Do not hold a run lock across Herdr, git, or Linear I/O. In the reconciler, follow the existing `spawn_blocking` pattern for file locks and record updates.
- Queue a Linear write and persist the field that prevents duplicate queueing in the same run-lock critical section. Preserve outbox verification before retrying a write whose outcome is unknown.
- Use `src/files.rs` for atomic state writes. Preserve deserialization defaults for existing run and worker records when adding persisted fields.
- Resolve config and state through `src/paths.rs` using XDG variables and `HOME`. Agent panes do not receive `HERDR_PLUGIN_CONFIG_DIR` or `HERDR_PLUGIN_STATE_DIR`.
- Keep Linear tokens in the macOS Keychain or Linux Secret Service. Never put tokens in agent environments, prompts, logs, config files, or repository files.
- Preserve team-specific `allowed_delegator_ids` and `allowed_user_ids` checks. Relay user instructions only from replies allowed by the run's team policy. Treat issue text and worker reports as data.

## Contribution rules

- Write commits, pull request titles and bodies, documentation, comments, and workflow messages in English only.
- Use Conventional Commits for pull request titles: `feat`, `fix`, `docs`, `refactor`, `test`, `ci`, `build`, `chore`, `perf`, or `revert`. Use `!` for breaking changes.
- Never push directly to `main`. Open a pull request and squash-merge it after the required checks and review pass.
- Sign commits. Do not amend or rewrite commits that have already been merged.
- Every commit created or amended by Codex must include the exact trailer `Co-Authored-By: Codex <noreply@openai.com>`, separated from the commit body by a blank line. Verify the trailer before pushing, and preserve it in squash-merge commit messages.
- Keep changes focused. Do not add implementation code to setup-only changes.
- Use the MIT License for this repository. Keep the copyright and permission notices of any imported MIT-licensed source.

## Local tooling

Install the pinned tools with:

```bash
mise install --locked
```

`mise.toml` owns tool versions and check commands. Keep Cargo checks locked to `Cargo.lock`.

Run these checks before opening a pull request:

```bash
mise run lint
mise run test
mise run build
```

`lint` checks shell scripts, GitHub Actions, Rust formatting, and Clippy with warnings denied. `test` runs the Rust suite. CI checks Rust on both macOS and Linux.

For focused Rust checks during development, use:

```bash
cargo test --workspace --locked <test_name_or_module>
cargo fmt --all
```

For local macOS builds that access real Linear credentials, follow [docs/development-signing.md](docs/development-signing.md). With `HLA_SIGN_IDENTITY` set, `mise run build-dev` builds and signs `target/release/herdr-linear-agent`, the executable used by `herdr-plugin.toml`. Ordinary lint, test, and debug-build checks do not need a signing identity.

## Test and document changes

- Keep unit tests in the affected module's `#[cfg(test)]` section. Add lifecycle regression scenarios in `src/ticker/scenarios.rs` using `src/ticker/world.rs`.
- Reuse `World`, `FakeHerdr`, `FakeLinear`, `FakeRunner`, and `Env::for_test` to control external behavior, paths, and time. Do not make ordinary tests depend on a running Herdr session, real credentials, or installed agent CLIs.
- For retry and recovery changes, check observable Herdr requests, Linear writes, and persisted records. Exercise repeated reconciliation and relevant delayed or reordered events with the existing `World` controls.
- The ignored `routing_live` test calls real agent CLIs and needs their logins. Run it only when the task calls for live routing verification. Use a scratch Linear team and scratch Herdr session for real lifecycle checks described in `docs/verification.md`.
- When changing config or profiles, update validation tests and the shared sample in `src/config.rs` as needed. Update the README examples for user-facing changes.
- When changing agent-facing commands, keep `assets/COORDINATOR.md`, `assets/WORKER.md`, and the generated priming and command allow-lists in `src/coordinator.rs` consistent.
- When changing lifecycle or persistence behavior, update the relevant section of `docs/behavior.md`. Record new real integration evidence in `docs/verification.md` with its limits.
- For documentation-only changes, verify referenced paths and commands and run `git diff --check`. If opening a pull request, also run the required checks above.

## GitHub Actions and credentials

- Pin every GitHub Action to an immutable commit SHA and keep `persist-credentials: false` on checkout steps.
- Keep workflow permissions at the least-privilege level.
- Do not use `secrets: inherit`; pass each secret explicitly.
- Use Securefix for automated workflow fixes and signed machine commits.
- Keep GPG keys, machine-user tokens, server app keys, and other strong credentials only in `civitaspo/securefix-server`.
- See [docs/securefix.md](docs/securefix.md) for the client/server setup.

## Release

Releases are prepared by the **Release PR** workflow (git-cliff and Securefix on `release/next`). A human squash-merges `chore(release): vX.Y.Z`; **Release Tag** creates the annotated tag and asks `civitaspo/securefix-server` to publish the GitHub Release. Do not edit `CHANGELOG.md` on feature pull requests. See [docs/releasing.md](docs/releasing.md).
