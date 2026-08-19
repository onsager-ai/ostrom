// src/doctor.ts
import { dirname as dirname3, resolve as resolve2 } from "node:path";
import { fileURLToPath } from "node:url";

// src/lib/doctor.ts
import { existsSync as existsSync6, readFileSync as readFileSync8 } from "node:fs";
import { join as join10 } from "node:path";

// src/checks/rules.ts
import { statSync } from "node:fs";
import { join } from "node:path";

// src/lib/process.ts
import { spawnSync } from "node:child_process";
function run(command, args2, options2 = {}) {
  const result = spawnSync(command, args2, {
    cwd: options2.cwd,
    env: options2.env,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"]
  });
  return {
    status: result.status ?? 1,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? result.error?.message ?? ""
  };
}
function git(cwd, args2) {
  return run("git", ["-C", cwd, ...args2]);
}

// src/checks/rules.ts
function isFile(path) {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}
function computeRulesLayers(context) {
  const hook = join(context.pluginRoot, "hooks", "inject-constitution.sh");
  if (!isFile(hook)) {
    return { hookMissing: true, hasUser: false, hasRepo: false };
  }
  const result = run("bash", [hook], {
    cwd: context.cwd,
    env: {
      ...context.env,
      CLAUDE_PLUGIN_ROOT: context.pluginRoot,
      CLAUDE_CONFIG_DIR: context.configDir
    }
  });
  return {
    hookMissing: false,
    hasUser: result.stdout.includes("<!-- constitution layer: user "),
    hasRepo: result.stdout.includes("<!-- constitution layer: repo ")
  };
}
function checkRulesLayers(context) {
  const layers = computeRulesLayers(context);
  if (layers.hookMissing) {
    return {
      status: "FAIL",
      name: "rules-layers",
      detail: `hook not found at ${join(context.pluginRoot, "hooks", "inject-constitution.sh")}`,
      remedy: "reinstall the ostrom plugin"
    };
  }
  const fired = ["shipped"];
  if (layers.hasUser) fired.push("user");
  if (layers.hasRepo) fired.push("repo");
  const summary = fired.length === 1 ? "shipped only" : fired.join(" + ");
  const notes = [];
  if (isFile(join(context.configDir, "ostrom", "rules.md")) && !layers.hasUser) {
    notes.push("user layer present but carries no rules yet (by design)");
  }
  if (isFile(join(context.cwd, ".ostrom", "rules.md")) && !layers.hasRepo) {
    notes.push("repo layer present but carries no rules yet (by design)");
  }
  return {
    status: "OK",
    name: "rules-layers",
    detail: notes.length > 0 ? `${summary} (${notes.join("; ")})` : summary,
    remedy: ""
  };
}

// src/checks/environment.ts
function checkEnvironment(context) {
  if (!context.env.CLAUDE_CODE_REMOTE) {
    return {
      status: "OK",
      name: "environment",
      detail: "local",
      remedy: ""
    };
  }
  if (computeRulesLayers(context).hasUser) {
    return {
      status: "OK",
      name: "environment",
      detail: "cloud, user rules layer resolved",
      remedy: ""
    };
  }
  return {
    status: "WARN",
    name: "environment",
    detail: "cloud session, no user rules layer resolved (private layer absent)",
    remedy: "provide the private layer's credentials/config for this environment"
  };
}

// src/checks/marketplace.ts
import { readFileSync, statSync as statSync2 } from "node:fs";
import { join as join2 } from "node:path";
var inspectionCache = /* @__PURE__ */ new WeakMap();
function inspectMarketplace(context) {
  const cached = inspectionCache.get(context);
  if (cached) return cached;
  const knownJson = join2(context.configDir, "plugins", "known_marketplaces.json");
  const marketplaceDir = join2(
    context.configDir,
    "plugins",
    "marketplaces",
    "ostrom"
  );
  let knownSource = "";
  try {
    knownSource = readFileSync(knownJson, "utf8");
  } catch {
  }
  let knownIsFile = false;
  let cloneIsDirectory = false;
  try {
    knownIsFile = statSync2(knownJson).isFile();
  } catch {
  }
  try {
    cloneIsDirectory = statSync2(join2(marketplaceDir, ".git")).isDirectory();
  } catch {
  }
  if (!knownIsFile || !/"ostrom"\s*:/.test(knownSource)) {
    const inspection2 = {
      directory: marketplaceDir,
      cloneAvailable: false,
      fetchAvailable: false,
      result: {
        status: "FAIL",
        name: "marketplace",
        detail: "ostrom not registered in known_marketplaces.json",
        remedy: "/plugin marketplace add onsager-ai/ostrom"
      }
    };
    inspectionCache.set(context, inspection2);
    return inspection2;
  }
  if (!cloneIsDirectory) {
    const inspection2 = {
      directory: marketplaceDir,
      cloneAvailable: false,
      fetchAvailable: false,
      result: {
        status: "FAIL",
        name: "marketplace",
        detail: `registered, but no cached clone at ${marketplaceDir}`,
        remedy: "/plugin marketplace add onsager-ai/ostrom"
      }
    };
    inspectionCache.set(context, inspection2);
    return inspection2;
  }
  const fetch = git(marketplaceDir, ["fetch", "origin", "main"]);
  if (fetch.status !== 0) {
    const firstLine2 = `${fetch.stdout}${fetch.stderr}`.split(/\r?\n/, 1)[0] ?? "";
    const inspection2 = {
      directory: marketplaceDir,
      cloneAvailable: true,
      fetchAvailable: false,
      result: {
        status: "WARN",
        name: "marketplace",
        detail: `cannot verify freshness, git fetch failed (offline?): ${firstLine2}`,
        remedy: ""
      }
    };
    inspectionCache.set(context, inspection2);
    return inspection2;
  }
  if (git(marketplaceDir, ["rev-parse", "--verify", "origin/main"]).status !== 0) {
    const inspection2 = {
      directory: marketplaceDir,
      cloneAvailable: true,
      fetchAvailable: true,
      result: {
        status: "WARN",
        name: "marketplace",
        detail: "fetched, but origin/main not found (default branch may differ)",
        remedy: ""
      }
    };
    inspectionCache.set(context, inspection2);
    return inspection2;
  }
  if (git(marketplaceDir, [
    "merge-base",
    "--is-ancestor",
    "HEAD",
    "origin/main"
  ]).status === 0) {
    const inspection2 = {
      directory: marketplaceDir,
      cloneAvailable: true,
      fetchAvailable: true,
      result: {
        status: "OK",
        name: "marketplace",
        detail: "cached clone can fast-forward to origin/main",
        remedy: ""
      }
    };
    inspectionCache.set(context, inspection2);
    return inspection2;
  }
  if (git(marketplaceDir, ["merge-base", "HEAD", "origin/main"]).status === 0) {
    const inspection2 = {
      directory: marketplaceDir,
      cloneAvailable: true,
      fetchAvailable: true,
      result: {
        status: "WARN",
        name: "marketplace",
        detail: "cached clone has diverged from origin/main (shared history, not fast-forwardable)",
        remedy: "/plugin marketplace update ostrom"
      }
    };
    inspectionCache.set(context, inspection2);
    return inspection2;
  }
  const inspection = {
    directory: marketplaceDir,
    cloneAvailable: true,
    fetchAvailable: true,
    result: {
      status: "FAIL",
      name: "marketplace",
      detail: "cached clone and origin/main have unrelated histories (marketplace was republished from a fresh history)",
      remedy: "/plugin marketplace remove ostrom && /plugin marketplace add onsager-ai/ostrom"
    }
  };
  inspectionCache.set(context, inspection);
  return inspection;
}
function checkMarketplace(context) {
  return inspectMarketplace(context).result;
}

