# Sweep operations

The production portfolio sweep runs through the installed `ostrom` binary. The
shell implementation was retired after `@ostrom/cli` 0.5.0 reconciled the live
portfolio and advanced the published state on 2026-08-19.

## Service configuration

Pin the service to an installed binary rather than a mutable build directory:

```ini
ExecStart=/absolute/path/to/ostrom sweep --publish-repository placeholder-org/alpha
```

Omit `--publish-repository` when the service must reconcile private state
without publishing. After changing the unit, run `systemctl --user
daemon-reload`, start it once, and inspect both its exit status and journal
before enabling the timer.

The service needs an explicit Ostrom state/config location through the normal
XDG environment or `OSTROM_HOME`, plus the gatekeeper App credentials used for
read-only acquisition. Each organisation in the roster receives its own token
restricted to that organisation's configured repositories. The sweep refuses
to persist a generation when no configured repository was acquired.

## Routine verification

For every production run, verify the journal's project and queue-change count,
review any acquisition or publication fault lines, and confirm that
`queue.jsonl` and `state.json` were updated only after a successful acquisition.
The `installation-token-minted` trace facts may be checked for repository and
read-only permission scope; they never contain credentials.

Use `--mode full` for an operator-requested complete reconciliation. Normal
`--mode auto` operation performs incremental issue acquisition when state is
eligible and schedules a full reconciliation at least every 24 hours.

## Recorded parity evidence

The developer parity command compares the current Rust sweep with bytes
captured from the retired implementation. It is hermetic and always disables
publication:

```sh
OSTROM_HOME=/absolute/path/to/scratch \
  ostrom parity sweep \
  --started-at 2026-08-01T00:00:00Z \
  --fixture crates/ostrom-cli/tests/fixtures/parity-sweep/github.json \
  --recorded-queue crates/ostrom-cli/tests/fixtures/parity-sweep/queue.shell.jsonl
```

The scratch home must contain the matching placeholder `mandates.yaml`. Zero
per-field divergences is the required result.

## Recovery

There is no script rollback path. Stop or disable the timer if the installed
binary is unhealthy, retain the private state for diagnosis, and deploy a
known-good binary. Publication can be disabled independently by removing
`--publish-repository`; inherited environment variables cannot enable it.
