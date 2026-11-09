# Ostrom experience

Status: accepted, 2026-11-09. The operator's decisions are recorded in each journey. This document comes before `direction.md` on runtime choices:
the experience decides the stack, not the other way round.

## Principle: Ostrom is background

Every Ostrom session (Router, Workers, probes) runs in the background. Ostrom never creates
foreground sessions, such as T3 Code threads, on its own, and never clutters the operator's
workbench. People reach Ostrom in two ways only:

- **GitHub**, where everyone else acts: issues, mentions, comments, reviews and PRs.
- **The operator's foreground**, by deliberate choice: the `ostrom` CLI and a dashboard,
  plus *promote to foreground* for a single session in T3.

```
 GitHub (people act)          Ostrom (background)                   Operator (foreground)   
 ───────────────────          ─────────────────────────────         ────────────────────────
 issue / @mention   ───▶      intake › router › workers › outbox    `ostrom` CLI + dashboard
 comments, reviews  ◀───      store · subscriptions · watchdog ───▶ pause / stop a ticket
 Ostrom's replies, PRs        ticket 12 · codex · luna              promote a session ⇄
                              ticket 15 · claude · haiku            (open in T3, hand back)
```

**Promote to foreground.** Any Worker is a real Claude Code or Codex session with its own session
or thread ID. The operator can open that session in T3, talk to it, and hand it back. Ostrom then
carries on with the same session. Nothing appears in T3 unless the operator asks for it.

## Journey 1: someone on GitHub

An issue author or reviewer, who never sees Ostrom's internals.

1. Opens an issue or mentions Ostrom.
2. Within minutes Ostrom replies with "taking this" or "not for me, because …".
3. A PR appears, linked to the issue.
4. Their later comments and reviews reach the **same** Worker, through thread subscription.
5. The Worker revises; the reviewer merges.

### Progress without flooding

GitHub sends no notification when a comment is edited, but sends one for every new comment. So
Ostrom edits silently and posts a new comment only when someone needs to act.

- **One status card per issue, edited in place.** It shows the state, a plan checklist, the PR link
  and the last activity. The "last activity" line makes silence visible. It also lists the
  controls.
- **Reactions acknowledge.** 👀 means received, 👍 means done, and 😕 means not allowed or unclear. A
  command or steer gets a reaction, not a reply.
- **New comments only at "needs you" moments:** taken or declined, a question, ready for review,
  stuck, and done or stopped.
- **The outbox batches, in code.** It gathers events per thread over a window (10 minutes to start)
  and sends them as one comment. A thread gets at most one new comment per 30 minutes. A blocking
  question goes out at once.
- **One reply per review**, never one per review comment. Ostrom replies inline only where it
  disagrees or asks a question.

### Stop and redirect

Plain comments steer. A few fixed words control. Code parses the commands, every command gets a
reaction, and the status card shows the result.

| Say or do | Effect | Who may | Acknowledged by |
|---|---|---|---|
| Any comment on the issue or PR | **Redirect:** steers the same Worker at its next turn boundary | Anyone; text from non-collaborators is information, not instruction | 👀; the card shows "redirected by @x" |
| `@ostrom pause` | The Worker finishes its current step, pushes its work in progress, and sleeps. The session is kept. | Issue author, or repo triage or higher | 👍; card **Paused**; label `ostrom:paused` |
| `@ostrom resume`, or removing the label | The same session continues where it stopped | Issue author, or repo triage or higher | 👍; card **Working** |
| `@ostrom stop` | Graceful stop: push work in progress, turn the PR into a draft, post one summary of where it stopped. The ticket ends. | Issue author, or repo triage or higher | 👍 and one closing comment |
| Close the issue | Same as stop. The branch is kept; reopening the issue asks the Router again. | Whoever can close it | Card **Stopped: issue closed** |
| Close the PR | Ends that attempt only; the issue stays open for a new decision | Whoever can close it | Card updated, no new comment |
| A command from someone without permission | Ignored | | 😕 only; no comment, so no noise and no argument |

