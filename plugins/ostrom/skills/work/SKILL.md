---
name: work
description: Advance the portfolio queue one pass by verifying, delegating,
  reviewing, and reporting; this loop is the builder's and must run in a
  builder session.
argument-hint: "[optional queue focus, e.g. project name or item class]"
---

# Mandate Work

Work the portfolio queue forward for one pass. Recurrence belongs to the
external pass timer; never create or renew an in-session recurring wake.

Assume no context from any previous session. Everything needed is on disk or
on GitHub. Relying on conversation memory makes the work unsustainable.

## 1. Stay in the builder role

Run this loop only in a builder session. The builder verifies and delegates
implementation, reviews returned work, commits, and opens pull requests. It
never merges its own work; `/ostrom:gatekeep` runs independently in a separate
gatekeeper session.

## 2. Acquire the builder lease and start its trace

Before reading config, the trace, queue state, or any GitHub artifact, choose
one unique, non-empty owner for this session and wake in the exact shape
`builder-<session>-wake<N>`. Retain that exact string for every trace record and
for cleanup, then run:

```sh
MANDATE_LEASE_NAME=builder.lease \
  bash "${CLAUDE_PLUGIN_ROOT}/scripts/lease.sh" acquire "$lease_owner"
```

Only exit 0 owns the pass. Exit 3 means another builder pass owns it: report
that this wake backed off and stop without reading config or trace, enumerating
items, sweeping, or appending anything. Any other nonzero exit is a lease
failure; report it and stop. Never infer concurrency or lease ownership from
`sprint.jsonl`, queue state, prior output, or wake timing.

Immediately after acquisition, append the first trace record:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/trace.sh" append pass-started \
  "$(jq -cn --arg owner "$lease_owner" '{owner: $owner}')" \
  '{}'
```

Every trace append in this protocol supplies separate fact and narration JSON
objects. Put identifiers, actions, and values returned by GitHub or another
tool in `fact`. Put only reasons, beliefs, and conclusions in `narration`; use
`{}` when there is none. Never put an issue or PR number, commit SHA, action,
verdict, or exit code only in narration. If any trace append fails, end the
pass as a failure and release the lease through step 7 rather than continuing
with an invisible pass.

## 3. Establish state

Decide whether to sweep, and sweep before reading anything the sweep
produces. Run the sweep when `state.json` is older than `cadence_hours`, or
whenever anything has changed the repositories since it ran. A previous pass
that closed issues or opened pull requests leaves the file young and its
contents wrong. The sweep is cheap and idempotent; when in doubt, run it.

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/sweep.sh"
```

Then read, in order:

- `~/.claude/ostrom/mandates.yaml` — the authorization boundary: what each
  project delegates entirely and what bounces back.
- `~/.claude/ostrom/queue.jsonl` — what the last sweep found. Rows are
  pointers, so resolve titles from GitHub rather than trusting cached text.
- The SessionStart digest, if it is in context.

