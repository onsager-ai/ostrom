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

function versionAt(pluginRoot: string): string {
  if (!pluginRoot) return "";
  const pluginJson = join(pluginRoot, ".claude-plugin", "plugin.json");
  if (!isFile(pluginJson)) return "";
  try {
    return field(readFileSync(pluginJson, "utf8"), "version");
  } catch {
    return "";
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
  const loadedVersion = versionAt(context.pluginRoot);
  const installPathVersion = versionAt(installPath);
  const registryVersion = installPathVersion || recordedVersion;

  if (loadedVersion && registryVersion) {
    const matchesRegistry = loadedVersion === registryVersion;
    return {
      status: matchesRegistry ? "OK" : "WARN",
      name: "plugin",
      detail: matchesRegistry
        ? `installed, loaded version ${loadedVersion}`
        : `installed, loaded version ${loadedVersion}, registry version ${registryVersion}`,
      remedy: matchesRegistry
        ? ""
        : "restart the session to reconcile the loaded plugin with the registry",
    };
  }
  if (!loadedVersion && registryVersion) {
    const registrySource = installPathVersion
      ? "registry version"
      : "registry-recorded version";
    return {
      status: "OK",
      name: "plugin",
      detail: `installed, version ${registryVersion} (loaded plugin.json not readable, using ${registrySource})`,
      remedy: "",
    };
  }
  if (loadedVersion) {
    return {
      status: "WARN",
      name: "plugin",
      detail: `installed, loaded version ${loadedVersion}, registry version not readable`,
      remedy: "restart the session to reconcile the loaded plugin with the registry",
    };
  }
  return {
    status: "FAIL",
    name: "plugin",
    detail: "ostrom@ostrom entry found but no version could be determined",
    remedy: "/plugin install ostrom@ostrom",
  };
}
