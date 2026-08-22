# Publication operations

Publication is an explicit capability of the native sweep. It is disabled
unless the command names one validated destination:

```sh
ostrom sweep --publish-repository placeholder-org/alpha
```

`MANDATE_PUBLISH_REMOTE` is not an enablement mechanism. This prevents an
inherited operator environment from turning a scratch `OSTROM_HOME` into a
publishing run. Parity and plan operations construct `PublishTarget::Disabled`
directly.

Publication begins only after reconciliation has durably written a successful
local generation. A refused generation, including zero acquired configured
repositories, cannot reach the publisher. A later clone, commit, or push
failure is reported as a sweep fault and does not roll back or supersede the
local queue and state.

The publisher rebuilds every public object from the allowlist, records
excluded fields in the manifest, keeps
90 daily gate partitions, and retains complete gate history in rollups.
Destination reads and writes mint separate repository-scoped credentials. The
private publication checkout remains mode `0700` so its remote configuration
is not exposed by a permissive process umask.

## Published record contracts

`queue.jsonl` is the current actionable queue and `state.json` is the current
sweep snapshot. Both may be rebuilt by a later sweep. Gate verdicts are local
rows in `gate.jsonl`; publication exposes their most recent 90 days as
`gate/<YYYY-MM-DD>.jsonl` and uses the full local verdict history for
`rollup.json`.

`merge.jsonl` is different: it is an append-only, all-history ledger of
immutable merge facts, not sweep snapshot state and not a rollup. Its natural
key is (`pr`, `merged_at`), so observing the same landing in later sweeps or
publishing it again produces one row. Every row has:

- `pr`: `owner/repo#number`;
- optional `order_id` when the observed pull request can be matched to one
  local work order;
- `opened_at` and `merged_at` RFC 3339 timestamps;
- `opened_by_class` and `merged_by_class`, each either `loop` or `principal`;
- optional `head_sha` when GitHub supplies it with the merged-PR observation.

The two attribution fields encode both sides independently. `loop` to `loop`
is unattended delivery, `loop` to `principal` is loop-produced work landed by
a human, and any `principal` opener remains attended regardless of the merger
class. Missing actor data is classified conservatively as `principal`. The
allowlist publishes classes only; actor logins have no field in this contract.
The manifest declares merge retention as `forever`, and `rollup.json` contains
no merge-velocity aggregate; consumers aggregate the fact rows for their own
window, repository, and attribution filters.

The sweep is the observation point because it sees both loop and human merges.
A gatekeeper-time emitter would know more about merges it performs, but it
cannot see the human landings needed to distinguish unattended from attended
delivery. The first upgraded sweep records only facts returned by the existing
recent-merge query; no rows are synthesized for periods when the loop was dark.

### Where the allowlist comes from

`MANDATE_PUBLISH_ALLOWLIST` if set, otherwise
`<plugin root>/config/publish-allowlist.json` — resolved from the **plugin
root**, not from `OSTROM_HOME`. This matters for an installed binary: run
outside a repository checkout with neither `OSTROM_PLUGIN_ROOT` nor
`CLAUDE_PLUGIN_ROOT` set, publication cannot find the allowlist. The operator's
sweep wrapper passes `CLAUDE_PLUGIN_ROOT`, which is why the 2026-08-19 cutover
published; a caller that does not is not merely missing a file it could create
anywhere.

## Verification

Use an explicit scratch `OSTROM_HOME` and a disposable local destination for
pre-production verification. No test or parity run should name a production
repository. For an enabled production run, verify:

- only the destination's `state` branch advanced;
- the branch contains `manifest.json`, `queue.jsonl`, `merge.jsonl`,
  `state.json`, `rollup.json`, and the expected `gate/` partitions;
- `manifest.json` has the expected schema, counts, retention, and dropped-field
  accounting;
- a second run with unchanged public content reports `mandate publish:
  unchanged` and creates no commit;
- the local queue and state remain authoritative if publication reports a
  fault.

## Disable or recover

Remove `--publish-repository` from the service command, reload the unit, and
run one sweep. Omission is the complete publication kill switch and does not
depend on environment cleanup.

Do not delete the private publication checkout during incident response. It is
safe to retain, makes the failed attempt auditable, and can be reused by a
known-good binary after the destination and credentials have been verified.
