import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parseRunResult, parseThreadShow, T3OutputError } from "./cli-json.ts";

// Real captures from the first paid run (2026-11-09, claude-haiku-5-5, t3 v0.1.7).
const fixture = (name: string) => readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8");
const threadId = "mcp:bde2f85c-59db-4aa8-9c6a-17b3791c4eee";
const run = (ordinal: number) => `run:thread:mcp%3Abde2f85c-59db-4aa8-9c6a-17b3791c4eee:ordinal:${ordinal}`;

test("reads `thread start --wait --json`", () => {
  assert.deepEqual(parseRunResult(fixture("start.json")), {
    threadId,
    runId: run(1),
    status: "completed",
    reply: "READY",
    error: null,
  });
});

test("reads `thread send --wait --json`", () => {
  assert.deepEqual(parseRunResult(fixture("send.json")), {
    threadId,
    runId: run(2),
    status: "completed",
    reply: "AGAIN",
    error: null,
  });
});

test("reads `thread show --json`", () => {
  const shown = parseThreadShow(fixture("show.json"));
  assert.deepEqual(shown.thread, {
    threadId,
    status: "completed",
    activeRunId: null,
    settled: false,
    pendingRequestCount: 0,
    branch: "spike/t3-start",
    worktreePath: "/home/marvin/.t3/worktrees/ostrom/spike-t3-start",
    linkedPullRequest: null,
    pullRequests: [],
  });
  assert.deepEqual(
    shown.messages.map(({ role, runId, text }) => ({ role, runId, text })),
    [
      {
        role: "user",
        runId: run(2),
        text: "Follow-up test: reply with the single word AGAIN. Do not edit files or run commands.",
      },
      { role: "assistant", runId: run(2), text: "AGAIN" },
    ],
  );
});

test("a missing field is an error naming its path", () => {
  const value = JSON.parse(fixture("show.json"));
  delete value.thread.pendingRequestCount;
  assert.throws(() => parseThreadShow(JSON.stringify(value)), {
    name: "T3OutputError",
    message: /^\$\.thread\.pendingRequestCount: expected a non-negative integer, got nothing$/,
  });
});

test("an unknown status is an error, not a guess", () => {
  const value = { ...JSON.parse(fixture("start.json")), status: "paused" };
  assert.throws(() => parseRunResult(JSON.stringify(value)), /^T3OutputError: \$\.status: expected one of /);
});

test("`start --wait` without a run prints the launch result, which is not a run result", () => {
  // Upstream prints {threadId, projectId, runId} when the launch returned no run id.
  const launched = JSON.stringify({ threadId, projectId: "mcp:project", runId: null });
  assert.throws(() => parseRunResult(launched), /\$\.runId: expected a string, got null/);
});

test("output that is not JSON is an error", () => {
  assert.throws(() => parseRunResult("Thread mcp:x\nREADY\n"), T3OutputError);
});
