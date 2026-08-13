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
review returned diffs, create worktrees, commit, push, or open pull requests in
this pass. The implementer owns those steps and `/ostrom:gatekeep` independently
judges what it produces.

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
pass as a failure and release the lease through step 8 rather than continuing
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
never run a script that itself calls `gh` (such as `gate.sh`) directly either —
that script belongs to the gatekeeper's protocol, not this one. A session's
Bash tool statically rejects
command substitution before permission matching, so this step cannot capture
`app-token.sh`'s output into a variable (`token="$(app-token.sh ...)"`) the way
an interactive shell would — no allow rule can fix that rejection, because it
never reaches allow-rule matching in the first place. Route every triage `gh`
call for an item's repository through `gh-as.sh`, naming the `builder` role and
the repository ahead of the command to run:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/gh-as.sh" builder "$repository" \
  gh issue view "$item_number" --repo "$repository" --comments
```

`gh-as.sh` mints a fresh installation token for that repository inside its
own process, exports it only there, and `exec`s the given command — the
token never enters this session's shell state, is never assigned to a
variable here, and is never written to disk. The implementer wrapper follows
the same rule for `git push`: `git` does not read that token on its own, so
`gh-as.sh` supplies a credential helper scoped to that process:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/gh-as.sh" builder "$repository" \
  git push "https://github.com/$repository.git" "HEAD:refs/heads/$branch"
```

Push to that explicit `https://github.com/<owner>/<repo>.git` URL, not to
whatever the checkout's `origin` happens to be. A checkout cloned over SSH
authenticates by SSH key — the operator's own — and a credential helper has
no effect on an SSH remote; the explicit HTTPS URL is what puts the App
token in the authentication path at all. `git commit` itself needs no
token — it never leaves the checkout — so only `git push` and every `gh` call
in the implementer protocol route through `gh-as.sh`.

Exit `111` means `gh-as.sh` itself could not authenticate and the given
command never ran at all; report that and stop work on the item rather than
retry with an ambient credential. Any other exit code is the given command's
own, unchanged.

The `builder` argument remains required even though it normally resolves to
the same credential as every other role. It makes the caller legible at the
call site; it does not restrict what the resulting token can do.

**A builder session that cannot mint an App token must stop working that
item, not continue as the principal.** Continuing with an ambient token
would escape the App's repository blast radius. `builder.settings.json` is
expected to eventually null `GH_TOKEN` and `GITHUB_TOKEN` the same way
`gatekeeper.settings.json` already does, removing the ambient fallback
entirely; until that lands, this protocol routes through `gh-as.sh` regardless
of what credential happens to already be present in the session's own
environment — a working `gh-as.sh` call never needs one.

## 5. Triage and dispatch each item

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
`work-order.sh create` derives `ostrom/<item-number>-<first-12-hex-of-sha256(item_id)>`
from `item_id`, warns when the supplied value differs, and writes the derived
value. `work-order.sh validate` intentionally accepts historical version 1
orders whose valid branch names predate this deterministic convention.

Create the canonical durable order, then dispatch it through the backend seam:

```sh
order_file="$(bash "${CLAUDE_PLUGIN_ROOT}/scripts/work-order.sh" create "$candidate_file")"
unit_name="$(bash "${CLAUDE_PLUGIN_ROOT}/scripts/dispatch.sh" "$order_file")"
```

Do not invoke `systemd-run` yourself. `dispatch.sh` is the protocol verb and
selects the backend; triage and the work-order format must not assume that the
implementer runs on this machine, shares this filesystem, or has a systemd
journal. The current systemd backend atomically checks all three duplicate
guards before launch: no live per-item implementer lease, no open pull request
referencing the item, and no `work-dispatched` row without a matching terminal
row. It also checks the daily cap, reserves the order ceiling, and enforces the
concurrency limit. Never reproduce or weaken those checks in prose.

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
bash "${CLAUDE_PLUGIN_ROOT}/scripts/trace.sh" append item-worked \
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
