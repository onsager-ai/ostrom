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
no hosted substrate at all: `ostrom up` generates units, `ostrom pass` runs, the sweep publishes.
If removing something would break the standalone CLI for one person on a laptop, it belongs here.
A feature that only functions when a hosted substrate is present is a defect.

The failure this guards against runs both ways: commercial concerns leaking into the commons, and
the commons being hollowed into a stub that only works when paired with the paid thing. ostrom must
remain fully useful to someone who never learns a hosted substrate exists.

<!-- Source: the boundary tests recorded one layer up in the commercial lane, 2026-08-13, restated
     here without naming it; cited by #451 as "CLAUDE.md boundary test 2". Preconditions: assumes
     the local systemd path stays supported. Invalid if the project ever decides ostrom is
     hosted-only, which would be a change of product, not of code. -->

**3. ostrom names no hosted substrate.** No commercial substrate, hosted URL, customer, or
downstream repository appears anywhere here — not in code, not in configuration, not in a comment,
and not as a citation. The dependency runs one way only, and it does not run back. Where a rule
originated downstream, restate it; do not cite its home.

<!-- Source: the same 2026-08-13 boundary tests, test 3; enforced on this file by #490's review,
     which caught it naming a downstream repository eight times while stating this rule.
     Preconditions: assumes ostrom stays the open commons half of the pair. -->

**4. Actors are data; the binary hardcodes no actor set.** `builder` and `gatekeeper` are one
operator's configuration, not architecture. Which actors exist, what each does, and how each runs
are answers the manifest gives. Reintroducing a fixed set as spec framing — "the builder pass" —
moves the hardcoding rather than removing it. This is the standard a change is held to, not a
description of the binary today: the fixed role enums are still here, and #537 records where.

