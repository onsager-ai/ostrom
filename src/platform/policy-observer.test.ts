import assert from "node:assert/strict";
import { test } from "node:test";

import { evaluatePolicyAndHealth } from "./policy-observer.ts";
import { PlatformStore } from "./store.ts";
import type { RunStatus } from "./types.ts";

const now = "2026-11-27T13:00:00.000Z";

test("keeps only the trailing failure streak open", () => {
  const store = PlatformStore.open(":memory:", { now: () => now });
  try {
    const agent = store.upsertAgent({
      id: "worker", label: "Worker", role: "worker", connectorKind: "t3", hostId: "east",
      externalRef: "environment/thread", status: "idle",
    });
    const record = (id: string, minute: number, status: "failed" | "completed") => {
      const startedAt = new Date(Date.parse("2026-11-27T12:00:00.000Z") + minute * 60_000).toISOString();
      store.upsertRun({ id, agentId: agent.id, connectorKind: agent.connectorKind, hostId: agent.hostId,
        externalRunRef: id, status, startedAt });
    };

    record("f1", 1, "failed");
    record("f2", 2, "failed");
    record("f3", 3, "failed");
    const first = evaluatePolicyAndHealth(store, { now });
    assert.deepEqual(first.events.map((event) => event.detail), [{ agentId: agent.id, runIds: ["f1", "f2"] }]);
    assert.equal(store.listOpenPolicyEvents().length, 1);
    assert(store.hasOpenPolicyEvent("two_failure_stop", "streak:worker:f2"));
    assert.deepEqual(evaluatePolicyAndHealth(store, { now }).events, []);

    record("completed", 4, "completed");
    assert.deepEqual(evaluatePolicyAndHealth(store, { now }).policyAlerts, []);
    assert(!store.hasOpenPolicyEvent("two_failure_stop", "streak:worker:f2"));

    record("f4", 5, "failed");
    record("f5", 6, "failed");
    const second = evaluatePolicyAndHealth(store, { now });
    assert.deepEqual(second.events.map((event) => event.detail), [{ agentId: agent.id, runIds: ["f4", "f5"] }]);
    assert.equal(store.listOpenPolicyEvents().length, 1);
    assert(store.hasOpenPolicyEvent("two_failure_stop", "streak:worker:f5"));
    assert.deepEqual(evaluatePolicyAndHealth(store, { now }).events, []);
    assert.equal(store.listFriction({ status: "open" }).length, 2);
  } finally {
    store.close();
  }
});

test("alerts once for stale open goals and resolves when progress or lane status changes", (t) => {
  let clock = "2026-11-26T12:00:00.000Z";
  const store = PlatformStore.open(":memory:", { now: () => clock });
  t.after(() => store.close());
  const lane = (id: string, status: "active" | "paused" | "archived" = "active") =>
    store.createLane({ id, slug: id, name: `${id} lane`, status, priorityRank: 1 });
  const active = lane("stale");
  const paused = lane("paused", "paused");
  const archived = lane("archived", "archived");
  const pauseMe = lane("pause-me");
  const recent = lane("recent");
  const met = lane("met");
  const goal = (laneId: string, id: string, metricCurrent: number | null = 0) => store.setGoal({
    id, laneId, statement: "Make progress", metricName: "items", metricCurrent, metricTarget: 2,
  });
  const staleGoal = goal(active.id, "stale-goal");
  goal(paused.id, "paused-goal");
  goal(archived.id, "archived-goal");
  goal(pauseMe.id, "pause-me-goal");
  clock = "2026-11-26T13:00:00.000Z";
  goal(recent.id, "recent-goal");
  goal(met.id, "met-goal", 2);

  clock = "2026-11-27T11:59:59.000Z";
  assert.deepEqual(evaluatePolicyAndHealth(store, { now: clock }).events, []);
  clock = "2026-11-27T12:00:00.000Z";
  const exactlyStale = evaluatePolicyAndHealth(store, { now: clock });
  assert.deepEqual(exactlyStale.events.map(({ laneId, ruleCode }) => [laneId, ruleCode]).sort(), [
    [active.id, "goal_stale"], [pauseMe.id, "goal_stale"],
  ].sort());
  assert.equal(exactlyStale.events.find(({ laneId }) => laneId === active.id)?.summary,
    "stale lane goal stale (no progress for 24h+)");
  assert.deepEqual(evaluatePolicyAndHealth(store, { now: clock }).events, []);
  assert.equal(store.listOpenPolicyEvents().filter(({ ruleCode }) => ruleCode === "goal_stale").length, 2);

  clock = "2026-11-27T12:30:00.000Z";
  store.setGoal({ id: staleGoal.id, laneId: active.id, statement: "Make progress", metricName: "items",
    metricCurrent: 1, metricTarget: 2 });
  assert.equal(evaluatePolicyAndHealth(store, { now: clock }).policyAlerts.some(({ laneId, ruleCode }) =>
    ruleCode === "goal_stale" && laneId === active.id), false);

  assert.equal(evaluatePolicyAndHealth(store, { now: clock }).policyAlerts.filter(({ ruleCode }) => ruleCode === "goal_stale").length, 1);
  store.updateLane(pauseMe.slug, { status: "paused" });
  assert.equal(evaluatePolicyAndHealth(store, { now: clock }).policyAlerts.some(({ ruleCode }) => ruleCode === "goal_stale"), false);
});

