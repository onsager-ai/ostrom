// Runs the `t3` CLI. Arguments go to the process as an argv array and never through a shell;
// briefs and messages go on stdin (`-` as the message), so their text is never parsed as a flag.
// A non-zero exit or output the parsers reject is an error, never a success (AGENTS.md rule 4).

import { spawn } from "node:child_process";
import {
  parseLaunchResult,
  parseRunResult,
  parseSendResult,
  parseThreadShow,
  type T3LaunchResult,
  type T3RunResult,
  type T3SendResult,
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
  override readonly name: string = "T3CommandError";
}

/**
 * `t3 thread show` on a thread that does not exist or was deleted. At the pin the server answers
 * `Thread <id> is no longer available.` (apps/server/src/mcp/OrchestratorMcpService.ts,
 * `loadReadableThread`) and the CLI prints that message alone and exits 1.
 */
export class T3ThreadGone extends T3CommandError {
  override readonly name = "T3ThreadGone";
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

/** A thread with no worktree that runs in the project folder, such as the Router. */
export interface StartRun {
  readonly project: string;
  readonly title: string;
  readonly message: string;
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

  /**
   * `t3 thread send` without `--wait`: T3 starts a run on an idle thread or queues the message as
   * the busy thread's next turn (docs/direction.md, open question 1). Nothing is interrupted.
   */
  async sendMessage(threadId: string, message: string): Promise<T3SendResult> {
    return parseSendResult(
      await this.#call({ args: ["thread", "send", "--json", threadId, "-"], stdin: message }),
    );
  }

  async showThread(threadId: string): Promise<T3ThreadShow> {
    const exit = await this.#run({ args: ["thread", "show", "--messages", "0", "--json", threadId] });
    if (exit.code === 1 && exit.stderr.trim() === `Thread ${threadId} is no longer available.`) {
      throw new T3ThreadGone(`t3 thread ${threadId} is no longer available`);
    }
    return parseThreadShow(this.#stdout(["thread", "show"], exit));
  }

  /**
   * `t3 thread start --wait --json`: creates the thread, waits for its first run to end and
   * returns the run. A run that failed or waits for an answer is returned too, not thrown, so
   * the caller keeps the new thread's id; it checks `status` itself.
   */
  async startRun(start: StartRun): Promise<T3RunResult> {
    return this.#waited({
      args: [
        "thread",
        "start",
        "--title",
        start.title,
        "--provider",
        start.provider,
        "--model",
        start.model,
        "--wait",
        "--json",
        start.project,
        "-",
      ],
      stdin: start.message,
    });
  }

  /** `t3 thread send --wait --json`: queues the message as the next turn and waits for it to end. */
  async sendRun(threadId: string, message: string): Promise<T3RunResult> {
    return this.#waited({ args: ["thread", "send", "--wait", "--json", threadId, "-"], stdin: message });
  }

  /**
   * With `--wait --json` the CLI prints the run result even when it exits 1 (the run failed) or
   * 2 (it waits for an answer). Any other exit, or no result on stdout, is a command error.
   */
  async #waited(invocation: T3Invocation): Promise<T3RunResult> {
    const exit = await this.#run(invocation);
    if ((exit.code === 1 || exit.code === 2) && exit.stdout.trim() !== "") {
      return parseRunResult(exit.stdout);
    }
    return parseRunResult(this.#stdout(invocation.args, exit));
  }

  async #call(invocation: T3Invocation): Promise<string> {
    return this.#stdout(invocation.args, await this.#run(invocation));
  }

  #stdout(args: readonly string[], exit: T3Exit): string {
    if (exit.code !== 0) {
      const detail = exit.stderr.trim() || exit.stdout.trim() || "no output";
      throw new T3CommandError(
        `t3 ${args.slice(0, 2).join(" ")} exited ${exit.code ?? "by signal"}: ${detail}`,
      );
    }
    return exit.stdout;
  }
}
