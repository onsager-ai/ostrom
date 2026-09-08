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

## Answers are not delivered to a running pass

A pass does not accept an answer to a decision through this descriptor. A
control verb other than `interrupt` — including an `answer` verb a supervisor
might try to send — is recorded as `control.requested` and immediately
answered with `control.applied`, `ok: false`, `reason: "unsupported"`, and the
same `by`, exactly as steering is above. This is the same path, not a special
case carved out for answers.

The reason is structural, not a missing feature. The harness is spawned
headless, with `--print` and `stdout` piped for capture; it has no writable
stdin, so there is no channel on which a running pass could be handed an
answer even if ostrom wanted to send one. The session's `RunControl` is built
with `NoSteer`, which cannot resume the harness session either. This is the
same limitation that makes steering unsupported above, not a second one.

ostrom itself never raises a permission decision. Where a permission decision
raised elsewhere is rendered for a principal, ostrom's hook output directs the
reader to "Answer through the requesting process" rather than offering a
command, because there is no `ostrom queue` verb for it. A decision ostrom
does raise — a tripwire, a gate inconclusive, a budget decision, or one put
to a human — is answered out of band, by a separate `ostrom
queue` invocation that emits its own `decision.answered` on its own run; the
pass that raised the decision has usually already finished by the time that
answer lands.

A supervisor should treat `reason: "unsupported"` on an attempted answer as a
definite negative acknowledgement, not as a reason to wait for a timeout, and
should deliver the actual answer through the out-of-band `ostrom queue` path
instead. ostrom issue #528 tracks a permission bridge that would let a
running pass be answered directly; until that lands, the descriptor behaves
as described here.

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
