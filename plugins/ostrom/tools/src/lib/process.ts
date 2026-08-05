import { spawnSync } from "node:child_process";

export interface CommandResult {
  status: number;
  stdout: string;
  stderr: string;
}

export function run(
  command: string,
  args: string[],
  options: {
    cwd?: string;
    env?: NodeJS.ProcessEnv;
  } = {},
): CommandResult {
  const result = spawnSync(command, args, {
    cwd: options.cwd,
    env: options.env,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });

  return {
    status: result.status ?? 1,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? result.error?.message ?? "",
  };
}

export function git(cwd: string, args: string[]): CommandResult {
  return run("git", ["-C", cwd, ...args]);
}
