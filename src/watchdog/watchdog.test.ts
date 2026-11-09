import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { Store } from "../store/store.ts";
import { parseThreadShow } from "../t3/cli-json.ts";
import { T3Cli, type T3Exit, type T3Runner } from "../t3/runner.ts";
import {
  parseSemonStalls,
  semonStallsArgs,
  t3StallReason,
  watchdog,
  type WatchdogOptions,
} from "./watchdog.ts";

const REPO = "onsager-ai/ostrom";
const NOW = new Date("2026-11-09T12:00:00.000Z");
const MINUTES = 20;
const fresh = "2026-11-09T11:59:00.000Z";
const stale = "2026-11-09T11:30:00.000Z";
const shownThread = () => JSON.parse(readFileSync(new URL("../t3/fixtures/show.json", import.meta.url), "utf8"));
const semonFixture = () => readFileSync(new URL("./fixtures/semon-stalls.json", import.meta.url), "utf8");
const ok = (value: unknown): T3Exit => ({ code: 0, stdout: JSON.stringify(value), stderr: "" });

/** A Semon stall entry as `semon query stalls --json` prints one. */
const semonStall = (branch: string | null, reason = "waiting-question", question: string | null = "Which colour?") => ({
  id: `session-${branch}`,
  harness: "claude",
  branch,
  state: reason,
  stall_reason: reason,
  idle_ms: 1_800_000,
  open_question: question === null ? null : { text: question, asked_at: 1, handoff: null },
});

function setup() {
  const store = Store.open(":memory:", { now: () => NOW });
  const source = store.ensureSource("github", REPO);
  const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: `https://github.com/${REPO}/issues/7` });
  const startWorker = (n: number) => {
    const trigger = store.insertTrigger({
      sourceId: source.id, sourceEventId: `IC_${n}`, threadId: issue, kind: "issue_comment", actor: "alice",
      url: `https://github.com/${REPO}/issues/7#issuecomment-${n}`, occurredAt: NOW,
    }).id;
    const { ticketId } = store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "fix it" });
    const assignmentId = store.createAssignment(ticketId!);
    store.startAssignment(assignmentId, `mcp:worker-${n}`);
    return { ticketId: ticketId!, assignmentId, branch: `ostrom/ticket-${ticketId}-${assignmentId}` };
  };
  // Each Worker thread's fields over the show.json capture; the branch follows the assignment.
  const threads = new Map<string, Record<string, unknown>>();
  const t3 = new T3Cli(async ({ args }) => {
    const threadId = args.at(-1)!;
    const fields = threads.get(threadId);
    if (fields === undefined) return { code: 1, stdout: "", stderr: "server down" };
    const value = shownThread();
    Object.assign(value.thread, { threadId, status: "running", activeRunId: "run:1", updatedAt: fresh }, fields);
    return ok(value);
  });
  let semonStalls: unknown[] = [];
  let semon: T3Runner = async ({ args }) => {
    assert.deepEqual(args, semonStallsArgs(MINUTES));
    return ok({ now: 1, idle_minutes: MINUTES, rule: "…", stalls: semonStalls, window_start: 0 });
  };
  const options = (): WatchdogOptions => ({ store, t3, semon, idleMinutes: MINUTES, now: () => NOW });
  return {
    store,
    startWorker,
    threads,
    options,
    setSemon: (stalls: unknown[]) => (semonStalls = stalls),
    breakSemon: (runner: T3Runner) => (semon = runner),
  };
}

test("Semon's captured stalls output parses to the fields the Watchdog reads", () => {
  const stalls = parseSemonStalls(semonFixture(), 2);
  assert.equal(stalls.length, 1);
  assert.equal(stalls[0]!.branch, "ostrom/ticket-1-1");
  assert.equal(stalls[0]!.stallReason, "waiting-question");
  assert.match(stalls[0]!.openQuestion ?? "", /colour/);
});

