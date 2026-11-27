import assert from "node:assert/strict";
import { test } from "node:test";

import { generateOwnerDigest } from "./digest.ts";
import { evaluatePolicyAndHealth } from "./policy-observer.ts";
import { PlatformStore } from "./store.ts";

const now = "2026-11-27T13:00:00.000Z";
const earlier = "2026-11-27T12:00:00.000Z";

test("observes failures and authority mismatches once, reports health, and generates deterministic digests", () => {
  const store = PlatformStore.open(":memory:", { now: () => now });
  try {
    const lane = store.createLane({ name: "Delivery", slug: "delivery" });
    store.setGoal({ laneId: lane.id, statement: "Ship updates weekly", metricName: "Releases", metricCurrent: 1,
      metricTarget: 2, weeklyPlan: ["Ship the API"] });
    const decision = (id: string, category: "owner_action" | "spending" | "routine", authority: "owner" | "chief_pm",
      urgency: "incident" | "digest" | "blocking_today") => store.createDecision({
      id, category, authority, urgency, laneId: lane.id, requestedBy: "Manager",
      title: id, recommendation: "Proceed", evidence: { required: true },
    });
    decision("owner-digest", "owner_action", "owner", "digest");
    decision("owner-now", "owner_action", "owner", "incident");
    const mismatch = decision("bad-routing", "spending", "chief_pm", "blocking_today");
    decision("routine", "routine", "chief_pm", "digest");
    const hostId = "east";
    store.upsertT3HostState({ hostId, label: "East", mcpUrl: "https://t3.example/mcp", originNamespace: null,
      connectionState: "stale", tokenExpiresAt: "2026-12-04T13:00:00.000Z" });
    store.recordT3Wait({ hostId, threadId: "thread", requestId: "q", kind: "user_input", questions: [{ question: "Continue?" }], occurredAt: earlier });
    store.recordT3Wait({ hostId, threadId: "thread", requestId: "a", kind: "approval", questions: null, occurredAt: earlier });
    store.recordT3PrWake({ hostId, threadId: "thread", prHost: "forge.example", repository: "org/repo", number: 4,
      outcome: "completed", summary: "CI passed", occurredAt: earlier, watchEnded: true });
    const agent = store.upsertAgent({ id: "worker", label: "Worker", role: "worker", connectorKind: "t3", hostId,
      laneId: lane.id, externalRef: "environment/thread", status: "active", lastSeenAt: earlier });
    const run = { agentId: agent.id, laneId: lane.id, hostId, connectorKind: "t3" as const, startedAt: earlier };
    store.upsertRun({ ...run, id: "fail-a", externalRunRef: "a", status: "failed", completedAt: "2026-11-27T12:05:00.000Z" });
    store.upsertRun({ ...run, id: "fail-b", externalRunRef: "b", status: "failed", completedAt: "2026-11-27T12:10:00.000Z" });
    store.upsertRun({ ...run, id: "success", externalRunRef: "success", status: "completed",
      startedAt: "2026-11-27T12:11:00.000Z", completedAt: "2026-11-27T12:12:00.000Z" });
    store.upsertRun({ ...run, id: "stall", externalRunRef: "c", status: "running", failureCount: 2 });
    store.recordFriction({ kind: "owner_intervention", summary: "Owner clarified the scope" });

    const options = { now };
    const observed = evaluatePolicyAndHealth(store, options);
    assert.deepEqual(observed.events.map((event) => event.ruleCode), ["two_failure_stop", "two_failure_stop", "authority_routing"]);
    assert.equal(observed.stalledRuns[0]?.id, "stall");
    assert.equal(observed.unhealthyHosts[0]?.hostId, hostId);
    assert.equal(observed.tokenExpiryWarnings[0]?.remainingMs, 7 * 86_400_000);
    assert.equal(observed.waits.length, 2);
    assert(store.hasOpenPolicyEvent("authority_routing", mismatch.id));
    assert(store.hasOpenPolicyEvent("two_failure_stop", "streak:worker:fail-b"));
    assert.deepEqual(observed.events[1]?.detail, { agentId: "worker", runIds: ["fail-a", "fail-b"] });
    assert.deepEqual(evaluatePolicyAndHealth(store, options).events, []);
    assert.equal(store.listFriction({ status: "open" }).length, 3);
    evaluatePolicyAndHealth(store, { now, markStalled: true });
    assert.equal(store.listRuns().find((item) => item.id === "stall")?.status, "stalled");

    const digest = generateOwnerDigest(store, { window: "weekly", now });
    assert.deepEqual(digest, generateOwnerDigest(store, { window: "weekly", now }));
    assert.deepEqual(digest.sections.decisions.slice(0, 2).map((item) => item.id), ["owner-now", "bad-routing"]);
    assert.equal(digest.sections.openFrictionCount, 3);
    assert.equal(digest.sections.interventionsByDay.length, 7);
    assert.deepEqual(digest.sections.interventionsByDay.at(-1), { day: "2026-11-27", count: 1 });
    assert.equal(digest.sections.spend.status, "unavailable");
    assert.equal(digest.sections.spend.coverage, "unknown");
    for (const entry of ["owner-now", "Ship updates weekly", "unapproved", "stalled", "authority_routing", "two_failure_stop",
      "Open friction items: 3", "Spend: unavailable", "Continue?", "CI passed"]) assert(digest.markdown.includes(entry), entry);

    store.expireDecision(mismatch.id, { actor: "Manager" });
    evaluatePolicyAndHealth(store, options);
    assert.equal(store.hasOpenPolicyEvent("authority_routing", mismatch.id), false);
  } finally {
    store.close();
  }
});
