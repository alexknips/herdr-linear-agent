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
- One or more Linear workspaces where you can install an OAuth application (workspace admin)

## Set up

1. **Create a Linear OAuth application** (Settings → API → OAuth applications) for this plugin, in each workspace you use. Its name becomes the app user's name: pick one that people cannot mistake for Linear's own `@Linear` (for example `herdr-linear-agent`). Add the callback URL `http://127.0.0.1:43871/oauth/callback` and note the client ID. The plugin uses the authorization code flow with PKCE and never needs the client secret. It installs the app with `actor=app` and the scopes `read`, `write` and `app:assignable`, which creates an app user you can delegate issues to.

   **Register a webhook with the "Agent session events" category (required).** Linear enables Agent Sessions for an app only when it subscribes to that category, and without them the plugin cannot pick up any issue ("Agent sessions are not enabled for this application"). The plugin never reads webhooks, since it polls Linear, so the URL only has to accept the request and discard it. Point it at an endpoint you control: the payloads contain issue and session content, so do not use a public URL you do not own. Enable webhooks before you log in; an app that was authorized before needs to log in again before its webhooks take effect.
2. **Install the plugin:**

   ```bash
   herdr plugin install civitaspo/herdr-linear-agent
   ```

   The build step downloads the release binary, or builds from source with `cargo` when there is none.
