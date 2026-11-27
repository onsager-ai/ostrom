import { execFile } from "node:child_process";
import { promisify } from "node:util";

import type { PlatformStore } from "./store.ts";
import type { GitHubPrObservation } from "./types.ts";

const exec = promisify(execFile);
type ReadGitHub = (path: string) => Promise<unknown>;

async function readGitHub(path: string): Promise<unknown> {
  const args = ["api", "--hostname", "github.com", "-H", "X-GitHub-Api-Version: 2022-11-28", "--method", "GET", path];
  const { stdout } = await exec("gh", args, {
    timeout: 5_000, maxBuffer: 2 * 1024 * 1024,
    env: { ...process.env, GH_PROMPT_DISABLED: "1", GH_DEBUG: "" },
  });
  return JSON.parse(stdout) as unknown;
}

function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("invalid GitHub response");
  return value as Record<string, unknown>;
}

async function pages(read: ReadGitHub, path: string, field: string): Promise<unknown[]> {
  const rows: unknown[] = [];
  for (let page = 1; page <= 5; page++) {
    const result = object(await read(`${path}${path.includes("?") ? "&" : "?"}per_page=100&page=${page}`));
    const batch = result[field];
    if (!Array.isArray(batch) || batch.length > 100) throw new Error("invalid GitHub page");
    rows.push(...batch);
    const total = result["total_count"];
    if (typeof total === "number" ? total === rows.length : batch.length < 100) return rows;
    if (batch.length === 0) break;
  }
  throw new Error("GitHub page limit");
}

const passing = new Set(["success", "neutral", "skipped"]);
const failing = new Set(["failure", "error", "cancelled", "timed_out", "action_required", "startup_failure", "stale"]);

export async function readGitHubPr(repository: string, number: number, read: ReadGitHub = readGitHub): Promise<GitHubPrObservation> {
  if (!/^[a-z\d][\w-]*\/[\w.-]+$/i.test(repository) || repository.endsWith("/.") || repository.endsWith("/..")
    || !Number.isSafeInteger(number) || number < 1) throw new Error("invalid GitHub PR");
  const base = `repos/${repository}`;
  const pr = object(await read(`${base}/pulls/${number}`));
  const headSha = object(pr["head"])["sha"];
  if (typeof headSha !== "string" || !/^[a-f\d]{40}$/i.test(headSha)) throw new Error("invalid GitHub head");
  if (pr["state"] === "closed") return { headSha, rollupState: "closed", totalChecks: 0, failedChecks: 0 };
  if (pr["state"] !== "open") throw new Error("invalid GitHub PR state");

  const commit = `${base}/commits/${headSha}`;
  const checks = await pages(read, `${commit}/check-runs?filter=latest`, "check_runs");
  const statuses = await pages(read, `${commit}/status`, "statuses");
  // The combined status endpoint returns the latest status per context.
  const results = [
    ...checks.map((entry) => {
      const check = object(entry);
      return check["status"] === "completed" ? check["conclusion"] : "pending";
    }),
    ...statuses.map((entry) => object(entry)["state"]),
  ];
  const failedChecks = results.filter((state) => typeof state === "string" && failing.has(state)).length;
  // Report a pending rollup until every known check has settled.
  const pending = results.some((state) => typeof state !== "string" || (!passing.has(state) && !failing.has(state)));
  const rollupState = pending || results.length === 0 ? "pending" : failedChecks > 0 ? "failure" : "success";
  return { headSha, rollupState, totalChecks: results.length, failedChecks };
}

export async function pollGitHubPrs(store: PlatformStore, read: ReadGitHub = readGitHub, signal?: AbortSignal): Promise<void> {
  for (const { repository, number } of store.listGitHubPrTargets()) {
    if (signal?.aborted) return;
    let observation: GitHubPrObservation;
    try {
      observation = await readGitHubPr(repository, number, read);
    } catch {
      // Child process errors include stdout and stderr and may contain credentials.
      observation = { headSha: null, rollupState: "error", totalChecks: 0, failedChecks: 0 };
    }
    if (signal?.aborted) return;
    store.recordGitHubPrCheck(repository, number, observation);
  }
}
