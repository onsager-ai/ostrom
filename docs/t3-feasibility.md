# Can an external process drive T3 Code?

Checked 2026-11-09 against the installed `t3` v0.1.7 (runtime `~/.t3/runtime/versions/0.1.7`,
database migration 79) and the public source at `pingdotgg/t3code` `main` @ `d0f1f7200d`.

**Verdict: yes.** A plain process with no agent session can create T3 Code threads, send them
messages, stop them, and read their state. The daemon therefore can drive Workers itself; the Router
does not *have* to live inside T3 Code. Where it should live is still a design choice (see the
direction doc).

## Surface 1: the `t3` CLI (talks to the running server)

`t3 thread --help` lists `list`, `start`, `send` and `show`:

| Command | What it gives a daemon |
|---|---|
| `t3 thread start <project> <message> [--worktree <ref>] [--branch <name>] [--provider <id> --model <slug>] [--title] [--wait] [--json]` | Create a Worker thread, optionally in a new git worktree, and print its id |
| `t3 thread send <thread> <message> [--wait] [--json]` | Deliver a follow-up. "A busy thread gets it as its next turn; nothing is interrupted." |
| `t3 thread show <thread> [--messages N] --json` | State: `status`, `activeRunId`, `settled`, `pendingRequestCount`, `branch`, `worktreePath`, `linkedPullRequest`, `pullRequests`, and the latest messages |
| `t3 thread list [--project] --json` | All threads in a project, newest first |

`--wait` exits 1 when the run fails and 2 when the thread is waiting for an answer, so a waiting
Worker can be told apart from a finished one.

**What was run.** `t3 thread list --json` and `t3 thread show <id> --json --messages 1` were run
from a plain shell and returned the JSON above. `start` and `send` were **not** run, because each one
starts a paid model turn. Running them is the first step of milestone 1.

**What the CLI lacks.** There is no interrupt and no true steer: `send` only queues.

## Surface 2: the WebSocket RPC, with a scoped bearer token

`t3 auth session issue --scope orchestration:read --scope orchestration:operate --ttl … --token-only`
issues a bearer token for "headless or remote clients". Methods
(`packages/contracts/src/orchestrationV2.ts`, `ORCHESTRATION_V2_WS_METHODS`):

- `orchestration.dispatchCommand` takes commands that include:
  - `thread.create` (project, model, runtime mode, branch, worktree)
  - `message.dispatch` with `deliveryIntent: "auto" | "steer" | "restart"`, which is the steer the CLI lacks
  - `thread.stop`
  - settle, archive, tag, and `thread.pull-request.link`/`watch`
- `orchestration.launchThread`, `orchestration.forceStopThread`
- `orchestration.subscribeThread` and `orchestration.subscribeShell`, push streams of thread state
  and domain events, so the daemon does not need to poll
- `orchestration.getThreadProjection`, `searchThreads`, `getTurnDiff`

Method-to-scope mapping: `packages/contracts/src/clientRpcPermissions.ts`. The shipped web client
uses the same calls, for example `ThreadCommands.interruptTurn` in `client/assets/runtime-*.js`.

**Caveat.** This is T3 Code's own app protocol ("V2"), not a published, versioned public API. It
can change in any `t3 update`. Ostrom should pin the T3 version it is tested against and keep an
agreement test against the upstream schemas. That is cheap because both sides are TypeScript.

## Surface 3: MCP tools (agent sessions only)

The `t3_thread_*`, `delegate_task` and `t3_thread_interrupt` tools reach only an agent session that
T3 Code hosts. A daemon cannot call them. They matter if the Router is itself a T3 thread.
