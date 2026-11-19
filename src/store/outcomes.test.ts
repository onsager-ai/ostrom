import assert from "node:assert/strict";
import { test } from "node:test";
import { Store } from "./store.ts";

test("outcomes aggregate ticket totals before medians, partition repositories/kinds/models, and retain failures", () => {
  const store = Store.open(":memory:");
  let number = 0;
  const ticket = (repository = "o/r", model = "small", harness: "claude" | "codex" = "codex") => {
    const source = store.ensureSource("github", repository);
    const threadId = store.ensureThread({ sourceId: source.id, kind: "issue", number: ++number, url: "https://example.test/issue" });
    const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: String(number), threadId, kind: "issue_comment", actor: "operator", url: "https://example.test/comment", occurredAt: new Date() }).id;
    const ticketId = store.recordDecision({ threadId, triggerIds: [trigger], verdict: "accept", reason: "work" }).ticketId!;
    const assignment = () => {
      const id = store.createAssignment(ticketId);
      store.configureNativeAssignment(id, { harness, model, worktree: "/tmp/work", branch: "work", machine: "test" });
      store.startNativeAssignment(id, `session-${id}`);
      return id;
    };
    return { ticketId, threadId, sourceId: source.id, assignment, id: assignment() };
  };
  const turn = (id: number, kind: string, fresh: number, status: "completed" | "failed" = "completed") => {
    store.startWorkerTurn(id, kind);
    store.finishWorkerTurn(id, status, { inputTokens: fresh, cachedInputTokens: 100, outputTokens: 10 });
  };
  try {
    assert.deepEqual(store.modelOutcomes(), []);
    const first = ticket();
    turn(first.id, "dispatch", 10);
    turn(first.id, "review", 20);
    // Multiple assignments and merge candidates must not duplicate ticket samples or merge counts.
    store.endAssignment(first.id);
    const resumed = first.assignment();
    turn(resumed, "review", 30);
    for (const [id, prNumber] of [[first.id, 100], [resumed, 101]] as const) {
      store.upsertMergeCandidate({ assignmentId: id, repository: "o/r", prNumber, headSha: "head", baseSha: "base", policy: "test", activityFingerprint: null, activityAt: null, readyReplyId: null, readySentAt: null, quietDeadline: null, blockingReason: null });
    }
    store.recordMergeAndOutcome({ repository: "o/r", prNumber: 101, headSha: "head", mergeSha: "merge", mergedAt: new Date(), mergedByLogin: null, mergedById: null, origin: "external", assignmentId: resumed, threadId: first.threadId, body: "merged", evidence: "test" });
    store.unsubscribeTicket(first.ticketId);
    const second = ticket();
    turn(second.id, "dispatch", 100, "failed");
    const third = ticket();
    store.startWorkerTurn(third.id, "dispatch"); // Started, even while usage is not yet known.
    ticket(); // Configured assignment without a turn is not a started sample.
    const otherKind = ticket();
    turn(otherKind.id, "recovery", 5);
    turn(ticket("other/repo").id, "dispatch", 200);
    turn(ticket("o/r", "large").id, "dispatch", 300);
    turn(ticket("o/r", "small", "claude").id, "dispatch", 400);
    const outcomes = store.modelOutcomes("o/r");
    assert.equal(outcomes.length, 4);
    assert.deepEqual(outcomes.find((row) => row.kind === "dispatch" && row.model === "small" && row.harness === "codex"), {
      repository: "o/r", kind: "dispatch", harness: "codex", model: "small", started: 3, merged: 1,
      medianTurns: 1, medianFreshTokens: 90, medianReviewRounds: 0,
    });
    assert.equal(store.modelOutcomes().length, 5);
    // Remove the running ticket's zero usage from this group to exercise even medians.
    store.finishWorkerTurn(third.id, "completed", { inputTokens: 190, cachedInputTokens: 0, outputTokens: 0 });
    const fourth = ticket();
    turn(fourth.id, "dispatch", 200);
    turn(fourth.id, "review", 0);
    const even = store.modelOutcomes("o/r").find((row) => row.kind === "dispatch" && row.model === "small" && row.harness === "codex")!;
    assert.equal(even.medianTurns, 1.5);
    assert.equal(even.medianFreshTokens, 150);
    assert.equal(even.medianReviewRounds, 0.5);
  } finally { store.close(); }
});

for (const kind of ["issue", "pull_request"] as const) test(`outcomes link merges through ${kind === "issue" ? "a subscribed PR" : "the original PR thread"} without a candidate`, () => {
  const store = Store.open(":memory:");
  try {
    const source = store.ensureSource("github", "o/r");
    const threadId = store.ensureThread({ sourceId: source.id, kind, number: 1, url: "https://example.test/thread" });
    const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: "one", threadId, kind: "issue_comment", actor: "operator", url: "https://example.test/comment", occurredAt: new Date() }).id;
    const ticketId = store.recordDecision({ threadId, triggerIds: [trigger], verdict: "accept", reason: "work" }).ticketId!;
    const id = store.createAssignment(ticketId);
    store.configureNativeAssignment(id, { harness: "codex", model: "small", worktree: "/tmp/work", branch: "work", machine: "test" });
    store.startNativeAssignment(id, "session");
    store.startWorkerTurn(id, "dispatch");
    store.finishWorkerTurn(id, "failed", { inputTokens: 5, cachedInputTokens: 10, outputTokens: 2 });
    const prNumber = kind === "issue" ? 2 : 1;
    store.recordMergeAndOutcome({ repository: "o/r", prNumber, headSha: "head", mergeSha: null, mergedAt: new Date(), mergedByLogin: null, mergedById: null, origin: "external", assignmentId: id, threadId, body: "merged", evidence: "test" });
    if (kind === "issue") {
      const pr = store.ensureThread({ sourceId: source.id, kind: "pull_request", number: prNumber, url: "https://example.test/pr" });
      store.subscribe(ticketId, pr);
    } else store.unsubscribeTicket(ticketId);
    assert.equal(store.modelOutcomes()[0]!.merged, 1);
    assert.equal(store.modelOutcomes()[0]!.medianFreshTokens, 7);
  } finally { store.close(); }
});
