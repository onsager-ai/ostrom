// The one way Ostrom talks to GitHub: `gh api`, run as a child process with an argv array and no
// shell. The runner is injected so tests can stand in a fake GitHub.

import { execFile } from "node:child_process";

/** Runs `gh` with `args` and resolves to its stdout. Rejects when `gh` exits non-zero. */
export type GhRunner = (args: readonly string[]) => Promise<string>;

/** Credentials stay in the child's environment, never in argv. */
export function githubRunner(token?: () => Promise<string>, env: NodeJS.ProcessEnv = process.env): GhRunner {
  return async (args) => {
    const credential = await token?.();
    return new Promise((resolve, reject) => {
      execFile("gh", [...args], { env: { ...env, ...(credential === undefined ? {} : { GH_TOKEN: credential }) }, maxBuffer: 64 * 1024 * 1024 }, (error, stdout, stderr) => {
        if (error) {
          const detail = stderr.trim() || error.message;
          reject(new Error(`gh ${args.slice(0, 2).join(" ")} failed: ${credential ? detail.replaceAll(credential, "[redacted]") : detail}`));
        } else {
          resolve(stdout);
        }
      });
    });
  };
}

export const execGh: GhRunner = githubRunner();

/** GETs a REST path (with its query string) and parses the JSON response. */
export async function ghGet(gh: GhRunner, path: string): Promise<unknown> {
  return JSON.parse(await gh(["api", "--method", "GET", path]));
}

/** REST reads whose shape affects merge eligibility are pinned to one GitHub API version. */
export async function ghMergeGet(gh: GhRunner, path: string): Promise<unknown> {
  return JSON.parse(await gh(["api", "-H", "X-GitHub-Api-Version: 2022-11-28", "--method", "GET", path]));
}

/** Sends the only merge request Ostrom supports: SHA-guarded squash. */
export async function ghSquashMerge(gh: GhRunner, repo: string, number: number, sha: string): Promise<unknown> {
  return JSON.parse(await gh([
    "api", "-H", "X-GitHub-Api-Version: 2022-11-28", "--method", "PUT", `repos/${repo}/pulls/${number}/merge`,
    "-f", `sha=${sha}`, "-f", "merge_method=squash",
  ]));
}

/** GitHub's `since` takes ISO 8601 to the second; dropping milliseconds only widens the window. */
export function sinceParam(at: Date): string {
  return at.toISOString().replace(/\.\d{3}Z$/, "Z");
}
