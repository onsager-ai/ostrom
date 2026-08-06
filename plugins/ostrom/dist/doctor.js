// src/doctor.ts
import { dirname as dirname2, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// src/checks/rules.ts
import { statSync } from "node:fs";
import { join } from "node:path";

// src/lib/process.ts
import { spawnSync } from "node:child_process";
function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd,
    env: options.env,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"]
  });
  return {
    status: result.status ?? 1,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? result.error?.message ?? ""
  };
}
function git(cwd, args) {
  return run("git", ["-C", cwd, ...args]);
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
function checkMarketplace(context) {
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
    return {
      status: "FAIL",
      name: "marketplace",
      detail: "ostrom not registered in known_marketplaces.json",
      remedy: "/plugin marketplace add onsager-ai/ostrom"
    };
  }
  if (!cloneIsDirectory) {
    return {
      status: "FAIL",
      name: "marketplace",
      detail: `registered, but no cached clone at ${marketplaceDir}`,
      remedy: "/plugin marketplace add onsager-ai/ostrom"
    };
  }
  const fetch = git(marketplaceDir, ["fetch", "origin", "main"]);
  if (fetch.status !== 0) {
    const firstLine = `${fetch.stdout}${fetch.stderr}`.split(/\r?\n/, 1)[0] ?? "";
    return {
      status: "WARN",
      name: "marketplace",
      detail: `cannot verify freshness, git fetch failed (offline?): ${firstLine}`,
      remedy: ""
    };
  }
  if (git(marketplaceDir, ["rev-parse", "--verify", "origin/main"]).status !== 0) {
    return {
      status: "WARN",
      name: "marketplace",
      detail: "fetched, but origin/main not found (default branch may differ)",
      remedy: ""
    };
  }
  if (git(marketplaceDir, [
    "merge-base",
    "--is-ancestor",
    "HEAD",
    "origin/main"
  ]).status === 0) {
    return {
      status: "OK",
      name: "marketplace",
      detail: "cached clone can fast-forward to origin/main",
      remedy: ""
    };
  }
  if (git(marketplaceDir, ["merge-base", "HEAD", "origin/main"]).status === 0) {
    return {
      status: "WARN",
      name: "marketplace",
      detail: "cached clone has diverged from origin/main (shared history, not fast-forwardable)",
      remedy: "/plugin marketplace update ostrom"
    };
  }
  return {
    status: "FAIL",
    name: "marketplace",
    detail: "cached clone and origin/main have unrelated histories (marketplace was republished from a fresh history)",
    remedy: "/plugin marketplace remove ostrom && /plugin marketplace add onsager-ai/ostrom"
  };
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
function field(source, name) {
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
function versionAt(pluginRoot2) {
  if (!pluginRoot2) return "";
  const pluginJson = join3(pluginRoot2, ".claude-plugin", "plugin.json");
  if (!isFile2(pluginJson)) return "";
  try {
    return field(readFileSync2(pluginJson, "utf8"), "version");
  } catch {
    return "";
  }
}
function checkPlugin(context) {
  const installedJson = join3(context.configDir, "plugins", "installed_plugins.json");
  if (!isFile2(installedJson)) {
    return {
      status: "FAIL",
      name: "plugin",
      detail: `no installed_plugins.json at ${installedJson}`,
      remedy: "/plugin install ostrom@ostrom"
    };
  }
  let source = "";
  try {
    source = readFileSync2(installedJson, "utf8");
  } catch {
  }
  const marker = source.indexOf('"ostrom@ostrom"');
  if (marker < 0) {
    return {
      status: "FAIL",
      name: "plugin",
      detail: "ostrom@ostrom not present in installed_plugins.json",
      remedy: "/plugin install ostrom@ostrom"
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

// src/checks/trace-lease.ts
import { existsSync, readFileSync as readFileSync3 } from "node:fs";
import { join as join4 } from "node:path";
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
function traceHealth(path, now) {
  if (!existsSync(path)) {
    return {
      status: "WARN",
      detail: "trace absent",
      remedy: "run /ostrom:gatekeep and confirm it creates sprint.jsonl"
    };
  }
  let source;
  try {
    source = readFileSync3(path, "utf8");
  } catch {
    return {
      status: "WARN",
      detail: "trace unreadable",
      remedy: "inspect sprint.jsonl and fix its permissions"
    };
  }
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
      remedy: "inspect sprint.jsonl; records must be written by trace.sh append"
    };
  }
  const timestamp = record.ts;
  const timestampMs = Date.parse(timestamp);
  if (!timestamp || !Number.isFinite(timestampMs)) {
    return {
      status: "WARN",
      detail: "trace last record has an invalid timestamp",
      remedy: "inspect sprint.jsonl; records must be written by trace.sh append"
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
    source = readFileSync3(path, "utf8");
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
      remedy: "inspect sprint.lease; only lease.sh may create or remove it"
    };
  }
  if (!validLease(lease)) {
    return {
      status: "WARN",
      detail: "lease has an invalid shape",
      remedy: "inspect sprint.lease; only lease.sh may create or remove it"
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
  const dataDir = join4(context.configDir, "ostrom");
  const now = nowEpoch(context);
  const trace = traceHealth(join4(dataDir, "sprint.jsonl"), now);
  const lease = leaseHealth(join4(dataDir, "sprint.lease"), now);
  const warned = trace.status === "WARN" || lease.status === "WARN";
  return {
    status: warned ? "WARN" : "OK",
    name: "trace-lease",
    detail: `${trace.detail}; ${lease.detail}`,
    remedy: [trace.remedy, lease.remedy].filter(Boolean).join("; ")
  };
}

// src/checks/touch.ts
import {
  accessSync,
  constants,
  existsSync as existsSync3,
  lstatSync,
  realpathSync,
  statSync as statSync4
} from "node:fs";
import { dirname, join as join6 } from "node:path";

// src/lib/config.ts
import { existsSync as existsSync2, readFileSync as readFileSync4 } from "node:fs";
import { join as join5 } from "node:path";
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
    return parseOstromYaml(readFileSync4(path, "utf8"));
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
    join5(pluginRoot2, "config", "touch-defaults.yaml"),
    join5(configDir2, "ostrom", "config.yaml"),
    join5(cwd, ".ostrom", "config.yaml")
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
function expandTilde(path, home2) {
  if (path === "~") return home2;
  if (path.startsWith("~/")) return join5(home2, path.slice(2));
  return path;
}

// src/checks/touch.ts
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
  const userConfig = join6(context.configDir, "ostrom", "config.yaml");
  let configStatus;
  let configDetail;
  let configRemedy;
  let symlink = false;
  try {
    symlink = lstatSync(userConfig).isSymbolicLink();
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
function runDoctor(options) {
  const env = options.env ?? process.env;
  const context = {
    ...options,
    env,
    resolveConfig: () => resolveTouchConfig(options.pluginRoot, options.configDir, options.cwd)
  };
  const results = [
    checkPlugin(context),
    checkMarketplace(context),
    checkRulesLayers(context),
    checkTouchDurability(context),
    checkProviderReachable(context),
    checkTraceLease(context),
    checkEnvironment(context),
    checkConfigParser()
  ];
  return `${results.map(formatResult).join("\n")}
`;
}

// src/doctor.ts
var pluginRoot = resolve(dirname2(fileURLToPath(import.meta.url)), "..");
var home = process.env.HOME ?? "";
var configDir = process.env.CLAUDE_CONFIG_DIR ?? resolve(home, ".claude");
process.stdout.write(
  runDoctor({
    pluginRoot,
    configDir,
    cwd: process.cwd(),
    home,
    env: process.env
  })
);
