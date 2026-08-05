import { readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import type { CheckResult } from "../lib/result.js";

function field(source: string, name: string): string {
  const match = new RegExp(`"${name}"\\s*:\\s*"([^"]*)"`).exec(source);
  return match?.[1] ?? "";
}

function isFile(path: string): boolean {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

export function checkPlugin(context: DoctorContext): CheckResult {
  const installedJson = join(context.configDir, "plugins", "installed_plugins.json");
  if (!isFile(installedJson)) {
    return {
      status: "FAIL",
      name: "plugin",
      detail: `no installed_plugins.json at ${installedJson}`,
      remedy: "/plugin install ostrom@ostrom",
    };
  }

  let source = "";
  try {
    source = readFileSync(installedJson, "utf8");
  } catch {
    // Match the old marker scanner: an unreadable file has no matching block.
  }
  const marker = source.indexOf('"ostrom@ostrom"');
  if (marker < 0) {
    return {
      status: "FAIL",
      name: "plugin",
      detail: "ostrom@ostrom not present in installed_plugins.json",
      remedy: "/plugin install ostrom@ostrom",
    };
  }

  const block = source.slice(marker);
  const installPath = field(block, "installPath");
  const recordedVersion = field(block, "version");
  const pluginJson = join(installPath, ".claude-plugin", "plugin.json");
  if (installPath && isFile(pluginJson)) {
    let version = "";
    try {
      version = field(readFileSync(pluginJson, "utf8"), "version");
    } catch {
      // existsSync and readFileSync can race; preserve the registry fallback.
    }
    return {
      status: "OK",
      name: "plugin",
      detail: `installed, version ${version}`,
      remedy: "",
    };
  }
  if (recordedVersion) {
    return {
      status: "OK",
      name: "plugin",
      detail: `installed, version ${recordedVersion} (installPath plugin.json not readable, using registry-recorded version)`,
      remedy: "",
    };
  }
  return {
    status: "FAIL",
    name: "plugin",
    detail: "ostrom@ostrom entry found but no version could be determined",
    remedy: "/plugin install ostrom@ostrom",
  };
}
