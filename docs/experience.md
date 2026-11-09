# Ostrom experience

Status: draft, 2026-11-09. The operator accepted this design on 2026-11-09. Some questions are still
open and are listed in each journey. This document comes before `direction.md` on runtime choices:
the experience decides the stack, not the other way round.

## Principle: Ostrom is background

Every Ostrom session (Router, Workers, probes) runs in the background. Ostrom never creates
foreground sessions, such as T3 Code threads, on its own, and never clutters the operator's
workbench. People reach Ostrom in two ways only:

- **GitHub**, where everyone else acts: issues, mentions, comments, reviews and PRs.
- **The operator's foreground**, by deliberate choice: one status view, plus *promote to
  foreground* for a single session.

```
 GitHub (people act)          Ostrom (background)                   Operator (T3 foreground)
 ───────────────────          ─────────────────────────────         ────────────────────────
 issue / @mention   ───▶      intake › router › workers › outbox    one standing status view
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

Open questions:

- Should progress be one status comment that Ostrom keeps editing, or one comment per event?
- How does someone stop or redirect Ostrom: a comment, a label, or closing the issue?
- What reply is guaranteed when a Worker is stuck?

## Journey 2: the operator

Background by default, no clutter.

1. Work happens without any new T3 threads.
2. The operator glances at one status view: what is running, what is stuck, and what it cost.
3. The Watchdog flags a stalled Worker.
4. The operator promotes that session into T3 and unblocks it.
5. The operator hands it back, and Ostrom carries on.

Open questions:

- Where does the status view live: a standing Ostrom thread, a dashboard, a GitHub project board,
  or `ostromd status`?
- Should each ticket have a cost limit?
- Does the operator choose which machine runs a Worker, or does Ostrom?

## Journey 3: people developing Ostrom

1. Run one tick locally against recorded GitHub data.
2. No paid model calls by default.
3. Replay any trigger and see each decision it led to.
4. Adding a harness or model tier is a configuration change.
5. CI runs the tests and the agreement tests; the operator's laptop does not.

Open questions:

- Which of these does milestone 1 need?
- Recorded fixtures or a sandbox repository?

## What the experience requires of the runtime

| Experience | Requirement | Evidence so far |
|---|---|---|
| Promote a Worker to T3 | Workers are native Claude Code or Codex sessions with their own IDs | `@mastra/claude` and `@openai/codex-sdk` both returned resumable native IDs |
| The same Worker answers follow-ups | Resume a session from a stored ID after a host crash | Passed between turns for both; mid-turn crash not tested |
| A reviewer replies days later | Durable pause and resume | Mastra workflows passed across processes; real multi-day delay not tested |
| The operator does not pick machines | Run and resume across machines | Unproven; Temporal and Restate support it directly, Mastra needs shared storage |
| Ostrom runs on the operator's plans | Use the operator's Claude and Codex logins, not API keys | Unproven |

The spike evidence is outside this repository, in `ostrom-spikes/mastra` and `ostrom-spikes/mastra-sdk`
(`FINDINGS.md` in each). ACP is deprioritised: it is not widely adopted, and Ostrom only needs
Claude and Codex for now.

The runtime in `direction.md`, with the Router and Workers as T3 threads, does not meet the
background principle. `direction.md` will be re-derived from this document once the open questions
above have answers.
