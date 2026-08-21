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

## Execution and scheduling

`ostrom check run` executes every resolvable mechanical criterion from the
user and repository `checks.yaml` catalogues and appends one complete run to
`check-runs.jsonl`. Every executed criterion gets its own receipt. Passes,
ordinary non-zero failures, and execution faults are distinct; a timeout or
failure does not prevent later criteria from running. The command exits
non-zero after recording the complete pass when any criterion fails, faults,
or cannot be resolved.

`ostrom plan` is also a producer: before assessment it executes criteria whose
latest verdict is stale or absent. Fresh passing and fresh failing verdicts
are reused. If execution still leaves a cited criterion stale or never run,
the goal receives `assessment_evidence_unavailable` and is not sent to the
assessor. This preserves the difference between no verdict and a recorded
failure throughout the plan document and assessor boundary.

The invocable command is the scheduling seam. An operator can install it in
the scheduler appropriate to the machine; no scheduler is installed by the
repository. For example, after replacing both paths with absolute paths, a
cron entry can run every five minutes:

```cron
*/5 * * * * OSTROM_HOME=/absolute/path/to/ostrom-state /absolute/path/to/ostrom check run
```

The equivalent systemd timer should invoke the same command from a oneshot
service and set `OnUnitActiveSec=5m`. The remaining operator step is to install
and enable that cron entry or service/timer with the desired state root and
binary path.

## Judged checks

`agent` is resolved separately through `JudgmentRegistry`; it is not an action
provider and cannot bypass the mechanical registry's reservation. Its verb is
the registered harness name. The shipped proof adapter is `agent/claude`, a
JSON-stdio executable harness whose environment is cleared before invocation.
An absent verb returns `unregistered_harness`.

The core-owned parameters are intentionally closed: `prompt`, non-empty
`evidence: [{from: <check-id>}]`, optional `model`, and the universal
`fresh_for`. Evidence references use the same exact catalogue namespace as
checks. Missing references, ambiguity, self-reference, and cycles fail before
execution.

The harness request is one JSON object:

```json
{
  "model": "opus",
  "prompt": "is the remaining difference material",
  "evidence": [
    {
      "name": "sweep-parity-diff-is-empty",
      "digest": "sha256:...",
      "output": {
        "basis": "mechanical",
        "verdict": "pass",
        "rendered": "pass"
      }
    }
  ]
}
```

Each digest identifies the exact source receipt, including its attempt and
timestamps. Re-running a source therefore makes an older judgment stale even
when the source returns the same verdict. The evidence's source freshness is
also stamped into the judged receipt, so source staleness composes directly.

A successful harness response has `verdict` and a non-empty `because` array;
each clause has `evidence` (one supplied name) and non-empty `detail`. A name
outside the bundle returns `evidence_incomplete` and no receipt is accepted.
An error response is recorded as an error-only receipt, so inability to
determine a verdict is never converted to `fail`. The executor stamps harness,
model, and version and records the exact evidence digests.

Rendered judged states are always qualified (`judged pass`, `judged fail`,
`judged stale`, or `judged never run`). Goal facts carry `basis: mechanical`
unless a contributing `met_when` check is judged, in which case they carry
`basis: includes_judgment`.

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
