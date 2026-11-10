# Milestone 2 sandbox run

Status: partially evidenced, 2026-11-10. The natural unattended merge is deferred after three
thermal shutdowns; no successful milestone-2 end-to-end unattended merge is claimed. All times below are UTC. This document records observed results
against the public `onsager-ai/ostrom-sandbox`; it does not substitute unit tests for runtime evidence.

## Setup

- Initial daemon revision: `8072bbb4eae1e26af6513f50e139a6040ed4503b` on the primary `main` checkout.
- Fresh store: `/home/marvin/.local/state/ostrom-e2e/ostrom-m2.sqlite`; the milestone-1 store is retained.
- Worker and Router: Codex `gpt-6-luna`, operator login, daemon interval 30 seconds.
- Batch window 2 minutes, comment gap 5 minutes, fixed merge quiet window 10 minutes.
- One GitHub login authors issues, comments, Worker PRs and merge requests. GitHub prevents formal
  approval or changes-requested reviews by a PR author; those review paths cannot be demonstrated here.
- Thermal guard applies to the daemon. Local tests/typechecks/builds are prohibited; sandbox CI runs them.

## Results

| Exercise | Runtime result | Limits |
|---|---|---|
| Default unattended merge | Deferred | PR #20 has real passing CI, but interrupted recovery is still running and no ready outcome was queued; no PUT or merge record exists |
| Review follow-up | Partial | Same session revised PR #15 and new head CI passed; final follow-up window/merge deferred; formal author review impossible with one login |
| Pause / restart / stop | Partial | PR #17 remained paused across restart; PR #16 became draft and ticket ended; resume-to-eligibility deferred |
| Human policy | Pass for external merge reconciliation after #704 | PR #17 human-merged after deadline; external origin/actor/SHA, terminal ticket, Done card and stable reply; paused-card display limitation noted |
| Approval runtime gate | Skipped | Actual local running/pending/paused work blocks before approvals; isolated live diagnostic below rejects missing approvals |
| Force-push | Partial | Changed head captured, quiet deadline reset, no merge attempt; performed under human policy, so no live send race claimed |
| Protection runtime gates | Skipped | Same local work blockers; live reader/policy diagnostics below cover actual protection and missing check shapes |
| Lost-response reconciliation | Deferred | Unsafe to stage on the real API; unit tests cover it; natural thermal restart had no in-flight merge attempt |

## Initial attempt and discovered failures

