# Policy loops

A loop binds one actor and one policy operation to a cadence. The operation is
dispatched through the same grant, deny, target-resolution, check, and action
boundaries as an interactive operation; a loop cannot name an action directly.

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
    target: placeholder-org/portfolio
    every: 08:15..21:15
  builder-night:
    actor: builder
    operation: build-pass
    target: placeholder-org/portfolio
    every: ["23:15", "02:15", "05:15"]
    concurrent: 2
  gatekeeper:
    actor: gatekeeper
    operation: gate-pass
    target: placeholder-org/portfolio
    every: hourly
  sweep:
    actor: sweeper
    operation: portfolio-sweep
    target: placeholder-org/portfolio
    every: "*:45"
    publish: placeholder-org/public-mirror
    cadence_hours: 24
    stuck_after_days: 7
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

Render and verify artifacts with:

```sh
ostrom loops render --output /path/to/fixture-or-unit-source
ostrom loops check /path/to/installed-units
```

Rendering only writes `ostrom-loop-*.service` and `.timer` files. It never
calls systemctl and never enables, starts, or reloads a unit. `sys/enable-loop`
remains an ungrantable action. Enabling a rendered timer is therefore a
separate principal-controlled installation step.

Each unattended agent is a distinct actor with its own derived operation
settings profile. In particular, queue triage is modeled as a separate actor
and operation rather than sharing or widening the builder profile. A rendered
service invokes only `ostrom loop run <name>`; there is no inline shell
ExecStart.
