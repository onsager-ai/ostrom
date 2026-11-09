// Runs a program with an argv array and no shell. Ostrom uses this only for deterministic local
// commands such as Semon; their exit status and output stay explicit at the call site.

import { spawn } from "node:child_process";

export interface ProcessInvocation {
  readonly args: string[];
  readonly stdin?: string;
}

export interface ProcessExit {
  readonly code: number | null;
  readonly stdout: string;
  readonly stderr: string;
}

export type ProcessRunner = (invocation: ProcessInvocation) => Promise<ProcessExit>;

export function processRunner(bin: string): ProcessRunner {
  return ({ args, stdin }) =>
    new Promise((resolve, reject) => {
      const child = spawn(bin, args, { shell: false, stdio: ["pipe", "pipe", "pipe"] });
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8").on("data", (chunk: string) => (stdout += chunk));
      child.stderr.setEncoding("utf8").on("data", (chunk: string) => (stderr += chunk));
      child.on("error", reject);
      child.on("close", (code) => resolve({ code, stdout, stderr }));
      child.stdin.on("error", () => {});
      child.stdin.end(stdin ?? "");
    });
}
