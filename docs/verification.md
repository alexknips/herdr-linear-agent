# Verification with a real Linear workspace

The test suite runs the ticker and the coordinator's commands against a fake Herdr and an in-memory fake of the Linear API. The questions below depend on how Linear and the agent CLIs behave for real. Check them with a scratch Linear team and a scratch Herdr session before relying on the plugin, and record the results here.

The first integration test ran on 2026-09-26 and 2026-09-27 with Herdr 0.9.1 and Claude Code 2.1.280 on macOS, in a scratch Linear team and a scratch Herdr session, with a sandbox GitHub repository. The second ran on 2026-09-28 with the same versions, after the rewrite on tokio (the event-driven ticker on the Herdr socket and the Linear task with its rate-limit budget).

## Open questions

| # | Question | Result |
| --- | --- | --- |
| 1 | When a person delegates an issue to an app without a webhook, does Linear create an Agent Session by itself? | **Checked.** Without the "Agent session events" webhook category, Linear refuses every session ("Agent sessions are not enabled for this application"); with it, Linear creates the session on delegation. The plugin uses that session and creates one only when none is open. |
| 2 | Does an `elicitation` activity notify the person who delegated the issue? | Not checked yet. A Herdr notification is also shown when `notifications.herdr` is on. |
| 3 | Can an activity created with our `id` be read back with `activities(filter: { id: { eq } })`? | Not exercised: no write lost its response during the test. |
| 4 | Does `agentSessionUpdate` accept a list for `plan`? | **Checked.** Linear accepts a list of `{content, status}`. |
| 5 | How should Claude Code's trust dialog in a new worktree be handled? | **Checked.** Every new run folder shows the dialog; a worktree does not when its repository's main checkout is already trusted. The plugin detects a dialog and asks for someone in Herdr, and with `claude.auto_accept_trust_dialog = true` it accepts the dialog ahead of time, so no run stopped at it. |
| 6 | Does Codex read `AGENTS.md` in a run folder that is not a git repository, and how do its approvals behave? | Not checked yet; approvals come from the profile's `args`. |
| 7 | Does the Keychain ask for confirmation when the ticker, started by the startup hook, reads the token? After a rebuild? | **Checked.** In the first test no confirmation appeared after rebuilds at the same binary path. In the second, each of three rebuilds of the rewrite at that path showed the macOS dialog once, until someone chose to allow it. While the dialog waits, the ticker's first Linear read holds the credential lock, so `action doctor` waits too. |
| 8 | What is the complexity of the batched run read (`HlaRuns`) with several runs? | **Checked.** 180 points per run, from `X-Complexity`, with one and with two runs in the batch (the cost per run did not change when the second run joined). The split rule therefore reads up to 27 runs per query, under the 5,000-point half of the 10,000-point limit. A read at the 5 s default costs about 1,440 requests an hour; the doctor showed `4999/5000 requests, 1999998/2000000 points`. |
| 9 | Does a worker start building before the worktree setup plugin finishes? | Not checked: no setup plugin was installed. |
| 10 | What does `Issue.estimate` return for a team with T-shirt estimates? | Not checked: the test issues had no estimate. |
| 11 | Where does `claude -p --output-format json --json-schema` put the schema-checked JSON? | **Checked.** In `structured_output`, and as text in `result`. One routing call with haiku cost about $0.24, mostly Claude Code's own cached context. |
| 12 | Does Linear report the granted scope with commas or spaces? | **Checked.** Spaces: `"app:assignable read write"`. |
| 13 | Does the Secret Service store work on a Linux desktop and in a headless session (for example over SSH, where no keyring runs)? | Not checked yet. |

## Flows checked end to end

| Flow | Issues | Result |
| --- | --- | --- |
| Pick up, route, coordinator, worker, PR, `finish` | THLA-2, THLA-3 | The issue moved to In Progress within seconds of the delegation and to In Review on `finish`; the PR was attached to the session and linked by Linear's GitHub integration. |
| Question and reply | THLA-4 | The coordinator asked in the session; the reply reached `conversation.md` and the coordinator, which then started the worker. |
| Stop | THLA-6 | Both agents were interrupted and the "Stopped" response was posted; the worktree and branch stayed. |
| Close | THLA-2 to THLA-5 | When an issue became Done, its run and workspaces were closed; branches and worktrees stayed. |

## Flows checked after the rewrite

