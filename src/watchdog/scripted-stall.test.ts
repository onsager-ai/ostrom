import assert from "node:assert/strict";
import { test } from "node:test";

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
  } finally {
    store.close();
  }
});
