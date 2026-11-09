import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

const snapshot = (name: string) => readFile(new URL(`./__snapshots__/${name}.md`, import.meta.url), "utf8");

test("batched comment snapshot", async () => {
  assert.equal(
    ["I’ve opened the pull request.", "The first check is failing; I’m investigating."].join("\n\n---\n\n") + "\n",
    await snapshot("batched-comment"),
  );
});

test("one reply per review snapshot", async () => {
  assert.equal(
    "Thanks for the review. I’ll address the two comments together in the next update.\n",
    await snapshot("review-reply"),
  );
});
