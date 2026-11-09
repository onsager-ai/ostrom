// Runs the `t3` CLI. Arguments go to the process as an argv array and never through a shell;
// the Worker brief goes on stdin (`-` as the message), so its text is never parsed as a flag.
// A non-zero exit or output the parsers reject is an error, never a success (AGENTS.md rule 4).

import { spawn } from "node:child_process";
import {
  parseLaunchResult,
  parseThreadShow,
  type T3LaunchResult,
  type T3ThreadShow,
} from "./cli-json.ts";

export interface T3Invocation {
  readonly args: readonly string[];
  readonly stdin?: string;
}

export interface T3Exit {
  readonly code: number | null;
  readonly stdout: string;
  readonly stderr: string;
}

/** Runs one `t3` command to completion. Injected so tests can stand in for the CLI. */
export type T3Runner = (invocation: T3Invocation) => Promise<T3Exit>;

export class T3CommandError extends Error {
  override readonly name = "T3CommandError";
}

/** The real runner: spawns `bin` (default `t3` on PATH) with no shell. */
export function processRunner(bin = "t3"): T3Runner {
  return ({ args, stdin }) =>
    new Promise((resolve, reject) => {
      const child = spawn(bin, args, { shell: false, stdio: ["pipe", "pipe", "pipe"] });
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8").on("data", (chunk: string) => (stdout += chunk));
      child.stderr.setEncoding("utf8").on("data", (chunk: string) => (stderr += chunk));
      child.on("error", reject);
      child.on("close", (code) => resolve({ code, stdout, stderr }));
      // A t3 that exits before reading stdin breaks the pipe; its exit code reports the failure.
      child.stdin.on("error", () => {});
      child.stdin.end(stdin ?? "");
    });
}

export interface StartThread {
  /** T3 project id, or a path inside the project. */
  readonly project: string;
  readonly title: string;
  readonly brief: string;
  /** The branch, tag or commit the new worktree starts from. */
  readonly worktreeBase: string;
  readonly branch: string;
  readonly provider: string;
  readonly model: string;
}

export class T3Cli {
  readonly #run: T3Runner;

  constructor(run: T3Runner) {
    this.#run = run;
  }

  /**
   * `t3 thread start` without `--wait`: returns once the thread exists and its first run is
   * queued. The Worker's progress is read later with `showThread`.
   */
  async startThread(start: StartThread): Promise<T3LaunchResult> {
    const stdout = await this.#call({
      args: [
        "thread",
        "start",
        "--title",
        start.title,
        "--worktree",
        start.worktreeBase,
        "--branch",
        start.branch,
        "--provider",
        start.provider,
        "--model",
        start.model,
        "--json",
        start.project,
        "-",
      ],
      stdin: start.brief,
    });
    const launched = parseLaunchResult(stdout);
    if (launched.runId === null) {
      throw new T3CommandError(`t3 thread start created ${launched.threadId} without starting a run`);
    }
    return launched;
  }

  async showThread(threadId: string): Promise<T3ThreadShow> {
    return parseThreadShow(
      await this.#call({ args: ["thread", "show", "--messages", "0", "--json", threadId] }),
    );
  }

  async #call(invocation: T3Invocation): Promise<string> {
    const exit = await this.#run(invocation);
    if (exit.code !== 0) {
      const detail = exit.stderr.trim() || exit.stdout.trim() || "no output";
      throw new T3CommandError(
        `t3 ${invocation.args.slice(0, 2).join(" ")} exited ${exit.code ?? "by signal"}: ${detail}`,
      );
    }
    return exit.stdout;
  }
}
