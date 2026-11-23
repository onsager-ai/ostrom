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

Budget exhaustion pauses with `budget reached: <limit>` and never opens a stall or advances the
stuck ladder. Extending the exhausted budget resumes the ticket only when every limit clears;
operator pauses remain paused. Public stalls clear through operator resume or an allowlisted human
comment or review, even if the ticket has already reopened or ended. A reply resumes only a pause
caused by the public stall; other pause causes remain in force.

## Scripted stall

Semon reports a completed Codex exec turn as `ended`, not `waiting-question`, even when its last
Worker message says it is waiting for an answer. Nor does a hung command look idle to Semon: Codex
polls a long-running command about once a minute, so the session keeps writing and Semon reports it
working. The exercise therefore hangs an *open* turn (`sleep 3600`) under a short idle budget. Idle
time is how long the running turn has gone without finishing; between turns a Worker waits on people
and is not idle. The budget pauses the hung turn as `budget reached: idle`; it does not open a
stall, run recovery attempts, or post a public stuck reply. This exercises budget pausing. Use the
Semon question fixtures and `stuck.test.ts` to exercise the separate stuck ladder.

Use a throwaway database. Start `ostromd run` against that store first; the script only seeds an
accepted decision, whose ticket takes its budget from the environment as a Router-accepted one does.
The daemon dispatches the ticket in its normal `route-candidates` pass and owns the turn it later
cancels. The status card on the supplied issue shows the budget pause when the daemon reaches its
`send` pass.

```sh
export OSTROM_REPO=onsager-ai/ostrom OSTROM_ALLOWLIST=<login> OSTROM_DB=/tmp/ostrom-stall/ostrom.sqlite
export OSTROM_WORKER_REPO=<main-checkout> OSTROM_WORKER_BASE=main OSTROM_STALL_MINUTES=2
node src/ostromd/main.ts run --interval 15
# in another terminal, with the same environment:
OSTROM_BUDGET_IDLE_MIN=3 node src/watchdog/scripted-stall.ts <issue-number>
```

The seeded Worker brief runs `sleep 3600` again after every interruption and never commits or opens a
pull request, so the ladder's recovery prompts cannot end the stall; it then asks which colour to
use, red or blue. Stop the
daemon and remove its scratch worktree and branch after the exercise.
