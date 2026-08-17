# Gate parity corpus

The files under `expected/` were recorded by invoking the repository's
`plugins/ostrom/scripts/gate.sh` before that script was deleted. The synthetic
`gh` executable and gate configuration are the inputs used for both the legacy
recording and `gate_parity.rs`.

The corpus contains a clean pass, one refusal for each of the six gate
conditions, an exception-backed pass, and malformed input. Each evaluated case
records stdout, stderr, exit status, and the appended verdict row.
