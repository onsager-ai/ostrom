---
name: brief
description: Synthesize the mandate queue into blocked-on-you, execution-debt,
  and dependency buckets without making decisions. Use whenever the user types
  /ostrom:brief, asks for a portfolio brief or decision-support plan, or the daily
  digest asks for the brief.
argument-hint: "[today]"
---

# Mandate Brief

Turn the durable queue facts into a proposed reading and execution order. This
is a plan, not a decision surface.

## 1. Resolve config (layered YAML)

Read and merge these layers, most-specific wins:

1. shipped defaults — `config/mandate-defaults.yaml` in this plugin (sibling of `skills/`)
2. user — `~/.claude/ostrom/mandates.yaml`
3. repo — `./.ostrom/mandates.yaml` (if present)

The v1 provider must resolve to `file`. Its fixed private records are
`~/.claude/ostrom/queue.jsonl` and `~/.claude/ostrom/state.json`. Never create,
display, or commit a roster anywhere else. If no mandates file is configured,
say so and stop. The optional root-level `hold_labels` list contains
case-insensitive label globs; matching delegated work remains in the queue as
`kind: "parked"` but is not execution debt while that label remains.

The optional root-level `work_ranking` is a highest-first ordered list of
canonical item IDs recorded by the principal. Read it as a fact, never as an
approval or authorization change. Confirm that `state.json` records the same
list from the last sweep and read `work_ranking_faults`; a mismatch means the
ranking has not been swept, while a non-empty fault list names stale pointers.

Read `~/.claude/ostrom/direction.md` only if it exists. Use recorded direction
calls to note concrete conflicts with queued items. If the file is absent,
continue with structural output. Never create it.

## 2. Read facts

Run:

```sh
ostrom queue list
jq '.repos | to_entries | map({repo: .key, items: .value.items, item_cap: .value.item_cap})' \
  "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/ostrom/state.json"
ostrom trace read
```

Use only pending and deferred rows. The sweep fields are facts, not agent
judgments: `age_days`, `aged_out`, `needs_judgment`, and `blocked_by`. For an
older row, derive `needs_judgment` from `kind` and treat absent `blocked_by` as
empty; do not rewrite the queue.

Count `parked` rows separately and do not place them in an execution or
judgment bucket.

From the trace, keep `plan-selection` facts and calculate the plan match rate
over selections where `plan_status` is `applied` or `rejected`; selections
where it is `absent` are reported separately and are not part of that
denominator. Count rejection clauses and choose the largest count as the
dominant clause (break ties lexically). Treat malformed trace input as missing
observability, never as a queue fact.

Resolve every `blocked_by` pointer read-only. A pointer still present in the
last sweep state is unsatisfied. For a pointer outside that state, use `gh` to
check whether the issue or pull request is open. If its state cannot be
resolved, leave the row unclassified and name the row and unresolved pointer.
For bucketing, use the unsatisfied subset; a fully satisfied dependency array
is structurally clear.

## 3. Bucket once

Place every classifiable row in exactly one bucket:

1. **Blocked on you** — `needs_judgment` is true and `blocked_by` is empty or
   every dependency is satisfied.
2. **Blocked on no one** — `needs_judgment` is false and `blocked_by` is empty
   after resolution. Call this execution debt plainly; name any satisfied
   dependency as cleared rather than as a blocker.
3. **Blocked on other work** — `blocked_by` is non-empty and any dependency is
   unsatisfied. Name each unsatisfied `owner/repo#number`.

Within **Blocked on you**, propose the cheapest judgment first: prefer the
smallest reading burden, narrowest blast radius, and most reversible call.
Never place a row before another queued row that it names in `blocked_by`.

If a row conflicts with a recorded direction call, keep its structural bucket
and mark the conflict beside it. Do not reinterpret the direction file as an
approval.

## 4. Report

Lead with honest counts for all three buckets and the unclassified remainder.
Immediately after the counts, show **Active work ranking** as the ordered item
IDs, or `none`. Name every `work_ranking_faults` pointer beside it so stale
direction is conspicuous rather than silently omitted. The structural buckets
remain authorization-aware; a ranked held, reserved, tripwire, deferred, or
otherwise unauthorized row stays in its existing bucket.

Immediately after **Active work ranking**, show **Plan match rate** for the
current sprint trace. Render a positive rate as `A/P (R%); dominant rejection:
CLAUSE (N); no plan present: S` (use `none (0)` when there were no rejections,
and round `R` to the nearest whole percent). A zero rate must read `0/P (0%) —
PROBLEM: computed plans never applied; dominant rejection: CLAUSE (N); no plan
present: S`. If every recorded selection has `plan_status: absent`, render `not
yet measurable — no plan present in S selections`; if there are no selection
records, render `unavailable — no selections recorded`. Never combine absent
plans with rejected plans.
Then show each bucket in proposed order with the resolvable
`owner/repo#number`, title, age, aged-out state, dependency status, and concise
rationale. End with `Could not classify: none.` or a line naming every omitted
row and the missing fact.

Propose only. Never approve, reject, or defer a row, and never call `ostrom queue`
with a mutating verb. `/ostrom:desk` remains the sole decision surface. `/ostrom:brief` stays
available as a manual pull.