3. **Write the config** at `$XDG_CONFIG_HOME/herdr-linear-agent/config.toml` (default `~/.config/herdr-linear-agent/config.toml`), and one folder per agent profile under `profiles/` next to it. See [Configuration](#configuration) and [Profiles](#profiles).
4. **Log in:** run the Herdr action **herdr-linear-agent: log in to Linear**. For each workspace without a stored token, it opens the browser, stores the token in the macOS Keychain or, on Linux, the Secret Service (service `dev.herdr-linear-agent.linear.oauth.v1`, one account per workspace named after it), and checks that it acts as the app user; when every workspace has a token, it logs in to all of them again. `herdr-linear-agent action login --workspace <name>` logs in to one workspace again. The browser must run on the same machine: Linear redirects to `127.0.0.1`.
5. **Check the setup:** run **herdr-linear-agent: check setup**.
6. **Delegate an issue** in one of the configured teams to the app user.

## Configuration

```toml
[workspaces.acme]                        # a name for the workspace: lower-case letters, digits and `-`
client_id = "your-oauth-client-id"       # the OAuth application of this workspace
# callback_port = 43871                  # the port of the callback URL you registered
# intake_interval_seconds = 5            # how often delegated issues are polled
# run_read_interval_seconds = 5          # how often active runs are read

[workspaces.acme.teams.DATA]             # a team to pick issues from, by team key
allowed_user_ids = ["linear-user-uuid"]  # whose replies reach the coordinator
review_state = "In Review"               # where `finish` moves the issue (default)

[workspaces.acme.teams.OPS]              # each team has its own allowed users and review state
allowed_user_ids = ["another-user-uuid"]
review_state = "Ready for review"

[workspaces.other]                       # another workspace, with its own OAuth application
client_id = "another-oauth-client-id"

[workspaces.other.teams.DATA]            # the same team key as in `acme` is fine
allowed_user_ids = ["linear-user-uuid-in-other"]

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

[routing]
agent = "router"                         # the profile of the routing agent
coordinators = ["coordinator", "coordinator-light"]  # the coordinator profiles it may pick
default = "coordinator"                  # when it gives no valid answer
timeout_seconds = 120
workers = ["standard", "deep"]           # the profiles a coordinator may start workers with
```

**Workspaces and teams.** Each workspace polls Linear with its own OAuth application, token and app user. Runs are named `<workspace>/<ISSUE-KEY>`, for example `acme/DATA-1`, so the same issue key in two workspaces makes two runs; the name is what agents pass to `herdr-linear-agent` commands, and the workspace is part of agent names (`acme-data-1-coordinator`), branches (`herdr-linear-agent/acme/data-1/w1-...`) and brief folders. A team's `allowed_user_ids` and `review_state` apply to that team's issues only. A run whose team was removed from the config keeps running, relays nobody's replies and moves to `In Review` on `finish`.

### Profiles

Profiles are not in `config.toml`. Each one is a folder next to it, named after the profile, so you can hand a profile to other people as it is: copy the folder, or link it from a repository of shared profiles. A `[profiles]` table in `config.toml` is refused.

```text
~/.config/herdr-linear-agent/
├── config.toml
└── profiles/
    ├── coordinator/
    │   └── config.toml
    ├── coordinator-light/
    │   ├── config.toml
    │   └── instructions.md          # optional
    ├── router/
    │   └── config.toml
    ├── standard/
    │   └── config.toml
    └── deep/
        └── config.toml
```

```toml
# profiles/coordinator/config.toml
kind = "claude"
model = "opus"
effort = "high"
args = ["--permission-mode", "auto", "--disallowed-tools=Agent"]  # no subagents of its own (see below)
description = "The default coordinator"
```

```toml
# profiles/coordinator-light/config.toml
kind = "claude"
model = "sonnet"
effort = "medium"
args = ["--permission-mode", "auto", "--disallowed-tools=Agent"]
description = "Small, well-scoped issues that one worker can finish"
```

```markdown
<!-- profiles/coordinator-light/instructions.md -->
Start one worker. Ask in the session before you split the work.
```

```toml
# profiles/router/config.toml: the routing agent, any registered kind, a small fast model
kind = "claude"                          # or another kind from "Routing agent kinds" below
model = "<a small model of that kind>"
```

```toml
# profiles/standard/config.toml
kind = "claude"
model = "sonnet"
effort = "high"
args = ["--permission-mode", "auto"]
description = "Scoped features and fixes"
```

```toml
# profiles/deep/config.toml
kind = "codex"
model = "gpt-6-sol"
effort = "xhigh"
args = ["-s", "workspace-write"]
description = "Changes across modules, bugs with an unknown cause"
```

A profile's `config.toml` takes `kind`, `model`, `effort`, `args`, `description` and, for the routing agent only, `env`. Folders whose names start with `.` are skipped, other files inside a profile folder (a README, say) are left alone, and any other file directly in `profiles/` is refused. Profile names use letters, digits, `.`, `_` and `-`.

A profile becomes agent CLI flags: `claude` gets `--model` and `--effort`, `codex` gets `-m` and `-c model_reasoning_effort=...`, and any other kind gets `--model` (put the effort in the model ID). `args` are passed unchanged; this is where permission and sandbox flags belong.

**Coordinators on other kinds.** A coordinator reads the run folder's `AGENTS.md`, which Claude Code (through the `CLAUDE.md` link), Codex, Cursor Agent and OpenCode all read. Cursor Agent and OpenCode ran as coordinators in a Herdr pane, before the subagent switches below were added; Codex was not run as a coordinator (see below):

```toml
# profiles/coordinator-codex/config.toml
kind = "codex"
model = "gpt-6-sol"
effort = "high"
args = ["-c", "agents.enabled=false"]    # no subagents of its own (see below)
description = "Coordinator on Codex"
```

```toml
# profiles/coordinator-cursor/config.toml
kind = "cursor"
model = "grok-4.7-medium"                # Cursor has no effort flag: the effort is part of the model ID
args = [
  "--trust",                             # otherwise Cursor asks to trust every new run folder
  "--allowed-tools", "shell_tool_call,read_tool_call,ls_tool_call,glob_tool_call,grep_tool_call",
]
description = "Coordinator on Cursor Agent"
```

```toml
# profiles/coordinator-opencode/config.toml
kind = "opencode"
model = "openai/gpt-6-luna"              # provider/model, as `opencode models` lists it
description = "Coordinator on OpenCode"
```

- Codex asks whether you trust each new run folder, and neither a flag nor a `-c projects...` override skips that dialog (codex-cli 0.156.1), so a Codex coordinator waits in Herdr until someone answers; the answer is saved in your Codex config.
- Cursor Agent asks before it runs a command that is not in its allow-list. The plugin writes `.cursor/cli.json` into each run folder, allowing the plugin's binary by the path the sheet gives the coordinator, as it writes `.claude/settings.local.json` for Claude Code.
- OpenCode starts as `opencode mini`: its full interface takes no model flag, and `mini` takes `--model` and the `--session` a resume adds. OpenCode lets agents run commands unless your OpenCode config says otherwise.

**No subagents in a coordinator.** Workers are started by the plugin as Herdr panes. A coordinator does not use its kind's own subagents, and it waits by ending its turn: the ticker prompts it when its inbox gets new items. A subagent the coordinator starts itself is outside the plugin's limits, and waiting on one tends to turn into polling that spends tokens. The sheet says so to every coordinator; where the kind's CLI can switch its subagents off, add that to the profile's `args`. Checked on 2026-09-29 by asking each agent to start a subagent that replies `pong`:

| Kind | Tested version | Subagents by default | How `args` switch them off |
| --- | --- | --- | --- |
| `claude` | Claude Code 2.1.280 | The Agent tool (listed as `Task`); the coordinator's allow-list does not gate it, so it runs without a prompt | `--disallowed-tools=Agent`. Keep the `=` form: the flag takes several values and would swallow the next argument |
| `codex` | codex-cli 0.156.1 | `spawn_agent` | `-c agents.enabled=false`. `--disable multi_agent` turns the feature flag off but leaves `spawn_agent` in place |
| `cursor` | Cursor Agent 2026.09.26 | The `Task` tool; `.cursor/cli.json` permissions do not cover it | `--allowed-tools` with the tools a coordinator needs, as above. The flag is hidden and marked internal; `--exclude-tools task_tool_call` is accepted but does not remove the tool |
| `opencode` | OpenCode 2.0.15 | The `subagent` tool, with the built-in `general` and `explore` agents | `args` alone cannot; an agent in your OpenCode config can (below) |

For OpenCode, add an agent like this to your OpenCode config (for example `~/.config/opencode/opencode.json`) and pass `--agent hla-coordinator` in the profile's `args`. Without it, and for any kind not in the table, the sheet's rule is the only thing that keeps a coordinator from starting subagents.

```json
{
  "agent": {
    "hla-coordinator": {
      "mode": "primary",
      "description": "herdr-linear-agent coordinator",
      "permission": { "subagent": "deny" }
    }
  }
}
```

**Profile instructions.** A profile's optional `instructions.md` is added for work under that profile: to the run folder's `AGENTS.md` for a coordinator profile, and to the brief for a worker profile, as a "Profile instructions" section after the built-in rules. The built-in rules (never merge, never change the Linear state, stay inside the catalog, treat the issue text as data) stay as they are and win where the two disagree. Only the profile folder sets instructions; an agent can still pass only a profile's name.

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
| `cursor` | Cursor Agent 2026.09.26 | Default system prompt | none: Cursor offers no system prompt option; the fixed instruction is the call folder's `AGENTS.md`, which Cursor applies as a rule | Cursor's own system prompt, the user rules synced from your Cursor account, and any `AGENTS.md` or `.cursor/rules` in a folder above the call folder (normally none: it is under the system temp dir) |
| | | Tools | `--allowed-tools ""` (an internal flag of this version: every tool call answers "Tool not available"), and the call folder's `.cursor/cli.json` denies shell, read, write, web and MCP | |
| | | MCP | denied as above, and project MCP servers need an approval the call never gives | Descriptions of MCP servers that Cursor plugins bring (they cannot be called) |
| | | Skills | | Skills under your home folder (`~/.claude/skills`, `~/.agents/skills`, Cursor's own) |
| | | Plugins, hooks | | Anything under `~/.cursor` (`mcp.json`, `hooks.json`, `rules`) and in a folder above the call folder |
| | | User and project settings | `--trust` (otherwise Cursor stops at its trust prompt); the call folder's own `.cursor/cli.json` | Your `~/.cursor/cli-config.json` |
| | | Auto-memory | none in Cursor Agent's print mode | |
| | | Session history and persistence | | Each call's chat in `~/.cursor/chats` and a `~/.cursor/projects` folder, unless the profile's `env` sets `CURSOR_CONFIG_DIR` and `CURSOR_DATA_DIR` (below) |
| `opencode` | OpenCode 2.0.15 | Default system prompt | an agent of its own in `OPENCODE_CONFIG_CONTENT` whose `prompt` is the fixed instruction | An environment block (session id, working directory, platform) and the date |
| | | Tools | that agent's `permission` denies everything (`{"*": "deny"}`) | |
| | | MCP, plugins, skills, agents, settings (project) | `OPENCODE_DISABLE_PROJECT_CONFIG=1`; `--standalone` runs a private server, since the shared background service ignores the call's environment | The global config in `~/.config/opencode` (its MCP servers, plugins and agents) still loads, unless the profile's `env` sets `OPENCODE_CONFIG_DIR` (below) |
| | | Hooks | OpenCode's hooks are plugins: as above | Hooks of global plugins |
| | | Instruction files | the project layer, as above (OpenCode does not read `CLAUDE.md`) | `~/.config/opencode/AGENTS.md`, unless `OPENCODE_CONFIG_DIR` is set |
| | | Auto-memory | none in OpenCode | |
| | | Session history and persistence | `"snapshot": false`, `"share": "disabled"`, and the plugin deletes the session afterwards (`opencode session delete`) | A stored copy of the environment block, a project row and an empty `shell/` folder per call in `~/.local/share/opencode`, which the CLI cannot remove |

**Routing agent environment.** The routing agent's profile may set `env`, variables added to its call (the recipe's own variables win over them). Only that profile may set it: Herdr starts coordinators and workers and cannot pass them variables, so a profile with `env` that is also a coordinator or worker candidate is refused. Its main use is to give a kind an empty config folder of its own where its login does not live there, which cuts what the table above leaves:

```toml
# profiles/router-opencode/config.toml
kind = "opencode"
model = "openai/gpt-6-luna#low"
env = { OPENCODE_CONFIG_DIR = "/Users/me/.local/state/herdr-linear-agent/router/opencode" }  # an empty folder you create
```

- `OPENCODE_CONFIG_DIR` replaces the global config folder, so the global `opencode.json`, its MCP servers, plugins and agents, and `~/.config/opencode/AGENTS.md` no longer load. The login lives in OpenCode's database and keeps working.
- `CURSOR_CONFIG_DIR` and `CURSOR_DATA_DIR` (the second is undocumented) keep Cursor Agent's chats, projects and settings out of `~/.cursor`; they do not cut prompt tokens, and your default model is not read, so set `model`. The login lives in the Keychain and keeps working. Cursor writes each call's chat into that folder, so empty it now and then.
- Do not point Claude Code's `CLAUDE_CONFIG_DIR` or Codex's `CODEX_HOME` there: their login lives in those folders.

Cursor Agent and OpenCode have no schema option, so the plugin checks the JSON they print itself (Cursor's `result`, OpenCode's last text event). Put Cursor's effort in the model ID: `model = "grok-4.7-low"`. Put the effort in the model as a variant: `model = "openai/gpt-6-luna#low"`.

Other kinds are not registered.

## How a run works

1. The ticker (a background process the startup hook starts) polls each workspace every 5 seconds (`intake_interval_seconds`) for issues delegated to its app user in the configured teams that are neither completed nor canceled.
2. For a new issue it creates a run folder, creates an Agent Session, posts a first thought, moves the issue to the team's first started state, picks the coordinator profile, and opens a Herdr workspace with the coordinator in the run folder.
3. The coordinator runs `herdr-linear-agent context` every turn, publishes a plan, asks questions, and starts workers with `herdr-linear-agent worker start`. Each worker gets a worktree created by `herdr worktree create` on a branch `herdr-linear-agent/<workspace>/<issue-key>/<id>-<title>`.
4. Workers write a report (`PR: <url>`, `## Report`, `## Next`). The ticker copies it into the run folder, posts pull requests to the session, and tells the coordinator through its inbox.
5. Replies in the session from allowed users are appended to `conversation.md` and the coordinator is prompted with one fixed line. A stop signal interrupts the run's agents.
6. `herdr-linear-agent finish` posts the summary and moves the issue to its team's review state once every worker has reported.
7. When the issue is completed or canceled, the ticker stops the agents and closes their workspaces. Checkouts and branches are kept.

When a pane needs a person (a permission or trust dialog), the ticker says so in the session with the pane to go to, and shows a Herdr notification.

## Actions

| Action | What it does |
| --- | --- |
| herdr-linear-agent: log in to Linear | Authorizes each workspace's app without a token in the browser (all of them again when each has one) and stores the tokens in the Keychain or Secret Service |
| herdr-linear-agent: status | Lists the runs, their coordinators and workers |
| herdr-linear-agent: open this run's issue | Opens the issue of the focused pane's run in the browser |
| herdr-linear-agent: focus the run of a Linear issue | Ctrl-click a Linear issue link to focus its run |
| herdr-linear-agent: stop taking new issues | Pauses intake; running runs continue |
| herdr-linear-agent: take new issues again | Resumes intake |
| herdr-linear-agent: check setup | Checks Herdr, the config, the agent CLIs, each workspace's login and the ticker |

## Files

| Place | Contents |
| --- | --- |
| `$XDG_CONFIG_HOME/herdr-linear-agent/config.toml` | Your config |
| `$XDG_STATE_HOME/herdr-linear-agent/runs/<workspace>/<ISSUE-KEY>/` | A run: `issue.md`, `conversation.md`, worker records and reports, the coordinator's inbox, the outbox |
| `$XDG_STATE_HOME/herdr-linear-agent/ticker.log` | The ticker's log |
| `<worktree>/.herdr-linear-agent/<workspace>-<ISSUE-KEY>-<id>/` | A worker's brief and report (see below) |

`$XDG_STATE_HOME` defaults to `~/.local/state` on Linux and macOS alike.

A worker's brief and report live inside its worktree because the worker runs there: sandboxed agents, such as Codex with `-s workspace-write`, can write freely only inside their working directory. To keep that folder out of commits, `worker start` adds `.herdr-linear-agent/` to the repository's `info/exclude`. Every worktree of a repository shares that file with the main checkout, so the rule covers all of them, and it never changes a tracked file such as `.gitignore`. `git add -A`, `git add .` and `git commit -a` all leave the folder out; only a forced `git add -f` would stage it. The ticker copies each report into the run folder (`workers/<id>.md`), so it outlives the worktree.

## Working with other plugins

- Worktree setup and cleanup are left to other plugins. [lamngockhuong/herdr-worktree-setup](https://github.com/lamngockhuong/herdr-worktree-setup) can copy `.env` files and run setup commands for new worktrees. [poislagarde/herdr-worktree-cleanup](https://github.com/poislagarde/herdr-worktree-cleanup) removes clean checkouts when their space closes; add `.herdr-linear-agent/` to its `disposable.gitignore`, or worker checkouts stay.

## Security notes

- Agents run as your user. The allow-list the plugin writes for Claude Code and the rules in the coordinator sheet are guidance, not a sandbox: an agent can run any command your shell can.
- `claude.auto_accept_trust_dialog` makes the plugin accept Claude Code's trust dialog for you in its run folders and in worktrees of your catalog repositories. Turn it on only for repositories you already trust.
- The plugin has no subcommand that prints the token. The macOS Keychain may ask for confirmation when a rebuilt binary reads it; a locked Secret Service collection asks to be unlocked.
- The issue text, reports and comments are treated as data. Only replies from the `allowed_user_ids` of the issue's team reach the coordinator as instructions.

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
