import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

type Scalar = string | boolean | number | string[];
type ConfigValue = Scalar | Record<string, Scalar>;
type Config = Record<string, ConfigValue>;

export interface TouchConfig {
  provider: string;
  path: string;
  autoCommit: string;
}

function stripComment(input: string): string {
  let singleQuoted = false;
  let doubleQuoted = false;

  for (let index = 0; index < input.length; index += 1) {
    const character = input[index];
    if (character === "'" && !doubleQuoted) singleQuoted = !singleQuoted;
    if (
      character === '"' &&
      !singleQuoted &&
      (index === 0 || input[index - 1] !== "\\")
    ) {
      doubleQuoted = !doubleQuoted;
    }
    if (character === "#" && !singleQuoted && !doubleQuoted) {
      return input.slice(0, index).trimEnd();
    }
  }

  return input.trimEnd();
}

function parseScalar(raw: string): Scalar {
  const value = raw.trim();
  if (
    (value.startsWith('"') && value.endsWith('"')) ||
    (value.startsWith("'") && value.endsWith("'"))
  ) {
    return value.slice(1, -1);
  }
  if (value.startsWith("[") && value.endsWith("]")) {
    const body = value.slice(1, -1).trim();
    return body === ""
      ? []
      : body.split(",").map((item) => String(parseScalar(item)));
  }
  if (value === "true") return true;
  if (value === "false") return false;
  if (/^-?(?:\d+|\d*\.\d+)$/.test(value)) return Number(value);
  return value;
}

// Ostrom configs deliberately use a small YAML subset: top-level scalars,
// one nested mapping/list level, inline lists, and comments. Unsupported YAML
// features are ignored instead of being guessed at; check 7 reports this
// parser's exact scope.
export function parseOstromYaml(source: string): Config {
  const config: Config = {};
  let parent: string | undefined;

  for (const originalLine of source.split(/\r?\n/)) {
    const line = stripComment(originalLine);
    if (line.trim() === "") continue;

    const indent = line.match(/^[ \t]*/)?.[0].length ?? 0;
    const trimmed = line.trim();

    if (indent === 0) {
      const match = /^([^:]+):(.*)$/.exec(trimmed);
      if (!match) {
        parent = undefined;
        continue;
      }
      const key = match[1]?.trim();
      const rawValue = match[2]?.trim() ?? "";
      if (!key) continue;
      if (rawValue === "") {
        config[key] = {};
        parent = key;
      } else {
        config[key] = parseScalar(rawValue);
        parent = undefined;
      }
      continue;
    }

    if (!parent) continue;
    if (trimmed.startsWith("- ")) {
      const current = config[parent];
      if (!Array.isArray(current)) config[parent] = [];
      (config[parent] as string[]).push(String(parseScalar(trimmed.slice(2))));
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

function load(path: string): Config {
  if (!existsSync(path)) return {};
  try {
    return parseOstromYaml(readFileSync(path, "utf8"));
  } catch {
    return {};
  }
}

function pythonStyleString(value: Scalar): string {
  if (value === true) return "True";
  if (value === false) return "False";
  return String(value);
}

function merge(base: Config, override: Config): Config {
  const merged = { ...base };
  for (const [key, value] of Object.entries(override)) {
    const previous = merged[key];
    if (
      value !== null &&
      !Array.isArray(value) &&
      typeof value === "object" &&
      previous !== null &&
      !Array.isArray(previous) &&
      typeof previous === "object"
    ) {
      merged[key] = { ...previous, ...value };
    } else {
      merged[key] = value;
    }
  }
  return merged;
}

export function resolveTouchConfig(
  pluginRoot: string,
  configDir: string,
  cwd: string,
): TouchConfig {
  const paths = [
    join(pluginRoot, "config", "touch-defaults.yaml"),
    join(configDir, "ostrom", "config.yaml"),
    join(cwd, ".ostrom", "config.yaml"),
  ];
  const config = paths.reduce<Config>(
    (resolved, path) => merge(resolved, load(path)),
    {},
  );
  const file = config.file;
  const fileConfig =
    file !== null && !Array.isArray(file) && typeof file === "object" ? file : {};
  const provider = config.provider;

  return {
    provider:
      provider === undefined || provider === ""
        ? "file"
        : pythonStyleString(provider as Scalar),
    path:
      fileConfig.path === undefined || fileConfig.path === ""
        ? "~/.claude/ostrom/touch-log.md"
        : pythonStyleString(fileConfig.path),
    autoCommit:
      fileConfig.auto_commit === undefined
        ? "False"
        : pythonStyleString(fileConfig.auto_commit),
  };
}

export function resolveMandateCadenceHours(
  pluginRoot: string,
  configDir: string,
  cwd: string,
): number | undefined {
  const paths = [
    join(pluginRoot, "config", "mandate-defaults.yaml"),
    join(configDir, "ostrom", "mandates.yaml"),
    join(cwd, ".ostrom", "mandates.yaml"),
  ];
  const config = paths.reduce<Config>(
    (resolved, path) => merge(resolved, load(path)),
    {},
  );
  const cadence = config.cadence_hours;
  return typeof cadence === "number" &&
    Number.isSafeInteger(cadence) &&
    cadence > 0
    ? cadence
    : undefined;
}

export function expandTilde(path: string, home: string): string {
  if (path === "~") return home;
  if (path.startsWith("~/")) return join(home, path.slice(2));
  return path;
}
