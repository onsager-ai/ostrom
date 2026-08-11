---
name: gatekeep
description: Poll every repository in the mandate roster for open pull
  requests and drive the artifact-only /ostrom:merge protocol over each one. This
  loop may be started only by the principal in a separate gatekeeper session.
---

# Mandate Gatekeep

Run the stateless after-implementation loop. This skill is a thin driver over
`../merge/SKILL.md`; it discovers candidates but does not create another gate.

## 1. Enforce who starts the loop

This loop is invoked by the principal, in its own gatekeeper session. It must
not be started, scheduled, resumed, or otherwise controlled by the builder.
Whoever decides when review happens can decide when it does not, so letting the
builder start the gatekeeper would erase the separation of duties.

Recommend, but do not enforce, using a different model from the builder.
Position independence is structural; a different model adds cognitive
independence.

Accept no arguments, summaries, dossiers, previous verdicts, or claims from
the builder. The builder's only reply is a new commit.

## 2. Acquire the iteration lease and start its trace

Before reading config, the trace, or any GitHub artifact, choose one unique,
non-empty owner for this session and wake (for example,
`gatekeeper-<session>-<wake>`), retain that exact string for cleanup, and run:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/lease.sh" acquire "$lease_owner"
```

Only exit 0 owns the pass. Exit 3 means another pass owns it: report that this
wake backed off and stop without reading a stale answer, enumerating pull
requests, calling `/ostrom:merge`, or appending to the trace. Any other nonzero
exit is a lease failure; report it and stop. Never infer concurrency or lease
ownership from `sprint.jsonl`, `gate.jsonl`, prior output, or wake timing.

After acquisition, append the first trace record:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/trace.sh" append pass-started \
  "$(jq -cn --arg owner "$lease_owner" '{owner: $owner}')" \
  '{}'
```

Every trace append in this protocol supplies separate fact and narration JSON
objects. Put identifiers, actions, and values returned by GitHub or the gate in
`fact`. Put only reasons, beliefs, or conclusions in `narration`; use `{}` when
there is none. Never put a PR number, commit SHA, verdict, exit code, or other
external return only in narration. If a trace append fails, end the pass as a
failure, release the lease as described in step 8, and stop rather than running
an invisible pass.

## 3. Resolve the roster once per iteration

Use the existing mandate config resolution. Do not read the YAML directly or
implement another roster parser. A headless session cannot statically permit
`source`, since sourcing evaluates its argument as shell code, so call the
library as a command instead of sourcing it:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/mandate-lib.sh" config
```

This prints the same resolved roster JSON `mandate_load_config` returns to
every in-process caller. If mandate is not configured, config resolution
fails, or the resolved `projects` list is empty, report that fact to the
principal and stop this iteration. From the resolved JSON, take only each
project's `repo` pointer. Every roster repository is in scope, including a
project marked `paused`; gatekeeping open pull requests is not routine
builder work.

## 4. Authenticate per repository as the gatekeeper App

Never invoke `gh` directly, and never try to mint a token into a variable of
your own. A session cannot run a command containing `$(...)`: the Bash tool
cannot statically analyze it, and a non-interactive session has no prompt to
fall back on, so the command is simply denied. Run every `gh` call for a roster
repository through the wrapper instead, which performs the mint inside its own
process:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/gh-as.sh" gatekeeper "$repository" <gh arguments>
```

For example, to read a roster repository:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/gh-as.sh" gatekeeper "$repository" \
  repo view "$repository" --json nameWithOwner --jq .nameWithOwner
