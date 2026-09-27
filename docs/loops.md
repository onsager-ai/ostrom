# Policy loops

A loop binds one actor and one policy operation to a cadence. Its optional
`repositories` scalar or list bounds the repositories a pass may act on; absent
or empty means every repository supplied by the operator environment. The
effective set is always intersected with that available set. A repository
outside availability or the actor-operation grant is recorded as skipped and
cannot stop granted repositories from running. A loop cannot name an action
directly.

```yaml
defaults:
  loop:
    concurrent: 6
    spend_usd: 50
    tokens: 200000
loops:
  builder-day:
    actor: builder
    operation: build-pass
    repositories: placeholder-org/portfolio
    every: 08:15..21:15
  builder-night:
    actor: builder
    operation: build-pass
    repositories: placeholder-org/portfolio
    every: ["23:15", "02:15", "05:15"]
    concurrent: 2
  gatekeeper:
    actor: gatekeeper
    operation: gate-pass
    repositories: placeholder-org/portfolio
    every: hourly
```

`every` is intentionally closed. It accepts only `hourly`, `*:MM`, an
inclusive same-minute range `HH:MM..HH:MM`, or a non-empty list of `HH:MM`
times. It does not accept cron, arbitrary systemd calendar expressions, or
other named schedules.

`concurrent`, `spend_usd`, and `tokens` resolve independently from
`defaults.loop`; a loop writes only an override. The generated service carries
the resolved values, and `ostrom loop run` refuses if a caller supplies a
different enforced value. A local `cmd/run` action receives the resolved
values in its child environment.

`ostrom pass builder --loop <name>` and `ostrom pass gatekeeper --loop <name>`
resolve the verified declaration and enforce its effective repository set.
Without `--loop`, a pass covers the available set. Sweep always covers the
whole available set, regardless of a loop's narrower list, so one fresh
generation can serve every pass.
Sweep acquisition and generation writes hold one exclusive lease with a
120-second expiry, renewed every 30 seconds while the sweep runs. Passes wait
up to 30 seconds for that lease. A killed holder therefore clears within one
expiry on every platform; where procfs is readable, a dead or recycled holder
is reclaimed immediately. Before writing a generation, the sweep confirms it
still owns the lease so a resumed, superseded holder cannot overwrite the new
owner's records.
That confirmation is checked once, immediately before the generation's first durable write, and everything after it in the same commit phase — decision requests, merge facts, the queue, the sweep snapshot, the state file, and dropped-item facts — is not re-checked write by write; a holder paused inside that window and resumed after another holder took over could still interleave with the new owner's generation. Publication, the last externally visible step in the commit phase, gets its own re-confirmation immediately before it runs: a lease lost by then refuses to publish and records why, without reporting the sweep itself as failed, because the generation's durable writes have already landed and are not undone. The window between the first check and publication remains open for those intermediate writes; re-confirming before publication narrows what a lost lease can still make visible outside the local store, it does not close the window itself.
The loop scope reaches child commands through `OSTROM_EFFECTIVE_REPOSITORIES`
in the session environment; it is a selection boundary, while grants remain
the authorization boundary. A loop-bound gatekeeper judges only pull requests
from the pass's sweep generation, so one opened afterward waits for the next
fresh generation, up to `sweep.max_age`.

An empty effective set is a failed wake named `no-effective-repositories`, with
every skipped repository and reason recorded at both ends of the pass. No
operation or agent starts. This differs from a gatekeeper wake whose effective
set is non-empty but whose supplied snapshot contains no pull requests: that
idle wake records `no-candidates`, starts no agent, and succeeds.

An unbound pass refuses the same way, and this is the case a local operator meets first. Without `--loop` the coverage is the available set, which is `OSTROM_AVAILABLE_REPOSITORIES` when that is set and otherwise derived: the repositories named by `mandates.projects`, plus those named in the manifest's `grants` and `denies`. With no manifest and no mandates that derivation is empty, so `ostrom pass builder` in a bare configuration exits 3 with `no-effective-repositories` rather than starting a session with nothing to act on. Setting `OSTROM_AVAILABLE_REPOSITORIES` is what supplies a roster in that case; it replaces the derivation rather than adding to it.

The current composed policy version can instead own loop lifecycle directly:

```sh
ostrom up
ostrom ps
ostrom logs builder-day
```

