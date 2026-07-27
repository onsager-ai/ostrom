import {
  accessSync,
  constants,
  existsSync,
  lstatSync,
  realpathSync,
  statSync,
} from "node:fs";
import { dirname, join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import { expandTilde } from "../lib/config.js";
import { git } from "../lib/process.js";
import type { CheckResult, Status } from "../lib/result.js";

function insideGit(path: string): boolean {
  return git(path, ["rev-parse", "--is-inside-work-tree"]).status === 0;
}

function writable(path: string): boolean {
  try {
    accessSync(path, constants.W_OK);
    return true;
  } catch {
    return false;
  }
}

function isFile(path: string): boolean {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

export function checkTouchDurability(context: DoctorContext): CheckResult {
  const config = context.resolveConfig();
  const expandedPath = expandTilde(config.path, context.home);
  let targetStatus: Status;
  let targetDetail: string;
  let targetRemedy: string;

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
      targetDetail = `file provider, ${expandedPath} is NOT inside a git repo — touches logged here never reach another machine`;
      targetRemedy =
        "point file.path into a synced repo and set auto_commit: true, or switch provider";
    }
  } else {
    targetStatus = "WARN";
    targetDetail = `unknown provider '${config.provider}' (durability undetermined)`;
    targetRemedy = "check the resolved touch config's provider value";
  }

  const userConfig = join(context.configDir, "ostrom", "config.yaml");
  let configStatus: Status;
  let configDetail: string;
  let configRemedy: string;
  let symlink = false;
  try {
    symlink = lstatSync(userConfig).isSymbolicLink();
  } catch {
    // Missing config is the normal shipped-defaults path.
  }

  if (symlink) {
    let target = "";
    try {
      target = realpathSync(userConfig);
    } catch {
      // A broken symlink is intentionally diagnosed as not versioned.
    }
    if (target && insideGit(dirname(target))) {
      configStatus = "OK";
      configDetail =
        "config.yaml is a symlink into a git repo (versioned, syncs across machines)";
      configRemedy = "";
    } else {
      configStatus = "WARN";
      configDetail =
        "config.yaml is a symlink, but its target is not inside a git repo";
      configRemedy = "version the symlink target in a private config repo";
    }
  } else if (isFile(userConfig)) {
    configStatus = "WARN";
    configDetail =
      "config.yaml is a plain machine-local file (will not sync across machines)";
    configRemedy = `version it: move it into a private config repo and symlink it back to ${userConfig}`;
  } else {
    configStatus = "OK";
    configDetail = "no user config.yaml present (shipped defaults only)";
    configRemedy = "";
  }

  return {
    status:
      targetStatus === "WARN" || configStatus === "WARN" ? "WARN" : "OK",
    name: "touch-durability",
    detail: `target: ${targetDetail} -- config: ${configDetail}`,
    remedy: [targetRemedy, configRemedy].filter(Boolean).join("; "),
  };
}

export function checkProviderReachable(context: DoctorContext): CheckResult {
  const config = context.resolveConfig();
  const expandedPath = expandTilde(config.path, context.home);
  if (config.provider === "notion") {
    return {
      status: "DEFER",
      name: "provider-reachable",
      detail:
        "notion: MCP availability is a session property, not visible to a shell",
      remedy: "",
    };
  }
  if (config.provider !== "file") {
    return {
      status: "WARN",
      name: "provider-reachable",
      detail: `unknown provider '${config.provider}' (undetermined)`,
      remedy: "",
    };
  }

  const directory = dirname(expandedPath);
  let existingDirectory = directory;
  while (
    !existsSync(existingDirectory) &&
    existingDirectory !== "/" &&
    existingDirectory !== ""
  ) {
    existingDirectory = dirname(existingDirectory);
  }
  if (writable(existingDirectory)) {
    return {
      status: "OK",
      name: "provider-reachable",
      detail:
        existingDirectory === directory
          ? `file: ${directory} is writable`
          : `file: ${directory} does not exist yet, nearest existing ancestor ${existingDirectory} is writable`,
      remedy: "",
    };
  }
  return {
    status: "FAIL",
    name: "provider-reachable",
    detail: `file: ${existingDirectory} is not writable — /touch cannot write its log`,
    remedy: `fix permissions on ${existingDirectory}, or point file.path elsewhere`,
  };
}
