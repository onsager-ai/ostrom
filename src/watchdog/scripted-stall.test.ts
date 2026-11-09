import assert from "node:assert/strict";
import { test } from "node:test";

import { loadBudgetDefaults } from "../budget/budgets.ts";
import { Store } from "../store/store.ts";
import { STALL_BRIEF, seedScriptedStall } from "./scripted-stall.ts";

test("scripted stall only seeds an accepted ticket for daemon dispatch", () => {
  const store = Store.open(":memory:");
  try {
    const ticketId = seedScriptedStall(store, "onsager-ai/ostrom", "alice", 7);
    assert.deepEqual(store.undispatchedTickets(), [ticketId]);
    assert.equal(store.latestAssignment(ticketId), null);
    assert.equal(store.ticketContext(ticketId).reason, STALL_BRIEF);
    assert.match(STALL_BRIEF, /sleep 3600/);
    assert.match(STALL_BRIEF, /red or blue/);
    assert.match(STALL_BRIEF, /never commit, push or open a pull request/);
    assert.match(STALL_BRIEF, /run `sleep 3600` again/);
  } finally {
    store.close();
  }
});

test("the seeded ticket takes its budget from the environment, as a Router-accepted one does", () => {
  const store = Store.open(":memory:");
  try {
    const ticketId = seedScriptedStall(store, "onsager-ai/ostrom", "alice", 7, loadBudgetDefaults({ OSTROM_BUDGET_IDLE_MIN: "3" }));
    assert.equal(store.ticketBudget(ticketId).idleMinutes, 3);
  } finally {
    store.close();
  }
});
