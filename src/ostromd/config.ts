// ostromd's configuration: three environment variables, all required.
//
//   OSTROM_REPO       owner/name of the one repository to watch
//   OSTROM_ALLOWLIST  comma-separated GitHub logins who may start work
//   OSTROM_DB         path to the SQLite store

import { readFileSync } from "node:fs";
import { hostname } from "node:os";

import { loadAppConfig } from "../github/app.ts";
import { loadMergePolicy, type MergePolicy } from "../github/merge-policy.ts";

export { loadBudgetDefaults, type BudgetDefaults } from "../budget/budgets.ts";

export interface Config {
  repo: string;
  allowlist: string[];
  dbPath: string;
  mergePolicy: MergePolicy;
}

export function loadConfig(env: Record<string, string | undefined>): Config {
  loadAppConfig(env);
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
  return { repo, allowlist, dbPath, mergePolicy: loadMergePolicy(env) };
}

// Native Workers read these. The first two are required; the rest have defaults:
//
//   OSTROM_WORKER_REPO       path to the git checkout each Worker's worktree is added to
//   OSTROM_WORKER_BASE       branch each Worker's worktree starts from and its PR targets
//   OSTROM_WORKER_WORKTREES  directory holding the worktrees; default <repo>.worktrees
//   OSTROM_WORKER_HARNESS    claude or codex; default codex
//   OSTROM_WORKER_MODEL      model slug; default gpt-6-luna
//   OSTROM_WORKER_RULES_FILE a file of the machine's house rules (heat limits, where tests run),
//                            added to every Worker brief; default none

export interface WorkerConfig {
  repo: string;
  base: string;
  worktrees: string;
  harness: "claude" | "codex";
  model: string;
  machine: string;
  /** House rules added to every brief, read from OSTROM_WORKER_RULES_FILE. */
  rules?: string;
}

export function loadWorkerConfig(env: Record<string, string | undefined>): WorkerConfig {
  const read = (name: string) => {
    const value = env[name]?.trim() ?? "";
    if (value === "") throw new Error(`${name} must be set to dispatch Workers`);
    return value;
  };
  const repo = read("OSTROM_WORKER_REPO");
  const harness = env["OSTROM_WORKER_HARNESS"]?.trim() || "codex";
  if (harness !== "claude" && harness !== "codex") {
    throw new Error(`OSTROM_WORKER_HARNESS must be claude or codex, got ${JSON.stringify(harness)}`);
  }
  return {
    repo,
    base: read("OSTROM_WORKER_BASE"),
    worktrees: env["OSTROM_WORKER_WORKTREES"]?.trim() || `${repo.replace(/\/+$/, "")}.worktrees`,
    harness,
    model: env["OSTROM_WORKER_MODEL"]?.trim() || "gpt-6-luna",
    machine: hostname(),
    ...rulesFrom(env["OSTROM_WORKER_RULES_FILE"]?.trim()),
  };
}

function rulesFrom(path: string | undefined): { rules?: string } {
  if (!path) return {};
  const rules = readFileSync(path, "utf8").trim();
  if (rules === "") throw new Error(`OSTROM_WORKER_RULES_FILE ${path} is empty`);
  return { rules };
}

// The Router uses a fresh, read-only session per batch:
//
//   OSTROM_ROUTER_HARNESS  claude or codex; default codex
//   OSTROM_ROUTER_MODEL    model slug; default gpt-6-luna

export interface RouterConfig {
  harness: "claude" | "codex";
  model: string;
}

export function loadRouterConfig(env: Record<string, string | undefined>): RouterConfig {
  const harness = env["OSTROM_ROUTER_HARNESS"]?.trim() || "codex";
  if (harness !== "claude" && harness !== "codex") {
    throw new Error(`OSTROM_ROUTER_HARNESS must be claude or codex, got ${JSON.stringify(harness)}`);
  }
  return {
    harness,
    model: env["OSTROM_ROUTER_MODEL"]?.trim() || "gpt-6-luna",
  };
}

// The Watchdog reads one more, required: how long a Worker may show no progress before it counts
// as stalled. It is Semon's `--idle-minutes` threshold.
//
//   OSTROM_STALL_MINUTES  a positive whole number of minutes, e.g. 20

export interface WatchdogConfig {
  idleMinutes: number;
}

export function loadWatchdogConfig(env: Record<string, string | undefined>): WatchdogConfig {
  const value = env["OSTROM_STALL_MINUTES"]?.trim() ?? "";
  if (!/^[1-9]\d*$/.test(value)) {
    throw new Error(`OSTROM_STALL_MINUTES must be a positive whole number of minutes, got ${JSON.stringify(value)}`);
  }
  return { idleMinutes: Number(value) };
}
