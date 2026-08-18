import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import {
  accessSync,
  closeSync,
  constants,
  existsSync,
  openSync,
  readFileSync,
  readSync,
  realpathSync,
  statSync,
} from "node:fs";
import { delimiter, dirname, join, resolve } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import type { CheckResult } from "../lib/result.js";

const INSTALL_COMMAND = "npm install -g @ostrom/cli";
const UPGRADE_COMMAND = "npm update -g @ostrom/cli";

interface SemVer {
  major: number;
  minor: number;
  patch: number;
  prerelease: string[];
}

interface CliProbe {
  resolvedPath?: string;
  realPath?: string;
  nativePath?: string;
  nodeLauncher: boolean;
}

function parseSemVer(value: string): SemVer | undefined {
  const match = value.match(
    /^(?:v)?(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/,
  );
  if (!match) return undefined;
  return {
    major: Number(match[1]),
    minor: Number(match[2]),
    patch: Number(match[3]),
    prerelease: match[4]?.split(".") ?? [],
  };
}

function compareIdentifiers(left: string, right: string): number {
  const leftNumeric = /^\d+$/.test(left);
  const rightNumeric = /^\d+$/.test(right);
  if (leftNumeric && rightNumeric) return Number(left) - Number(right);
  if (leftNumeric) return -1;
  if (rightNumeric) return 1;
  return left.localeCompare(right);
}

function compareSemVer(left: SemVer, right: SemVer): number {
  for (const key of ["major", "minor", "patch"] as const) {
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
    if (leftIdentifier === undefined) return -1;
    if (rightIdentifier === undefined) return 1;
    const difference = compareIdentifiers(leftIdentifier, rightIdentifier);
    if (difference !== 0) return difference;
  }
  return 0;
}

function executableNames(env: NodeJS.ProcessEnv): string[] {
  if (process.platform !== "win32") return ["ostrom"];
  const extensions = (env.PATHEXT ?? ".EXE;.CMD;.BAT;.COM")
    .split(";")
    .filter(Boolean);
  return ["ostrom", ...extensions.map((extension) => `ostrom${extension}`)];
}

function firstLine(path: string): string {
  const descriptor = openSync(path, "r");
  try {
    // Native binaries can be large; the shebang decision needs only the first line.
    const buffer = Buffer.alloc(256);
    const bytesRead = readSync(descriptor, buffer, 0, buffer.length, 0);
    return (
      buffer.subarray(0, bytesRead).toString("utf8").split(/\r?\n/, 1)[0] ??
      ""
    );
  } finally {
    closeSync(descriptor);
  }
}

function resolveOnPath(context: DoctorContext): string | undefined {
  const path = context.env.PATH ?? context.env.Path ?? context.env.path ?? "";
  for (const directory of path.split(delimiter)) {
    for (const name of executableNames(context.env)) {
      // A relative PATH segment resolves against the *inspected* environment's
      // working directory, not this process's. `resolve()` would silently use
      // `process.cwd()` instead, which makes the answer depend on where doctor
      // happens to have been started from — and defeats the point of taking cwd
      // from the context at all. An empty segment already means "here", and
      // means the same "here".
      const candidate = resolve(context.cwd, directory || ".", name);
      try {
        if (!statSync(candidate).isFile()) continue;
        accessSync(candidate, constants.X_OK);
        return candidate;
      } catch {
        // PATH entries are routinely missing or unreadable; only an executable is a hit.
      }
    }
  }
  return undefined;
}

function nativeBinary(realPath: string): string | undefined {
  try {
    const manifest = JSON.parse(
      readFileSync(join(dirname(realPath), "package.json"), "utf8"),
    ) as { ostrom?: { platformPackages?: Record<string, string> } };
    const packageName =
      manifest.ostrom?.platformPackages?.[`${process.platform}-${process.arch}`];
    if (!packageName) return undefined;
    const require = createRequire(realPath);
    const platformManifestPath = require.resolve(`${packageName}/package.json`);
    const platformManifest = JSON.parse(
      readFileSync(platformManifestPath, "utf8"),
    ) as { main?: unknown };
    if (typeof platformManifest.main !== "string") return undefined;
    const candidate = join(dirname(platformManifestPath), platformManifest.main);
    accessSync(candidate, constants.X_OK);
    return candidate;
  } catch {
    return undefined;
  }
}

function probeCli(context: DoctorContext): CliProbe {
  const resolvedPath = resolveOnPath(context);
  if (!resolvedPath) return { nodeLauncher: false };
  try {
    const realPath = realpathSync(resolvedPath);
    const nodeLauncher = /^#!\s*\/usr\/bin\/env(?:\s+-S)?\s+node(?:\s|$)/.test(
      firstLine(realPath),
    );
    const probe: CliProbe = {
      resolvedPath,
      realPath,
      nodeLauncher,
    };
    const nativePath = nodeLauncher ? nativeBinary(realPath) : undefined;
    if (nativePath) probe.nativePath = nativePath;
    return probe;
  } catch {
    return { resolvedPath, nodeLauncher: false };
  }
}

