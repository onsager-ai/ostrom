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
import { formatResult } from "./result.js";

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

export function runDoctor(options: DoctorOptions): string {
  const env = options.env ?? process.env;
  const context: DoctorContext = {
    ...options,
    env,
    resolveConfig: () =>
      resolveTouchConfig(options.pluginRoot, options.configDir, options.cwd),
    readTrace: createTraceReader(options.configDir),
  };
  const results = [
    checkPlugin(context),
    checkMarketplace(context),
    checkPluginCacheDrift(context),
    checkRulesLayers(context),
    checkTouchDurability(context),
    checkProviderReachable(context),
    checkDispatchSourceRoots(context),
    checkTraceLease(context),
    checkWorkOrders(context),
    checkBuilderPass(context),
    checkGatekeeperPass(context),
    checkPublish(context),
    checkEnvironment(context),
    checkConfigParser(),
  ];
  return `${results.map(formatResult).join("\n")}\n`;
}
