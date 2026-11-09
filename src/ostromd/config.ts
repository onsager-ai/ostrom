// ostromd's configuration: three environment variables, all required.
//
//   OSTROM_REPO       owner/name of the one repository to watch
//   OSTROM_ALLOWLIST  comma-separated GitHub logins who may start work
//   OSTROM_DB         path to the SQLite store

export interface Config {
  repo: string;
  allowlist: string[];
  dbPath: string;
}

export function loadConfig(env: Record<string, string | undefined>): Config {
  const repo = env["OSTROM_REPO"]?.trim() ?? "";
  if (!/^[\w.-]+\/[\w.-]+$/.test(repo)) {
    throw new Error(`OSTROM_REPO must be owner/name, got ${JSON.stringify(repo)}`);
  }
  const allowlist = (env["OSTROM_ALLOWLIST"] ?? "")
    .split(",")
    .map((login) => login.trim())
    .filter((login) => login !== "");
  if (allowlist.length === 0) {
    throw new Error("OSTROM_ALLOWLIST must name at least one GitHub login");
  }
  const dbPath = env["OSTROM_DB"]?.trim() ?? "";
  if (dbPath === "") throw new Error("OSTROM_DB must be the path to the store");
  return { repo, allowlist, dbPath };
}

// Worker dispatch reads four more, all required and with no defaults, so a Worker never runs
// on a model nobody chose:
//
//   OSTROM_T3_PROJECT       T3 Code project id, or a path inside the project
//   OSTROM_WORKER_BASE      branch each Worker's worktree starts from and its PR targets
//   OSTROM_WORKER_PROVIDER  T3 provider instance id, e.g. claudeAgent
//   OSTROM_WORKER_MODEL     model slug, e.g. claude-haiku-5-5

export interface WorkerConfig {
  project: string;
  base: string;
  provider: string;
  model: string;
}

export function loadWorkerConfig(env: Record<string, string | undefined>): WorkerConfig {
  const read = (name: string) => {
    const value = env[name]?.trim() ?? "";
    if (value === "") throw new Error(`${name} must be set to dispatch Workers`);
    return value;
  };
  return {
    project: read("OSTROM_T3_PROJECT"),
    base: read("OSTROM_WORKER_BASE"),
    provider: read("OSTROM_WORKER_PROVIDER"),
    model: read("OSTROM_WORKER_MODEL"),
  };
}

// The Router reads two more, also required with no defaults, plus OSTROM_T3_PROJECT, the project
// its thread runs in:
//
//   OSTROM_ROUTER_PROVIDER  T3 provider instance id for the Router thread, e.g. claudeAgent
//   OSTROM_ROUTER_MODEL     model slug for the Router thread, e.g. claude-haiku-5-5

export interface RouterConfig {
  project: string;
  provider: string;
  model: string;
}

export function loadRouterConfig(env: Record<string, string | undefined>): RouterConfig {
  const read = (name: string) => {
    const value = env[name]?.trim() ?? "";
    if (value === "") throw new Error(`${name} must be set to run the Router`);
    return value;
  };
  return {
    project: read("OSTROM_T3_PROJECT"),
    provider: read("OSTROM_ROUTER_PROVIDER"),
    model: read("OSTROM_ROUTER_MODEL"),
  };
}
