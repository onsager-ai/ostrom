import assert from "node:assert/strict";
import { mkdtempSync, rmSync, statSync } from "node:fs";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { Store } from "../store/store.ts";
import { defaultSocketPath, localApi } from "./server.ts";

function get(socketPath: string, path: string): Promise<{ status: number; body: unknown }> {
  return new Promise((resolve, reject) => {
    const client = request({ socketPath, path, method: "GET" }, (response) => {
      let text = "";
      response.setEncoding("utf8");
      response.on("data", (chunk: string) => { text += chunk; });
      response.on("end", () => resolve({ status: response.statusCode!, body: JSON.parse(text) }));
    });
    client.once("error", reject);
    client.end();
  });
}

function fixture(path: string): number {
  const store = Store.open(path);
  try {
    const source = store.ensureSource("github", "onsager-ai/ostrom");
    const thread = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://example.test/issues/7" });
    const trigger = store.insertTrigger({
      sourceId: source.id, sourceEventId: "event-7", threadId: thread, kind: "issue_comment", actor: "alice",
      url: "https://example.test/issues/7#comment", occurredAt: new Date(), body: "please fix it",
    }).id;
    const ticket = store.recordDecision({ threadId: thread, triggerIds: [trigger], verdict: "accept", reason: "requested fix" }).ticketId!;
    const assignment = store.createAssignment(ticket);
    store.configureNativeAssignment(assignment, { harness: "codex", model: "gpt-6-luna", worktree: "/work/ticket-1", branch: "ostrom/ticket-1-1", machine: "host" });
    store.startNativeAssignment(assignment, "sess-1");
    store.beginTurn(assignment);
    store.endTurn(assignment, { inputTokens: 12, cachedInputTokens: 3, outputTokens: 4 });
    store.enqueueReply({ id: "reply-1", threadId: thread, body: "Taking this." });
    return ticket;
  } finally {
    store.close();
  }
}

test("the local API serves status, tickets and a ticket over a 0600 Unix socket", async () => {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-api-"));
  const database = join(directory, "ostrom.sqlite");
  const socketPath = join(directory, "run", "ostromd.sock");
  const ticketId = fixture(database);
  const store = Store.open(database);
  const api = localApi(store, socketPath);
  try {
    await api.start();
    assert.equal(statSync(socketPath).mode & 0o777, 0o600);

    const status = await get(socketPath, "/v1/status");
    assert.equal(status.status, 200);
    const statusBody = status.body as {
      tickets: { open: number; paused: number; ended: number };
      runningWorkers: { sessionId: string; inputTokens: number; outputTokens: number; turns: number }[];
      stuckTickets: unknown[];
      tokensToday: number;
    };
    assert.deepEqual(statusBody.tickets, { open: 1, paused: 0, ended: 0 });
    assert.deepEqual(statusBody.runningWorkers.map((worker) => ({
      sessionId: worker.sessionId, turns: worker.turns, inputTokens: worker.inputTokens, outputTokens: worker.outputTokens,
    })), [{ sessionId: "sess-1", turns: 1, inputTokens: 12, outputTokens: 4 }]);
    assert.deepEqual(statusBody.stuckTickets, []);
    assert.equal(statusBody.tokensToday, 16);

    const tickets = await get(socketPath, "/v1/tickets");
    assert.equal(tickets.status, 200);
    const list = tickets.body as { id: number; assignment: { sessionId: string } }[];
    assert.equal(list.length, 1);
    assert.equal(list[0]!.id, ticketId);
    assert.equal(list[0]!.assignment.sessionId, "sess-1");

    const detail = await get(socketPath, `/v1/tickets/${ticketId}`);
    assert.equal(detail.status, 200);
    const shown = detail.body as { context: { reason: string }; triggers: { body: string }[]; outbox: { id: string }[] };
    assert.equal(shown.context.reason, "requested fix");
    assert.equal(shown.triggers[0]!.body, "please fix it");
    assert.equal(shown.outbox[0]!.id, "reply-1");
  } finally {
    await api.close();
    store.close();
    rmSync(directory, { recursive: true, force: true });
  }
});

test("socket path uses XDG runtime and otherwise local state", () => {
  assert.equal(defaultSocketPath({ XDG_RUNTIME_DIR: "/run/user/1000" }), "/run/user/1000/ostrom/ostromd.sock");
  assert.equal(defaultSocketPath({ HOME: "/home/alice" }), "/home/alice/.local/state/ostrom/ostromd.sock");
});
