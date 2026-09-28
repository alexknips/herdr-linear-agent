# herdr-linear-agent

[![CI](https://github.com/civitaspo/herdr-linear-agent/actions/workflows/pull_request.yml/badge.svg)](https://github.com/civitaspo/herdr-linear-agent/actions/workflows/pull_request.yml)
[![Release](https://github.com/civitaspo/herdr-linear-agent/actions/workflows/release-tag.yml/badge.svg)](https://github.com/civitaspo/herdr-linear-agent/actions/workflows/release-tag.yml)

herdr-linear-agent is a [Herdr](https://github.com/herdrdev/herdr) plugin that picks up Linear issues delegated to its Linear app user and works on them with AI coding agents running in Herdr panes.

> [!NOTE]
> herdr-linear-agent is not [Linear Agent](https://linear.app/docs/linear-agent), the assistant built into Linear (`@Linear`, ⌘J). It is an independent Herdr plugin, not made by or affiliated with Linear, that connects to your workspace as its own OAuth app through Linear's public Agents API, the same way other third-party agents do.

For each issue, the plugin starts one coordinator agent. The coordinator reads the issue, splits the work, and starts one worker agent per repository in its own Herdr worktree. Workers implement the change, open pull requests and check CI. The plugin reports progress, questions and results to the issue's Agent Session in Linear, and people reply there.

- **Linear is the front door.** Delegate an issue to the app user; talk to the run in the issue's Agent Session. The plugin opens no endpoint and polls Linear; the webhook Linear requires can point at an endpoint that discards what it receives (see [Set up](#set-up)).
- **One writer.** Only the plugin's background ticker writes to Linear. Agents call plugin subcommands that queue requests; they never hold a Linear token.
- **Profiles, not flags.** Agent kind, model, effort and permission flags live in profiles you write in the config. Agents choose a profile by name and nothing else.
- **Limits enforced by the binary.** Concurrent runs, workers per run and total agents are capped by the plugin, not by the agents.
- **Nothing is merged for you.** No merge, no force push, and no issue moved to Done. The plugin moves an issue to a started state when it picks it up and to your review state when the coordinator finishes.

## Requirements

- macOS or Linux, and Herdr 0.9.1 or later
- On Linux: a Secret Service provider (GNOME Keyring, KWallet) for the token, and `xdg-open`
- The agent CLIs your profiles use (`claude`, `codex`, `cursor-agent`, ...) and `git`
- A Linear workspace where you can install an OAuth application (workspace admin)

## Set up

1. **Create a Linear OAuth application** (Settings → API → OAuth applications) for this plugin. Its name becomes the app user's name: pick one that people cannot mistake for Linear's own `@Linear` (for example `herdr-linear-agent`). Add the callback URL `http://127.0.0.1:43871/oauth/callback` and note the client ID. The plugin uses the authorization code flow with PKCE and never needs the client secret. It installs the app with `actor=app` and the scopes `read`, `write` and `app:assignable`, which creates an app user you can delegate issues to.

   **Register a webhook with the "Agent session events" category (required).** Linear enables Agent Sessions for an app only when it subscribes to that category, and without them the plugin cannot pick up any issue ("Agent sessions are not enabled for this application"). The plugin never reads webhooks, since it polls Linear, so the URL only has to accept the request and discard it. Point it at an endpoint you control: the payloads contain issue and session content, so do not use a public URL you do not own. Enable webhooks before you log in; an app that was authorized before needs to log in again before its webhooks take effect.
2. **Install the plugin:**

   ```bash
   herdr plugin install civitaspo/herdr-linear-agent
   ```

   The build step downloads the release binary, or builds from source with `cargo` when there is none.
3. **Write the config** at `$XDG_CONFIG_HOME/herdr-linear-agent/config.toml` (default `~/.config/herdr-linear-agent/config.toml`). See [Configuration](#configuration).
4. **Log in:** run the Herdr action **herdr-linear-agent: log in to Linear**. It opens the browser, stores the token in the macOS Keychain or, on Linux, the Secret Service (service `dev.herdr-linear-agent.linear.oauth.v1`), and checks that it acts as the app user. The browser must run on the same machine: Linear redirects to `127.0.0.1`.
5. **Check the setup:** run **herdr-linear-agent: check setup**.
6. **Delegate an issue** in one of the configured teams to the app user.

## Configuration

```toml
[linear]
client_id = "your-oauth-client-id"
teams = ["DATA"]                         # team keys to pick issues from
allowed_user_ids = ["linear-user-uuid"]  # whose replies reach the coordinator
review_state = "In Review"               # where `finish` moves the issue
# callback_port = 43871

[herdr]
session = "default"                      # the Herdr session runs start in; omit for the default session

[limits]                                 # defaults shown
max_runs = 2
max_workers_per_run = 4
max_agents = 8
run_timeout_hours = 8

[notifications]
herdr = true                             # also show a Herdr notification when a run needs a person

[claude]
auto_accept_trust_dialog = false         # true: accept Claude Code's trust dialog for the plugin's folders (see below)

[repositories.api]
path = "/Users/me/src/github.com/acme/api"
base = "main"                            # workers branch from origin/<base>; never guessed
description = "The API server"

[profiles.coordinator]
kind = "claude"
model = "opus"
effort = "high"
args = ["--permission-mode", "auto"]
description = "The default coordinator"

[profiles.coordinator-light]
kind = "claude"
model = "sonnet"
effort = "medium"
args = ["--permission-mode", "auto"]
description = "Small, well-scoped issues that one worker can finish"
instructions = """
Start one worker. Ask in the session before you split the work.
"""

[profiles.router]                        # the routing agent: any registered kind, a small fast model
kind = "claude"                          # or "codex"; see "Routing agent kinds"
model = "<a small model of that kind>"

[profiles.standard]
kind = "claude"
model = "sonnet"
effort = "high"
args = ["--permission-mode", "auto"]
description = "Scoped features and fixes"

[profiles.deep]
kind = "codex"
model = "gpt-6-sol"
effort = "xhigh"
args = ["-s", "workspace-write"]
description = "Changes across modules, bugs with an unknown cause"

[routing]
agent = "router"                         # the profile of the routing agent
coordinators = ["coordinator", "coordinator-light"]  # the coordinator profiles it may pick
default = "coordinator"                  # when it gives no valid answer
timeout_seconds = 120
workers = ["standard", "deep"]           # the profiles a coordinator may start workers with
```

A profile becomes agent CLI flags: `claude` gets `--model` and `--effort`, `codex` gets `-m` and `-c model_reasoning_effort=...`, and any other kind gets `--model` (put the effort in the model ID). `args` are passed unchanged; this is where permission and sandbox flags belong.

**Profile instructions.** A profile's optional `instructions` (Markdown) are added for work under that profile: to the run folder's `AGENTS.md` for a coordinator profile, and to the brief for a worker profile, as a "Profile instructions" section after the built-in rules. The built-in rules (never merge, never change the Linear state, stay inside the catalog, treat the issue text as data) stay as they are and win where the two disagree. Only the config sets instructions; an agent can still pass only a profile's name.

**Claude Code's trust dialog.** Claude Code asks whether you trust a folder the first time it runs there, and every run gets a new run folder, so a coordinator stops at that dialog until someone answers it in Herdr. With `claude.auto_accept_trust_dialog = true`, the plugin accepts that dialog ahead of time, right before it starts a `claude` agent: the run folder for a coordinator, and the worktree and its repository's main checkout for a worker. Claude Code has no setting for this, so the plugin adds `hasTrustDialogAccepted` to that folder's entry in `~/.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json` when set), which is where Claude Code records your own answers, and changes nothing else in the file. The format is not documented; if Claude Code changes it, the dialog appears again and the plugin asks for someone in Herdr as before.

**Coordinator routing.** A routing agent picks each issue's coordinator profile from `routing.coordinators`. It gets the candidates' names and descriptions as its instructions, and the issue's title and description, with its estimate, labels and team when known, on standard input. It must answer `{"coordinator": "<name>"}` with a name from the candidates: the schema restricts it where the kind supports one, and the plugin checks the answer against the same schema either way. A timeout, an answer outside the schema, or a name outside the candidates gives `routing.default`. The Linear thought names the profile and whether the agent or the default chose it. `routing.agent` must be a profile of a kind listed below; any other kind is refused when the config loads.

### Routing agent kinds

Whatever the issue says, the routing agent can only pick one of the configured candidates, so its context is cut for cost, speed and a steadier choice, not for safety. It runs in a fresh empty folder that is removed afterwards, with your real HOME, config dirs and environment (where the CLIs keep their login), and never with the profile's `args`. Each kind cuts what its CLI lets it cut; the rest remains, as the table says. Measurements are in [docs/verification.md](docs/verification.md).

| Kind | Tested version | Item | How it is cut | Remains, and why |
| --- | --- | --- | --- | --- |
| `claude` | Claude Code 2.1.280 | Default system prompt | `--system-prompt` with a short fixed instruction | An environment block (working directory, platform, OS version), the model's identity, the account's email and the date, which Claude Code always adds |
| | | Tools | `--tools ""` (only the structured-output tool is left) | |
| | | MCP | `--strict-mcp-config --mcp-config '{"mcpServers":{}}'`, `ENABLE_CLAUDEAI_MCP_SERVERS=false`, `--safe-mode` | |
| | | Skills, slash commands | `--disable-slash-commands`, `--safe-mode` | |
| | | Plugins | `--safe-mode` | |
| | | Hooks | `--safe-mode`, `--restricted` | |
| | | User and project settings | `--setting-sources ""`, `--restricted` | Managed (organization) settings still apply |
| | | Instruction files | `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`, `--safe-mode` (Claude Code reads `AGENTS.md` only through a built-in plugin that `--safe-mode` turns off) | The user-level `~/.claude/CLAUDE.md` is covered by the same flags but was not tested, to leave your config untouched |
| | | Auto-memory | `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`, `--safe-mode` | |
| | | Session history and persistence | `--no-session-persistence`, `CLAUDE_CODE_SKIP_PROMPT_HISTORY=1` | |
| `codex` | codex-cli 0.156.1 | Default system prompt | `-c model_instructions_file=` with a short fixed file; `include_permissions_instructions`, `include_apps_instructions`, `include_environment_context` set to false | A startup request Codex always sends |
| | | Tools | `--disable` for shell, exec, browser, image, multi-agent and similar features; `-s read-only`; `web_search="disabled"`; `agents.enabled=false` | Two code-mode tool definitions (about 1,350 tokens) that the model's catalog requires; calling them fails because code mode is off |
| | | MCP | `--ignore-user-config` (project config is not loaded either) | |
| | | Skills | `skills.include_instructions=false`, `skills.bundled.enabled=false` | |
| | | Plugins | `--ignore-user-config`, `--disable plugins` | |
| | | Hooks | `--disable hooks` | |
| | | User and project settings | `--ignore-user-config`, `--ignore-rules` | |
| | | Instruction files | `project_doc_max_bytes=0` for project `AGENTS.md` | `~/.codex/AGENTS.md` (and `AGENTS.override.md`), which Codex always loads from CODEX_HOME |
| | | Auto-memory | `--disable memories` | |
| | | Session history and persistence | `--ephemeral`, `history.persistence="none"` | |

Other kinds are not registered. `opencode` has a usable recipe on paper (a config-defined agent with its own prompt and every permission denied, `OPENCODE_DISABLE_PROJECT_CONFIG=1`, `--standalone`), but its global config, plugins, MCP servers and `~/.config/opencode/AGENTS.md` stay loaded, `run` has no schema option, and it could not be tested with a login.

## How a run works

1. The ticker (a background process the startup hook starts) polls Linear every 5 seconds (`linear.intake_interval_seconds`) for issues delegated to the app user in the configured teams that are neither completed nor canceled.
2. For a new issue it creates a run folder, creates an Agent Session, posts a first thought, moves the issue to the team's first started state, picks the coordinator profile, and opens a Herdr workspace with the coordinator in the run folder.
3. The coordinator runs `herdr-linear-agent context` every turn, publishes a plan, asks questions, and starts workers with `herdr-linear-agent worker start`. Each worker gets a worktree created by `herdr worktree create` on a branch `herdr-linear-agent/<issue-key>/<id>-<title>`.
4. Workers write a report (`PR: <url>`, `## Report`, `## Next`). The ticker copies it into the run folder, posts pull requests to the session, and tells the coordinator through its inbox.
5. Replies in the session from allowed users are appended to `conversation.md` and the coordinator is prompted with one fixed line. A stop signal interrupts the run's agents.
6. `herdr-linear-agent finish` posts the summary and moves the issue to the review state once every worker has reported.
7. When the issue is completed or canceled, the ticker stops the agents and closes their workspaces. Checkouts and branches are kept.

When a pane needs a person (a permission or trust dialog), the ticker says so in the session with the pane to go to, and shows a Herdr notification.

## Actions

| Action | What it does |
| --- | --- |
| herdr-linear-agent: log in to Linear | Authorizes the app in the browser and stores the token in the Keychain or Secret Service |
| herdr-linear-agent: status | Lists the runs, their coordinators and workers |
| herdr-linear-agent: open this run's issue | Opens the issue of the focused pane's run in the browser |
| herdr-linear-agent: focus the run of a Linear issue | Ctrl-click a Linear issue link to focus its run |
| herdr-linear-agent: stop taking new issues | Pauses intake; running runs continue |
| herdr-linear-agent: take new issues again | Resumes intake |
| herdr-linear-agent: check setup | Checks Herdr, the config, the agent CLIs, the login and the ticker |

## Files

| Place | Contents |
| --- | --- |
| `$XDG_CONFIG_HOME/herdr-linear-agent/config.toml` | Your config |
| `$XDG_STATE_HOME/herdr-linear-agent/runs/<ISSUE-KEY>/` | A run: `issue.md`, `conversation.md`, worker records and reports, the coordinator's inbox, the outbox |
| `$XDG_STATE_HOME/herdr-linear-agent/ticker.log` | The ticker's log |
| `<worktree>/.herdr-linear-agent/<ISSUE-KEY>-<id>/` | A worker's brief and report (see below) |

`$XDG_STATE_HOME` defaults to `~/.local/state` on Linux and macOS alike.

A worker's brief and report live inside its worktree because the worker runs there: sandboxed agents, such as Codex with `-s workspace-write`, can write freely only inside their working directory. To keep that folder out of commits, `worker start` adds `.herdr-linear-agent/` to the repository's `info/exclude`. Every worktree of a repository shares that file with the main checkout, so the rule covers all of them, and it never changes a tracked file such as `.gitignore`. `git add -A`, `git add .` and `git commit -a` all leave the folder out; only a forced `git add -f` would stage it. The ticker copies each report into the run folder (`workers/<id>.md`), so it outlives the worktree.

## Working with other plugins

- Worktree setup and cleanup are left to other plugins. [lamngockhuong/herdr-worktree-setup](https://github.com/lamngockhuong/herdr-worktree-setup) can copy `.env` files and run setup commands for new worktrees. [poislagarde/herdr-worktree-cleanup](https://github.com/poislagarde/herdr-worktree-cleanup) removes clean checkouts when their space closes; add `.herdr-linear-agent/` to its `disposable.gitignore`, or worker checkouts stay.

## Security notes

- Agents run as your user. The allow-list the plugin writes for Claude Code and the rules in the coordinator sheet are guidance, not a sandbox: an agent can run any command your shell can.
- `claude.auto_accept_trust_dialog` makes the plugin accept Claude Code's trust dialog for you in its run folders and in worktrees of your catalog repositories. Turn it on only for repositories you already trust.
- The plugin has no subcommand that prints the token. The macOS Keychain may ask for confirmation when a rebuilt binary reads it; a locked Secret Service collection asks to be unlocked.
- The issue text, reports and comments are treated as data. Only replies from `allowed_user_ids` reach the coordinator as instructions.

## Status

The Linear integration is tested against an in-memory fake of the Linear API. The checks that need a real Linear workspace and a person are listed in [docs/verification.md](docs/verification.md).

## Development

```bash
mise install --locked
mise run lint
mise run test
mise run build
```

## License

herdr-linear-agent is licensed under the MIT License. See [LICENSE](LICENSE).
