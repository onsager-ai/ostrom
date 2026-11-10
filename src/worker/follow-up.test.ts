import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import { Store } from "../store/store.ts";
import { FakeRunner } from "./fake-runner.ts";
import { followUpMessage, routeFollowUps } from "./follow-up.ts";
import { Workers } from "./workers.ts";

const REPO = "onsager-ai/ostrom";
const tick = () => new Promise((resolve) => setImmediate(resolve));
const stores: Store[] = [];

afterEach(() => {
  while (stores.length > 0) stores.pop()!.close();
});

/**
 * Issue #7 asked for work and was accepted; its Worker runs in session `session-1` and
 * opened PR #12, which the ticket is subscribed to.
 */
function fixture() {
  const store = Store.open(":memory:", { now: () => new Date("2026-11-09T12:00:00.000Z") });
  stores.push(store);
  const runner = new FakeRunner();
  const config = { repo: "/repo", base: "main", worktrees: "/wt", harness: "codex" as const, model: "gpt-6-luna", machine: "host" };
  const workers = new Workers({ store, config, runners: { claude: runner, codex: runner } });
  const source = store.ensureSource("github", REPO);
  const thread = (kind: "issue" | "pull_request", number: number) =>
    store.ensureThread({
      sourceId: source.id,
      kind,
      number,
      url: `https://github.com/${REPO}/${kind === "issue" ? "issues" : "pull"}/${number}`,
    });
  const issue = thread("issue", 7);
  const pr = thread("pull_request", 12);
  let events = 0;
  const trigger = (
    threadId: number,
    kind = "issue_comment",
    extra: Partial<{ reviewId: string; body: string; path: string; line: number }> = {},
  ) =>
    store.insertTrigger({
      sourceId: source.id,
      sourceEventId: `E_${++events}`,
      threadId,
      kind,
      actor: "alice",
      url: `${store.getThread(threadId).url}#event-${events}`,
      occurredAt: new Date(Date.UTC(2026, 10, 9, 11, events)),
      ...extra,
    }).id;
  const accept = (threadId: number) =>
    store.recordDecision({ threadId, triggerIds: [trigger(threadId)], verdict: "accept", reason: "asks for work" })
      .ticketId!;
  const worker = (ticketId: number, sessionId: string) => {
    const assignmentId = store.createAssignment(ticketId);
    store.configureNativeAssignment(assignmentId, {
      harness: "codex", model: "gpt-6-luna", worktree: `/wt/${assignmentId}`, branch: `b${assignmentId}`, machine: "host",
    });
    store.startNativeAssignment(assignmentId, sessionId);
    return assignmentId;
  };
  const ticketId = accept(issue);
  const assignmentId = worker(ticketId, "session-1");
  store.subscribe(ticketId, pr);
  return { store, runner, workers, thread, issue, pr, trigger, accept, worker, ticketId, assignmentId };
}

test("a review on a subscribed PR goes to the ticket's Worker session, with no decision", async () => {
  const { store, runner, workers, pr, trigger, ticketId, assignmentId } = fixture();
  const review = trigger(pr, "pull_request_review_comment");
  const [pending] = store.pendingDeliveries();
  assert.deepEqual({ ...pending, trigger: pending?.trigger.id }, {
    trigger: review, ticketId, assignmentId,
  });

  const pass = routeFollowUps(store, workers);
  assert.deepEqual(pass, { queued: [{ triggerId: review, ticketId, assignmentId }], failed: [] });
  await workers.drain();
  const [session] = runner.sessions;
  assert.equal(session?.resumed, true);
  assert.equal(session?.id, "session-1");
  assert.deepEqual(session?.prompts, [followUpMessage(ticketId, pending!.trigger, store.getThread(pr))]);
  assert.deepEqual(store.pendingDeliveries(), []);
  assert.deepEqual(store.undecidedTriggers(), [], "a delivered trigger never reaches the Router");
  assert.equal(store.getAssignment(assignmentId).turns, 1);
});