One table in code defines the commands, permissions and reactions. The parser, the help line on the
status card and this table are all generated from it.

### When a Worker is stuck

The guarantee: no ticket goes quiet. Every stall ends in automatic recovery or in exactly one
specific, honest reply.

1. **Detect (silent).** The Watchdog spots no progress for N minutes, a repeated error loop, a
   crash, an exhausted budget, or an unanswered question.
2. **Recover (silent).** Ostrom resumes the same session, then retries, then moves up one tier, with
   at most two attempts. The card shows "recovering".
3. **Operator.** The stall appears in the operator's status view first. The operator can promote
   the session to T3 and unblock it.
4. **Public.** Ostrom posts one stuck reply, labels the issue `ostrom:stuck` and sets the card to
   **Stuck**. There are no further pings until someone replies.

The stuck reply follows a fixed template. The Worker fills it in, and code checks that every field
is present before it is sent:

> **I'm stuck on this and need a decision.**
> **Trying to:** what the Worker was doing.
> **Blocked by:** the concrete cause.
> **Tried:** what it attempted, with links to commits or the PR.
> **I need you to choose:** numbered options.
> **Work so far:** where the pushed work in progress is. Reply with a number, or anything else to
> redirect.

### Developer experience for this journey

- `ostromd preview` renders exactly the comments, edits, reactions and labels a tick would post, as
  Markdown, without sending anything.
- Status cards, batched comments and stuck replies are snapshot-tested against recorded GitHub
  fixtures, so a wording change shows up as a diff in review.

Decided by the operator on 2026-11-09:

- Pause and stop will also be labels that people can add, such as `ostrom:stop`, for bulk triage
  from the issue list. This comes in milestone 2.
- The operator gets a 30-minute grace window before the public stuck reply. This comes in
  milestone 2; until then, the stuck reply goes out when the operator step is reached.
- Batching defaults: a 10-minute gathering window, and at most one new comment per thread per 30
  minutes. A blocking question skips the wait.

## Journey 2: the operator

Background by default, no clutter.

1. Work happens without any new T3 threads.
2. The operator glances at one status view: what is running, what is stuck, and what it cost.
3. The Watchdog flags a stalled Worker.
4. The operator promotes that session into T3 and unblocks it.
5. The operator hands it back, and Ostrom carries on.

### Status view: the `ostrom` CLI and a dashboard

`ostromd` is only the daemon; it has no user-facing commands. `ostrom` is the operator's command
line, in the style of `t3`. The CLI and a dashboard are two clients of one local API served by
`ostromd`, so both always show the same thing.

| Command | Does |
|---|---|
| `ostrom status` | Machines, running Workers, stuck tickets, and spend today |
| `ostrom tickets` / `ostrom show <ticket>` | List tickets, or show one: timeline, decisions, session, budget used |
| `ostrom pause` / `resume` / `stop <ticket>` | The same effects as the GitHub commands |
| `ostrom extend <ticket> --time 1h --cost 2` | Raise a ticket's budget |
| `ostrom promote <ticket>` / `ostrom handback <ticket>` | Open the Worker's own session in T3, then return it to the background |
| `ostrom pin <ticket> <machine>` | Override where a ticket runs |
| `ostrom preview` / `ostrom replay <trigger>` | Developer tools (see journey 3) |

Every command takes `--json`. The dashboard is a local web page with the same views, plus an
attention list: stuck tickets, near-budget tickets, and machines that are offline or running hot.

### Budgets per ticket

Every ticket runs under a budget with several limits: wall-clock time, idle time, turns, tokens,
estimated cost, and tier escalations. Defaults come per tier and can be overridden per repository
or per label.

- At 80% of any limit, the status card and dashboard show a warning.
- At a limit, the Worker **pauses**. It is not killed: it pushes its work in progress, and the stuck
  ladder starts with the cause "budget reached". The operator can `ostrom extend` it or stop it.
- Subscription plans have no per-call price, so Ostrom always enforces tokens and time, and treats
  cost as an estimate.

### Machine placement strategy

