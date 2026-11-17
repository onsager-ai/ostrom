import assert from "node:assert/strict";
import { test } from "node:test";
import { rewritePublicPaths } from "./public-reply.ts";

const worktree = "/home/marvin/projects/onsager-ai/ostrom-pilot/duhem.worktrees/ticket-2-2-pilot";
const context = { worktree, repo: "onsager-ai/duhem", headSha: "6ce1fb8" };

test("pilot file links point at the PR head, with either line notation", () => {
  for (const [suffix, anchor] of [["", ""], [":23", "#L23"], ["#L23", "#L23"]]) {
    assert.equal(rewritePublicPaths(`[docs/duhem-spec.md](${worktree}/docs/duhem-spec.md${suffix})`, [context]),
      `[docs/duhem-spec.md](https://github.com/onsager-ai/duhem/blob/6ce1fb8/docs/duhem-spec.md${anchor})`);
  }
});

test("plain paths and links without a known head use repository-relative code", () => {
  assert.equal(rewritePublicPaths(`Changed ${worktree}/docs/duhem-spec.md. See \`${worktree}/src/main.ts:12\`.`, [context]),
    "Changed `docs/duhem-spec.md`. See `src/main.ts:12`.");
  assert.equal(rewritePublicPaths(`[spec](${worktree}/docs/duhem-spec.md:12)`, [{ worktree, repo: context.repo }]),
    "`docs/duhem-spec.md:12`");
});

test("other host paths expose only a basename, including traversal and sibling roots", () => {
  assert.equal(rewritePublicPaths(`[private](/home/marvin/.claude/private.md) \`/tmp/log.txt\` ${worktree}-other/secret.txt ${worktree}/../private.txt`, [context]),
    "`private.md` `log.txt` `secret.txt` `private.txt`");
});

test("URLs and repository-relative paths survive, and model text is never evaluated", () => {
  const text = "See https://github.com/o/r/pull/1 and [file](src/main.ts), `src/main.ts`, $(touch /tmp/secret).";
  assert.equal(rewritePublicPaths(text, [context]),
    "See https://github.com/o/r/pull/1 and [file](src/main.ts), `src/main.ts`, $(touch `secret`).");
});

test("absolute paths in link labels are rewritten too, and unknown assignments expose no host root", () => {
  assert.equal(rewritePublicPaths(`[${worktree}/docs/spec.md](${worktree}/docs/spec.md)`, [context]),
    "[`docs/spec.md`](https://github.com/onsager-ai/duhem/blob/6ce1fb8/docs/spec.md)");
  assert.equal(rewritePublicPaths("[log](/home/operator/log.txt:12)", []), "`log.txt`");
});

test("Markdown targets with spaces and angle brackets are encoded as file URLs", () => {
  assert.equal(rewritePublicPaths(`[file](<${worktree}/docs/a file.md#L2>)`, [context]),
    "[file](https://github.com/onsager-ai/duhem/blob/6ce1fb8/docs/a%20file.md#L2)");
});