// src/checks/parser.ts
function checkConfigParser() {
  return {
    status: "OK",
    name: "config-parser",
    detail: "used the built-in ostrom-shape parser (top-level scalars, one level of nesting, inline lists, and comments; the values behind touch-durability/provider-reachable are authoritative for this supported config shape; a DEFER line is still resolved by the caller)",
    remedy: ""
  };
}

// src/checks/plugin.ts
import { readFileSync as readFileSync2, statSync as statSync3 } from "node:fs";
import { join as join3 } from "node:path";
function pluginJsonField(source, name) {
  const match = new RegExp(`"${name}"\\s*:\\s*"([^"]*)"`).exec(source);
  return match?.[1] ?? "";
}
function isFile2(path) {
  try {
    return statSync3(path).isFile();
  } catch {
    return false;
  }
}
function pluginVersionAt(pluginRoot2) {
  if (!pluginRoot2) return "";
  const pluginJson = join3(pluginRoot2, ".claude-plugin", "plugin.json");
  if (!isFile2(pluginJson)) return "";
  try {
    return pluginJsonField(readFileSync2(pluginJson, "utf8"), "version");
  } catch {
    return "";
  }
}
function resolvePluginInstallation(context) {
  const installedJson = join3(context.configDir, "plugins", "installed_plugins.json");
  if (!isFile2(installedJson)) {
    return { kind: "missing-registry", path: installedJson };
  }
  let source = "";
  try {
    source = readFileSync2(installedJson, "utf8");
  } catch {
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
      registryVersion: installPathVersion || recordedVersion
    }
  };
}
function checkPlugin(context) {
  const resolution = resolvePluginInstallation(context);
  if (resolution.kind === "missing-registry") {
    return {
      status: "FAIL",
      name: "plugin",
      detail: `no installed_plugins.json at ${resolution.path}`,
      remedy: "/plugin install ostrom@ostrom"
    };
  }
  if (resolution.kind === "plugin-absent") {
    return {
      status: "FAIL",
      name: "plugin",
      detail: "ostrom@ostrom not present in installed_plugins.json",
      remedy: "/plugin install ostrom@ostrom"
    };
  }
  const {
    installPathVersion,
    loadedVersion,
    registryVersion
  } = resolution.installation;
  if (loadedVersion && registryVersion) {
    const matchesRegistry = loadedVersion === registryVersion;
    return {
      status: matchesRegistry ? "OK" : "WARN",
      name: "plugin",
      detail: matchesRegistry ? `installed, loaded version ${loadedVersion}` : `installed, loaded version ${loadedVersion}, registry version ${registryVersion}`,
      remedy: matchesRegistry ? "" : "restart the session to reconcile the loaded plugin with the registry"
    };
  }
  if (!loadedVersion && registryVersion) {
    const registrySource = installPathVersion ? "registry version" : "registry-recorded version";
    return {
      status: "OK",
      name: "plugin",
      detail: `installed, version ${registryVersion} (loaded plugin.json not readable, using ${registrySource})`,
      remedy: ""
    };
  }
  if (loadedVersion) {
    return {
      status: "WARN",
      name: "plugin",
      detail: `installed, loaded version ${loadedVersion}, registry version not readable`,
      remedy: "restart the session to reconcile the loaded plugin with the registry"
    };
  }
  return {
    status: "FAIL",
    name: "plugin",
    detail: "ostrom@ostrom entry found but no version could be determined",
    remedy: "/plugin install ostrom@ostrom"
  };
}

// src/checks/plugin-cache-drift.ts
import { createHash } from "node:crypto";
import {
  lstatSync,
  readFileSync as readFileSync3,
  readdirSync,
  readlinkSync
} from "node:fs";
import { join as join4 } from "node:path";
var shippedDirectories = [
  "skills",
  "scripts",
  "hooks",
  "rules",
  "runtime"
];
var marketplacePluginRoot = "plugins/ostrom";
function blobHash(contents) {
  return createHash("sha1").update(`blob ${contents.byteLength}\0`).update(contents).digest("hex");
}
function installedFiles(pluginRoot2) {
  const files = /* @__PURE__ */ new Map();
  function walk(path, relativePath) {
    const stat = lstatSync(path);
    if (stat.isDirectory()) {
      if (relativePath.split("/").includes("node_modules")) return;
      for (const entry of readdirSync(path, { withFileTypes: true })) {
        walk(join4(path, entry.name), `${relativePath}/${entry.name}`);
      }
      return;
    }
    if (!stat.isFile() && !stat.isSymbolicLink()) return;
    const contents = stat.isSymbolicLink() ? Buffer.from(readlinkSync(path)) : readFileSync3(path);
    const mode = stat.isSymbolicLink() ? "120000" : stat.mode & 73 ? "100755" : "100644";
    files.set(relativePath, { mode, object: blobHash(contents) });
  }
  for (const directory of shippedDirectories) {
    const path = join4(pluginRoot2, directory);
    try {
      walk(path, directory);
    } catch (error) {
      const code = error.code;
      if (code !== "ENOENT") throw error;
    }
  }
  return files;
}
function marketplaceFiles(marketplaceDir) {
  const result = git(marketplaceDir, [
    "ls-tree",
    "-r",
    "-z",
    "HEAD",
    "--",
    ...shippedDirectories.map(
      (directory) => `${marketplacePluginRoot}/${directory}`
    )
  ]);
  if (result.status !== 0) return void 0;
  const files = /* @__PURE__ */ new Map();
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
function marketplaceVersion(marketplaceDir) {
  const result = git(marketplaceDir, [
    "show",
    `HEAD:${marketplacePluginRoot}/.claude-plugin/plugin.json`
  ]);
  if (result.status !== 0) return "";
  return pluginJsonField(result.stdout, "version");
}
function differences(installed, marketplace) {
  const paths = [.../* @__PURE__ */ new Set([...installed.keys(), ...marketplace.keys()])].sort();
  const result = [];
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
function summarize(items) {
  const shown = items.slice(0, 8);
  const remaining = items.length - shown.length;
  return remaining > 0 ? `${shown.join("; ")}; plus ${remaining} more` : shown.join("; ");
}
function checkPluginCacheDrift(context) {
  const resolution = resolvePluginInstallation(context);
  if (resolution.kind !== "found") {
    const detail = resolution.kind === "missing-registry" ? `installed plugin registry missing at ${resolution.path}` : "ostrom@ostrom not present in installed plugin registry";
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `cannot compare shipped files: ${detail}`,
      remedy: "/plugin install ostrom@ostrom"
    };
  }
  const marketplace = inspectMarketplace(context);
  if (!marketplace.cloneAvailable || !marketplace.fetchAvailable) {
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `cannot compare shipped files: ${marketplace.result.detail}`,
      remedy: marketplace.result.remedy
    };
  }
  const installedVersion = resolution.installation.registryVersion;
  const checkoutVersion = marketplaceVersion(marketplace.directory);
  if (!installedVersion || !checkoutVersion) {
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: "cannot compare shipped files: installed or marketplace version is unreadable",
      remedy: "reinstall ostrom@ostrom, then restart the session"
    };
  }
  if (installedVersion !== checkoutVersion) {
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `versions differ: installed cache ${installedVersion}, marketplace checkout ${checkoutVersion}`,
      remedy: "update and reinstall ostrom@ostrom, then restart the session"
    };
  }
  let installed;
  try {
    installed = installedFiles(resolution.installation.installPath);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `cannot read installed shipped files: ${message}`,
      remedy: "reinstall ostrom@ostrom, then restart the session"
    };
  }
  const checkout = marketplaceFiles(marketplace.directory);
  if (!checkout) {
    return {
      status: "WARN",
      name: "plugin-cache-drift",
      detail: "cannot read shipped files from the marketplace checkout's current commit",
      remedy: "/plugin marketplace update ostrom"
    };
  }
  const drift = differences(installed, checkout);
  if (drift.length > 0) {
    return {
      status: "FAIL",
      name: "plugin-cache-drift",
      detail: `version ${installedVersion} agrees but shipped files drift: ${summarize(drift)}`,
      remedy: "update and reinstall ostrom@ostrom, then restart the session"
    };
  }
  return {
    status: "OK",
    name: "plugin-cache-drift",
    detail: `version ${installedVersion} and shipped files agree with the marketplace checkout`,
    remedy: ""
  };
}

