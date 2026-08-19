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
ostrom lease acquire "$lease_owner"
```

Only exit 0 owns the pass. Exit 3 means another pass owns it: report that this
wake backed off and stop without reading a stale answer, enumerating pull
requests, calling `/ostrom:merge`, or appending to the trace. Any other nonzero
exit is a lease failure; report it and stop. Never infer concurrency or lease
ownership from `sprint.jsonl`, `gate.jsonl`, prior output, or wake timing.

After acquisition, append the first trace record:

```sh
ostrom trace append pass-started \
  "$(jq -cn --arg owner "$lease_owner" '{owner: $owner}')" \
  '{}'
```

Immediately after that successful append, initialize `completed_candidates` to
`0` and `skipped_repos` to `[]`; maintain both values for the rest of the pass.

Every trace append in this protocol supplies separate fact and narration JSON
objects. Put identifiers, actions, and values returned by GitHub or the gate in
`fact`. Put only reasons, beliefs, or conclusions in `narration`; use `{}` when
there is none. Never put a PR number, commit SHA, verdict, exit code, or other
external return only in narration. If a trace append fails, end the pass as a
failure, release the lease as described in step 8, and stop rather than running
an invisible pass.

## 3. Resolve the roster once per iteration

Use the existing mandate config resolution. Do not read the YAML directly or
implement another roster parser. Resolve the same layered roster through the
native CLI:

```sh
ostrom config
```

This prints the resolved roster JSON used by every native caller. If mandate
is not configured, config resolution
fails, or the resolved `projects` list is empty, report that fact to the
principal and end the pass. From the resolved JSON, take only each
project's `repo` pointer. Every roster repository is in scope, including a
project marked `paused`; gatekeeping open pull requests is not routine
builder work.

## 4. Authenticate per repository through the shared App

`gatekeeper.settings.json` sets `GH_TOKEN` and `GITHUB_TOKEN` to empty, so
there is no ambient credential here to discard or fall back to by accident.
Never call `gh` directly, and never run a command that itself calls `gh`
(such as `ostrom gate`) directly either. A session never needs to handle the
installation token itself. Route every `gh` call for a roster repository
through `ostrom credential`, naming the role, repository, and complete scope
ahead of the command to run:

```sh
ostrom credential gatekeeper "$repository" \
  --repositories "$repository" \
  --permissions metadata:read,pull_requests:read -- \
  gh pr list --repo "$repository" --state open --limit 100 --json number
