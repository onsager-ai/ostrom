import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { Store } from "../store/store.ts";
import { main } from "./main.ts";

function ticket(path: string): number {
  const store = Store.open(path);
  try {
    const source = store.ensureSource("github", "onsager-ai/ostrom");
    const thread = store.ensureThread({ sourceId: source.id, kind: "issue", number: 1, url: "https://example.test/1" });
    const trigger = store.insertTrigger({
      sourceId: source.id, sourceEventId: "event-1", threadId: thread, kind: "issue_comment", actor: "alice",
      url: "https://example.test/1#comment", occurredAt: new Date(),
    }).id;
    return store.recordDecision({ threadId: thread, triggerIds: [trigger], verdict: "accept", reason: "work" }).ticketId!;
  } finally {
    store.close();
  }
}

test("extend raises stored limits and resumes a budget-paused ticket", async () => {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-cli-"));
  const path = join(directory, "ostrom.db");
  try {
    const id = ticket(path);
    const store = Store.open(path);
    store.pauseTicketForBudget(id, "turns");
    store.close();
    assert.equal(await main(["extend", String(id), "--time", "60", "--turns", "2", "--tokens", "100"], { OSTROM_DB: path }), 0);
    const extended = Store.open(path);
    try {
      assert.equal(extended.getTicket(id).state, "open");
      const budget = extended.ticketBudget(id);
      assert.deepEqual([budget.timeMinutes, budget.turnLimit, budget.tokenLimit], [540, 82, 1_000_100]);
    } finally {
      extended.close();
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