test("a review with three inline comments gets one Worker turn, one reply, and receipt reactions", async () => {
  const { store, runner, workers, pr, trigger, ticketId, assignmentId } = fixture();
  runner.hold = true;
  const review = trigger(pr, "pull_request_review", { reviewId: "44", body: "Please make these changes." });
  const comments = [
    trigger(pr, "pull_request_review_comment", { reviewId: "44", body: "Rename this.", path: "src/a.ts", line: 12 }),
    trigger(pr, "pull_request_review_comment", { reviewId: "44", body: "Handle null.", path: "src/b.ts", line: 31 }),
    trigger(pr, "pull_request_review_comment", { reviewId: "44", body: "Add coverage.", path: "src/c.ts", line: 8 }),
  ];
  const pass = routeFollowUps(store, workers);
  assert.deepEqual(pass, { queued: [{ triggerId: review, ticketId, assignmentId }], failed: [] });
  await tick();
  runner.sessions[0]!.release();
  await workers.drain();

  const [prompt] = runner.sessions[0]!.prompts;
  assert.match(prompt!, /Review body:\nPlease make these changes\./);
  assert.match(prompt!, /1\. src\/a\.ts:12\nRename this\./);
  assert.match(prompt!, /2\. src\/b\.ts:31\nHandle null\./);
  assert.match(prompt!, /3\. src\/c\.ts:8\nAdd coverage\./);
  assert.equal(runner.sessions[0]!.prompts.length, 1);
  assert.deepEqual(store.pendingDeliveries(), []);
  assert.deepEqual(
    store.dueReplies().filter((reply) => reply.kind === "comment" && reply.id.startsWith("review-")).map((reply) => [reply.id, reply.body]),
    [["review-1-44", "ok"]],
  );
  assert.deepEqual(
    store.dueReplies().filter((reply) => reply.kind === "reaction").map((reply) => reply.id).sort(),
    comments.map((id) => `received-${id}`).sort(),
  );
});

test("the follow-up is a summary plus a pointer to the trigger", () => {
  const { store, pr, trigger, ticketId } = fixture();
  const id = trigger(pr, "pull_request_review");
  const [pending] = store.pendingDeliveries();
  assert.equal(pending?.trigger.id, id);
  const message = followUpMessage(ticketId, pending!.trigger, store.getThread(pr));
  assert.match(message, new RegExp(`^Follow-up for Ostrom ticket ${ticketId}: a new review \\(pull_request_review\\) by @alice\n`));
  assert.match(message, new RegExp(`on pull request #12: https://github.com/${REPO}/pull/12#event-${id}\n`));
  assert.match(message, /Read it with `gh`/);
  assert.match(message, /push\nto the branch of your existing pull request; do not open a new one/);
});

test("a replayed pass sends nothing twice", async () => {
  const { store, runner, workers, pr, trigger, worker, ticketId, assignmentId } = fixture();
  trigger(pr);
  trigger(pr);
  assert.equal(routeFollowUps(store, workers).queued.length, 2);
  await workers.drain();
  assert.deepEqual(routeFollowUps(store, workers), { queued: [], failed: [] });
  assert.equal(runner.sessions[0]?.prompts.length, 2);

  // A new Worker on the same ticket does not get the ticket's old follow-ups again.
  store.endAssignment(assignmentId);
  const second = worker(ticketId, "session-2");
  assert.deepEqual(store.pendingDeliveries(), []);
  const later = trigger(pr);
  const pass = routeFollowUps(store, workers);
  assert.deepEqual(pass.queued.map((d) => [d.triggerId, d.assignmentId]), [[later, second]]);
});

test("follow-ups queue behind a running turn, one at a time, and are never queued twice", async () => {
  const { store, runner, workers, pr, trigger } = fixture();
  runner.hold = true;
  const first = trigger(pr);
  const second = trigger(pr);
  assert.equal(routeFollowUps(store, workers).queued.length, 2);
  await tick();
  const session = runner.sessions[0]!;
  assert.equal(session.prompts.length, 1, "the second waits for the first turn");
  assert.deepEqual(store.pendingDeliveries().map((p) => p.trigger.id), [second], "delivery is recorded as the turn starts");
  assert.deepEqual(routeFollowUps(store, workers), { queued: [], failed: [] }, "the queued one is not queued again");

  session.release();
  await tick();
  assert.equal(session.prompts.length, 2);
  assert.deepEqual(store.pendingDeliveries(), []);
  session.release();
  await workers.drain();
  assert.equal(session.maxRunning, 1);
  assert.equal(first < second, true);
});

test("a trigger on an unsubscribed thread is left for the Router", async () => {
  const { store, runner, workers, thread, trigger } = fixture();
  const other = trigger(thread("issue", 8));
  assert.deepEqual(routeFollowUps(store, workers), { queued: [], failed: [] });
  assert.deepEqual(runner.sessions, []);
  assert.deepEqual(store.undecidedTriggers().map((t) => t.id), [other]);
});

