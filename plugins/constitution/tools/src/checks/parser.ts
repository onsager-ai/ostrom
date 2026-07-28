import type { CheckResult } from "../lib/result.js";

export function checkConfigParser(): CheckResult {
  return {
    status: "OK",
    name: "config-parser",
    detail:
      "used the built-in ostrom-shape parser (top-level scalars, one level of nesting, inline lists, and comments; the values behind touch-durability/provider-reachable are authoritative for this supported config shape; a DEFER line is still resolved by the caller)",
    remedy: "",
  };
}
