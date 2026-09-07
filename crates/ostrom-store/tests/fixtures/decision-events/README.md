# Decision captures

These complete, sink-stamped events are destined for ethogram's conformance
corpus. The five original sweep fixtures remain unchanged.

`cargo +1.88 test -p ostrom-store --test decision_fixtures` reproduces and
asserts the three additional captures through the production emitters and
Umwelt's `FileSink`:

- `expected/gate-inconclusive.json`: `run_gate` reads `gate-roster.json` through
  a fixture `gh` executable. Unknown mergeability and an unrecognized required
  check conclusion produce two inconclusive conditions, offering
  `excuse:mergeable`, `excuse:required_checks`, `wait`, and `fail`.
- `expected/decision-answered.json`: `answer_queue_decision` answers that
  emitted request with `excuse:required_checks`. The fixture forge returns a
  placeholder numeric identity. The answer's reversal is
  `revoke:required_checks`, and its `decisionId` matches the gate capture.
- `expected/budget.json`: `run_pass` reads a fixture trace recording 50 USD
  spent on the fixed clock's current day, equal to its 50 USD daily cap. It
  emits `raise` and `wait`, finishes blocked, and never starts the harness.

Each test runs in an isolated child process with a fixed workflow clock and
relative state path `fixture-org/decisions`. Narration comes directly from the
emitters over placeholder inputs; it is not rewritten after capture. The
budget subject is the emitter's account scope, `account:fixture-org/decisions`.

As in the original sweep tests, the sink's wall-clock `ts` is normalized to
`2030-01-02T03:04:05.000Z`. The process ID component in generated run IDs and the
budget decision ID is replaced with `fixture`; their prefix, fixed timestamp,
and generation counter remain intact. Gate decision IDs are unchanged. No
`seq` is rewritten: the tests check the full streams before selecting the
events. Each new capture is at position 2 in its own run, after `run.started`;
the answer is in a separate judgment run.

The files were first written by running:

```sh
OSTROM_CAPTURE_DECISION_FIXTURES=1 cargo +1.88 test -p ostrom-store --test decision_fixtures -- --nocapture
```

The capture flag only creates missing files from emitted events and refuses
to overwrite existing captures. Ordinary test runs require the files to exist,
compare their pretty-printed envelopes, and parse and validate each against
the pinned ethogram SDK (`ba892e843db9df7cbc74bd52e2933799fa8cd3d9`).