test("Semon's output is read strictly", () => {
  const value = JSON.parse(semonFixture());
  assert.throws(() => parseSemonStalls(semonFixture(), 20), /idle_minutes: expected 20/);
  assert.throws(() => parseSemonStalls("semon: oops", 2), /not JSON/);
  delete value.stalls[0].stall_reason;
  assert.throws(() => parseSemonStalls(JSON.stringify(value), 2), /stall_reason: expected a string/);
});

test("the T3 rule: a pending request or a running run with no update for the threshold", () => {
  const thread = (fields: Record<string, unknown>) => {
    const value = shownThread();
    Object.assign(value.thread, fields);
    return parseThreadShow(JSON.stringify(value)).thread;
  };
  const reason = (fields: Record<string, unknown>) => t3StallReason(thread(fields), NOW, MINUTES);
  // The scripted stall read `running` with a pending request, not `waiting`.
  assert.equal(reason({ status: "running", activeRunId: "run:1", pendingRequestCount: 1, updatedAt: stale }), "waiting-request");
  assert.equal(reason({ status: "waiting", activeRunId: null, pendingRequestCount: 1, updatedAt: stale }), "waiting-request");
  assert.equal(reason({ status: "running", activeRunId: "run:1", pendingRequestCount: 0, updatedAt: stale }), "no-update");
  assert.equal(reason({ status: "running", activeRunId: "run:1", pendingRequestCount: 1, updatedAt: fresh }), null);
  assert.equal(reason({ status: "completed", activeRunId: null, pendingRequestCount: 0, updatedAt: stale }), null);
  assert.equal(reason({ status: "waiting", activeRunId: null, pendingRequestCount: 0, updatedAt: stale }), null);
  assert.throws(() => reason({ updatedAt: "yesterday" }), /updatedAt "yesterday"/);
});

test("a Semon stall on a Worker's branch is recorded once per episode", async () => {
  const s = setup();
  const a = s.startWorker(1);
  s.threads.set("mcp:worker-1", { branch: a.branch });
  s.setSemon([semonStall("someone/else"), semonStall(a.branch)]);

  const first = await watchdog(s.options());
  assert.deepEqual(first, {
    checked: [{
      assignmentId: a.assignmentId, t3ThreadId: "mcp:worker-1", semonReason: "waiting-question", t3Reason: null,
      stallId: 1, newEpisode: true, clearedStallId: null,
    }],
    failed: [],
  });
  assert.equal(s.store.getStall(1).detail, "Which colour?");
  assert.deepEqual(s.store.unansweredStalls().map((stall) => stall.id), [1]);

  // Still stalled: the same episode, not a new flag.
  const again = await watchdog(s.options());
  assert.equal(again.checked[0]!.stallId, 1);
  assert.equal(again.checked[0]!.newEpisode, false);

  // Recovered: the episode clears and the Router no longer sees it.
  s.setSemon([]);
  const recovered = await watchdog(s.options());
  assert.equal(recovered.checked[0]!.clearedStallId, 1);
  assert.notEqual(s.store.getStall(1).clearedAt, null);
  assert.deepEqual(s.store.unansweredStalls(), []);

  // Stalled again: a new episode, flagged again.
  s.setSemon([semonStall(a.branch, "no-output", null)]);
  const relapse = await watchdog(s.options());
  assert.equal(relapse.checked[0]!.stallId, 2);
  assert.equal(relapse.checked[0]!.newEpisode, true);
  assert.equal(s.store.getStall(2).semonReason, "no-output");
});

test("the T3 signal flags a stall alone, and both signals are kept when both fire", async () => {
  const s = setup();
  const a = s.startWorker(1);
  const b = s.startWorker(2);
  s.threads.set("mcp:worker-1", { branch: a.branch, pendingRequestCount: 1, updatedAt: stale });
  s.threads.set("mcp:worker-2", { branch: b.branch, updatedAt: stale });
  s.setSemon([semonStall(b.branch, "no-output", null)]);

  const pass = await watchdog(s.options());
  assert.deepEqual(
    pass.checked.map((c) => [c.assignmentId, c.semonReason, c.t3Reason, c.newEpisode]),
    [[a.assignmentId, null, "waiting-request", true], [b.assignmentId, "no-output", "no-update", true]],
  );
});

