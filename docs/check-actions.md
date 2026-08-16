# Check actions

`ostrom-checks` resolves each authored `domain/verb` through an
`ActionRegistry`. An `ActionProvider` owns one exact domain, declares its exact
verbs and stable action metadata, and validates the otherwise opaque `with`
map by preparing an executable action. Registering a second owner returns
`ambiguous_domain`; resolving an absent domain or verb returns
`unregistered_action`.

Registration has no basis or judgment field. The reserved `agent` domain is
rejected with `judged_domain_registration`, leaving `ostrom-core`'s domain
derivation as the only source of basis truth.

The shipped registry contains:

- `http/get`: `url` and `expect` are required; `timeout` is optional.
  Expectations are deliberately limited to `status <op> integer` and
  `path.to.value|length <op> integer`, where `<op>` is one of `==`, `!=`, `>`,
  `>=`, `<`, or `<=`. Length applies to JSON arrays, objects, and strings.
  There is no general jq, arithmetic, boolean composition, filtering, or
  arbitrary value comparison; those expressions return `unsupported_expect`.
- `cmd/run`: `script` is required and is passed to `sh -c`; `timeout` is
  optional. Exit zero passes, ordinary non-zero exits fail, and an inability
  to spawn or locate the command is `cmd_execute_error`.
- `doctor/check`: `check` selects one exact doctor check name and `timeout` is
  optional. The adapter runs `node doctor.js --check <name>` and accepts one
  text-protocol line. `OK` passes, `FAIL` fails, `WARN` is `doctor_warn`, and
  `DEFER` is `doctor_defer`.

Numeric timeouts are seconds. String timeouts accept positive integer `ms`,
`s`, or `m` suffixes and default to 30 seconds. `fresh_for` remains the one
universal core parameter and is accepted alongside each provider's keys.
