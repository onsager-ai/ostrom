# Pass control descriptor

`ostrom pass <role> --control-fd <N>` reads newline-delimited ethogram
`control.requested` drafts from an inherited descriptor. `OSTROM_CONTROL_FD`
selects the same descriptor; the flag takes precedence, including over an invalid
environment value. The internal pass worker receives the resolved number.
Replies join the durable run log and its existing `--events-fd` /
`OSTROM_EVENTS_FD` mirror.

## Who can write to this

The descriptor is inherited from the spawning supervisor at spawn time, just
like the events descriptor. It is not a socket and is not network-reachable.
The only writer is the spawning supervisor. Ostrom creates no listener, named
pipe, or configurable path for controls. Authorisation belongs to the supervisor:
ostrom records `by` without checking it, including unfamiliar or empty identities.
This adds no reachable surface.

## Input and replies

Each line is an ethogram `EventDraft`, with `type` and `payload`, without an event
envelope. For example:

```json
{"type":"control.requested","payload":{"controlId":"stop-17","kind":"interrupt","by":"supervisor"}}
{"type":"control.requested","payload":{"controlId":"steer-18","kind":"steer","by":"supervisor","text":"Reconsider the next step"}}
```

The pass validates drafts with the pinned ethogram vocabulary before acting.
Only `control.requested` is admitted. Ostrom's sink stamps the request's `runId`,
`seq`, and `ts`; the supervisor does not supply them. Request payload extensions
are preserved. Idempotency and delivery tracking remain the supervisor's work.

An interrupt uses umwelt's existing `RunControl::interrupt`, child process group,
termination grace, and watchdog open-tool-call identity. Its events are ordered:

1. `control.requested`, preserving `controlId` and `by`.
2. `control.applied` with `ok: true` and `landedIn` when a tool call is open.
3. Exactly one `run.finished` with `outcome: "interrupted"`.

Descriptor responses also preserve `by` as a payload extension on
`control.applied` and the interrupt's `run.finished`. The pass exits with code 130
and releases its leases. Normal exit and guard cleanup do not append another
terminal event. The existing SIGTERM fallback retains its existing behaviour.

Steering is currently unsupported: `NoSteer` cannot resume the harness session.
A valid steer is recorded as `control.requested`, immediately followed by
`control.applied` with `ok: false`, `reason: "unsupported"`, and the same `by`.
Nothing is queued, and the pass continues to its normal terminal event.

## Permission answers

Live permission answers require adopted policy and a derived profile. With an
operator-owned `roles/<role>.settings.json`, an answer still receives
`control.applied` with `ok: false`, `reason: "unsupported"`, and the same `by`.
Ostrom never edits or composes over that operator-owned file.

For a derived profile, the pass writes per-run settings and MCP configuration
under its run directory before spawning Claude. Settings contain the policy
renderer's profile, without a hooks block, except for one override: the bridge
sets `permissions.defaultMode` to `default` where the renderer emits `dontAsk`.
Measured against Claude Code 2.1.265, an ungranted call under `dontAsk` is
refused before the permission-prompt tool is ever consulted — there would be
nothing for this bridge to receive — while `default` is the mode that reaches
the tool for exactly that call. This override applies only to the bridged,
per-run settings; an unbridged operator-owned or generated profile keeps
`dontAsk`, which stays correct there because a prompt with nobody to answer it
should be denied. The MCP config registers
`ostrom permission-server --channel <private-path>` as `ostrom_permission` and
sets its per-server `timeout` in milliseconds. Claude receives `--mcp-config`,
`--strict-mcp-config`, `--permission-prompts host`, and
`--permission-prompt-tool mcp__ostrom_permission__approve`.

The channel file and transport messages are mode 0600 inside a mode 0700
directory; the runner removes that directory, the settings, and the MCP config
at run end, including interruption and errors. Durable events remain. Private
channel opening supports Linux and macOS; other platforms refuse bridge setup.
Two runs never share `roles/<role>.derived.settings.json`.

The stdio server supports MCP initialization, tool discovery, and calls to its
single `approve` tool. Claude sends `tool_name`, `input`, and `tool_use_id`;
the response is one MCP text content block containing a JSON allow/deny object.
An allow returns the original input in `updatedInput`.

