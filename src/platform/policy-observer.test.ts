import assert from "node:assert/strict";
import { test } from "node:test";

import { evaluatePolicyAndHealth } from "./policy-observer.ts";
import { PlatformStore } from "./store.ts";

test("records one two_failure_stop event per contiguous streak", () => {
  const now = "2026-11-27T13:00:00.000Z";
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

    record("completed", 4, "completed");
    record("f4", 5, "failed");
    record("f5", 6, "failed");
    const second = evaluatePolicyAndHealth(store, { now });
    assert.deepEqual(second.events.map((event) => event.detail), [{ agentId: agent.id, runIds: ["f4", "f5"] }]);
    assert.equal(store.listOpenPolicyEvents().length, 2);
    assert(store.hasOpenPolicyEvent("two_failure_stop", "streak:worker:f5"));
    assert.deepEqual(evaluatePolicyAndHealth(store, { now }).events, []);
    assert.equal(store.listFriction({ status: "open" }).length, 2);
  } finally {
    store.close();
  }
});
