import { readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import type { CheckResult } from "../lib/result.js";

export function pluginJsonField(source: string, name: string): string {
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

export function pluginVersionAt(pluginRoot: string): string {
  if (!pluginRoot) return "";
  const pluginJson = join(pluginRoot, ".claude-plugin", "plugin.json");
  if (!isFile(pluginJson)) return "";
  try {
    return pluginJsonField(readFileSync(pluginJson, "utf8"), "version");
  } catch {
    return "";
  }
}

export interface PluginInstallation {
  installPath: string;
  recordedVersion: string;
  loadedVersion: string;
  installPathVersion: string;
  registryVersion: string;
}

export type PluginResolution =
  | { kind: "missing-registry"; path: string }
  | { kind: "plugin-absent" }
  | { kind: "found"; installation: PluginInstallation };

export function resolvePluginInstallation(
  context: DoctorContext,
): PluginResolution {
  const installedJson = join(context.configDir, "plugins", "installed_plugins.json");
  if (!isFile(installedJson)) {
    return { kind: "missing-registry", path: installedJson };
  }

  let source = "";
  try {
    source = readFileSync(installedJson, "utf8");
  } catch {
    // Match the old marker scanner: an unreadable file has no matching block.
  }
  const marker = source.indexOf('"ostrom@ostrom"');
  if (marker < 0) {
    return { kind: "plugin-absent" };
  }

  const block = source.slice(marker);
  const installPath = pluginJsonField(block, "installPath");
  const recordedVersion = pluginJsonField(block, "version");
  const loadedVersion = pluginVersionAt(context.pluginRoot);
  const installPathVersion = pluginVersionAt(installPath);
  return {
    kind: "found",
    installation: {
      installPath,
      recordedVersion,
      loadedVersion,
      installPathVersion,
      registryVersion: installPathVersion || recordedVersion,
    },
  };
}

export function checkPlugin(context: DoctorContext): CheckResult {
  const resolution = resolvePluginInstallation(context);
  if (resolution.kind === "missing-registry") {
    return {
      status: "FAIL",
      name: "plugin",
      detail: `no installed_plugins.json at ${resolution.path}`,
      remedy: "/plugin install ostrom@ostrom",
    };
  }
  if (resolution.kind === "plugin-absent") {
    return {
      status: "FAIL",
      name: "plugin",
      detail: "ostrom@ostrom not present in installed_plugins.json",
      remedy: "/plugin install ostrom@ostrom",
    };
  }

  const {
    installPathVersion,
    loadedVersion,
    registryVersion,
  } = resolution.installation;

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