`ostrom up` is a one-shot reconciler. It verifies `<state>/current`, finds each
loop's most recent local civil-time cadence slot, records process state under
`<state>/loop-runs`, and exits. A second invocation in the same version and
cadence slot is a no-op; the next slot is a new activation. Slots more than two
hours old are recorded as `stale:slot_age_exceeded` and are not replayed. It
neither reads an uncomposed working-tree manifest nor calls systemd. The worker
receives the resolved ceilings from the manifest, and measured consumption is
checked before the operation begins. `ps` and `logs` read the persisted state
and log files; an unavailable measurement is printed as `unknown:<cause>`,
never as zero.

### Holdings

After the loop table, `ostrom ps` lists every open hold, read from local files only: the trace, the lease files, and each run's event log. An implementer is held from its `work-dispatched` row until a `work-completed` or `work-failed` row with the same `order_id`; a pass is held from its `pass-started` row until a `pass-ended` row with the same `owner`. Each hold shows its run id, its runner (the registry key, such as `agent/codex`; `-` for a pass), its item (`-` for a pass), the age of the record that opened it, the time of the newest event in that run's `events.jsonl` under `<state>/runs`, its lease state: `live`, `expired`, or `-` when no lease names the hold's owner, and the wall cap, idle cap and last progress the stall reaper judges it by. A record written before run ids were recorded shows `-` for what it lacks.

`ostrom ps --json` prints only the holds, one JSON object per line, with the fields `kind` (`implementer` or `pass`), `run_id`, `runner`, `item`, `order_id`, `owner`, `started_at`, `age_seconds`, `last_event_at`, `lease`, and since #619 `wall_seconds`, `idle_seconds`, `stall_threshold_seconds`, `last_progress_at` and `seconds_without_progress` (see Run caps and stalls); a field the record lacks is `null`. It needs no current policy version, so a scheduler or an observer can read it on a machine that has never composed one.

One run id names each hold. `ostrom dispatch` mints the implementer's run id before it starts the unit, records it in `work-dispatched` as `run_id` together with `runner` and, when the dispatcher itself runs under an ostrom run, `parent_run_id` (a pass's agent runs `ostrom dispatch`, so this is the pass-to-implementer edge), and passes it to `ostrom implement`, which writes its events and its terminal row under it. A pass records the run id it mints in `pass-started`; `pass-ended` is part of the frozen pass contract and is unchanged. A hand-run `ostrom implement` mints its own run id, as it always has, and its terminal row does not name it; its harness child still receives that id, as below.

#### The run environment is a contract

`OSTROM_RUN_ID` and `OSTROM_WORK_ORDER_ID` are an external contract. An observer outside ostrom maps a live process to the run and the order it serves by reading them from `/proc/<pid>/environ`, so neither name is renamed, removed or given a different meaning except by a spec that the observer's side carries too.

| Variable | Set on | Value |
|---|---|---|
| `OSTROM_RUN_ID` | a pass's harness process; every implementer harness process; a dispatched `ostrom implement` | the pass's `run_id` from `pass-started`; the implementer's effective run id: the `run_id` recorded in `work-dispatched`, or the one a hand run minted |
| `OSTROM_WORK_ORDER_ID` | every implementer harness process; a dispatched `ostrom implement` | the `order_id` of the work order being executed |

ostrom sets each value explicitly on the child it starts, never by changing its own process environment, and the value it sets replaces any the parent inherited from an enclosing run. A dispatched `ostrom implement` receives both through its runner's launch environment, under either dispatch backend. `ostrom implement` then sets both again, directly on its harness child, from its own effective run id and order, so an implementer run by hand inside another run (a pass's agent, say) never labels its harness with that run's id. ostrom reads `OSTROM_RUN_ID` itself only to record a dispatch's `parent_run_id`. The loop worker `ostrom up` starts does not carry the contract; it has no run id of its own.

### Run caps and stalls

Every run has a wall cap, declared or defaulted. A loop declares `wall` and `idle` beside its other ceilings, and `defaults.loop` supplies them for every loop that does not. Implementers take theirs from `defaults.implementer_ceilings`, which `ostrom dispatch` reads from the current composed version:

```yaml
defaults:
  loop:
    wall: 30m
  implementer_ceilings:
    wall: 4h
    idle: 20m
loops:
  builder-night:
    actor: builder
    operation: build-pass
    every: ["23:15", "02:15", "05:15"]
    wall: 45m
```

