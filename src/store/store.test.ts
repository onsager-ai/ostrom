import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";
import { migrate, migrations } from "./migrations.ts";
import { IllegalTransition, Store } from "./store.ts";

/** A store on an in-memory database with a clock the test moves by hand. */
function fixture() {
  let now = new Date("2026-11-09T12:00:00.000Z");
  const store = Store.open(":memory:", { now: () => now });
  const source = store.ensureSource("github", "onsager-ai/ostrom");
  const issue = store.ensureThread({
    sourceId: source.id,
    kind: "issue",
    number: 7,
    url: "https://github.com/onsager-ai/ostrom/issues/7",
  });
  let events = 0;
  const trigger = (threadId = issue) =>
    store.insertTrigger({
      sourceId: source.id,
      sourceEventId: `IC_${++events}`,
      threadId,
      kind: "issue_comment",
      actor: "someone",
      url: `https://github.com/onsager-ai/ostrom/issues/7#issuecomment-${events}`,
      occurredAt: now,
    }).id;
  const accept = () =>
    store.recordDecision({ threadId: issue, triggerIds: [trigger()], verdict: "accept", reason: "asks for work" })
      .ticketId!;
  return {
    store,
    source,
    issue,
    trigger,
    accept,
    advance: (ms: number) => (now = new Date(now.getTime() + ms)),
  };
}

test("migrations apply once and record their version", () => {
  const db = new DatabaseSync(":memory:");
  assert.equal(migrate(db), migrations.length);
  assert.equal(migrate(db), migrations.length);
  const rows = db.prepare("SELECT version FROM schema_version ORDER BY version").all();
  assert.deepEqual(
    rows.map((r) => r["version"]),
    migrations.map((_, i) => i + 1),
  );
});

test("a database newer than the code is refused", () => {
  const db = new DatabaseSync(":memory:");
  migrate(db);
  db.prepare("INSERT INTO schema_version (version, applied_at) VALUES (?, ?)").run(
    migrations.length + 1,
    "2026-11-09T00:00:00.000Z",
  );
  assert.throws(() => migrate(db), /newer than this code/);
});

test("ensureSource and ensureThread return the same record on repeat", () => {
  const { store, source, issue } = fixture();
  assert.deepEqual(store.ensureSource("github", "onsager-ai/ostrom"), source);
  const again = store.ensureThread({
    sourceId: source.id,
    kind: "issue",
    number: 7,
    url: "https://github.com/onsager-ai/ostrom/issues/7",
  });
  assert.equal(again, issue);
  assert.throws(
    () => store.ensureThread({ sourceId: source.id, kind: "pull_request", number: 7, url: "x" }),
    /is kind issue, not pull_request/,
  );
});

test("a trigger is idempotent on (source, source event id)", () => {
  const { store, source, issue } = fixture();
  const event = {
    sourceId: source.id,
    sourceEventId: "IC_kwDOabc",
    threadId: issue,
    kind: "issue_comment",
    actor: "someone",
    url: "https://github.com/onsager-ai/ostrom/issues/7#issuecomment-1",
    occurredAt: new Date("2026-11-09T11:59:00.000Z"),
  };
  const first = store.insertTrigger(event);
  assert.equal(first.duplicate, false);
  const second = store.insertTrigger(event);
  assert.deepEqual(second, { id: first.id, duplicate: true });

  const otherSource = store.ensureSource("github", "onsager-ai/semon");
  const otherThread = store.ensureThread({ sourceId: otherSource.id, kind: "issue", number: 7, url: "y" });
  const elsewhere = store.insertTrigger({ ...event, sourceId: otherSource.id, threadId: otherThread });
  assert.equal(elsewhere.duplicate, false);
  assert.notEqual(elsewhere.id, first.id);
});

