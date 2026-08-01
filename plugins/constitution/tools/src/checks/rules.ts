import { readFileSync, readdirSync, statSync } from "node:fs";
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

interface CachedPayload {
  marketplace: string;
  cacheVersion: string;
  rules: string | undefined;
  declaredVersion: string;
}

interface SemanticVersion {
  core: [number, number, number];
  prerelease: string[] | undefined;
}

function directories(path: string): string[] {
  try {
    return readdirSync(path, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => entry.name);
  } catch {
    return [];
  }
}

function parseSemanticVersion(source: string): SemanticVersion | undefined {
  const match =
    /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(
      source,
    );
  if (!match) return undefined;
  return {
    core: [Number(match[1]), Number(match[2]), Number(match[3])],
    prerelease: match[4]?.split("."),
  };
}

function comparePrerelease(
  left: string[] | undefined,
  right: string[] | undefined,
): number {
  if (!left && !right) return 0;
  if (!left) return 1;
  if (!right) return -1;

  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const leftPart = left[index];
    const rightPart = right[index];
    if (leftPart === undefined) return -1;
    if (rightPart === undefined) return 1;
    if (leftPart === rightPart) continue;

    const leftNumeric = /^\d+$/.test(leftPart);
    const rightNumeric = /^\d+$/.test(rightPart);
    if (leftNumeric && rightNumeric) return Number(leftPart) - Number(rightPart);
    if (leftNumeric) return -1;
    if (rightNumeric) return 1;
    return leftPart.localeCompare(rightPart);
  }
  return 0;
}

function compareVersions(left: string, right: string): number {
  const leftVersion = parseSemanticVersion(left);
  const rightVersion = parseSemanticVersion(right);
  if (!leftVersion && !rightVersion) {
    return left.localeCompare(right, undefined, { numeric: true });
  }
  if (!leftVersion) return -1;
  if (!rightVersion) return 1;

  for (let index = 0; index < leftVersion.core.length; index += 1) {
    const difference =
      leftVersion.core[index]! - rightVersion.core[index]!;
    if (difference !== 0) return difference;
  }
  return comparePrerelease(leftVersion.prerelease, rightVersion.prerelease);
}

function cachedPayloads(context: DoctorContext): CachedPayload[] {
  const cacheRoot = join(context.configDir, "plugins", "cache");
  return directories(cacheRoot).sort().flatMap((marketplace) => {
    const pluginCache = join(cacheRoot, marketplace, "constitution");
    const cacheVersion = directories(pluginCache).sort(compareVersions).at(-1);
    if (!cacheVersion) return [];

    const payloadRoot = join(pluginCache, cacheVersion);
    const json = read(join(payloadRoot, ".claude-plugin", "plugin.json"));
    return [
      {
        marketplace,
        cacheVersion,
        rules: read(join(payloadRoot, "rules", "frozen-rules.md")),
        declaredVersion: json === undefined ? "" : version(json),
      },
    ];
  });
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
  const runningRules = read(
    join(context.pluginRoot, "rules", "frozen-rules.md"),
  );
  const facts = [
    runningRules === undefined
      ? "running payload rule count unavailable"
      : `running payload has ${frozenRules(ruleCount(runningRules))}`,
  ];
  const caches = cachedPayloads(context);
  if (caches.length === 0) {
    facts.push("no constitution marketplace cache found");
  } else {
    for (const cache of caches) {
      const cacheLabel = `marketplace ${cache.marketplace} cache ${cache.cacheVersion}`;
      if (cache.rules === undefined || cache.declaredVersion === "") {
        facts.push(`${cacheLabel} payload or declared version is unreadable`);
      } else {
        facts.push(
          `${cacheLabel} has ${frozenRules(ruleCount(cache.rules))} and declares version ${cache.declaredVersion}`,
        );
      }
    }
  }

  const checkout = findOstromCheckout(context);
  if (!checkout) {
    facts.push("repo checkout not found");
    return {
      status: "OK",
      name: "rule-distribution",
      detail: facts.join("; "),
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
      detail: `${facts.join("; ")}; repo checkout found, but its constitution payload or version is unreadable`,
      remedy: "restore plugins/constitution/rules/frozen-rules.md and .claude-plugin/plugin.json in the checkout",
    };
  }

  const repoCount = ruleCount(repoRules);
  const repoVersion = version(repoJson);
  facts.push(
    `repo has ${frozenRules(repoCount)} and declares version ${repoVersion}`,
  );
  if (caches.length === 0) {
    return {
      status: "OK",
      name: "rule-distribution",
      detail: facts.join("; "),
      remedy: "",
    };
  }

  const unreadable = caches.filter(
    (cache) => cache.rules === undefined || cache.declaredVersion === "",
  );
  const missedBumps = caches.filter(
    (cache) =>
      cache.rules !== undefined &&
      cache.rules !== repoRules &&
      cache.declaredVersion === repoVersion,
  );
  const stale = caches.filter(
    (cache) =>
      cache.rules !== undefined &&
      (cache.rules !== repoRules || cache.declaredVersion !== repoVersion) &&
      cache.declaredVersion !== repoVersion,
  );

  if (unreadable.length === 0 && missedBumps.length === 0 && stale.length === 0) {
    return {
      status: "OK",
      name: "rule-distribution",
      detail: facts.join("; "),
      remedy: "",
    };
  }

  if (missedBumps.length > 0) {
    facts.push(
      `missed-version-bump signature in ${missedBumps.map((cache) => `marketplace ${cache.marketplace}`).join(", ")}: equal version ${repoVersion} with differing rule content`,
    );
  }
  if (stale.length > 0) {
    facts.push(
      `cache differs from repo in ${stale.map((cache) => `marketplace ${cache.marketplace}`).join(", ")}`,
    );
  }

  const remedies: string[] = [];
  if (missedBumps.length > 0) {
    remedies.push(
      "bump the constitution plugin version in plugins/constitution/.claude-plugin/plugin.json",
    );
  }
  const refresh = [...missedBumps, ...stale, ...unreadable];
  if (refresh.length > 0) {
    remedies.push(
      `${missedBumps.length > 0 ? "then refresh" : "refresh"} the ${refresh.map((cache) => cache.marketplace).join(", ")} marketplace ${refresh.length === 1 ? "cache" : "caches"}; if one stays stale, remove and re-add that marketplace`,
    );
  }

  return {
    status: "FAIL",
    name: "rule-distribution",
    detail: facts.join("; "),
    remedy: remedies.join("; "),
  };
}
