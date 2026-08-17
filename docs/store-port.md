# Sweep store port

`ostrom-core::SweepStore` is the public persistence boundary for a sweep. The
sweep supplies one `SweepPass`; the configured implementation decides where
that pass is kept. The core crate has no filesystem, environment, or transport
API, and none of those concepts appears in a trait signature.

One call to `write_pass` is one transaction. The attempt and all queue, gate,
and repository-state facts become visible together. The pass identifier is the
idempotency key: an identical retry returns `Unchanged`, while different
content under the same identifier returns `PassConflict`. A failed pass with
no facts is still stored. Consequently, no stored passes means “the sweep never
ran,” not “the sweep ran and found nothing.” Write failures are returned as a
named `StoreFault`; they are never represented by an empty collection.

The portable records contain facts and Ostrom classifications only. They have
no narration field for prompts, tool output, explanations, or operator-facing
reasons, and unknown fields are rejected during deserialization. Local
compatibility adapters may keep narration in their existing private records,
but narration cannot cross the public store port.

The repository ships one implementation, `ostrom-store::JsonlSweepStore`. It
uses compact, newline-terminated JSONL in the resolved XDG state directory and
retains the Bash tools’ byte discipline. Its compatibility queue reader is
covered by the Rust/Bash byte-parity integration test. Store selection remains
part of the same typed configuration that selected the sweep inputs; the
existing scratch-config guard is evaluated before any configured publish
destination is contacted.

The check executor has a parallel `ostrom-core::CheckStore` boundary. One
`CheckRun` is one transaction and may contain zero receipts: a stored empty run
means the executor ran and selected nothing, while no stored runs means it has
never run. Implementations key idempotency by `run_id`. The in-tree
`ostrom-store::JsonlCheckStore` appends compact records to
`check-runs.jsonl` beneath the resolved XDG state root (or `OSTROM_HOME`).

## Out-of-tree implementations

Consumers should pin a Git revision of this repository and depend on
`ostrom-core` by Git. Nothing is published to crates.io, and this repository
contains no release or public-name registration plumbing.

Enable the `conformance` feature and run
`ostrom_core::conformance::check_store` against a fresh disposable instance.
The in-tree JSONL implementation runs exactly that battery. This is the
executable contract for atomic read-back, pass-id idempotency, conflict
handling, and recording failed empty attempts.

The public Rust surface follows pre-1.0 semantic-versioning policy. Compatible
additions may be made in a minor release. A breaking public API or record
contract change requires a minor release increment and migration notes. A Git
revision pin remains the authoritative consumption lock in either case.