| Flow | Issues | Result |
| --- | --- | --- |
| Pick up | THLA-7, THLA-9, THLA-10 | Picked up 1.5 to 3 s after the issue was created; THLA-7 was In Progress 3 s after creation, within Linear's 10 s. |
| Question and reply | THLA-7 | The coordinator asked about an SSH agent that needed approval; the reply reached it and it started the worker. |
| Worker, PR, `finish` | THLA-7, THLA-9, THLA-10 | PRs #6, #7 and #8 of the sandbox were attached to their sessions and `finish` moved each issue to In Review. |
| Limits | THLA-9, THLA-10 | With `max_agents = 3`, THLA-10's worker was refused until THLA-9 closed, then started. |
| Worker restart | THLA-10 | After the fix below, `worker restart` reopened the kept worktree and the run finished. |
| Stop | THLA-8 | `Stopped 2 agent(s) as asked...` was posted, the worktree and branch stayed, and no prompt reached the agents until a reply. |
| Issue edits | THLA-11 | After #42, an agent activity (04:15:21) and a reply in the session (04:15:59) wrote no "issue was edited" item, while a person's plain comment on the issue (04:17:06) wrote one; `issue.md` listed all of them. A reply in the session did not move the issue's `updatedAt`; the activity and the plain comment did. |
| Close | THLA-7 to THLA-11 | Done or Canceled closed the run and its workspaces within about 4 s. |

## Bugs the test found

| Bug | Fix |
| --- | --- |
| Herdr refused the manifest: a link handler needs a `title`. | #22 |
| Login always failed: Linear's space-separated scope did not parse. | #23 |
| No session could be created without the webhook category, and a claim that failed there left the run without a coordinator. | #24 |
| After a stop, an interrupted worker without a report became Idle and the coordinator was nudged back to work. | #28 |
| A freshly started coordinator was judged lost: Herdr reports an agent it is still launching without a kind, and reports directories with symlinks resolved. | #37 |
| `inbox done --all` moved an item the coordinator had not been shown yet (a worker's report). | #39 |
| A worker idle before its launch prompt reached it was reported idle without a report and restarted; the restart then failed because `worktree.open` was sent without the checkout as `cwd`. | #40 |

## Routing agent context (2026-09-28)

The routing agent picks a coordinator from a closed list, and the binary re-checks the answer, so its context is cut for cost, speed and a steadier choice, not for safety. Each registered kind was run the same way: the fixed instruction, a 3-line issue on standard input, and a schema with 3 candidates. Each call ran 3 times with only the minimal headless flags (baseline) and 3 times with the recipe in `src/routing.rs` (reduced), in a fresh empty folder, with the real HOME, config dirs and environment. All twelve calls picked the same candidate.

| Kind | Variant | Input tokens (avg) | Cost (avg) | Time (avg) |
| --- | --- | --- | --- | --- |
| `claude` (Claude Code 2.1.280, haiku) | baseline | 32,058 | $0.0410 | 15.1 s |
| | reduced | 1,280 | $0.0033 | 5.7 s |
| `codex` (codex-cli 0.156.1, gpt-5.6-luna) | baseline | 17,711 | not reported (ChatGPT login) | 9.4 s |
| | reduced | 1,505 | not reported | 7.3 s |

About 6 s of every codex call is fixed start-up time. The baseline's cost varies with prompt caching: the first claude baseline call wrote 32,051 cache tokens and cost $0.071.

Marker test. The markers were in the parent of the working directory, in folders the calls could otherwise see: `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`, `.claude/rules`, `.claude/skills`, `.claude/agents`, `.claude/commands`, a `.claude/settings.json` with hooks and a project MCP server; for Codex, `.git`, `AGENTS.md`, a skill, and a project `.codex/config.toml` with an MCP server, a `developer_instructions` marker and hooks. The user's real config was not written.

| Kind | Baseline saw | Reduced saw |
| --- | --- | --- |
| `claude` | the parent's `CLAUDE.md`, `CLAUDE.local.md` and rule markers; its hooks and MCP server ran; the user's real skills and agent types | nothing: no marker, no hook or MCP side effect, no skill |
| `codex` | the project config, skill, `AGENTS.md` and both hook markers; its tools and the marker MCP server | nothing |

Tools: asked to run `ls -la /` and read `../CLAUDE.md`, the reduced Claude Code call answered that only its structured-output tool exists, and the reduced Codex call could not run a command. The baselines tried and were denied, or ran them. The working directory held only the recipe's files after each reduced call; neither reduced call wrote a session file.

What remains is listed in README.md ("Routing agent kinds"). The items that could only be tested by writing to the user's config (Claude Code's `~/.claude/CLAUDE.md`, Codex's `~/.codex/AGENTS.md`) are marked there from the docs; the Codex one always loads, and it is empty on this machine.

In the scratch Linear team (THLA-12, a documentation-only issue, candidates `coordinator` and `coordinator-docs`, the routing agent on `claude` haiku), the ticker picked the issue up 4.8 s after it was created, the routing agent chose `coordinator-docs` (recorded as `chosen by the routing agent`), the run's `AGENTS.md` carried that profile's instructions after the pointer to the sheet, and no routing folder was left in the temporary directory.

The binary's own recipes ran once each against the real CLIs (`cargo test -- --ignored routing_live`): `claude` picked a candidate in 4.6 s and `codex` in 6.5 s, and the temporary folders were gone afterwards.

`opencode` 2.0.15 was measured on 2026-09-29, after it was logged in, the same way (`openai/gpt-6-luna#low`, 3 runs each): baseline 14,049 input tokens and 2.7 s on average, reduced 487 tokens and 2.4 s; cost is not reported with the ChatGPT login; all six calls picked the same candidate. With markers in a git parent (`AGENTS.md`, an `opencode.json` with an MCP server, skills under `.opencode`, `.claude` and `.agents`, an agent and a command), the baseline read the project `AGENTS.md`, started the MCP server, and listed the project skills and the user's real skills with about 25 tools; the reduced call saw none of them and, asked to run `ls` or read a file, answered that it had no such tool. The global layer (`~/.config/opencode`) was not written to and stays loaded, as the README says. A reduced call leaves no session behind: the plugin deletes it (the count of session rows in OpenCode's database did not change across a call). The binary's own recipe picked a candidate in 2.2 s.

