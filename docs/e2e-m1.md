# Milestone 1 end-to-end run

Status: run on 2026-11-09 against the private sandbox
[`onsager-ai/ostrom-sandbox`](https://github.com/onsager-ai/ostrom-sandbox). This file records the
evidence for the pass criterion in [`direction.md`](direction.md#milestone-1): the store's records,
joined to the native session IDs (read through Semon) and the GitHub URLs. All times are UTC.

## Setup

- One machine; `ostromd run --interval 30` from `main`, restarted onto each fix as it merged.
- Worker and Router: Codex `gpt-6-luna` on the operator's ChatGPT login.
- Shorter windows for the run: `OSTROM_BATCH_WINDOW_MIN=2`, `OSTROM_COMMENT_GAP_MIN=5`,
  `OSTROM_STALL_MINUTES=10` (2 for the stall runs). Budgets at their defaults, except the scripted
  stall's ticket, seeded with `OSTROM_BUDGET_IDLE_MIN=3`.
- House rules file for Workers: no local tests, typechecks or builds (CI only), small commands, this
  worktree only, kill by PID.
- The sandbox's own CI (`test`) ran on every Worker push.

## Pass criterion

| Requirement | Result | Evidence |
|---|---|---|
| An issue becomes a merged PR with no human relaying | Pass | Tickets 1 and 3 below |
| Review follow-up reaches the same Worker | Pass | Review 2 on PR #2: one turn, one reply |
| The status card stays a single comment | Pass | One card comment per issue, edited in place (`card-thread-1`, `card-thread-4`) |
| `@ostrom pause` and `resume` keep the same session | Pass | Triggers 5 and 6, session `01a1c0c5-ff62-…` throughout |
| An `ostromd` restart mid-ticket resumes the session by its ID | Pass | SIGKILL during turn 6, recovery `1 interrupted`, `codex exec resume 01a1c0c5-ff62-…` |
| A scripted stall ends in the stuck reply | Pass | Ticket 8 below: two silent recoveries in the same session, then the public reply `stuck-6` |

A human merged both PRs (open question 4). The merge is the operator's decision; nothing was relayed
between GitHub, the Router and the Worker. The scripted stall passed only on its last run, after the
fixes its earlier runs found (#679 to #686).

## Ticket 1: issue #1 to PR #2

[ostrom-sandbox#1](https://github.com/onsager-ai/ostrom-sandbox/issues/1), "Last row is dropped when
input has no trailing newline". Worker session `01a1c0c5-ff62-79a1-a36b-29841b042653` (Codex,
`gpt-6-luna`, branch `ostrom/ticket-1-1`), 7 turns, 331,615 fresh input and 65,602 output tokens.

| Time | Event | Record |
|---|---|---|
| 14:42 | `@ostrom please take this one` comment | trigger 1 |
| 14:46 | Router accepts (session `01a1c0c5-e00d-…`); Worker dispatched; card posted | decision 1, ticket 1, assignment 1, `card-thread-1` |
| 14:50 | Worker opens [PR #2](https://github.com/onsager-ai/ostrom-sandbox/pull/2) (commit `1a8af6b`); CI green; observe subscribes the PR thread | — |
| 14:51 | "Acknowledged" reply (held behind the card by the comment gap) | `decision-1` |
| 14:51 | Review 1, two inline comments: one follow-up turn, 👀 on each comment | triggers 2–4, `received-2`, `received-3` |
| 14:54 | Review 1's turn ends (commit `ecd3dc2`); `ostromd` crashes writing the reply (fixed in #672); the reply is lost | — |
| 15:06 | "Ready for review" outcome reply | `outcome-a1-pr2-opened` |
| 15:08 | `@ostrom pause`: 👍, card **Paused**, ticket paused | trigger 5, `command-+1-5` |
| 15:09 | `@ostrom resume`: 👍, ticket open, same session | trigger 6, `command-+1-6` |
| 15:14 | Review 2, two inline comments: one turn, 👀 on each, commit `98b3185`, CI green | triggers 7–9, `received-8`, `received-9` |
| 15:17 | One reply for the whole review | [`review-1-5827946670`](https://github.com/onsager-ai/ostrom-sandbox/pull/2#issuecomment-6743663671) |
| 15:18 | Plain comment on the PR (redirect): 👀, turn 6 starts 15:19:00 | trigger 10, `command-eyes-10` |
| 15:19:28 | `ostromd` and its Codex child SIGKILLed by PID; store shows turn 6 open | — |
| 15:19 | Restart: `recover: 1 resumed, 1 interrupted`; `codex exec … resume 01a1c0c5-ff62-… --model gpt-6-luna`; rollout carries the recovery prompt; every `turn_context` is `gpt-6-luna` | — |
| 15:21 | The resumed turn finishes the redirect: commit `20b9a40`, CI green; card links the PR (#673) | — |
| 15:24 | Operator merges PR #2 (`da7190b`) | trigger 12 |
| 15:27 | Ticket ends with outcome **failed** ("the pull request was closed"): the merge was taken as a close (fixed in #676) | `outcome-a1-failed` |

## Ticket 3: issue #4 to PR #5, after the fixes

[ostrom-sandbox#4](https://github.com/onsager-ai/ostrom-sandbox/issues/4), "CRLF line endings leave
a carriage return in the last field". Worker session `01a1c0ef-fa84-7e10-997b-6ca96b05e9d3` (Codex,
`gpt-6-luna`, branch `ostrom/ticket-3-3`), 1 turn, 66,724 fresh input and 11,584 output tokens.

| Time | Event | Record |
|---|---|---|
| 15:31 | `@ostrom please take this one` comment | trigger 13 |
| 15:31 | Router accepts; Worker dispatched; card posted | decision 3, ticket 3, assignment 3, `card-thread-4` |
| 15:36 | Worker opens [PR #5](https://github.com/onsager-ai/ostrom-sandbox/pull/5); CI green; card shows the PR | — |
| 15:37 | Acceptance and "PR opened" go out as one batched comment | `decision-3`, `outcome-a3-pr5-opened` |
| 15:38 | Operator merges PR #5 (`c079445`); outcome `pr_merged`; ticket ends; card **Done** with the PR link | trigger 14, `outcome-a3-pr5-merged` |

## Ticket 8: the scripted stall on issue #9

[ostrom-sandbox#9](https://github.com/onsager-ai/ostrom-sandbox/issues/9), seeded by
`scripted-stall.ts` at `215680a` with a 3-minute idle budget. Worker session
`01a1c155-b015-7621-ae08-91b60d61f447` (Codex, `gpt-6-luna`, branch `ostrom/ticket-8-8`), 4 turns,
44,154 fresh input and 3,641 output tokens. The Worker made no edits, commits, pushes or pull
requests.

| Time | Event | Record |
|---|---|---|
| 17:23:00 | Daemon dispatches the seeded ticket; card posted; the Worker starts `sleep 3600` | ticket 8, assignment 8, `card-thread-9` |
| 17:26:07 | Idle budget reached during the turn; the turn is cancelled and a silent recovery prompt sent; the Worker restarts the sleep | stall 4, `budget reached: idle`, recovery 1 |
| 17:29:36 | Flagged again; second silent recovery; the Worker restarts the sleep | stall 5, recovery 2 |
| 17:33:01 | Flagged a third time; ticket paused; the Worker is asked for the stuck reply as JSON | stall 6, `public_at` 17:33:01 |
| 17:33:50 | [Public stuck reply](https://github.com/onsager-ai/ostrom-sandbox/issues/9#issuecomment-6746930596) posted from the fixed template; card **Stuck** | `stuck-6` |
| 17:34:51 | Operator stops the paused ticket (`ostrom stop 8`, after #686) | ticket 8 ended |

## Sessions through Semon

`semon query get_session <id>` reads every session Ostrom stored:

| Session | Role | Harness, model | Repo, branch | State |
|---|---|---|---|---|
| `01a1c0c5-ff62-79a1-a36b-29841b042653` | Worker, ticket 1 | codex, gpt-6-luna | ostrom-sandbox, `ostrom/ticket-1-1` | ended |
| `01a1c0ef-fa84-7e10-997b-6ca96b05e9d3` | Worker, ticket 3 | codex, gpt-6-luna | ostrom-sandbox, `ostrom/ticket-3-3` | ended |
| `01a1c155-b015-7621-ae08-91b60d61f447` | Worker, ticket 8 (scripted stall) | codex, gpt-6-luna | ostrom-sandbox, `ostrom/ticket-8-8` | ended |
| `01a1c0c5-e00d-7281-9f26-d68c241fae73` | Router, first decision | codex, gpt-6-luna | scratch directory, no branch | ended |

## Failures the run found

Each was fixed in its own PR, merged on green CI, and the daemon restarted onto it.

| PR | Failure on the run |
|---|---|
| #670 | The Router's scratch directory is not a Git repo; Codex refused to start, so trigger 1 was never decided |
| #671 | The first status card carried its reply marker twice |
| #672 | A review turn's reply was written to a store connection the pass had already closed; `ostromd` crashed |
| #673 | The card never showed the Worker's PR |
| #674 | Semon's `no-output` on a Worker idle between turns, or on a just-started turn, opened a stall |
| #676 | A merged PR was taken as "PR closed" and the ticket ended as failed; the card kept "Working" after a ticket ended |
| #679 | The first scripted stall was never flagged: Semon reports a finished `codex exec` turn as `ended`, not `waiting-question`. The exercise now hangs an open turn; each step of the stuck ladder first cancels the running turn, and the recovery count is kept per assignment |
| #680 | The stall run on issue #3 was ended by a false reopen: Ostrom's own card edit read as a state change and was routed to `close_issue`. Reopens now come only from GitHub's `reopened` issue events |
| #681 | The idle budget of ticket 5 (issue #6) ran out at 16:43, and recording that stall broke a constraint, because the `stall` table still required a Semon or T3 reason. Every Watchdog pass then failed |
| #682 | Stopping `ostromd` with SIGTERM to restart it onto #681 also stopped the Worker's Codex process, and the cut-off turn was recorded as **failed** (ticket 5, issue #6). The SIGKILL restart above had passed. A stop now leaves running turns open for recovery |
| #683 | On issue #7 the hung `sleep 3600` never looked idle to Semon: Codex polls a long command about once a minute, so the session kept writing. Only the idle budget can flag it, and the seed script ignored budget settings. It now takes them from the environment |
| #684 | After the first recovery, ticket 7's Worker opened PR #8 and waited for review; the 3-minute idle budget then flagged it twice more and posted a public stuck reply for a Worker that was only waiting. Idle time now counts only while a turn runs |
| #685 | The first recovery prompt ("make progress") made the scripted Worker drop the sleep, assume an answer and open an empty PR. The stall brief now re-runs the sleep and never opens a PR |
| #686 | `ostrom stop 7` did nothing to the paused ticket: every stop path is a turn, and Workers skipped turns on paused tickets |

Recorded for later, not needed for the pass criterion:

- #675: a review turn interrupted by a restart loses its reply.
- #677: a newly opened issue that asks for Ostrom is not a trigger; only comments are.
- #678 (fixed in #680): any update to an open issue with an ended ticket, including Ostrom's own card
  edit, was taken as a reopen and cost a Router call.
- `ostrom tickets | head` throws `EPIPE` when the pipe closes early.
- After the wrongful close in ticket 1, `ostrom show 1` lost the ticket's triggers, decisions and
  outbox, because that view joins through subscriptions, which the close removed.
- The Worker resolved review threads on GitHub itself. The brief forbids comments, not this.
- Ticket 7's Worker could not commit in its worktree ("read-only Git metadata mount"), so it wrote the
  commit object under `/tmp` and pushed it directly. Not investigated in this run.
- The public stuck reply on issue #7 ends "Reply with a number", but the Worker's choices were not
  numbered; the template does not check that.
- A comment on an ended ticket's closed PR (sandbox PR #8) failed routing once ("ticket 7 has no
  running Worker") and failed that tick; it did not repeat.
- One Semon read failed transiently (`native query source generation unavailable`); the Watchdog
  pass failed and the next one succeeded.
