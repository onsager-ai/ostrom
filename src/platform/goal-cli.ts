#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { Readable } from "node:stream";

import { parseGoalSelfArgs, runPlatformCli } from "./cli.ts";

const usage = `usage: ostrom-goal set <slug> [--thread <threadId>] --statement TEXT --metric NAME --target N
                       [--current N] [--unit TEXT] [--week YYYY-MM-DD] [--plan JSON] [--approved-decision ID]
       ostrom-goal complete <slug> [--thread <threadId>] [--week YYYY-MM-DD] [--current N]`;

type RemoteConfig = { sshTarget?: string; remoteNode?: string; remoteCli?: string };

function remoteConfig(env: NodeJS.ProcessEnv): RemoteConfig {
  const explicitPath = env["OSTROM_GOAL_CONFIG"];
  const path = explicitPath ?? join(homedir(), ".config", "ostrom", "goal-remote.json");
  let contents: string;
  try {
    contents = readFileSync(path, "utf8");
  } catch (error) {
    if (explicitPath === undefined && (error as NodeJS.ErrnoException).code === "ENOENT") return {};
    throw error;
  }
  const config: unknown = JSON.parse(contents);
  if (typeof config !== "object" || config === null || Array.isArray(config)) {
    throw new Error("goal remote config must be a JSON object");
  }
  for (const key of ["sshTarget", "remoteNode", "remoteCli"] as const) {
    if (!(key in config)) continue;
    const value = (config as Record<string, unknown>)[key];
    if (typeof value !== "string" || value.trim() === "" || value.includes("\0")) {
      throw new Error(`goal remote config ${key} must be non-empty text`);
    }
  }
  return config as RemoteConfig;
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

function runSsh(args: string[], input: string, env: NodeJS.ProcessEnv) {
  return spawnSync("ssh", args, { input, env, encoding: "utf8", maxBuffer: 4 * 1024 * 1024 });
}

interface GoalWrapperIO {
  stdout?: { write(text: string): unknown };
  stderr?: { write(text: string): unknown };
  env?: NodeJS.ProcessEnv;
  ssh?: typeof runSsh;
}

export async function runGoalWrapper(argv: readonly string[], io: GoalWrapperIO = {}): Promise<number> {
  const { stdout = process.stdout, stderr = process.stderr, env = process.env, ssh = runSsh } = io;
  let input: string;
  try {
    const request = parseGoalSelfArgs(argv, env["T3CODE_THREAD_ID"]?.trim());
    input = `${JSON.stringify(request)}\n`;
  } catch (error) {
    stderr.write(`ostrom-goal: ${(error as Error).message}\n${usage}\n`);
    return 2;
  }

  try {
    const config = remoteConfig(env);
    const target = env["OSTROM_GOAL_SSH_TARGET"]?.trim() || config.sshTarget;
    const command = ["--json", "goal-self", "--stdin-json"];
    if (target === undefined) {
      return await runPlatformCli(command, { stdin: Readable.from([input]), stdout, stderr, env });
    }
    const remoteCli = config.remoteCli ?? "ostrom-platform";
    // SSH uses the remote login shell. Quote configured paths and keep all goal fields on stdin.
    const executable = config.remoteNode === undefined ? [remoteCli] : [config.remoteNode, "--", remoteCli];
    const remote = [...executable, ...command].map(shellQuote).join(" ");
    const result = ssh(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=accept-new",
      "--", target, remote], input, env);
    if (result.error !== undefined) throw result.error;
    stdout.write(result.stdout);
    stderr.write(result.stderr);
    return result.status ?? 1;
  } catch (error) {
    stderr.write(`ostrom-goal: ${(error as Error).message}\n`);
    return 1;
  }
}

if (import.meta.main) process.exitCode = await runGoalWrapper(process.argv.slice(2));
