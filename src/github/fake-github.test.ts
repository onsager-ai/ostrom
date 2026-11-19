import assert from "node:assert/strict";
import { test } from "node:test";

import { FakeGitHub } from "./fake-github.ts";
import { ghMergeGet } from "./gh.ts";

test("fake GitHub serves version-pinned merge reads and PR review comments", async () => {
  const now = new Date("2026-11-19T12:00:00Z");
  const github = new FakeGitHub(() => now, "o/r");
  const pull = { number: 2, head: { sha: "head" } };
  github.mergePulls.set(2, { pull, evidence: {} });
  github.issue(2, true);
  github.issue(3, true);
  const comment = github.reviewComment(2, { login: "reviewer", body: "fix this", at: now });
  github.reviewComment(3, { login: "reviewer", body: "another PR", at: now });

  assert.deepEqual(await ghMergeGet(github.gh, "repos/o/r/pulls/2"), pull);
  assert.deepEqual(await ghMergeGet(github.gh, "repos/o/r/pulls/2/comments?per_page=100&page=1"), [comment]);
  assert.deepEqual(await ghMergeGet(github.gh, "repos/o/r/pulls/4/comments?per_page=100&page=1"), []);
  assert.ok(github.calls.every((args) => args.includes("X-GitHub-Api-Version: 2022-11-28")));
});
