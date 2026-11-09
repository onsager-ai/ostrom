import assert from "node:assert/strict";
import { test } from "node:test";

import { Store } from "../store/store.ts";
import { T3Cli, type T3Exit, type T3Invocation } from "../t3/runner.ts";
import { followUpMessage, routeFollowUps } from "./follow-up.ts";

const REPO = "onsager-ai/ostrom";

/** A stand-in `t3 thread send`: each send to a known thread becomes the next run on it. */
class FakeT3 {
  readonly sends: T3Invocation[] = [];
  readonly threads = new Set<string>();
  #runs = 0;

  readonly cli = new T3Cli(async (invocation) => {
    assert.deepEqual(invocation.args.slice(0, 3), ["thread", "send", "--json"]);
    const threadId = invocation.args[3]!;
    if (!this.threads.has(threadId)) return { code: 1, stdout: "", stderr: `Thread ${threadId} was not found.\n` };
    this.sends.push(invocation);
    return ok({ threadId, messageId: "message:m", runId: `run:${++this.#runs}`, status: "queued", delivery: "queued" });
  });
}

const ok = (value: unknown): T3Exit => ({ code: 0, stdout: JSON.stringify(value), stderr: "" });

/**
 * Issue #7 asked for work and was accepted; its Worker runs in T3 thread `mcp:worker-1` and
 * opened PR #12, which the ticket is subscribed to.
 */
function fixture() {
  const store = Store.open(":memory:", { now: () => new Date("2026-11-09T12:00:00.000Z") });
  const t3 = new FakeT3();
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
  const trigger = (threadId: number, kind = "issue_comment") =>
    store.insertTrigger({
      sourceId: source.id,
      sourceEventId: `E_${++events}`,
      threadId,
      kind,
      actor: "alice",
      url: `${store.getThread(threadId).url}#event-${events}`,
      occurredAt: new Date(Date.UTC(2026, 10, 9, 11, events)),
    }).id;
  const accept = (threadId: number) =>
    store.recordDecision({ threadId, triggerIds: [trigger(threadId)], verdict: "accept", reason: "asks for work" })
      .ticketId!;
  const worker = (ticketId: number, t3ThreadId: string) => {
    const assignmentId = store.createAssignment(ticketId);
    store.startAssignment(assignmentId, t3ThreadId);
    t3.threads.add(t3ThreadId);
    return assignmentId;
  };
  const ticketId = accept(issue);
  const assignmentId = worker(ticketId, "mcp:worker-1");
  store.subscribe(ticketId, pr);
  return { store, t3, thread, issue, pr, trigger, accept, worker, ticketId, assignmentId };
}

test("a review on a subscribed PR goes to the ticket's Worker, with no decision", async () => {
  const { store, t3, pr, trigger, ticketId, assignmentId } = fixture();
  const review = trigger(pr, "pull_request_review_comment");
  const [pending] = store.pendingDeliveries();
  assert.deepEqual({ ...pending, trigger: pending?.trigger.id }, {
    trigger: review, ticketId, assignmentId, t3ThreadId: "mcp:worker-1",
  });

  const pass = await routeFollowUps(store, t3.cli);
  assert.deepEqual(pass, {
    delivered: [
      { triggerId: review, ticketId, assignmentId, t3ThreadId: "mcp:worker-1", t3RunId: "run:1", delivery: "queued" },
    ],
    failed: [],
  });
  const [send] = t3.sends;
  assert.deepEqual(send?.args, ["thread", "send", "--json", "mcp:worker-1", "-"]);
  assert.ok(!send!.args.includes("--wait"), "routing does not hold a process per Worker");
  assert.equal(send?.stdin, followUpMessage(ticketId, pending!.trigger, store.getThread(pr)));
  assert.deepEqual(store.pendingDeliveries(), []);
  assert.deepEqual(store.undecidedTriggers(), [], "a delivered trigger never reaches the Router");
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
  const { store, t3, pr, trigger, worker, ticketId, assignmentId } = fixture();
  trigger(pr);
  trigger(pr);
  assert.equal((await routeFollowUps(store, t3.cli)).delivered.length, 2);
  assert.deepEqual(await routeFollowUps(store, t3.cli), { delivered: [], failed: [] });
  assert.equal(t3.sends.length, 2);

  // A new Worker on the same ticket does not get the ticket's old follow-ups again.
  store.endAssignment(assignmentId);
  worker(ticketId, "mcp:worker-2");
  assert.deepEqual(store.pendingDeliveries(), []);
  const later = trigger(pr);
  const pass = await routeFollowUps(store, t3.cli);
  assert.deepEqual(pass.delivered.map((d) => [d.triggerId, d.t3ThreadId]), [[later, "mcp:worker-2"]]);
});