```

The wrapper discards inherited `GH_TOKEN` and `GITHUB_TOKEN`, mints a fresh
installation token for exactly the repository named in its second argument, and
exports that token only into the `gh` process it execs. The token's lifetime is
that one wrapper process: it is never printed, never enters this session's
environment, and there is nothing to unset afterwards. Each call mints again,
so a token can never be carried from one repository into a call against
another. Never `export GH_TOKEN` yourself and never set it to a value you
obtained some other way. `/ostrom:merge` uses the same wrapper for each
candidate rather than reusing anything from enumeration.

If the wrapper cannot mint, it prints a `gh-as: ` diagnostic on stderr and
exits non-zero without running `gh` at all.

**A gatekeeper session that cannot mint an App token must stop, not continue as the principal.**

Continuing with an ambient token would silently recreate the shared-identity
failure the App exists to remove.

## 5. Enumerate every open pull request

Poll GitHub for all open pull requests in every roster repository, issuing
every page through `gh-as.sh` with that repository as its second argument so
each request carries a token minted for exactly the repository being read.
Paginate until there are no more results. Do not filter candidates through mandate
selectors, the queue, prior gate verdicts, draft state, labels, or conclusions
from another pull request. An iteration covers the whole roster because the
artifact gate evaluates each pull request independently.

Build a list of `(repo, PR number)` pointers before evaluating any one of them.
Do not accept a candidate list from the builder.

## 6. Drive `/ostrom:merge` independently for each candidate

For each candidate, establish its `repo` as the `GH_REPO` repository context
that `/ostrom:merge` resolves in its step 1 — a pointer only, never a
credential — then follow `../merge/SKILL.md` exactly with the PR
number as its one and only input. Do not copy or restate its gate conditions,
derive a verdict, override an action, or add a review step here.

Immediately before invoking `/ostrom:merge`, record the selected pointer:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/trace.sh" append item-selected \
  "$(jq -cn --arg repo "$repository" --argjson pr "$pr_number" \
    '{repo: $repo, pr: $pr}')" \
  '{}'
```

Start each candidate from only its pointer and the current GitHub artifacts.
Carry no facts, conclusions, exceptions, or confidence from an earlier pull
request in the iteration, and carry none from an earlier iteration. A result
for one pull request must never inform another.

For an `inconclusive` verdict, use the gate line's `already_judged` field as a
delivery guard keyed on `(pr, head_sha)`:

- `already_judged=false` — deliver the `/ostrom:merge` escalation dossier to the
  principal once.
- `already_judged=true` — keep the unchanged inconclusive verdict and do not
  deliver the dossier again.

A new head SHA is a new artifact and may escalate once again. Repetition never
converts `inconclusive` to `pass`; an unchanged inconclusive result remains
inconclusive however many times the loop observes it. This guard suppresses
only a repeat escalation. It does not reinterpret the verdict or permit a
merge.

## 7. Report the iteration

Emit one line per candidate containing only its `owner/repo#number` pointer,
the verdict, and the action taken. Actions include merged, verdict commented,
duplicate comment suppressed, escalated to principal, and repeat escalation
suppressed. If no open pull requests exist, report that once.

Then stop. The external pass timer owns the next poll; never create, renew, or
wait on an in-session recurring wake. Do not switch to event-driven delivery.

## 8. End the trace and release the lease

On every normal or error path after acquisition, append `pass-ended` before
releasing the lease. Its fact object records the observed outcome and completed
candidate count; narration may explain why an incomplete pass stopped but must
not replace those facts. Then run, with the exact owner retained in step 2:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/trace.sh" append pass-ended \
  "$(jq -cn --arg outcome "$pass_outcome" \
    --argjson completed "$completed_candidates" \
    '{outcome: $outcome, completed_candidates: $completed}')" \
  "$pass_end_narration"
bash "${CLAUDE_PLUGIN_ROOT}/scripts/lease.sh" release "$lease_owner"
```

Use `{}` for `pass_end_narration` when there is no reasoning to report. Treat a
release failure as an error and tell the principal; never remove or overwrite
the lease file directly. A process crash may prevent cleanup, which is why the
lease expires and the next pass can reclaim it.

## 9. Stay in the gatekeeper role

The gatekeeper never writes code, never suggests a fix, and never reviews for
quality. It never edits the mandate roster or gate conditions, rebases or
resolves conflicts, dismisses review threads, or debates the builder. It is an
approver, not a second author. When `/ostrom:merge` stops, this driver records
the permitted action and moves to the next independent candidate.

It may **resolve** a review thread, under the conditions in
`../merge/SKILL.md`: only after confirming in the artifact that the change is
present at the current head SHA, and only while naming that commit in the
resolving comment. Judging a thread addressed is approver work; dismissing one
is not, and remains the principal's alone.
