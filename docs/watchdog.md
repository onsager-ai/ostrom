# The Watchdog and scripted stall

Milestone 1 step 6 (`docs/direction.md`). `ostromd watchdog` checks every running native Worker once; `ostromd tick` runs it after follow-up routing and before the Router pass.

## Signal

The Watchdog makes one deterministic Semon read:

```sh
semon query stalls --idle-minutes <OSTROM_STALL_MINUTES> --since 1d --json
```

Semon reports native sessions waiting on a question, whose process has gone, or which produce no output. A Worker is matched by its worktree branch, `ostrom/ticket-<ticket>-<assignment>`. A failed Semon read makes the pass fail and does not clear an existing episode.

Workers are driven turn by turn, so Semon's verdicts are read against the turn. Between turns no process runs and a Worker that waits for review produces no output, so only an unanswered question counts. During a turn, a verdict counts only once the turn itself has run for `OSTROM_STALL_MINUTES`, because Semon's idle clock starts at the session's last write, which may predate the turn.

## Episodes and the Router

A stall opens one `stall` row and remains one episode until Semon no longer reports it. The Router sees each unanswered, uncleared episode once and answers `wait` or `stop`; a stop needs a reply and ends the assignment only after that Router answer.

## Scripted stall

Use a throwaway database and never run `ostromd send` against it.

```sh
export OSTROM_REPO=onsager-ai/ostrom OSTROM_ALLOWLIST=<login> OSTROM_DB=/tmp/ostrom-stall/ostrom.sqlite
export OSTROM_WORKER_REPO=<main-checkout> OSTROM_WORKER_BASE=main OSTROM_STALL_MINUTES=2
node src/watchdog/scripted-stall.ts <issue-number>
# wait OSTROM_STALL_MINUTES
node src/ostromd/main.ts watchdog
node src/ostromd/main.ts route-candidates
```

The script starts a Codex `gpt-6-luna` session in a scratch Git worktree on `ostrom/ticket-<t>-<a>` and asks one question. It prints the ticket, assignment, native session ID, branch and worktree. Remove that worktree and branch after the exercise.
