import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { Store } from "../store/store.ts";
import { T3Cli, type T3Exit, type T3Invocation } from "../t3/runner.ts";
import { dispatch, observe, workerBrief } from "./dispatch.ts";

const REPO = "onsager-ai/ostrom";
const worker = { project: "/work/ostrom", base: "main", provider: "claudeAgent", model: "claude-haiku-5-5" };
// The real `thread show --json` capture; each test edits its thread fields.
const shownThread = () => JSON.parse(readFileSync(new URL("../t3/fixtures/show.json", import.meta.url), "utf8"));

/** A stand-in `t3`: `start` hands out thread ids, `show` prints whatever `threads` holds. */
class FakeT3 {
  readonly calls: T3Invocation[] = [];
  readonly threads = new Map<string, Record<string, unknown>>();
  startExit: T3Exit | undefined;
  #started = 0;

  readonly cli = new T3Cli(async (invocation) => {
    this.calls.push(invocation);
    const [, verb] = invocation.args;
    if (verb === "start") {
      if (this.startExit !== undefined) return this.startExit;
      const threadId = `mcp:worker-${++this.#started}`;
      this.threads.set(threadId, { status: "running", pendingRequestCount: 0 });
      return ok({ threadId, projectId: "mcp:project", runId: `run:${threadId}:1` });
    }
    const threadId = invocation.args.at(-1)!;
    const fields = this.threads.get(threadId);
    if (fields === undefined) return { code: 1, stdout: "", stderr: `Thread ${threadId} not found.` };
    const value = shownThread();
    Object.assign(value.thread, { threadId, linkedPullRequest: null, pullRequests: [] }, fields);
    return ok(value);
  });
}

const ok = (value: unknown): T3Exit => ({ code: 0, stdout: JSON.stringify(value), stderr: "" });

function fixture() {
  const store = Store.open(":memory:", { now: () => new Date("2026-11-09T12:00:00.000Z") });
  const source = store.ensureSource("github", REPO);
  const issue = store.ensureThread({
    sourceId: source.id,
    kind: "issue",
    number: 7,
    url: `https://github.com/${REPO}/issues/7`,
  });
  const trigger = store.insertTrigger({
    sourceId: source.id,
    sourceEventId: "IC_1",
    threadId: issue,
    kind: "issue_comment",
    actor: "alice",
    url: `https://github.com/${REPO}/issues/7#issuecomment-1`,
    occurredAt: new Date("2026-11-09T11:59:00.000Z"),
  }).id;
  const { ticketId } = store.recordDecision({
    threadId: issue,
    triggerIds: [trigger],
    verdict: "accept",
    reason: "fix the README typo",
  });
  return { store, t3: new FakeT3(), ticketId: ticketId! };
}

const pr = (number: number, extra: Record<string, unknown> = {}) => ({
  host: "github.com",
  repository: REPO,
  number,
  url: `https://github.com/${REPO}/pull/${number}`,
  source: "created",
  state: "open",
  ...extra,
});

test("dispatch starts a Worker and records its T3 thread on a running assignment", async () => {
  const { store, t3, ticketId } = fixture();
  const dispatched = await dispatch(store, t3.cli, worker, ticketId);
  assert.deepEqual(dispatched, { assignmentId: 1, t3ThreadId: "mcp:worker-1", branch: `ostrom/ticket-${ticketId}-1` });
  const assignment = store.getAssignment(dispatched.assignmentId);
  assert.equal(assignment.state, "running");
  assert.equal(assignment.t3ThreadId, "mcp:worker-1");

  const [call] = t3.calls;
  assert.deepEqual(call?.args.slice(0, 2), ["thread", "start"]);
  assert.ok(!call!.args.includes("--wait"), "dispatch does not hold a process per Worker");
  assert.equal(call?.stdin, workerBrief(store.ticketContext(ticketId), dispatched.branch, "main"));
  for (const flag of ["--worktree main", "--branch ostrom/ticket-1-1", "--provider claudeAgent", "--model claude-haiku-5-5"]) {
    assert.ok(call!.args.join(" ").includes(flag), flag);
  }
});

test("the brief is a summary plus pointers and asks for a PR that references the issue", () => {
  const { store, ticketId } = fixture();
  const brief = workerBrief(store.ticketContext(ticketId), "ostrom/ticket-1-1", "main");
  assert.match(brief, /fix the README typo/);
  assert.match(brief, new RegExp(`issue #7: https://github.com/${REPO}/issues/7\n`));
  assert.match(brief, new RegExp(`- https://github.com/${REPO}/issues/7#issuecomment-1\n`));
  assert.match(brief, /branch ostrom\/ticket-1-1/);
  assert.match(brief, new RegExp(`pull request\non ${REPO} against main whose description says "Refs #7"`));
  // A queued GitHub reply cannot answer a pending question, so the brief rules them out.
  assert.match(brief, /Never ask a question with an interactive tool such as AskUserQuestion/);
  assert.match(brief, /with your question in it, and end your\nturn/);
});

