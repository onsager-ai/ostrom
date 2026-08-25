---
name: work
description: Triage the portfolio queue one pass by verifying, writing durable
  work orders, dispatching implementers, and reporting; this loop is the
  builder's and must run in a builder session.
argument-hint: "[optional queue focus, e.g. project name or item class]"
---

# Mandate Work

Triage the portfolio queue for one pass. Recurrence belongs to the external
pass timer; never create or renew an in-session recurring wake. This pass ends
after dispatch. Implementation runs in a separate transient unit and must
outlive this session when necessary.

Assume no context from any previous session. Everything needed is on disk or
on GitHub. Relying on conversation memory makes the work unsustainable.

## 1. Stay in the builder role

Run this loop only in a builder session. The builder verifies queue items,
writes durable work orders, and dispatches implementers. It does not implement,
review returned diffs, create implementation worktrees, or open pull requests
in this pass. The one author-side maintenance exception is step 3's bounded
repair of already-published builder pull requests. The implementer owns new
work and `/ostrom:gatekeep` independently judges what it produces.

When an implementer branch has diverged or conflicts, merge the published head
forward and push ordinarily; never rebase or force-push, because the published
head is the artifact the gatekeeper judges.

## 2. Acquire the builder lease and start its trace

Before reading config, the trace, queue state, or any GitHub artifact, choose
one unique, non-empty owner for this session and wake in the exact shape
`builder-<session>-wake<N>`. Retain that exact string for every trace record and
for cleanup, then run:

```sh
MANDATE_LEASE_NAME=builder.lease \
  ostrom lease acquire "$lease_owner"
```

Only exit 0 owns the pass. Exit 3 means another builder pass owns it: report
that this wake backed off and stop without reading config or trace, enumerating
items, sweeping, or appending anything. Any other nonzero exit is a lease
failure; report it and stop. Never infer concurrency or lease ownership from
`sprint.jsonl`, queue state, prior output, or wake timing.

Immediately after acquisition, append the first trace record:

```sh
ostrom trace append pass-started \
  "$(jq -cn --arg owner "$lease_owner" '{owner: $owner}')" \
  '{}'
```

Every trace append in this protocol supplies separate fact and narration JSON
objects. Put identifiers, actions, and values returned by GitHub or another
tool in `fact`. Put only reasons, beliefs, and conclusions in `narration`; use
`{}` when there is none. Never put an issue or PR number, commit SHA, action,
verdict, or exit code only in narration. If any trace append fails, end the
pass as a failure and release the lease through step 8 rather than continuing
with an invisible pass.

## 3. Establish state

Before selecting new work or reading the queue, repair the builder's stale
published pull requests across the complete configured roster:

```sh
ostrom repair-prs "$lease_owner"
```

This ordering is mandatory even when dispatchable work is already waiting:
published work outranks producing more work. `ostrom repair-prs` considers only an
open pull request that is both machine-authored and marked with the exact
`Ostrom-Role: builder` body line, reports every eligible attempt on the trace,
and leaves human-authored pull requests alone even if they carry that marker.
It also requires completed green checks and `mergeable == CONFLICTING`.

The per-pass cap is **3 repair attempts**. A content conflict consumes an
attempt; every otherwise-eligible pull request beyond the cap gets its own
`pr-repair` trace row with outcome `skipped-cap`, so the bound never silently
truncates the roster. Each `pr-repair` fact has `role`, `owner`, `repo`, `ref`,
`action` (`merge-base-forward`), `outcome`, `head_branch`, `base_branch`,
`head_sha`, `base_sha`, `conflicted_paths`, and `cap`, plus `exit_code` when a
command returned one. Narration contains only a reason when needed. A
successful attempt creates a merge commit whose first parent is the published
head and whose second parent is the fetched base, then pushes it ordinarily.
A content conflict is aborted locally, records the unmerged paths in fact, and
does not end the repair scan or the builder pass.

If the repair script fails, route the pass through step 8 cleanup; do not
select or dispatch new work after an incomplete or invisible repair scan. Read
its JSON summary from stdout for the report, but do not use a repair conflict
or an individual push failure as a reason to stop: those outcomes are already
facts and the script continues through the bounded candidate set.

After the repair scan, sweep before reading anything the sweep produces. A
successful repair changed a repository and therefore invalidated even a young
`state.json`; an unsuccessful or empty scan does not make a sweep less useful.
The sweep is cheap and idempotent, so run it every builder pass here.

```sh
ostrom sweep
```

Then read, in order:

