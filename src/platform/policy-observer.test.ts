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
  record(false, "b");
  assert.deepEqual(openRules(), ["pr_green_ci", "two_failure_stop"]);
  record(true, "b");
  assert.deepEqual(openRules(), []);
  assert.equal(store.listT3ThreadPrs()[0]?.isDraft, false);
});