`cursor` (Cursor Agent 2026.09.26, `grok-4.7-low`) was measured on 2026-09-29, after it was logged in. The baseline (`-p --trust --output-format json --model`, all text on standard input) averaged 16,560 prompt tokens and 11.9 s; the recipe in `src/routing.rs` averaged 12,163 tokens and 12.3 s, and all calls picked the same candidate. Tool definitions (3.8k tokens) and subagents go; Cursor's system prompt (about 2k), the account's user rules (about 2.3k), the skills under the real HOME (about 6k) and plugin MCP descriptions (about 1k) stay, because Cursor reads them from the account and the home folder, and an empty HOME loses the login. With markers in a parent folder, both variants still loaded the parent's `AGENTS.md`, `CLAUDE.md` and `.cursor/rules`, and its `sessionStart` hook ran in some calls; parent skills, agents and commands did not load, and the project MCP server never started. Asked to run a command, read and write files, the baseline read and wrote files without `--force`; the recipe answered "Tool not available" and wrote nothing. Each call leaves its chat under `~/.cursor/chats` and a folder under `~/.cursor/projects`. The binary's own recipe picked a candidate in 11.9 s.

With the profile's `env` giving each kind an empty config folder of its own (`OPENCODE_CONFIG_DIR`, or `CURSOR_CONFIG_DIR` and `CURSOR_DATA_DIR`), the binary's recipe still picked a candidate with the login intact: `opencode` in 2.4 s and `cursor` in 13.4 s (`cargo test -- --ignored routing_live`, 2026-09-29).

## Coordinators on Cursor Agent and OpenCode (2026-09-29)

In the scratch Linear team, with the routing candidates `coordinator-cursor` (`cursor`, `grok-4.7-medium`, `--trust`) and `coordinator-opencode` (`opencode`, `openai/gpt-6-luna`) whose descriptions named a title tag:

| Issue | Chosen | Result |
| --- | --- | --- |
| THLA-13 `[cursor] ...` | `coordinator-cursor`, by the routing agent | Cursor Agent (Grok 4.7 Medium) started in the run folder, ran the plugin's `skill` and `context` without an approval prompt (allowed by the run folder's `.cursor/cli.json`), and read the issue and the repository |
| THLA-14 `[opencode] ...` | `coordinator-opencode`, by the routing agent | OpenCode started as `opencode mini --model openai/gpt-6-luna`, followed `AGENTS.md`, published its plan and started worker w1 |

Both issues were canceled afterwards and their runs closed.


## Waiting for CI (2026-09-29)

What Linear returns for the workers' pull requests (THLA-2 to THLA-11 of the scratch team, read with the Linear MCP):

