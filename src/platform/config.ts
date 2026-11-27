import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";

import type { T3TargetConfig } from "../connectors/t3/index.ts";

export interface PlatformT3Host extends T3TargetConfig {
  laneSlug?: string;
}

export function defaultHostsConfigPath(env: NodeJS.ProcessEnv = process.env): string {
  const config = env["XDG_CONFIG_HOME"] ?? "";
  return join(isAbsolute(config) ? config : join(homedir(), ".config"), "ostrom", "platform-hosts.json");
}

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("expected an object in the platform hosts config");
  }
  return value as Record<string, unknown>;
}

export async function loadPlatformHosts(path = defaultHostsConfigPath()): Promise<PlatformT3Host[]> {
  const config = record(JSON.parse(await readFile(path, "utf8")));
  const hosts = config["hosts"];
  if (!Array.isArray(hosts)) throw new Error("platform hosts config requires a hosts array");
  const seen = new Set<string>();
  return hosts.map((entry: unknown) => {
    const { id, label, mcpUrl, originNamespace, laneSlug } = record(entry);
    if (typeof id !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(id) || seen.has(id)) {
      throw new Error("platform host IDs must be unique and contain 1–64 letters, numbers, dots, underscores or hyphens, starting with a letter or number");
    }
    if (typeof label !== "string" || label.trim() === "") throw new Error(`missing label for platform host ${id}`);
    const url = typeof mcpUrl === "string" && /^https?:\/\//i.test(mcpUrl) ? URL.parse(mcpUrl) : null;
    if (url === null || url.username || url.password || url.search || url.hash
      || (url.protocol !== "https:" && !(url.protocol === "http:" && ["127.0.0.1", "localhost"].includes(url.hostname)))) {
      throw new Error(`invalid mcpUrl for platform host ${id}`);
    }
    if (originNamespace !== undefined && (typeof originNamespace !== "string" || originNamespace.trim() === "")) {
      throw new Error(`invalid originNamespace for platform host ${id}`);
    }
    if (laneSlug !== undefined && (typeof laneSlug !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(laneSlug))) {
      throw new Error(`invalid laneSlug for platform host ${id}`);
    }
    seen.add(id);
    return {
      id, label, mcpUrl: url.href,
      ...(originNamespace === undefined ? {} : { originNamespace }),
      ...(laneSlug === undefined ? {} : { laneSlug }),
    };
  });
}
