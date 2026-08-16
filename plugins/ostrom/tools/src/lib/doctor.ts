import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { checkEnvironment } from "../checks/environment.js";
import { checkMarketplace } from "../checks/marketplace.js";
import { checkConfigParser } from "../checks/parser.js";
import { checkPlugin } from "../checks/plugin.js";
import { checkPluginCacheDrift } from "../checks/plugin-cache-drift.js";
import { checkRulesLayers } from "../checks/rules.js";
import { checkTraceLease } from "../checks/trace-lease.js";
import { checkWorkOrders } from "../checks/work-orders.js";
import { checkDispatchSourceRoots } from "../checks/dispatch-source-roots.js";
import {
  checkProviderReachable,
  checkTouchDurability,
} from "../checks/touch.js";
import {
  checkBuilderPass,
  checkGatekeeperPass,
} from "../checks/builder-pass.js";
import { checkPublish } from "../checks/publish.js";
import { resolveTouchConfig } from "./config.js";
import type { DoctorContext, TraceFile } from "./context.js";
import { formatResult, type CheckResult } from "./result.js";

export interface DoctorOptions {
  pluginRoot: string;
  configDir: string;
  cwd: string;
  home: string;
  env?: NodeJS.ProcessEnv;
}

// One doctor run may consult sprint.jsonl from more than one check. The file
// only grows, so read it at most once per run and hand every check the same
// cached result rather than each doing its own readFileSync.
function createTraceReader(configDir: string): () => TraceFile {
  let cached: TraceFile | undefined;
  return () => {
    if (cached) return cached;
    const path = join(configDir, "ostrom", "sprint.jsonl");
    if (!existsSync(path)) {
      cached = { exists: false };
      return cached;
    }
    try {
      cached = { exists: true, content: readFileSync(path, "utf8") };
    } catch (error) {
      cached = { exists: true, error };
    }
    return cached;
  };
}

export const DOCTOR_CHECK_NAMES = [
  "plugin",
  "marketplace",
  "plugin-cache-drift",
  "rules-layers",
  "touch-durability",
  "provider-reachable",
  "dispatch-source-roots",
  "trace-lease",
  "work-orders",
  "builder-pass",
  "gatekeeper-pass",
  "publish",
  "environment",
  "config-parser",
] as const;

export type DoctorCheckName = (typeof DOCTOR_CHECK_NAMES)[number];

function checkRunners(
  context: DoctorContext,
): Record<DoctorCheckName, () => CheckResult> {
  return {
    plugin: () => checkPlugin(context),
    marketplace: () => checkMarketplace(context),
    "plugin-cache-drift": () => checkPluginCacheDrift(context),
    "rules-layers": () => checkRulesLayers(context),
    "touch-durability": () => checkTouchDurability(context),
    "provider-reachable": () => checkProviderReachable(context),
    "dispatch-source-roots": () => checkDispatchSourceRoots(context),
    "trace-lease": () => checkTraceLease(context),
    "work-orders": () => checkWorkOrders(context),
    "builder-pass": () => checkBuilderPass(context),
    "gatekeeper-pass": () => checkGatekeeperPass(context),
    publish: () => checkPublish(context),
    environment: () => checkEnvironment(context),
    "config-parser": () => checkConfigParser(),
  };
}

function createContext(options: DoctorOptions): DoctorContext {
  const env = options.env ?? process.env;
  return {
    ...options,
    env,
    resolveConfig: () =>
      resolveTouchConfig(options.pluginRoot, options.configDir, options.cwd),
    readTrace: createTraceReader(options.configDir),
  };
}

export function runDoctor(options: DoctorOptions): string {
  const runners = checkRunners(createContext(options));
  const results = DOCTOR_CHECK_NAMES.map((name) => runners[name]());
  return `${results.map(formatResult).join("\n")}\n`;
}

export function runDoctorCheck(
  options: DoctorOptions,
  name: string,
): string {
  if (!(DOCTOR_CHECK_NAMES as readonly string[]).includes(name)) {
    throw new Error(`unknown doctor check: ${name}`);
  }
  const exactName = name as DoctorCheckName;
  const result = checkRunners(createContext(options))[exactName]();
  return `${formatResult(result)}\n`;
}