- The issue's attachments list each pull request with `id`, `title`, `subtitle` and `url` only. THLA-12 and THLA-14 were canceled before a worker opened one.
- The Diffs view (`get_diff`, `list_diffs`) finds every pull request by its branch `herdr-linear-agent/<key>/<id>-<title>` and gives `status`, `mergeStatus`, `reviewers` and `viewerReviewState`, but no checks. Linear's documentation says the Diffs view shows the overall check status; the MCP does not return it.
- With the plugin's own token (GraphQL, read only), each attachment has `sourceType` `github`, `source` `{"type":"github","pullRequestId":...}`, and `metadata` with `status` (`open`, `merged`), `draft`, `hasConflicts`, `reviews`, `reviewers` and `reviewerDetails`, but no checks.
- The schema has checks elsewhere: `PullRequest.checks` is a list of `PullRequestCheck` (`name`, `workflowName`, `status`, `url`, `isRequired`, `startedAt`, `completedAt`), next to `PullRequest.status`, `mergeStatus` and `hasConflicts`. No root query returns a `PullRequest`; the only ways in are `AgentSession.pullRequest`, `AgentSession.pullRequests` and `Diff.pullRequest`. With a workflow added to the scratch repository and pull request #8 (THLA-10) running three checks (passed, failed, still running on GitHub), THLA-10's agent session returned `pullRequest: null` and no `pullRequests`, although the plugin had put the pull request's URL in the session's `externalUrls`; `Query.diff` answered `Entity not found` for the id the Linear MCP gives that pull request. So the app user cannot read a worker's checks from Linear today. This is not a scope limit: Linear's OAuth scopes have none for code or pull requests, and nothing in the API links a pull request to an agent session. The session mutations (`agentSessionCreate`, `agentSessionCreateOnIssue`, `agentSessionCreateOnComment`, `agentSessionUpdate`, `agentSessionUpdateExternalUrl`, `agentSessionRestartWithDefaultModel`) take no pull request field, and `attachmentLinkGitHubPR` only adds the attachment above.
- The scratch repository has no workflows, so no pull request had checks.

The workers' Claude Code sessions (`~/.claude/projects/` for each worktree, Sonnet), from `gh pr create` to the end:

| Issue | CI calls | Calls with `sleep` | Blocking calls | Input tokens of the CI turns |
| --- | --- | --- | --- | --- |
| THLA-2, 5, 6, 7, 9 | 1 each | 1 each (5 to 15 s) | 0 | about 60,000 to 70,000 each |
| THLA-3, 4, 10, 11 | 2 each | 1 each (5 to 15 s) | 0 | about 120,000 to 126,000 each |

Every check was `sleep <n> && gh pr checks <number>` or `gh pr view --json statusCheckRollup`, answered at once with no checks. The 13 CI turns read 805,735 input tokens, 800,488 of them from the prompt cache. No worker looped, because there was nothing to wait for; each check that comes back pending adds a turn of about the same size. The worker rules now say to wait with one blocking command such as `gh pr checks <number> --watch`.

## Subagents in coordinators (2026-09-29)

Each kind was asked to start a subagent that replies `pong`, in a temporary folder, with and without the switch the README lists:

| Kind | Default | With the switch |
| --- | --- | --- |
| `claude` 2.1.280, print mode, the coordinator's `.claude/settings.local.json` | Started an Agent subagent with no permission denial, in the default and the `auto` permission mode | `--disallowed-tools=Agent`: `Task` gone from the tool list, answered that it had no such tool. The space-separated form swallowed the prompt |
| `codex` 0.156.1, `exec --ephemeral` | `spawn_agent` ran, also with `--disable multi_agent` | `-c agents.enabled=false`: no `spawn_agent`. `codex debug prompt-input` at the top level, as an interactive start reads it, lists `spawn_agent` by default and with `--disable multi_agent`, and not with `-c agents.enabled=false` |
| `cursor` 2026.09.26, print and interactive | `taskToolCall` ran, with a deny-all `.cursor/cli.json` too; `--exclude-tools task_tool_call,create_agent_tool_call` changed nothing | `--allowed-tools shell_tool_call,read_tool_call,ls_tool_call,glob_tool_call,grep_tool_call`: no subagent tool, and `echo` still ran |
| `opencode` 2.0.15, `mini` in a Herdr pane | The `subagent` tool started `general` | `"permission": {"subagent": "deny"}`, globally or in an agent chosen with `--agent`: no subagent tool |

Interactive Codex was not run past its first screen: it asks to trust every new folder, with `-a never`, with `--dangerously-bypass-approvals-and-sandbox`, and with a `-c projects."<folder>".trust_level="trusted"` override for the folder or its parent. No answer was given, so the Codex config was not changed.

## Workspace-qualified runs (2026-09-29)