test("a source's cursor starts empty and advances", () => {
  const { store, source } = fixture();
  assert.equal(store.readCursor(source.id), null);
  store.advanceCursor(source.id, "2026-11-09T12:00:00Z");
  assert.equal(store.readCursor(source.id), "2026-11-09T12:00:00Z");
  store.advanceCursor(source.id, "2026-11-09T12:05:00Z");
  assert.equal(store.readCursor(source.id), "2026-11-09T12:05:00Z");
  assert.equal(store.ensureSource("github", "onsager-ai/ostrom").cursor, "2026-11-09T12:05:00Z");
  assert.throws(() => store.readCursor(999), /no source 999/);
  assert.throws(() => store.advanceCursor(999, "x"), /no source 999/);
});

test("only an accept decision creates a ticket", () => {
  const { store, issue, trigger } = fixture();
  const ignored = store.recordDecision({
    threadId: issue,
    triggerIds: [trigger()],
    verdict: "ignore",
    reason: "chatter",
  });
  assert.equal(ignored.ticketId, null);
  const stopped = store.recordDecision({ threadId: issue, triggerIds: [trigger()], verdict: "stop", reason: "asked to stop" });
  assert.equal(stopped.ticketId, null);

  const accepted = store.recordDecision({
    threadId: issue,
    triggerIds: [trigger(), trigger()],
    verdict: "accept",
    reason: "asks for a fix",
  });
  assert.notEqual(accepted.ticketId, null);
  const ticket = store.getTicket(accepted.ticketId!);
  assert.equal(ticket.decisionId, accepted.decisionId);
  assert.equal(ticket.state, "open");
});

test("the schema refuses a ticket for a decision that is not an accept", () => {
  const db = new DatabaseSync(":memory:");
  const store = new Store(db);
  const source = store.ensureSource("github", "o/r");
  const thread = store.ensureThread({ sourceId: source.id, kind: "issue", number: 1, url: "u" });
  const { decisionId } = store.recordDecision({ threadId: thread, triggerIds: [], verdict: "ignore", reason: "no" });
  assert.throws(
    () =>
      db.prepare("INSERT INTO ticket (decision_id, state, opened_at) VALUES (?, 'open', 'now')").run(decisionId),
    /only an accept decision creates a ticket/,
  );
});

test("a decision rolls back whole when a trigger is not on its thread", () => {
  const { store, source, issue, trigger } = fixture();
  const pr = store.ensureThread({ sourceId: source.id, kind: "pull_request", number: 8, url: "pr" });
  const onPr = trigger(pr);
  assert.throws(
    () => store.recordDecision({ threadId: issue, triggerIds: [onPr], verdict: "accept", reason: "r" }),
    /is not on thread/,
  );
  assert.throws(
    () => store.recordDecision({ threadId: issue, triggerIds: [], verdict: "accept", reason: "" }),
    /CHECK constraint/,
  );
  const next = store.recordDecision({ threadId: issue, triggerIds: [], verdict: "accept", reason: "r" });
  assert.equal(next.decisionId, 1, "the failed decisions left no rows behind");
  assert.equal(next.ticketId, 1);
});

test("a trigger cannot be edited after it is stored", () => {
  const db = new DatabaseSync(":memory:");
  const store = new Store(db);
  const source = store.ensureSource("github", "o/r");
  const thread = store.ensureThread({ sourceId: source.id, kind: "issue", number: 1, url: "u" });
  const { id } = store.insertTrigger({
    sourceId: source.id,
    sourceEventId: "e",
    threadId: thread,
    kind: "issue_comment",
    actor: "a",
    url: "u",
    occurredAt: new Date(),
  });
  assert.throws(() => db.prepare(`UPDATE "trigger" SET actor = 'b' WHERE id = ?`).run(id), /immutable/);
});

test("a ticket goes open → ended once", () => {
  const { store, accept, advance } = fixture();
  const ticketId = accept();
  advance(1000);
  store.endTicket(ticketId);
  const ticket = store.getTicket(ticketId);
  assert.equal(ticket.state, "ended");
  assert.equal(ticket.endedAt, "2026-11-09T12:00:01.000Z");
  assert.throws(() => store.endTicket(ticketId), IllegalTransition);
  assert.throws(() => store.createAssignment(ticketId), IllegalTransition);
});

