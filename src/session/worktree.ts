import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { promisify } from "node:util";

const exec = promisify(execFile);

export async function createWorktree(repo: string, path: string, base: string, ticket: number, assignment: number, suffix = randomUUID()): Promise<string> {
  if (!Number.isSafeInteger(ticket) || ticket < 1 || !Number.isSafeInteger(assignment) || assignment < 1) {
    throw new Error("ticket and assignment must be positive integers");
  }
  // Ticket/assignment IDs are local to one store. A fresh store must never adopt an old PR
  // merely because its IDs restart at one; the recorded branch remains the ownership identity.
  const branch = `ostrom/ticket-${ticket}-${assignment}-${suffix}`;
  await exec("git", ["-C", repo, "fetch", "origin", base]);
  await exec("git", ["-C", repo, "worktree", "add", "-b", branch, "--", path, `origin/${base}`]);
  return branch;
}

export async function removeWorktree(repo: string, path: string): Promise<void> {
  await exec("git", ["-C", repo, "worktree", "remove", "--", path]);
}

/** A successful push to origin records an update in the branch's remote-tracking reflog. */
export async function lastPushedHead(worktree: string, branch: string): Promise<string | null> {
  const ref = `refs/remotes/origin/${branch}`;
  try {
    await exec("git", ["-C", worktree, "reflog", "exists", ref]);
  } catch (error) {
    if (typeof error === "object" && error !== null && "code" in error && error.code === 1) return null;
    throw error;
  }
  const { stdout } = await exec("git", ["-C", worktree, "reflog", "show", "-1", "--format=%H", "--grep-reflog=^update by push$", ref, "--"]);
  return stdout.trim() || null;
}
