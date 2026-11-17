# Model selection

Status: proposed, 2026-11-17. This is a design; none of it exists yet. The operator asked for
selection that is smarter than a fixed tier table, optimised for the operator's experience. On
2026-11-17 they chose two defaults: **the lowest cost that still merges**, and **escalation within
one harness** at first.

## Starting point

Current code at `1e0474e`:

- `src/ostromd/config.ts` reads one Worker model and one Router model from the environment
  (`OSTROM_WORKER_MODEL`, `OSTROM_ROUTER_MODEL`, both `gpt-6-luna` by default). Every ticket gets
  the same model. `assignment.model` records it.
- `src/session/runner.ts` passes only the model. Neither SDK receives an effort, so Codex uses its
  default. Both SDKs accept one: Codex `modelReasoningEffort` (`minimal` to `max`) as a thread
  option, the Claude Agent SDK `effort` (`low` to `max`).
- `src/budget/budgets.ts` has an `escalations` limit (`OSTROM_BUDGET_ESCALATIONS=0`), but nothing
  escalates, so it never counts.
- `docs/experience.md` and `docs/direction.md` describe tiers per repository, per label and per
  budget. None of that exists.

Evidence from [the first real-repository pilot](pilot-1.md): `gpt-6-luna` merged all six tickets.
Every turn ran at `high` effort, including approval wake-ups that changed nothing (#725). The extra
turns on duhem came from Ostrom's defects, not from the model failing the task.

## Shape

The operator states constraints. Ostrom chooses within them, says why, and learns from the result.
The operator does not name models for routine work.

### Constraints

A small file, `OSTROM_MODELS_FILE`, lists what Ostrom may use. Proposed shape (JSON, so it parses
without a new dependency):

```json
{
  "objective": "cost",
  "models": [
    { "harness": "codex", "model": "gpt-6-luna", "efforts": ["low", "medium", "high"], "cost": 1 },
    { "harness": "codex", "model": "gpt-6.1-terra", "efforts": ["low", "medium", "high"], "cost": 2 },
    { "harness": "codex", "model": "gpt-6.1-sol", "efforts": ["medium", "high"], "cost": 4 },
    { "harness": "claude", "model": "claude-opus-5-5", "efforts": ["medium"], "cost": 16, "approval": true }
  ]
}
```

- `cost` is a relative weight, not a price. Subscription plans have no per-call price, and the
  measured token history below replaces the weight once it exists.
- `approval: true` means Ostrom asks on the issue and waits before using that model.
- `objective` is `cost` by default. `balanced` and `quality` let the Router start higher on work it
  judges large or risky.
- Without the file, Ostrom behaves as today: the one configured model, now with an effort chosen per
  turn. Existing deployments keep working.
- The Router's own model stays a single setting. It is one-shot, cheap and already bounded.

### Model: once per ticket

When the Router accepts an issue, it also chooses a model from the allowed list and gives a
one-line reason. Its input:

- the issue and repository context it already reads;
- the allowed models, with their cost and approval flag;
- a small table of past outcomes for this repository (below).

Code checks the choice: it must be on the list, and an approval model waits for the operator. An
invalid choice falls back to the cheapest model, and that fallback is recorded as a decision.

The model stays fixed for the ticket's session. Changing models mid-session discards the cached
context, and resuming a session on different settings already caused #715.

### Effort: per turn

Turns within one ticket differ. A typo from review needs little thought, a merge conflict more, a
second CI failure more again. Before each turn, the Router chooses an effort from the model's
allowed list, from the turn's prompt and the ticket's history so far. Code enforces the list and any
escalation floor (below). The effort goes to the SDK on that resume; it does not start a new
session.

### Learning from outcomes

A new `turn` record stores, for every turn: ticket, model, effort, kind (first work, review
follow-up, CI fix, conflict, recovery), fresh tokens, and result. With the existing assignment and
merge records, code computes per repository and task kind: tickets merged, median turns, median
fresh tokens, and review rounds per model. That table goes into the Router's input. Code computes
the numbers; the Router makes the judgement.

With no history, the Router starts on the cheapest model and a moderate effort.

### Escalation

Code detects struggle, deterministically:

- the same change is requested again after the Worker said it was fixed;
- CI fails on two heads in a row;
- the Watchdog's stuck ladder starts a recovery;
- a turn fails.

The first response raises effort for the next turn. If that does not help, Ostrom moves the ticket
to the next model by cost **in the same harness**: the old session writes a short handoff, and a new
session continues on the same branch and worktree. Each model change counts against the existing
`escalations` budget. Its default becomes 2 when a models file is present. A move to an approval
model waits for the operator. Moving across harnesses is deferred until a real ticket needs it.

### What the operator sees and controls

- The status card and the dashboard show the model and the effort of the current turn, with the
  reason.
- `@ostrom use <model>` and `@ostrom effort <level>` on the issue or PR override for that ticket.
  The same overrides are `ostrom model <ticket> <model>` and `ostrom effort <ticket> <level>` on the
  CLI. An override is recorded and holds until changed; escalation never lowers it.
- `ostrom models` lists the allowed models and their outcome history. `ostrom replay` shows each
  selection decision with its input.
- Every turn's real model is confirmed from the native session record, which closes
  [open question 2](direction.md#open-questions). A mismatch is a turn failure, not a silent run on
  another model.

## Project rule 3

Rule 3 in `AGENTS.md` sends only accept/ignore judgements and reply wording to a model. Choosing a
model and an effort is a judgement too, so this design needs the rule amended to: "Only
accept/ignore, model and effort judgements, and reply wording go to a model." Validation,
escalation triggers, budgets and outcome statistics stay in code. The amendment is the operator's
call and lands only with their approval.

## Plan

Each PR is reviewed and merged on green CI before the next starts.

1. **Effort reaches the SDKs.** The runner passes an effort to both SDKs, covered by agreement
   tests. Turns record model and effort in a new `turn` table. Ostrom checks each turn's real model
   from the native session record.
2. **Constraints and the Router's choice.** `OSTROM_MODELS_FILE`, the Router's model and per-turn
   effort choices with reasons, validation and fallback, and the status card and dashboard lines.
3. **Overrides.** The two commands, the two CLI verbs, and `ostrom models` without history.
4. **Outcome history.** The statistics, the Router's input table, and history in `ostrom models`.
5. **Escalation.** The triggers, the effort step, the same-harness model move with handoff, and the
   escalations budget.

**Pass criterion:** on a pilot repository with a models file listing luna and sol, a small docs
ticket and a larger code ticket run with different chosen efforts, both with reasons on the status
card. An override on one ticket takes effect on its next turn. A scripted second CI failure raises
effort, and a third moves the ticket to sol in a new session that continues the same branch and
merges.