test("undecided triggers: no decision, no delivery, and no open ticket subscribed to the thread", () => {
  const { store, workers, thread, issue, pr, trigger, ticketId, assignmentId } = fixture();
  // The accept subscribed the ticket to #7, so a later comment there is the Worker's.
  const onIssue = trigger(issue);
  const other = thread("issue", 9);
  const onOther = trigger(other);
  const chatter = thread("issue", 10);
  store.recordDecision({ threadId: chatter, triggerIds: [trigger(chatter)], verdict: "ignore", reason: "chatter" });
  const onPr = trigger(pr);

  const undecided = store.undecidedTriggers();
  assert.deepEqual(undecided.map((t) => t.id), [onOther], "the issue's and the PR's triggers are the Worker's");
  assert.deepEqual(store.pendingDeliveries().map((p) => p.trigger.id), [onIssue, onPr]);
  assert.deepEqual(
    { ...undecided[0] },
    {
      id: onOther,
      sourceId: 1,
      sourceEventId: `E_${onOther}`,
      threadId: other,
      kind: "issue_comment",
      actor: "alice",
      url: `https://github.com/${REPO}/issues/9#event-${onOther}`,
      occurredAt: new Date(Date.UTC(2026, 10, 9, 11, onOther)).toISOString(),
      receivedAt: "2026-11-09T12:00:00.000Z",
    },
  );

  // While the Worker cannot take them, the triggers still belong to it, not the Router.
  store.endAssignment(assignmentId);
  assert.equal(routeFollowUps(store, workers).failed.length, 2, "the issue's and the PR's follow-ups");
  assert.ok(!store.undecidedTriggers().some((t) => t.id === onPr));
  assert.equal(store.getTicket(ticketId).state, "open");
});

test("when every subscribed ticket has ended, the trigger goes to the Router, not to the old Worker", async () => {
  const { store, runner, workers, pr, trigger, ticketId, assignmentId } = fixture();
  trigger(pr);
  routeFollowUps(store, workers);
  await workers.drain();
  store.endAssignment(assignmentId);
  store.endTicket(ticketId);

  const afterEnd = trigger(pr);
  assert.deepEqual(routeFollowUps(store, workers), { queued: [], failed: [] });
  assert.equal(runner.sessions[0]?.prompts.length, 1);
  assert.deepEqual(store.undecidedTriggers().map((t) => t.id), [afterEnd], "already delivered stays delivered");
});

test("a trigger fans out to every open subscribed ticket; ended ones are skipped", () => {
  const { store, workers, issue, pr, trigger, accept, worker, ticketId, assignmentId } = fixture();
  const second = accept(issue);
  const secondWorker = worker(second, "session-2");
  store.subscribe(second, pr);
  const third = accept(issue);
  store.subscribe(third, pr);
  store.endTicket(third);

  const id = trigger(pr);
  const pass = routeFollowUps(store, workers);
  assert.deepEqual(pass.queued.map((d) => [d.triggerId, d.ticketId, d.assignmentId]), [
    [id, ticketId, assignmentId],
    [id, second, secondWorker],
  ]);
});

test("a ticket ended while paused at public Stuck is owed no follow-up", () => {
  const { store, workers, pr, trigger, ticketId, assignmentId } = fixture();
  store.recordStall({ assignmentId, semonReason: "no-output", detail: null });
  const stall = store.unansweredStalls()[0]!;
  store.publishStall(stall.id);
  store.endAssignment(assignmentId);
  store.endTicket(ticketId);
  trigger(pr);

  assert.deepEqual(store.pendingDeliveries(), []);
  assert.deepEqual(routeFollowUps(store, workers), { queued: [], failed: [] });
});

test("an open ticket with no running Worker holds its follow-up and reports it", () => {
  const { store, runner, workers, pr, trigger, ticketId, assignmentId } = fixture();
  store.endAssignment(assignmentId);
  const id = trigger(pr);
  assert.deepEqual(routeFollowUps(store, workers), {
    queued: [],
    failed: [{ triggerId: id, ticketId, error: `ticket ${ticketId} has no running Worker` }],
  });
  assert.deepEqual(store.undecidedTriggers(), []);
  assert.deepEqual(runner.sessions, []);
});
