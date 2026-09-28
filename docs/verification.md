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
| Close | THLA-7 to THLA-10 | Done or Canceled closed the run and its workspaces within about 4 s. |

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

## Observations

- Every agent activity and every reply in the session is also a comment on the issue, so each one changes the issue's hash and writes an "issue was edited" inbox item. The first test had the same behavior; the coordinator handles those items quickly, but they are noise.
