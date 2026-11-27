import assert from "node:assert/strict";
import { test } from "node:test";

import { pollGitHubPrs, readGitHubPr } from "./github-observer.ts";
import { evaluatePolicyAndHealth } from "./policy-observer.ts";
import { PlatformStore } from "./store.ts";

const now = "2026-11-27T13:00:00.000Z";
const sha = "a".repeat(40);

test("PR CI reads the exact head, counts the latest checks and statuses, and waits for completion", async () => {
  const paths: string[] = [];
  const observation = await readGitHubPr("org/repo", 42, async (path) => {
    paths.push(path);
    if (path.endsWith("/pulls/42")) return { state: "open", head: { sha } };
    if (path.includes("/check-runs")) return { total_count: 2, check_runs: [
      { status: "completed", conclusion: "timed_out" },
      { status: "completed", conclusion: "neutral" },
    ] };
    return { statuses: [{ state: "pending" }, { state: "error" }] };
  });
  assert.deepEqual(observation, { headSha: sha, rollupState: "pending", totalChecks: 4, failedChecks: 2 });
  assert.deepEqual(paths, [
    "repos/org/repo/pulls/42",
    `repos/org/repo/commits/${sha}/check-runs?filter=latest&per_page=100&page=1`,
    `repos/org/repo/commits/${sha}/status?per_page=100&page=1`,
  ]);
  await assert.rejects(readGitHubPr("org/..", 42, async () => assert.fail("unsafe route")), /invalid GitHub PR/);
});

test("pages checks within a fixed limit", async () => {
  let calls = 0;
  await assert.rejects(readGitHubPr("org/repo", 1, async (path) => {
    if (path.endsWith("/pulls/1")) return { state: "open", head: { sha } };
    calls++;
    return { total_count: 501, check_runs: Array.from({ length: 100 }, () => ({ status: "completed", conclusion: "success" })) };
  }), /page limit/);
  assert.equal(calls, 5);
});

test("deduplicates watched PRs and creates one alert per failing streak until green", async (t) => {
  const store = PlatformStore.open(":memory:", { now: () => now });
  t.after(() => store.close());
  for (const [hostId, repository] of [["east", "Org/Repo"], ["west", "org/repo"]] as const) {
    store.upsertT3HostState({ hostId, label: hostId, mcpUrl: "https://t3.example/mcp",
      originNamespace: null, connectionState: "connected" });
    store.recordT3PrWake({ hostId, repository, threadId: "thread", prHost: "github.com", number: 42,
      outcome: "check", summary: "Checking", occurredAt: now, watchEnded: false });
  }
  store.recordT3PrWake({ hostId: "east", repository: "org/other", threadId: "thread", prHost: "forge.example", number: 42,
    outcome: "check", summary: "Checking", occurredAt: now, watchEnded: false });

  let head = sha;
  let state = "failure";
  let prState = "open";
  let requests = 0;
  const tick = async () => {
    await pollGitHubPrs(store, async (path) => {
      if (path.endsWith("/pulls/42")) {
        requests++;
        return { state: prState, head: { sha: head } };
      }
      if (path.includes("check-runs")) return { check_runs: [] };
      return { statuses: [{ state }] };
    });
    return evaluatePolicyAndHealth(store, { now }).events.map((event) => event.ruleCode);
  };
  assert.deepEqual(await tick(), ["pr_green_ci"]);
  assert.equal(requests, 1);
  assert.deepEqual(await tick(), []);
  assert.equal(store.listGitHubPrChecks()[0]?.consecutiveFailures, 1);
  assert.equal(Object.getPrototypeOf(store.listGitHubPrChecks()[0]), Object.prototype);

  head = "b".repeat(40);
  state = "pending";
  assert.deepEqual(await tick(), []);
  assert.equal(store.listOpenPolicyEvents().length, 1);
  state = "failure";
  assert.deepEqual(await tick(), ["two_failure_stop"]);
  head = "c".repeat(40);
  assert.deepEqual(await tick(), []);
  head = sha;
  assert.deepEqual(await tick(), []);
  assert.equal(store.listGitHubPrChecks()[0]?.consecutiveFailures, 3);

  await pollGitHubPrs(store, async () => { throw new Error("sensitive stderr"); });
  assert.deepEqual(evaluatePolicyAndHealth(store, { now }).events, []);
  const error = store.listGitHubPrChecks()[0];
  assert.equal(error?.lastError, "GitHub PR checks unavailable");
  assert.equal(error?.consecutiveFailures, 3);

  state = "success";
  assert.deepEqual(await tick(), []);
  assert.equal(store.listGitHubPrChecks()[0]?.consecutiveFailures, 0);
  assert.equal(store.listOpenPolicyEvents().length, 0);
  state = "failure";
  assert.deepEqual(await tick(), ["pr_green_ci"]);
  head = "b".repeat(40);
  assert.deepEqual(await tick(), ["two_failure_stop"]);
  assert.equal(store.listGitHubPrChecks()[0]?.failureStreak, 2);
  prState = "closed";
  assert.deepEqual(await tick(), []);
  assert.equal(store.listOpenPolicyEvents().length, 0);
});
