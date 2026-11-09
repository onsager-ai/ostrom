import { execFile } from "node:child_process";
import { promisify } from "node:util";

const exec = promisify(execFile);

export async function createWorktree(repo: string, path: string, base: string, ticket: number, assignment: number): Promise<string> {
  if (!Number.isSafeInteger(ticket) || ticket < 1 || !Number.isSafeInteger(assignment) || assignment < 1) {
    throw new Error("ticket and assignment must be positive integers");
  }
  const branch = `ostrom/ticket-${ticket}-${assignment}`;
  await exec("git", ["-C", repo, "worktree", "add", "-b", branch, "--", path, base]);
  return branch;
}

export async function removeWorktree(repo: string, path: string): Promise<void> {
  await exec("git", ["-C", repo, "worktree", "remove", "--", path]);
}