With the config moved to `[workspaces.civitaspo]` and `[workspaces.civitaspo.teams.THLA]` and a build of `532c219`, in the scratch Linear team (one workspace):

| Step | Result |
| --- | --- |
| Login | `action login` stored the token under the Keychain account `civitaspo` and read the app user `herdr-agent`; `doctor` reported ``Linear `civitaspo`: credential stored`` and the workspace's budget |
| Pick up and routing | THLA-15 was picked up 2.3 s after it was created, as `civitaspo/THLA-15` in `runs/civitaspo/THLA-15`, moved to In Progress, and the routing agent chose `coordinator` |
| Coordinator | `civitaspo-thla-15-coordinator` followed `AGENTS.md` (`skill civitaspo/THLA-15`, `context civitaspo/THLA-15`) and started worker w1 with `worker start civitaspo/THLA-15` |
| Worker | `civitaspo-thla-15-w1` worked on `herdr-linear-agent/civitaspo/thla-15/w1-add-title-case-helper-with-tests`, with its brief in `.herdr-linear-agent/civitaspo-THLA-15-w1`, opened PR #10 of the sandbox and waited for its checks with one `gh pr checks 10 --watch` (the 90 s `slow` check passed) |
| Finish | the coordinator ran `finish`; the issue moved to In Review 4 min 13 s after it was created, with the PR attached |
| focus-run | the issue's URL focused the run; the same key under another organization gave `there is no run for THLA-15` |
| Close | Canceled closed the run and its workspaces 5 s later |

Then with a second workspace (its own OAuth application and app user, and a team with the same key `THLA`), named `second` in the config, and `max_runs = 2`:

| Step | Result |
| --- | --- |
| Login | `action login --workspace second` stored the second workspace's token under the account `second`; `doctor` listed both workspaces' credentials and budgets |
| Two issues at once | THLA-1 of `second` and THLA-16 of `civitaspo`, created 3 s apart, were picked up as `second/THLA-1` and `civitaspo/THLA-16` within 3 s and 6 s; each workspace's task logged its own run read cost |
| Names | the coordinators were `second-thla-1-coordinator` and `civitaspo-thla-16-coordinator`, the workers worked on `herdr-linear-agent/second/thla-1/w1-...` and `herdr-linear-agent/civitaspo/thla-16/w1-...` of the same repository, and the Herdr workspace labels started with the run keys |
| Reply | a reply from the second workspace's allowed user reached `second/THLA-1`'s `conversation.md` 5 s later; the coordinator handled it and sent the worker a follow-up, which ended up in its pull request |
| Finish | both workers waited for CI with one `--watch` and opened PRs #11 and #12; both issues moved to In Review. Only the first workspace attached its PR to the issue, since only it has a GitHub integration for the sandbox repository; both sessions carry the PR URL |
| Close | canceling both issues closed both runs and their workspaces within 4 s |

The same issue key in both workspaces at once (for example THLA-1 in each) was not run against Linear; `tests/scenarios:two_workspaces_with_the_same_issue_key_run_apart_under_their_own_team_rules` covers it.

## Profile inheritance and config reload (2026-09-29)

With a build of `8b4c4b8` and the ticker running in the default Herdr session, the profiles were rewritten without stopping it: a new `claude-base` (`kind = "claude"`, `args = ["--permission-mode", "auto"]`, an `instructions.md`), `coordinator` made `base = "claude-base"` with its own `instructions.md` and no `args`, and `--disallowed-tools=WebSearch` added to the `worker` profile's `args`.

| Step | Result |
| --- | --- |
| Reload | `config changed; restarting the ticker's tasks` was logged 1 s after the files were written, and again 4 s after they were put back at the end |
| Inheritance | THLA-17's routing agent chose `coordinator`; its `AGENTS.md` had the multi-layer section with `### From the claude-base profile` and then `### From the coordinator profile`, and the coordinator ran as `claude --model sonnet --effort medium --permission-mode auto`, the args coming from the base |
| Next worker | worker w1, started after the reload, ran as `claude --model sonnet --effort medium --permission-mode auto --disallowed-tools=WebSearch` |
| Close | Canceled closed the run 3 s later |

THLA-17 was picked up 20.5 s after it was created, where the earlier checks took 2 to 6 s; the log shows nothing in between. A reload builds the Linear clients again, which reads the Keychain again, and macOS asked before those reads of the freshly built binary; the pickup waited until the prompts were approved, as the person who approved them confirmed.

The first workspace's webhook delivery was disabled about 20 minutes before THLA-17, whose run opened its session and sent every activity with nothing left in its outbox.
