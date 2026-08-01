import { checkEnvironment } from "../checks/environment.js";
import { checkMarketplace } from "../checks/marketplace.js";
import { checkConfigParser } from "../checks/parser.js";
import { checkPlugin } from "../checks/plugin.js";
import {
  checkRuleDistribution,
  checkRulesLayers,
} from "../checks/rules.js";
import {
  checkProviderReachable,
  checkTouchDurability,
} from "../checks/touch.js";
import { resolveTouchConfig } from "./config.js";
import type { DoctorContext } from "./context.js";
import { formatResult } from "./result.js";

export interface DoctorOptions {
  pluginRoot: string;
  configDir: string;
  cwd: string;
  home: string;
  env?: NodeJS.ProcessEnv;
}

export function runDoctor(options: DoctorOptions): string {
  const env = options.env ?? process.env;
  const context: DoctorContext = {
    ...options,
    env,
    resolveConfig: () =>
      resolveTouchConfig(options.pluginRoot, options.configDir, options.cwd),
  };
  const results = [
    checkPlugin(context),
    checkMarketplace(context),
    checkRulesLayers(context),
    checkRuleDistribution(context),
    checkTouchDurability(context),
    checkProviderReachable(context),
    checkEnvironment(context),
    checkConfigParser(),
  ];
  return `${results.map(formatResult).join("\n")}\n`;
}
