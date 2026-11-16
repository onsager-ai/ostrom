# Milestone 2 sandbox run on hp-code-2

Status: passed, 2026-11-16. This run completes the milestone-2 evidence that [the first
run](e2e-m2.md) could not, because hp-code-1 overheated. It also proves the GitHub App identity
end to end. All times are UTC. The sandbox is the public `onsager-ai/ostrom-sandbox`, with no
branch protection.

## Setup

- Host hp-code-2 (30–37 °C under load). The daemon ran as `thermal gate --max-seconds 5400 -- node
  src/ostromd/main.ts run --interval 30` from the primary checkout.
- Identity: GitHub App `ostrom-agent` (App ID 4544316, bot `ostrom-agent[bot]`, user ID 315274524),
  configured with `OSTROM_GITHUB_APP_ID` and `OSTROM_GITHUB_APP_KEY_FILE`. The key was generated for
  this host and was never printed.
- Router and Worker: Codex `gpt-6-luna`. Batch window 2 minutes, comment gap 5 minutes, stall 15
  minutes, fixed merge quiet window 10 minutes.
- Store: `~/.local/state/ostrom-e2e/ostrom-m2-hp2b.sqlite`, fresh at 07:16. The earlier
  `ostrom-m2-hp2.sqlite` from the first attempt below is kept.

## Results

| Exercise | Result | Evidence |
|---|---|---|
| App-authored PR | Pass | [PR #28](https://github.com/onsager-ai/ostrom-sandbox/pull/28) and [PR #30](https://github.com/onsager-ai/ostrom-sandbox/pull/30): author `app/ostrom-agent`, every commit authored by `ostrom-agent[bot]` |
| Approvals policy | Pass | PR #28 held with `policy approvals are not satisfied` until `tikazyq` approved at 10:45:59; merged by the App at 11:01:42 |
| Default unattended merge | Pass | PR #30 merged by the App at 11:24:47 with no approval |
| Review follow-up | Pass | A plain PR comment at 11:08:41 became a `follow_up` turn in the same native session, 11:09:32–11:10:28; the revision pushed at 11:10:07 |
| Ticket completion | Pass | Done replies on [#27](https://github.com/onsager-ai/ostrom-sandbox/issues/27) at 11:04:27 and [#29](https://github.com/onsager-ai/ostrom-sandbox/issues/29) at 11:27:31; both tickets ended |
| Watchdog | Pass | Every tick of the final runs read `semon query stalls` without failure |

### Approvals policy: PR #28

Settings: `OSTROM_MERGE_STRATEGY=approvals`, `OSTROM_MERGE_APPROVALS=1`,
`OSTROM_MERGE_APPROVERS=tikazyq`. Revision `6ec6280`.

- Issue #27 opened at 10:43:15 and was accepted as ticket 3, assignment 3. Its native session was
  `01a1e3f5-0355-7e82-9760-b4bdef0b589c` and its branch
  `ostrom/ticket-3-3-a632a7be-61d8-42d0-87b4-add43c4320c5`.
- PR #28 head `2b80c3972e957e5dd3102bce1f594354e08156cd` passed workflow `ci`, job `test`
  ([run 43851673048](https://github.com/onsager-ai/ostrom-sandbox/actions/runs/43851673048)).
- The operator approved with an empty review body at 10:45:59. The approval restarted the quiet
  window; ticks reported `quiet period has not elapsed` until the merge.
- The store's `pr_merge` row reads: `pr_number=28`,
  `merge_sha=e5f4a3bfddda3bad75587a9488301854eb67e781`, `merged_by_login=ostrom-agent[bot]`,
  `merged_by_id=315274524`, `origin=ostrom`, `attempt_id=1`.

### Unattended default with follow-up: PR #30

Settings: no merge-strategy variables. Same store and revision.

- Issue #29 opened at 11:06:18 and was accepted as ticket 4, assignment 4. Its native session was
  `01a1e40a-027f-7971-907f-cad769f52995` and its branch
  `ostrom/ticket-4-4-2b21cb6a-b950-4611-b5fa-07e6c7a9abcf`.
- The first head was `c3c23801` (11:07:51). The operator's comment asked for an example. The
  Worker's second turn pushed `8998a46ded8221bda1cabf0b8992c6e36778da53`, which passed `ci` / `test`
  ([run 43855366416](https://github.com/onsager-ai/ostrom-sandbox/actions/runs/43855366416)).
  The example's claimed result matches `src/csv.js`.
- The store's `pr_merge` row reads: `pr_number=30`,
  `merge_sha=856d5e69e0e03bbe6912504d456163b13e0f05df`, `merged_by_login=ostrom-agent[bot]`,
  `merged_by_id=315274524`, `origin=ostrom`, `attempt_id=2`.

## Failures found on the way, and their fixes

| Failure | Fix |
|---|---|
| The installed Semon was newer than its session index ("unsupported session index: older schema"), so every tick failed at the Watchdog | The Semon lane moved the old index aside, and Semon rebuilt it. Semon now prints recovery steps for this case |
| A Worker opened sandbox PR #24 as the operator's login. `~/.profile` put `~/.local/bin`, which holds the real `gh`, ahead of the App wrapper in Codex's login-shell snapshot. Git was correct, because its credential helper doesn't depend on PATH | [#711](https://github.com/onsager-ai/ostrom/pull/711): Workers get an empty `GH_CONFIG_DIR`, so a bypassed `gh` fails instead of acting as the operator. [#712](https://github.com/onsager-ai/ostrom/pull/712): in App mode, Codex Workers disable login shells and shell snapshots |
| A fresh store's ticket 1 collided with the old store's `ticket-1-1` worktree folder | [#713](https://github.com/onsager-ai/ostrom/pull/713): worktree folders carry the same UUID suffix as their branch |
| Approving PR #26 with a comment queued a Worker turn. Resuming it failed: the session had started on the config-default provider `magpie`, but resumed with `OSTROM_CODEX_PROVIDER=openai` | Open as [#715](https://github.com/onsager-ai/ostrom/issues/715). This run dropped the override, so start and resume match |

The first attempt (issues #23 and #25, PRs #24 and #26) is closed in the sandbox, with the reason
on each.

## Follow-ups

- [#715](https://github.com/onsager-ai/ostrom/issues/715): the provider at start versus resume.
- Ended tickets leave their worktrees on disk (`ticket-3-3-…` and `ticket-4-4-…` remain).
- An approval that has a review body is queued to the Worker as a follow-up turn, which costs a turn
  and restarts the quiet window. An approval with an empty body does not.
- Not exercised here, as in the first run: resume-to-merge after a pause, protection gates at
  runtime, and reconciling a lost merge response.