<!-- Source: #447 and #450, which specify deleting PassRole, CliPassRole and DeliveryRole. Both
     are open and neither has been implemented: the three enums are live at
     crates/ostrom-store/src/pass.rs:70, crates/ostrom-cli/src/main.rs:615 and
     crates/ostrom-checks/src/doctor.rs:1481 (#537). Preconditions: assumes the manifest can
     express every actor property the binary needs. Invalid if some property genuinely cannot be
     declared, which is a reason to extend the manifest. -->

**5. A refusal is distinguishable from a success.** Anything a scheduler reads must tell "it
refused" apart from "it ran and succeeded" — exit status first, then the recorded fact. A silent
zero is the defect class that has cost this project the most: a failed publication exited 0 and six
days of stale records read as a healthy loop (#478); a disarmed pass exits 0 and a scheduler
recorded success for runs that never happened (#485). An undeclared actor, an unknown operation, a
refused pass: each fails loudly.

<!-- Source: #478 (landed), #485 (open at the time of writing), #450's name-resolution contract.
     Preconditions: assumes callers are schedulers reading exit codes, not people reading stderr.
     Invalid if every caller becomes an API that reads structured output instead. -->

**6. One definition, or a test that they agree.** Where a type, format or rendered byte sequence
exists on both sides of a boundary, either there is one definition or there is a test proving the
two agree. This binds ostrom's edge to umwelt, ostrom's pass-state format to any independent reader
of it, and any golden ostrom must reproduce.

<!-- Source: #487, applied to the six boundary pairs and the reconciler goldens. Preconditions:
     assumes the other side is willing to ship a fixture. Invalid for a boundary where ostrom is
     the only reader, where a single definition is simply correct. -->

**7. A guard you have never seen fail is not a guard.** Every guard ships with a test that trips
it. A guard added without one is worse than none: it reads as coverage, and the next person defers
to it. The same applies to a test asserting current behaviour that happens to be wrong — that is
not an accident someone catches later, it is a documented decision.

<!-- Source: inherited from the wider Run Tree rules and umwelt's CLAUDE.md; earned twice during
     #487, where a collision guard shipped untested and a drift test pinned a bug as correct.
     Preconditions: none. -->

**8. Fact and narration stay separate.** A trace row carries exactly `ts`, `kind`, `fact` and
`narration`, and the store rejects anything else. Facts are evidence one delivery role may consume
from another's trace; narration is principal-facing context and is not. They keep separate read
paths (`TraceView::Facts` and `TraceView::Narration`) because merging them would let narration be
consumed as evidence.

<!-- Source: the rule the store already enforces in crates/ostrom-store/src/trace.rs; restated
     here because it is a judgment boundary, which is why the read path stayed in ostrom when the
     append half moved to umwelt (#487). Preconditions: assumes roles read each other's traces. -->

## The frozen pass contract

`ostrom pass <name>` is invoked as a child process by a supervisor: by hand, or by any scheduler an
operator writes. **Not by the units `ostrom up` generates** — those invoke `ostrom loop run <name>`
(`ostrom-checks/src/umwelt_edge.rs`), and nothing generates an `ostrom pass` unit. A scheduler may
still invoke a pass on a timer, but the argv is the same either way, so ostrom cannot observe that one
did; that is why a pass declares `handoff` and not `loop` (#546), and why letting a scheduler declare
its schedule is a change to this contract (#550). These are frozen and change only by a spec that both
sides carry:

- the argv shape `ostrom pass <role>` — today a fixed `builder | gatekeeper`; the open
  `ostrom pass <actor> [<operation>]` form is #447's intent, not the code (#537)
- the `<name>-pass-id` and `<name>-wake-counter` identity files: 8 lowercase hex, unsigned integer
- the `<name>-dispatchability-hash` file: 64 hex
- the `pass-ended` fact in `sprint.jsonl`

A scheduler may parse these with its own reader rather than linking ostrom, so principle 6 applies:
umwelt ships the fixture, and the agree test is owed.

<!-- Source: #447, #450, #451; carried unchanged through #487. Preconditions: assumes schedulers
     keep spawning the binary rather than linking it. Invalid if a caller ever links ostrom as a
     library, at which point the contract becomes a Rust API. -->

## Dependency rules

- **`ostrom-core` depends on neither umwelt nor ethogram.** The domain types and the store port
  stay free of the runtime; only `ostrom-store` and `ostrom-checks` reach the boundary.
- **umwelt and ethogram are pinned by git rev**, not a registry version. Both are unpublished.
- **ostrom enables `serde_json/preserve_order`** for its own JSON byte order. umwelt deliberately
  does not, and its `TraceAppend` uses `IndexMap` fields with a frozen struct order instead — a
  producer that deserializes through `serde_json::Value` before handing umwelt a record loses the
  operator's top-level key order silently. ostrom's edge must convert, not re-parse.
- **The one-way subsystem convention.** The mandate subsystem may reuse the constitution
  subsystem's escalation-dossier shape; the constitution subsystem must never learn about mandates,
  queues, grants or GitHub. Nothing enforces this.

<!-- Source: onsager-ai/umwelt#7 and its removal; the subsystem convention from README.
     Preconditions: assumes Cargo keeps unifying features across a graph. Invalid if umwelt ever
     publishes, at which point the pin becomes a version. -->

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

- **The store ports.** `ostrom-core::SweepStore` and its siblings are the public persistence
  boundary; a change there is a change to every substrate that implements them.
- **The record shapes.** Trace rows, queue rows, gate records, pass state — anything another
  process reads off disk.
- **The frozen pass contract** above.
- **The policy manifest schema and its signing.** Trust follows the signature, not the filesystem.
- **The check action catalogue** and the closed selector universe.
- **The publication allowlist** and what publication may write.
- **Spend, concurrency and retention ceilings** moving between manifest and constant (#348).

## The boundary with ethogram and umwelt

| | ethogram | umwelt | ostrom |
|---|---|---|---|
| Owns | the wire | the process and the sink | the judgment |
| Depends on | nothing | ethogram | ethogram, umwelt |
| Never names | a consumer | a governor's rule | a hosted substrate |

Four tests decide where a change belongs:

1. Can it be written as an envelope and a payload? Then it is ethogram.
2. Does it start, bound, observe, ship or control a process? Then it is umwelt.
3. Does it decide what is true about a portfolio, an item or a verdict? Then it is here.
4. Would removing it break the standalone CLI for a solo operator with no hosted substrate at all?
   Then it belongs here, whatever else it looks like.

## Alignment boundary

Reserved to the principal: publishing a release, changing what the shipped prompts instruct, the
publication allowlist, credentials and hosting spend, and any change that makes something publicly
reachable. Everything else is an "AI implements" item — state the call, do not ask.
