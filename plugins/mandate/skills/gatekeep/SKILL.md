---
name: gatekeep
description: Poll every repository in the mandate roster for open pull
  requests and drive the artifact-only /merge protocol over each one. This
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

## 2. Resolve the roster once per iteration

Use the existing mandate config resolution. Do not read the YAML directly or
implement another roster parser:

```sh
source "${CLAUDE_PLUGIN_ROOT}/scripts/mandate-lib.sh"
mandate_load_config
```

If mandate is not configured, config resolution fails, or the resolved
`projects` list is empty, report that fact to the principal and stop this
iteration. From the resolved JSON, take only each project's `repo` pointer.
Every roster repository is in scope, including a project marked `paused`;
gatekeeping open pull requests is not routine builder work.

## 3. Enumerate every open pull request

Poll GitHub for all open pull requests in every roster repository. Paginate
until there are no more results. Do not filter candidates through mandate
selectors, the queue, prior gate verdicts, draft state, labels, or conclusions
from another pull request. An iteration covers the whole roster because the
artifact gate evaluates each pull request independently.

Build a list of `(repo, PR number)` pointers before evaluating any one of them.
Do not accept a candidate list from the builder.

## 4. Drive `/merge` independently for each candidate

For each candidate, establish its `repo` as the `GH_REPO` environment context
used by `gh repo view`, then follow `../merge/SKILL.md` exactly with the PR
number as its one and only input. Do not copy or restate its gate conditions,
derive a verdict, override an action, or add a review step here.

Start each candidate from only its pointer and the current GitHub artifacts.
Carry no facts, conclusions, exceptions, or confidence from an earlier pull
request in the iteration, and carry none from an earlier iteration. A result
for one pull request must never inform another.

For an `inconclusive` verdict, use the gate line's `already_judged` field as a
delivery guard keyed on `(pr, head_sha)`:

- `already_judged=false` — deliver the `/merge` escalation dossier to the
  principal once.
- `already_judged=true` — keep the unchanged inconclusive verdict and do not
  deliver the dossier again.

A new head SHA is a new artifact and may escalate once again. Repetition never
converts `inconclusive` to `pass`; an unchanged inconclusive result remains
inconclusive however many times the loop observes it. This guard suppresses
only a repeat escalation. It does not reinterpret the verdict or permit a
merge.

## 5. Report the iteration

Emit one line per candidate containing only its `owner/repo#number` pointer,
the verdict, and the action taken. Actions include merged, verdict commented,
duplicate comment suppressed, escalated to principal, and repeat escalation
suppressed. If no open pull requests exist, report that once.

Then wait for the next poll. Recommend a period of 30–60 minutes and let the
principal choose; do not poll faster than the builder's sprint pass. The loop
uses Claude Code's existing recurring-wake mechanism, for example:

```text
/loop 30m /gatekeep
```

Do not build or invoke a separate scheduler and do not switch to event-driven
delivery.

## 6. Stay in the gatekeeper role

The gatekeeper never writes code, never suggests a fix, and never reviews for
quality. It never edits the mandate roster or gate conditions, rebases or
resolves conflicts, resolves or dismisses review threads, or debates the
builder. It is an approver, not a second author. When `/merge` stops, this
driver records the permitted action and moves to the next independent
candidate.
