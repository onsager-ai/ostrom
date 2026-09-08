# Merge observation and velocity (#343)

The sweep is the sole producer of `pr-merged` trace facts. It sees both machine
and human mergers through GitHub's `mergedBy` actor. Observations arrive at sweep
cadence, after the merge; latency uses GitHub's `createdAt` and `mergedAt` instants,
never the observation timestamp. The gate does not emit this fact.

The additive trace kind keeps the existing four-field envelope:

```json
{"ts":"2026-08-03T12:00:00Z","kind":"pr-merged","fact":{"pr":"placeholder-org/example#1","order_id":null,"opened_at":"2026-08-01T01:00:00Z","merged_at":"2026-08-03T01:00:00Z","attribution":"loop_to_loop"},"narration":{}}
```

`pr` is the repository-qualified PR number. `order_id` comes first from an exact
PR link in `work-completed`, otherwise from a unique local work order matching
the repository and head branch. Ambiguous or absent links leave it null; neither
prevents recording the merge. Closing an issue alone does not identify an order.

Both author and merger use the same predicate: a bot flag (including GraphQL's
`__typename: Bot`) or a login ending in `[bot]`. No configured actor identity is
compiled into the binary. The attribution vocabulary is:

| Value | Author | Merger |
| --- | --- | --- |
| `loop_to_loop` | Machine | Machine |
| `loop_to_principal` | Machine | Human |
| `principal` | Human | Either |

Only `loop_to_loop` is unattended. A missing author, a missing merger for a
machine-authored PR, or invalid merge times refuses the sweep before advancing
its queue/state generation. The merger is irrelevant to human-authored work's
class. Actor identities never enter new trace facts or published aggregates.

Merge facts are keyed by PR, appended before state is advanced, and checked
against the existing trace on subsequent sweeps. A retry after a failed state
write reuses the appended fact and repairs the private ledger. Malformed trace
history or conflicting merge facts refuses observation rather than silently
resetting deduplication. This protects sequential sweep retries; simultaneous
sweeps are not serialized by this change. It introduces no additional producer
and changes no store port.

The private `state.json` gains `velocity`, containing `observed_days` (UTC dates
to successfully acquired repository sets) and `pulls` (PR pointers to opening
time, repository, nullable attribution and nullable merge fact). This ledger is
retained across sweeps and roster changes. It flows through the existing
publication snapshot; it is excluded from published `state.json`. The publisher
decodes that ledger through the shared typed definition and publishes counters
only. The publication allowlist, existing rollup fields, and pass contract are
unchanged. There is no new published trace file.

`rollup.json` adds `velocity_by_day`. Each UTC date maps to:

```json
{
  "observed_repositories": 1,
  "opened": {"loop_to_loop": 2, "loop_to_principal": 1, "principal": 3},
  "opened_pending": 1,
  "merged": {"loop_to_loop": 0, "loop_to_principal": 0, "principal": 0},
  "unattended_latency_seconds": {"count": 0, "total": 0, "min": null, "max": null, "mean": null}
}
```

`opened` counts observed PRs by their eventual merge class on their opening date.
Human-authored PRs are immediately `principal`. Machine-authored PRs without an
observed merger contribute to `opened_pending`, which is a count of unresolved
attribution, not a fourth class or a count of currently open PRs. When a merge is
observed, its opening moves from pending into its class; the sum stays constant.
`merged` counts observed merges by their GitHub merge date. Neither count measures
`work-completed` events.

Latency is opened-to-merged elapsed whole seconds, aggregated on the merge date
only for `loop_to_loop`. `count` and `total` support weighted aggregation across
days; `min`, `max`, and arithmetic `mean` are null when there are no samples.
There are no PR or actor identifiers in these aggregates.

There is no historical backfill or calendar gap filling. The sweep's existing
bounded recent-merge query can return merges from before observation began or
from a dark day. Those still get trace facts with their true timestamps, but a
PR contributes to a day's counters only if its repository was successfully
observed on that UTC date. A day with no observations is absent; an observed day
with no merges has explicit zero merge counts. A failed repository contributes
no fabricated observation even when another repository's acquisition succeeds.
`observed_repositories` reports that day's scope size, not continuous coverage.

This is the population visible through the existing open-PR and recent-merge
queries: it does not reconstruct openings of PRs that closed unmerged between
sweeps, query beyond the existing lookback, or filter merges by base branch.
A consumer must treat these counts as observed delivery, not exhaustive forge
history. Historical import would need its own explicit coverage contract.

The existing `machine_author.login` remains private: publication omits both
`repos.*.merge_gate_merges` and queue `mandate.scope_evidence`. The privacy regression
fixture includes distinctive placeholder logins in these paths and tests every
derived publication file plus the merge trace for their absence.
