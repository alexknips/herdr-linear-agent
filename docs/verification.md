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

The second workspace's app then had its webhook delivery disabled too (the webhook and its "Agent session events" category kept) before `action login --workspace second` revoked its credential and authorized it again. THLA-2 of that workspace, delegated at 15:09:02, was picked up 4 s later with its Agent Session opened, moved to In Progress and had every activity sent; a coordinator was chosen and started. During the new login the running ticker logged `second: intake: Linear OAuth access is not ready` once and went on with the new token without a restart. So Agent Sessions need the webhook's category, not its delivery.

## Closing response and manual reload (2026-09-30)

With a build of `f4ea69d`, THLA-18 was canceled right after it was picked up: the run closed 3 s later, the closing response went out at once (the run's last activity moved to the close, with nothing left in its outbox), where THLA-17 and the second workspace's THLA-2, canceled before `finish` on earlier builds, had kept their sessions showing "Working".

With a build of `13c7fb2` in the default Herdr session:

| Step | Result |
| --- | --- |
| Change notice | a comment line added to the `worker` profile's `config.toml` was noticed within 1 s (`config changed; run the reload action to apply it`), and nothing was reloaded |
| Reload action | `action reload` answered `Reloaded the config: the ticker's tasks start again with it.` and the ticker logged `config reloaded; restarting the ticker's tasks` |
| Back again | removing the line gave a new notice 1 s later and a second reload applied it |
| After the reloads | `doctor` read both workspaces' credentials and budgets |

## Allowed delegators (2026-09-30)

Before the change, Linear's API was read to find who delegated an issue:

| Source | Result |
| --- | --- |
| `AgentSession.creator` (`issue.agentSessions.nodes[].creator { id name }`) | the person who delegated when the session opened: every session of the civitaspo app (THLA-3 to THLA-18) had the user's own ID, `6b7b1cde-f3cc-4886-8339-660f4851e476` |
| `AgentSession.creator` of an issue the app user created and delegated to itself | `null`: the second workspace's THLA-1 and THLA-2, created with that app's token |
| `AgentSession.creator` of an issue made through Linear's Slack integration | the person who asked in Slack: the second workspace's THLA-3, made from a Slack DM, has the user's ID there, `40bb3469-bb61-49b4-b444-cf9ebd0b1fc1`, and `sourceMetadata` `{ type: "integration", subType: "slack", aiMetadata: { source: "intake", invokedByUserId: <the same ID> } }` |
| `IssueHistory` (`actor`, `fromDelegate`, `toDelegate`) | an entry for each delegation made after the issue was created, with the person as `actor`. There is none for a delegation made as the issue was created, which is why THLA-3 to THLA-18 show none |
| workspace-wide `agentSessions(first: 50)` | holds other apps' sessions too: in the second workspace, 2 of the 50 were this app's. `Issue.agentSessions`, marked internal, gives an issue's own sessions |

With a build of `a6e5452` (#71) in the default Herdr session:

| Step | Result |
| --- | --- |
| Allowed delegator | THLA-19, delegated by the user at 04:41:32, was picked up 2 s later with Linear's session (creator the user) stored at the claim; its coordinator read the issue, called `finish`, and the issue went to In Review |
| Delegator not allowed | with `allowed_delegator_ids` set to a placeholder ID for the THLA team and the config reloaded, THLA-20, delegated by the user at 04:42:10, got no run 5 s later, while THLA-19 held the only run allowed by `max_runs = 1`. The ticker logged `civitaspo/THLA-20: not picked up: delegated by civi@hey.com (6b7b1cde-f3cc-4886-8339-660f4851e476), who is not in allowed_delegator_ids of team THLA`, and its session got one response, `This agent does not take issues delegated by this user. Ask someone allowed to delegate it.`, which made it `complete` |
| Once | after more polls and a ticker restart, the log line appeared once and the session still had one activity |
| Slack | the second workspace's THLA-3, made and delegated through Slack while THLA-19 held the only run, was picked up the moment THLA-19 was canceled, in the session Linear opened from Slack |

Delegating again, on the same build:

| Step | Result |
| --- | --- |
| Canceled issue | taking the delegation off THLA-19 and giving it back added two history entries (`fromDelegate`, then `toDelegate`, actor the user) and no session; the only session stayed `complete` |
| Open issue | THLA-19 moved to Todo restarted its closed run in its old session; taking the delegation off detached it, and giving it back again added no session either (the old one stayed `active`) |

So the creator of the newest session names whoever delegated first, and a closed run that someone else delegated again would have started again. #73 takes the delegator from the history. Its live check also found that a session's `endedAt` and `updatedAt` stay at its first completion: THLA-19's stayed at 05:00:30 after a closing response at 05:04:54 and a decline at 05:05:08.

With a build of #73:

| Step | Result |
| --- | --- |
| Delegated again by someone not allowed | with the placeholder ID allowed again, THLA-19's run was closed (canceled), its delegation taken off and given back, and the issue moved to Todo. The ticker logged `civitaspo/THLA-19: not picked up: delegated by civi@hey.com (6b7b1cde-f3cc-4886-8339-660f4851e476), who is not in allowed_delegator_ids of team THLA`; the run stayed closed, and the session got the decline after its closing response |
| Once | after a restart on the final build, which tells an answered decline by the session's latest activity, no second decline went out |
| Query | `HlaDelegatedIssues` with the sessions' latest activity and the history cost 116 complexity points with one delegated issue |

The config was put back afterwards, and THLA-19 and THLA-20 were canceled.

The Herdr notification, shown with `notification_show`, was not checked by eye; Herdr's CLI lists no past notifications.

## Past runs (2026-09-30)

Before the change, read-only:

| Question | Finding |
| --- | --- |
| Can an action open a pane that runs the binary's TUI? | Yes. Herdr 0.9.1's plugin manifest declares `[[panes]]` entrypoints, and the `plugin.pane.open` request (`plugin_id`, `entrypoint`, optional `placement` `overlay`, `popup`, `split`, `tab` or `zoomed`) opens one; the default placement is an overlay over the focused pane |
| Is `agent_session` recorded? | No: it is empty in every coordinator and worker record, since Herdr reports a session id only with its agent integrations, which are not installed. The agent's folder (`cwd`) is recorded |
| `claude` | `~/.claude/projects/<cwd with each non-alphanumeric byte as ->/<session id>.jsonl`. THLA-19's run folder had two, one per start of its coordinator, and THLA-17's worker worktree one. THLA-15, THLA-16 and the second workspace's THLA-1 and THLA-2 have a folder with only `memory/` left, so their transcripts are gone |
| `codex` | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, the folder in the first line (`session_meta.payload.cwd`), then `response_item` records: `message` (user, assistant, developer), `function_call`/`custom_tool_call` and their `*_output`, `reasoning`. No run of the scratch team used Codex, so only its record shapes were checked |
| `cursor` | `~/.cursor/projects/<cwd's alphanumeric runs joined by ->/agent-transcripts/<id>/<id>.jsonl`, found for the old THLA-13 coordinator: user and assistant text and `tool_use`, without results |
| `opencode` | a SQLite database, `~/.local/share/opencode/opencode.db` (`session_v2.directory` holds the folder); not read, to keep a SQLite library out |

With a build of the branch in the default Herdr session:

| Step | Result |
| --- | --- |
| Action | `herdr plugin action invoke herdr-linear-agent.browse` opened the `herdr-linear-agent: past runs` pane over the focused pane; the action exited 0 without a notification |
| List | the eight runs of both workspaces, newest first (second/THLA-3 at 08:25, then civitaspo/THLA-19 at 05:04, …), with the PR of each run that has one, and the preview of the selected run |
| Filter | typing `THLA-19` left THLA-19 first, with THLA-18 after it, and the preview followed the selection once the text cache was keyed by run (a first build kept the previous run's preview) |
| Coordinator transcript | Enter on THLA-19 listed its two coordinator sessions (`393e265d`, `6843b742`); the first showed the launch prompt, each `Bash` call of the plugin's `skill`, `context` and `say` commands with the first lines of their output, and the coordinator's text |
| Worker transcript | THLA-17's `w1` showed the worker reading its brief; `r resume` was offered, since the worker is stopped and its worktree is there (not pressed: it would start the agent) |
| Gone | THLA-16 showed `coordinator  claude  no transcript` and `w1  claude  no transcript`, with `No transcript is left in …` and the run folder's records |
| Quit | `q` closed the pane |

## Recorded agent sessions and kept transcripts (2026-09-30)

Why THLA-15's transcripts were gone, read-only:

- `~/.claude/settings.json` sets no `cleanupPeriodDays`, so Claude Code's default of 30 days applies; `~/.claude/.last-cleanup` says a cleanup ran at 2026-09-30T13:41:31Z.
- THLA-15, THLA-16 and the second workspace's THLA-1 and THLA-2 (runs of 13:55Z to 15:09Z on 2026-09-29) have a project folder with only `memory/`, and their session folders under `~/.claude/session-env/` (for example `0f77631b-…` and `08ab2ce8-…`, made at THLA-15's start) are still there; THLA-17 (14:52Z), THLA-18 and THLA-19 kept their `.jsonl` files. So the files did not go by age, and none is in the Trash.
- The cause is not found. Whatever it is, the agent CLIs' own files cannot be relied on, which is why the run folder now keeps a copy.

The CLIs, in scratch folders with harmless prompts:

| Kind | Finding |
| --- | --- |
| `claude` 2.1.280 | `claude -p --session-id <uuid> …` wrote `~/.claude/projects/<folder>/<uuid>.jsonl`; `claude -p --resume <uuid> …` answered from the first turn and wrote to the same file. The folder name comes from the physical path (`/private/tmp/…` for `/tmp/…`) |
| `codex` 0.156.1 | no option to choose the session id. After `codex exec …`, the rollout whose `session_meta.payload.cwd` was the folder and that was written after the start gave `payload.id`; `codex exec resume <id> …` answered from the first turn in the same rollout |
| `opencode` 2.0.15 | `opencode session list --format json`, run in the folder, gives `[{id, created, updated, projectId, directory}]`, newest first, `created` in milliseconds. `opencode session export <id>` gives `{info, messages}`: `user` messages with `text`, `assistant` messages with `content` parts of type `text` and `tool` (`name`, `state` with `status`, `input` and `content`), and `idle` entries |
| `cursor-agent` | `create-chat` returns a new chat id, and `--resume <id>` runs in it; its transcript is `~/.cursor/projects/<folder>/agent-transcripts/<id>/<id>.jsonl`. So a session can also be found after the start by the folder made then; `--resume <id>` is its resume form |

With a build of the branch in the default Herdr session, THLA-21 (a coordinator and a worker, both `claude`; Herdr reports no session ids):

| Step | Result |
| --- | --- |
| Recorded sessions | the coordinator started with `--session-id fc0cd56e-…` and worker `w1` with `--session-id 62e463e4-…`; both ids were in the records, with `started_at`, and Claude wrote `fc0cd56e-….jsonl` in the run folder's project folder and `62e463e4-….jsonl` in the worktree's |
| Resume | closing the coordinator's workspace made the ticker ask ``The coordinator's pane for civitaspo/THLA-21 is gone. Reply `resume` to start it again with its previous session.``; after the reply `resume` it ran `claude --model sonnet --effort medium --permission-mode auto --resume fc0cd56e-…`, `agent_session` stayed the same, and the same transcript file grew from 48 to 56 lines with no new file |
| Kept transcripts | the worker opened civitaspo/testing-herdr-linear-agent#13; when the issue was canceled, the run closed and `.state/transcripts/coordinator/fc0cd56e-….jsonl` (82 lines) and `.state/transcripts/w1/62e463e4-….jsonl` (110 lines) appeared, as long as the originals |
| History | the browser listed `coordinator  claude  … fc0cd56e  copy` and `w1  claude  … 62e463e4  copy`, one line each, and showed `The run folder's copy, <path>` above each transcript |

No Codex or OpenCode profile was set up for a run; their session lookup and the OpenCode export were checked with the CLIs above and with fixtures in the tests.

## History colors (2026-09-30)

With a build of the branch, the browser opened on THLA-21's transcripts drew only these SGR codes, as `herdr pane read --format ansi` reports them: reset, bold, reversed, and the foreground palette entries 2 to 8 (`38;5;2` … `38;5;8`). No RGB (`38;2;…`) and no entry above 15, so the colors come from the Herdr theme's 16-color palette.

## Postmortems (2026-10-01)

Read-only, before the change: Linear's schema has `commentCreate(input: { id, issueId, body, … })`, `issueAddLabel(id, labelId)` and `comments(filter: { id })`. For THLA-21, `issue.team.labels` listed the workspace's labels too (`Bug`, `Feature`, `Improvement`).

With a build of the branch in the default Herdr session, the THLA team given the method `review` (`profile = "router"`, Claude Haiku; `labels = ["Improvement"]`; a short method in `instructions.md`):

| Step | Result |
| --- | --- |
| Interim | THLA-22's coordinator called `finish` 56 s after the pick-up; the ticker logged `wrote the interim postmortem` at once, kept `.state/postmortems/<time>-interim.json`, and herdr-agent's comment `**Postmortem (interim)**, method `review` version `9e649254e084`` with the summary was on the issue 3 s later, with the label `Improvement` |
| Final | canceling the issue closed the run; 16 s later `wrote the final postmortem` was logged and a second comment, `**Postmortem (final)** …`, was posted; the outbox was empty afterwards |
| Content | both summaries said the coordinator had not replied, because `conversation.md` was empty: the agent did not know that the agents reply through `say`, `ask` and `finish` in the Agent Session. The fixed frame of the instructions now says so |

After the frame named `say`, `ask` and `finish` (a build of `f03b1ed`), THLA-23, the same kind of issue, got an interim summary 56 s after the pick-up that said the coordinator read the issue, found no change was needed and finished in 40 s, and no label; after the cancel, a final one 10 s after the close that said the run went well. So the replies are no longer missed. The final one called the canceled run completed, since its input said only "completed or canceled": the run now keeps the state it closed in (`closed_state`), and the final input names it (`final: the issue is Canceled, so the run is closed`).

With a build of `2ba09e0`, THLA-24's run kept `closed_state = "Canceled"`, but its final summary began "THLA-24 live check completed successfully." and did not say the issue was canceled: the state was in the input, and the summary's wording was left to the agent. The frame now asks every summary to begin with how the run stands. With that build, THLA-25's interim summary began with the stage (`interim: the coordinator called `finish`; people may still ask for changes.`, close to the input's words), and its final one, 37 s after the cancel, began `**Status:** Verification run closed—issue Canceled.`

## Routing per team (2026-10-01)

The config was moved to the new format: `[routing.default]` with the routing agent `router` (Claude Haiku), `coordinators = ["coordinator", "coordinator-docs"]`, `postmortems = ["review", "review-brief"]` and `workers = ["worker"]`; both THLA teams name it with `routing = "default"`, and `run_timeout_hours` became `limits.ask_to_continue_after_hours`. The method of the old `postmortems/review` became the profile `profiles/review` ("Reviews runs that changed code or opened pull requests"), and `profiles/review-brief` ("Reviews runs that ended with no change") was added. The ticker of the previous build logged that the new config does not load and kept its own.

With a build of the branch in the default Herdr session, THLA-26 asked for no change:

| Step | Result |
| --- | --- |
| Coordinator | picked up 6 s after the delegation reached the ticker; the record kept `coordinator` with `routing_source = "chosen by the routing agent"` |
| Interim | the coordinator called `finish` about 60 s later; the routing agent read the brief and picked `review-brief`, logged as `wrote the interim postmortem with the `review-brief` profile (chosen by the routing agent)`; the comment `**Postmortem (interim)**, method `review-brief` version `04a55ce8444c`` was posted, and the kept JSON had `picked = "chosen by the routing agent"` |
| Final | canceling the issue closed the run; 21 s later the routing agent picked again, `review-brief` wrote the final postmortem, and its summary began `Closed as Canceled.` |

A routing with one candidate each, which asks no routing agent, was checked with the scenario tests only.

## Spelled-out names (2026-10-01)

With a build of the branch in the default Herdr session, THLA-27 asked for one worker on the testing repository:

| Step | Result |
| --- | --- |
| Coordinator pane | `herdr pane get` showed `"tokens": {"herdr_linear_agent_state": "working"}`, and `done` after `finish` |
| Worker pane | the worker's pane showed `"tokens": {"herdr_linear_agent_activity": "Opening PR", "herdr_linear_agent_state": "Working"}`; it opened civitaspo/testing-herdr-linear-agent#14 |
| Temporary folders | a watch of the ticker's temp dir (every 0.3 s) saw, for the interim postmortem, `herdr-linear-agent-routing-f40pLj` at 08:49:57, then `herdr-linear-agent-postmortem-9Y2ykb` from 08:50:02 until 08:50:20, and nothing after; for the final one, `herdr-linear-agent-routing-HRo0hS` at 08:50:45 and `herdr-linear-agent-postmortem-HH6bL9` until 08:51:32. No `hla-*` folder appeared |
| Postmortems | the routing agent picked `review` both times; both comments were posted |

The worker's first start failed: `git fetch` in the testing repository needed the 1Password SSH agent, which was locked. The coordinator asked in the Agent Session, and started the worker after the reply.

## Progress in Linear (2026-10-02)

With a build of the branch in the default Herdr session, THLA-29 asked for one worker on the testing repository. It finished in 8 minutes with civitaspo/testing-herdr-linear-agent#15.

| Step | Result |
| --- | --- |
| Coordinator notes | following the new sheet, it posted the approach and why (`Plan: one worker in `testing` (opencode-luna, since this is a small change)…`), and before it ended its turn `Started w1 in `testing`. Waiting for its PR and CI result.` Both were seen in the session |
| Worker activities | the worker reported `Inspecting repository`, `Implementation tested`, `Preparing pull request`, `Pushing branch`, `Opening pull request`, `PR opened`, `Waiting for PR checks` and `CI passed`; the worker record kept `activity = "CI passed"`, and the run's 28 queued Linear writes all went out (the outbox was empty) |
| Report | the `Worker report` action was in the session after `Pull request`, but Linear showed its result as code and the bullets joined into one line were hard to read. The report is now a thought with the `## Report` section as written, which Linear renders as Markdown |
| Heartbeat | not seen: the run never had 10 quiet minutes. `a_workers_activity_goes_to_linear_once_and_the_heartbeat_says_how_long` covers it |

After the fix, THLA-30 had one worker list the testing repository's files without a change. Its report reached the session as the thought `w1 (testing) reported:` with the `## Report` section, and Linear rendered its lines and bullets as Markdown.

The first attempt, THLA-28, was picked up by the installed release build, whose startup hook had replaced the branch's ticker, and its catalog had no `testing` repository; it was canceled.

## Native input while progress is reported (2026-10-02)

The baseline at `43ec69c` was checked in Personal Linear (`civitaspo`) on [CIV-10](https://linear.app/civitaspo/issue/CIV-10/test-native-agent-input-while-a-worker-reports), using the `hla-test` Herdr session and only the `testing` repository. Temporary config and state folders kept the test ticker separate from the installed ticker. The CIV team was used because the installed ticker already polls THLA; this avoided two binaries claiming the same test issue.

After the coordinator asked a question with choices, Linear displayed **Input needed to continue** and the choices. The test ticker was stopped, only its local `last_activity` timestamp was moved eleven minutes back, and the same binary was restarted. Its heartbeat **Still on it: no workers.** changed the session to **Working…**, removed the choices, and changed the composer to **Queue message**. Submitting a reply left it queued with **Send immediately (steer)**. Clicking that control delivered the reply, which appeared once in the local conversation. This reproduces the reported native input failure without waiting ten minutes.

The first worker start failed because the agent's shell did not retain the temporary Herdr binary override. The test instructions were corrected to prefix plugin commands with the temporary config, state and Herdr binary explicitly. This was a test setup failure; the heartbeat reproduction above did not depend on a worker. After a native reply requested retry with that prefix, the worker completed a read-only inspection. Its local Markdown report also appeared as a thought after the new question, returning Linear to **Working…** and **Queue message**. The completion instruction was sent with the native immediate-send shortcut. No repository files were edited or pushed.

Linear [documents](https://linear.app/developers/agent-interaction) that the last emitted activity determines session state. The regression fixtures therefore model elicitation as `awaitingInput`, thought/action as `active`, and response as `complete`, rather than treating every non-response activity as active.

The fixed build was then checked on [CIV-11](https://linear.app/civitaspo/issue/CIV-11/verify-native-input-survives-worker-reports-and-ticker-restart) in the same isolated setup. A worker wrote its read-only report locally after the coordinator's question. Linear retained **Input needed to continue** and both choices. The ticker was stopped, the test's heartbeat timestamp was moved eleven minutes back and its timeout timestamp nine hours back, then the same fixed binary was restarted. The question and choices remained; no heartbeat or additional timeout question replaced them. The synthetic timeout timestamp was restored before testing replies. Selecting **Keep waiting** and **Continue with selected option** produced a native `wait` prompt, saved once in `conversation.md`, and cleared the persisted wait without **Send immediately (steer)**.

## OpenCode process recovery (2026-10-02)

Personal Linear (`civitaspo`) [CIV-12](https://linear.app/civitaspo/issue/CIV-12/verify-bounded-opencode-native-recovery-in-testing) uses the `hla-test` Herdr session and only the `testing` repository. Dedicated config and state folders isolate its ticker from the installed ticker. Test agents must not edit or push repository files.

OpenCode 2.0.15 started as `opencode mini --model openai/gpt-6-luna#max`. The test checked the target's agent name, working directory, foreground process, and shell PID before sending SIGTERM to that OpenCode process. Herdr retained the pane and its shell, but removed the agent entry from `session.snapshot`. The baseline ticker left the CIV-12 coordinator stopped for 61 seconds without a resume. Herdr also refused `agent.start` with `agent_pane_busy` when a separate test pane had `sleep` in the foreground.

The first recovery build marked the missing coordinator stale with zero attempts when native-session lookup was unavailable. It did not start a fresh OpenCode session. The isolated ticker initially resolved an unconfigured mise shim; using the native OpenCode executable then showed that the plugin's temporary XDG config and state variables also differed from the agent's environment. A test-only wrapper that removes those temporary variables returned the original session in the coordinator's canonical working directory.

After Keychain access was allowed, [CIV-13](https://linear.app/civitaspo/issue/CIV-13/verify-native-session-crash-recovery-with-resolved-opencode-path) verified the final build in the same isolated environment. Its coordinator resumed in `wM:p1` after the 30-second confirmation and 15-second delay, retaining native session `ses_f03686629ffeyilC0V1Lxqlz0P`. Actual foreground process arguments included `mini --model openai/gpt-6-luna#max --session` with that session ID.

The single testing worker was killed and automatically resumed three times in `wN:p1`, retaining its worktree and native session `ses_f03673cf1ffe6M48inUuQi5JWl`. Persisted retry deadlines were 15, 30, and 60 seconds after confirmation. Restarting the isolated ticker between the second crash and its resume preserved the cumulative budget. The third resume completed at 12:36:55 UTC. A fourth SIGTERM at 12:37:11 produced `stale` with `attempts = 3` after confirmation; no fourth automatic start occurred. One recovery-stopped inbox item requested manual restart, `reported` became true, and the Linear outbox drained. A separate earlier idle-without-report inbox item was unrelated to recovery exhaustion. No repository files were edited or pushed.
