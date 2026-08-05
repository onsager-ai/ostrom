import { statSync } from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import { run } from "../lib/process.js";
import type { CheckResult } from "../lib/result.js";

export interface RuleLayers {
  hookMissing: boolean;
  hasUser: boolean;
  hasRepo: boolean;
}

function isFile(path: string): boolean {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

export function computeRulesLayers(context: DoctorContext): RuleLayers {
  const hook = join(context.pluginRoot, "hooks", "inject-constitution.sh");
  if (!isFile(hook)) {
    return { hookMissing: true, hasUser: false, hasRepo: false };
  }
  const result = run("bash", [hook], {
    cwd: context.cwd,
    env: {
      ...context.env,
      CLAUDE_PLUGIN_ROOT: context.pluginRoot,
      CLAUDE_CONFIG_DIR: context.configDir,
    },
  });
  return {
    hookMissing: false,
    hasUser: result.stdout.includes("<!-- constitution layer: user "),
    hasRepo: result.stdout.includes("<!-- constitution layer: repo "),
  };
}

export function checkRulesLayers(context: DoctorContext): CheckResult {
  const layers = computeRulesLayers(context);
  if (layers.hookMissing) {
    return {
      status: "FAIL",
      name: "rules-layers",
      detail: `hook not found at ${join(context.pluginRoot, "hooks", "inject-constitution.sh")}`,
      remedy: "reinstall the ostrom plugin",
    };
  }

  const fired = ["shipped"];
  if (layers.hasUser) fired.push("user");
  if (layers.hasRepo) fired.push("repo");
  const summary = fired.length === 1 ? "shipped only" : fired.join(" + ");
  const notes: string[] = [];
  if (
    isFile(join(context.configDir, "ostrom", "rules.md")) &&
    !layers.hasUser
  ) {
    notes.push("user layer present but carries no rules yet (by design)");
  }
  if (isFile(join(context.cwd, ".ostrom", "rules.md")) && !layers.hasRepo) {
    notes.push("repo layer present but carries no rules yet (by design)");
  }

  return {
    status: "OK",
    name: "rules-layers",
    detail: notes.length > 0 ? `${summary} (${notes.join("; ")})` : summary,
    remedy: "",
  };
}
