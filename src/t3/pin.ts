// The T3 Code version Ostrom is built and tested against (AGENTS.md rule 5).
// To re-pin: run scripts/vendor-t3code.sh <commit>, update this record, and let CI run
// src/t3/agreement.test.ts against the newly vendored upstream schemas.

export const T3_PIN = {
  /** What `t3 --version` prints on the operator's machine. */
  cliVersion: "0.1.7",
  repository: "pingdotgg/t3code",
  /** The upstream commit whose schemas are vendored under vendor/t3code. */
  commit: "d0f1f7200dce1229f75314b3d9eb1d7ac3abb607",
} as const;
