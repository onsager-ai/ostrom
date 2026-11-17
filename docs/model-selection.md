# Model selection

Status: accepted, 2026-11-17. This is a design; none of it exists yet. The operator asked for
selection that is smarter than a fixed tier table, optimised for the operator's experience. On
2026-11-17 they chose: **the lowest cost that still merges** as the default objective, **escalation
within one harness** at first, allowed models **selected by an admin from those the harnesses
report**, and the rule-3 amendment below.

## Starting point

PR 1 implementation (effort and turn records):

- `src/ostromd/config.ts` reads one Worker model and one Router model from the environment
  (`OSTROM_WORKER_MODEL`, `OSTROM_ROUTER_MODEL`, both `gpt-6-luna` by default). Every ticket gets
  the same model. `assignment.model` records it.
- `src/session/runner.ts` accepts optional session and turn efforts, forwarding Codex
  `modelReasoningEffort` as a thread option and Claude Agent SDK `effort`. A changed Codex effort
  resumes the same native thread with new options. Null or absent effort leaves the harness default.
- Every Worker invocation records requested and observed model/effort, kind, usage and status in
  `turn`; `ostrom show` includes these records. Existing source kinds distinguish dispatch, review,
  redirect, control, ordinary follow-up, budget pause, stuck handling and recovery. CI fixes and
  conflicts are not separately identified by today's call sites.
- Claude's init message confirms its model; observed Claude effort remains null because the SDK
  does not report it. Codex's last native rollout `turn_context` confirms raw model and effort using
  the existing bounded reader. Semon's query summary lacks per-turn effort and formats model names,
  so it cannot supply this check. Missing evidence stays null; a model mismatch fails the turn.
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

### Allowed models: discovered, then chosen by the operator

The operator chose on 2026-11-17: an admin selects the allowed models from those the harnesses
report, and can customise each one. Nobody types model IDs or effort lists by hand.

**Discovery.** Ostrom asks each harness, on the operator's login, which models it offers:

- Claude Agent SDK: `supportedModels()` returns each model's ID, display name, description and
  `supportedEffortLevels`.
- Codex: the app-server's `model/list` returns each model's ID, display name, description,
  `supportedReasoningEfforts` and `defaultReasoningEffort`. It is marked experimental and the Codex
  SDK does not expose it, so Ostrom calls the app-server directly and pins the response shape with
  an agreement test (rule 5).

Discovery runs when `ostromd` starts and when the operator asks. Its result is cached in the store,
so a failed discovery call keeps the last good list and is reported, not hidden.

**Selection.** `ostrom models` shows every discovered model, marked allowed or not, with its efforts
and outcome history. The operator allows or removes models there (`ostrom models allow
codex/gpt-6.1-sol`, `ostrom models remove …`); the dashboard shows the same list read-only. A newly
discovered model is not allowed until the operator allows it. An allowed model that a harness stops
offering is skipped and flagged.

**Customisation.** Per allowed model, the operator may set:

- `efforts`: a subset of the discovered efforts, for example to forbid `max`;
- `approval`: Ostrom asks on the issue and waits before using the model;
- `cost`: a relative weight for the cold start, before measured token history exists;
- `note`: free text the Router reads, for example "use for Rust changes".

Repository-wide: `objective` is `cost` by default; `balanced` and `quality` let the Router start
higher on work it judges large or risky.

The selections live in the store, not in a hand-edited file, so the CLI validates every change
against the discovered list. `ostrom models export` and `import` move them between machines as
JSON.

Without any selection, Ostrom behaves as today: the one configured model, now with an effort chosen
per turn. Existing deployments keep working. The Router's own model stays a single setting. It is
one-shot, cheap and already bounded.

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
`escalations` budget. Its default becomes 2 once the operator has allowed more than one model. A move to an approval
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

Rule 3 in `AGENTS.md` sent only accept/ignore judgements and reply wording to a model. Choosing a
model and an effort is a judgement too. The operator approved the amendment on 2026-11-17: "Only
accept/ignore, model and effort judgements, and reply wording go to a model." Validation,
escalation triggers, budgets and outcome statistics stay in code.

## Plan

Each PR is reviewed and merged on green CI before the next starts.

1. **Effort reaches the SDKs.** The runner passes an effort to both SDKs, covered by agreement
   tests. Turns record model and effort in a new `turn` table. Ostrom checks each turn's real model
   from the native session record.
2. **Discovery and selection.** Both harnesses' model lists with agreement tests, the cached list
   and the operator's selections in the store, and `ostrom models` with `allow`, `remove`, the
   per-model settings, `export` and `import`.
3. **The Router's choice and overrides.** The Router's model and per-turn effort choices with
   reasons, validation and fallback, the status card and dashboard lines, and the two override
   commands and CLI verbs.
4. **Outcome history.** The statistics, the Router's input table, and history in `ostrom models`.
5. **Escalation.** The triggers, the effort step, the same-harness model move with handoff, and the
   escalations budget.

**Pass criterion:** on a pilot repository with luna and sol allowed through `ostrom models`, a small docs
ticket and a larger code ticket run with different chosen efforts, both with reasons on the status
card. An override on one ticket takes effect on its next turn. A scripted second CI failure raises
effort, and a third moves the ticket to sol in a new session that continues the same branch and
merges.