test("an assignment goes pending → running → ended with its T3 thread id", () => {
  const { store, accept } = fixture();
  const ticketId = accept();
  const first = store.createAssignment(ticketId);
  assert.equal(store.getAssignment(first).state, "pending");
  assert.equal(store.getAssignment(first).t3ThreadId, null);

  assert.throws(() => store.endAssignment(first), /pending → ended is not allowed/);

  store.startAssignment(first, "t3-thread-1");
  const running = store.getAssignment(first);
  assert.equal(running.state, "running");
  assert.equal(running.t3ThreadId, "t3-thread-1");
  assert.throws(() => store.startAssignment(first, "t3-thread-2"), /running → running is not allowed/);

  store.endAssignment(first);
  assert.equal(store.getAssignment(first).state, "ended");
  assert.equal(store.getAssignment(first).t3ThreadId, "t3-thread-1");
  assert.throws(() => store.endAssignment(first), /ended → ended is not allowed/);
  assert.throws(() => store.startAssignment(first, "t3-thread-3"), /ended → running is not allowed/);

  const second = store.createAssignment(ticketId);
  assert.notEqual(second, first, "a ticket may have several assignments");
  assert.throws(() => store.startAssignment(second, "t3-thread-1"), /UNIQUE constraint/);
  assert.equal(store.getAssignment(second).state, "pending");
  assert.throws(() => store.startAssignment(second, ""), /must not be empty/);
  assert.throws(() => store.getAssignment(999), /no assignment 999/);
});

test("subscriptions are many-to-many and looked up by thread", () => {
  const { store, source, issue, accept } = fixture();
  const pr = store.ensureThread({ sourceId: source.id, kind: "pull_request", number: 8, url: "pr" });
  const a = accept();
  const b = accept();

  // An accept subscribes its ticket to the thread it was accepted on.
  assert.deepEqual(store.subscribe(a, issue), { duplicate: true });
  assert.deepEqual(store.subscribe(a, pr), { duplicate: false });
  assert.deepEqual(store.subscribe(b, pr), { duplicate: false });
  assert.deepEqual(store.subscribe(a, pr), { duplicate: true });

  assert.deepEqual(store.ticketsForThread(issue).map((t) => t.id), [a, b]);
  assert.deepEqual(store.ticketsForThread(pr).map((t) => t.id), [a, b]);
  const nobody = store.ensureThread({ sourceId: source.id, kind: "issue", number: 9, url: "i9" });
  assert.deepEqual(store.ticketsForThread(nobody), []);
});

test("an outbox reply is sent once and retried after a failure", () => {
  const { store, issue, advance } = fixture();
  assert.deepEqual(store.enqueueReply({ id: "ack:ticket-1", threadId: issue, body: "On it." }), {
    duplicate: false,
  });
  assert.deepEqual(store.enqueueReply({ id: "ack:ticket-1", threadId: issue, body: "On it." }), {
    duplicate: true,
  });
  assert.throws(
    () => store.enqueueReply({ id: "ack:ticket-1", threadId: issue, body: "Different." }),
    /already holds a different reply/,
  );
  assert.deepEqual(store.dueReplies().map((r) => r.id), ["ack:ticket-1"]);

  store.recordReplyFailure("ack:ticket-1", "HTTP 502", new Date("2026-11-09T12:01:00.000Z"));
  const failed = store.dueReplies();
  assert.deepEqual(failed, [], "not due until the retry time");

  advance(60_000);
  const [due] = store.dueReplies();
  assert.equal(due?.id, "ack:ticket-1");
  assert.equal(due?.attempts, 1);
  assert.equal(due?.lastError, "HTTP 502");

  store.markReplySent("ack:ticket-1");
  assert.deepEqual(store.dueReplies(), []);
  assert.throws(() => store.markReplySent("ack:ticket-1"), IllegalTransition);
  assert.throws(() => store.recordReplyFailure("ack:ticket-1", "late", new Date()), IllegalTransition);
  assert.deepEqual(
    store.enqueueReply({ id: "ack:ticket-1", threadId: issue, body: "On it." }),
    { duplicate: true },
    "re-enqueueing a sent reply does not queue it again",
  );
  assert.deepEqual(store.dueReplies(), []);
  assert.throws(() => store.markReplySent("missing"), /no outbox reply missing/);
});