Each machine runs `ostromd` and registers its capabilities: installed harnesses and logins,
repositories, free Worker slots, and thermal headroom. Ostrom places each new ticket by a fixed
strategy, in this order:

1. **Pinned:** an operator pin (`ostrom pin`) or a repository rule wins.
2. **Affinity:** a ticket that already has a session stays on that machine. Follow-ups, resumes and
   promotion all go to where the session lives.
3. **Capable:** only machines that have the needed harness, login and repository.
4. **Headroom:** prefer the machine with free slots and thermal headroom; a laptop near its heat
   limit takes no new Workers.
5. **Long-running work prefers always-on machines**, such as a server, over a laptop that sleeps.

The strategy is code with a reason recorded for each placement, and `ostrom show` displays it.

## Journey 3: people developing Ostrom

1. Run Ostrom end to end against a **sandbox GitHub repository**, a real repository kept for this
   purpose, rather than recorded fixtures.
2. Use cheap tiers by default: gpt-6-luna and Haiku.
3. `ostrom preview` shows what a tick would post; `ostrom replay <trigger>` re-runs one trigger and
   shows each decision it led to.
4. Adding a harness or model tier is a configuration change.
5. CI runs the tests, snapshot tests and agreement tests; the operator's laptop does not.

## Milestone 1 scope

Milestone 1 proves the loop on one machine and the sandbox repository, with the GitHub experience
complete. Each item below is something a person can see working.

| In milestone 1 | Later |
|---|---|
| Intake → Router → one Worker → PR, on the sandbox repository | More repositories, chat and CI as sources |
| Status card, reactions, outbox batching, one reply per review | Labels as controls (`ostrom:stop`) |
| Redirect by comment; `@ostrom pause`/`resume`/`stop`; closing the issue or PR | |
| The stuck ladder and the stuck template (operator step is `ostrom status`) | A grace window before the public stuck reply |
| Budgets: time, idle time, turns and tokens; pause at a limit; `ostrom extend` | Cost estimates per provider |
| `ostrom status`, `tickets`, `show`, `pause`/`resume`/`stop`, all with `--json` | The dashboard (milestone 2, on the same API) |
| Native Claude and Codex sessions resumed by ID after an `ostromd` restart | `ostrom promote`/`handback` (milestone 2) |
| A `machine` field on every assignment, always this machine | The placement strategy across machines (milestone 3) |
| `ostrom preview` and snapshot tests of everything posted | `ostrom replay` |

## What the experience requires of the runtime

| Experience | Requirement | Evidence so far |
|---|---|---|
| Promote a Worker to T3 | Workers are native Claude Code or Codex sessions with their own IDs | `@mastra/claude` and `@openai/codex-sdk` both returned resumable native IDs |
| The same Worker answers follow-ups | Resume a session from a stored ID after a host crash | Passed between turns for both; mid-turn crash not tested |
| A reviewer replies days later | Durable pause and resume | Mastra workflows passed across processes; real multi-day delay not tested |
| The operator does not pick machines | A placement strategy with machine affinity | Designed above; no runs yet |
| Ostrom runs on the operator's plans | Use the operator's Claude and Codex logins, not API keys | Unproven |
| Budgets per ticket | Token and turn counts from every turn | Both SDKs report token usage; the Codex SDK does not report the model, so Ostrom checks it from the session record |

**Runtime decision (operator, 2026-11-09): no orchestration framework.** Mastra is not needed. Ostrom
calls the vendor SDKs directly, `@anthropic-ai/claude-agent-sdk` and `@openai/codex-sdk`. It keeps its
durable state (tickets, sessions, pauses, budgets and placement) in its own SQLite store, as the
ticket state machine it already has. ACP is deprioritised: it is not widely adopted, and Ostrom only
needs Claude and Codex for now. The spike evidence is outside this repository, in
`ostrom-spikes/mastra` and `ostrom-spikes/mastra-sdk` (`FINDINGS.md` in each).

The runtime in `direction.md`, with the Router and Workers as T3 threads, does not meet the
background principle. `direction.md` will be re-derived from this document once the open questions
above have answers.
