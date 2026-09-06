# umwelt

**The harness runtime.** Spawns or attaches to a coding-agent session, bounds it, normalises what it does into [ethogram](https://github.com/onsager-ai/ethogram) events, ships them to a sink, and carries control back. Shared by [`ostrom`](https://github.com/onsager-ai/ostrom), `ostrom-hub`, and a companion that runs on an operator's own machine.

In von Uexküll's ethology the *Umwelt* is the bounded world an organism perceives and acts in. That is what this is for an agent: the sandbox, the caps, the tools, and the channel through which its behaviour is recorded. Ethogram is the vocabulary of behaviour; umwelt is the world it happens in.

## Status

**Scaffold.** Nothing depends on this repository yet. The founding decisions below are settled; the first extraction is not.

## Why it is a separate repository

Two codebases hold the same plumbing today. chreode's `packages/agent-runner` (TypeScript) spawns five harnesses, normalises their NDJSON, enforces caps, and tees transcripts. ostrom's `ostrom-store` and `ostrom-checks` (Rust) spawn two harnesses three different ways and capture two facts from a whole session. Both are downstream of the thing they would need to share.

Ethogram cannot hold this: its principle 2 admits only what is expressible as an envelope and a payload, and a process supervisor is not. ostrom cannot hold it: chreode would then depend on a governor. So it stands alone, depends on ethogram and nothing else of theirs, and both depend on it.

## Why it is public

The same reason ethogram is. A runtime that only one hub can link is a moat pretending to be a library.

## The founding decisions

**A run is one harness session.** The runtime's unit is the run ethogram defines. A loop pass, an implementer handoff, a subagent, an interactive session and a judgment are kinds of run; nothing here has a second unit.

**Spawn and attach are the same interface.** A run the runtime started (a hosted pass) and a run it found (a transcript a harness is writing on a laptop) produce the same event stream through the same sink. The attach path is not a lesser mode; it is how the operator's own sessions are observed at all.

**Bounds are enforced here, and reported as events.** Wall clock, idle (suspended while a tool call is in flight), turns, tokens, dollars. A trip terminates the process group with grace and emits `run.finished` with the outcome and a usage lower bound, so a cap-killed run never reports zero.

**The sink is a trait, and the file sink is the reference.** Every run writes `events.jsonl` locally. A remote sink is additive. `seq` and `ts` are stamped by the first sink and preserved by every later one.

**Control is two verbs.** `interrupt` and `steer`, exactly as ethogram defines them. What a harness cannot do headlessly is not offered.

**Nothing here decides.** No classification, no gate, no verdict, no model call. A consumer that needs one has ostrom.

<!-- Source: principal, 2026-09-06, from the Run Tree design. Preconditions: assumes ethogram stays wire-only and ostrom-core never depends on this crate. Invalid if either changes, which is a spec on the repository that changed. -->

## Layout

```
crates/umwelt-runtime/    spawn, process group, caps watchdog, sink trait, file sink, shipper
crates/umwelt-capture/    normalisers: claude-code stream-json, codex exec --json → ethogram drafts
crates/umwelt-companion/  the operator-machine daemon: attach adapters, hooks bridge, dial-out
```

The bare name `umwelt` is taken on crates.io and npm by unrelated projects; crates publish under these prefixed names and the repository keeps the short one.

## Licence

MIT.