test("due replies come back oldest due first, up to the limit", () => {
  const { store, issue, advance } = fixture();
  store.enqueueReply({ id: "r1", threadId: issue, body: "1" });
  advance(1);
  store.enqueueReply({ id: "r2", threadId: issue, body: "2" });
  advance(1);
  store.enqueueReply({ id: "r3", threadId: issue, body: "3" });
  store.recordReplyFailure("r1", "boom", new Date("2026-11-09T12:00:00.002Z"));
  assert.deepEqual(store.dueReplies().map((r) => r.id), ["r2", "r1", "r3"]);
  assert.deepEqual(store.dueReplies(2).map((r) => r.id), ["r2", "r1"]);
});

test("an assignment whose Worker never started ends from pending with the reason", () => {
  const { store, accept } = fixture();
  const id = store.createAssignment(accept());
  assert.throws(() => store.abandonAssignment(id, ""), /must not be empty/);
  store.abandonAssignment(id, "t3 thread start exited 1: no such project");
  const ended = store.getAssignment(id);
  assert.equal(ended.state, "ended");
  assert.equal(ended.t3ThreadId, null);
  assert.equal(ended.endReason, "t3 thread start exited 1: no such project");
  assert.equal(ended.endedAt, "2026-11-09T12:00:00.000Z");
  assert.throws(() => store.abandonAssignment(id, "again"), /ended → ended is not allowed/);
  assert.throws(() => store.startAssignment(id, "t3-late"), /ended → running is not allowed/);

  const running = store.createAssignment(accept());
  store.startAssignment(running, "t3-thread-9");
  assert.throws(() => store.abandonAssignment(running, "no"), /running → ended is not allowed/);
});

test("the schema refuses an unstarted assignment that ends without a reason", () => {
  const db = new DatabaseSync(":memory:", { enableForeignKeyConstraints: true });
  const store = new Store(db);
  const source = store.ensureSource("github", "o/r");
  const thread = store.ensureThread({ sourceId: source.id, kind: "issue", number: 1, url: "u" });
  const trigger = store.insertTrigger({
    sourceId: source.id, sourceEventId: "e", threadId: thread, kind: "issue_comment", actor: "a", url: "u",
    occurredAt: new Date(),
  }).id;
  const ticketId = store.recordDecision({ threadId: thread, triggerIds: [trigger], verdict: "accept", reason: "r" })
    .ticketId!;
  const id = store.createAssignment(ticketId);
  assert.throws(
    () => db.prepare("UPDATE assignment SET state = 'ended', ended_at = 'x' WHERE id = ?").run(id),
    /CHECK constraint failed/,
  );
});

test("a running assignment records its observed outcome, timed from when it first held", () => {
  const { store, accept, advance } = fixture();
  const id = store.createAssignment(accept());
  assert.throws(() => store.recordOutcome(id, "waiting"), /pending; only a running one has an outcome/);
  store.startAssignment(id, "t3-thread-1");
  assert.deepEqual(store.runningAssignments().map((a) => a.id), [id]);

  store.recordOutcome(id, "waiting");
  assert.equal(store.getAssignment(id).outcomeAt, "2026-11-09T12:00:00.000Z");
  advance(60_000);
  store.recordOutcome(id, "waiting");
  assert.equal(store.getAssignment(id).outcomeAt, "2026-11-09T12:00:00.000Z", "the same outcome keeps its time");
  store.recordOutcome(id, "failed");
  assert.deepEqual(
    [store.getAssignment(id).outcome, store.getAssignment(id).outcomeAt],
    ["failed", "2026-11-09T12:01:00.000Z"],
  );
  store.recordOutcome(id, null);
  assert.deepEqual([store.getAssignment(id).outcome, store.getAssignment(id).outcomeAt], [null, null]);

  store.endAssignment(id);
  assert.deepEqual(store.runningAssignments(), []);
  assert.throws(() => store.recordOutcome(id, "failed"), /ended; only a running one has an outcome/);
});