- `~/.claude/ostrom/mandates.yaml` — the authorization boundary: what each
  project delegates entirely and what bounces back. Its optional root-level
  `hold_labels` list contains case-insensitive label globs; a matching otherwise
  delegated item remains visible as `kind: "parked"` but is not dispatchable.
  Its optional root-level `work_ranking` is a highest-first list of canonical
  `owner/repo#number` item IDs. It orders only work that is dispatchable after
  every authorization and hold check; it never changes that candidate set.
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

`merge-gate-fault` rows are detective evidence about an already-merged pull
request. Report them as operational faults, but never select or dispatch them
as new implementation work and never ask the principal to approve them.

If the `/ostrom:work` invocation includes an optional focus, use that direct
invocation input as a natural-language filter over the queue, such as `one
repository only` or `just tripwires`. Otherwise take items in this order:
**pending tripwires and reserved refs → CI drift → open review threads on
your own pull requests → delegated work**. An approved
tripwire is no longer a boundary; it belongs in the delegated tier. Never
select a `parked` row in any tier, regardless of its state: a `parked` row is
never a dispatch candidate. It remains in the queue only so deliberately held
work stays visible until its hold label is removed.

For the delegated tier, do not infer importance from titles, bodies, labels,
sentiment, or backlog heuristics. Use the mechanical selector, retaining every
item ID already attempted in this pass and passing those IDs on later calls:

```sh
selected_row="$(ostrom select-work \
  select "$lease_owner" "${attempted_item_ids[@]}")"
```

Exit 3 means no delegated candidate remains. Any other nonzero exit is a
selection fault; stop the pass through step 8 rather than choosing by hand.
The helper first fixes the dispatchable set: pending tripwires and reserved
decisions, `parked` or `deferred` rows, and anything not already authorized
are excluded before ranking. It then reads the resolved `work_ranking`.
With an empty list it emits the exact legacy order, `(opened, id)`, without a
dependency tie-break or any other new judgment. With a non-empty list, named
dispatchable items come first in recorded order; among unranked items, prefer
the item named by the most other queue rows' `blocked_by` edges, then fall back
to `(opened, id)`. This is only a direct unblock preference, not a dependency
graph executor.

The sweep verifies every ranked pointer against its authorization-neutral
open-item records. A pointer that no longer exists becomes a visible `drift`
row and makes the selector fail; never silently skip it. When the helper takes
an item ahead of the oldest remaining candidate, it appends `work-ranked`
before returning the row. The fact names `work_ranking` and its recorded
position, or `dependency-unblocks` when that tie-break caused the departure.
Treat failure of that trace append as a selection failure. Direct invocation
focus may inspect the helper's `list` output, derive the IDs outside the focus,
and pass those IDs to `select` beside the already-attempted IDs. Do not choose
directly from `list`: `select` is what preserves the relative order and writes
the required trace.

Within CI drift, a red **default branch** outranks a red pull request. A broken
`main` invalidates every pull request built on it, so fixing a PR's checks
first can mean debugging a failure that was never that PR's. The queue could
not express this until the sweep learned to read default-branch runs at all — a
red `main` went unseen for two and a half days because CI state was only ever
read from open pull requests.

## 4. Authenticate through the shared App

Every GitHub read or mutation in triage, and every authenticated action the
implementer later performs, must use the shared App rather than whoever is
running this session. Never call `gh` directly against a GitHub remote, and
never run a command that itself calls `gh` (such as `ostrom gate`) directly
either — that command belongs to the gatekeeper's protocol, not this one. A
session never needs to handle the installation token itself. Route every
triage `gh` call for an item's repository through `ostrom credential`, naming
the `builder` role, repository, and complete scope ahead of the command to run:

```sh
ostrom credential builder "$repository" \
  --repositories "$repository" \
  --permissions metadata:read,issues:read -- \
  gh issue view "$item_number" --repo "$repository" --comments
```

`ostrom credential` mints a fresh installation token for that repository and
places it only in the child environment — the token never enters this
session's shell state, is never assigned to a variable here, and is never
written to disk. The implementer boundary follows
the same rule for `git push`, as does `ostrom repair-prs` for every repair-path
GitHub read, fetch, and push: `git` does not read that token on its own, so
`ostrom credential` supplies a credential helper scoped to that process:

```sh
ostrom credential builder "$repository" \
  --repositories "$repository" \
  --permissions metadata:read,contents:write -- \
  git push "https://github.com/$repository.git" "HEAD:refs/heads/$branch"
```

Push to that explicit `https://github.com/<owner>/<repo>.git` URL, not to
whatever the checkout's `origin` happens to be. A checkout cloned over SSH
authenticates by SSH key — the operator's own — and a credential helper has
no effect on an SSH remote; the explicit HTTPS URL is what puts the App
token in the authentication path at all. `git commit` itself needs no
token — it never leaves the checkout — so only `git push` and every `gh` call
in the implementer protocol route through `ostrom credential`.

