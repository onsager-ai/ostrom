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