function minimumCliVersion(context: DoctorContext): string | undefined {
  try {
    const manifest = JSON.parse(
      readFileSync(
        join(context.pluginRoot, ".claude-plugin", "plugin.json"),
        "utf8",
      ),
    ) as { minimumCliVersion?: unknown };
    return typeof manifest.minimumCliVersion === "string"
      ? manifest.minimumCliVersion
      : undefined;
  } catch {
    return undefined;
  }
}

function reportedVersion(output: string): string | undefined {
  const match = output.match(
    /(?:^|\s)(v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)(?=\s|$)/,
  )?.[1];
  return match?.replace(/^v/, "");
}

export function checkCliInstalled(context: DoctorContext): CheckResult {
  const probe = probeCli(context);
  if (!probe.resolvedPath) {
    return {
      status: "FAIL",
      name: "cli-installed",
      detail: "ostrom is not installed or is absent from PATH",
      remedy: INSTALL_COMMAND,
    };
  }
  return {
    status: "OK",
    name: "cli-installed",
    detail: "ostrom found on PATH",
    remedy: "",
  };
}

export function checkCliVersion(context: DoctorContext): CheckResult {
  const probe = probeCli(context);
  if (!probe.resolvedPath) {
    return {
      status: "OK",
      name: "cli-version",
      detail: "not checked because ostrom is absent",
      remedy: "",
    };
  }

  const required = minimumCliVersion(context);
  const requiredVersion = required ? parseSemVer(required) : undefined;
  if (!required || !requiredVersion) {
    return {
      status: "FAIL",
      name: "cli-version",
      detail: "plugin manifest has no valid minimumCliVersion",
      remedy: "repair the installed ostrom plugin manifest",
    };
  }

  // The npm launcher needs `node` on PATH. Probe the packaged native binary
  // when possible so doctor can still distinguish an old CLI from the exact
  // non-interactive launcher failure that prompted this check.
  const versionExecutable = probe.nativePath ?? probe.resolvedPath;
  const result = spawnSync(versionExecutable, ["--version"], {
    cwd: context.cwd,
    env: context.env,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 5_000,
  });
  const installed = reportedVersion(`${result.stdout ?? ""}\n${result.stderr ?? ""}`);
  const installedVersion = installed ? parseSemVer(installed) : undefined;
  if (result.status !== 0 || !installed || !installedVersion) {
    return {
      status: "FAIL",
      name: "cli-version",
      detail: `ostrom resolves at ${probe.resolvedPath}, but its version could not be read`,
      remedy: UPGRADE_COMMAND,
    };
  }
  if (compareSemVer(installedVersion, requiredVersion) < 0) {
    return {
      status: "FAIL",
      name: "cli-version",
      detail: `installed ostrom CLI version ${installed} is older than required ${required}`,
      remedy: UPGRADE_COMMAND,
    };
  }
  return {
    status: "OK",
    name: "cli-version",
    detail: `installed version ${installed} satisfies required ${required}`,
    remedy: "",
  };
}

export function checkCliLauncher(context: DoctorContext): CheckResult {
  const probe = probeCli(context);
  if (!probe.resolvedPath) {
    return {
      status: "OK",
      name: "cli-launcher",
      detail: "not checked because ostrom is absent",
      remedy: "",
    };
  }
  if (!probe.nodeLauncher) {
    return {
      status: "OK",
      name: "cli-launcher",
      detail: "resolved executable is not a Node launcher",
      remedy: "",
    };
  }
  if (probe.nativePath && existsSync(probe.nativePath)) {
    return {
      status: "WARN",
      name: "cli-launcher",
      detail: `ostrom resolves to the Node launcher at ${probe.resolvedPath}; native binary is ${probe.nativePath}`,
      remedy: `configure non-interactive units to invoke ${probe.nativePath} directly`,
    };
  }
  return {
    status: "WARN",
    name: "cli-launcher",
    detail: `ostrom resolves to the Node launcher at ${probe.resolvedPath}, but its native binary could not be resolved`,
    remedy: `${INSTALL_COMMAND} (without --no-optional or --omit=optional)`,
  };
}
