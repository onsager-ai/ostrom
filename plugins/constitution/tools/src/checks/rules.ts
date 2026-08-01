import { readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import { git, run } from "../lib/process.js";
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

function read(path: string): string | undefined {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return undefined;
  }
}

function version(source: string): string {
  return /"version"\s*:\s*"([^"]+)"/.exec(source)?.[1] ?? "";
}

function ruleCount(source: string): number {
  return source.match(/^## /gm)?.length ?? 0;
}

function frozenRules(count: number): string {
  return `${count} frozen ${count === 1 ? "rule" : "rules"}`;
}

function findOstromCheckout(context: DoctorContext): string | undefined {
  for (const candidate of [context.cwd, context.pluginRoot]) {
    const topLevel = git(candidate, ["rev-parse", "--show-toplevel"]);
    if (topLevel.status !== 0) continue;

    const root = topLevel.stdout.trim();
    if (
      isFile(join(root, "plugins", "constitution", "rules", "frozen-rules.md")) &&
      isFile(
        join(root, "plugins", "constitution", ".claude-plugin", "plugin.json"),
      )
    ) {
      return root;
    }
  }
  return undefined;
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
      remedy: "reinstall the constitution plugin",
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

export function checkRuleDistribution(context: DoctorContext): CheckResult {
  const installedRulesPath = join(
    context.pluginRoot,
    "rules",
    "frozen-rules.md",
  );
  const installedJsonPath = join(
    context.pluginRoot,
    ".claude-plugin",
    "plugin.json",
  );
  const installedRules = read(installedRulesPath);
  const installedJson = read(installedJsonPath);

  if (installedRules === undefined) {
    return {
      status: "FAIL",
      name: "rule-distribution",
      detail: `installed frozen-rules.md not readable at ${installedRulesPath}`,
      remedy: "reinstall the constitution plugin",
    };
  }

  const installedCount = ruleCount(installedRules);
  if (installedCount === 0) {
    return {
      status: "FAIL",
      name: "rule-distribution",
      detail: "installed payload has 0 frozen rules",
      remedy: "reinstall the constitution plugin",
    };
  }

  if (installedJson === undefined || version(installedJson) === "") {
    return {
      status: "FAIL",
      name: "rule-distribution",
      detail: `installed payload has ${frozenRules(installedCount)}, but its plugin version is unreadable`,
      remedy: "reinstall the constitution plugin",
    };
  }

  const checkout = findOstromCheckout(context);
  if (!checkout) {
    return {
      status: "OK",
      name: "rule-distribution",
      detail: `installed payload has ${frozenRules(installedCount)}`,
      remedy: "",
    };
  }

  const repoRules = read(
    join(checkout, "plugins", "constitution", "rules", "frozen-rules.md"),
  );
  const repoJson = read(
    join(checkout, "plugins", "constitution", ".claude-plugin", "plugin.json"),
  );
  if (repoRules === undefined || repoJson === undefined || version(repoJson) === "") {
    return {
      status: "FAIL",
      name: "rule-distribution",
      detail: "ostrom checkout found, but its constitution payload or version is unreadable",
      remedy: "restore plugins/constitution/rules/frozen-rules.md and .claude-plugin/plugin.json in the checkout",
    };
  }

  const repoCount = ruleCount(repoRules);
  const installedVersion = version(installedJson);
  const repoVersion = version(repoJson);
  const counts = `installed payload has ${frozenRules(installedCount)}; repo has ${frozenRules(repoCount)}`;

  if (installedRules === repoRules && installedVersion === repoVersion) {
    return {
      status: "OK",
      name: "rule-distribution",
      detail: `${counts}; both declare version ${installedVersion}`,
      remedy: "",
    };
  }

  if (installedRules !== repoRules && installedVersion === repoVersion) {
    return {
      status: "FAIL",
      name: "rule-distribution",
      detail: `${counts}; both declare version ${installedVersion}, but rule content differs — plugin payload changed without a version bump (silent distribution bug)`,
      remedy:
        "re-add the ostrom marketplace to refresh the installed payload, or bump the constitution plugin version in the repo",
    };
  }

  return {
    status: "FAIL",
    name: "rule-distribution",
    detail: `${counts}; installed version ${installedVersion}, repo version ${repoVersion}`,
    remedy:
      "/plugin marketplace update ostrom; if the installed payload stays stale, remove and re-add the marketplace",
  };
}
