import { strict as assert } from "node:assert";
import { execFileSync } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createWorktree, removeWorktree } from "./worktree.ts";

test("create and remove a ticket worktree from the requested base", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-worktree-"));
  const repo = join(root, "repo");
  const path = join(root, "worker");
  try {
    execFileSync("git", ["init", repo]);
    execFileSync("git", ["-C", repo, "-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "base"]);
    const base = execFileSync("git", ["-C", repo, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
    // A previous store can leave both the old fixed branch and its historical PR behind.
    execFileSync("git", ["-C", repo, "branch", "ostrom/ticket-12-34"]);
    const first = await createWorktree(repo, path, base, 12, 34);
    assert.match(first, /^ostrom\/ticket-12-34-[a-f0-9-]{36}$/);
    assert.equal(execFileSync("git", ["-C", path, "branch", "--show-current"], { encoding: "utf8" }).trim(), first);
    const secondPath = join(root, "other-worker");
    const second = await createWorktree(repo, secondPath, base, 12, 34);
    assert.notEqual(second, first, "a new store with the same IDs gets a different branch identity");
    await removeWorktree(repo, secondPath);
    assert.equal(execFileSync("git", ["-C", path, "rev-parse", "HEAD"], { encoding: "utf8" }).trim(), base);
    await removeWorktree(repo, path);
    assert.doesNotMatch(execFileSync("git", ["-C", repo, "worktree", "list", "--porcelain"], { encoding: "utf8" }), /worker/);
  } finally { await rm(root, { recursive: true, force: true }); }
});