test("a ticket's context is its thread, the decision reason, and its triggers in order", () => {
  const { store, source, issue, trigger } = fixture();
  const first = trigger();
  const second = trigger();
  const { ticketId } = store.recordDecision({
    threadId: issue,
    triggerIds: [second, first],
    verdict: "accept",
    reason: "asks for a fix",
  });
  const context = store.ticketContext(ticketId!);
  assert.equal(context.ticket.id, ticketId);
  assert.deepEqual(context.thread, {
    id: issue,
    sourceId: source.id,
    kind: "issue",
    number: 7,
    url: "https://github.com/onsager-ai/ostrom/issues/7",
  });
  assert.deepEqual(context.source, source);
  assert.equal(context.reason, "asks for a fix");
  assert.deepEqual(context.triggerUrls, [
    "https://github.com/onsager-ai/ostrom/issues/7#issuecomment-1",
    "https://github.com/onsager-ai/ostrom/issues/7#issuecomment-2",
  ]);
});

test("migration 2 keeps the assignments a version 1 store already holds", () => {
  const db = new DatabaseSync(":memory:", { enableForeignKeyConstraints: true });
  db.exec(migrations[0]!);
  db.exec(`
    CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL) STRICT;
    INSERT INTO schema_version VALUES (1, '2026-11-09T00:00:00.000Z');
    INSERT INTO source (id, kind, name) VALUES (1, 'github', 'o/r');
    INSERT INTO thread (id, source_id, kind, number, url) VALUES (1, 1, 'issue', 1, 'u');
    INSERT INTO decision (id, thread_id, verdict, reason, decided_at) VALUES (1, 1, 'accept', 'r', 't');
    INSERT INTO ticket (id, decision_id, state, opened_at) VALUES (1, 1, 'open', 't');
    INSERT INTO assignment (id, ticket_id, state, created_at) VALUES (1, 1, 'pending', 't');
    INSERT INTO assignment (id, ticket_id, state, t3_thread_id, created_at, started_at)
      VALUES (2, 1, 'running', 'thr', 't', 't');
  `);
  const store = new Store(db);
  assert.equal(store.getAssignment(1).state, "pending");
  assert.deepEqual(
    { ...store.getAssignment(2) },
    {
      id: 2, ticketId: 1, state: "running", t3ThreadId: "thr", createdAt: "t", startedAt: "t", endedAt: null,
      endReason: null, outcome: null, outcomeAt: null,
    },
  );
  store.abandonAssignment(1, "never started");
  assert.equal(store.getAssignment(1).endReason, "never started");
});

test("a delivery is recorded once, and only for a ticket subscribed to the trigger's thread", () => {
  const { store, source, issue, trigger, accept } = fixture();
  const ticketId = accept();
  const assignmentId = store.createAssignment(ticketId);
  store.startAssignment(assignmentId, "mcp:worker");
  // The accept subscribed the ticket to its own thread, so a later comment there is a follow-up.
  const followUp = trigger();
  assert.deepEqual(store.pendingDeliveries().map((p) => [p.trigger.id, p.ticketId, p.assignmentId, p.t3ThreadId]), [
    [followUp, ticketId, assignmentId, "mcp:worker"],
  ]);
  assert.deepEqual(store.recordDelivery(followUp, assignmentId, "run:1"), { duplicate: false });
  assert.deepEqual(store.recordDelivery(followUp, assignmentId, "run:2"), { duplicate: true });
  assert.deepEqual(store.pendingDeliveries(), []);

  const elsewhere = store.ensureThread({ sourceId: source.id, kind: "issue", number: 8, url: "u8" });
  assert.throws(
    () => store.recordDelivery(trigger(elsewhere), assignmentId, "run:3"),
    /needs the assignment's ticket subscribed/,
  );
});

