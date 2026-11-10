# Merging

Status: proposed, 2026-11-10. Milestone 2. The operator chose unattended merging by default,
with policy options ([direction.md, question 4](direction.md#open-questions)). This is a design;
none of the merge pass or configuration below exists yet.

## Starting point

The milestone-1 loop ran with human merges ([e2e-m1.md](e2e-m1.md)). Its observed failures included
taking a merge as a close (#676), and treating a Worker waiting for review as stuck (#684).
Unattended merging must preserve those fixes. The policy options are the operator's explicit
request, not a new governance layer.

Current code at `680f96f`:

- `src/worker/dispatch.ts` asks the Worker to commit, push and open a PR. `observe` subscribes its
  ticket to PRs found by branch. The lookup reads number, URL, state and `merged_at`; it does not
  read checks, reviews, mergeability or who merged.
- `src/router/router.ts` derives `pr_opened`, `pr_merged` and `pr_closed` outcomes from that lookup.
  The Router words the reply; `Store.enqueueOutcomeReply` in `src/store/store.ts` queues it and
  ends terminal work atomically. A merge is observed, never performed by Ostrom.
- `src/ostromd/main.ts` runs poll, observe, commands, follow-ups, Watchdog, Router and send, in that
  order. A tick continues after a failed pass and returns non-zero overall.
- `src/worker/workers.ts` serializes turns through in-memory chains. `assignment.turn_started_at`
  is durable, but the queued turns are not. `src/worker/follow-up.ts` records delivery when a turn
  starts. Redirect command receipts can precede delivery (`src/commands/commands.ts`), so checking
  only pending deliveries or only `turn_started_at` cannot prove the Worker is idle.
- Pause changes ticket state immediately. Stop and issue-close cancel a turn and queue cleanup;
  the ticket ends only after cleanup. These paths live in `src/commands/commands.ts`.
- `src/github/cards.ts` derives Working, Recovering, Paused, Stuck, Done or Stopped from local state.
  `src/github/outbox.ts` edits one card and sends marker-deduplicated comments with batching and
  backoff. `src/github/poller.ts` deduplicates immutable triggers; it is not a complete current
  view of edits, review dismissals, thread resolutions or CI.
- `src/budget/budgets.ts` counts wall time from ticket opening, including review waiting, and idle
  time only during a running turn. `src/watchdog/watchdog.ts` records budget/stall episodes;
  `src/watchdog/stuck.ts` recovers twice before pausing and posting Stuck.

## Default: unattended

`ostromd` performs a deterministic merge pass. A Worker never decides or performs the merge; the
Router may word replies but cannot authorize a merge (rule 3). Amend the Worker brief and every
follow-up contract to forbid merging, enabling GitHub auto-merge, dismissing reviews, or resolving
human review threads. This is a runtime contract, not a new permissions boundary: Workers currently
share the operator's GitHub login (`src/github/gh.ts`).

A candidate is a PR in the configured repository, from the exact recorded Worker branch and head
repository, targeting `OSTROM_WORKER_BASE`, owned by one live assignment/ticket. Multiple open PRs
on that branch or multiple live ticket owners block automatic selection. A subscription alone does
not establish ownership. Human policy skips merge requests entirely. Otherwise all conditions hold:

1. The issue remains open; the ticket is open, has no pending pause/stop/close, and has no uncleared
   stall or Worker failure. No budget limit is reached, using `budgetReached` at the instant of
   evaluation. An 80% warning alone does not block. Waiting can exhaust the wall-time budget;
   the operator must extend it before automatic merging can continue.
2. The Worker has no running or queued turn, undelivered follow-up, unhandled command, recovery or
   stop cleanup. Check durable work as well as the runner queue. Reserve that assignment while
   sending the merge request, so no local turn can start concurrently.
3. The PR is open, not draft, and still has the captured head and base SHAs. GitHub reports
   `mergeable=true` and GraphQL `mergeStateStatus=CLEAN`. Null/UNKNOWN waits for another tick;
   conflicts, BEHIND, BLOCKED, UNSTABLE and HAS_HOOKS block this first implementation. CLEAN alone
   is insufficient evidence of CI or protection compliance. [1][2]
4. CI evidence comes primarily from the head commit's `statusCheckRollup`, in the same GraphQL
   query as merge/review state and threads. Require `commit.oid` to equal the captured head,
   `state=SUCCESS`, and read every page of `contexts` (CheckRun or StatusContext). Every required
   check/status from protection or rules must be present with the required identity. Also require
   at least one completed SUCCESS check from this repository's successful `pull_request` workflow
   for this PR; an empty rollup, push run or manual dispatch cannot qualify. Identify its suite,
   matching PR and workflow through GraphQL, as described below. Accept NEUTRAL/SKIPPED contexts
   as GitHub does only alongside that actual successful PR check/workflow. If GitHub evaluates
   required checks on a test merge commit, read its rollup too, tied to this head/base pair. [3][4][6]
5. No effective submitted CHANGES_REQUESTED review remains, from any reviewer, and no unresolved
   review thread remains, including outdated threads. Read every page. Reduce each reviewer's
   submitted decisive reviews in order: APPROVED replaces their previous request for changes;
   DISMISSED clears that review's effect without reviving earlier requests; COMMENTED or a new
   push does not erase an outstanding request. PENDING is not a submitted review. Also require GitHub's review decision not to be
   CHANGES_REQUESTED or REVIEW_REQUIRED. Policy approval requirements, if any, hold too. [2][7]
6. Read the base branch's classic protection and active rules, including inherited rulesets, on
   each final evaluation. Honor required checks and app identities, strict/up-to-date settings,
   required approvals, code owners, stale-review dismissal, last-push approval and conversation
   resolution. Read these ourselves because the operator's login may be an admin: GitHub exempts
   admins from classic protection by default, so API acceptance alone cannot prove compliance. [10]
   Never use an admin override or bypass. A protection read denied or ambiguous 404 is not evidence
   of no protection. Block when applicable requirements cannot be established; merge queues,
   unsupported rules and restricted squash merging require operator action. [8][9]
7. The ready reply has actually been posted, and ten quiet minutes have passed. The timer starts
   at the latest of that post, PR discovery, any head/base change, any completed Worker turn, and
   the latest observed human activity on the issue or PR. Activity includes comments/edits,
   submitted/dismissed reviews, thread replies/resolutions, pushes, PR edits and controls. Refresh
   these directly before merging; the intake cursor alone is insufficient. Newly observed changes
   with no reliable timestamp restart the timer at observation. Treat unmarked actor activity as
   human conservatively; Ostrom's marked cards/replies and CI updates do not restart it. Pending
   review drafts are not visible reliably, so this window cannot promise time for an unseen review.

Ten minutes is a fixed milestone-2 default, not another policy knob. A new head/base or changed
policy invalidates previous readiness and starts a new window after refreshed evidence. CI progress
alone does not reset it, but every check must still pass at send time. A daemon restart retains the
window only after a complete fresh read confirms the same inputs; otherwise it starts again.

Use `PUT /repos/{repo}/pulls/{number}/merge` with `sha=<captured head>` and
`merge_method=squash`. Squash makes one ticket's revisions one base-branch commit and avoids
preserving temporary Worker commits. Record the returned merge SHA as distinct from the tested
head SHA. Require squash to be enabled by repository settings and applicable rules; do not silently
fall back to merge or rebase. Do not delete the branch in this change. [1]

### Card, reply and recorded result

Add card state **Waiting to merge**, with the policy, PR/head, blocking reason or earliest merge
time, and existing pause/stop controls. Under human policy it says **Waiting for a human merge**.
The existing `pr_opened` reply becomes the ready reply and says whether Ostrom intends to merge
automatically after the quiet window or is waiting for a human. It keeps the stable opened-outcome
ID. Ordinary batching remains; the timer starts at confirmed delivery, never at enqueue time.
Later revisions update the card rather than posting repeated ready comments.

On confirmed merge, both human and Ostrom merges yield the same `pr_merged` outcome and **Done**
card. Keep `outcome-a<assignment>-pr<number>-merged` and one done reply on the originating thread.
The done reply identifies who merged and links the PR and merge commit. `pr_closed` remains a
close without merge. A terminal observation cancels remaining Worker work before ending it.

Add small domain records, not verdicts or a check catalogue:

- `merge_candidate`: assignment/PR identity, head/base SHAs, policy snapshot, observed activity
  fingerprint/time, ready reply ID and sent time, quiet deadline, current blocking reason.
- `merge_attempt`: candidate/head, attempt number, method, authenticated actor ID, request time,
  response SHA or error, next retry time, and state `pending`, `confirmed`, `failed` or `unknown`.
  At most one pending/unknown request per PR. Persist it before the HTTP call.
- `pr_merge`: one row per repository/PR, head, merge SHA, merged time, GitHub `merged_by` ID/login,
  origin `ostrom`, `external` or `unknown`, and optional attempt ID. Retain the narrow evidence
  used: observation time, check/run IDs and attempts, review IDs, protection/rule identifiers and
  relevant settings. No full API dumps, transcripts, signatures or model verdicts.

A confirmed response belongs to Ostrom's attempt. A merge observed without one is external, which
may be a human or another tool; do not call every external merge human. After a lost response,
GitHub's actor can match the operator's shared login for either path. Matching actor/head/time is
not proof of who sent the request: record unknown origin unless evidence settles it. Completion
still records `pr_merged`. [1]

Reconcile the merge record, end work and queue the stable outcome reply transactionally. Reuse
`Store.enqueueOutcomeReply`'s semantics, extending it for this reconciliation. Human merge wording
may still use the Router; an Ostrom merge can use a fixed truthful done template so model failure
cannot delay closing work. The Router must skip an outcome already queued by reconciliation.

## Policy options

One repository per daemon today (`src/ostromd/config.ts`). Store the policy in its existing env file
([running.md](running.md)); it applies to that `OSTROM_REPO`, including existing unmerged tickets
after restart. Store the evaluated snapshot for explanation and reset readiness on a change.

| Variable | Values and meaning |
|---|---|
| `OSTROM_MERGE_STRATEGY` | `unattended` (default), `human`, or `approvals` |
| `OSTROM_MERGE_APPROVALS` | Positive integer N; required only for `approvals` |
| `OSTROM_MERGE_APPROVERS` | Optional comma-separated logins for `approvals`; empty means eligible repository reviewers |

`approvals` adds N distinct approving people on the current head to every unattended condition.
An allowlist with N=1 expresses “only after approval by one of these people”; with N>1, all N must
come from it. Match logins case-insensitively; record stable user IDs when read. Count submitted,
non-dismissed APPROVED reviews on this head from people with repository write/maintain/admin
permission, excluding bots and the PR author. A later COMMENTED review does not cancel an approval;
a later decisive request for changes blocks. A new head needs fresh approvals even if the repository
does not dismiss stale reviews. GitHub's own stricter approval requirements always apply. [2][7][8]

Reject unknown strategies, inconsistent fields, invalid counts and duplicate/empty allowlist entries
at startup, with non-zero exit. Human policy ignores CI readiness for performing merges because
Ostrom never sends one; the card still reports observed status and external merges still complete
work. `OSTROM_ALLOWLIST` continues to control intake; it is not the merge approver list.

Leave out policy expressions, per-file risk scores, model approval, signed policies, a gate service,
new verdict records, method selection, labels as overrides and per-ticket exceptions. Repository
configuration and GitHub's existing protections express the requested alternatives. Multiple
repositories and label-based selection can wait for a demonstrated need.

## Races, refusal and stopping

Run the new pass after commands, follow-ups, Watchdog and their state transitions, before the Router
outcomes and send. Any failed prerequisite pass suppresses automatic merging for that tick; the
current tick's “continue after error” behavior must not leak into a merge. Perform a final direct
refresh of PR/checks/reviews/threads/protections and issue/PR activity, ingest and handle new controls,
then recheck local state and acquire the assignment reservation. Serialize local pause/stop against
the transition that sends HTTP; do not hold a SQLite transaction across the network call.

- **Push or force-push:** the endpoint's `sha` precondition rejects a different head with 409. Clear
  readiness, read again and test the new head. Never substitute a new SHA into an old attempt. A
  changed base similarly invalidates readiness; the REST endpoint has no base-SHA precondition. [1]
- **Human merge/close at the same time:** re-read after any ambiguous result. Already merged means
  reconcile once, never post a second done reply. Closed without merge means `pr_closed`; never
  reopen it. The actual GitHub merge result wins over a local pending request.
- **Re-runs:** the rollup combines check runs and statuses; its contexts expose current run status
  and conclusion. Any check not COMPLETED and any status other than SUCCESS blocks. GitHub documents
  NEUTRAL/SKIPPED as successful for required checks, but the GraphQL reference does not specify
  their rollup aggregation or promise latest-attempt/atomic re-request visibility. [3][6]
  A suite re-request explicitly resets the suite to QUEUED with no conclusion; the docs do not
  promise its child runs reset simultaneously. [5] Supplement the rollup in the same query with
  all head `checkSuites` pages and their status/conclusion, plus `workflowRun` event, workflow
  identity, `runAttempt` and matching PRs. These fields establish PR-workflow provenance and detect
  a queued suite before replacement contexts appear. [3][4] Use the latest suite/attempt per
  workflow, never an older successful one; paginate and block ambiguous ordering. No separate
  REST check-run, status, suite or Actions reader is planned. Fixtures must establish latest-
  attempt selection and the suite/rollup transition before enabling merging. This cannot rule out
  a re-run requested after the final read; that remains the send-time race described below.
- **Protection:** read classic protection and active branch rules, plus repository merge settings;
  do not infer requirements from `CLEAN` or assume `check` is the required check everywhere. Treat
  unknown enforced rules as unsupported. GitHub remains the final enforcer, but a privileged login
  can bypass some rules, so API acceptance alone is not proof of compliance. [8][9]
- **API failure:** require HTTP success, `merged=true`, a returned SHA and a confirming PR read.
  `merged=false` is a refusal even with HTTP 200. Log it and fail the explicit merge pass/command
  non-zero. 403/405/422 surface the concrete block; no blind retry each tick. Transport errors,
  429 or server failures get bounded backoff, with a fresh eligibility read before another request.
  An expected wait for CI/quiet time is reported as waiting, not as a successful merge. [1]
- **Restart after send:** reconcile pending/unknown attempts from GitHub before allowing a retry
  or Worker turn. If merged, finish once. If still open, revalidate everything before retrying;
  the endpoint changes PR state once, but it has no client idempotency key. Persist backoff and
  recover stable outbox IDs. A failed reply/card send never repeats the merge.
- **Pause/stop:** pause suppresses merging as soon as accepted locally. Add a durable stop-request
  flag, set before cancelling/queueing cleanup, also for issue-close. Resume does not clear a stop.
  A command arriving through GitHub is effective only after intake; a comment is not an instant
  interlock. Restart must preserve these blocks and any queued cleanup.

To stop an imminent merge, run `ostrom pause <ticket>` and wait for the local API acknowledgement;
GitHub `@ostrom pause` also works after intake. The acknowledgement means no request will start
later while paused. If the HTTP request was already sent, return an explicit “merge may already be
in flight” result with non-zero exit, reconcile and show the result; do not promise cancellation.
A maintainer can also convert the PR to draft or close it on GitHub. Stop ends the ticket after
cleanup and turns an unmerged PR draft as today. Once GitHub has merged it, stopping cannot undo it.

GitHub offers no transaction covering human comments/reviews, check re-runs, base movement and this
REST merge call. Fresh reads and the quiet period narrow that race; the SHA guard covers only head
movement. Enforced branch rules protect server-side requirements. Document this residual race in
`running.md`; human policy is available where the operator needs deliberate merge timing.

## Verification

CI runs tests and typechecks; no local test, build or paid model probe is needed for this design.
Use the repository's `ci` workflow, job `check` (`.github/workflows/ci.yml`), on each implementation PR.

Agreement tests must pin the REST `X-GitHub-Api-Version` header (choose `2022-11-28` for the first
implementation) and check recorded sandbox responses against the fields the production reader
uses. GraphQL has no date-version header: commit the exact query and narrow fixture shapes, tolerate
extra fields and reject missing required fields/unknown enum values. Do not test a fixture against
a second handwritten copy of itself. Cover:

- PR head/base repository, ref and SHA, state/draft, null mergeability, merged time/actor and merge
  SHA; repository squash setting; successful/false merge bodies and 403/405/409/422 responses. [1]
- GraphQL merge/review states, all pages of review threads and `isResolved`, review author/state/
  commit; REST review order, submitted/dismissed reviews and null/deleted users. [2][7]
- The shared GraphQL query's commit OID, rollup state and every contexts page: CheckRun identity/
  suite/app/name/status/conclusion and StatusContext name/state/creator. Cover empty/null rollups,
  SUCCESS alongside NEUTRAL/SKIPPED, pending/error/failure states, missing required checks,
  same-name different apps, and head versus test-merge SHA. Supplement only with paginated head
  suites and their workflow event/identity/attempt/matching PRs: prove a queued re-request blocks
  while old contexts remain green, and an older success cannot mask the latest attempt. [3][4][5][6]
- Classic protection including strict checks, app IDs, approval and bypass settings; active inherited
  branch rules including required checks, reviews, queue and merge methods; permission denial and
  genuinely unprotected branches. The selected API version's available fields must be verified,
  rather than assuming the current docs' newer examples apply. [8][9]

Deterministic tests exercise each blocker separately, review reduction and approval identities,
quiet time after actual reply delivery, queued redirects/recovery across restart, suppression after
an earlier failed pass, and pause/stop while HTTP is pending. Crash injection covers before send,
a lost response, before reconciliation commit and before outbox delivery. Assert one merge record,
one terminal outcome and one done reply, not just HTTP calls.

### Sandbox run

Use `onsager-ai/ostrom-sandbox`, a throwaway store and cheap Workers as in `e2e-m1.md`. Record UTC
times, daemon revision/config, ticket/assignment/native session IDs, store rows, tested head/base,
workflow run/attempt URLs, review URLs and merge SHA. Add an evidence document only after the run.

| Exercise | Pass evidence |
|---|---|
| Default, no human approval | Issue → Worker PR → real PR CI → posted ready notice → quiet deadline → daemon squash merge; no relay or human merge; Done and one reply |
| Review follow-up | Human requests changes during the window; no merge; same session revises; human resolves threads/approves as needed; new head CI and window precede merge |
| Paused, stopped, stuck or over budget | Green PR stays unmerged; pause survives restart; stop cleanup is draft; extension/resume only restores eligibility when all blockers clear |
| Human policy | Green, quiet PR remains waiting; a human merge records external origin and the same `pr_merged` completion |
| Approval policy | N distinct eligible people on current head required; an outsider, duplicate or stale approval does not count; allowlisted N=1 works |
| Push/re-run/protection race | Force-push after green rejects old evidence; a queued re-run and missing/wrong-app required check block; conflict/draft/required review also block |
| Restart/failure/racing human | Crash after accepted merge with lost response reconciles once; a concurrent human merge completes once with honest attribution; denied API returns non-zero |
| Stop at send boundary | Pause before reservation prevents HTTP; pause after send reports possible in-flight merge and reconciles without a false promise |

## Implementation PRs

1. **GitHub reader and policy data.** Add one GraphQL query for head rollup, merge/review state,
   threads and the narrow suite/workflow supplement above, with pagination and agreement fixtures.
   Keep REST reads for PR/merge settings and branch requirements; do not add separate CI readers.
   Add policy parsing and a read-only eligibility report. No merge calls. Forward migration adds
   `merge_candidate` with ownership, policy/activity and ready-delivery fields. Show the report in
   preview; do not introduce a general check catalogue.
2. **Durable work and merge inhibition.** Forward migration adds ticket stop-request state and a
   small queued-turn table keyed by assignment/delivery key, with kind/source pointer and queued/
   running/finished state. Persist redirects, controls and recovery before scheduling; recover
   unfinished work and expose a runner reservation. Add waiting card snapshots and ready reply
   delivery tracking. Cover current commands as well as review follow-ups.
3. **Merge and reconciliation.** Forward migrations add `merge_attempt` and `pr_merge` with unique
   PR result and in-flight constraints. Wire the deterministic pass, preflight suppression, SHA-
   guarded squash, backoff, restart reconciliation, operator race responses and stable completion
   outbox IDs. Enable unattended by default together with human/approval policies and update
   `running.md`. The Worker contract changes ship here before merging is enabled.
4. **Sandbox evidence.** Run the matrix, record actual evidence and remaining failures, then update
   the milestone status. No schema change. Fix a discovered failure in its own reviewable PR.

Append migrations to `src/store/migrations.ts`; never edit shipped entries. Preserve existing
subscriptions, assignments and outbox IDs. The first merge-enabled restart reconstructs candidates
for existing PRs with a fresh posted notice/window, so an old green PR cannot merge immediately.

## Open questions

- The operator's shared GitHub identity cannot prove origin after a lost HTTP response. This design
  records unknown; a dedicated GitHub App remains a separate identity decision.
- Confirm with the committed GraphQL query how rollup contexts and suite/workflow attempts change
  during re-request, including NEUTRAL/SKIPPED and test-merge checks. The docs define the fields,
  not atomic ordering or latest-attempt selection. Block ambiguous evidence until fixtures settle
  it; no separate REST/Actions CI reader is justified by the current documented field surface.
- Confirm that the deployed credential can read both classic protection and inherited rules.
  A private repository's 404 can be ambiguous; setup must distinguish inaccessible protection
  from an unprotected branch. Missing permission blocks automatic merge rather than guessing.

## Sources

GitHub documentation checked 2026-11-10. These are external contracts; the implementation still
needs fixtures at its pinned version.

1. [Pull requests: get and merge](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request).
2. [GraphQL pull requests: merge/review enums, reviewThreads and reviewDecision](https://docs.github.com/en/graphql/reference/pulls).
3. [GraphQL commit rollup, contexts and status states](https://docs.github.com/en/graphql/reference/commits),
   and [check runs, suites and check states](https://docs.github.com/en/graphql/reference/checks).
4. [GraphQL workflow identity, event and attempt](https://docs.github.com/en/graphql/reference/actions).
5. [Check suite re-request reset](https://docs.github.com/en/rest/checks/suites#rerequest-a-check-suite).
6. [Troubleshooting required status checks](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/collaborating-on-repositories-with-code-quality-features/troubleshooting-required-status-checks).
7. [Pull request reviews](https://docs.github.com/en/rest/pulls/reviews#list-reviews-for-a-pull-request).
8. [Branch protection](https://docs.github.com/en/rest/branches/branch-protection#get-branch-protection).
9. [Active rules for a branch](https://docs.github.com/en/rest/repos/rules#get-rules-for-a-branch).
10. [Protected branch bypass defaults and disabling bypass](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches).