test("completing a stale goal resolves its alert and a same-week replacement gets 24 hours", (t) => {
  let clock = "2026-11-23T12:00:00.000Z";
  const store = PlatformStore.open(":memory:", { now: () => clock });
  t.after(() => store.close());
  const lane = store.createLane({ slug: "releases", name: "Releases" });
  const input = { laneId: lane.id, statement: "Ship the release", metricName: "Releases", metricCurrent: 0,
    metricTarget: 1, weekStart: "2026-11-23" };
  const original = store.setGoal(input);
  const observe = () => evaluatePolicyAndHealth(store, { now: clock }).policyAlerts;
  const goalKey = (id: string) => `goal:${lane.id}:${id}`;

  clock = "2026-11-24T12:00:00.000Z";
  assert.equal(observe().length, 1);
  assert(store.hasOpenPolicyEvent("goal_stale", goalKey(original.id)));
  store.completeGoal(lane.id);
  assert(!store.hasOpenPolicyEvent("goal_stale", goalKey(original.id)));
  assert.deepEqual(observe(), []);
  assert.equal(store.listPolicyEvents({ ruleCode: "goal_stale" }).length, 1);

  clock = "2026-11-25T12:00:00.000Z";
  assert.deepEqual(observe(), []);
  const replacement = store.setGoal({ ...input, statement: "Ship the next release" });
  assert.deepEqual(observe(), []);
  clock = "2026-11-26T11:59:59.000Z";
  assert.deepEqual(observe(), []);
  clock = "2026-11-26T12:00:00.000Z";
  assert.equal(observe().length, 1);
  assert(store.hasOpenPolicyEvent("goal_stale", goalKey(replacement.id)));
  assert(!store.hasOpenPolicyEvent("goal_stale", goalKey(original.id)));
});

test("keeps repeated failures on live runs and the latest settled run", (t) => {
  const store = PlatformStore.open(":memory:", { now: () => now });
  t.after(() => store.close());
  const agent = store.upsertAgent({ id: "worker", label: "Worker", role: "worker", connectorKind: "t3",
    hostId: "east", externalRef: "environment/thread", status: "active" });
  const record = (id: string, minute: number, status: RunStatus, failureCount: number) => {
    const startedAt = `2026-11-27T12:0${minute}:00.000Z`;
    store.upsertRun({ id, agentId: agent.id, connectorKind: agent.connectorKind, hostId: agent.hostId,
      externalRunRef: id, status, startedAt, failureCount });
  };
  const openRunAlerts = () => evaluatePolicyAndHealth(store, { now }).policyAlerts.map((alert) => alert.summary).sort();

  record("older", 1, "failed", 2);
  assert.deepEqual(openRunAlerts(), ["Run older recorded 2 failures"]);
  record("latest", 2, "completed", 2);
  record("live", 3, "running", 2);
  assert.deepEqual(openRunAlerts(), ["Run latest recorded 2 failures", "Run live recorded 2 failures"]);
  record("recovered", 4, "completed", 0);
  assert.deepEqual(openRunAlerts(), ["Run live recorded 2 failures"]);
  record("live", 3, "completed", 2);
  assert.deepEqual(openRunAlerts(), []);
});

