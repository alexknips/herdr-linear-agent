# herdr-linear-agent: target behavior

This document is the behavior specification for the async (tokio) rewrite of herdr-linear-agent. It is written from the test modules, the kept modules, the docs and the Herdr socket schema. It states behavior as it must be after the rewrite. Where the rewrite changes timing on purpose, a line marked **Timing change** says so; everything else stays as it is today.

Conventions:

- `tests/<file>:<test>` cites a test in `tests/` that pins the rule. `src/<file>:<test>` cites a test inside a kept module. Port every cited test.
- Literal strings are in code spans and must be reproduced byte for byte. `<KEY>` is the issue key (for example `DATA-1`), `<key>` is its lower-case form, `<id>` is a worker id (`w1`), `<bin>` is the shell-quoted absolute path of the binary.
- Two literals contain characters that are shown here by code point: the digest's catalog separator is written `<U+2014>`, and the pane display separator `·` is U+00B7.
- "Pass" means one reconciliation of one run (see [The ticker process](#the-ticker-process)). The tests count ticks; in the rewrite they count passes or wait until the ticker is quiescent.

## Contents

1. [Architecture of the rewrite](#architecture-of-the-rewrite)
2. [Paths and environment](#paths-and-environment)
3. [Config](#config)
4. [Run folder layout and state files](#run-folder-layout-and-state-files)
5. [Names, agent kinds and profile arguments](#names-agent-kinds-and-profile-arguments)
6. [Small helpers](#small-helpers)
7. [External commands](#external-commands)
8. [Herdr access](#herdr-access)
9. [The ticker process](#the-ticker-process)
10. [Linear client](#linear-client)
11. [Linear polling and rate limits](#linear-polling-and-rate-limits)
12. [Intake and claim](#intake-and-claim)
13. [Routing](#routing)
14. [Reading runs: close, detach, issue edits](#reading-runs-close-detach-issue-edits)
15. [Relay of Linear prompts](#relay-of-linear-prompts)
16. [Watching coordinators and workers](#watching-coordinators-and-workers)
17. [Launching agents](#launching-agents)
18. [Nudge](#nudge)
19. [Heartbeat and run timeout](#heartbeat-and-run-timeout)
20. [Outbox and flush](#outbox-and-flush)
21. [The inbox](#the-inbox)
22. [Worker commands](#worker-commands)
23. [Coordinator commands and the digest](#coordinator-commands-and-the-digest)
24. [Progress reports](#progress-reports)
25. [Actions and doctor](#actions-and-doctor)
26. [Command line](#command-line)
27. [Install script and build version](#install-script-and-build-version)
28. [Porting the scenario tests](#porting-the-scenario-tests)
29. [Decisions for the questions the inputs left open](#decisions-for-the-questions-the-inputs-left-open)

## Architecture of the rewrite

- One process, the ticker, runs a tokio runtime with three independent parts:
  1. **Herdr state.** A socket subscription keeps an in-memory view of workspaces, panes and agents. Every Herdr request (start, prompt, keys, rename, close, create, notification, metadata) opens its own socket connection, sends one request and reads one response.
  2. **Linear poll task.** It polls delegated issues and reads active runs on config-driven intervals, reads rate-limit and complexity headers, and backs off. It never waits on Herdr.
  3. **Run reconciliation.** Each run is reconciled in a pass whenever one of its inputs changes: a Herdr event that touches its panes, a Linear read result, a file an agent wrote (outbox, report, inbox), a routing agent finishing, or a timer that makes a time-based rule due.
- The ticker is the only Linear writer. Agents and short-lived subcommands only write outbox requests and files in the run folder. No subcommand except `action login`, `action doctor` and the ticker reads the Keychain.
- Short-lived subcommands (`context`, `worker start`, `finish`, `report`, actions) stay separate processes. They read Herdr state with `session.snapshot` over the socket and make their own one-shot socket requests.
- The routing agent runs under `tokio::process` with a timeout and never blocks other runs.
- Judgment belongs to the agents. Claiming, limits and state transitions are deterministic and done by the binary.

**Timing change:** the 15 second tick becomes event-driven reconciliation plus timers. Every rule below that says "a tick" in the tests means "a pass" in the rewrite.

## Paths and environment

Operations:

| Operation | Behavior |
| --- | --- |
| `config_dir` | `$XDG_CONFIG_HOME/herdr-linear-agent` when the variable is set to an absolute path; otherwise `~/.config/herdr-linear-agent`. |
| `state_dir` | `$XDG_STATE_HOME/herdr-linear-agent` when absolute; otherwise `~/.local/state/herdr-linear-agent`. Same rule on macOS and Linux. |
| `runs_dir` | `<state_dir>/runs`. |
| `herdr_bin` | `$HERDR_BIN_PATH` when set and non-empty; otherwise `herdr`. |
| `binary` | The running executable's absolute path. `binary_command` is that path shell-quoted. |

Rules:

- An empty or relative XDG value falls back to the default. `tests/paths:xdg_absolute_values_win_and_relative_ones_fall_back`
- An empty `HERDR_BIN_PATH` falls back to `herdr`. `tests/paths:herdr_bin_prefers_the_variable`
- Paths never come from `HERDR_PLUGIN_STATE_DIR` or `HERDR_PLUGIN_CONFIG_DIR`: agent panes do not get those variables, and agents must resolve the same paths.
- At process start, `main` appends the usual install folders to `PATH` (never prepends: what the user's `PATH` resolves still wins). A Herdr server not started from a login shell hands plugins a minimal `PATH`. The folder list is an open question.
- Test contexts carry a `detached_ticker` flag. When false, `ticker start` never spawns a process (commands in tests call it).

## Config

File: `<config_dir>/config.toml`. Unknown keys are refused in every table. The kept module `src/config.rs` defines it; the rewrite adds only the poll interval keys.

| Key | Type | Default | Validation |
| --- | --- | --- | --- |
| `linear.client_id` | string | required | not blank: `linear.client_id is empty` |
| `linear.callback_port` | u16 | `43871` | not 0: `linear.callback_port may not be 0` |
| `linear.teams` | list of team keys | required | non-empty: `linear.teams lists no team` |
| `linear.allowed_user_ids` | list | `[]` | |
| `linear.review_state` | string | `In Review` | not blank: `linear.review_state is empty` |
| `linear.intake_interval_seconds` (new, name proposed) | u64 | `5` | at least 1 |
| `linear.run_read_interval_seconds` (new, name proposed) | u64 | `5` | at least 1 |
| `herdr.session` | string | unset: Herdr's default session | |
| `limits.max_runs` | u32 | `2` | the three limits must satisfy `max_runs >= 1`, `max_workers_per_run >= 1`, `max_agents >= 2`: `limits must allow one run with one worker` |
| `limits.max_workers_per_run` | u32 | `4` | |
| `limits.max_agents` | u32 | `8` | |
| `limits.run_timeout_hours` | u64 | `8` | at least 1: `limits.run_timeout_hours must be at least 1` |
| `notifications.herdr` | bool | `true` | |
| `claude.auto_accept_trust_dialog` | bool | `false` | |
| `repositories.<name>.path` | absolute path | required | `repositories.<name>.path must be an absolute path` |
| `repositories.<name>.base` | branch | required | not blank, not starting with `-`: `repositories.<name>.base is not a branch name` |
| `repositories.<name>.description` | string | `""` | |
| `profiles.<name>.kind` | Herdr agent kind | required | `profiles.<name>.kind \`<kind>\` is not a Herdr agent kind` |
| `profiles.<name>.model` | string | none | |
| `profiles.<name>.effort` | string | none | only for kinds with an effort flag: ``profiles.<name>: the `<kind>` CLI has no effort flag; put the effort in the model ID instead`` |
| `profiles.<name>.args` | list | `[]` | passed unchecked |
| `profiles.<name>.description` | string | `""` | |
| `routing.default` | profile name | required | must exist (context `routing.default`) |
| `routing.size_label_group` | string | none | |
| `routing.workers` | profile names | required | non-empty (`routing.workers lists no profile`), each must exist |
| `routing.agent.profile` | profile name | none | must exist; kind must be `claude` or `codex`: ``routing.agent.profile must be a `claude` or `codex` profile: only they return schema-checked JSON headless`` |
| `routing.agent.timeout_seconds` | u64 | `120` | at least 1: `routing.agent.timeout_seconds must be at least 1` |
| `routing.rules[n].sizes` / `.teams` / `.labels_any` | lists | `[]` | |
| `routing.rules[n].coordinator` | profile name | required | must exist (context `routing.rules[n].coordinator`) |

Rules:

- Repository and profile names: 1 to 64 characters of ASCII letters, digits, `.`, `_`, `-`, not starting with `-` or `.`. Messages: ``repository name `<name>` may use only letters, digits, `.`, `_` and `-` `` and the same with `profile name`.
- `profile(name)` fails with ``no profile named `<name>` in [profiles]``.
- `worker_profile(name)` fails when the name is not in `routing.workers`: `` `<name>` is not a worker profile; routing.workers lists <a, b> ``.
- `repository(name)` fails with `` `<name>` is not in the repository catalog (<api, web>) `` or `(empty)` for an empty catalog.
- `Config::load` errors read `could not read <path>` or `<path> is not valid`.
- Sizes are `XS S M L XL XXL XXXL unknown`, parsed by exact name. `src/config.rs:sizes_parse_and_print`
- Pinned by `src/config.rs:the_sample_parses_with_defaults` and `src/config.rs:invalid_configs_are_refused`.

The shared test config (`SAMPLE`) has teams `["DATA"]`, allowed user `user-1`, session `work`, repositories `api` (`/src/api`, base `main`, description `The API server`) and `web` (`/src/web`, base `develop`), profiles `coordinator` (claude opus high, `--permission-mode auto`), `coordinator-light` (claude sonnet), `router` (claude haiku), `standard` (claude sonnet high, `--permission-mode auto`), `deep` (codex gpt-6-sol xhigh, `-s workspace-write`), routing default `coordinator`, `size_label_group = "size"`, workers `standard, deep`, routing agent `router` 60 s, one rule `sizes = ["XS", "S"] -> coordinator-light`. The rewrite keeps it and adds the new interval keys only through defaults.

## Run folder layout and state files

State directory:

```text
<state_dir>/
  runs/<KEY>/                 one run per Linear issue
  ticker.lock                 the ticker's lock and its {"version","pid"}
  ticker.log                  the ticker's log, capped
  <stop file>                 asks the running ticker to exit (name: open question)
  paused                      intake is paused while this file exists
  progress/                   progress records written by `report`
  credentials.lock            the Linear credential lock
```

Run folder (`src/run.rs`):

```text
runs/<KEY>/
  AGENTS.md, CLAUDE.md          priming; CLAUDE.md is a symlink to AGENTS.md
  issue.md                      the issue snapshot
  conversation.md               replies from allowed users
  workers/<id>.toml             worker records
  workers/<id>.task.md          the coordinator's task and later follow-ups
  workers/<id>.md               the copy of the worker's report
  inbox/                        items for the coordinator
  inbox/done/                   handled items
  .state/run.json               the run record
  .state/lock                   the run lock
  .state/outbox/NNNNNNNNNN.json queued Linear requests
  .state/outbox/failed/         requests Linear refused or that do not parse
  .state/outbox-counter.json    the last outbox counter
  .state/ignored-prompts.md     replies from users who are not allowed
  .state/routing.out            the routing agent's answer
  .state/routing.schema.json    the routing agent's schema
  .claude/settings.local.json   the coordinator's allow-list
```

Rules:

- `Run::create` makes the subdirectories `workers`, `inbox`, `inbox/done`, `.state`, `.state/outbox`, `.claude`, then writes the first record. A second create for the same key fails: `a run for <KEY> already exists`. `src/run.rs:runs_are_created_listed_and_updated_under_the_lock`
- `Run::load` fails with `there is no run for <KEY>` when `.state/run.json` is missing. `Run::list` returns every folder with a record, sorted by key.
- The key is validated before any path is built: `TEAM-NUMBER`, team starts with an ASCII upper-case letter, then upper-case letters or digits, at most 16 characters; number 1 to 12 digits. Error: ``\`<key>\` is not a Linear issue key (expected the form TEAM-123)``. `src/run.rs:keys_are_validated_before_any_path_is_built`
- The run lock is an exclusive file lock on `.state/lock`. It is held while reading and rewriting anything under `workers/`, `inbox/` or `.state/`, and never across a Herdr, git or Linear call. In the async rewrite, take it on a blocking thread or with an async file lock; never hold it across an `.await` on I/O to Herdr, git or Linear.
- `update(change)` is a read-modify-write of the record under the lock; each step changes only the fields it owns.
- `canonical_dir` is the run folder with symlinks resolved (or the plain path when that fails). Herdr reports physical working directories and records compare against it.
- JSON and text files are written atomically (temporary file and rename, no leftover). `tests/files:slugs_quotes_and_atomic_writes`

Run record (`run.json`), every field defaulted when missing:

| Field | Meaning |
| --- | --- |
| `issue_id`, `identifier`, `title`, `url`, `team_key` | the issue |
| `labels` | label names, for the routing rules |
| `session_id` | the Agent Session; empty until one is opened |
| `status` | `active`, `detached` or `closed` |
| `created` | claim time |
| `issue_updated_at` | the issue's `updatedAt` when `issue.md` was last written |
| `issue_hash` | hash of the parts a person edits |
| `size`, `size_source` | size and where it came from: `estimate`, `label`, `agent`, `agent (timed out)`, `default` |
| `routing` | a running routing job: `pid`, `started`, `output` path; `null` otherwise |
| `coordinator` | the coordinator's agent record |
| `prompt_cursor` | prompts created after this timestamp are unread |
| `last_activity` | when an activity was last sent |
| `finished` | `finish` was accepted |
| `external_urls` | list of `{label, url}` |
| `timeout_since` | start of the run timeout window |
| `timeout_asked` | the timeout question is open |
| `coordinator_lost` | the coordinator's pane is gone and the resume question was asked |
| `stopped` | a person pressed stop; no prompt or heartbeat goes out until they reply |

Agent record (coordinator and every worker's `agent`):

| Field | Meaning |
| --- | --- |
| `status` | `pending` (not placed), `open` (placed; launched, prompted and watched), `failed` (placing or launching failed; `error` says why), `stopped` (its run ended) |
| `error`, `profile`, `kind`, `agent_name` | |
| `workspace_id`, `tab_id`, `pane_id`, `cwd` | where it runs |
| `prompt_pending` | the launch prompt is not delivered yet |
| `launch_attempts` | |
| `agent_session` | the agent's native session id, for a resume |
| `resume` | the next start resumes `agent_session` |
| `last_state`, `last_state_change` | the last Herdr status seen and when it changed |
| `last_group` | the last worker group token |
| `blocked_reported` | the "needs someone in Herdr" question was sent for this episode |

Issue snapshot `issue.md` (`issue_markdown`):

```text
# <identifier> <title>

- URL: <url>
- Team: <team name> (<team key>)
- State: <state name> (<state type>)
- Estimate: <estimate>            (only when set)
- Labels: <group/name or name>, ...  (only when any)
- Updated: <updatedAt>

## Description

<trimmed description, or "(no description)">

## Comments

(no comments)                     (when none)

### <author> at <createdAt>

<trimmed body>
```

`issue_hash` is the SHA-256 hex of `title \n description \n label names joined by "," \n comments`, each comment as `author \n createdAt \n body`, comments joined by newline. The state is not part of it.

`conversation.md` starts with `# Conversation\n\nReplies from allowed users in the issue's Agent Session, oldest first.\n` and gets one block per reply: `\n## <createdAt> (user <user id>)\n\n<trimmed body>\n`. `ignored-prompts.md` gets the same block without a header. `src/run.rs:runs_are_created_listed_and_updated_under_the_lock`, `tests/scenarios:replies_are_relayed_only_from_allowed_users_and_stop_interrupts`

## Names, agent kinds and profile arguments

Agent names (`names`):

- Coordinator: `<key>-coordinator`. Worker: `<key>-<id>`. Example: `data-123-coordinator`, `data-123-w2`.
- A valid name matches `[a-z][a-z0-9_-]{0,31}`. `Hla-x` and the empty string are invalid.
- When the lower-case form is invalid (longer than 32 characters, or the key starts with a digit), the key part becomes `i` plus the first 8 hex characters of the SHA-256 of the issue UUID. `VERYLONGTEAMKEY-123456` gives `i<8 hex>-coordinator`. `tests/names:names_are_lower_case_and_fall_back_to_a_hash`

Kinds (`agents`):

- `is_kind` accepts Herdr's agent kinds: `pi, claude, codex, gemini, cursor, devin, agy, cline, omp, mastracode, opencode, copilot, kimi, kiro, droid, amp, grok, hermes, kilo, qodercli, qwen, letta, maki, muse` (Herdr 0.9.1 `agent start --kind`). `chatgpt` is refused.
- `has_effort_flag`: true for `claude` and `codex`, false for every other kind.
- The executable Herdr starts for kind `cursor` is `cursor-agent`; for every other kind it is the kind name.

`profile_args(profile)`, in this order: model flag, effort flag, then `args` unchanged.

| Kind | Model | Effort |
| --- | --- | --- |
| `claude` | `--model <m>` | `--effort <e>` |
| `codex` | `-m <m>` | `-c model_reasoning_effort=<e>` |
| any other | `--model <m>` | none |

A profile with no model, effort or args gives no arguments. `tests/agents:profiles_become_per_kind_flags`

`resume_args(kind, session)`:

| Kind | Arguments |
| --- | --- |
| `claude` | `--resume <s>` |
| `codex` | `resume <s>` |
| `opencode` | `--session <s>` |
| `copilot` | `--resume=<s>` |
| any other | none |

An empty session id, or one starting with `-`, gives none. `tests/agents:resume_arguments_follow_herdrs_table`

## Small helpers

- `slugify(text)`: lower-case ASCII, every run of other characters becomes one `-`, no leading or trailing `-`, at most 40 characters. `Fix the $(login) bug!` gives `fix-the-login-bug`; `???` gives the empty string. `tests/files:slugs_quotes_and_atomic_writes`
- `shell_quote(text)`: unchanged when it has only safe characters (`/bin/hla` stays as is); otherwise single-quoted with each `'` written `'\''`: `/my dir/it's` gives `'/my dir/it'\''s'`. `tests/files:slugs_quotes_and_atomic_writes`
- `seconds_since(timestamp, now)`: whole seconds since an RFC 3339 timestamp; `0` when it does not parse. `tests/files:seconds_since_parses_or_is_zero`
- `now()`: the current time as RFC 3339 UTC.
- `read_text_arg(path)`: `-` reads standard input; otherwise the file.
- `OPEN_COMMAND`: `open` on macOS, `xdg-open` on Linux.

## External commands

Git, the routing agent, `open`/`xdg-open` and `herdr session list --json` run as child processes under `tokio::process`.

- The runner captures stdout, stderr and the exit code. A program that does not exist is an error. A child that runs past its deadline is killed and reported as timed out; its output keeps being drained so a chatty child cannot block the deadline. Timeouts: git 10 s, `git fetch` 60 s, `open` 10 s. `tests/runner:captures_output_and_exit_code`, `tests/runner:missing_program_is_an_error`, `tests/runner:times_out_a_chatty_child`
- Tests fake these child processes.

## Herdr access

### Version and session

- The Herdr version and protocol come from `session.snapshot` (`version`, `protocol`). The version is compared by its first `X.Y.Z`: `0.9.2-preview.3` counts as 0.9.2. `MIN_VERSION` is 0.9.1.
- The socket of the configured session comes from `herdr session list --json`: `{"sessions":[{"default","name","running","socket_path"}]}`. With no `herdr.session`, the session with `default: true`; with a name, the session of that name; an unknown name is an error. The ticker never uses the `HERDR_SOCKET_PATH` it was started with. `tests/herdr:session_sockets_come_from_herdr`
- Herdr errors carry Herdr's `code` (for example `agent_not_ready`) and `message`. `tests/herdr:errors_carry_herdrs_code_and_calls_carry_the_socket`

### Socket protocol (measured on Herdr 0.9.1, protocol 22)

- Newline-delimited JSON on the session's unix socket. Request `{"id","method","params"}`; response `{"id","result"}` or `{"id","error":{"code","message"}}`.
- The server closes a request connection after its one response. Open one connection per request.
- `events.subscribe {subscriptions:[...]}` answers `{"result":{"type":"subscription_started"}}` and then pushes `{"event":<name>,"data":{...}}` lines. There is no replay.
- A second `events.subscribe` on a subscription connection closes it without an ack. To change a subscription: open a new connection, wait for its ack, then drop the old one.
- Pushed global event names use underscores (`pane_created`, `pane_closed`, `pane_agent_detected`, `workspace_created`, `workspace_closed`) although subscription types are dotted (`pane.created`). `pane.agent_status_changed` arrives dotted. Accept both spellings for every event.
- An agent status change does not emit `pane.updated`. `pane.agent_status_changed` must be subscribed per pane with `pane_id` (omitting it is `invalid_request`); one subscribe may list many panes. Its data has `agent`, `agent_status`, `pane_id`, `workspace_id`, and no agent name or session: read those with `agent.get` or `session.snapshot`.
- Reporting `idle` after `working` shows as `done` in the status event.
- Closing a workspace's last pane emits `pane_closed` then `workspace_closed`. `workspace.close` emits `workspace_closed` only.
- `session.snapshot` returns `{type, snapshot:{agents, panes, tabs, workspaces, layouts, focused_*, protocol, version}}`. Agent fields used: `agent` (kind), `agent_session.value`, `agent_status`, `cwd`, `name`, `pane_id`, `tab_id`, `terminal_id`, `workspace_id`, `interactive_ready`, `launch_pending`.
- `herdr server stop` gives every subscription EOF and removes the socket file; connecting fails with ENOENT until the server is back. After a restart, workspaces and panes come back with the same ids, agents are gone and every status is `unknown`.
- Agent status values: `idle`, `working`, `blocked`, `done`, `unknown`.

### State view in the ticker

- Bootstrap: subscribe (workspace created/closed, pane created/closed/updated, pane agent detected, and `pane.agent_status_changed` for every known pane), wait for the ack, buffer events, take `session.snapshot` on another connection, apply it, replay the buffer. After any reconnect take a new snapshot.
- When the set of panes changes, replace the status subscription as described above.
- After a status change, re-read the agent (`agent.get` or a snapshot) when its name, cwd or session may have changed.
- The view replaces the old per-tick `agent list` and `pane list`. **Timing change:** Herdr is no longer polled.

### Requests used

| Request | Params | Used by |
| --- | --- | --- |
| `workspace.create` | `cwd`, `label`, `focus: false` | coordinator placement |
| `workspace.close` | `workspace_id` | run close, worker restart |
| `workspace.focus` | `workspace_id` | focus-run action |
| `worktree.create` | `cwd`, `branch`, `base`, `focus: false` | worker start; result has `root_pane` and `worktree.path` |
| `worktree.open` | `path`, `focus: false` | worker restart in the kept checkout |
| `agent.start` | `name`, `kind`, `pane_id`, `args` | launch |
| `agent.prompt` | `target` (pane id), `text` | launch prompt, nudge, worker prompt |
| `agent.send_keys` | `target`, `keys: ["esc"]` | stop, close, detach |
| `agent.rename` | `target`, `name` | an unnamed resumed agent |
| `notification.show` | `title`, `body` | notices |
| `pane.report_metadata` | `pane_id`, `source`, `display_agent`, `tokens`, `ttl_ms` | pane state tokens |
| `pane.current` | `caller_pane_id` | `report` |
| `session.snapshot` | none | commands and actions |

`agent.start` args follow the CLI form `agent start <name> --kind <kind> --pane <pane> -- <args...>`; the arguments after `--` go to the agent CLI unchanged. `tests/herdr:errors_carry_herdrs_code_and_calls_carry_the_socket`

### Identity of an agent

An agent in Herdr is the recorded agent only when pane id, working directory, kind and name agree. `find_agent(record, agents)`:

- Same pane, cwd, kind and name: found.
- Same pane, cwd and kind, empty name: found (a natively resumed agent lost its name). The ticker renames it to the recorded name (`agent.rename`).
- Same pane, another name, cwd or kind: not ours.
- `tests/worker:identity_is_pane_cwd_kind_and_name`

`live_state(record, view, now, state_dir, socket)` returns `Live`:

| Field | Rule |
| --- | --- |
| `pane_exists` | true when our agent is found in its pane, or when found by name, cwd and kind in another pane, or when the recorded pane exists with no agent in it (a placed pane at its shell prompt). False when the pane is gone or another agent sits in it. |
| `moved_to` | `(workspace, tab, pane)` when found by name, cwd and kind in another pane (Herdr renumbered it). |
| `agent_state` | our agent's status, or none when no agent is ours. |
| `state_secs` | seconds since `last_state_change` when the status equals `last_state`, else 0. |
| `self_report` | the progress record for this pane from this socket, ignored when its terminal id is not the pane's current terminal. |

`tests/worker:identity_is_pane_cwd_kind_and_name` (moved, foreign, `state_secs == 45`).

## The ticker process

### Operations

| Operation | Behavior |
| --- | --- |
| `ticker start` (also `startup`, and every agent-facing subcommand) | Starts the ticker detached unless one of this version runs. Creates nothing when no config exists. |
| `ticker stop` | Asks the running ticker to exit and waits until the lock is free. With a free lock, removes a stale stop file. |
| `ticker status` | Prints `describe`. |
| `ticker run` | Runs the loop in the foreground; the detached process runs this. |

### Rules

- `decide_start(lock, my_version, stop_file_exists)`: lock free gives `Spawn`; held by the same version with no stop file gives `Nothing`; held by another version gives `StopThenSpawn`; held by the same version while a stop file exists (a stop in progress) gives `StopThenSpawn`. `tests/ticker:start_decisions`
- `start` with no config returns successfully and does not create the state directory. `tests/ticker:start_creates_nothing_without_a_config`
- The lock is an exclusive `try_lock` on `<state_dir>/ticker.lock`. The holder writes `{"version":"<VERSION>","pid":<pid>}` into it. `lock_state` probes: free, or held with the parsed info. A child forked at the same instant may briefly share the descriptor, so callers poll until the lock is free. `describe` contains `not running` when free. `tests/ticker:lock_probe_sees_a_holder_and_its_version`
- `stop` with a free lock removes a stale stop file. `tests/ticker:stop_with_a_free_lock_removes_a_stale_stop_file`
- Spawning: a new session (`setsid`) with null stdio, running `<binary> ticker run`. The startup hook returns at once; Herdr runs at most 32 plugin commands at a time, so a hook must never stay resident.
- Version handoff: `VERSION` is the release version plus a build id, so a rebuilt binary always differs from the running one. A start from another version stops the old ticker and spawns the new one.
- The ticker exits only when the stop file exists or the configured Herdr session has been unreachable for 5 minutes. It keeps reading Linear while no run exists.
- A crashed ticker comes back on the next startup hook or the next agent-facing subcommand (`context`, `plan set`, `say`, `ask`, `finish`, `worker ...`), because each runs `ticker start` first.
- Log: `<state_dir>/ticker.log`, one line per event. Its size stays at most `LOG_CAP` and, after it is trimmed, more than `LOG_CAP / 4` remains. After 150 lines of 10,000 characters the file obeys both bounds. `tests/ticker:log_is_capped`
- Only one ticker may run per state directory. Running the same app on two machines would claim the same issue twice; the plugin does not guard against that.

### Pass structure

Each run pass runs, in this order, the parts whose inputs are ready: relay (after a run read), watch, routing results, launch (placement, start, prompt, nudge), heartbeat, inbox pruning, flush. Intake runs in the Linear task. A failure in one run is logged as `<KEY>: <error>` and never stops the others. After Herdr state changes, progress records of panes that no longer exist are pruned.

**Timing change:** passes are driven by events and timers, not by a 15 second loop. Time-based rules (30 s blocked, 60 s launch dialog, 60 s idle nudge, 20 min heartbeat, run timeout, routing timeout, 10 min write failure) must be re-evaluated by timers at least as often as they would have been at 15 s.

### Log lines

| Line | When |
| --- | --- |
| `Linear is not available: <error>; run the login action` | the credential cannot be used; logged once |
| `intake: <error>` | the delegated-issue poll failed |
| `<KEY>: picked up` | a claim |
| `<KEY>: could not pick up: <error>` | a claim failed |
| `<KEY>: could not open the session yet: <error>` | claim without a session |
| `<KEY>: could not create the session: <error>` | flush without a session |
| `<KEY>: Linear refused request <request id>: <error>` | a definitive refusal |
| `<KEY>: Linear write failed, will retry: <error>` | the queue is blocked |
| `<KEY>: closed (the issue is <state name>)` | run closed |
| `<KEY>: detached (no longer delegated)` | run detached |
| `<KEY>: could not close workspace <id>: <error>` | close failed |
| `<KEY>: <error>` | any other per-run failure |

## Linear client

The kept modules `src/linear/*` define the operations; the rewrite may make them async but keeps every query text, every rule and every error.

### Transport

- Endpoint `https://api.linear.app/graphql`. HTTPS only, no redirects, no proxy, no retries. Connect timeout 10 s, request timeout 30 s. Body `{"operationName","query","variables"}`, header `Authorization: Bearer <token>` marked sensitive.
- Response bound 4 MiB (`ResponseTooLarge` above). Exactly one `application/json` content type, parameters allowed.
- `decode(status, json, body)`: non-JSON gives `ContentType` for 200 and `HttpStatus(status)` otherwise; JSON that does not parse gives `ReadFieldsInvalid` for 200 and `HttpStatus` otherwise; a GraphQL `errors[0]` gives `Graphql(message)` preferring `extensions.userPresentableMessage`, control characters removed, at most 200 characters; then a non-200 status gives `HttpStatus`; `data` must be an object. `src/linear/transport.rs:decoding_prefers_graphql_errors_and_requires_json`
- Every read selects `viewer { id app isMe }`. A read is accepted only when `app` and `isMe` are true and `id` is non-blank and at most 4096 bytes; else `ActorIdentityMismatch`. The credential manager binds the viewer id to the stored credential; writes take a token only after that binding exists. `src/linear/transport.rs:only_an_app_viewer_is_verified`
- Errors never contain a token. Display strings: `the Linear request configuration is invalid`, `the Linear HTTPS client is unavailable`, `the Linear request failed before a response`, `the Linear response is too large`, `the Linear response content type is invalid`, `Linear answered HTTP <n>`, `Linear reported an error: <message>`, `the Linear viewer is not this plugin's app user; log in again`, `a Linear response field is missing or malformed`.
- The rewrite also reads the rate-limit headers from every response (see the next section) and surfaces them with the result.

### Operations (fixed GraphQL text in `src/linear/api.rs`)

| Operation | Name | Behavior |
| --- | --- | --- |
| `viewer` | `HlaViewer` | `{id, name}` |
| `delegated_issues(teams)` | `HlaDelegatedIssues` | issues with `delegate.isMe`, team key in the list, state type not `completed`/`canceled`; 50 per page, at most 4 pages |
| `issue(id)` | `HlaIssue` | `IssueDetail`: team states, estimation type (default `notUsed`), labels with parent group name, first 50 comments (author default `(unknown)`) |
| `open_session(issue)` | `HlaSessions`, then `HlaSessionCreate` | the newest (by `createdAt`) session of this app user on the issue whose status is not `complete`; otherwise creates one with `agentSessionCreateOnIssue` |
| `create_activity(session, id, activity)` | `HlaActivityCreate` | input `agentSessionId`, `id` (the caller's UUID), `content`, `ephemeral`, and `signal`/`signalMetadata` when set |
| `activity_exists(session, id)` | `HlaActivityFind` | whether an activity with that id exists |
| `set_plan(session, plan)` | `HlaSessionUpdate` | replaces the plan |
| `set_external_urls(session, urls)` | `HlaSessionUpdate` | replaces the whole URL list |
| `set_issue_state(issue, state)` | `HlaIssueState` | `issueUpdate` with `stateId` only |
| `run_updates(queries)` | `HlaRuns` | one request for every active run: per run `i<n>: issue(...) { updatedAt state { type name } delegate { id } }` and `s<n>: agentSession(...) { activities(first: 50, filter: prompt type, createdAt > cursor) }`; only the alias number varies; prompts sorted oldest first; an empty list sends nothing |

Writes fail with `Graphql("<payload> did not succeed")` when `success` is not true. A missing field is `ReadFieldsInvalid`. `src/linear/api.rs:reads_parse_into_typed_records`, `src/linear/api.rs:writes_and_run_updates_round_trip`, `src/linear/api.rs:a_missing_field_is_an_error`

Activity content JSON: `{"type":"thought","body"}`, `{"type":"action","action","parameter","result"?}`, `{"type":"elicitation","body"}`, `{"type":"response","body"}`, `{"type":"error","body"}`. An elicitation with choices has `signal: "select"` and `signalMetadata: {"options":[{"label","value"}...]}`.

## Linear polling and rate limits

Operations of the Linear task:

| Operation | Interval | Behavior |
| --- | --- | --- |
| intake poll | `linear.intake_interval_seconds` (default 5) | `delegated_issues`, then intake |
| run read | `linear.run_read_interval_seconds` (default 5) | `run_updates` for every active run with a session, then per-run handling |
| viewer | once, cached | the app user id; retried until known; run reads wait for it |
| flush | after new outbox requests and after each read | see [Outbox and flush](#outbox-and-flush) |

**Timing change:** intake was every 30 s and the run read ran every 15 s tick; both now run on these intervals (default 5 s each).

Rules:

- The Linear client is built from the Keychain credential on first use. When that fails, log the `Linear is not available` line once and try again on the next interval.
- The run read is batched. When the batch fails, read each run with its own request, so one broken run does not hide the others.
- Rate limits for an OAuth app are 5,000 requests and 2,000,000 complexity points per hour per user, and 10,000 points per query. Headers: `X-RateLimit-Requests-Limit`, `X-RateLimit-Requests-Remaining`, `X-RateLimit-Requests-Reset`, `X-RateLimit-Complexity-Limit`, `X-RateLimit-Complexity-Remaining`, `X-RateLimit-Complexity-Reset` (resets in UTC epoch milliseconds), `X-Complexity` (this query), and the endpoint variants `X-RateLimit-Endpoint-Requests-*`, `X-RateLimit-Endpoint-Name`. A rate-limited request answers HTTP 400 with a GraphQL error whose `extensions.code` is `RATELIMITED`.
- Backoff (the policy the rewrite implements):
  - A `RATELIMITED` error or HTTP 429 pauses every read and write until the latest reset time in the headers, or for an exponential delay starting at the interval and capped at 5 minutes when no reset is given.
  - When remaining requests or remaining complexity would not last until the reset at the current rate, stretch the read intervals so the remainder lasts until the reset.
  - The intake poll yields to the run read, and reads yield to writes that are due, when the budget is short.
  - `RATELIMITED` is never a definitive refusal: an outbox request that met it stays queued (today's `decode` maps any HTTP 400 GraphQL error to `Graphql`, which the outbox treats as definitive; the rewrite must not).
- At 5 s intervals with one run the plugin sends about 1,440 reads per hour, within the limit; complexity of `HlaRuns` with several runs is not measured.

## Intake and claim

### `intake(issues)`

For each delegated issue in the order returned:

1. A run exists for the key:
   - Active, with no coordinator profile decided and no routing job (a claim cut short, or an older build): read the issue and run `finish_claim`. Log errors as `<KEY>: <error>`.
   - Not active (detached or closed): set it active. When the coordinator is `stopped` (a closed run), set it `pending` with `resume = agent_session non-empty` and `launch_attempts = 0`. Queue the thought `The issue was delegated again; the run continues.` and write an inbox item (kind `issue`, subject `issue`): `The issue was delegated to this agent again; the run is active again.`
   - Otherwise nothing. The issue is never claimed twice. `tests/scenarios:a_delegated_issue_becomes_a_run_whose_coordinator_is_started_and_primed`
2. No run: skip while `<state_dir>/paused` exists. `tests/scenarios:max_runs_limits_intake_and_pause_stops_it`
3. Stop the whole intake (not only this issue) when active runs are at `max_runs`, or when the agent count plus one would exceed `max_agents`. `tests/scenarios:max_runs_limits_intake_and_pause_stops_it`
4. Claim. A failure is logged as `<KEY>: could not pick up: <error>`.

Agent count: over active runs, one for a coordinator that is `pending` or `open`, plus each worker that counts (see [Worker commands](#worker-commands)).

### `claim(issue)`

1. Validate the key; read the issue detail.
2. Create the state and runs directories. Create the run with `issue_id`, `identifier`, `title`, `url`, `team_key`, `labels`, `created = now`, `issue_updated_at`, `issue_hash`, and `prompt_cursor = last_activity = timeout_since = now`.
3. Write `issue.md`. Log `<KEY>: picked up`.
4. Queue the thought `Picked up <KEY>.`
5. `finish_claim`.

### `finish_claim(run, detail)`

1. When the run has no session: `open_session` (the session Linear created on delegation, or a new one). On failure log `<KEY>: could not open the session yet: <error>` and go on; the flush opens it later.
2. Queue an issue-state request with target `started`.
3. Route.

Rules pinned:

- In the claim pass the session gets the thoughts `Picked up DATA-1.` then ``The coordinator uses the `coordinator-light` profile (size S from the estimate).``, the issue moves to `In Progress`, the size is S from estimate 2 (fibonacci), and the coordinator is placed (status `open`) with the pane's cwd equal to `canonical_dir`. `tests/scenarios:a_delegated_issue_becomes_a_run_whose_coordinator_is_started_and_primed`
- The session Linear created on delegation is used; no second session is made. `tests/scenarios:the_session_linear_created_on_delegation_is_used`
- A claim without a session still decides the coordinator (`coordinator`, since the size is unknown and no routing agent is configured). A later poll finishes an active run whose coordinator is undecided: session opened, one session only, `Picked up DATA-1.` sent, issue `In Progress`. `tests/scenarios:a_claim_without_a_session_still_decides_its_coordinator`
- Linear shows an agent as unresponsive when a session gets no activity within 10 s of its creation. With a 5 s poll the first thought goes out within one interval of the delegation.

## Routing

### Size

- `size_from_estimate(type, estimate)`: none gives `unknown`; `0` gives XS; otherwise the position of the value in the team's scale gives the n-th size. Scales: `exponential` 1 2 4 8 16 32 64; `fibonacci` and `tShirt` 1 2 3 5 8 13 21; `linear` 1 2 3 4 5 6 7; any other type (for example `notUsed`) gives `unknown`. A value not in the scale gives `unknown`. `src/routing.rs:estimates_map_by_scale_position`
- `size_from_labels(labels, group)`: only labels whose group matches `routing.size_label_group` case-insensitively and whose name is a known size; none without a group. `src/routing.rs:size_labels_count_only_inside_the_group`
- `known_size`: the estimate first (source `estimate`), then a label (source `label`).

### Profile

`coordinator_profile(size, team, labels)`: the first rule whose non-empty conditions all hold (`sizes` contains the size; `teams` contains the team key; `labels_any` shares a label name case-insensitively) gives its `coordinator`; otherwise `routing.default`. The run's stored label names are used (no groups). `src/routing.rs:the_first_matching_rule_wins`

### Route and decide

- `route`: a known size decides now. Without `[routing.agent]`, decide `(unknown, "default")`. Otherwise start the routing agent, record the job in `run.json` (`pid`, `started`, `output`) and return; a spawn failure is logged and decides `(unknown, "agent")`.
- `decide(size, source)`: pick the profile, set `size`, `size_source`, `routing = null` and a pending coordinator record (`status pending`, `profile`, `kind`, `agent_name`), then queue the thought ``The coordinator uses the `<profile>` profile (<why>).`` with `<why>`:

| Size and source | `<why>` |
| --- | --- |
| unknown, `default` | `size unknown` |
| unknown, other source | `size unknown after the routing <source>` (for example `size unknown after the routing agent (timed out)`) |
| known, `agent` | `size <S> from the routing agent` |
| known, other source | `size <S> from the <source>` |

### Routing agent

- Command for `claude`: `claude -p [--model m] [--effort e] --tools "" --no-session-persistence --output-format json --json-schema <schema JSON> <INSTRUCTIONS>`. For `codex`: `codex exec [-m m] [-c model_reasoning_effort=e] -s read-only --skip-git-repo-check --ephemeral --output-schema <schema file> -o <output file> <INSTRUCTIONS>`. The profile's `args` are never passed. `src/routing.rs:commands_keep_the_issue_out_of_the_arguments`
- `INSTRUCTIONS` is the fixed text in `src/routing.rs`; the last argument is always that text.
- Schema: object with one required property `size`, a string enum of the 8 size names, no additional properties. It is written to `.state/routing.schema.json`.
- The child runs in `.state/`, with the ticker's `PATH`, stderr discarded, stdin `Title: <title>\n\n<description>\n`. For `claude`, stdout goes to `.state/routing.out`; for `codex`, stdout is discarded and `-o` writes that file. A child that exits before reading closes the pipe; that is not an error and the answer reads as unknown. `src/routing.rs:the_agent_gets_the_issue_on_standard_input`
- `parse_output`: take `structured_output` when it is an object, else parse the string `result` as JSON, else the whole value. The answer must be an object with exactly one key, `size`, whose value is a size name; anything else is `unknown`. `src/routing.rs:outputs_are_checked_against_the_schema`
- Collection: when the child finished, decide `(parse_output(routing.out), "agent")`. When it runs past `routing.agent.timeout_seconds` (120 when the section is absent) from `started`, kill it and decide `(unknown, "agent (timed out)")`. A job started by an earlier ticker is not a child: check it with signal 0 and kill it with SIGKILL. The job is in the record, so a ticker restart never runs the agent twice.
- With `tokio::process`, wait on the child with a timeout in its own task and hand the result to the run's pass. `tests/scenarios:the_routing_agent_decides_an_unsized_issue` (fake `claude` answers XS: size XS, source `agent`, profile `coordinator-light`).

## Reading runs: close, detach, issue edits

For every active run with a session, per update, in this order:

1. State type `completed` or `canceled`: close the run.
2. Delegate is not the app user: detach the run.
3. `updatedAt` differs from `issue_updated_at`: refresh the issue.
4. Relay the prompts.

`refresh_issue`: read the issue, rewrite `issue.md`, store `issue_updated_at`, `issue_hash`, `title`, `labels`. When the hash changed, write an inbox item (`issue`, `issue`): `The issue was edited in Linear; issue.md is updated.` A change the plugin caused (a state move) changes `updatedAt` but not the hash, so it writes no item.

`close_run`:

1. Interrupt the agents (Escape).
2. For each worker: copy its report home; when it is `open` and its pane exists, collect its workspace (the live one, or the recorded one); set it `stopped`.
3. When the coordinator is `open` and its pane exists, collect its workspace.
4. Close every collected workspace; log failures.
5. Set the run `closed` and the coordinator `stopped`. Log the close line.
6. A closed run is left alone: nothing is started for it. Reopened and still delegated, it becomes active with the coordinator resumed from its session (`--resume sess-...`). `tests/scenarios:a_completed_issue_closes_the_run_and_a_removed_delegation_detaches_it` (two workspaces closed, worker `stopped`).

`detach_run`: interrupt the agents, set the run `detached`, log. Workspaces stay. Delegated again, the run is active again. `tests/scenarios:a_completed_issue_closes_the_run_and_a_removed_delegation_detaches_it`

Checkouts and branches are never removed.

## Relay of Linear prompts

`relay(run, prompts)`, prompts oldest first. Nothing happens for an empty list. For each prompt:

- A user not in `linear.allowed_user_ids`: append to `.state/ignored-prompts.md`; nothing else. Stop signals from such users are ignored too.
- Signal `stop` from an allowed user: interrupt the agents, set `stopped = true`, queue the response `Stopped <n> agent(s) as asked. Their worktrees are kept; reply here to continue.`
- Any other allowed prompt:
  1. Append it to `conversation.md`.
  2. Write an inbox item (kind `reply`, subject `reply`): `A new reply from user <user id> is in conversation.md.`
  3. Set `stopped = false`. When `timeout_asked`, clear it and set `timeout_since = now`.
  4. When `coordinator_lost` and the trimmed body equals `resume` case-insensitively: clear `coordinator_lost`, set the coordinator `pending`, `resume = agent_session non-empty`, `launch_attempts = 0`.

After the loop, `prompt_cursor` becomes the last prompt's `createdAt`, ignored prompts included. A prompt is relayed once. `tests/scenarios:replies_are_relayed_only_from_allowed_users_and_stop_interrupts`

Interrupting sends `esc` to the coordinator's pane and to every `open` worker's pane, for each agent found by identity; the count is the number of successful sends. With a coordinator and one worker the count is 2. `tests/scenarios:replies_are_relayed_only_from_allowed_users_and_stop_interrupts`

The reply body never goes into a prompt: prompts to the coordinator are fixed lines, and the body is read from `conversation.md` by `context`. Herdr can merge a prompt with text a person is typing into the pane.

## Watching coordinators and workers

### Tracking (`track`)

For an `open` record, given `Live`:

- `moved_to` updates `workspace_id`, `tab_id`, `pane_id`.
- A found agent with an empty name is renamed to the recorded name.
- A found agent's non-empty session id is stored in `agent_session`.
- A status different from `last_state` sets `last_state` (empty when no agent) and `last_state_change = now`.
- `blocked_reported` is cleared when the agent no longer needs a person.

### Needs a person

`needs_person(record, live)` is true when:

- the status is `blocked` for at least 30 s, or
- the launch prompt is pending and the status is `unknown` for at least 60 s (a launch stuck on a dialog).

A blocked status for 29 s does not count: a quickly answered prompt never shows. `tests/worker:groups_follow_the_rows_in_order`

`ask_for_person(run, who, record)` queues an elicitation without options and shows the notification `<KEY> needs you` with the same body:

```text
<who> needs someone in Herdr: it is waiting on a dialog in pane `<pane id>` (session `<herdr.session or default>`, run <KEY> <title>). Answer it there.
```

`<KEY> <title>` is the workspace label: the key, a space, and the title without control characters, at most 60 characters. `<who>` is `The coordinator` or `Worker <id> (<repo>)`. It is sent once per episode (`blocked_reported`). `tests/scenarios:a_dialog_in_a_pane_is_reported_once_and_a_lost_coordinator_can_be_resumed`

Notifications (`notify`) are shown only when `notifications.herdr` is true.

### Coordinator

For an `open` coordinator:

1. Track it.
2. When it needs a person and was not reported: ask for a person (`The coordinator`) and set `blocked_reported`.
3. When its pane is gone and it is not already lost: queue the elicitation `The coordinator's pane for <KEY> is gone. <how>` with option `Resume`=`resume`, and show the notification `<KEY> coordinator is gone` with body `<how>`. `<how>` is `Reply \`resume\` to start it again with its previous session.` when the session id is non-empty and the kind has resume arguments, else `Reply \`resume\` to start a new coordinator.` Set `coordinator_lost`. The ticker never restarts it by itself.
4. When the pane exists: report pane metadata with display `<KEY> · coordinator` and state `needs you` (when it needs a person), else the agent status, else `starting`.
5. Save the record when it changed.

After a `resume` reply the coordinator is started again with `--resume <session>` and its launch prompt is the restart line. `tests/scenarios:a_dialog_in_a_pane_is_reported_once_and_a_lost_coordinator_can_be_resumed`

### Worker groups

`group(worker, live)`, the first row that holds:

| # | Condition | Group |
| --- | --- | --- |
| 1 | status `failed` | Waiting on you |
| 2 | pane gone, report written | Reported |
| 3 | pane gone, no report | Waiting on you |
| 4 | needs a person | Waiting on you |
| 5 | the self-report says `Waiting for you` | Waiting on you |
| 6 | status is not idle (working, short blocked, unknown, no agent yet) | Working |
| 7 | idle, report written | Reported |
| 8 | idle, no report | Idle |

"Report written" means `report_hash` is non-empty. A pending launch with no agent yet is Working. `tests/worker:groups_follow_the_rows_in_order`

| Group | Label | Token (`last_group`) |
| --- | --- | --- |
| Waiting on you | `Waiting on you` | open question |
| Working | `Working` | `working` |
| Idle | `Idle` | open question (`idle` expected) |
| Reported | `Reported` | `reported` |

The tokens are stored in worker records, so existing values must keep parsing.

### Worker watch (`watch_worker`)

For every worker that is `open` or `failed`:

1. Track it.
2. Report: when `report_hash` of the report file differs from the stored one, copy the report home, store the new hash, and read the home copy. When it has a PR line different from `pr_url`: queue the action `Pull request` with parameter `<url> (worker <id>, repo <repo>)` and no result; add `{label: "<id> <repo> PR", url}` to the run's `external_urls` once; queue the full URL list; store `pr_url`. A report written in this pass counts for the group at once.
3. Group: when its token differs from `last_group`, write an inbox item (kind `worker`, subject `<id>`) and store the token:
   - Waiting on you: `<id> (<repo>) is Waiting on you: <reason>.` with `<reason>` the first that holds: `failed: <error>`; `its pane closed before it wrote a report`; `it asked a question in its report`; `it waits on a dialog in pane <pane id>`; the agent status; `no agent`.
   - Idle: `<id> (<repo>) is idle without a report; check its pane <pane id>.`
   - Reported and Working write nothing.
4. When the group is Reported and the report hash differs from `announced_report_hash`: write `<id> (<repo>) has a new report: workers/<id>.md` and store the hash.
5. When it needs a person and was not reported: ask for a person (`Worker <id> (<repo>)`), set `blocked_reported`.
6. When it is `open`, its pane is gone, no report exists and this was not reported: queue the error activity `Worker <id> (<repo>) lost its pane before it wrote a report.` and set `gone_reported`.
7. When the pane exists: report pane metadata with display `<KEY> · <id> <title>` and state the group label.
8. Save the record when it changed, keeping its `created` and `updated`.

Pinned: the report with a PR gives the inbox summary containing `w1 (api) has a new report`, the home copy `workers/w1.md`, the actions `Start worker` then `Pull request`, the external URL `https://github.com/acme/api/pull/7`. `tests/scenarios:a_worker_runs_in_a_worktree_and_its_report_and_pr_reach_linear`. A blocked worker gives one elicitation containing its pane id, a Herdr notification, and an inbox item containing `Waiting on you`. `tests/scenarios:a_dialog_in_a_pane_is_reported_once_and_a_lost_coordinator_can_be_resumed`

### Pane metadata tokens

`pane.report_metadata` with `pane_id`, `source` (the plugin's metadata source id), `display_agent`, `tokens: {"hla_state": "<state>"}`, `ttl_ms: 300000`. Failures are ignored. All pane tokens use the `hla_` prefix. Herdr allows at most 16 tokens per report, names `^[A-Za-z0-9_-]{1,32}$`. Refresh the token before the TTL runs out while the pane exists (the old tick refreshed it every 15 s).

## Launching agents

### Coordinator placement

For an active run whose coordinator is `pending`:

1. Write the priming (it is rewritten at every placement, so an updated binary's path is what the coordinator sees):
   - `AGENTS.md`:

     ```text
     # herdr-linear-agent run <KEY>

     If your working directory is this folder, you are the coordinator of the herdr-linear-agent run for the Linear issue <KEY> (<title with newlines as spaces>).

     Run `<bin> skill <KEY>` now and follow the sheet it prints. Then run `<bin> context <KEY>` at the start of every turn.
     ```
   - `CLAUDE.md`: a symlink to `AGENTS.md`, replaced when it points elsewhere.
   - `.claude/settings.local.json`: `{"permissions":{"allow":["Bash(<bin> skill:*)", "Bash(<bin> context:*)", "Bash(<bin> inbox done:*)", "Bash(<bin> plan:*)", "Bash(<bin> say:*)", "Bash(<bin> ask:*)", "Bash(<bin> worker:*)", "Bash(<bin> finish:*)"]}}`. `startup`, `action` and `ticker` are never allowed.
   - `tests/coordinator:priming_names_the_binary_and_the_allow_list_leaves_out_plugin_commands`
2. `workspace.create` with `cwd` = `canonical_dir`, `label` = the workspace label, not focused.
3. Record `workspace_id`, `tab_id`, `pane_id`, `cwd` from the root pane; set `status open`, `prompt_pending true`, `launch_attempts 0`.
4. Placement happens in the claim pass. `tests/scenarios:a_delegated_issue_becomes_a_run_whose_coordinator_is_started_and_primed`

A placement failure counts as a launch attempt (see below).

### Start

For each `open` agent of the run with `prompt_pending` (coordinator first, then workers by id):

- Start only when the pane exists and no agent is in it (the pane is at its shell prompt). An agent already in the pane (for example one left `blocked` by `agent_not_ready`) is never started again.
- At most one start per run per pass. **Timing change:** in the rewrite a run has at most one start in flight, and the next start waits until the previous one's outcome is known.
- Arguments: `profile_args(profile)`, then `resume_args(kind, agent_session)` when `resume` is set and the arguments exist. Examples: a `coordinator-light` start ends `-- --model sonnet`; a resumed coordinator ends `--resume sess-data-1-coordinator`; a `standard` worker ends `--model sonnet --effort high --permission-mode auto`; a `deep` worker includes `model_reasoning_effort=xhigh`. `tests/scenarios:a_delegated_issue_becomes_a_run_whose_coordinator_is_started_and_primed`, `tests/scenarios:a_worker_runs_in_a_worktree_and_its_report_and_pr_reach_linear`, `tests/scenarios:restarts_switch_profiles_and_are_limited`
- Trust dialog: when `claude.auto_accept_trust_dialog` is true and the kind is `claude`, right before the start, trust the coordinator's `canonical_dir`, or the worker's worktree (its `cwd`) and its repository's main checkout (`repo_path`). Nothing is trusted when the option is off. `tests/scenarios:the_trust_dialog_is_accepted_only_when_enabled`
- `claude_trust.trust(env, dirs)`: the config is `$CLAUDE_CONFIG_DIR/.claude.json` when set, else `~/.claude.json`. A missing file is left missing; a file whose top level is not an object is left alone. For each non-empty folder, set `projects.<folder>.hasTrustDialogAccepted = true` when it is not already true, keeping every other key, the key order and the file mode (0600 when unknown). Write through `.claude.json.hla.<pid>.tmp` and a rename. Returns whether the file changed. `src/claude_trust.rs:trust_is_added_once_and_everything_else_is_kept`, `src/claude_trust.rs:a_missing_or_unexpected_config_is_left_alone`
- Outcome:
  - Success: the agent is running; the prompt is still pending.
  - Error `agent_not_ready`: the agent exists in the pane but is not ready (a trust or permission dialog); keep `open` and `prompt_pending`; the needs-a-person rules take over. `tests/scenarios:a_dialog_in_a_pane_is_reported_once_and_a_lost_coordinator_can_be_resumed`
  - Any other error: increase `launch_attempts`. After 3 attempts, set `failed` with `error` = the message and queue an error activity (text: open question).

### Launch prompt

When `prompt_pending` and our agent is in the pane and ready for input (idle or done), send the prompt and clear `prompt_pending`. It goes out once. `tests/scenarios:a_delegated_issue_becomes_a_run_whose_coordinator_is_started_and_primed`

| Agent | Prompt |
| --- | --- |
| coordinator | `[herdr-linear-agent ticker] Start <KEY>. Follow AGENTS.md.` |
| resumed coordinator | `[herdr-linear-agent ticker] You were restarted as the coordinator of <KEY>. Run context.` |
| worker | `Read .herdr-linear-agent/<KEY>-<id>/brief.md and do what it says.` |

`tests/worker:ids_branches_briefs_and_pr_lines`, `tests/scenarios:a_worker_runs_in_a_worktree_and_its_report_and_pr_reach_linear`, `tests/scenarios:a_dialog_in_a_pane_is_reported_once_and_a_lost_coordinator_can_be_resumed`

While the run is `stopped` or `timeout_asked`, no prompt goes to the coordinator. `tests/scenarios:quiet_runs_get_a_heartbeat_and_long_runs_ask_to_continue`, `tests/scenarios:a_stop_holds_prompts_until_the_next_reply`

## Nudge

- Constants: `NUDGE_REPLY` = `[herdr-linear-agent ticker] There is a new reply in Linear. Run context.`; `NUDGE_INBOX` = `[herdr-linear-agent ticker] There are new inbox items. Run context.`
- Condition: the coordinator is `open`, not lost, its launch prompt is delivered, its status is `idle` for at least 60 s (`COORDINATOR_IDLE_SECS`), the run is neither `stopped` nor `timeout_asked`, and unseen inbox items exist (unhandled and not marked seen).
- The prompt is `NUDGE_REPLY` when any unseen item has kind `reply`, else `NUDGE_INBOX`.
- One nudge per set of items: the ticker keeps, per run, a hash of the unseen item ids it last nudged about, and nudges again only when that set changes. The memory is in-process; a new ticker may nudge once more.
- Pinned: `tests/scenarios:a_worker_runs_in_a_worktree_and_its_report_and_pr_reach_linear` (inbox nudge once), `tests/scenarios:replies_are_relayed_only_from_allowed_users_and_stop_interrupts` (reply nudge), `tests/scenarios:a_stop_holds_prompts_until_the_next_reply` (no nudge while stopped; after the next reply, `NUDGE_REPLY` although a worker item is also unseen), `tests/scenarios:quiet_runs_get_a_heartbeat_and_long_runs_ask_to_continue` (no nudge while the timeout question is open).

## Heartbeat and run timeout

Skipped entirely while the run has no session or is `stopped`.

- Heartbeat: when `last_activity` is at least 20 minutes old and the outbox is empty, queue an ephemeral thought `Still on it: <summary>.` `<summary>` is `no workers`, or the counts of `open` workers by group in first-seen order, as `<n> <label in lower case>` joined by `, ` (for example `1 working, 1 waiting on you`). Linear marks a session `stale` after 30 minutes without an activity.
- Run timeout: when not `timeout_asked` and `timeout_since` is at least `run_timeout_hours * 3600` s old, queue the elicitation `This run has been going for <h> hours. Reply to let it continue; until then the coordinator gets no prompts.` with option `Continue`=`continue`, set `timeout_asked`, and show the notification `<KEY> ran <h> hours` with body `Reply in the Linear session to let it continue.`
- A reply clears `timeout_asked` and restarts the window. `tests/scenarios:quiet_runs_get_a_heartbeat_and_long_runs_ask_to_continue`

## Outbox and flush

The kept module `src/outbox.rs` defines the queue.

### Requests

- `push(run, op)`: under the run lock, increase `.state/outbox-counter.json`, write `.state/outbox/<counter as 10 digits>.json` with `{id: UUIDv4, created, attempted: false, op...}`, return the id. Requests are sent in the order written.
- Ops (`op` tag): `activity {activity}`, `plan {plan}`, `external_urls {urls}`, `issue_state {target: "started" | "review"}`.
- `pending(run)`: queued requests oldest first; a file that does not parse is moved to `outbox/failed/`.

### Send

`send(run, session, issue, review_state)` handles requests in order:

1. When `attempted` is set, check first: an activity is looked up by its id (`activity_exists`) and skipped when found; plan, URL and state requests are not checked (they are idempotent or read before writing).
2. Set `attempted`, save it, and apply.
3. Success: remove the file.
4. A definitive refusal (`Graphql`, `Configuration`, HTTP 400 to 428 or 430 to 499): move the file to `outbox/failed/` and go on. HTTP 429 and `RATELIMITED` are not definitive (see above).
5. Any other error: stop the queue; the rest waits for the next flush.

- A lost response is checked by a read and never sent twice. `src/outbox.rs:a_lost_response_is_checked_by_a_read_and_never_sent_twice`
- A failure keeps the rest in order; a refusal is set aside and the next request still goes out. `src/outbox.rs:a_failure_keeps_the_rest_in_order_and_a_refusal_is_set_aside`
- Requests go out in order and are removed. `src/outbox.rs:requests_go_out_in_order_and_are_removed`

Apply:

| Op | Behavior |
| --- | --- |
| activity | `create_activity` with the request id as the activity id |
| plan | `set_plan` |
| external_urls | `set_external_urls` with the full list |
| issue_state | read the issue; compute the target; when there is one, set it, then read the issue again and fail with `RequestFailed` when the state is not the target |

`target_state(issue, target, review_state)`: `started` gives none when the state type is `started`, `completed` or `canceled`, else the team's `started` state with the lowest position, else `Graphql("team <key> has no started state")`. `review` gives none when the state name equals the review state case-insensitively or the type is `completed` or `canceled`, else the state of that name (case-insensitive), else ``Graphql("team <key> has no state named `<review_state>`")``. `src/outbox.rs:state_targets_leave_later_states_alone`

`parse_plan(text)`: non-empty trimmed lines, each `- [m] content` or `* [m] content` with a one-character mark: space `pending`, `>` `inProgress`, `x`/`X` `completed`, `-` `canceled`. Errors: ``plan lines look like `- [ ] step`; got `<line>` ``, `` `[<m>]` is not a plan mark; use [ ], [>], [x] or [-] ``, ``a plan step is empty: `<line>` ``, `the plan has no steps`. `src/outbox.rs:plans_parse_from_a_checklist`

### Flush

For every run with queued requests, including detached and closed runs:

1. No session: `open_session`, store it; on failure log and count the flush as blocked.
2. Send. When an activity went out, set `last_activity = now`. Log refusals and a blocked queue.
3. When any run was blocked, remember since when. When Linear has accepted no write for 10 minutes, show once the notification `herdr-linear-agent` / `Linear has not accepted writes for 10 minutes. They are kept and retried; see the ticker log.` A flush without a block resets the timer and the notice.

Requests queued before the session existed wait and go out once it exists. `tests/scenarios:a_claim_without_a_session_still_decides_its_coordinator`

### Linear writes by event

| Event | Write |
| --- | --- |
| claim | session (when none), thought `Picked up <KEY>.`, issue state `started` |
| coordinator decided | thought with the profile and the reason |
| delegated again | thought `The issue was delegated again; the run continues.` |
| `plan set` | plan |
| `say` | thought |
| `ask` | elicitation, with `select` options when given |
| worker started | action `Start worker` |
| PR in a report | action `Pull request` and the URL list |
| dialog in a pane | elicitation (and notification) |
| coordinator pane gone | elicitation with `Resume` |
| worker pane gone before a report | error activity |
| launch failed 3 times | error activity |
| 20 minutes quiet | ephemeral thought |
| run timeout | elicitation with `Continue` |
| stop | response `Stopped <n> agent(s) ...` |
| `finish` | response and issue state `review` |

The plugin never merges, force-pushes, moves an issue to Done, or writes a comment.

## The inbox

Items for the coordinator in `runs/<KEY>/inbox/`.

| Operation | Behavior |
| --- | --- |
| `write(run, kind, subject, summary)` | writes an item under the run lock and returns its id, which ends with `-<kind>-<subject>-<n>` (`-worker-w1-1` for the first); ids are unique |
| `unhandled(run)` | items not yet done, oldest first; `summary` has newlines replaced by spaces (`second line`) |
| `mark_seen(run, ids)` / `seen(run)` | the ids shown to the coordinator |
| `done(run, ids, all)` | moves the named items, or all with `all`, to `inbox/done/`; returns the count |
| `prune_done(run)` | prunes `inbox/done/` (policy: open question) |

- An id containing `/`, `..`, starting with `.`, or empty is refused. `tests/inbox:hostile_ids_are_refused`
- `tests/inbox:items_are_written_listed_marked_seen_and_moved_to_done`

Kinds and subjects written by the ticker: `issue`/`issue`, `reply`/`reply`, `worker`/`<id>`.

## Worker commands

### Worker record (`workers/<id>.toml`)

Fields: `id`, `title`, `repo`, `repo_path`, `branch`, `base`, `worktree_path`, `brief_dir`, `restarts`, `report_hash`, `announced_report_hash`, `pr_url`, `gone_reported`, `created`, `updated`, `agent` (agent record; the profile name is `agent.profile`).

| Operation | Behavior |
| --- | --- |
| `allocate(run, check, init)` | under the run lock, gives the next id (`w1`, `w2`, ...), lets `check(existing workers)` refuse, applies `init`, writes the record |
| `update(run, id, change)` / `load(run, id)` / `list(run)` | read-modify-write, read, all records by id |
| `validate_id` | `w` then a positive number without leading zero: `w1`, `w12` pass; `""`, `w`, `w0`, `w01`, `x1`, `../w1`, `w1a` fail |
| `branch_name(KEY, id, title)` | `herdr-linear-agent/<key>/<id>-<slug(title)>`, or `herdr-linear-agent/<key>/<id>` when the slug is empty |
| `brief_dir(worktree, KEY, id)` | `<worktree without trailing slash>/.herdr-linear-agent/<KEY>-<id>` |
| `task_path(run, id)` | `workers/<id>.task.md` |
| `append_follow_up(run, id, text)` | adds `## Follow-ups` once, then `### <timestamp>` and the trimmed text |
| `home_report_path(run, id)` | `workers/<id>.md` |
| `report_hash(worker)` | SHA-256 of `<brief_dir>/report.md`; none when missing or a symbolic link |
| `copy_report_home(run, worker)` | copies the report to `workers/<id>.md`; `None` when there is no report; never reads through a symbolic link |
| `pr_line(report)` | the URL from the first line `PR: https://github.com/<owner>/<repo>/pull/<digits>`; any other host, path or trailing text gives none |

`tests/worker:ids_branches_briefs_and_pr_lines`, `tests/worker:allocation_follow_ups_and_report_copies`

A worker counts against `max_agents` when its status is `pending` or `open`.

### `worker start <KEY> --repo --profile --title --task-file`

1. `ticker start`; load the config; load the run; it must be active, else ``run <KEY> is <Status>; nothing more is done for it`` (`Active`, `Detached`, `Closed`).
2. Checks, in this order: the repository is in the catalog; the profile is a worker profile (`not a worker profile`); no other worker of the run uses the repository (message contains `one worker per repository`); the run has fewer than `max_workers_per_run` workers (message contains `max_workers_per_run`); the agent count leaves room under `max_agents`. `tests/scenarios:finish_waits_for_every_worker_and_limits_hold`
3. Allocate the id. Branch: `branch_name(KEY, id, title)`.
4. `git -C <repo path> fetch origin <base>` (60 s).
5. `worktree.create` with `cwd` = repo path, `branch`, `base` = `origin/<base>`, not focused. Record `workspace_id`, `tab_id`, `pane_id`, `cwd` and `worktree_path` from the result.
6. Add `.herdr-linear-agent/` to the repository's shared `info/exclude` (the common git dir, so every worktree is covered), once. A failure here does not fail the command. `tests/commands:exclude_is_added_once_to_the_shared_file` (git status stays clean with files under `.herdr-linear-agent/`); the scenario World fails every `git -C` except the fetch and the command still succeeds.
7. Write `workers/<id>.task.md` and `<brief_dir>/brief.md`.
8. Set the agent `open`, `prompt_pending`, `profile`, `kind`, `agent_name = <key>-<id>`.
9. Queue the action `Start worker` with a parameter naming the repository and the profile.
10. Return the worker. The ticker starts the agent in a later pass.

`tests/scenarios:a_worker_runs_in_a_worktree_and_its_report_and_pr_reach_linear` (branch `herdr-linear-agent/data-1/w1-change-api`, `--base origin/main`, `brief.md` exists).

### Brief (`compose_brief`)

Input: issue key, title, URL, worker, task, restart flag, binary. Order in the text:

1. The heading `# Worker brief` with the issue key, title and URL, the repository, the worktree, the branch, the base and the report path `<brief_dir>/report.md`.
2. When restarting, a note that mentions `previous attempt` (read the current state of the worktree and the report before going on).
3. The rules from `assets/WORKER.md`.
4. The report command, starting `<bin> report --percent N`, with `--activity`.
5. The task text last.

`tests/worker:ids_branches_briefs_and_pr_lines`

### `worker prompt <KEY> <id> --text-file`

- Load the active run and the worker. Refuse while the worker's live status is `blocked`; the message contains `dialog` (answer the dialog in Herdr first).
- Append the text to the task file as a follow-up and deliver it to the worker's pane.
- `tests/scenarios:a_prompt_to_a_worker_is_refused_while_it_waits_on_a_dialog`

### `worker restart <KEY> <id> [--profile]`

- At most 2 restarts per worker; the third fails with a message containing `limit is 2`.
- A new profile must be a worker profile; without one the profile stays.
- Close the old workspace (the checkout stays), open the kept worktree again with `worktree.open` and `path` = `worktree_path`, record the new pane, rewrite the brief with the restart note, set `restarts += 1`, `kind` of the profile, `open`, `prompt_pending`, `launch_attempts = 0`. The ticker starts it in a later pass with the new profile's arguments.
- `tests/scenarios:restarts_switch_profiles_and_are_limited` (kind `codex`, restarts 1, one workspace closed, `previous attempt` in the brief, `model_reasoning_effort=xhigh` in the next start).

## Coordinator commands and the digest

All take `<KEY>`. `plan set`, `say`, `ask`, `finish` and the worker commands require an active run and run `ticker start` first. None reads the Keychain.

| Command | Behavior | Output |
| --- | --- | --- |
| `skill [KEY]` | prints `assets/COORDINATOR.md` with `{bin}` and `{key}` replaced (`<ISSUE-KEY>` without a key) | the sheet |
| `context <KEY>` | `ticker start`; prints the digest; marks the shown inbox ids seen; the run need not be active | the digest |
| `inbox done <KEY> [ids] [--all]` | moves items to done; error `name the inbox item ids, or pass --all` with neither | `<n> item(s) handled` |
| `plan set <KEY> --file` | parses the checklist, queues the plan | `the plan is queued for Linear` |
| `say <KEY> --text-file` | queues a thought with the trimmed text; empty: `the text is empty` | `queued for the Linear session` |
| `ask <KEY> --text-file [--option label=value]...` | queues an elicitation; options add `select`; empty: `the question is empty` | `the question is queued for the Linear session; end your turn, the answer arrives in your inbox` |
| `finish <KEY> --text-file` | see below; empty: `the summary is empty` | ``the summary is queued; the issue moves to `<review_state>` `` |

- `parse_option`: `label=value`, or a bare label used as its own value; both trimmed and non-empty, else ``an option looks like `label=value`; got `<text>` ``. `tests/commands:options_parse_as_label_and_value`
- `finish`: needs the Herdr view (error `the configured Herdr session is not reachable`). Every worker that is `open`, `failed` or `pending` must be in the Reported group, else `not finished: <id> is <Label>, ... . Every worker must have written a report and be neither working nor waiting` (exactly `not finished: ` + the list joined by `, ` + `. Every worker ...`). Then queue the response and the `review` state, and set `finished`. `tests/scenarios:finish_waits_for_every_worker_and_limits_hold` (`w1 is Working`), `tests/scenarios:a_worker_runs_in_a_worktree_and_its_report_and_pr_reach_linear` (issue `In Review`, response body).
- After `finish`, replies still reach the coordinator and `finish` may run again.

### Digest

`digest(run, config, bin, rows)` returns the text and the inbox ids it showed. It contains, in this order:

- `issue.md`;
- `conversation.md`;
- the repository catalog, one line per repository: `- <name>: <path> (base <base>) <U+2014> <description>`;
- the worker profiles only (never coordinator profiles), one line each starting `- <name>: <kind> <model>, effort <effort>`, with the description;
- the workers, one line each starting `- <id> [<group label>] <title>`;
- the unhandled inbox items, each containing `[<kind>] <subject>: <summary>` and its id.

`worker_rows(run, view)`: with a Herdr view, the group is computed live; without one (Herdr unreachable), it comes from `last_group`. `tests/coordinator:the_digest_shows_the_catalog_profiles_workers_and_inbox`, `tests/scenarios:context_shows_the_digest_and_marks_items_seen`

The coordinator sheet and the worker rules are embedded in the binary (`assets/COORDINATOR.md`, `assets/WORKER.md`); nothing is installed as a skill. The sheet contains `` `<bin> worker start <KEY> --repo `` and the other command lines. `tests/coordinator:priming_names_the_binary_and_the_allow_list_leaves_out_plugin_commands`

## Progress reports

`report [--percent N | --unknown] --activity <text>`, run by a worker in its own pane.

- `percent` above 100 is an error. `--unknown` drops the percent.
- The activity is cleaned: trimmed, control and bidirectional-override characters removed, cut to 40 display columns (a wide character counts 2: `日本語テキスト` at 6 columns gives `日本語`). `tests/progress:activity_is_cleaned_and_capped_at_forty_columns`
- Outside a Herdr pane (`HERDR_ENV` not `1`, or no `HERDR_PANE_ID` / `HERDR_SOCKET_PATH`) nothing is written and the command succeeds.
- Inside a pane: `pane.current` with the caller's pane id gives `pane_id` and `terminal_id`; save a record `{socket, pane_id, terminal_id, activity, percent, reported_at}` under `<state_dir>/progress/`, keyed by socket and pane; report pane metadata with the token `hla_activity=<activity>`. `tests/progress:report_writes_the_record_and_a_token_inside_a_pane`
- `WAITING` is `Waiting for you`; a record is waiting when its activity equals it.
- `load(state, socket, pane)`, `self_report(state, socket, pane, terminal)`: a record whose terminal differs from a non-empty current terminal is ignored. `prune(state, socket, live panes)` removes that socket's records of other panes and keeps other sockets' records. `tests/progress:records_round_trip_per_session_and_stale_terminals_are_ignored`

## Actions and doctor

Each action prints its result and shows it as a notification titled `herdr-linear-agent` in the invoking session (`HERDR_SOCKET_PATH`), or else in the configured session. A failure shows `<Action> failed: <error>` with the action's name (`Login`, `Status`, `OpenIssue`, `FocusRun`, `Pause`, `Resume`, `Doctor`) and exits non-zero.

| Action (manifest id) | Behavior | Result |
| --- | --- | --- |
| `login` | revoke a stored credential, run the OAuth login, read the viewer, `ticker start` | `Logged in to Linear as the app user <name, or id when the name is empty>.` |
| `status` | status text | see below |
| `open-issue` | the run of the invoking pane's cwd (`HERDR_PLUGIN_CONTEXT_JSON` `focused_pane_cwd`, else `workspace_cwd`); runs `open`/`xdg-open` on its URL | `Opened <KEY> in the browser.` |
| `focus-run` | the key from `HERDR_PLUGIN_CLICKED_URL`; focuses the coordinator's live workspace, else the recorded one | `Focused the run of <KEY>.` |
| `pause` | writes `<state_dir>/paused` | `Paused: new issues are not picked up. Running runs continue.` |
| `resume` | removes it, `ticker start` | `Resumed: delegated issues are picked up again.` |
| `doctor` | checks | see below |

Errors: `this action needs a pane of a run`, `this pane does not belong to a run`, `could not open <url>: <error>`, `Ctrl-click a Linear issue link to focus its run`, `<url> is not a Linear issue URL`, `there is no run for <KEY>`, `<KEY> has no coordinator workspace yet`.

- `key_from_url`: `https://linear.app/<workspace>/issue/<KEY>[/...]` with a valid key; other hosts and paths give none. `src/actions.rs:issue_urls_give_their_key`
- `run_for_cwd(cwd)`: the run whose coordinator cwd or a worker's worktree path is a path prefix of `cwd`. `src/actions.rs:status_lists_runs_and_panes_map_to_their_run`
- Status text, one line each: `describe`; `paused: new issues are not picked up` when paused; `no runs` when none; per run `<KEY> <Status>[, finished]: <coordinator>[; <workers>]`. Coordinator: `coordinator gone` (open and lost), `coordinator <last_state or starting>` (open), `coordinator pending`, `coordinator failed: <error>`, `coordinator stopped`. Workers other than stopped, joined by `, `: `<id> <failed | starting (empty last_group) | last_group> (<repo>)`. Example: `DATA-1 Active: coordinator idle; w1 working (api)`. `src/actions.rs:status_lists_runs_and_panes_map_to_their_run`
- Doctor collects problems and OK lines:
  - herdr version: `herdr <v>` OK, or `herdr <v> is older than 0.9.1`, or `herdr: <error>`;
  - config: `config <path>` OK or its error;
  - with a config: ``Herdr session `<name or default>` is reachable`` (a snapshot or ping answers), `the configured Herdr session does not answer`, or the session error; each distinct executable of the profiles' kinds and `git`: `` `<program>` found `` or `` `<program>` is not on the ticker's PATH `` (resolved from this process's `PATH`); each repository without `.git`: ``repository `<name>`: <path> is not a git checkout``; credential: `Linear credential stored` (ready or refresh needed), else `Linear credential is <status>; run the login action` or `Linear credential: <error>`;
  - ticker: `ticker running` (same version), `the ticker runs <v>, this binary is <v>`, `the ticker is not running`;
  - `<n> active run(s)` OK.
  - Text: `All checks passed.` or `<n> problem(s):\n- <problem>\n- ...`, then `\nOK:\n- <ok>\n- ...`.
- The manifest names only existing actions; `command[2]` equals the action id; every link handler has a non-empty title (Herdr 0.9.1 refuses one without). `src/cli.rs:the_manifest_names_only_existing_actions`

## Command line

`herdr-linear-agent` (clap, `--version` prints `VERSION`):

```text
startup
action <login|status|open-issue|focus-run|pause|resume|doctor>
ticker <start|stop|status|run>
skill [KEY]
context KEY
inbox done KEY [IDS...] [--all]
plan set KEY --file <path|->
say KEY --text-file <path|->
ask KEY --text-file <path|-> [--option label=value]...
finish KEY --text-file <path|->
worker start KEY --repo R --profile P --title T --task-file <path|->
worker prompt KEY ID --text-file <path|->
worker restart KEY ID [--profile P]
report [--percent N | --unknown] --activity TEXT
```

- `--percent` conflicts with `--unknown`. `action logout` does not exist. `src/cli.rs:the_command_line_is_consistent`
- Errors print `herdr-linear-agent: <error chain>` on stderr and exit 1.

## Install script and build version

- `herdr-plugin.toml`: id and name `herdr-linear-agent`, `min_herdr_version = "0.9.1"`, platforms macOS and Linux, build `sh scripts/install.sh`, startup `target/release/herdr-linear-agent startup`, the seven actions, and the link handler `linear-issue` (pattern `^https://linear\.app/[^/]+/issue/[A-Z][A-Z0-9]*-[0-9]+`, action `focus-run`, title `herdr-linear-agent: focus the run of this issue`).
- `scripts/install.sh` downloads the release binary for `.release-version` and the host target (`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`), asset `herdr-linear-agent-<target>` with `herdr-linear-agent-<target>.sha256`, into `target/release/herdr-linear-agent`. Without a release binary it runs `cargo build --release --locked`.
- `VERSION` = `<HLA_RELEASE_VERSION>+<HLA_BUILD_ID>`: the release version (from `.release-version`) plus a build id (short git hash and build time), so a rebuilt binary always differs from the one a running ticker came from. The release workflow checks that the binary reports the tag's version.
- The versions in `Cargo.toml` and `herdr-plugin.toml` are not bumped by releases.
- Linux needs a Secret Service provider and `xdg-open`. The Keychain / Secret Service service is `dev.herdr-linear-agent.linear.oauth.v1`. OAuth uses the authorization code flow with PKCE, `actor=app`, scopes `read write app:assignable`, callback `http://127.0.0.1:<callback_port>/oauth/callback`; Linear reports the granted scope space-separated.

## Porting the scenario tests

- The World's fake Herdr is a trait-level fake, not a socket server. It holds the same model (panes, agents, prompts, keys, closed workspaces, notifications, starts, a one-shot `start_error`), records each request, and produces the `HerdrView` the reconciler reads. The socket protocol is tested separately in `src/herdr/`.
- `agent_not_ready` in the fake leaves a `blocked` agent in the pane and returns the error.
- Git: every `git -C` fails with 128 `not a git repository`, except `fetch origin`, which succeeds.
- The fake Linear (`src/linear/api.rs` `fake`) is kept, with its clock that stamps each activity after the present.
- `World::tick` becomes "run the ticker until quiescent" with both Linear intervals due. Assertions that count ticks become assertions on the order of effects (placement, then one start, then one prompt).
- Ages are simulated by rewriting recorded timestamps (`last_state_change`, `last_activity`, `timeout_since`), so time rules must read those fields, not only in-memory timers.

## Decisions for the questions the inputs left open

The inputs did not pin these. Each line is the rule the rewrite follows. A rule marked **Kept** repeats today's behavior; the others are new decisions.

1. `last_group` stores `waiting_on_you`, `working`, `reported` or `idle`. An unknown stored value reads as none, so the next group is always a transition.
2. **Kept:** the stop file is `ticker.stop` and the log is `ticker.log`, capped at 1 MB. When a write would pass the cap, the older half of the file is dropped at a line boundary. A running ticker is described as `ticker <version> running since <started> (pid <pid>)`.
3. Progress records use the metadata `source` `herdr-linear-agent`. **Kept:** a record is `state/progress/<pane>-<hash>.json` and sets the pane token `hla_activity` with a 300 s TTL. The record holds `pane`, `terminal`, `percent` (or null for `--unknown`), `activity` and `at`. The percent is not sent as a token.
4. An inbox item is `inbox/<id>.json` with the fields `id`, `kind`, `created`, `worker` (optional) and `body`. Ids are `i<n>` from a counter kept under the run lock. `inbox done` moves an item to `inbox/done/`. Done items older than 7 days are pruned. The digest's headings are `## Issue`, `## Conversation`, `## Repositories`, `## Worker profiles`, `## Workers` and `## Inbox`. A worker line is `<id> <repo> <group>`, followed by `PR <url>` when one is known. An inbox line starts with its id.
5. The `Start worker` action is queued when a worker's launch prompt is delivered, not when `worker start` returns. Its parameter is `<id> <repo>: <title>`.
6. A launch attempt is one failed placement or one failed `agent.start`, including `agent_not_ready`. After 3 attempts the agent is `Failed` with the last error. The ticker then posts the error activity `Could not start the <role> agent: <error>`, where the role is `coordinator` or `worker <id>`. `agent.start` gets `timeout_ms` 30000.
7. `worker prompt` sends the given text itself with `agent.prompt`. It fails with `worker <id> is not running` when the worker is not `open` or has no pane. It is refused while the worker waits on a dialog, as the tests pin.
8. A restart resets `last_group`, `report_hash`, `gone_reported`, `blocked_reported`, `error` and `launch_attempts`, and sets `prompt_pending`. `announced_report_hash` and `pr_url` are kept. A `failed` worker whose worktree was never created is placed again from its base.
9. When `git fetch` fails, `worker start` fails with `git fetch failed: <stderr first line>` and writes no worker record. The `max_agents` refusal is `the limit of <n> agents is reached`.
10. Only workers that are not `stopped` count for one worker per repository and for `max_workers_per_run`.
11. A `Waiting for you` self-report counts only while Herdr does not show the agent `working`, and only while the report is younger than 5 minutes.
12. Herdr's `done` is the same as `idle` for groups, nudges and prompt readiness.
13. While a run is `stopped`, every prompt the ticker would send is held, both to the coordinator and to workers. `timeout_asked` holds only the coordinator's nudges.
14. For `codex`, the resume words `resume <id>` come first and the profile flags follow them.
15. `extend_path` appends these folders when they exist and are not already on `PATH`, in this order: `~/.local/bin`, `~/.cargo/bin`, `~/.local/share/mise/shims`, `/opt/homebrew/bin`, `/usr/local/bin`, `/home/linuxbrew/.linuxbrew/bin`.
16. `shell_quote` leaves a word made only of ASCII letters, digits and `/._-` bare, and single-quotes everything else. An empty word becomes `''`.
17. `scripts/install.sh` downloads the asset and its `.sha256` and refuses to install on a mismatch. **Kept:** the build version is the contents of `.release-version`, a `+`, and a build id made from the git short hash and the build time, so a rebuilt binary always differs from the running one.
18. The interval keys are `linear.intake_interval_seconds` and `linear.run_read_interval_seconds`, both defaulting to 5. Backoff starts when fewer than 20% of the requests or of the complexity points remain. The read intervals are then stretched so that the reads until the reset use at most the remainder minus 10%, which is kept for writes.
19. Run records, worker records and outbox files written by the current build must stay readable, so a ticker can be upgraded in place. Inbox items and progress records in another format are skipped, with one log line each.
20. The batched run read is split so that each query's measured `X-Complexity` stays under 5,000, half the per-query limit. The first read of a batch measures the cost per run, and later batches use that measurement. The result is recorded in `docs/verification.md` #8.
21. The PR is the first line of the report that starts with `PR:`. It counts only when it names a GitHub pull request URL.