// src/checks/trace-lease.ts
import { existsSync, readFileSync as readFileSync4 } from "node:fs";
import { join as join5 } from "node:path";
var TRACE_STALE_SECONDS = 24 * 60 * 60;
var MAX_DATE_EPOCH_SECONDS = 864e10;
function nowEpoch(context) {
  const explicit = context.env.MANDATE_NOW_EPOCH;
  if (explicit && /^\d+$/.test(explicit)) return Number(explicit);
  const sweepTime = context.env.MANDATE_SWEEP_TIME;
  if (sweepTime) {
    const parsed = Date.parse(sweepTime);
    if (Number.isFinite(parsed)) return Math.floor(parsed / 1e3);
  }
  return Math.floor(Date.now() / 1e3);
}
function exactKeys(value, expected) {
  return JSON.stringify(Object.keys(value).sort()) === JSON.stringify(expected);
}
function jsonObject(value) {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
function traceHealth(trace, now) {
  if (!trace.exists) {
    return {
      status: "WARN",
      detail: "trace absent",
      remedy: "run /ostrom:gatekeep and confirm it creates sprint.jsonl"
    };
  }
  if (!("content" in trace)) {
    return {
      status: "WARN",
      detail: "trace unreadable",
      remedy: "inspect sprint.jsonl and fix its permissions"
    };
  }
  const source = trace.content;
  let contentEnd = source.length;
  while (contentEnd > 0 && source[contentEnd - 1] === "\n") {
    contentEnd -= 1;
    if (contentEnd > 0 && source[contentEnd - 1] === "\r") {
      contentEnd -= 1;
    }
  }
  const lastLineStart = source.lastIndexOf("\n", contentEnd - 1) + 1;
  const lastLine = source.slice(lastLineStart, contentEnd);
  if (lastLine.length === 0) {
    return {
      status: "WARN",
      detail: "trace present but empty",
      remedy: "run /ostrom:gatekeep and confirm it appends a complete pass"
    };
  }
  let record;
  try {
    record = JSON.parse(lastLine);
  } catch {
    return {
      status: "WARN",
      detail: "trace last record is unreadable",
      remedy: "inspect sprint.jsonl and repair or remove its malformed last record"
    };
  }
  if (!jsonObject(record) || !exactKeys(record, ["fact", "kind", "narration", "ts"]) || typeof record.ts !== "string" || record.ts.length === 0 || typeof record.kind !== "string" || record.kind.length === 0 || !jsonObject(record.fact) || !jsonObject(record.narration)) {
    return {
      status: "WARN",
      detail: "trace last record has an invalid shape",
      remedy: "inspect sprint.jsonl; records must be written by ostrom trace append"
    };
  }
  const timestamp = record.ts;
  const timestampMs = Date.parse(timestamp);
  if (!timestamp || !Number.isFinite(timestampMs)) {
    return {
      status: "WARN",
      detail: "trace last record has an invalid timestamp",
      remedy: "inspect sprint.jsonl; records must be written by ostrom trace append"
    };
  }
  const ageSeconds = now - Math.floor(timestampMs / 1e3);
  if (ageSeconds > TRACE_STALE_SECONDS) {
    return {
      status: "WARN",
      detail: `trace stale, last ${timestamp} (older than 24h)`,
      remedy: "run /ostrom:gatekeep and confirm the recurring loop is active"
    };
  }
  return {
    status: "OK",
    detail: `trace current, last ${timestamp}`,
    remedy: ""
  };
}
function validLease(value) {
  if (!value || typeof value !== "object") return false;
  if (!exactKeys(value, ["expires_at", "owner", "started_at"])) return false;
  const lease = value;
  return typeof lease.owner === "string" && lease.owner.length > 0 && Number.isSafeInteger(lease.started_at) && (lease.started_at ?? -1) >= 0 && (lease.started_at ?? Infinity) <= MAX_DATE_EPOCH_SECONDS && Number.isSafeInteger(lease.expires_at) && (lease.expires_at ?? -1) >= (lease.started_at ?? 0) && (lease.expires_at ?? Infinity) <= MAX_DATE_EPOCH_SECONDS;
}
function leaseHealth(path, now) {
  if (!existsSync(path)) {
    return { status: "OK", detail: "lease idle", remedy: "" };
  }
  let source;
  try {
    source = readFileSync4(path, "utf8");
  } catch {
    return {
      status: "WARN",
      detail: "lease unreadable",
      remedy: "inspect sprint.lease and fix its permissions"
    };
  }
  let lease;
  try {
    lease = JSON.parse(source);
  } catch {
    return {
      status: "WARN",
      detail: "lease unreadable",
      remedy: "inspect sprint.lease; only ostrom lease may create or remove it"
    };
  }
  if (!validLease(lease)) {
    return {
      status: "WARN",
      detail: "lease has an invalid shape",
      remedy: "inspect sprint.lease; only ostrom lease may create or remove it"
    };
  }
  const expiry = new Date(lease.expires_at * 1e3).toISOString();
  if (now >= lease.expires_at) {
    return {
      status: "WARN",
      detail: `lease stale for ${lease.owner}, expired ${expiry}`,
      remedy: "allow the next gatekeeper pass to reclaim the expired lease"
    };
  }
  return {
    status: "OK",
    detail: `lease held by ${lease.owner} until ${expiry}`,
    remedy: ""
  };
}
function checkTraceLease(context) {
  const dataDir = join5(context.configDir, "ostrom");
  const now = nowEpoch(context);
  const trace = traceHealth(context.readTrace(), now);
  const lease = leaseHealth(join5(dataDir, "sprint.lease"), now);
  const warned = trace.status === "WARN" || lease.status === "WARN";
  return {
    status: warned ? "WARN" : "OK",
    name: "trace-lease",
    detail: `${trace.detail}; ${lease.detail}`,
    remedy: [trace.remedy, lease.remedy].filter(Boolean).join("; ")
  };
}

// src/checks/work-orders.ts
import { spawnSync as spawnSync2 } from "node:child_process";
function object(value) {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
function dispatchFact(value) {
  return object(value) && value.schema_version === 1 && typeof value.item_id === "string" && value.item_id.length > 0 && typeof value.order_id === "string" && value.order_id.length > 0 && typeof value.unit_name === "string" && value.unit_name.length > 0 && typeof value.backend === "string" && value.backend.length > 0;
}
function inFlight(source) {
  const dispatched = /* @__PURE__ */ new Map();
  const terminal = /* @__PURE__ */ new Set();
  for (const line of source.split(/\r?\n/)) {
    if (!line) continue;
    let record;
    try {
      record = JSON.parse(line);
    } catch {
      continue;
    }
    if (!object(record) || !object(record.fact)) continue;
    if (record.kind === "work-dispatched" && dispatchFact(record.fact)) {
      dispatched.set(record.fact.order_id, record.fact);
    } else if ((record.kind === "work-completed" || record.kind === "work-failed") && typeof record.fact.order_id === "string") {
      terminal.add(record.fact.order_id);
    }
  }
  return [...dispatched.entries()].filter(([orderId]) => !terminal.has(orderId)).map(([, fact]) => fact);
}
function systemdUnitState(context, unitName) {
  const systemctl = context.env.MANDATE_SYSTEMCTL_BIN || "systemctl";
  const result = spawnSync2(
    systemctl,
    ["--user", "show", `${unitName}.service`, "--property=ActiveState", "--value"],
    { encoding: "utf8", env: context.env }
  );
  if (result.status === 4) return null;
  if (result.status !== 0) return void 0;
  const state = result.stdout.trim();
  return state || null;
}
function checkWorkOrders(context) {
  const trace = context.readTrace();
  if (!trace.exists || !("content" in trace)) {
    return {
      status: "OK",
      name: "work-orders",
      detail: "no work orders in flight",
      remedy: ""
    };
  }
  const orders = inFlight(trace.content);
  if (orders.length === 0) {
    return {
      status: "OK",
      name: "work-orders",
      detail: "no work orders in flight",
      remedy: ""
    };
  }
  const faults = [];
  const unknown = [];
  const visible = [];
  for (const order of orders) {
    visible.push(`${order.item_id} (${order.unit_name})`);
    if (order.backend !== "systemd") continue;
    const state = systemdUnitState(context, order.unit_name);
    if (state === void 0) {
      unknown.push(order);
    } else if (!state || !["active", "activating", "reloading", "deactivating"].includes(state)) {
      faults.push(order);
    }
  }
  if (faults.length > 0) {
    return {
      status: "FAIL",
      name: "work-orders",
      detail: `${orders.length} in flight; unit exited without terminal row: ${faults.map((order) => `${order.item_id} (${order.unit_name})`).join(", ")}`,
      remedy: "inspect the transient unit journal and append work-failed before clearing its per-item lease"
    };
  }
  if (unknown.length > 0) {
    return {
      status: "WARN",
      name: "work-orders",
      detail: `${orders.length} in flight; could not inspect unit state: ${unknown.map((order) => `${order.item_id} (${order.unit_name})`).join(", ")}`,
      remedy: "confirm the user systemd manager is reachable and inspect the transient unit"
    };
  }
  return {
    status: "OK",
    name: "work-orders",
    detail: `${orders.length} in flight: ${visible.join(", ")}`,
    remedy: ""
  };
}

// src/lib/config.ts
import { existsSync as existsSync2, readFileSync as readFileSync5 } from "node:fs";
import { join as join6 } from "node:path";
function stripComment(input) {
  let singleQuoted = false;
  let doubleQuoted = false;
  for (let index = 0; index < input.length; index += 1) {
    const character = input[index];
    if (character === "'" && !doubleQuoted) singleQuoted = !singleQuoted;
    if (character === '"' && !singleQuoted && (index === 0 || input[index - 1] !== "\\")) {
      doubleQuoted = !doubleQuoted;
    }
    if (character === "#" && !singleQuoted && !doubleQuoted) {
      return input.slice(0, index).trimEnd();
    }
  }
  return input.trimEnd();
}
function parseScalar(raw) {
  const value = raw.trim();
  if (value.startsWith('"') && value.endsWith('"') || value.startsWith("'") && value.endsWith("'")) {
    return value.slice(1, -1);
  }
  if (value.startsWith("[") && value.endsWith("]")) {
    const body = value.slice(1, -1).trim();
    return body === "" ? [] : body.split(",").map((item) => String(parseScalar(item)));
  }
  if (value === "true") return true;
  if (value === "false") return false;
  if (/^-?(?:\d+|\d*\.\d+)$/.test(value)) return Number(value);
  return value;
}
function parseOstromYaml(source) {
  const config = {};
  let parent;
  for (const originalLine of source.split(/\r?\n/)) {
    const line = stripComment(originalLine);
    if (line.trim() === "") continue;
    const indent = line.match(/^[ \t]*/)?.[0].length ?? 0;
    const trimmed = line.trim();
    if (indent === 0) {
      const match2 = /^([^:]+):(.*)$/.exec(trimmed);
      if (!match2) {
        parent = void 0;
        continue;
      }
      const key2 = match2[1]?.trim();
      const rawValue2 = match2[2]?.trim() ?? "";
      if (!key2) continue;
      if (rawValue2 === "") {
        config[key2] = {};
        parent = key2;
      } else {
        config[key2] = parseScalar(rawValue2);
        parent = void 0;
      }
      continue;
    }
    if (!parent) continue;
    if (trimmed.startsWith("- ")) {
      const current2 = config[parent];
      if (!Array.isArray(current2)) config[parent] = [];
      config[parent].push(String(parseScalar(trimmed.slice(2))));
      continue;
    }
    const match = /^([^:]+):(.*)$/.exec(trimmed);
    const current = config[parent];
    if (!match || Array.isArray(current) || typeof current !== "object") continue;
    const key = match[1]?.trim();
    const rawValue = match[2]?.trim() ?? "";
    if (key && rawValue !== "") current[key] = parseScalar(rawValue);
  }
  return config;
}
function load(path) {
  if (!existsSync2(path)) return {};
  try {
    return parseOstromYaml(readFileSync5(path, "utf8"));
  } catch {
    return {};
  }
}
function pythonStyleString(value) {
  if (value === true) return "True";
  if (value === false) return "False";
  return String(value);
}
function merge(base, override) {
  const merged = { ...base };
  for (const [key, value] of Object.entries(override)) {
    const previous = merged[key];
    if (value !== null && !Array.isArray(value) && typeof value === "object" && previous !== null && !Array.isArray(previous) && typeof previous === "object") {
      merged[key] = { ...previous, ...value };
    } else {
      merged[key] = value;
    }
  }
  return merged;
}
function resolveTouchConfig(pluginRoot2, configDir2, cwd) {
  const paths = [
    join6(pluginRoot2, "config", "touch-defaults.yaml"),
    join6(configDir2, "ostrom", "config.yaml"),
    join6(cwd, ".ostrom", "config.yaml")
  ];
  const config = paths.reduce(
    (resolved, path) => merge(resolved, load(path)),
    {}
  );
  const file = config.file;
  const fileConfig = file !== null && !Array.isArray(file) && typeof file === "object" ? file : {};
  const provider = config.provider;
  return {
    provider: provider === void 0 || provider === "" ? "file" : pythonStyleString(provider),
    path: fileConfig.path === void 0 || fileConfig.path === "" ? "~/.claude/ostrom/touch-log.md" : pythonStyleString(fileConfig.path),
    autoCommit: fileConfig.auto_commit === void 0 ? "False" : pythonStyleString(fileConfig.auto_commit)
  };
}
function resolveMandateSearchRoots(pluginRoot2, configDir2, cwd) {
  const paths = [
    join6(pluginRoot2, "config", "mandate-defaults.yaml"),
    join6(configDir2, "ostrom", "mandates.yaml"),
    join6(cwd, ".ostrom", "mandates.yaml")
  ];
  const config = paths.reduce(
    (resolved, path) => merge(resolved, load(path)),
    {}
  );
  const searchRoots = config.search_roots;
  return Array.isArray(searchRoots) ? searchRoots : [];
}
function expandTilde(path, home2) {
  if (path === "~") return home2;
  if (path.startsWith("~/")) return join6(home2, path.slice(2));
  return path;
}

// src/checks/dispatch-source-roots.ts
function checkDispatchSourceRoots(context) {
  const searchRoots = resolveMandateSearchRoots(
    context.pluginRoot,
    context.configDir,
    context.cwd
  );
  if (searchRoots.length === 0) {
    return {
      status: "FAIL",
      name: "dispatch-source-roots",
      detail: "search_roots is empty; dispatch cannot resolve source repositories",
      remedy: "configure search_roots with a parent directory containing the roster checkouts"
    };
  }
  const noun = searchRoots.length === 1 ? "root" : "roots";
  return {
    status: "OK",
    name: "dispatch-source-roots",
    detail: `${searchRoots.length} search ${noun} configured for dispatch`,
    remedy: ""
  };
}

// src/checks/touch.ts
import {
  accessSync,
  constants,
  existsSync as existsSync3,
  lstatSync as lstatSync2,
  realpathSync,
  statSync as statSync4
} from "node:fs";
import { dirname, join as join7 } from "node:path";
function insideGit(path) {
  return git(path, ["rev-parse", "--is-inside-work-tree"]).status === 0;
}
function writable(path) {
  try {
    accessSync(path, constants.W_OK);
    return true;
  } catch {
    return false;
  }
}
function isFile3(path) {
  try {
    return statSync4(path).isFile();
  } catch {
    return false;
  }
}
function checkTouchDurability(context) {
  const config = context.resolveConfig();
  const expandedPath = expandTilde(config.path, context.home);
  let targetStatus;
  let targetDetail;
  let targetRemedy;
  if (config.provider === "notion") {
    targetStatus = "OK";
    targetDetail = "provider notion (target is inherently shared)";
    targetRemedy = "";
  } else if (config.provider === "file") {
    if (insideGit(dirname(expandedPath))) {
      targetStatus = "OK";
      targetDetail = `file provider, ${expandedPath} is inside a git repo (auto_commit=${config.autoCommit})`;
      targetRemedy = "";
    } else {
      targetStatus = "WARN";
      targetDetail = `file provider, ${expandedPath} is NOT inside a git repo \u2014 touches logged here never reach another machine`;
      targetRemedy = "point file.path into a synced repo and set auto_commit: true, or switch provider";
    }
  } else {
    targetStatus = "WARN";
    targetDetail = `unknown provider '${config.provider}' (durability undetermined)`;
    targetRemedy = "check the resolved touch config's provider value";
  }
  const userConfig = join7(context.configDir, "ostrom", "config.yaml");
  let configStatus;
  let configDetail;
  let configRemedy;
  let symlink = false;
  try {
    symlink = lstatSync2(userConfig).isSymbolicLink();
  } catch {
  }
  if (symlink) {
    let target = "";
    try {
      target = realpathSync(userConfig);
    } catch {
    }
    if (target && insideGit(dirname(target))) {
      configStatus = "OK";
      configDetail = "config.yaml is a symlink into a git repo (versioned, syncs across machines)";
      configRemedy = "";
    } else {
      configStatus = "WARN";
      configDetail = "config.yaml is a symlink, but its target is not inside a git repo";
      configRemedy = "version the symlink target in a private config repo";
    }
  } else if (isFile3(userConfig)) {
    configStatus = "WARN";
    configDetail = "config.yaml is a plain machine-local file (will not sync across machines)";
    configRemedy = `version it: move it into a private config repo and symlink it back to ${userConfig}`;
  } else {
    configStatus = "OK";
    configDetail = "no user config.yaml present (shipped defaults only)";
    configRemedy = "";
  }
  return {
    status: targetStatus === "WARN" || configStatus === "WARN" ? "WARN" : "OK",
    name: "touch-durability",
    detail: `target: ${targetDetail} -- config: ${configDetail}`,
    remedy: [targetRemedy, configRemedy].filter(Boolean).join("; ")
  };
}
function checkProviderReachable(context) {
  const config = context.resolveConfig();
  const expandedPath = expandTilde(config.path, context.home);
  if (config.provider === "notion") {
    return {
      status: "DEFER",
      name: "provider-reachable",
      detail: "notion: MCP availability is a session property, not visible to a shell",
      remedy: ""
    };
  }
  if (config.provider !== "file") {
    return {
      status: "WARN",
      name: "provider-reachable",
      detail: `unknown provider '${config.provider}' (undetermined)`,
      remedy: ""
    };
  }
  const directory = dirname(expandedPath);
  let existingDirectory = directory;
  while (!existsSync3(existingDirectory) && existingDirectory !== "/" && existingDirectory !== "") {
    existingDirectory = dirname(existingDirectory);
  }
  if (writable(existingDirectory)) {
    return {
      status: "OK",
      name: "provider-reachable",
      detail: existingDirectory === directory ? `file: ${directory} is writable` : `file: ${directory} does not exist yet, nearest existing ancestor ${existingDirectory} is writable`,
      remedy: ""
    };
  }
  return {
    status: "FAIL",
    name: "provider-reachable",
    detail: `file: ${existingDirectory} is not writable \u2014 /ostrom:touch cannot write its log`,
    remedy: `fix permissions on ${existingDirectory}, or point file.path elsewhere`
  };
}

// src/checks/builder-pass.ts
var CADENCE_HOURS = {
  builder: 3,
  gatekeeper: 1
};
var ROLE_SKILL = {
  builder: "/ostrom:work",
  gatekeeper: "/ostrom:gatekeep"
};
var PASS_FAULT_THRESHOLD = 3;
function nowEpoch2(context) {
  const explicit = context.env.MANDATE_NOW_EPOCH;
  if (explicit && /^\d+$/.test(explicit)) return Number(explicit);
  const sweepTime = context.env.MANDATE_SWEEP_TIME;
  if (sweepTime) {
    const parsed = Date.parse(sweepTime);
    if (Number.isFinite(parsed)) return Math.floor(parsed / 1e3);
  }
  return Math.floor(Date.now() / 1e3);
}
function object2(value) {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
function formatAge(ageSeconds) {
  const ageMinutes = Math.max(0, Math.floor(ageSeconds / 60));
  if (ageMinutes < 60) return `${ageMinutes}m`;
  const hours = Math.floor(ageMinutes / 60);
  const minutes = ageMinutes % 60;
  return minutes === 0 ? `${hours}h` : `${hours}h${minutes}m`;
}
function recentRolePassEnded(source, role, limit) {
  const records = [];
  let contentEnd = source.length;
  while (contentEnd > 0 && records.length < limit) {
    while (contentEnd > 0 && (source[contentEnd - 1] === "\n" || source[contentEnd - 1] === "\r")) {
      contentEnd -= 1;
    }
    if (contentEnd === 0) break;
    const lineStart = source.lastIndexOf("\n", contentEnd - 1) + 1;
    const line = source.slice(lineStart, contentEnd);
    contentEnd = lineStart > 0 ? lineStart - 1 : 0;
    let record;
    try {
      record = JSON.parse(line);
    } catch {
      continue;
    }
    if (!object2(record) || record.kind !== "pass-ended" || !object2(record.fact)) {
      continue;
    }
    const owner = record.fact.owner;
    if (typeof owner === "string" && owner.startsWith(`${role}-`)) {
      records.push(record);
    }
  }
  return records;
}
function checkRolePass(context, role) {
  const cadenceHours = CADENCE_HOURS[role];
  const checkName = `${role}-pass`;
  const trace = context.readTrace();
  if (!trace.exists) {
    return {
      status: "WARN",
      name: checkName,
      detail: `no ${role} pass ever recorded`,
      remedy: `run ${ROLE_SKILL[role]} and confirm it records pass-ended`
    };
  }
  if (!("content" in trace)) {
    return {
      status: "WARN",
      name: checkName,
      detail: `${role} pass history is unreadable`,
      remedy: "inspect sprint.jsonl and fix its permissions"
    };
  }
  const recent = recentRolePassEnded(trace.content, role, PASS_FAULT_THRESHOLD);
  const record = recent[0];
  if (!record) {
    return {
      status: "WARN",
      name: checkName,
      detail: `no ${role} pass ever recorded`,
      remedy: `run ${ROLE_SKILL[role]} and confirm it records pass-ended`
    };
  }
  const timestamp = record.ts;
  const timestampMs = typeof timestamp === "string" ? Date.parse(timestamp) : NaN;
  if (!Number.isFinite(timestampMs)) {
    return {
      status: "WARN",
      name: checkName,
      detail: `last ${role} pass has an invalid timestamp`,
      remedy: "inspect sprint.jsonl; records must be written by ostrom trace append"
    };
  }
  const ageSeconds = nowEpoch2(context) - Math.floor(timestampMs / 1e3);
  const age = formatAge(ageSeconds);
  if (recent.length === PASS_FAULT_THRESHOLD && recent.every(
    (candidate) => object2(candidate.fact) && candidate.fact.outcome === "no-op"
  )) {
    return {
      status: "FAIL",
      name: checkName,
      detail: `${role} loop has produced ${PASS_FAULT_THRESHOLD} consecutive no-op passes, last ${timestamp} (age ${age})`,
      remedy: `inspect pass-runs/${role} transcripts; the loop is running but the protocol never takes ownership`
    };
  }
  if (recent.length === PASS_FAULT_THRESHOLD && recent.every(
    (candidate) => object2(candidate.fact) && candidate.fact.outcome === "failed"
  )) {
    return {
      status: "FAIL",
      name: checkName,
      detail: `${role} loop has produced ${PASS_FAULT_THRESHOLD} consecutive failed passes, last ${timestamp} (age ${age})`,
      remedy: `inspect pass-runs/${role} transcripts; the protocol takes ownership but does not complete`
    };
  }
  if (ageSeconds > cadenceHours * 60 * 60) {
    return {
      status: "WARN",
      name: checkName,
      detail: `${role} pass stale, last ${timestamp} (age ${age}; older than ${cadenceHours}h cadence)`,
      remedy: `confirm ostrom-${role}-pass.timer is active and loop-armed is present`
    };
  }
  return {
    status: "OK",
    name: checkName,
    detail: `${role} pass current, last ${timestamp} (age ${age}; ${cadenceHours}h cadence)`,
    remedy: ""
  };
}
function checkBuilderPass(context) {
  return checkRolePass(context, "builder");
}
function checkGatekeeperPass(context) {
  return checkRolePass(context, "gatekeeper");
}

// src/checks/publish.ts
import { existsSync as existsSync4, readFileSync as readFileSync6 } from "node:fs";
import { join as join8 } from "node:path";
function nowEpoch3(context) {
  const explicit = context.env.MANDATE_NOW_EPOCH;
  if (explicit && /^\d+$/.test(explicit)) return Number(explicit);
  const sweepTime = context.env.MANDATE_SWEEP_TIME;
  if (sweepTime) {
    const parsed = Date.parse(sweepTime);
    if (Number.isFinite(parsed)) return Math.floor(parsed / 1e3);
  }
  return Math.floor(Date.now() / 1e3);
}
function object3(value) {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
function checkPublish(context) {
  const publishDir = context.env.MANDATE_PUBLISH_DIR ?? join8(context.configDir, "ostrom", "publish");
  const manifestPath = join8(publishDir, "manifest.json");
  if (!existsSync4(manifestPath)) {
    return {
      status: "WARN",
      name: "publish",
      detail: "no publish has been recorded",
      remedy: "run mandate publish.sh and confirm the state branch is reachable"
    };
  }
  let manifest;
  try {
    manifest = JSON.parse(readFileSync6(manifestPath, "utf8"));
  } catch {
    return {
      status: "WARN",
      name: "publish",
      detail: "publish manifest is unreadable",
      remedy: "inspect the cached publish clone and repair or recreate it"
    };
  }
  if (!object3(manifest)) {
    return {
      status: "WARN",
      name: "publish",
      detail: "publish manifest is malformed",
      remedy: "run mandate publish.sh to regenerate the cached record tree"
    };
  }
  const publishedAt = manifest.published_at;
  const cadenceHours = manifest.expected_sweep_interval_hours;
  const publishedMs = typeof publishedAt === "string" ? Date.parse(publishedAt) : Number.NaN;
  if (!Number.isFinite(publishedMs) || typeof cadenceHours !== "number" || !Number.isInteger(cadenceHours) || cadenceHours <= 0) {
    return {
      status: "WARN",
      name: "publish",
      detail: "publish manifest has invalid cadence or timestamp",
      remedy: "run mandate publish.sh to regenerate the cached record tree"
    };
  }
  const ageSeconds = nowEpoch3(context) - Math.floor(publishedMs / 1e3);
  if (ageSeconds > cadenceHours * 60 * 60) {
    return {
      status: "WARN",
      name: "publish",
      detail: `publish stale, last ${publishedAt} (older than ${cadenceHours}h cadence)`,
      remedy: "run mandate publish.sh and confirm the state branch is reachable"
    };
  }
  return {
    status: "OK",
    name: "publish",
    detail: `publish current, last ${publishedAt} (${cadenceHours}h cadence)`,
    remedy: ""
  };
}

// src/checks/cli.ts
import { spawnSync as spawnSync3 } from "node:child_process";
import { createRequire } from "node:module";
import {
  accessSync as accessSync2,
  closeSync,
  constants as constants2,
  existsSync as existsSync5,
  openSync,
  readFileSync as readFileSync7,
  readSync,
  realpathSync as realpathSync2,
  statSync as statSync5
} from "node:fs";
import { delimiter, dirname as dirname2, join as join9, resolve } from "node:path";
var INSTALL_COMMAND = "npm install -g @ostrom/cli";
var UPGRADE_COMMAND = "npm update -g @ostrom/cli";
function parseSemVer(value) {
  const match = value.match(
    /^(?:v)?(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/
  );
  if (!match) return void 0;
  return {
    major: Number(match[1]),
    minor: Number(match[2]),
    patch: Number(match[3]),
    prerelease: match[4]?.split(".") ?? []
  };
}
function compareIdentifiers(left, right) {
  const leftNumeric = /^\d+$/.test(left);
  const rightNumeric = /^\d+$/.test(right);
  if (leftNumeric && rightNumeric) return Number(left) - Number(right);
  if (leftNumeric) return -1;
  if (rightNumeric) return 1;
  return left.localeCompare(right);
}
function compareSemVer(left, right) {
  for (const key of ["major", "minor", "patch"]) {
    const difference = left[key] - right[key];
    if (difference !== 0) return difference;
  }
  if (left.prerelease.length === 0 && right.prerelease.length === 0) return 0;
  if (left.prerelease.length === 0) return 1;
  if (right.prerelease.length === 0) return -1;
  const length = Math.max(left.prerelease.length, right.prerelease.length);
  for (let index = 0; index < length; index += 1) {
    const leftIdentifier = left.prerelease[index];
    const rightIdentifier = right.prerelease[index];
    if (leftIdentifier === void 0) return -1;
    if (rightIdentifier === void 0) return 1;
    const difference = compareIdentifiers(leftIdentifier, rightIdentifier);
    if (difference !== 0) return difference;
  }
  return 0;
}
function executableNames(env) {
  if (process.platform !== "win32") return ["ostrom"];
  const extensions = (env.PATHEXT ?? ".EXE;.CMD;.BAT;.COM").split(";").filter(Boolean);
  return ["ostrom", ...extensions.map((extension) => `ostrom${extension}`)];
}
function firstLine(path) {
  const descriptor = openSync(path, "r");
  try {
    const buffer = Buffer.alloc(256);
    const bytesRead = readSync(descriptor, buffer, 0, buffer.length, 0);
    return buffer.subarray(0, bytesRead).toString("utf8").split(/\r?\n/, 1)[0] ?? "";
  } finally {
    closeSync(descriptor);
  }
}
function resolveOnPath(context) {
  const path = context.env.PATH ?? context.env.Path ?? context.env.path ?? "";
  for (const directory of path.split(delimiter)) {
    for (const name of executableNames(context.env)) {
      const candidate = resolve(context.cwd, directory || ".", name);
      try {
        if (!statSync5(candidate).isFile()) continue;
        accessSync2(candidate, constants2.X_OK);
        return candidate;
      } catch {
      }
    }
  }
  return void 0;
}
function nativeBinary(realPath) {
  try {
    const manifest = JSON.parse(
      readFileSync7(join9(dirname2(realPath), "package.json"), "utf8")
    );
    const packageName = manifest.ostrom?.platformPackages?.[`${process.platform}-${process.arch}`];
    if (!packageName) return void 0;
    const require2 = createRequire(realPath);
    const platformManifestPath = require2.resolve(`${packageName}/package.json`);
    const platformManifest = JSON.parse(
      readFileSync7(platformManifestPath, "utf8")
    );
    if (typeof platformManifest.main !== "string") return void 0;
    const candidate = join9(dirname2(platformManifestPath), platformManifest.main);
    accessSync2(candidate, constants2.X_OK);
    return candidate;
  } catch {
    return void 0;
  }
}
function probeCli(context) {
  const resolvedPath = resolveOnPath(context);
  if (!resolvedPath) return { nodeLauncher: false };
  try {
    const realPath = realpathSync2(resolvedPath);
    const nodeLauncher = /^#!\s*\/usr\/bin\/env(?:\s+-S)?\s+node(?:\s|$)/.test(
      firstLine(realPath)
    );
    const probe = {
      resolvedPath,
      realPath,
      nodeLauncher
    };
    const nativePath = nodeLauncher ? nativeBinary(realPath) : void 0;
    if (nativePath) probe.nativePath = nativePath;
    return probe;
  } catch {
    return { resolvedPath, nodeLauncher: false };
  }
}
function minimumCliVersion(context) {
  try {
    const manifest = JSON.parse(
      readFileSync7(
        join9(context.pluginRoot, ".claude-plugin", "plugin.json"),
        "utf8"
      )
    );
    return typeof manifest.minimumCliVersion === "string" ? manifest.minimumCliVersion : void 0;
  } catch {
    return void 0;
  }
}
function reportedVersion(output) {
  const match = output.match(
    /(?:^|\s)(v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)(?=\s|$)/
  )?.[1];
  return match?.replace(/^v/, "");
}
function checkCliInstalled(context) {
  const probe = probeCli(context);
  if (!probe.resolvedPath) {
    return {
      status: "FAIL",
      name: "cli-installed",
      detail: "ostrom is not installed or is absent from PATH",
      remedy: INSTALL_COMMAND
    };
  }
  return {
    status: "OK",
    name: "cli-installed",
    detail: "ostrom found on PATH",
    remedy: ""
  };
}
function checkCliVersion(context) {
  const probe = probeCli(context);
  if (!probe.resolvedPath) {
    return {
      status: "OK",
      name: "cli-version",
      detail: "not checked because ostrom is absent",
      remedy: ""
    };
  }
  const required = minimumCliVersion(context);
  const requiredVersion = required ? parseSemVer(required) : void 0;
  if (!required || !requiredVersion) {
    return {
      status: "FAIL",
      name: "cli-version",
      detail: "plugin manifest has no valid minimumCliVersion",
      remedy: "repair the installed ostrom plugin manifest"
    };
  }
  const versionExecutable = probe.nativePath ?? probe.resolvedPath;
  const result = spawnSync3(versionExecutable, ["--version"], {
    cwd: context.cwd,
    env: context.env,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 5e3
  });
  const installed = reportedVersion(`${result.stdout ?? ""}
${result.stderr ?? ""}`);
  const installedVersion = installed ? parseSemVer(installed) : void 0;
  if (result.status !== 0 || !installed || !installedVersion) {
    return {
      status: "FAIL",
      name: "cli-version",
      detail: `ostrom resolves at ${probe.resolvedPath}, but its version could not be read`,
      remedy: UPGRADE_COMMAND
    };
  }
  if (compareSemVer(installedVersion, requiredVersion) < 0) {
    return {
      status: "FAIL",
      name: "cli-version",
      detail: `installed ostrom CLI version ${installed} is older than required ${required}`,
      remedy: UPGRADE_COMMAND
    };
  }
  return {
    status: "OK",
    name: "cli-version",
    detail: `installed version ${installed} satisfies required ${required}`,
    remedy: ""
  };
}
function checkCliLauncher(context) {
  const probe = probeCli(context);
  if (!probe.resolvedPath) {
    return {
      status: "OK",
      name: "cli-launcher",
      detail: "not checked because ostrom is absent",
      remedy: ""
    };
  }
  if (!probe.nodeLauncher) {
    return {
      status: "OK",
      name: "cli-launcher",
      detail: "resolved executable is not a Node launcher",
      remedy: ""
    };
  }
  if (probe.nativePath && existsSync5(probe.nativePath)) {
    return {
      status: "WARN",
      name: "cli-launcher",
      detail: `ostrom resolves to the Node launcher at ${probe.resolvedPath}; native binary is ${probe.nativePath}`,
      remedy: `configure non-interactive units to invoke ${probe.nativePath} directly`
    };
  }
  return {
    status: "WARN",
    name: "cli-launcher",
    detail: `ostrom resolves to the Node launcher at ${probe.resolvedPath}, but its native binary could not be resolved`,
    remedy: `${INSTALL_COMMAND} (without --no-optional or --omit=optional)`
  };
}

// src/lib/result.ts
function sanitize(value) {
  return value.replace(/[\r\n]/g, " ").replaceAll("|", "/");
}
function formatResult(result) {
  return [
    result.status,
    result.name,
    sanitize(result.detail),
    sanitize(result.remedy)
  ].join("|");
}

// src/lib/doctor.ts
function createTraceReader(configDir2) {
  let cached;
  return () => {
    if (cached) return cached;
    const path = join10(configDir2, "ostrom", "sprint.jsonl");
    if (!existsSync6(path)) {
      cached = { exists: false };
      return cached;
    }
    try {
      cached = { exists: true, content: readFileSync8(path, "utf8") };
    } catch (error) {
      cached = { exists: true, error };
    }
    return cached;
  };
}
var DOCTOR_CHECK_NAMES = [
  "cli-installed",
  "cli-version",
  "cli-launcher",
  "plugin",
  "marketplace",
  "plugin-cache-drift",
  "rules-layers",
  "touch-durability",
  "provider-reachable",
  "dispatch-source-roots",
  "trace-lease",
  "work-orders",
  "builder-pass",
  "gatekeeper-pass",
  "publish",
  "environment",
  "config-parser"
];
function checkRunners(context) {
  return {
    "cli-installed": () => checkCliInstalled(context),
    "cli-version": () => checkCliVersion(context),
    "cli-launcher": () => checkCliLauncher(context),
    plugin: () => checkPlugin(context),
    marketplace: () => checkMarketplace(context),
    "plugin-cache-drift": () => checkPluginCacheDrift(context),
    "rules-layers": () => checkRulesLayers(context),
    "touch-durability": () => checkTouchDurability(context),
    "provider-reachable": () => checkProviderReachable(context),
    "dispatch-source-roots": () => checkDispatchSourceRoots(context),
    "trace-lease": () => checkTraceLease(context),
    "work-orders": () => checkWorkOrders(context),
    "builder-pass": () => checkBuilderPass(context),
    "gatekeeper-pass": () => checkGatekeeperPass(context),
    publish: () => checkPublish(context),
    environment: () => checkEnvironment(context),
    "config-parser": () => checkConfigParser()
  };
}
function createContext(options2) {
  const env = options2.env ?? process.env;
  return {
    ...options2,
    env,
    resolveConfig: () => resolveTouchConfig(options2.pluginRoot, options2.configDir, options2.cwd),
    readTrace: createTraceReader(options2.configDir)
  };
}
function runDoctor(options2) {
  const runners = checkRunners(createContext(options2));
  const results = DOCTOR_CHECK_NAMES.map((name) => runners[name]());
  return `${results.map(formatResult).join("\n")}
`;
}
function runDoctorCheck(options2, name) {
  if (!DOCTOR_CHECK_NAMES.includes(name)) {
    throw new Error(`unknown doctor check: ${name}`);
  }
  const exactName = name;
  const result = checkRunners(createContext(options2))[exactName]();
  return `${formatResult(result)}
`;
}

// src/doctor.ts
var pluginRoot = resolve2(dirname3(fileURLToPath(import.meta.url)), "..");
var home = process.env.HOME ?? "";
var configDir = process.env.CLAUDE_CONFIG_DIR ?? resolve2(home, ".claude");
var options = {
  pluginRoot,
  configDir,
  cwd: process.cwd(),
  home,
  env: process.env
};
var args = process.argv.slice(2);
if (args.length === 0) {
  process.stdout.write(runDoctor(options));
} else if (args.length === 2 && args[0] === "--check" && args[1]) {
  try {
    process.stdout.write(runDoctorCheck(options, args[1]));
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : "doctor check failed"}
`);
    process.exitCode = 2;
  }
} else {
  process.stderr.write("usage: doctor.js [--check <exact-name>]\n");
  process.exitCode = 2;
}
