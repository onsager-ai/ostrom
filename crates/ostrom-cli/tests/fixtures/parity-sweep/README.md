# Recorded sweep parity corpus

`queue.shell.jsonl` was captured from the shipped shell sweep at
`3b09bee2489946fa1ad6777dd550923e2b813014` before that implementation was
retired. The capture ran at `2026-08-01T00:00:00Z` in an isolated scratch
home. Local command adapters returned the placeholder GitHub observations in
`github.json`, token minting was intercepted, and publication was refused by
the scratch-home guard.

The corpus deliberately covers an `ostrom/` branch without a matching work
order. This produces a nested dossier whose leaf fields exercise the parity
command's per-field comparison. All identities and bytes are synthetic.