Each row's `state` is `pending`, `approved`, or `deferred`. `needs_judgment`
is derived from `kind` alone and is true for every tripwire and decision row
regardless of `state`, so it marks the row's kind, not whether a judgment is
still outstanding — never use it by itself to decide whether an item is the
owner's call. `approved` clears the mandate boundary: the principal has already
ruled and minted a handoff token, so the item needs no fresh escalation. It is
no guarantee the work is still wanted — the queue keeps no approval timestamp,
so an approval cannot be assumed to be the item's latest word. Before acting on
an approved row, read the item itself, body and comments, for a later decision,
hold, or superseding instruction (the same gap #63 tracks for classification).
Only one that changes, cancels, holds, or narrows the work wins over `state`;
record it and move on rather than implementing. One that reaffirms,
re-approves, or merely clarifies the work is a reason to proceed. If what you
find is genuinely ambiguous, ask the narrow question on the item and move on
rather than guessing.
`deferred` means the principal has explicitly parked it: leave it, and do not
re-escalate it — re-raising a deferred item is noise, not diligence. Only a
`pending` tripwire or reserved ref is genuinely the owner's call.

If the `/ostrom:work` invocation includes an optional focus, use that direct
invocation input as a natural-language filter over the queue, such as `one
repository only` or `just tripwires`. Otherwise take items in this order:
**pending tripwires and reserved refs → CI drift → open review threads on
your own pull requests → delegated work**, oldest first. An approved
tripwire is no longer a boundary; it belongs in the delegated tier.

## 4. Work each item

**Verify before acting.** Check the named file, symbol, or command yourself. A
claim not checked is not a finding. This applies equally to previous-session
notes, implementer reports, and recollection. If a claim is wrong, say so
plainly and correct the record.

**Respect the boundary.** A tripwire, reserved ref, or anything outside a
project's `delegated` selectors is the owner's call. Produce an escalation
dossier — question, options ruled out, recommended action, blast radius — and
move on. An approved row clears that boundary: the principal has already ruled
and minted the handoff token, so the item needs no fresh escalation. It carries
no guarantee the work is still wanted, since the queue keeps no approval
timestamp. Read the item itself, body and comments, for a later decision, hold,
or superseding instruction before acting. Only one that changes, cancels,
holds, or narrows the work wins over `state`; record it and move on rather
than implementing. One that reaffirms, re-approves, or merely clarifies the
work is a reason to proceed. If what you find is genuinely ambiguous, ask the
narrow question on the item and move on rather than guessing. That is not
license to re-escalate an approved row as though it were pending — read the
item, then act.
Never widen a mandate to unblock yourself.

**Delegate implementation.** Never implement inline. Spec the work first,
then route by size: send a bounded, single-concern change to a subagent that
stays in this transcript; send substantial or multi-file work to a separate
implementer harness in a git worktree. When either route fits, prefer the
asynchronous worktree route. A pass is cheap to repeat, and the next pass can
pick up a long build.

Either way, review the returned diff against the spec. The summary is not
evidence: confirm the artifact changed, run the tests, probe the load-bearing
claim, and scan for what the implementer was never asked about — leaked
secrets, private data in public files, and edits outside scope.

**Hand it off; do not land it.** Get the pull request open and CI green, then
stop. The gatekeeper merges. Do not squash-merge, delete the branch, or remove
the worktree; those are the merging party's actions. The gatekeeper's advantage
is position: it did not write the code and has not already argued that the code
is correct. A gate the builder can satisfy is not a gate.

**Answer your own open review threads.** A thread left open on a pull request
you opened is the same shape as CI drift: work already begun, already failing a
gate condition, and holding back something otherwise finished. That is why it
outranks new delegated work. Read the thread, verify the point against the diff
yourself, and then either fix it or reply saying why no change is warranted —
both are answers; silence is not. An automated reviewer comments on most pull
requests and will not chase you, so a thread nobody answers blocks the gate for
as long as nobody answers it.

That rule covers an **unanswered** thread — the reviewer's comment is still the
last word. Once you have replied, the thread is **answered**, and `gate.sh`
reports the two counts separately for exactly this reason: an answered thread
still fails `review_threads`, but it is no longer outstanding work. Do not
re-read it looking for something to do, do not dispatch an implementer at it,
and do not add a second reply restating the first — none of that moves the
gate, and a thread that keeps growing looks like neglect rather than the
settled position it is. An answered thread is stuck on the reviewer, who does
not come back, or on the principal, who can override the gate; it is not stuck
on you. Report it as awaiting the principal and move on.

Never resolve or dismiss a review thread on your own pull request, including a
thread you believe is fixed. `gate.sh` counts a thread as unresolved when it is
unresolved or was resolved by the PR author. Reply and let the reviewer close
it.

The two rules are one design. You answer; the gatekeeper judges whether the
answer landed and resolves. A CI failure is safe to clear yourself because CI
re-runs and re-judges independently — you certify nothing. A review thread has
no re-judge, so resolving it is an assertion rather than a verification, and an
assertion from the author is what the condition exists to refuse.

After every item attempted, append exactly one `item-worked` record before
moving on, including failed and blocked attempts. Its fact object carries the
owner, durable item pointer, action taken, outcome, and every external return
used in the report. Add fields such as `pr`, `head_sha`, or `exit_code` when
observed; do not hide them in narration. For example:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/trace.sh" append item-worked \
  "$(jq -cn --arg owner "$lease_owner" --arg repo "$repository" \
    --arg ref "$item_ref" --arg action "$item_action" \
    --arg outcome "$item_outcome" \
    '{owner: $owner, repo: $repo, ref: $ref, action: $action,
      outcome: $outcome}')" \
  "$item_narration"
```

Use `{}` for `item_narration` when no reasoning is needed. Increment the
worked-item count only after this append succeeds. Report the pull request
number and stop work on that item once its handoff is complete. If nothing has
merged in a while, that is information for the owner, not a reason to merge.

## 5. Preserve durable state

- Work that outlives this pass belongs in a GitHub issue in the repository that
  owns it, never in a session task list. If a list is forming, file it.
- Decisions the owner makes belong in a memory file because they exist nowhere
  else.
- Anything learned that changes how the next pass runs belongs in the
  repository — a rule, a spec, or an issue comment.

Filing an issue and closing one are both the builder's own judgment inside its
boundary, not the principal's, and each is only safe to make unsupervised
because it is cheap to undo — a filed issue is undone by closing it, a closed
issue by reopening it. At the moment of either action, append a
`decision-taken` trace record naming that reversal. Keep it separate from the
item's `item-worked` record: that one covers what happened to the item, this
one covers how to undo the action taken on it.

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/trace.sh" append decision-taken \
  "$(jq -cn --arg owner "$lease_owner" --arg repo "$repository" \
    --arg ref "#$item_number" --arg decision "$item_decision" \
    --arg reversal "$item_reversal" \
    '{role: "builder", owner: $owner, repo: $repo, ref: $ref,
      decision: $decision, reversal: $reversal}')" \
  "$item_decision_narration"
```

For a filed issue, `$item_decision` is `filed issue` and `$item_reversal` is
`close <repo>#<new issue number>`. For a closed one, `$item_decision` is
`closed issue` and `$item_reversal` is `reopen <repo>#<ref>`. Use `{}` for
`$item_decision_narration` when there is no reasoning beyond the action
itself.

## 6. Report and stop

Report briefly, visually, and inline; do not attach a file. Never give a bare
issue number without its title. Say what landed, what failed, and what needs
the owner. Name what was sampled rather than verified.

Stop when the queue is drained or every remaining item is blocked on an owner
decision. Do not invent work to stay busy; an empty queue is a good outcome
and should be reported in one line.

## 7. End the trace and release the builder lease

Route every normal or error path after acquisition through this cleanup,
including a sweep failure, config failure, trace failure, and failure midway
through an item. First append `pass-ended`; its fact object records the same
builder owner, observed outcome, and worked-item count. Narration may explain
why an incomplete pass stopped, but must not replace those facts. Then release
the named lease with the exact owner retained in step 2:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/trace.sh" append pass-ended \
  "$(jq -cn --arg owner "$lease_owner" --arg outcome "$pass_outcome" \
    --argjson worked "$worked_items" \
    '{owner: $owner, outcome: $outcome, worked_items: $worked}')" \
  "$pass_end_narration"
MANDATE_LEASE_NAME=builder.lease \
  bash "${CLAUDE_PLUGIN_ROOT}/scripts/lease.sh" release "$lease_owner"
```

Use `{}` for `pass_end_narration` when there is no reasoning to report. If the
`pass-ended` append fails, still attempt the release and report both outcomes.
Treat a release failure as an error and tell the principal; never remove or
overwrite the lease file directly. A process crash may prevent cleanup, which
is why the lease expires and the next builder pass can reclaim it.