A call the actor's grants permit is auto-allowed by Claude itself under
`default` mode before the permission-prompt tool is ever consulted, so
unattended operation is unaffected: the grants that already authorize an
operation keep authorizing it without a principal in the loop. The bridge
independently re-checks the rendered `Bash(ostrom <operation> *)` grants,
conservatively requiring an `ostrom` command with plain arguments, and treats
a call its own check finds granted as belt-and-braces — it should not
normally arrive at all — and auto-allows it rather than escalating.

A call the grants do not permit is exactly what reaches the permission-prompt
tool. For that call, the handler asks the runner to emit `decision.requested`
through its existing sink, then waits for an answer: this becomes a decision
the principal answers, not an unattended denial. Shell syntax and quoted
arguments the bridge's conservative check cannot recognize as a rendered
grant fall into the same path. An unanswered decision expires and denies, as
below.

A decision offers `allow` and `deny`, sets `expiresAt` to its deadline, and sets
ethogram's decision-level `onTimeout` to `deny`. Its wait is 30 seconds. The
per-server MCP `timeout` is derived as `(wait + five-second margin) * 1000`,
currently 35000 ms. The inequality test reads the emitted MCP and channel
configurations. No environment inheritance is assumed for the path or timeout.

Ostrom's explicit denial is the expiry mechanism. Its stable, model-visible
message is `<tool_use_id>: no answer within onTimeout`, with `behavior: deny`
and `interrupt: false`; a missed answer denies the tool call without stopping
the pass. Pass caps still apply. No answer, a lost channel, and invalid transport
data fail closed. A step-1 probe against Claude Code 2.1.265 observed a response
after a 90 s stall being honoured. No shorter bound was in force; a bound above
90 s was never measured. The design relies only on Ostrom's wait and explicitly
sets the server timeout rather than inferring a harness default.

The spawning supervisor sends an `answer` control whose `decisionId` is the
harness's `tool_use_id` and whose `optionId` is an offered option. Before
forwarding, the runner rejects unknown decisions, duplicate answers, and
unoffered options with `no-such-decision`, `already-answered`, and
`option-not-offered`. An expired or unavailable channel receives `not-live`.
`by` is preserved without interpreting the principal identity.

After the handler acknowledges the choice, the runner emits `decision.answered`
with `requestedRunId` naming the asking run, followed by `control.applied` with
`ok: true`. Expiry emits `decision.answered` with `byTimeout: true`; a forwarded
control that did not arrive in time gets `ok: false`. Requests, replies, and
receipts match on `tool_use_id`; the per-channel sequence only orders requests.
Repeated tool-use ids cannot reuse an earlier allow.

Claude Code 2.1.265's `doctor` validates the settings but ignores the MCP carrier:
it also accepts deliberately malformed `mcpServers` fields. The agreement test
passes the rendered files and pins that limitation. Local protocol and pass
lifecycle tests cover the emitted MCP argv, framing, and timeout; doctor success
is not evidence that Claude loaded the server.

Tripwire, gate, budget, and other out-of-band decisions retain their existing
`ostrom queue` answer path. Steering remains unsupported on a Claude pass.

Malformed JSON, invalid drafts, and other event types produce
`capture.refused` with `cause: "malformed"` and a bounded explanation. The reader
continues with the next line, including after invalid UTF-8. An unavailable or
unreadable descriptor records a refusal and a stderr diagnostic; the pass
continues. EOF simply ends control input. As with capture, a failure of the
durable event store itself follows the existing pass error path.

The pass loop alone applies controls and emits events. A reader thread hands
off at most one pending line and never writes to the sink. On terminal exit the
receiver is dropped; any line still being read is dropped without emission.
Neither EOF nor closing the supervisor's write end is required for the pass to
finish. Nothing is emitted after `run.finished`.

## Compatibility

Without a control descriptor, there is no reader and stdin is not interpreted
as control input. Existing pass output, capture bytes, signal handling, identity
files, and the `pass-ended` fact are unchanged. The only argv addition is the
optional descriptor flag. Dependency revisions remain ethogram `9e3cd370` and
umwelt `1c2c1f60`.
