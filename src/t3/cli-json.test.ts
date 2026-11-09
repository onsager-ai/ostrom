import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parseLaunchResult, parseRunResult, parseThreadShow, T3OutputError } from "./cli-json.ts";

// Real captures from the first paid runs (2026-11-09, claude-haiku-5-5, t3 v0.1.7). show.json is
// redacted: the home directory in worktreePath and the app.t3.codes link are placeholders.
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
    worktreePath: "/home/user/.t3/worktrees/ostrom/spike-t3-start",
    linkedPullRequest: null,
    pullRequests: [],
    updatedAt: "2026-11-09T04:02:18.386Z",
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

test("reads `thread start --json` without --wait", () => {
  assert.deepEqual(parseLaunchResult(fixture("launch.json")), {
    threadId: "mcp:c400630d-714d-4262-9616-09205df51320",
    projectId: "mcp:28193aba-d9d3-429f-9bb9-7d0e4262c7a3",
    runId: "run:thread:mcp%3Ac400630d-714d-4262-9616-09205df51320:ordinal:1",
  });
});

test("a pull request's link source is read, and an unknown one is an error", () => {
  const value = JSON.parse(fixture("show.json"));
  const pr = { host: "github.com", repository: "o/r", number: 3, url: "https://github.com/o/r/pull/3", state: "open" };
  value.thread.pullRequests = [{ ...pr, source: "created" }];
  assert.equal(parseThreadShow(JSON.stringify(value)).thread.pullRequests[0]?.source, "created");
  value.thread.pullRequests = [{ ...pr, source: "guessed" }];
  assert.throws(() => parseThreadShow(JSON.stringify(value)), /pullRequests\[0\]\.source: expected one of/);
});
