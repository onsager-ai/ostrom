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
