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

test("read-only CLI falls back to the store", async () => {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-cli-"));
  const path = join(directory, "ostrom.db");
  try {
    const id = ticket(path);
    assert.equal(await main(["tickets", "--json"], { OSTROM_DB: path, HOME: directory }), 0);
    assert.equal(await main(["show", String(id), "--json"], { OSTROM_DB: path, HOME: directory }), 0);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("mutating CLI commands require a running daemon", async () => {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-cli-"));
  const path = join(directory, "ostrom.db");
  try {
    const id = ticket(path);
    assert.equal(await main(["extend", String(id), "--time", "60"], { OSTROM_DB: path, HOME: directory }), 1);
    const unchanged = Store.open(path);
    try {
      assert.equal(unchanged.ticketBudget(id).timeMinutes, 480);
    } finally {
      unchanged.close();
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
