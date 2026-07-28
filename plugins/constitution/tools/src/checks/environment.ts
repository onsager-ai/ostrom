import type { DoctorContext } from "../lib/context.js";
import type { CheckResult } from "../lib/result.js";
import { computeRulesLayers } from "./rules.js";

export function checkEnvironment(context: DoctorContext): CheckResult {
  if (!context.env.CLAUDE_CODE_REMOTE) {
    return {
      status: "OK",
      name: "environment",
      detail: "local",
      remedy: "",
    };
  }
  if (computeRulesLayers(context).hasUser) {
    return {
      status: "OK",
      name: "environment",
      detail: "cloud, user rules layer resolved",
      remedy: "",
    };
  }
  return {
    status: "WARN",
    name: "environment",
    detail: "cloud session, no user rules layer resolved (private layer absent)",
    remedy:
      "provide the private layer's credentials/config for this environment",
  };
}
