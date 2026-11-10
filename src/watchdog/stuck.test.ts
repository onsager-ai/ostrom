import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import { Store } from "../store/store.ts";
import { FakeRunner } from "../worker/fake-runner.ts";
import { Workers } from "../worker/workers.ts";
import { routeFollowUps } from "../worker/follow-up.ts";
import { advanceStuckLadder, parseStuckReply, renderStuckReply } from "./stuck.ts";

const REPO = "onsager-ai/ostrom";
const stores: Store[] = [];

afterEach(() => {
  while (stores.length > 0) stores.pop()!.close();
});

function fixture() {
  const now = new Date("2026-11-09T12:00:00.000Z");
  const store = Store.open(":memory:", { now: () => now });
  stores.push(store);
  const source = store.ensureSource("github", REPO);
  const threadId = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: `https://github.com/${REPO}/issues/7` });
  const trigger = store.insertTrigger({
    sourceId: source.id, sourceEventId: "accepted", threadId, kind: "issue_comment", actor: "alice",
    url: `https://github.com/${REPO}/issues/7#issuecomment-1`, occurredAt: now,
  });
  const ticketId = store.recordDecision({ threadId, triggerIds: [trigger.id], verdict: "accept", reason: "work" }).ticketId!;
  const runner = new FakeRunner();
  runner.hold = true;
  const workers = new Workers({
    store,
    config: { repo: "/repo", base: "main", worktrees: "/wt", harness: "codex", model: "gpt-6-luna", machine: "host" },
    runners: { claude: runner, codex: runner },
    addWorktree: async () => "branch",
  });
  return { store, runner, workers, ticketId, threadId };
}

test("stuck reply snapshot", () => {
  assert.equal(
    renderStuckReply({
      tryingTo: "update the parser", blockedBy: "the expected format is unclear", tried: "read the fixtures",
      choices: "1. use format A\n2. use format B", workSoFar: "The branch has the parser change.",
    }),
    [
      "**I'm stuck on this and need a decision.**",
      "**Trying to:** update the parser",
      "**Blocked by:** the expected format is unclear",
      "**Tried:** read the fixtures",
      "**I need you to choose:** 1. use format A\n2. use format B",
      "**Work so far:** The branch has the parser change. Reply with a number, or anything else to redirect.",
    ].join("\n"),
  );
});

test("stuck reply numbers unnumbered choices", () => {
  const base = { tryingTo: "finish work", blockedBy: "a choice", tried: "two approaches", workSoFar: "The branch is pushed." };
  assert.ok(renderStuckReply({ ...base, choices: "Resume the stall later, or stop the stall and keep the current pull request" }).includes(
    "**I need you to choose:** 1. Resume the stall later\n2. stop the stall and keep the current pull request",
  ));
  assert.ok(renderStuckReply({ ...base, choices: "1. continue\n2. stop" }).includes("**I need you to choose:** 1. continue\n2. stop"));
});

test("malformed Worker reply gets a complete, honest fallback", () => {
  assert.deepEqual(parseStuckReply('{"tryingTo":"only one field"}', "budget reached: tokens"), {
    tryingTo: "complete the requested work", blockedBy: "budget reached: tokens",
    tried: "two automatic recovery attempts on the same Worker session",
    choices: "1. provide the missing decision\n2. stop this ticket",
    workSoFar: "The Worker was asked to push its work in progress before this message.",
  });
});

test("three stall episodes on one Worker recover twice, then queue its public stuck reply", async () => {
  const { store, runner, workers, ticketId } = fixture();
  await workers.dispatch(ticketId);
  const assignmentId = 1;
  const session = runner.sessions[0]!;

  const first = store.recordStall({ assignmentId, semonReason: "no-output", detail: null, cause: "no-output" }).id;
  const recoveryOne = await advanceStuckLadder(store, workers);
  session.auto = true;
  await workers.drain();
  assert.deepEqual(recoveryOne, { recovered: [first], published: [] });
  assert.match(session.prompts.at(-1)!, /detected that this ticket is stuck/);
  assert.equal(store.getAssignment(assignmentId).stallRecoveryAttempts, 1);

  store.clearStall(assignmentId);
  const second = store.recordStall({ assignmentId, semonReason: "no-output", detail: null, cause: "no-output" }).id;
  const recoveryTwo = await advanceStuckLadder(store, workers);
  await workers.drain();
  assert.deepEqual(recoveryTwo, { recovered: [second], published: [] });
  assert.match(session.prompts.at(-1)!, /still sees this ticket as stuck/);
  assert.equal(store.getAssignment(assignmentId).stallRecoveryAttempts, 2);

  store.clearStall(assignmentId);
  const third = store.recordStall({ assignmentId, semonReason: "no-output", detail: null, cause: "no-output" }).id;
  assert.deepEqual(await advanceStuckLadder(store, workers), { recovered: [], published: [third] });
  await workers.drain();
  assert.equal(store.getStall(third).publicAt, "2026-11-09T12:00:00.000Z");
  assert.ok(store.dueReplies().some((reply) => reply.id === `stuck-${third}`));
});

test("a human comment during a silent recovery reaches the Worker and leaves the ticket open", async () => {
  const { store, runner, workers, ticketId, threadId } = fixture();
  await workers.dispatch(ticketId);
  const assignmentId = 1;
  runner.sessions[0]!.auto = true;
  store.recordStall({ assignmentId, semonReason: "no-output", detail: null });
  await advanceStuckLadder(store, workers);
  await workers.drain();
  store.insertTrigger({
    sourceId: store.ensureSource("github", REPO).id, sourceEventId: "human-early", threadId, kind: "issue_comment", actor: "alice",
    url: `https://github.com/${REPO}/issues/7#issuecomment-early`, occurredAt: new Date("2026-11-09T12:01:00.000Z"), body: "try red",
  });
  const pass = routeFollowUps(store, workers);
  await workers.drain();
  assert.deepEqual(pass.failed, []);
  assert.equal(pass.queued.length, 1);
  assert.equal(store.getTicket(ticketId).state, "open");
  assert.equal(store.getAssignment(assignmentId).stallRecoveryAttempts, 0);
});

test("a human follow-up resets the Worker recovery count", async () => {
  const { store, runner, workers, ticketId, threadId } = fixture();
  await workers.dispatch(ticketId);
  const assignmentId = 1;
  const session = runner.sessions[0]!;
  const first = store.recordStall({ assignmentId, semonReason: "no-output", detail: null }).id;
  await advanceStuckLadder(store, workers);
  session.auto = true;
  await workers.drain();
  store.clearStall(assignmentId);
  const second = store.recordStall({ assignmentId, semonReason: "no-output", detail: null }).id;
  await advanceStuckLadder(store, workers);
  await workers.drain();
  store.clearStall(assignmentId);
  const publicStall = store.recordStall({ assignmentId, semonReason: "no-output", detail: null }).id;
  await advanceStuckLadder(store, workers);
  await workers.drain();
  assert.equal(store.getStall(publicStall).publicAt, "2026-11-09T12:00:00.000Z");

  store.insertTrigger({
    sourceId: store.ensureSource("github", REPO).id, sourceEventId: "human", threadId, kind: "issue_comment", actor: "alice",
    url: `https://github.com/${REPO}/issues/7#issuecomment-human`, occurredAt: new Date("2026-11-09T12:01:00.000Z"), body: "try red",
  });
  routeFollowUps(store, workers);
  await workers.drain();
  assert.equal(store.getAssignment(assignmentId).stallRecoveryAttempts, 0);
});
