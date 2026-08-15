import { createHash } from "node:crypto";
import {
  lstatSync,
  readFileSync,
  readdirSync,
  readlinkSync,
} from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import { git } from "../lib/process.js";
import type { CheckResult } from "../lib/result.js";
import { inspectMarketplace } from "./marketplace.js";
import {
  pluginJsonField,
  resolvePluginInstallation,
} from "./plugin.js";

const shippedDirectories = ["skills", "scripts", "hooks", "rules"] as const;
const marketplacePluginRoot = "plugins/ostrom";

interface Fingerprint {
  mode: string;
  object: string;
}

function blobHash(contents: Buffer): string {
  return createHash("sha1")
    .update(`blob ${contents.byteLength}\0`)
    .update(contents)
    .digest("hex");
}

function installedFiles(pluginRoot: string): Map<string, Fingerprint> {
  const files = new Map<string, Fingerprint>();

  function walk(path: string, relativePath: string): void {
    const stat = lstatSync(path);
    if (stat.isDirectory()) {
      if (relativePath.split("/").includes("node_modules")) return;
      for (const entry of readdirSync(path, { withFileTypes: true })) {
        walk(join(path, entry.name), `${relativePath}/${entry.name}`);
      }
      return;
    }

    if (!stat.isFile() && !stat.isSymbolicLink()) return;
    const contents = stat.isSymbolicLink()
      ? Buffer.from(readlinkSync(path))
      : readFileSync(path);
    const mode = stat.isSymbolicLink()
      ? "120000"
      : stat.mode & 0o111
        ? "100755"
        : "100644";
    files.set(relativePath, { mode, object: blobHash(contents) });
  }

  for (const directory of shippedDirectories) {
    const path = join(pluginRoot, directory);
    try {
      walk(path, directory);
    } catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      if (code !== "ENOENT") throw error;
    }
  }
  return files;
}

function marketplaceFiles(
  marketplaceDir: string,
): Map<string, Fingerprint> | undefined {
  const result = git(marketplaceDir, [
    "ls-tree",
    "-r",
    "-z",
    "HEAD",
    "--",
    ...shippedDirectories.map(
      (directory) => `${marketplacePluginRoot}/${directory}`,
    ),
  ]);
  if (result.status !== 0) return undefined;

  const files = new Map<string, Fingerprint>();
  for (const record of result.stdout.split("\0")) {
    if (!record) continue;
    const match = /^(\d+) blob ([0-9a-f]+)\t(.+)$/.exec(record);
    if (!match?.[1] || !match[2] || !match[3]) continue;
    const relativePath = match[3].slice(`${marketplacePluginRoot}/`.length);
    if (relativePath.split("/").includes("node_modules")) continue;
    files.set(relativePath, { mode: match[1], object: match[2] });
  }
  return files;
}

function marketplaceVersion(marketplaceDir: string): string {
  const result = git(marketplaceDir, [
    "show",
    `HEAD:${marketplacePluginRoot}/.claude-plugin/plugin.json`,
  ]);
  if (result.status !== 0) return "";
  return pluginJsonField(result.stdout, "version");
}

function differences(
  installed: Map<string, Fingerprint>,
  marketplace: Map<string, Fingerprint>,
): string[] {
  const paths = [...new Set([...installed.keys(), ...marketplace.keys()])].sort();
  const result: string[] = [];
  for (const path of paths) {
    const installedFile = installed.get(path);
    const marketplaceFile = marketplace.get(path);
    if (!installedFile) {
      result.push(`missing from installed cache: ${path}`);
    } else if (!marketplaceFile) {
      result.push(`only in installed cache: ${path}`);
    } else if (installedFile.object !== marketplaceFile.object) {
      result.push(`content differs: ${path}`);
    } else if (installedFile.mode !== marketplaceFile.mode) {
      result.push(`mode differs: ${path}`);
    }
  }
  return result;
}

function summarize(items: string[]): string {
  const shown = items.slice(0, 8);
  const remaining = items.length - shown.length;
  return remaining > 0
    ? `${shown.join("; ")}; plus ${remaining} more`
    : shown.join("; ");
}

export function checkPluginCacheDrift(context: DoctorContext): CheckResult {
  const resolution = resolvePluginInstallation(context);
  if (resolution.kind !== "found") {
    const detail =
      resolution.kind === "missing-registry"
        ? `installed plugin registry missing at ${resolution.path}`
        : "ostrom@ostrom not present in installed plugin registry";
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `cannot compare shipped files: ${detail}`,
      remedy: "/plugin install ostrom@ostrom",
    };
  }

  const marketplace = inspectMarketplace(context);
  if (!marketplace.cloneAvailable || !marketplace.fetchAvailable) {
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `cannot compare shipped files: ${marketplace.result.detail}`,
      remedy: marketplace.result.remedy,
    };
  }

  const installedVersion = resolution.installation.registryVersion;
  const checkoutVersion = marketplaceVersion(marketplace.directory);
  if (!installedVersion || !checkoutVersion) {
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: "cannot compare shipped files: installed or marketplace version is unreadable",
      remedy: "reinstall ostrom@ostrom, then restart the session",
    };
  }
  if (installedVersion !== checkoutVersion) {
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `versions differ: installed cache ${installedVersion}, marketplace checkout ${checkoutVersion}`,
      remedy: "update and reinstall ostrom@ostrom, then restart the session",
    };
  }

  let installed: Map<string, Fingerprint>;
  try {
    installed = installedFiles(resolution.installation.installPath);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `cannot read installed shipped files: ${message}`,
      remedy: "reinstall ostrom@ostrom, then restart the session",
    };
  }
  const checkout = marketplaceFiles(marketplace.directory);
  if (!checkout) {
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: "cannot read shipped files from the marketplace checkout's current commit",
      remedy: "/plugin marketplace update ostrom",
    };
  }

  const drift = differences(installed, checkout);
  if (drift.length > 0) {
    return {
      status: "FAIL",
      name: "plugin-cache-drift",
      detail: `version ${installedVersion} agrees but shipped files drift: ${summarize(drift)}`,
      remedy: "update and reinstall ostrom@ostrom, then restart the session",
    };
  }
  return {
    status: "OK",
    name: "plugin-cache-drift",
    detail: `version ${installedVersion} and shipped files agree with the marketplace checkout`,
    remedy: "",
  };
}
