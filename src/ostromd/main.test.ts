import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

test("ostromd exits non-zero while nothing is implemented", () => {
  const entry = fileURLToPath(new URL("./main.ts", import.meta.url));
  const run = spawnSync(process.execPath, [entry], { encoding: "utf8" });
  assert.equal(run.status, 1);
  assert.match(run.stderr, /not implemented/);
});