At 03:57:36, issue [#11](https://github.com/onsager-ai/ostrom-sandbox/issues/11) requested a small
README example. Router session `01a1c39b-2a12-7061-ac07-fe95cf01b163` accepted at 03:58:16.751;
ticket 1 / assignment 1 started Worker session `01a1c39b-4b5a-79d3-80fe-4eff5922743b` at
03:58:17.146 with model `gpt-6-luna`, branch `ostrom/ticket-1-1`. The Working card was posted at
03:58:20 ([comment](https://github.com/onsager-ai/ostrom-sandbox/issues/11#issuecomment-6758569325)).

The next observation subscribed historical merged PR #2, which used the same branch name in the
milestone-1 store. The new Worker had not opened a PR. This run therefore cannot count that PR as
new work. [Fix PR #702](https://github.com/onsager-ai/ostrom/pull/702) gives newly created branches
UUID identities while retaining store IDs in their names. The original store is retained as failure
evidence; no false terminal result was recorded before shutdown.

During that tick the thermal guard reported `stopped at 91C; use CI for this` and exited 75.
The daemon PID `2895053` was no longer present; subsequent temperature was 54C. This was a real
thermal interruption, not a simulated lost merge response. No merge request or candidate existed.
Issues #12 (follow-up), #13 (pause) and #14 (stop) were opened before the stop and remained pending.

## Restart after branch-identity fix

Manager merged #702 at `e76a745bcf01dfce5a5927ad02279332f975daf7` and updated the primary checkout.
A second fresh store, `/home/marvin/.local/state/ostrom-e2e/ostrom-m2-run2.sqlite`, retains the
failed original. Its source cursor was initialized to 03:57:00 UTC to ingest issues #11–14.
The daemon restarted under `thermal gate -- taskset -c 0` with the same model/policy settings.
Initial temperature was 58C after intake. A previous marked card on issue #11 was correctly dropped.
Router session `01a1c3a0-bb4d-7cc1-aff5-d65e1cd1367c` accepted four requests in one batch at 04:04:26.

| Issue | Ticket / assignment | Native Worker session | Branch suffix |
|---|---|---|---|
| #11 default | 1 / 1 | `01a1c3a0-ee8d-7ba3-9a52-b8bb6cf3a38a` | `d4831727-ef91-4fe8-bd0f-19b072348c57` |
| #12 follow-up | 2 / 2 | `01a1c3a0-f16a-75d1-9997-c53606117387` | `10087cbf-b22f-40f7-8d78-37499fd371f7` |
| #13 pause | 3 / 3 | `01a1c3a0-f706-7f51-b300-57c208f1cc0b` | `ba84659a-2c32-4a44-88a7-28eef1d084a2` |
| #14 stop | 4 / 4 | `01a1c3a0-fcbb-7880-9ca1-4310858b836a` | `01a9a811-27d8-4e5a-b6f9-16beadf92847` |

Every complete branch is `ostrom/ticket-<ticket>-<assignment>-<suffix>`. All four first turns began
between 04:04:26.801 and 04:04:31.400 with outcome null. Temperature remained 54C at 04:04:46.

Sandbox PRs #15 (empty fields), #16 (whitespace) and #17 (headerless input) passed workflow `ci`,
job `test`, on their first heads. At 04:06:07 the operator API paused ticket 3; the durable state
was `paused` and its card showed Paused. At 04:06:16 a plain review-follow-up comment was posted
on [PR #15](https://github.com/onsager-ai/ostrom-sandbox/pull/15#issuecomment-6758681055).
At 04:06:31.161 the operator API set ticket 4's stop request; its cleanup completed at 04:06:45.376,
ending the ticket and leaving PR #16 open, draft and unmerged at head
`6879d8d20e5fcee792f3817ec116e02dbebb439a`.

The Semon prerequisite failed with `native query source generation unavailable`; the tick reported
Watchdog failure and merge candidates recorded `an earlier prerequisite pass failed`. No PUT was
sent. The one-core run then hit a second thermal stop at 90C (exit 75). Ticket 3 remained paused;
its pause-control turn was open, ticket 2 had completed its first turn, and ticket 1's initial turn
remained open. This demonstrates durable pause state across a real interruption, but not yet
successful merge reconciliation.

After cooling to 55C at 04:08:20, the daemon restarted under a transient user scope with
`CPUQuota=25%`, nested under the thermal guard and one-core affinity. The Manager authorized this
scope and required that a third thermal trip end retries pending a new decision. No persistent
host settings or guard thresholds changed.

At approximately 04:10:59, that scope also hit 90C and exited 75. Process inspection confirmed no
daemon or Codex child remained. The Manager required no further daemon/Worker restarts on this host.
Subsequent work used only one-shot passes or explicitly read-only diagnostics, under the thermal
guard, with temperature below about 60C before starting. No local test/typecheck/build ran.

## Real PR and CI evidence

All PRs target base `c07944528911f306a9d81d46a9eec784d706d764` before the later human merge.
Every workflow below is event `pull_request`, workflow `ci`, job `test`, attempt 1, conclusion SUCCESS.

| PR | Head | Workflow run |
|---|---|---|
| [#20 default](https://github.com/onsager-ai/ostrom-sandbox/pull/20) | `5c141726a36e74ce37c01af35b63a03da298e546` | [42754108451](https://github.com/onsager-ai/ostrom-sandbox/actions/runs/42754108451), 04:10:10–04:10:19 |
| [#15 initial](https://github.com/onsager-ai/ostrom-sandbox/pull/15) | `087c23af83b699ce313cab9a012b46db2411d4c6` | [42753577901](https://github.com/onsager-ai/ostrom-sandbox/actions/runs/42753577901) |
| #15 same-session revision | `f0addafec0c2505436170e0868beb575ac2e0126` | [42754052283](https://github.com/onsager-ai/ostrom-sandbox/actions/runs/42754052283), 04:09:41–04:09:48 |
| [#17 pause/human](https://github.com/onsager-ai/ostrom-sandbox/pull/17) | `0b8224b6ac3ebb44e4e9d1a1e845a59ae4c54949` | [42753586712](https://github.com/onsager-ai/ostrom-sandbox/actions/runs/42753586712), 04:05:25–04:06:13 |
| [#16 stopped](https://github.com/onsager-ai/ostrom-sandbox/pull/16) | `6879d8d20e5fcee792f3817ec116e02dbebb439a` | [42753579131](https://github.com/onsager-ai/ostrom-sandbox/actions/runs/42753579131), 04:05:21–04:05:38 |

PR #15's first follow-up was trigger 5, delivered by queued turn `redirect-5-2` in native session
`01a1c3a0-f16a-75d1-9997-c53606117387`, started 04:08:55.254 and finished 04:10:12.440.
Its head changed from `087c23a` to `f0addaf`. The [ready reply](https://github.com/onsager-ai/ostrom-sandbox/issues/12#issuecomment-6758735001)
was actually posted at 04:10:06.890 with ID `outcome-a2-pr15-opened`. A second plain comment
[at 04:10:58](https://github.com/onsager-ai/ostrom-sandbox/pull/15#issuecomment-6758747261) arrived
inside the ten-minute period. It became trigger 8 after shutdown; no second revision is claimed.

PR #17's ready reply `outcome-a3-pr17-opened` was [posted](https://github.com/onsager-ai/ostrom-sandbox/issues/13#issuecomment-6758736411)
at 04:10:13.036. Ticket 3 remained paused on the API after restart with its original native session.
PR #16's [Stopped card](https://github.com/onsager-ai/ostrom-sandbox/issues/14#issuecomment-6758659711)
and [stop summary](https://github.com/onsager-ai/ostrom-sandbox/issues/14#issuecomment-6758732442)
were delivered; no merge occurred.

## One-shot passes after thermal shutdown

The runtime remained `main` at `e76a745bcf01dfce5a5927ad02279332f975daf7` until fix #703 merged;
after that it was `ae33f6d1c99856313e58d9bc68c70932f7f9373d`. The secret env file was sourced only
inside the command shell and was never printed or copied. The same run2 store was used.
The command form, with the nonsecret settings above exported, was:

```sh
thermal gate -- systemd-run --user --scope --quiet -p CPUQuota=25% \
  taskset -c 0 node /home/marvin/projects/onsager-ai/ostrom/src/ostromd/main.ts dev <pass>
```

| UTC | Pass and policy | Result |
|---|---|---|
| 04:13:07 | `dev poll`, unattended | exit 0; ingested issues #18/#19 and second PR #15 comment |
| 04:13:23 | `dev observe`, unattended | exit 0; subscribed new PR #20 without changing the running recovery |
| 04:13:30 | `dev send`, unattended | exit 0; four deliveries |
| 04:14:03 / 04:14:17 | `dev poll` / `dev send`, human | exit 0 |
| 04:14:19 | `dev merge`, human | exit 0; all three live candidates waiting for a human; no attempts |
| 04:15:42 / 04:15:55 | `dev poll` / `dev send`, human | exit 0 |
| 04:16:04 | `dev merge`, human | exit 0; refreshed forced head and deadline; no attempts |
| 04:21:49 | `dev send`, human | exit 0; delivered human-policy card |

These were production passes, but not an unattended daemon tick. `dev commands`, `route`,
`route-candidates`, `tick` and `run` were not used after the final thermal shutdown because they
could start Codex. The merge pass's send-time command refresh can also dispatch a pending redirect;
only human-policy merge observations/reconciliation were used, which return before that send path.

Ticket 1 still has `turn_started_at=04:08:31.838`, queued `recovery-1` running and no ready reply for
PR #20. The Worker committed/opened PR #20 before shutdown, but no natural final completion reached
the store. Completing unattended evidence requires a real Worker completion and Router outcome on
a cooler host. Neither these records nor local prerequisites were reconstructed in the store.

## Force-push after green

At 04:15:34, the operator amended the documentation-only PR #15 commit without changing its tree
and force-pushed with an explicit lease on old head `f0addafec0c2505436170e0868beb575ac2e0126`.
The new head was `8f781b38aeb27fbf231ceff6c6f2910ac5f0fbf1`; its [PR CI run 42754714437](https://github.com/onsager-ai/ostrom-sandbox/actions/runs/42754714437)
passed. At 04:16:41.335 the production pass captured that new head and reset `quiet_deadline` to
04:26:41.335. `merge_attempt` remained empty. This proves head/window refresh under human policy;
it does not claim the endpoint rejected a stale SHA during an actual send.

## Read-only diagnostics

These are isolated diagnostics, not runtime pass evidence. The script called the real
`readMergeEvidence(execGh, "onsager-ai/ostrom-sandbox", 15)` and then `mergeEligibility` against its
live response. It made no PUT and no store mutation. The supplied local prerequisites were exactly:

```text
issueOpen: true
ticketOpen: true
pausedOrStopping: false
stalledOrFailed: false
budgetReached: false
workerIdle: true
readyReplySent: true
quietUntil: new Date("2026-11-10T00:00:00.000Z")
now: new Date() // diagnostic observation time
```

For approval policy it supplied `strategy: "approvals", approvals: 1, approvers: []`, the live PR
author login, and an empty reviewer-identity map (there are no submitted approvals).
Actual runtime blockers bypassed by these inputs: ticket 1's running interrupted recovery and
missing ready reply; ticket 2's unhandled redirect trigger; ticket 3's paused state and queued
recovery. No runtime protection or approval gate result is claimed from these diagnostics.

At 04:17:35 sandbox classic protection was enabled with the following exact submitted settings:

```json
{"required_status_checks":{"strict":false,"checks":[{"context":"test","app_id":15368}]},"enforce_admins":true,"required_pull_request_reviews":null,"restrictions":null}
```

The live read returned `required_status_checks.strict=false`, `contexts=["test"]`,
`checks=[{"context":"test","app_id":15368}]`, `enforce_admins.enabled=true`; active rules were `[]`.
The real head context was CheckRun `test`, database ID `129626290462`, COMPLETED/SUCCESS, suite
`115792785369`, app `15368` (`github-actions`), workflow event `pull_request`, attempt 1, matching
PR #15/repository. The 04:17 diagnostic returned eligible for unattended and
`policy approvals are not satisfied` for approvals.

At 04:18 a second protection PUT required both `test` and `m2-missing-check`, each app `15368`,
keeping the other settings unchanged. The response retained both check identities and `rules=[]`.
GitHub reported `mergeStateStatus=BLOCKED`; both policies returned
`pull request is not clean at the captured head and base`. This is the actual first blocking gate,
not a claimed observation of the later missing-check message.

Protection DELETE succeeded at 04:18:30–04:18:42 and the branch read returned `protected=false`.
The unprotected diagnostic then failed, exposing #703: the real error wrapper was
`gh api -H failed: gh: Branch not protected (HTTP 404)`, rather than #701's assumed prefix.
After Manager merged [#703](https://github.com/onsager-ai/ostrom/pull/703) and updated main, the
04:23:12 live diagnostic returned `protection={}`, `rules=[]`, unattended eligible and approvals
blocked with `policy approvals are not satisfied`. The branch remained unprotected.

## Human-policy merge and reconciliation

PR #17 was chosen from the paused-ticket exercise; no other exercise needs that PR. At 04:25:20,
`dev merge` with human policy still reported `waiting for a human merge` for tickets 1–3. PR #17
was OPEN/CLEAN past its stored deadline 04:25:07.521 and `merge_attempt` was empty. The PR #15
[card](https://github.com/onsager-ai/ostrom-sandbox/issues/12#issuecomment-6758658226) displayed
Waiting for a human merge with policy human; PR #17's card still displayed Paused because ticket
state takes precedence over human policy. The Manager accepted this display limitation as a
follow-up, not a reason to change the runtime during this milestone.

The operator ran exactly:

```sh
gh pr merge 17 --repo onsager-ai/ostrom-sandbox --squash \
  --match-head-commit 0b8224b6ac3ebb44e4e9d1a1e845a59ae4c54949
```

It exited 0. GitHub records merge time 04:26:25, actor `tikazyq` / ID `3393101`, merge SHA
`b419f01fcebdb1544afdf64ce77fc181c9924e19`. This is the only manual sandbox merge in this run.

The before-merge polling/sending pair at 04:25:02/04:25:12 exited 0. After the human merge,
`dev poll` at 04:26:28 and `dev send` at 04:26:36 exited 0. `dev merge` at 04:26:38 returned failure:
`CHECK constraint failed: (state = 'queued') = (started_at IS NULL)`. A never-started queued
`recovery-1` for assignment 3 was being marked finished with no start timestamp. The transaction
rolled back: no `pr_merge` row or done reply existed and ticket 3 remained paused. The shell's
subsequent successful commands did not change the merge pass's nonzero result.

[Fix #704](https://github.com/onsager-ai/ostrom/pull/704) adds a forward migration with cancelled
never-started turns and audits terminal cleanup. No start timestamp was fabricated and the live
store was not manually patched. Manager merged #704 as
`0f3e8a100e7c5d56c59131bc8e8d190b556222ed` and updated the primary checkout.

At 04:36:10 `dev poll` opened/migrated the store; a read-only query confirmed schema version 28.
`dev send` at 04:36:18 exited 0; `dev merge` at 04:36:20 exited 0 and reconciled PR #17 without
starting a native session. The transaction ended ticket/assignment 3 at 04:37:05.252 and changed
its queued `recovery-1` to `cancelled`, `started_at=null`, `turn_id=null`,
`finished_at=04:37:05.251`. The merge row is:

```text
repository=onsager-ai/ostrom-sandbox; pr_number=17
head_sha=0b8224b6ac3ebb44e4e9d1a1e845a59ae4c54949
merge_sha=b419f01fcebdb1544afdf64ce77fc181c9924e19
merged_at=2026-11-10T04:26:25.000Z
merged_by_login=tikazyq; merged_by_id=3393101
origin=external; attempt_id=null
```

`merge_attempt` remained empty, as expected for an external merge. The stable reply ID is
`outcome-a3-pr17-merged`. `dev send` at 04:37:22 delivered the [Done card](https://github.com/onsager-ai/ostrom-sandbox/issues/13#issuecomment-6758658943)
and deferred the outcome until 04:39:05.252 for ordinary batching. The card heading is Done but its
merge detail retains the prior waiting reason; this stale terminal detail is another display
follow-up, not a false merge-origin claim in the completion reply.

At 04:38:31/04:38:41 another poll/send pair exited 0; `dev merge` at 04:38:43 exited 0 with zero
new merges. A read-only store count showed exactly one `pr_merge`, one stable merged-outcome row
and no reservations. `dev send` at 04:39:35 delivered the [single Done reply](https://github.com/onsager-ai/ostrom-sandbox/issues/13#issuecomment-6759149152)
at 04:39:40. It names `@tikazyq`, links PR #17 and its merge commit, and does not claim Ostrom
performed the merge. GitHub's issue-comment read returned exactly one comment carrying
`outcome-a3-pr17-merged`; outbox `sent_at=04:39:41.345`, attempts 1.

## Fixes found by this run

| Fix PR | Observed failure | Verification before restart/retry |
|---|---|---|
| [#702](https://github.com/onsager-ai/ostrom/pull/702) | Fresh store reused a historical Worker branch and subscribed old merged PR #2 | CI [42753170286](https://github.com/onsager-ai/ostrom/actions/runs/42753170286), then merged main `e76a745` |
| [#703](https://github.com/onsager-ai/ostrom/pull/703) | Real `execGh` error prefix includes `-H`, so #701's absence normalization still blocked | Fake-executable agreement and CI [42755326261](https://github.com/onsager-ai/ostrom/actions/runs/42755326261), then merged main `ae33f6d` and successful live read |
| [#704](https://github.com/onsager-ai/ostrom/pull/704) | External merge with never-started recovery violated queued-turn CHECK and rolled back completion | Migration/terminal cleanup tests, CI [42756644515](https://github.com/onsager-ai/ostrom/actions/runs/42756644515), then merged main `0f3e8a1` and successful live reconciliation |

## Deferred work and preserved state

The natural unattended merge, Done reply and `origin=ostrom` row remain unproven. Ticket 1 must
finish its real recovery and receive the Router's ready outcome; the host could not sustain those
sessions today. Ticket 2's second comment awaits a real Worker turn, followed by new CI/window and
merge. Ticket 3's resume exercise is consumed by the authorized human merge; no resume-to-merge
claim is made. Formal CHANGES_REQUESTED and satisfied approval policy need another GitHub identity.
Runtime protection/approval gates are skipped because local work blocks first. No lost-response or
mid-request kill was staged; deterministic unit tests cover those paths, not this run.

Pending issues [#18](https://github.com/onsager-ai/ostrom-sandbox/issues/18) and
[#19](https://github.com/onsager-ai/ostrom-sandbox/issues/19) are left open and undispatched.
PRs #15/#20 and their native sessions remain recorded for a later cooler-host run; PR #16 remains
draft/unmerged. The old milestone-1 store, failed M2 store and run2 store remain intact. On restart,
recorded native sessions and pending work can be resumed; this document does not authorize a
runtime restart on the hot host.

No daemon/Codex process remains and sandbox classic protection is off. Follow-ups: finish the
missing natural matrix evidence; improve the Paused/human-policy card display; investigate the
intermittent Semon source-generation read failure. None is presented as a pass here.






