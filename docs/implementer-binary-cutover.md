# Implementer operation and diagnosis

`ostrom dispatch <work-order-file>` always starts the Rust implementer as a
transient user unit:

```text
ostrom implement <work-order-file> <unit-name> [implementer-runner]
```

The builder coordinator hands the order to the named runner in the agent
registry. `agent/codex` is the shipped default and there is no fallback.
Dispatch resolves the `ostrom` executable before reserving work or invoking
`systemd-run`. Set
`MANDATE_OSTROM_BIN` to an absolute executable path when the interactive
`PATH` does not represent the transient unit environment. The selected runner
supplies its transient-unit environment; for Codex, this includes the resolved
Node directory required by an npm launcher's `#!/usr/bin/env node` shebang.

For a failed launch, start with the dispatch error and the terminal row in
`$OSTROM_HOME/sprint.jsonl`. `ostrom-unavailable` means
`MANDATE_OSTROM_BIN` was invalid or, when the override was unset, `ostrom`
could not be resolved on `PATH`. `codex-unavailable` means the Codex executable
or its Node interpreter could not be resolved or executed. Neither failure
starts the transient unit.

After a unit starts, inspect it with `systemctl --user status <unit-name>` and
`journalctl --user-unit <unit-name>`. A terminal `work-failed` row names the
implementer reason and records any preserved worktree. Failed or terminated
runs deliberately retain unpublished edits under
`$OSTROM_HOME/implementer-worktrees/<item-hash>`; retrying the same item reuses
that worktree. The item lease is released only after the terminal row is
durable, and systemd uses `KillMode=control-group` so termination covers Codex
and its descendants.