Durations are whole numbers of `s`, `m`, `h` or `d`. With nothing declared, a pass (and any loop run) gets a 30-minute wall cap and an implementer gets 4 hours. Neither gets an idle default: an idle cap alone is weaker than a wall cap, because a hung tool call suspends idle timing indefinitely. The defaults are named constants in `ostrom-core` (`DEFAULT_PASS_WALL_SECONDS`, `DEFAULT_IMPLEMENTER_WALL_SECONDS`, `RUN_TERMINATION_GRACE_SECONDS`), and `ostrom ps` and `ostrom doctor` show them.

Each cap is enforced inside the run. A pass hands `wall` and `idle` to its harness watchdog. An implementer checks its wall cap beside the Codex harness; when it trips, the run stops through the same path a `SIGTERM` takes and its `work-failed` row says `wall-cap`. Codex reports no per-turn events yet, so an implementer's idle cap is enforced only by the stall reaper below. Each cap also has an outer bound: a rendered loop unit's `TimeoutStartSec` and a systemd implementer unit's `RuntimeMaxSec` are the wall cap plus the five-second termination grace, so the in-process watchdog fires first and the run writes its own terminal row.

A live hold that stops making progress is reaped where ostrom already runs, with no resident process: `ostrom up` checks every open hold, and `ostrom dispatch` checks every open hold before it counts in-flight holds against the concurrency ceilings. Progress is the newest of the hold's start, its run's last event, and its transcript file's modification time. A hold is stalled when it is live and has made no progress for longer than its idle cap or, with none, its wall cap plus the termination grace. The caps are the ones the hold started with: `work-dispatched` records `wall_seconds` and `idle_seconds`, and a pass's `run.started` carries its own `ceilings`.

Reaping writes the terminal row first, then stops exactly what the hold's own lease or unit names. For the systemd backend that is the implementer's unit. For the process backend, and for a pass, it is the recorded process group, or the recorded process alone when that process does not lead its group. Before each signal the reaper re-checks the recorded pid, start time and process group, and it never signals a pid that now belongs to another process. An implementer gets `work-failed` with `reason: "stalled"`, `last_progress_at`, `stalled_seconds` and its `run_id`. A pass gets `pass-ended` with `outcome: "failed"`, `reason: "stalled"` and `recorded_by: "reaper"`. A pass whose process is already gone with no terminal row, for example one systemd stopped at its unit timeout, is closed the same way with `reason: "exited-without-terminal"`. A reaped run's `cost_usd` is its declared cost ceiling, never `null`: for an implementer, the order's `cost_ceiling_usd`; for a pass, its `run.started` `costUsd`, or the daily cap when it declared none. The real figure is unknowable once the process is gone, and the daily-cap readers must stay conservative. A reaped item can be dispatched again like any failed one, and two identical failures still escalate. `ostrom doctor`'s `work-orders` check fails on a stalled hold, naming its run id, using the same definition the reaper uses.

The pass lease names the pass's process and renews every 30 seconds while the pass runs, with a 120-second TTL (`MANDATE_LEASE_TTL_SECONDS` overrides it; a shorter TTL renews proportionally faster). A pass that cannot renew stops with `reason: "pass-lease-lost"`. `ostrom up` does not launch a loop's slot while the previous worker for that loop is still running (its pid and start time from the loop-runs state); the slot is recorded as `skipped:previous-live` in the state and the log, and `up` reports `skipped=` beside its other counts.

Render and verify artifacts with:

```sh
ostrom loops render --output /path/to/fixture-or-unit-source
ostrom loops check /path/to/installed-units
```

Rendering writes the inspectable `ostrom-loop-*.service` and `.timer` files,
the `ostrom-up.service` oneshot unit, and an `ostrom-up.timer` that requests a
reconciliation every five minutes. It never calls systemctl and never enables,
starts, or reloads a unit. `sys/enable-loop` remains an ungrantable action.
Installing and enabling the rendered reconciler timer is therefore a separate
principal-controlled step.

Each unattended agent is a distinct actor with its own derived operation
settings profile. In particular, queue triage is modeled as a separate actor
and operation rather than sharing or widening the builder profile. A rendered
service invokes only `ostrom loop run <name>`; there is no inline shell
ExecStart.

The builder loop actor is a coordinator, not the implementation engine. It
selects work, writes durable work orders, and dispatches each order to a named
implementer harness. The shipped default is `agent/codex`; changing the
implementer is a runner registration and named handoff, while the builder's
coordination path remains unchanged.
