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

Semon currently reports a completed Codex exec turn as `ended`, not `waiting-question`, even when
its last Worker message says it is waiting for an answer. The exercise therefore hangs an *open*
turn with no output (`sleep 3600`), which Semon reports as `no-output`; the Watchdog counts that
only after the turn itself has run for `OSTROM_STALL_MINUTES`.

Use a throwaway database. Start `ostromd run` against that store first, with a small stall threshold;
the script only seeds an accepted decision. The daemon dispatches the ticket in its normal
`route-candidates` pass and owns the turn it later cancels. Its public stuck reply is queued in the
outbox and posts to the supplied issue when the daemon reaches its `send` pass.

```sh
export OSTROM_REPO=onsager-ai/ostrom OSTROM_ALLOWLIST=<login> OSTROM_DB=/tmp/ostrom-stall/ostrom.sqlite
export OSTROM_WORKER_REPO=<main-checkout> OSTROM_WORKER_BASE=main OSTROM_STALL_MINUTES=2
node src/ostromd/main.ts run --interval 15
# in another terminal, with the same environment:
node src/watchdog/scripted-stall.ts <issue-number>
```

The seeded Worker brief runs `sleep 3600`, then asks which colour to use, red or blue. Stop the
daemon and remove its scratch worktree and branch after the exercise.