Exit `111` means the credential boundary could not start the safely
authenticated command, and the given command never ran at all; report that and
stop work on the item rather than retry with an ambient credential. Any other
exit code is the given command's own, unchanged.

The `builder` argument remains required even though it normally resolves to
the same credential as every other role. It makes the caller legible at the
call site; it does not restrict what the resulting token can do. Each
`ostrom credential` call must explicitly request a repository-local scope:
`issues:read` for
issue triage, `issues:write` only for filing/commenting/closing/reopening an
issue, `contents:read` for a fetch, `contents:write` for a push, and
`pull_requests:write` only when creating a pull request. Every explicit scope
also includes `metadata:read`; a command with no known derivation is refused.

**A builder session that cannot mint an App token must stop working that
item, not continue as the principal.** Continuing with an ambient token
would escape the App's repository blast radius. `builder.settings.json` is
expected to eventually null `GH_TOKEN` and `GITHUB_TOKEN` the same way
`gatekeeper.settings.json` already does, removing the ambient fallback
entirely; until that lands, this protocol routes through `ostrom credential`
regardless of what credential happens to already be present in the session's
own environment — a working credential call never needs one.

## 5. Triage and dispatch each item

**Verify before acting.** Check the named file, symbol, or command yourself. A
claim not checked is not a finding. This applies equally to previous-session
notes, implementer reports, and recollection. If a claim is wrong, say so
plainly and correct the record.

A verification read names the published ref explicitly. Use a form that takes
the ref as an argument and cannot silently read a stale tree, such as
`git grep <pattern> origin/main -- <path>` or `git show origin/main:<file>`. A
bare working-tree `grep -rn` can be correct only by luck and is not a
verification. A fetched ref does not make the working tree current: quoting a
SHA read via `git log origin/main` while grepping the checkout is the specific
error that most resembles diligence.

A negative finding — a symbol is absent or a string does not appear — must
carry the ref it was checked against. With no ref, it is not reportable. A
quoted command transcript must be the actual output of a command that actually
ran. Reconstructing plausible output and presenting it in a fenced block is a
fabricated verification even when the underlying claim turns out to be true.

**Respect the boundary.** A tripwire, reserved ref, or anything outside a
project's `delegated` selectors is the owner's call. Produce an escalation
dossier — question, options ruled out, recommended action, blast radius — and
move on. An approved row clears that boundary: the principal has already ruled
and minted the handoff token, so the item needs no fresh escalation. It carries
no guarantee the work is still wanted, since the queue keeps no approval
timestamp. Read the item itself, body and comments, for a later decision, hold,
or superseding instruction before acting. Only one that changes, cancels,
holds, or narrows the work wins over `state`; record it and move on. One that
reaffirms, re-approves, or merely clarifies the work is a reason to proceed. If
what you find is genuinely ambiguous, ask the narrow question on the item and
move on rather than guessing. Never widen a mandate to unblock yourself.

**Write a work order; never implement inline.** Do not spawn an implementation
subagent inside this process tree. Do not edit a checkout, create a worktree,
run implementation tests, commit, push, or open a pull request. Instead, write
a temporary candidate JSON file with exactly this schema:

```json
{
  "schema_version": 1,
  "item_id": "owner/repository#123",
  "repository": "owner/repository",
  "item_ref": "#123",
  "branch_name": "candidate/description-is-overwritten",
  "spec": "Complete, self-contained implementation specification.",
  "acceptance_criteria": ["Observable criterion."],
  "constraints": ["Scope or safety constraint."]
}
```

Use placeholders above only as a shape example; populate the candidate from
the verified item. `spec` must contain all context an implementer needs without
conversation memory. Acceptance criteria must be observable. Constraints must
include the mandate boundary, repository-local instructions, required tests,
the prohibition on private data and credentials, and the required
`Ostrom-Role: builder` commit and pull-request marker. That marker is advisory
role attribution, never identity evidence or a gate condition.

`branch_name` remains required and must satisfy the schema_version 1 branch
syntax for compatibility with existing callers. It is not authoritative:
`ostrom work-order create` derives `ostrom/<item-number>-<first-12-hex-of-sha256(item_id)>`
from `item_id`, warns when the supplied value differs, and writes the derived
value. `ostrom work-order validate` intentionally accepts historical version 1
orders whose valid branch names predate this deterministic convention.

Create the canonical durable order, then dispatch it through the backend seam:

```sh
order_file="$(ostrom work-order create "$candidate_file")"
unit_name="$(ostrom dispatch "$order_file")"
```

