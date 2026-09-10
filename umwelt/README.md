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

## What a consumer needs to know

**Run directories are percent-encoded, and the encoding is a contract.** A run id is an arbitrary wire string, so every byte outside `[A-Za-z0-9_-]` becomes `%XX` and the empty run id becomes `%`; a run id of `..` therefore cannot escape the sink root. `umwelt_runtime::run_directory_name` is public because anything locating a run's `events.jsonl` — `ostrom logs`, a shipper, `--events-fd` — must call it rather than reproduce it. Dotted run ids are unreadable on disk as a consequence: `run.1` is the directory `run%2E1`. That is deliberate.

**`FileSink` writes with ethogram's `serialise_event`, never `serde_json::to_string`.** The canonical form — declared envelope key order, payload objects sorted recursively by UTF-8 bytes, ECMAScript number notation — is what makes `events.jsonl` comparable across languages.

**Two ceiling types, and only one reaches the wire.** `LoopCeilings` is loop scheduling: how many workers a scheduled loop may run. `RunCaps` is per-run enforcement — wall, idle, turns, tokens, cost — plus `kill_grace_ms`, which is an enforcement detail no consumer needs and never leaves the process. `RunCaps::to_wire()` produces ethogram's `RunCeilings`.

**A harness declares the caps it can honestly enforce, and `prepare` refuses the rest.** A cap accepted and not applied is worse than one refused, because the operator believes they are protected. Codex claims only wall today: its `exec --json` schema is unverified, and a claim resting on an unverified schema is a guess wearing a guarantee.

**Idle suspension trusts the harness.** Idle does not advance while a tool call is in flight, so a tool call that hangs and never reports a result suspends the idle cap indefinitely — such a run is bounded by the wall cap, not the idle cap. A run declaring idle without wall is accepted and warned about at start, so the operator learns it from the run rather than from an incident.

**Golden fixtures live here, normalised events live in ethogram.** A case is `crates/umwelt-capture/tests/fixtures/<harness>/<case>/` holding `raw.ndjson`, `expected.jsonl` and a `meta.toml` recording the CLI version and capture date — because a fixture without provenance cannot later be told apart from a guess. Goldens are compared as canonical bytes, and every case runs through both a file source and an in-memory source, which is how spawn and attach are proved equal rather than tested twice.

**Capture tooling lives in `tools/`.** `tools/capture-codex-two-turns.sh` runs one codex session across two turns — `codex exec`, then `codex exec resume --last` in the same thread — and writes both raw streams, a concatenated `raw.ndjson` and a draft `meta.toml`. It exists to settle one question a single-turn capture cannot answer: whether codex reports **session-cumulative** usage on `turn.completed` or a **per-turn increment**. `input_tokens` cannot tell them apart, because the prompt carries the conversation history and so grows every turn regardless. `output_tokens` can, which is why turn 1 asks for a deliberately long reply and turn 2 for a single word: if turn 2's `output_tokens` comes back small the usage is an increment and a codex normaliser must accumulate before emitting, and if it comes back at or above turn 1's the usage is cumulative and the normaliser must not. The script prints that verdict. It starts a real paid harness process, so run it only when a capture has been authorised, and never retry it after a usage-limit error — stop and report instead.

**The companion, on an operator's machine:** what the agent said and did leaves, bounded and with machine paths rewritten; transcripts and files do not. Scrubbing happens at capture, so the local `events.jsonl` and the shipped stream hold the same bytes and there is no second, more permissive path.

## Layout

```
crates/umwelt-runtime/    spawn, process group, caps watchdog, sink trait, file sink, shipper
crates/umwelt-capture/    normalisers: claude-code stream-json, codex exec --json → ethogram drafts
crates/umwelt-companion/  the operator-machine daemon: attach adapters, hooks bridge, dial-out
```

Raw captures live in `crates/umwelt-capture/tests/fixtures/`; ethogram holds only the normalised events.

The bare name `umwelt` is taken on crates.io and npm by unrelated projects; crates publish under these prefixed names and the repository keeps the short one.

## Licence

MIT.