test("GitHub draft state overrides linked T3 metadata and clears PR alerts", (t) => {
  const store = PlatformStore.open(":memory:", { now: () => now });
  t.after(() => store.close());
  const hostId = "east";
  const repository = "org/repo";
  const number = 42;
  const lane = store.createLane({ slug: "repo", name: "Repo lane" });
  store.reconcileT3Host({ hostId, label: "East", mcpUrl: "https://t3.example/mcp", originNamespace: null,
    connectionState: "connected" }, [{
    threadId: "thread", runs: [], agent: { id: "manager", label: "Manager", role: "manager", connectorKind: "t3",
      hostId, externalRef: "environment/thread", status: "idle" },
    prs: [{ hostId, threadId: "thread", prHost: "github.com", repository, number,
      url: "https://github.com/org/repo/pull/42", source: "manual", state: "open", watching: true, title: null,
      headBranch: null, baseBranch: null, isDraft: false }],
  }], { observedAt: now });
  const record = (isDraft: boolean, head = "a") => store.recordGitHubPrCheck(repository, number, {
    isDraft, headSha: head.repeat(40), rollupState: "failure", totalChecks: 1, failedChecks: 1,
  });
  const openRules = () => evaluatePolicyAndHealth(store, { now }).policyAlerts.map((alert) => alert.ruleCode).sort();

  record(true);
  assert.deepEqual(openRules(), []);
  record(false);
  assert.deepEqual(openRules(), ["pr_green_ci"]);
  assert.equal(store.listOpenPolicyEvents()[0]?.summary, "Repo lane · org/repo#42: 1/1 checks failed");
  assert.equal(store.listOpenPolicyEvents()[0]?.laneId, lane.id);
  assert.equal((store.listOpenPolicyEvents()[0]?.detail as { laneSlug?: string }).laneSlug, "repo");
  record(false, "b");
  assert.deepEqual(openRules(), ["two_failure_stop"]);
  assert.equal(store.listOpenPolicyEvents()[0]?.summary,
    "Repo lane · org/repo#42: 1/1 checks failed (2 consecutive heads)");
  record(true, "b");
  assert.deepEqual(openRules(), []);
  assert.equal(store.listT3ThreadPrs()[0]?.isDraft, false);
});

test("refreshes open PR alert attribution and resolves it when lane attribution appears", (t) => {
  const store = PlatformStore.open(":memory:", { now: () => now });
  t.after(() => store.close());
  const repository = "org/crawlab-pro";
  store.reconcileT3Host({ hostId: "east", label: "East", mcpUrl: "https://t3.example/mcp", originNamespace: null,
    connectionState: "connected" }, [{
    threadId: "thread", runs: [], agent: { id: "manager", label: "Manager", role: "manager", connectorKind: "t3",
      hostId: "east", externalRef: "environment/thread", status: "idle" },
    prs: [{ hostId: "east", threadId: "thread", prHost: "github.com", repository, number: 9,
      url: "https://github.com/org/crawlab-pro/pull/9", source: "manual", state: "open", watching: true,
      title: null, headBranch: null, baseBranch: null, isDraft: false }],
  }], { observedAt: now });
  store.recordGitHubPrCheck(repository, 9, { isDraft: false, headSha: "a".repeat(40), rollupState: "error",
    totalChecks: 0, failedChecks: 0 });
  evaluatePolicyAndHealth(store, { now });
  assert.equal(store.listOpenPolicyEvents()[0]?.summary,
    "org/crawlab-pro#9: CI error: GitHub PR checks unavailable");

  const lane = store.createLane({ slug: "crawlab", name: "Crawlab" });
  evaluatePolicyAndHealth(store, { now });
  const alert = store.listOpenPolicyEvents()[0];
  assert.equal(alert?.laneId, lane.id);
  assert.equal(alert?.summary, "Crawlab · org/crawlab-pro#9: CI error: GitHub PR checks unavailable");
  assert.equal((alert?.detail as { laneSlug?: string }).laneSlug, "crawlab");
  assert.equal(store.listFriction({ status: "open" }).length, 1);
});