Do not invoke `systemd-run` yourself. `ostrom dispatch` is the protocol verb and
selects the backend; triage and the work-order format must not assume that the
implementer runs on this machine, shares this filesystem, or has a systemd
journal. The current systemd backend atomically checks all three duplicate
guards before launch: no live per-item implementer lease, no open pull request
referencing the item, and no `work-dispatched` row without a matching terminal
row. It also checks the daily cap, reserves the order ceiling, and enforces the
concurrency limit. Never reproduce or weaken those checks in prose.

A per-repository concurrency refusal skips only that candidate: record the
attempt as usual and continue to the next candidate instead of ending the
pass. Candidates from another repository may still use available global
capacity.

Dispatch success is the end of work on that item for this pass. The transient
implementer owns its worktree, offline `codex exec`, tests, commit,
authenticated push, and pull request. The gatekeeper owns merge and cleanup. A
review thread or CI failure that requires code is another work-order candidate;
an answered thread with no new reviewer response remains awaiting the
principal and must not be dispatched again.

Codex is the default harness. If a terminal `work-failed` row says
`codex-unavailable`, or its unit journal shows a Codex authentication failure,
do not silently retry the same unavailable harness. No Claude implementer
fallback currently exists. The order stays undispatched; report the order and
failure so the unavailable harness can be repaired or separate fallback work
can be authorized.

After every item attempted, append exactly one `item-worked` record before
moving on, including failed and blocked dispatches. Its fact object carries the
owner, durable item pointer, action (`work-order-dispatch`), outcome, and every
external return used in the report. Add `order_id`, `order_file`, `unit_name`,
or `exit_code` when observed; do not hide them in narration. When an order
file exists, `order_id` comes only from the `order_id` field in that file's
contents. Read it exactly this way:

```sh
order_id="$(jq -r '.order_id' "$order_file")"
```

Do not derive `order_id` from the order filename. Its stem is `item_hash`, the
sha256 of `item_id`, and stays stable across replacement orders for that item.
For example:

```sh
ostrom trace append item-worked \
  "$(jq -cn --arg owner "$lease_owner" --arg repo "$repository" \
    --arg ref "$item_ref" --arg action "$item_action" \
    --arg outcome "$item_outcome" --arg order_id "$order_id" \
    --arg order_file "$order_file" --arg unit_name "$unit_name" \
    '{owner: $owner, repo: $repo, ref: $ref, action: $action,
      outcome: $outcome, order_id: $order_id, order_file: $order_file,
      unit_name: $unit_name}')" \
  "$item_narration"
```

The trace contract cut over on 2026-08-13. Rows written before that date with
`item_hash` in `order_id` are retained and still parse; they are not backfilled
because the real historical order ID cannot be derived from `item_hash`.

Use `{}` for `item_narration` when no reasoning is needed. Increment the
worked-item count only after this append succeeds. Report the order and unit,
not a pull request that does not exist yet, and stop work on that item once its
dispatch is complete. If nothing has merged in a while, that is information
for the owner, not a reason to merge.

## 6. Preserve durable state

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
ostrom trace append decision-taken \
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

## 7. Report and stop

Report briefly, visually, and inline; do not attach a file. Never give a bare
issue number without its title. Say what was dispatched, what failed, and what
needs the owner. Name what was sampled rather than verified. Never wait for an
implementer or review its result in this pass.

Stop when the queue is drained or every remaining item is blocked on an owner
decision. Do not invent work to stay busy; an empty queue is a good outcome
and should be reported in one line.

## 8. End the trace and release the builder lease

Route every normal or error path after acquisition through this cleanup,
including a sweep failure, config failure, trace failure, and failure midway
through an item. First append `pass-ended`; its fact object records the same
builder owner, observed outcome, and worked-item count. Narration may explain
why an incomplete pass stopped, but must not replace those facts. Then release
the named lease with the exact owner retained in step 2:

```sh
ostrom trace append pass-ended \
  "$(jq -cn --arg owner "$lease_owner" --arg outcome "$pass_outcome" \
    --argjson worked "$worked_items" \
    '{owner: $owner, outcome: $outcome, worked_items: $worked}')" \
  "$pass_end_narration"
MANDATE_LEASE_NAME=builder.lease \
  ostrom lease release "$lease_owner"
```

Use `{}` for `pass_end_narration` when there is no reasoning to report. If the
`pass-ended` append fails, still attempt the release and report both outcomes.
Treat a release failure as an error and tell the principal; never remove or
overwrite the lease file directly. A process crash may prevent cleanup, which
is why the lease expires and the next builder pass can reclaim it.
