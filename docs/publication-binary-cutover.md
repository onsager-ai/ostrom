# Publication binary cutover

## Enable the native publisher

The native sweep does not publish by default. Enable one destination only by
passing a validated `owner/repository` value on the command line:

```sh
ostrom sweep --publish-repository placeholder-org/alpha
```

Put the same option in the service `ExecStart` only after a manual run has
produced the expected `state` branch. `MANDATE_PUBLISH_REMOTE` is intentionally
not an enablement mechanism for the native sweep: an inherited environment
cannot turn a scratch `OSTROM_HOME` into a publishing run.

Publication runs only after reconciliation has written a successful local
generation. A sweep that refuses its generation, including one that acquired
zero configured repositories, returns before any publication work. A later
clone, commit, or push failure is reported as a sweep fault; it does not roll
back or supersede the local queue and state.

The publisher rebuilds every public object from
`config/publish-allowlist.json`, records excluded fields in the manifest, keeps
90 daily gate partitions, and retains the complete gate history in rollups.
Destination reads and writes mint separate repository-scoped credentials.

## Verify the cutover

Use an explicit scratch `OSTROM_HOME` and a disposable local adapter first.
Do not use a production repository for a comparison run. For the production
run, verify all of the following before leaving the option enabled:

- the push advanced only the `state` branch at the named destination;
- the branch contains `manifest.json`, `queue.jsonl`, `state.json`,
  `rollup.json`, and the expected `gate/` partitions;
- `manifest.json` names the expected schema, counts, retention, and dropped
  fields;
- a second sweep with unchanged public content reports
  `mandate publish: unchanged` and creates no commit.

The parity command remains publication-free. Its shell half replaces the
private mirrored `publish.sh`, while its native half constructs a disabled
publication target and passes no `--publish-repository` option.

## Roll back

Remove `--publish-repository placeholder-org/alpha` from the native command,
reload the service definition, and run one sweep. Omission is the complete
native kill switch and does not depend on environment cleanup.

If the entire native sweep must be rolled back during the comparison window,
restore the legacy sweep wrapper described in
[`sweep-binary-cutover.md`](sweep-binary-cutover.md). The legacy
`scripts/publish.sh` remains shipped and untouched for that route. Do not
delete the native publication cache until the rollback has been validated; it
is safe to retain and makes a later retry auditable.
