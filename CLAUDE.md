# umwelt

GitHub slug: **`onsager-ai/umwelt`** (public, MIT).

The harness runtime. Read the README first for what this is and why it stands alone; this file carries the rules that bind changes here.

## Repo principles

**1. Nothing here decides.** No classification, selector, gate, verdict or model call. A change that adds one is a defect, not a feature. The line is the same as ostrom-hub's principle 3, held one layer down.

<!-- Source: repo scaffold, 2026-09-06, from the Run Tree design. Preconditions: assumes ostrom remains the sole home of judgment. Invalid if a consumer ever needs a rule ostrom cannot express, which is a signal to change ostrom. -->

**2. Bounded at capture, once.** Every field that carries what an agent said or did is excerpted here, with ethogram's `excerpt()` and flag, before it reaches any sink. A sink that receives an over-bound event refuses it; nothing downstream truncates a second time.

**3. A cap that trips is reported.** Wall, idle, turn, token and dollar caps terminate the process group and emit `run.finished` with the outcome and a usage lower bound. A run that died silently with no terminal event is a bug here, never a consumer's problem to infer.

**4. Attach is not a lesser mode.** A normaliser is correct when its output for a golden raw transcript equals the golden events, whether the runtime spawned the process or found the file. Every normaliser ships with both.

**5. Two verbs, honestly.** `interrupt` and `steer` are offered only where a harness can honour them headlessly. A verb that would be accepted and not applied is not offered.

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

## Alignment boundary

Reserved to the principal: publishing a crate, adding a harness, and anything that changes what leaves an operator's machine. Everything else is an "AI implements" item — state the call, do not ask.
