import type { TouchConfig } from "./config.js";

export interface DoctorContext {
  pluginRoot: string;
  configDir: string;
  cwd: string;
  home: string;
  env: NodeJS.ProcessEnv;
  resolveConfig(): TouchConfig;
}
