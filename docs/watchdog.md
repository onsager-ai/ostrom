# The Watchdog and the scripted stall

Milestone 1 step 6 (`docs/direction.md`). `ostromd watchdog` checks every running Worker once;
`ostromd tick` runs it after follow-up routing and before the Router pass.

## Signals

- **Semon:** `semon query stalls --idle-minutes <OSTROM_STALL_MINUTES> --since 1d --json`, the one
  Semon read Ostrom makes. The answer is `{now, idle_minutes, rule, stalls: [...], window_start}`;
  each stall is a session with, among others, `id`, `name`, `repo`, `branch`, `state`,
  `stall_reason` (`waiting-question`, `process-gone` or `no-output`) and `open_question`
  (`{text, asked_at, handoff}` or null). `src/watchdog/fixtures/semon-stalls.json` is a real
  capture, trimmed to the scripted Worker's entry. Semon has no T3 thread id, so a Worker is
  matched by its branch, `ostrom/ticket-<ticket>-<assignment>`, which T3 creates per Worker.
  `--since 1d` keeps the read near 0.3 s here; the default 30-day window took about 2.7 s.
- **T3:** `t3 thread show`: a pending request (`pendingRequestCount > 0`), or a running run
  (`activeRunId` set, status `running` or `starting`), with `updatedAt` at least
  `OSTROM_STALL_MINUTES` old. A Worker blocked on its question read `running` with one pending
  request, not `waiting`, so the rule does not require `waiting`.

A Semon read that fails is reported and the pass exits non-zero; the T3 rule still runs alone, and
no episode is cleared while Semon is unread.

## Episodes and the Router

A stall opens one `stall` row (migration 5) and stays one episode until no signal shows it, when
the row is cleared. A new stall after that is a new episode. The Router sees each unanswered,
uncleared episode once, as `stalls` in its batch, and must answer each with
`{"key": "stall-<id>", "action": "wait" | "stop", "reply": "..." | null}`; a stop needs a reply.
`wait` leaves the Worker running; `stop` ends the assignment and, when no other assignment of it
runs, the ticket, in the same transaction that queues the reply. Code never ends work on a stall
without that answer. At the T3 pin the CLI cannot interrupt a run, so a stopped Worker's T3 thread
is left as it is; Ostrom stops tracking it.

## Scripted stall

Uses a throwaway store and real T3 runs on Haiku. Never run `ostromd send` on this store: the
Router's reply would be posted to the issue.

```sh
export OSTROM_REPO=onsager-ai/ostrom OSTROM_ALLOWLIST=<login> OSTROM_DB=/tmp/ostrom-stall/ostrom.sqlite
export OSTROM_T3_PROJECT=<path inside the T3 project> OSTROM_WORKER_BASE=main
export OSTROM_STALL_PROVIDER=claudeAgent OSTROM_STALL_MODEL=claude-haiku-5-5
export OSTROM_ROUTER_PROVIDER=claudeAgent OSTROM_ROUTER_MODEL=claude-haiku-5-5 OSTROM_STALL_MINUTES=2

node src/watchdog/scripted-stall.ts <issue-number>   # prints ticketId, assignmentId, t3ThreadId, branch
# wait OSTROM_STALL_MINUTES
node src/ostromd/main.ts watchdog                    # newEpisode: true; again: newEpisode: false
node src/ostromd/main.ts route-candidates            # the Router answers the stall; reply queued, not sent
```

The Worker is asked to put one question with AskUserQuestion and wait. Clean up afterwards: stop
the Worker's T3 thread, then from the main checkout `git worktree remove <worktreePath>` and
`git branch -D <branch>`. The Worker pushes nothing.
