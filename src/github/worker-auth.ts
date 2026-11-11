import { access, mkdtemp, rm, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { GitHubApp } from "./app.ts";

/** On-demand helpers keep long turns authenticated without storing a token on disk. */
export async function workerAuth(app: GitHubApp, env: NodeJS.ProcessEnv) {
  let executable: string | undefined;
  for (const directory of (env["PATH"] ?? "").split(delimiter)) {
    const candidate = resolve(directory, "gh");
    try { await access(candidate, constants.X_OK); executable = candidate; break; } catch { /* Try the next PATH entry. */ }
  }
  if (!executable) throw new Error("gh executable not found for App Workers");
  const identity = await app.identity();
  const directory = await mkdtemp(join(tmpdir(), "ostrom-app-"));
  try {
    const helper = fileURLToPath(new URL("./worker-credential.ts", import.meta.url));
    for (const [name, mode] of [["gh", "gh"], ["git-credential-ostrom", "git"]] as const) {
      await writeFile(join(directory, name), `#!${process.execPath}\nimport { run } from ${JSON.stringify(new URL("./worker-credential.ts", import.meta.url).href)};\nawait run(${JSON.stringify(mode)}, ${JSON.stringify(executable)});\n`, { mode: 0o700 });
    }
    const count = Number(env["GIT_CONFIG_COUNT"] ?? "0");
    if (!Number.isSafeInteger(count) || count < 0) throw new Error("Invalid GIT_CONFIG_COUNT");
    const repo = env["OSTROM_REPO"]!;
    const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
    const settings = [["credential.helper", ""], ["credential.https://github.com.helper", `!${quote(process.execPath)} ${quote(helper)} git`], ["credential.useHttpPath", "true"],
      [`url.https://github.com/${repo}.insteadOf`, `git@github.com:${repo}`],
      [`url.https://github.com/${repo}.insteadOf`, `ssh://git@github.com/${repo}`]];
    const result: NodeJS.ProcessEnv = { ...env, PATH: `${directory}${delimiter}${env["PATH"] ?? ""}` };
    delete result["GH_TOKEN"];
    delete result["GITHUB_TOKEN"];
    settings.forEach(([key, value], index) => {
      result[`GIT_CONFIG_KEY_${count + index}`] = key;
      result[`GIT_CONFIG_VALUE_${count + index}`] = value;
    });
    result["GIT_CONFIG_COUNT"] = String(count + settings.length);
    for (const role of ["AUTHOR", "COMMITTER"]) {
      result[`GIT_${role}_NAME`] = identity.name;
      result[`GIT_${role}_EMAIL`] = identity.email;
    }
    return { env: result, identity, close: () => rm(directory, { recursive: true, force: true }) };
  } catch (error) { await rm(directory, { recursive: true, force: true }); throw error; }
}
