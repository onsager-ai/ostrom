# Gate parity corpus

The files under `expected/` are recorded behavior snapshots for the synthetic
`gh` executable and gate configuration used by `gate_parity.rs`. The corpus was
originally captured from the retired shell gate; schema migrations regenerate
affected snapshots from the Rust implementation that now owns the behavior.

The corpus contains a clean pass, one refusal for each of the six gate
conditions, an exception-backed pass, and malformed input. Each evaluated case
records stdout, stderr, exit status, and the appended verdict row.
