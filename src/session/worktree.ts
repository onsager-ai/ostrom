import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { promisify } from "node:util";

const exec = promisify(execFile);

export async function createWorktree(repo: string, path: string, base: string, ticket: number, assignment: number): Promise<string> {
  if (!Number.isSafeInteger(ticket) || ticket < 1 || !Number.isSafeInteger(assignment) || assignment < 1) {
    throw new Error("ticket and assignment must be positive integers");
  }
  // Ticket/assignment IDs are local to one store. A fresh store must never adopt an old PR
  // merely because its IDs restart at one; the recorded branch remains the ownership identity.
  const branch = `ostrom/ticket-${ticket}-${assignment}-${randomUUID()}`;
  await exec("git", ["-C", repo, "worktree", "add", "-b", branch, "--", path, base]);
  return branch;
}

export async function removeWorktree(repo: string, path: string): Promise<void> {
  await exec("git", ["-C", repo, "worktree", "remove", "--", path]);
}