test("a trigger on an unsubscribed thread is left for the Router", async () => {
  const { store, t3, thread, trigger } = fixture();
  const other = trigger(thread("issue", 8));
  assert.deepEqual(await routeFollowUps(store, t3.cli), { delivered: [], failed: [] });
  assert.deepEqual(t3.sends, []);
  assert.deepEqual(store.undecidedTriggers().map((t) => t.id), [other]);
});

test("undecided triggers: no decision, no delivery, and no open ticket subscribed to the thread", async () => {
  const { store, t3, thread, issue, pr, trigger, ticketId } = fixture();
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

  // While a send to the Worker keeps failing, the trigger still belongs to the Worker, not the Router.
  t3.threads.clear();
  assert.equal((await routeFollowUps(store, t3.cli)).failed.length, 2, "the issue's and the PR's follow-ups");
  assert.ok(!store.undecidedTriggers().some((t) => t.id === onPr));
  assert.equal(store.getTicket(ticketId).state, "open");
});

test("when every subscribed ticket has ended, the trigger goes to the Router, not to the old Worker", async () => {
  const { store, t3, pr, trigger, ticketId, assignmentId } = fixture();
  trigger(pr);
  await routeFollowUps(store, t3.cli);
  store.endAssignment(assignmentId);
  store.endTicket(ticketId);

  const afterEnd = trigger(pr);
  assert.deepEqual(await routeFollowUps(store, t3.cli), { delivered: [], failed: [] });
  assert.equal(t3.sends.length, 1);
  assert.deepEqual(store.undecidedTriggers().map((t) => t.id), [afterEnd], "already delivered stays delivered");
});

test("a trigger fans out to every open subscribed ticket; ended ones are skipped", async () => {
  const { store, t3, issue, pr, trigger, accept, worker, ticketId } = fixture();
  const second = accept(issue);
  worker(second, "mcp:worker-2");
  store.subscribe(second, pr);
  const third = accept(issue);
  store.subscribe(third, pr);
  store.endTicket(third);

  const id = trigger(pr);
  const pass = await routeFollowUps(store, t3.cli);
  assert.deepEqual(pass.delivered.map((d) => [d.triggerId, d.ticketId, d.t3ThreadId]), [
    [id, ticketId, "mcp:worker-1"],
    [id, second, "mcp:worker-2"],
  ]);
});

test("a failed send is reported, not recorded, and retried on the next pass", async () => {
  const { store, t3, pr, trigger, ticketId, assignmentId } = fixture();
  const id = trigger(pr);
  t3.threads.delete("mcp:worker-1");

  const failed = await routeFollowUps(store, t3.cli);
  assert.deepEqual(failed, {
    delivered: [],
    failed: [{ triggerId: id, ticketId, error: "t3 thread send exited 1: Thread mcp:worker-1 was not found." }],
  });
  assert.equal(store.pendingDeliveries().length, 1);

  t3.threads.add("mcp:worker-1");
  const retried = await routeFollowUps(store, t3.cli);
  assert.deepEqual(retried.delivered.map((d) => [d.triggerId, d.assignmentId]), [[id, assignmentId]]);
  assert.deepEqual(retried.failed, []);
});

test("an open ticket with no running Worker holds its follow-up and reports it", async () => {
  const { store, t3, pr, trigger, ticketId, assignmentId } = fixture();
  store.endAssignment(assignmentId);
  const id = trigger(pr);
  assert.deepEqual(await routeFollowUps(store, t3.cli), {
    delivered: [],
    failed: [{ triggerId: id, ticketId, error: `ticket ${ticketId} has no running Worker` }],
  });
  assert.deepEqual(store.undecidedTriggers(), []);
  assert.deepEqual(t3.sends, []);
});
