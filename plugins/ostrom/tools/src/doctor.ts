import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { runDoctor, runDoctorCheck } from "./lib/doctor.js";

const pluginRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const home = process.env.HOME ?? "";
const configDir = process.env.CLAUDE_CONFIG_DIR ?? resolve(home, ".claude");

const options = {
  pluginRoot,
  configDir,
  cwd: process.cwd(),
  home,
  env: process.env,
};
const args = process.argv.slice(2);
if (args.length === 0) {
  process.stdout.write(runDoctor(options));
} else if (args.length === 2 && args[0] === "--check" && args[1]) {
  try {
    process.stdout.write(runDoctorCheck(options, args[1]));
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : "doctor check failed"}\n`);
    process.exitCode = 2;
  }
} else {
  process.stderr.write("usage: doctor.js [--check <exact-name>]\n");
  process.exitCode = 2;
}
