import { resolveMandateSearchRoots } from "../lib/config.js";
import type { DoctorContext } from "../lib/context.js";
import type { CheckResult } from "../lib/result.js";

export function checkDispatchSourceRoots(
  context: DoctorContext,
): CheckResult {
  const searchRoots = resolveMandateSearchRoots(
    context.pluginRoot,
    context.configDir,
    context.cwd,
  );

  if (searchRoots.length === 0) {
    return {
      status: "FAIL",
      name: "dispatch-source-roots",
      detail:
        "search_roots is empty; dispatch cannot resolve source repositories",
      remedy:
        "configure search_roots with a parent directory containing the roster checkouts",
    };
  }

  const noun = searchRoots.length === 1 ? "root" : "roots";
  return {
    status: "OK",
    name: "dispatch-source-roots",
    detail: `${searchRoots.length} search ${noun} configured for dispatch`,
    remedy: "",
  };
}
