import assert from "node:assert/strict";
import { test } from "node:test";
import type { TestContext } from "node:test";

import { readSpendSummary } from "./budget-observer.ts";
import { generateOwnerDigest } from "./digest.ts";
import { evaluatePolicyAndHealth } from "./policy-observer.ts";
import { PlatformStore } from "./store.ts";

const now = "2026-11-27T13:00:00Z";
const filter = { scopeKind: "team", scopeId: null, period: "weekly", source: "semon" } as const;
const policy = { ...filter, capUsd: 10, warnPct: 70, spendSource: "semon" } as const;

function fixture(t: TestContext) {
  const store = PlatformStore.open(":memory:", { now: () => now });
  t.after(() => store.close());
  const lane = store.createLane({ name: "Platform", slug: "platform" });
  const agent = store.upsertAgent({ id: "worker", label: "Worker", role: "worker", connectorKind: "t3", hostId: "east",
    laneId: lane.id, externalRef: "environment/thread", status: "active" });
  const addRun = (id: string, semonSessionUid: string, startedAt = "2026-11-27T12:00:00Z") => store.upsertRun({
    id, agentId: agent.id, laneId: lane.id, connectorKind: "t3", hostId: agent.hostId,
    status: "completed", externalRunRef: id, startedAt, semonSessionUid,
  });
  return { store, addRun, lane, agent };
}

test("budget summaries use UTC calendar windows and deduplicate session UIDs without mixing metrics", (t) => {
  const { store, addRun, lane, agent } = fixture(t);
  addRun("one", "uid-a", "2026-11-23T00:00:00Z");
  addRun("repeat", "uid-a");
  addRun("two", "uid-b");
  addRun("three", "uid-c");
  addRun("old", "outside", "2026-11-22T23:59:59Z");
  const save = (sessionUid: string, usdEst: number | null, sourceReportedUsd: number | null, billedUsd: number | null) =>
    store.upsertSpendSnapshot({ sessionUid, source: "semon", coverage: "complete", usdEst, sourceReportedUsd, billedUsd });
  save("uid-a", 7, 4, 3);
  save("uid-b", 6, 5, null);
  save("outside", 20, null, null);
  save("unlinked", 100, 100, 100);

  const summary = readSpendSummary(store, filter, now);
  assert.equal(summary.since, "2026-11-23T00:00:00.000Z");
  assert.equal(summary.sessionCount, 3);
  assert.equal(summary.snapshotCount, 2);
  assert.equal(summary.unpricedCount, 1);
  assert.equal(summary.incompleteCount, 1);
  assert.equal(summary.coverage, "partial");
  assert.equal(summary.sourceCoverage, "complete");
  assert.equal(summary.usdEst, 13);
  assert.equal(summary.sourceReportedUsd, 9);
  assert.equal(summary.billedUsd, 3);
  assert.equal(summary.preferredUsd, 8);
  assert.equal(summary.billedCount, 1);
  assert.equal(summary.reportedCount, 1);
  assert.equal(readSpendSummary(store, { ...filter, scopeKind: "lane", scopeId: lane.id }, now).preferredUsd, 8);
  assert.equal(readSpendSummary(store, { ...filter, scopeKind: "agent", scopeId: agent.id }, now).preferredUsd, 8);

  const daily = readSpendSummary(store, { ...filter, period: "daily" }, now);
  assert.equal(daily.billedUsd, null);
  assert.equal(daily.usdEst, 6);
  assert.equal(readSpendSummary(store, { ...filter, period: "monthly" }, now).usdEst, 33);
  const missing = readSpendSummary(store, { ...filter, source: "magpie" }, now);
  assert.equal(missing.status, "unavailable");
  assert.equal(missing.preferredUsd, null);
  assert.equal(missing.unpricedCount, 3);
});

test("budget warnings and breaches dedupe, survive incomplete recovery, and resolve on confirmed recovery or disable", (t) => {
  const { store, addRun } = fixture(t);
  addRun("one", "uid");
  store.upsertBudget(policy);
  const save = (usdEst: number | null) => store.upsertSpendSnapshot({ sessionUid: "uid", source: "semon",
    coverage: "complete", usdEst });
  const read = () => evaluatePolicyAndHealth(store, { now });
  assert.equal(read().events.length, 0);
  save(7);
  assert.deepEqual(read().events.map((event) => event.summary), ["team weekly budget warning reached (USD 7 of 7)"]);
  save(10);
  assert.deepEqual(read().events.map((event) => event.summary), ["team weekly budget cap reached (USD 10 of 10)"]);
  assert.equal(read().events.length, 0);
  const digest = generateOwnerDigest(store, { now, window: "weekly" });
  assert.match(digest.markdown, /Estimated USD 10 · Reported USD unknown · Billed USD unknown · Coverage complete/);
  assert.equal(digest.sections.policyAlerts.length, 2);
  save(null);
  assert.equal(read().policyAlerts.length, 2);
  save(8);
  assert.equal(read().policyAlerts.length, 1);
  save(1);
  assert.equal(read().policyAlerts.length, 0);
  save(11);
  assert.equal(read().events.length, 2);
  store.upsertBudget({ ...policy, enabled: false });
  assert.equal(read().policyAlerts.length, 0);
});
