import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { renderStatusCard, type StatusCard, type StatusCardState } from "./status-card.ts";
import { Store } from "../store/store.ts";
import { queueStatusCard } from "./cards.ts";

const base: Omit<StatusCard, "state"> = {
  plan: ["Inspect the failure", "Patch and verify the fix"],
  pullRequest: "https://github.com/onsager-ai/ostrom/pull/42",
  sessionUrl: "https://semon.onsager.ai/session/by-native/codex/session-1",
  harness: "codex",
  model: "gpt-6-luna",
  effort: "medium",
  selectionReason: "Cheapest suitable model. Moderate effort for the first work turn.",
  lastActivityAt: new Date("2026-11-09T12:13:00Z"),
  now: new Date("2026-11-09T12:18:59Z"),
  markerId: "card-thread-7",
};

for (const state of ["working", "blocked", "recovering", "paused", "stuck", "waiting_to_merge", "waiting_for_human_merge", "done", "stopped"] as const) {
  test(`status card snapshot: ${state}`, async () => {
    assert.equal(renderStatusCard({ ...base, state }), await snapshot(state));
  });
}

test("status card derives pending effort and model overrides from ticket selection history", () => {
  const store = Store.open(":memory:", { now: () => new Date("2026-11-09T12:00:00Z") });
  try {
    const source = store.ensureSource("github", "org/repo");
    const threadId = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://github.com/org/repo/issues/7" });
    const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: "accepted", threadId, kind: "issue_comment", actor: "alice", url: "https://github.com/org/repo/issues/7#1", occurredAt: new Date() });
    const ticketId = store.recordDecision({ threadId, triggerIds: [trigger.id], verdict: "accept", reason: "work" }).ticketId!;
    const assignmentId = store.createAssignment(ticketId);
    store.configureNativeAssignment(assignmentId, { harness: "codex", model: "luna", worktree: "/wt", branch: "b", machine: "host" });
    store.startNativeAssignment(assignmentId, "session");
    store.startWorkerTurn(assignmentId, "first_work", "medium", "router chose medium");
    store.finishWorkerTurn(assignmentId, "completed", null);
    store.selections.record(ticketId, "effort", "medium", "Router selected medium.", "router", {});
    store.selections.record(ticketId, "effort", "high", "Effort override: high.", "override", {});
    store.selections.record(ticketId, "model", "codex/luna", "Current model.", "router", {});
    store.selections.record(ticketId, "model", "codex/sol", "Model override: codex/sol.", "override", {});
    queueStatusCard(store, ticketId);
    assert.match(store.statusCardBody(threadId)!, /Model: `luna` \(sol from the next session\)/);
    assert.match(store.statusCardBody(threadId)!, /Effort: `medium` \(high from the next turn\)/);
  } finally { store.close(); }
});

test("status card omits the session link when session identifiers are missing", () => {
  const store = Store.open(":memory:");
  try {
    const source = store.ensureSource("github", "org/repo");
    const threadId = store.ensureThread({ sourceId: source.id, kind: "issue", number: 8, url: "https://github.com/org/repo/issues/8" });
    const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: "accepted", threadId, kind: "issue_comment", actor: "alice", url: "https://github.com/org/repo/issues/8#1", occurredAt: new Date() });
    const ticketId = store.recordDecision({ threadId, triggerIds: [trigger.id], verdict: "accept", reason: "work" }).ticketId!;
    const assignmentId = store.createAssignment(ticketId);
    store.configureNativeAssignment(assignmentId, { harness: "codex", model: "luna", worktree: "/wt", branch: "b", machine: "host" });
    queueStatusCard(store, ticketId, new Date(), "https://semon.example");
    assert.doesNotMatch(store.statusCardBody(threadId)!, /Session:/);
  } finally { store.close(); }
});

test("status card links the latest native session when configured", () => {
  const store = Store.open(":memory:");
  try {
    const source = store.ensureSource("github", "org/repo");
    const threadId = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://github.com/org/repo/issues/7" });
    const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: "accepted", threadId, kind: "issue_comment", actor: "alice", url: "https://github.com/org/repo/issues/7#1", occurredAt: new Date() });
    const ticketId = store.recordDecision({ threadId, triggerIds: [trigger.id], verdict: "accept", reason: "work" }).ticketId!;
    const assignmentId = store.createAssignment(ticketId);
    store.configureNativeAssignment(assignmentId, { harness: "claude", model: "sonnet", worktree: "/wt", branch: "b", machine: "host" });
    store.startNativeAssignment(assignmentId, "a session");
    queueStatusCard(store, ticketId, new Date(), "https://semon.example/");
    assert.match(store.statusCardBody(threadId)!, /Session: \[View in Semon\]\(https:\/\/semon\.example\/session\/by-native\/claude\/a%20session\)/);
    queueStatusCard(store, ticketId, new Date(), "");
    assert.doesNotMatch(store.statusCardBody(threadId)!, /Session:/);
  } finally { store.close(); }
});

test("status card shows pending effort and model overrides", () => {
  const rendered = renderStatusCard({ ...base, state: "working", pendingEffort: "high", pendingModel: "sol" });
  assert.match(rendered, /Model: `gpt-6-luna` \(sol from the next session\)/);
  assert.match(rendered, /Effort: `medium` \(high from the next turn\)/);
});

test("status card omits absent pending overrides", () => {
  const rendered = renderStatusCard({ ...base, state: "working" });
  assert.doesNotMatch(rendered, /from the next (turn|session)/);
});

test("status card shows stale approvals instead of approval satisfaction", () => {
  const rendered = renderStatusCard({ ...base, state: "waiting_to_merge", mergeStatus: "policy approvals; head 53975b6; Approved on 0e65021; the head is now 53975b6. Approve again to merge." });
  assert.ok(rendered.includes("Approved on 0e65021; the head is now 53975b6. Approve again to merge."));
  assert.doesNotMatch(rendered, /Approved, waiting to merge/);
});

test("status card snapshot: paused while waiting for a human merge", async () => {
  assert.equal(
    renderStatusCard({ ...base, state: "paused", mergeStatus: "policy human; head abc123; waiting for a human merge" }),
    await readFile(new URL("./__snapshots__/status-card-paused-human-merge.md", import.meta.url), "utf8"),
  );
});

test("status card snapshot: budget warning and budget pause", async () => {
  assert.equal(
    renderStatusCard({ ...base, state: "paused", budgetWarning: "tokens", pauseCause: "budget reached: tokens" }),
    await budgetSnapshot(),
  );
});

async function snapshot(state: StatusCardState): Promise<string> {
  return readFile(new URL(`./__snapshots__/status-card-${state}.md`, import.meta.url), "utf8");
}

async function budgetSnapshot(): Promise<string> {
  return readFile(new URL("./__snapshots__/status-card-budget-paused.md", import.meta.url), "utf8");
}