```

`ostrom credential` mints a fresh installation token for that repository and
places it only in the child environment — the token never enters this
session's shell state, is never assigned to a variable here, and is never
written to disk. Exit `111` means the credential boundary could not start the
safely authenticated command, and the given command never ran at all. Any
other exit code is the given command's own, unchanged.

The required `gatekeeper` argument names the caller at the call site; it does
not narrow the shared token. The mandatory flags make the repository-local
`metadata:read,pull_requests:read` scope explicit; every pagination retry must
repeat the same command shape. The gatekeeper's own role is
recorded in its `decision-taken` trace record, not stamped onto the merge commit — see
`/ostrom:merge` step 4 for why. An `Ostrom-Role: builder` trailer arriving on a
commit under review was written by the builder itself, so it is self-asserted
advisory metadata, not evidence of who acted and never an input to the gate.

Keep these two exit-`111` cases distinct:

- **Credentials cannot be loaded at all.** An error saying that the secrets
  file is absent, neither `gatekeeper` nor shared credentials are configured,
  a credential field is missing or malformed, the private key is unavailable,
  or another session-wide authentication prerequisite failed means this pass
  has no authority it can use. Do not retry the repository call. Set the pass
  outcome to `error`, report the credential-loading failure without exposing
  credential values, append the terminal row, release the lease, and end the
  pass.
- **Minting fails for one repository.** Any other exit `111` from a correctly
  formed roster-repository invocation is scoped to that repository. Retry the
  exact same `ostrom credential` invocation once immediately, still with the
  `gatekeeper` role and that repository. If the retry also exits `111`, add the
  repository once to `skipped_repos`, report that it was skipped, discard any
  partially enumerated candidates for it, and continue to the next repository.
  One immediate retry is the deliberate bound: it cheaply absorbs a transient
  installation lookup or token exchange failure without repeatedly delaying a
  pass against a genuinely broken repository.

**No exit-`111` path may run the command under an ambient credential, continue
as the principal, call `gh` directly, or fall back to any token already in the
environment.** Ending the pass for an unusable credential configuration and
skipping a repository after its bounded retry both fail closed. An ambient
token would escape the App's repository blast radius.

## 5. Enumerate every open pull request

Poll GitHub for all open pull requests in every roster repository, issuing
every call through `ostrom credential` as in step 4. Each invocation mints and uses
its own token and exits with it, so there is no persisted `GH_TOKEN` to
unset between repositories or between pages. Paginate until there are no more results. Do not filter candidates through mandate
selectors, the queue, prior gate verdicts, draft state, labels, or conclusions
from another pull request. An iteration covers the whole roster because the
artifact gate evaluates each pull request independently.

Apply step 4's bounded retry to every pagination call. If a repository is
skipped after a later page fails, discard the earlier pages from that
repository so a partial enumeration is never mistaken for its complete set.
Continue enumerating every other roster repository.

Build a list of `(repo, PR number)` pointers before evaluating any one of them.
Do not accept a candidate list from the builder. Judge every candidate gathered
from successfully enumerated repositories even when `skipped_repos` is not
empty.

## 6. Drive `/ostrom:merge` independently for each candidate

For each candidate, establish its `repo` as the `GH_REPO` environment context
used by `gh repo view`, then follow `../merge/SKILL.md` exactly with the PR
number as its one and only input. Do not copy or restate its gate conditions,
derive a verdict, override an action, or add a review step here.

Immediately before invoking `/ostrom:merge`, record the selected pointer:

```sh
ostrom trace append item-selected \
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
suppressed. Also emit one line per skipped repository naming its `owner/repo`
pointer and that token minting still failed after one retry. If no open pull
requests exist in the repositories that were successfully enumerated, report
that once; do not describe a skipped repository as having no open pull
requests.

Then stop. The external pass timer owns the next poll; never create, renew, or
wait on an in-session recurring wake. Do not switch to event-driven delivery.

## 8. End the trace and release the lease

On every normal or error path after acquisition, append `pass-ended` before
releasing the lease. Increment `completed_candidates` only after a selected
candidate has returned from `/ostrom:merge` with its action recorded. Add each
repository that exhausts the retry in step 4 to `skipped_repos` once. The
terminal fact uses the two values maintained since step 2 to record the observed
outcome, truthful completed-candidate count, and skipped-repository list;
narration may explain why an incomplete pass stopped but must not replace those
facts.

Use the existing outcome `completed` when the pass reaches the end without a
skipped repository. Use outcome `partial` when the pass reaches the end after
skipping one or more repositories, including when it successfully judged
candidates in the other repositories; a productive pass with skips is not
`error`. Reserve `error` for a pass-ending failure such as credentials that
cannot be loaded at all or a trace failure. Then run, with the exact owner
retained in step 2:

```sh
ostrom trace append pass-ended \
  "$(jq -cn --arg outcome "$pass_outcome" \
    --argjson completed "$completed_candidates" \
    --argjson skipped "$skipped_repos" \
    '{outcome: $outcome, completed_candidates: $completed,
      skipped_repos: $skipped}')" \
  "$pass_end_narration"
ostrom lease release "$lease_owner"
```

Use `{}` for `pass_end_narration` when there is no reasoning to report. Treat a
release failure as an error and tell the principal; never remove or overwrite
the lease file directly. A process crash may prevent cleanup, which is why the
lease expires and the next pass can reclaim it.

## 9. Stay in the gatekeeper role

The gatekeeper never writes code, never suggests a fix, and never reviews for
quality. It never edits the mandate roster or gate conditions, rebases or
resolves conflicts, dismisses review threads, or debates the builder. It is a
judge, not a second author. When `/ostrom:merge` stops, this driver records
the permitted action and moves to the next independent candidate.

It may **resolve** a review thread, under the conditions in
`../merge/SKILL.md`: only after confirming in the artifact that the change is
present at the current head SHA, and only while naming that commit in the
resolving comment. Judging a thread addressed is judge work; dismissing one
is not, and remains the principal's alone.
