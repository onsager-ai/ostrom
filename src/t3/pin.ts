// The T3 Code version Ostrom is built and tested against (AGENTS.md rule 5).
// To re-pin: run scripts/vendor-t3code.sh <release-tag>, update this record, and let CI run
// src/t3/agreement.test.ts against the newly vendored upstream schemas.

export const T3_PIN = {
  /** What `t3 --version` prints on the operator's machine. */
  cliVersion: "0.1.7",
  repository: "pingdotgg/t3code",
  /** The release tag that binary was built from. */
  releaseTag: "v0.1.7",
  /** The commit that tag points to; its schemas are vendored under vendor/t3code. */
  commit: "322f6544dfad0a3c8a0c8cbecf2c7f7874b14268",
} as const;
