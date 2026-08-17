# Platform packages

The package directories below mirror the entries in
`npm/publish.config.json`. Release staging writes each generated manifest and
CI-built binary under `target/npm-packages/<directory>` before packing. The
generated packages contain binaries, metadata, and no install-time scripts.
