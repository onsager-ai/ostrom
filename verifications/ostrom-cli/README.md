# Ostrom CLI Verification Definition

This page-free suite drives the real `ostrom` artifact through
`cli/invoke`. It needs no browser, network service, database, Docker, or
Postgres. OpenSSL is used only to create ephemeral test signing principals;
private-key bytes are never printed or bound into Duhem evidence.

Run it from the repository root against locally built binaries:

```sh
cargo build -p ostrom-cli
cargo build --manifest-path /home/marvin/projects/onsager-ai/duhem/Cargo.toml -p duhem-cli
/home/marvin/projects/onsager-ai/duhem/target/debug/duhem run \
  verifications/ostrom-cli/duhem.yml \
  --inputs ostrom_bin="$PWD/target/debug/ostrom"
```

The scratch root is `target/duhem-ostrom-cli`, which is gitignored.
Every check recreates only its own named directory, so reruns are idempotent.
The version check intentionally requires an exact Git tag; it is a release
artifact check and is inconclusive as a development-branch claim.

This VD is deliberately not wired into the release workflow. Whether it gates
tagging remains the unresolved principal decision recorded in ostrom#469.
