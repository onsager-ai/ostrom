# Sweep binary cutover

## Cut over

Build and install a fixed `ostrom` binary, then change `mandate-sweep.service`'s
`ExecStart` to the binary directly:

```ini
ExecStart=/absolute/path/to/ostrom sweep
```

Run `systemctl --user daemon-reload`, start the unit once, and confirm its exit
status and journal before re-enabling its timer. Keep the binary path fixed for
the comparison window; do not point the unit at a mutable build directory.

## Roll back

In the same `ExecStart` line, restore the existing wrapper:

```ini
ExecStart=/home/<operator>/projects/dotclaude/bin/mandate-sweep.sh
```

Reload the user daemon and start the unit once. That wrapper remains the
single-edit route back to `plugins/ostrom/scripts/sweep.sh` until the comparison
window closes.

## Comparison window

Before each production change, copy the roster and state into an explicit
scratch `OSTROM_HOME`, choose one RFC3339 instant, and run:

```sh
OSTROM_HOME=/absolute/path/to/scratch ostrom parity sweep --started-at 2026-08-01T00:00:00Z
```

Require zero per-field divergences. Also compare the unit exit status, fault
lines, queue/state change counts, and the `installation-token-minted` trace fact
(repository and read-only permission scope only). The parity command cannot
publish; keep any production publication verification separate from this
window.