test("with Semon unreadable the T3 signal still runs, the pass fails, and no episode clears", async () => {
  const s = setup();
  const a = s.startWorker(1);
  const b = s.startWorker(2);
  s.threads.set("mcp:worker-1", { branch: a.branch, pendingRequestCount: 1, updatedAt: stale });
  s.threads.set("mcp:worker-2", { branch: b.branch });
  s.setSemon([semonStall(b.branch)]);
  await watchdog(s.options()); // b's episode opens on Semon's word
  assert.deepEqual(s.store.unansweredStalls().map((stall) => stall.assignmentId), [a.assignmentId, b.assignmentId]);

  s.breakSemon(async () => ({ code: 1, stdout: "", stderr: "semon: query stalls: cannot read ~/.claude" }));
  const pass = await watchdog(s.options());
  assert.deepEqual(pass.failed, [
    { what: "semon stalls", error: "semon query stalls exited 1: semon: query stalls: cannot read ~/.claude" },
  ]);
  assert.deepEqual(pass.checked.map((c) => [c.t3Reason, c.stallId, c.clearedStallId]), [
    ["waiting-request", 1, null],
    [null, null, null],
  ]);
  assert.equal(s.store.getStall(2).clearedAt, null);

  // A missing binary is the same: reported, not fatal.
  s.breakSemon(async () => {
    throw Object.assign(new Error("spawn semon ENOENT"), { code: "ENOENT" });
  });
  const missing = await watchdog(s.options());
  assert.match(missing.failed[0]!.error, /ENOENT/);
  assert.equal(missing.checked.length, 2);
});

test("an unreadable Worker thread is reported and the pass goes on", async () => {
  const s = setup();
  s.startWorker(1);
  const b = s.startWorker(2);
  s.threads.set("mcp:worker-2", { branch: b.branch, pendingRequestCount: 1, updatedAt: stale });
  const pass = await watchdog(s.options());
  assert.deepEqual(pass.failed, [{ what: "read assignment 1", error: "t3 thread show exited 1: server down" }]);
  assert.deepEqual(pass.checked.map((c) => c.assignmentId), [b.assignmentId]);
});

test("a stall answered wait stays answered for its episode; stop ends the work", () => {
  const s = setup();
  const a = s.startWorker(1);
  const b = s.startWorker(2);
  const first = s.store.recordStall({ assignmentId: a.assignmentId, semonReason: "waiting-question", t3Reason: null, detail: "?" });
  assert.deepEqual(s.store.recordStall({ assignmentId: a.assignmentId, semonReason: null, t3Reason: "no-update", detail: null }), {
    id: first.id,
    duplicate: true,
  });
  assert.deepEqual(s.store.answerStall(first.id, "wait", null), { replyId: null, ticketId: a.ticketId, ticketEnded: false });
  assert.throws(() => s.store.answerStall(first.id, "stop", "x"), /already answered wait/);
  assert.deepEqual(s.store.unansweredStalls(), []);
  assert.equal(s.store.getAssignment(a.assignmentId).state, "running");

  const second = s.store.recordStall({ assignmentId: b.assignmentId, semonReason: null, t3Reason: "waiting-request", detail: null });
  assert.deepEqual(s.store.answerStall(second.id, "stop", "The Worker stopped."), {
    replyId: `stall-${second.id}`,
    ticketId: b.ticketId,
    ticketEnded: true,
  });
  assert.equal(s.store.getAssignment(b.assignmentId).state, "ended");
  assert.equal(s.store.getTicket(b.ticketId).state, "ended");
  assert.deepEqual(s.store.dueReplies().filter((r) => r.id.startsWith("stall-")).map((r) => r.body), ["The Worker stopped."]);
  assert.throws(
    () => s.store.recordStall({ assignmentId: b.assignmentId, semonReason: null, t3Reason: "no-update", detail: null }),
    /assignment 2 is ended/,
  );
});
