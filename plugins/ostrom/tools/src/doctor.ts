import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { runDoctor } from "./lib/doctor.js";

const pluginRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const home = process.env.HOME ?? "";
const configDir = process.env.CLAUDE_CONFIG_DIR ?? resolve(home, ".claude");

process.stdout.write(
  runDoctor({
    pluginRoot,
    configDir,
    cwd: process.cwd(),
    home,
    env: process.env,
  }),
);
