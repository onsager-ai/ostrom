# Platform packages

The package directories below mirror the entries in
`npm/publish.config.json`. Release staging writes each generated manifest and
CI-built binary under `target/npm-packages/<directory>` before packing. The
generated packages contain binaries, metadata, and no install-time scripts.

## Recovering a partial release

`npm/scripts/publish.mjs` and `npm/scripts/wait-for-platforms.mjs` decide
what to do with each package by comparing the registry against the tarball
`npm/scripts/pack.sh` actually produced — by integrity first, and by
`gitHead` only under `--rebuild-of-tag`. A release run that fails partway is
therefore safe to re-run: anything the registry already holds with matching
bytes is skipped, not re-published, and anything that genuinely differs
throws instead of silently overwriting or silently succeeding.

Two situations follow from that:

- **The registry just hasn't caught up yet.** `wait-for-platforms.mjs`
  polls with a 45-minute budget before giving up; its timeout message names
  exactly which packages are still missing. If every publish step reported
  success and the packages are visible on the registry by the time you read
  this (`npm view <name>@<version>`), the release run itself did its job —
  re-run the failed job (`gh run rerun <run-id> --failed`). It will skip
  every package the registry already has and only publish, or wait for,
  what's left.
- **The build artifacts have expired.** CI keeps build artifacts for one day
  (`retention-days: 1`), so a rerun after that window has nothing to
  re-publish from. Dispatch the Release workflow with the `tag` input set to
  the release tag (e.g. `v0.14.0`) instead of leaving it empty. That checks
  out the tag, rebuilds it from source, and passes `--rebuild-of-tag` to
  publish and wait — so a package the original run already published is
  recognised by its `gitHead` rather than its bytes, since Rust release
  builds are not byte-reproducible across runs and a rebuild will not
  reproduce the original tarball's integrity.

A plain `workflow_dispatch` with no `tag` still only builds and packs; it
never publishes.