test("a decision's reply is queued with it, under an id derived from the decision", () => {
  const { store, issue, trigger } = fixture();
  const { decisionId, replyId } = store.recordDecision({
    threadId: issue,
    triggerIds: [trigger()],
    verdict: "accept",
    reason: "asks for work",
    reply: "On it.",
  });
  assert.equal(replyId, `decision-${decisionId}`);
  assert.equal(store.hasReply(replyId!), true);
  assert.deepEqual(store.dueReplies().map((r) => [r.id, r.threadId, r.body]), [[replyId, issue, "On it."]]);
  assert.equal(store.recordDecision({ threadId: issue, triggerIds: [trigger()], verdict: "ignore", reason: "no" }).replyId, null);
  // An empty reply fails the whole decision: no decision row, no trigger link.
  const left = trigger();
  assert.throws(
    () => store.recordDecision({ threadId: issue, triggerIds: [left], verdict: "accept", reason: "r", reply: " " }),
    /must not be empty/,
  );
  assert.deepEqual(store.undecidedTriggers().map((t) => t.id), [left]);
});

test("a terminal outcome reply ends the assignment and, with no other running, the ticket", () => {
  const { store, issue, accept } = fixture();
  const ticketId = accept();
  assert.deepEqual(store.undispatchedTickets(), [ticketId]);
  const first = store.createAssignment(ticketId);
  store.startAssignment(first, "mcp:w1");
  const second = store.createAssignment(ticketId);
  store.startAssignment(second, "mcp:w2");
  assert.deepEqual(store.undispatchedTickets(), []);

  const opened = store.enqueueOutcomeReply({ id: "outcome-a1-pr12-opened", threadId: issue, body: "PR open" });
  assert.deepEqual(opened, { duplicate: false, ticketEnded: false });
  assert.equal(store.getAssignment(first).state, "running");

  const failed = store.enqueueOutcomeReply({ id: "outcome-a1-failed", threadId: issue, body: "x" }, { assignmentId: first });
  assert.deepEqual(failed, { duplicate: false, ticketEnded: false });
  assert.equal(store.getAssignment(first).state, "ended");
  assert.equal(store.getTicket(ticketId).state, "open");

  const merged = store.enqueueOutcomeReply({ id: "outcome-a2-pr13-merged", threadId: issue, body: "y" }, { assignmentId: second });
  assert.deepEqual(merged, { duplicate: false, ticketEnded: true });
  assert.equal(store.getTicket(ticketId).state, "ended");
});

test("an abandoned assignment is listed only while its ticket is open", () => {
  const { store, issue, accept } = fixture();
  const ticketId = accept();
  const assignmentId = store.createAssignment(ticketId);
  store.abandonAssignment(assignmentId, "t3 down");
  assert.deepEqual(store.abandonedAssignments().map((a) => a.id), [assignmentId]);
  store.enqueueOutcomeReply({ id: "outcome-a1-failed", threadId: issue, body: "failed" }, { assignmentId });
  assert.deepEqual(store.abandonedAssignments(), []);
  assert.equal(store.getTicket(ticketId).state, "ended");
});

test("the newest recorded Router thread is the current one", () => {
  const { store } = fixture();
  assert.equal(store.routerThread(), null);
  store.recordRouterThread("mcp:router-1");
  store.recordRouterThread("mcp:router-2");
  assert.equal(store.routerThread(), "mcp:router-2");
  assert.throws(() => store.recordRouterThread("mcp:router-1"), /UNIQUE/);
});
