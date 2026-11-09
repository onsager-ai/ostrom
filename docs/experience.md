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

Open questions:

- Should pause and stop also be labels that people add, such as `ostrom:stop`, for bulk triage
  from the issue list?
- Should the operator get a grace window (30 minutes, say) before the public stuck reply, or should
  both go out at once?
- The batching window and the limit on new comments: are 10 and 30 minutes the right defaults?

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
