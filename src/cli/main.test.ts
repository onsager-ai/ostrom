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

test("show adds the Semon session link only when configured", async () => {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-cli-semon-"));
  const path = join(directory, "ostrom.db");
  const original = process.stdout.write;
  let output = "";
  try {
    const id = ticket(path);
    const store = Store.open(path);
    try {
      const assignment = store.createAssignment(id);
      store.configureNativeAssignment(assignment, { harness: "codex", model: "luna", worktree: "/w", branch: "b", machine: "h" });
      store.startNativeAssignment(assignment, "session !'()* /?");
    } finally { store.close(); }
    process.stdout.write = ((chunk: string) => { output += chunk; return true; }) as typeof process.stdout.write;
    assert.equal(await main(["show", String(id)], { OSTROM_DB: path, HOME: directory }), 0);
    assert.doesNotMatch(output, /Session: https:\/\/semon/);
    output = "";
    assert.equal(await main(["show", String(id)], { OSTROM_DB: path, HOME: directory, OSTROM_SEMON_URL: "https://semon.example/" }), 0);
    assert.match(output, /Session: https:\/\/semon\.example\/session\/by-native\/codex\/session%20%21%27%28%29%2A%20%2F%3F/);
    output = "";
    assert.equal(await main(["show", String(id), "--json"], { OSTROM_DB: path, HOME: directory, OSTROM_SEMON_URL: "https://semon.example" }), 0);
    assert.equal((JSON.parse(output) as { sessionUrl: string }).sessionUrl, "https://semon.example/session/by-native/codex/session%20%21%27%28%29%2A%20%2F%3F");
    output = "";
    assert.equal(await main(["show", String(id), "--json"], { OSTROM_DB: path, HOME: directory }), 0);
    assert.equal(Object.hasOwn(JSON.parse(output), "sessionUrl"), false);
  } finally {
    process.stdout.write = original;
    rmSync(directory, { recursive: true, force: true });
  }
});

test("show renders turn model, effort and kind in text and JSON", async () => {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-cli-turn-"));
  const path = join(directory, "ostrom.db");
  const original = process.stdout.write;
  let output = "";
  try {
    const id = ticket(path);
    const store = Store.open(path);
    try {
      const assignment = store.createAssignment(id);
      store.configureNativeAssignment(assignment, { harness: "codex", model: "luna", worktree: "/w", branch: "b", machine: "h" });
      store.startWorkerTurn(assignment, "dispatch", "low");
      store.finishWorkerTurn(assignment, "completed", null, { model: "luna", effort: "low" });
    } finally { store.close(); }
    process.stdout.write = ((chunk: string) => { output += chunk; return true; }) as typeof process.stdout.write;
    assert.equal(await main(["show", String(id)], { OSTROM_DB: path, HOME: directory }), 0);
    assert.match(output, /turn 1  dispatch  completed  model luna \(requested luna\)  effort low/);
    output = "";
    assert.equal(await main(["show", String(id), "--json"], { OSTROM_DB: path, HOME: directory }), 0);
    const view = JSON.parse(output) as { turns: { kind: string; observedModel: string; observedEffort: string }[] };
    assert.deepEqual(view.turns.map((turn) => [turn.kind, turn.observedModel, turn.observedEffort]), [["dispatch", "luna", "low"]]);
  } finally {
    process.stdout.write = original;
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
