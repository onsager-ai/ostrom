# umwelt

GitHub slug: **`onsager-ai/umwelt`** (public, MIT).

The harness runtime. Read the README first for what this is and why it stands alone; this file carries the rules that bind changes here.

## Repo principles

**1. Nothing here decides.** No classification, selector, gate, verdict or model call. A change that adds one is a defect, not a feature. The line is the same as ostrom-hub's principle 3, held one layer down.

The companion's permission bridge is the closest case and stays on the right side of the line: it transports a decision request to a human and carries the answer back. A timeout is a refusal to proceed without a decision, never a denial the companion computed. A rule, a pattern or a remembered previous answer living on an operator's machine would be a governor on a laptop.

<!-- Source: repo scaffold, 2026-09-06, from the Run Tree design. Preconditions: assumes ostrom remains the sole home of judgment. Invalid if a consumer ever needs a rule ostrom cannot express, which is a signal to change ostrom. -->

**2. Bounded at capture, once.** Every field that carries what an agent said or did is excerpted here, with ethogram's `excerpt()` and flag, before it reaches any sink. A sink that receives an over-bound event refuses it; nothing downstream truncates a second time.

Which fields are bounded is ethogram's rule, not ours. `parse_event` deliberately does not check bounds — folding them in would make a bound change breaking — so a sink refuses by calling ethogram's `validate` on both `append` and `forward`, rather than by holding its own copy of which fields are bounded. A refusal is a recorded gap with a named cause, never a silent drop.

**3. A cap that trips is reported.** Wall, idle, turn, token and dollar caps terminate the process group and emit `run.finished` with the outcome and a usage lower bound. A run that died silently with no terminal event is a bug here, never a consumer's problem to infer.

**4. Attach is not a lesser mode.** A normaliser is correct when its output for a golden raw transcript equals the golden events, whether the runtime spawned the process or found the file. Every normaliser ships with both.

**5. Two verbs, honestly.** `interrupt` and `steer` are offered only where a harness can honour them headlessly. A verb that would be accepted and not applied is not offered.

**6. Never name the governor.** No ostrom-specific constant, environment variable or rendered string in non-test source; a caller supplies its own at ostrom's edge. The no-`ostrom-*` manifest test cannot see this — a module passes it while hardcoding `ostrom-loop-` — and a generator only ostrom can call leaves chreode exactly where it started, which is the argument this repository exists on. Test code is exempt; check per file up to its first `#[cfg(test)]`, and never obfuscate a literal to satisfy a search.

## Dependency rules

- **ethogram is pinned by git rev**, not a registry version. It is unpublished and the names are provisional.
- **No `ostrom-*` crate**, asserted over the manifest text. The test matches `ostrom[-_]` because a rename can spell it either way.
- **umwelt must not enable `serde_json/preserve_order`.** Cargo unifies features across a whole graph, so enabling it here turns it on inside every crate a consumer builds — it once came close to silently disabling ethogram's canonical key sort while both repositories' tests stayed green. Byte order is a property of our own types instead: the trace record is a struct with a frozen field order and `IndexMap` fields, and a manifest test forbids the feature returning.

## Working in this repository

**One checkout per session.** Two sessions sharing a working tree will move each other's `HEAD` and can land a commit on the wrong branch. Use a worktree — `~/projects/onsager-ai/<repo>-wt-<topic>` — and check `git status` and `git branch --show-current` before branching anywhere you do not exclusively own.

## Always-spec surfaces

Regardless of diff size, these get a spec issue:

- **A new harness normaliser**, or a change to what an existing one emits. Its golden fixtures are ethogram's conformance corpus, so the spec lands there too.
- **The sink trait.** Every consumer implements it.
- **Cap semantics**: what a cap measures, when idle is suspended, what outcome a trip records.
- **The companion's attach paths and hooks bridge**: what it reads on an operator's machine and what leaves it.

## The boundary with ostrom and ethogram

| | ethogram | umwelt | ostrom |
|---|---|---|---|
| Owns | the wire | the process and the sink | the judgment |
| Depends on | nothing | ethogram | ethogram, umwelt |
| Never names | a consumer | a governor's rule | a hosted substrate |

Three tests decide which side a change belongs on:

1. Can it be written as an envelope and a payload? Then it is ethogram.
2. Does it decide what is true about a portfolio, an item or a verdict? Then it is ostrom.
3. Does it start, bound, observe, ship or control a process? Then it is here.

## Assertions that can fail

Inherited from ostrom-hub, because the same defects will happen here: a guard you have never seen fail is not a guard. Every cap has a test that trips it. Every normaliser has a raw line it refuses. The "no dependency on ostrom-core" property is a test over `Cargo.toml`, not a comment.

**A control must travel the same path as the case it controls.** *"It works over here"* is evidence only when over-here and over-there differ in exactly one thing. Check that before reporting which component is at fault — three defects in this work were reported against the wrong component because the probe was never checked:

- A cost value survived `serde_json::to_string` but not ethogram's canonical path, reported here as a serialiser defect. The control parsed a Rust literal through rustc while the failing path parsed through serde_json: two parsers, two different doubles. The defect was in parsing.
- ethogram's exponent sweep compared `10.powi(e)` against `Math.pow(10, e)` and reported 150 serialiser divergences. The two produce different doubles, so it was comparing serialisations of different numbers. Regenerating from decimal literals, which both languages parse identically, gave zero.
- A determinism check diffed generated files against the commit rather than against the previous run, guaranteeing a diff and briefly reading as non-determinism.

## Alignment boundary

Reserved to the principal: publishing a crate, adding a harness, and anything that changes what leaves an operator's machine. Everything else is an "AI implements" item — state the call, do not ask.
