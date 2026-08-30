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

### Where the allowlist comes from

`MANDATE_PUBLISH_ALLOWLIST` if set, otherwise the allowlist compiled into the
binary. An operator override is read strictly: a missing or malformed override
fails publication instead of falling back to the shipped allowlist.

## Verification

Use an explicit scratch `OSTROM_HOME` and a disposable local destination for
pre-production verification. No test or parity run should name a production
repository. For an enabled production run, verify:

- only the destination's `state` branch advanced;
- the branch contains `manifest.json`, `queue.jsonl`, `state.json`,
  `rollup.json`, and the expected `gate/` partitions;
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