test("a failed start ends the assignment with the reason and is not a success", async () => {
  const { store, t3, ticketId } = fixture();
  t3.startExit = { code: 1, stdout: "", stderr: "Provider claudeAgent is not available.\n" };
  await assert.rejects(dispatch(store, t3.cli, worker, ticketId), /exited 1: Provider claudeAgent is not available/);
  const assignment = store.getAssignment(1);
  assert.equal(assignment.state, "ended");
  assert.equal(assignment.t3ThreadId, null);
  assert.equal(assignment.endReason, "t3 thread start exited 1: Provider claudeAgent is not available.");
  assert.deepEqual(store.runningAssignments(), []);

  t3.startExit = { code: 0, stdout: "Thread mcp:x\n", stderr: "" };
  await assert.rejects(dispatch(store, t3.cli, worker, ticketId), /not JSON/);
  assert.equal(store.getAssignment(2).endReason, "t3 printed output that is not JSON");
});

test("dispatch refuses an ended ticket without starting anything", async () => {
  const { store, t3, ticketId } = fixture();
  store.endTicket(ticketId);
  await assert.rejects(dispatch(store, t3.cli, worker, ticketId), /takes no assignment/);
  assert.deepEqual(t3.calls, []);
});

test("observe subscribes the ticket to the Worker's PR, once across passes", async () => {
  const { store, t3, ticketId } = fixture();
  const { t3ThreadId } = await dispatch(store, t3.cli, worker, ticketId);

  const before = await observe(store, t3.cli, REPO);
  assert.deepEqual(before.observed[0]?.pullRequests, []);

  t3.threads.set(t3ThreadId, {
    status: "completed",
    pendingRequestCount: 0,
    pullRequests: [
      pr(12),
      pr(13, { repository: "someone/else" }),
      pr(14, { source: "stack-dismissed" }),
      pr(15, { host: "gitlab.com" }),
    ],
  });
  const first = await observe(store, t3.cli, REPO);
  assert.deepEqual(first, {
    observed: [{ assignmentId: 1, t3ThreadId, pullRequests: [12], newlySubscribed: [12], outcome: null }],
    failed: [],
  });
  const prThread = store.ensureThread({
    sourceId: store.ensureSource("github", REPO).id,
    kind: "pull_request",
    number: 12,
    url: `https://github.com/${REPO}/pull/12`,
  });
  assert.equal(store.getThread(prThread).kind, "pull_request");
  assert.deepEqual(store.ticketsForThread(prThread).map((t) => t.id), [ticketId]);

  // The linked PR reads the same; a second pass subscribes nothing new.
  t3.threads.set(t3ThreadId, {
    status: "completed",
    pendingRequestCount: 0,
    linkedPullRequest: { projectId: "mcp:project", repository: "Onsager-AI/Ostrom", number: 12, url: pr(12).url },
    pullRequests: [pr(12)],
  });
  const second = await observe(store, t3.cli, REPO);
  assert.deepEqual(second.observed[0]?.pullRequests, [12]);
  assert.deepEqual(second.observed[0]?.newlySubscribed, []);
  assert.deepEqual(store.ticketsForThread(prThread).map((t) => t.id), [ticketId]);
});

test("observe records waiting and failed outcomes, and clears them when the Worker moves on", async () => {
  const { store, t3, ticketId } = fixture();
  const a = await dispatch(store, t3.cli, worker, ticketId);
  const b = await dispatch(store, t3.cli, worker, ticketId);
  const c = await dispatch(store, t3.cli, worker, ticketId);
  // A Worker blocked on its question reads `running` with a pending request, as the scripted stall showed.
  t3.threads.set(a.t3ThreadId, { status: "running", pendingRequestCount: 1 });
  t3.threads.set(b.t3ThreadId, { status: "failed", pendingRequestCount: 0 });
  t3.threads.set(c.t3ThreadId, { status: "cancelled", pendingRequestCount: 0 });

  const pass = await observe(store, t3.cli, REPO);
  assert.deepEqual(pass.observed.map((o) => o.outcome), ["waiting", "failed", "cancelled"]);
  assert.deepEqual(
    [a, b, c].map(({ assignmentId }) => store.getAssignment(assignmentId).outcome),
    ["waiting", "failed", "cancelled"],
  );

  // Waiting with nothing pending is not flagged; an answered Worker running again clears it.
  t3.threads.set(a.t3ThreadId, { status: "waiting", pendingRequestCount: 0 });
  t3.threads.set(b.t3ThreadId, { status: "running", pendingRequestCount: 0 });
  await observe(store, t3.cli, REPO);
  assert.equal(store.getAssignment(a.assignmentId).outcome, null);
  assert.equal(store.getAssignment(b.assignmentId).outcome, null);
  assert.equal(store.getAssignment(c.assignmentId).outcome, "cancelled");
});

test("a thread that cannot be read fails that assignment and the pass goes on", async () => {
  const { store, t3, ticketId } = fixture();
  const lost = await dispatch(store, t3.cli, worker, ticketId);
  const kept = await dispatch(store, t3.cli, worker, ticketId);
  t3.threads.delete(lost.t3ThreadId);
  t3.threads.set(kept.t3ThreadId, { status: "waiting", pendingRequestCount: 2 });

  const pass = await observe(store, t3.cli, REPO);
  assert.deepEqual(pass.failed, [
    { assignmentId: lost.assignmentId, error: `t3 thread show exited 1: Thread ${lost.t3ThreadId} not found.` },
  ]);
  assert.deepEqual(pass.observed.map((o) => o.assignmentId), [kept.assignmentId]);
  assert.equal(store.getAssignment(kept.assignmentId).outcome, "waiting");
  assert.equal(store.getAssignment(lost.assignmentId).state, "running");
});
