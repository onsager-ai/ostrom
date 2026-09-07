# ostrom

GitHub slug: **`onsager-ai/ostrom`** (public, MIT). Read the README first for what this ships and
how the two subsystems fit; this file carries the rules that bind changes here.

Every rule below is recorded with the decision it came from, in ostrom's own convention: a
`Source:` and falsifiable `Preconditions:`. A rule whose preconditions no longer hold is retired,
not quietly kept.

## Repo principles

**1. ostrom owns the judgment.** Classification, selection, gates, verdicts, freshness, spend
policy and the model calls that inform them live here and nowhere else. This is the positive half
of umwelt's principle 1: the runtime executes and observes, ostrom decides. A consumer that needs
a rule ostrom cannot express is a signal to change ostrom, not to grow a rule somewhere downstream.

<!-- Source: the Run Tree design, 2026-09-06, recorded in onsager-ai/umwelt CLAUDE.md and #487.
     Preconditions: assumes umwelt and ethogram stay free of decisions. Invalid if a second
     repository starts computing verdicts, at which point one of the two is in the wrong place. -->

**2. ostrom stays fully useful for a solo operator.** Every capability must work on a machine with
no hub: `ostrom up` generates units, `ostrom pass` runs, the sweep publishes. A feature that only
functions when a hosted substrate is present is a defect. ostrom never names a hosted substrate.

<!-- Source: #451, cited there as "CLAUDE.md boundary test 2"; the two-schedulers-one-contract
     shape. Preconditions: assumes the local systemd path stays supported. Invalid if the project
     ever decides ostrom is hosted-only, which would be a change of product, not of code. -->

**3. Actors are data; the binary hardcodes no actor set.** `builder` and `gatekeeper` are one
operator's configuration, not architecture. Which actors exist, what each does, and how each runs
are answers the manifest gives. Reintroducing a fixed set as spec framing — "the builder pass" —
moves the hardcoding rather than removing it.

<!-- Source: #447 and #450, which deleted PassRole, CliPassRole and DeliveryRole; the same
     correction as ostrom-hub #50. Preconditions: assumes the manifest can express every actor
     property the binary needs. Invalid if some property genuinely cannot be declared, which is a
     reason to extend the manifest. -->

**4. A refusal is distinguishable from a success.** Anything a scheduler reads must tell "it
refused" apart from "it ran and succeeded" — exit status first, then the recorded fact. A silent
zero is the defect class that has cost this project the most: a failed publication exited 0 and six
days of stale records read as a healthy loop (#478); a disarmed pass exits 0 and a hosted loop
reported success for runs that never happened (#485). An undeclared actor, an unknown operation, a
refused pass: each fails loudly.

<!-- Source: #478 (landed), #485 (open at the time of writing), #450's name-resolution contract.
     Preconditions: assumes callers are schedulers reading exit codes, not people reading stderr.
     Invalid if every caller becomes an API that reads structured output instead. -->

**5. One definition, or a test that they agree.** Where a type, format or rendered byte sequence
exists on both sides of a boundary, either there is one definition or there is a test proving the
two agree. This binds ostrom's edge to umwelt, ostrom's pass-state format to ostrom-hub's own
reader, and any golden ostrom must reproduce.

<!-- Source: #487, applied to the six boundary pairs and the reconciler goldens. Preconditions:
     assumes the other side is willing to ship a fixture. Invalid for a boundary where ostrom is
     the only reader, where a single definition is simply correct. -->

**6. A guard you have never seen fail is not a guard.** Every guard ships with a test that trips
it. A guard added without one is worse than none: it reads as coverage, and the next person defers
to it. The same applies to a test asserting current behaviour that happens to be wrong — that is
not an accident someone catches later, it is a documented decision.

<!-- Source: inherited from ostrom-hub and umwelt CLAUDE.md; earned twice during #487, where a
     collision guard shipped untested and a drift test pinned a bug as correct. Preconditions:
     none. -->

## The frozen contract with ostrom-hub

`ostrom pass <name>` is invoked as a child process by ostrom-hub. These are frozen and change only
by a spec that both repositories carry:

- the argv shape `ostrom pass <actor> [<operation>]`
- the `<name>-pass-id` and `<name>-wake-counter` identity files: 8 lowercase hex, unsigned integer
- the `<name>-dispatchability-hash` file: 64 hex
- the `pass-ended` fact in `sprint.jsonl`

ostrom-hub parses these with its own reader (`hub-server/src/loops.rs`), so principle 5 applies:
umwelt ships the fixture, and the agree test is owed.

<!-- Source: #447, #450, #451; carried unchanged through #487. Preconditions: assumes ostrom-hub
     keeps spawning the binary rather than linking it. Invalid if the hub ever links ostrom as a
     library, at which point the contract becomes a Rust API. -->

## Dependency rules

- **umwelt and ethogram are pinned by git rev**, not a registry version. Both are unpublished.
- **ostrom enables `serde_json/preserve_order`** for its own JSON byte order. umwelt deliberately
  does not, and its `TraceAppend` uses `IndexMap` fields with a frozen struct order instead — a
  producer that deserializes through `serde_json::Value` before handing umwelt a record loses the
  operator's top-level key order silently. ostrom's edge must convert, not re-parse.
- **The one-way subsystem convention.** The mandate subsystem may reuse the constitution
  subsystem's escalation-dossier shape; the constitution subsystem must never learn about mandates,
  queues, grants or GitHub. Nothing enforces this.

<!-- Source: onsager-ai/umwelt#7 and its removal; the convention from README. Preconditions:
     assumes Cargo keeps unifying features across a graph. Invalid if umwelt ever publishes, at
     which point the pin becomes a version. -->

## Working in this repository

**One checkout per session.** Two sessions sharing a working tree will move each other's `HEAD`
and can land a commit on the wrong branch. Use a worktree — `~/projects/onsager-ai/ostrom-wt-<topic>`
— and check `git status` and `git branch --show-current` before branching anywhere you do not
exclusively own. Remove the worktree when its PR merges.

**Clippy runs at the MSRV, so newer lints accumulate unseen** (#326). Run `cargo +<msrv>` to match
CI before claiming green, and do not treat a lint that only your local toolchain reports as a
regression from your change.

<!-- Source: adopted across the Run Tree repositories 2026-09-06 after a concurrent checkout moved
     HEAD mid-run; #326. Preconditions: assumes more than one session works these repos. -->

## Always-spec surfaces

Regardless of diff size, these get a spec issue:

- **The policy manifest schema and its signing.** Trust follows the signature, not the filesystem.
- **The frozen pass contract** above, and anything ostrom-hub reads.
- **The check action catalogue** and the closed selector universe.
- **The publication allowlist** and what publication may write.
- **Spend, concurrency and retention ceilings** moving between manifest and constant (#348).

## The boundary with ethogram and umwelt

| | ethogram | umwelt | ostrom |
|---|---|---|---|
| Owns | the wire | the process and the sink | the judgment |
| Depends on | nothing | ethogram | ethogram, umwelt |
| Never names | a consumer | a governor's rule | a hosted substrate |

Three tests decide which side a change belongs on:

1. Can it be written as an envelope and a payload? Then it is ethogram.
2. Does it start, bound, observe, ship or control a process? Then it is umwelt.
3. Does it decide what is true about a portfolio, an item or a verdict? Then it is here.

## Alignment boundary

Reserved to the principal: publishing a release, changing what the shipped prompts instruct, the
publication allowlist, credentials and hosting spend, and any change that makes something publicly
reachable. Everything else is an "AI implements" item — state the call, do not ask.
