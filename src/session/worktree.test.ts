import { strict as assert } from "node:assert";
import { execFileSync } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createWorktree, removeWorktree } from "./worktree.ts";

function git(args: string[], cwd?: string): string {
  return execFileSync("git", [...(cwd === undefined ? [] : ["-C", cwd]), ...args], { encoding: "utf8" }).trim();
}

test("create and remove a ticket worktree from the freshly fetched base", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-worktree-"));
  const remote = join(root, "remote.git");
  const seed = join(root, "seed");
  const repo = join(root, "repo");
  const path = join(root, "worker");
  try {
    git(["init", "--bare", remote]);
    git(["init", seed]);
    git(["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"], seed);
    git(["branch", "-M", "main"], seed);
    git(["remote", "add", "origin", remote], seed);
    git(["push", "-u", "origin", "main"], seed);
    git(["clone", remote, repo]);
    git(["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "stale local main"], repo);
    const stale = git(["rev-parse", "HEAD"], repo);
    git(["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "updated remote main"], seed);
    const fresh = git(["rev-parse", "HEAD"], seed);
    git(["push", "origin", "main"], seed);
    assert.notEqual(stale, fresh);
    // A previous store can leave both the old fixed branch and its historical PR behind.
    git(["branch", "ostrom/ticket-12-34"], repo);

    const first = await createWorktree(repo, path, "main", 12, 34);
    assert.match(first, /^ostrom\/ticket-12-34-[a-f0-9-]{36}$/);
    assert.equal(git(["branch", "--show-current"], path), first);
    assert.equal(git(["rev-parse", "HEAD"], path), fresh);
    const secondPath = join(root, "other-worker");
    const second = await createWorktree(repo, secondPath, "main", 12, 34);
    assert.notEqual(second, first, "a new store with the same IDs gets a different branch identity");
    await removeWorktree(repo, secondPath);
    assert.equal(git(["rev-parse", "HEAD"], path), fresh);
    await removeWorktree(repo, path);
    assert.doesNotMatch(git(["worktree", "list", "--porcelain"], repo), /worker/);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("a failed fetch does not create a ticket worktree", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-worktree-fetch-"));
  const repo = join(root, "repo");
  const path = join(root, "worker");
  try {
    git(["init", repo]);
    git(["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "local base"], repo);
    await assert.rejects(createWorktree(repo, path, "main", 12, 34));
    assert.equal(git(["worktree", "list", "--porcelain"], repo).split("\n").filter((line) => line.startsWith("worktree ")).length, 1);
  } finally { await rm(root, { recursive: true, force: true }); }
});
