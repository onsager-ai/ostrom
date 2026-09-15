# Implementer operation and diagnosis

`ostrom dispatch <work-order-file>` starts the Rust implementer through the
backend selected by `MANDATE_DISPATCH_BACKEND`. The default, `systemd`, starts
a transient user unit and requires a user service manager. `process` must be
selected explicitly for an environment without one; there is no automatic
fallback between the backends.

Both backends run:

```text
ostrom implement <work-order-file> <unit-name> [implementer-runner]
```

The process backend starts this command in a new session with null stdin and
appends stdout and stderr to the private
`implementer-item-<hash>.log` file beside its lease. The item lease records the
process id, process-group id, and process start time. That identity lets a later
pass distinguish the original implementer from a recycled process id. A
dispatching pass may end without ending the implementer; the next pass reads
the lease before deciding whether the item is still live. The environment's
init process should reap orphaned processes because a process-backend
implementer outlives its dispatcher.

The builder coordinator hands the order to the named runner in the agent
registry. `agent/codex` is the shipped default and there is no fallback. A
process-backend environment must supply the `codex` binary and its credential;
Ostrom does not provision credentials. `claude-cli` remains selectable. The
backend also requires the `setsid` utility and a Linux process-information
filesystem for session creation and PID-recycling-safe liveness checks.

Dispatch resolves the `ostrom` executable before reserving work or launching
either backend. Set
`MANDATE_OSTROM_BIN` to an absolute executable path when the interactive
`PATH` does not represent the implementer environment. The selected runner
supplies its launch environment; for Codex, this includes the resolved Node
directory required by an npm launcher's `#!/usr/bin/env node` shebang. Both
backends also receive the state path, `OSTROM_PLUGIN_ROOT`, the daily and
concurrency ceilings, the selected backend, and the item lease name.

For a failed launch, start with the dispatch error and the terminal row in
`$OSTROM_HOME/sprint.jsonl`. `ostrom-unavailable` means
`MANDATE_OSTROM_BIN` was invalid or, when the override was unset, `ostrom`
could not be resolved on `PATH`. `codex-unavailable` means the Codex executable
or its Node interpreter could not be resolved or executed. Neither failure
starts an implementer.

For the systemd backend, inspect a started unit with `systemctl --user status
<unit-name>` and `journalctl --user-unit <unit-name>`. For the process backend,
inspect the corresponding `implementer-item-<hash>.lease` and
`implementer-item-<hash>.log`; the lease is live only while its `pid` exists
with the recorded `process_start_time` in a non-terminal process state.

A terminal `work-failed` row names the implementer reason and records any
preserved worktree. Failed or terminated runs deliberately retain unpublished
edits under `$OSTROM_HOME/implementer-worktrees/<item-hash>`; retrying the same
item reuses that worktree. The item lease is released only after the terminal
row is durable. The systemd backend uses `KillMode=control-group`;
process-backend launch cleanup sends TERM to the new process group, waits
`MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS`, then sends KILL when the group
remains live. Both paths cover the runner and its descendants.
