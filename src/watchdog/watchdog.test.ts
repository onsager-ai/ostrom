import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { afterEach, test } from "node:test";

import type { ProcessRunner } from "../process.ts";
import { Store } from "../store/store.ts";
import { parseSemonStalls, semonStallsArgs, watchdog, type WatchdogOptions } from "./watchdog.ts";

const REPO = "onsager-ai/ostrom";
const NOW = new Date("2026-11-09T12:00:00.000Z");
const MINUTES = 20;
const semonFixture = () => readFileSync(new URL("./fixtures/semon-stalls.json", import.meta.url), "utf8");
const ok = (value: unknown) => ({ code: 0, stdout: JSON.stringify(value), stderr: "" });
const stores: Store[] = [];

afterEach(() => {
  while (stores.length > 0) stores.pop()!.close();
});

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
  stores.push(store);
  const source = store.ensureSource("github", REPO);
  const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: `https://github.com/${REPO}/issues/7` });
  const startWorker = (n: number) => {
    const trigger = store.insertTrigger({
      sourceId: source.id, sourceEventId: `IC_${n}`, threadId: issue, kind: "issue_comment", actor: "alice",
      url: `https://github.com/${REPO}/issues/7#issuecomment-${n}`, occurredAt: NOW,
    }).id;
    const { ticketId } = store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "fix it" });
    const assignmentId = store.createAssignment(ticketId!);
    store.configureNativeAssignment(assignmentId, {
      harness: "codex", model: "gpt-6-luna", worktree: `/wt/${assignmentId}`,
      branch: `ostrom/ticket-${ticketId}-${assignmentId}`, machine: "host",
    });
    store.startNativeAssignment(assignmentId, `session-${n}`);
    return { ticketId: ticketId!, assignmentId, branch: `ostrom/ticket-${ticketId}-${assignmentId}` };
  };
  let semonStalls: unknown[] = [];
  let semon: ProcessRunner = async ({ args }) => {
    assert.deepEqual(args, semonStallsArgs(MINUTES));
    return ok({ now: 1, idle_minutes: MINUTES, rule: "…", stalls: semonStalls, window_start: 0 });
  };
  const options = (): WatchdogOptions => ({ store, semon, idleMinutes: MINUTES, now: () => NOW });
  return { store, startWorker, options, setSemon: (stalls: unknown[]) => (semonStalls = stalls), breakSemon: (runner: ProcessRunner) => (semon = runner) };
}

test("Semon's captured stalls output parses to the fields the Watchdog reads", () => {
  const stalls = parseSemonStalls(semonFixture(), 2);
  assert.equal(stalls.length, 1);
  assert.equal(stalls[0]!.branch, "ostrom/ticket-1-1");
  assert.equal(stalls[0]!.stallReason, "waiting-question");
});

test("Semon's output is read strictly", () => {
  const value = JSON.parse(semonFixture());
  assert.throws(() => parseSemonStalls(semonFixture(), 20), /idle_minutes: expected 20/);
  assert.throws(() => parseSemonStalls("semon: oops", 2), /not JSON/);
  delete value.stalls[0].stall_reason;
  assert.throws(() => parseSemonStalls(JSON.stringify(value), 2), /stall_reason: expected a string/);
});

test("a Semon stall on a native Worker's branch is recorded once per episode", async () => {
  const s = setup();
  const a = s.startWorker(1);
  s.setSemon([semonStall("someone/else"), semonStall(a.branch)]);
  const first = await watchdog(s.options());
  assert.deepEqual(first, { checked: [{ assignmentId: a.assignmentId, semonReason: "waiting-question", stallId: 1, newEpisode: true, clearedStallId: null }], failed: [] });
  assert.equal(s.store.getStall(1).detail, "Which colour?");
  assert.equal((await watchdog(s.options())).checked[0]!.newEpisode, false);
  s.setSemon([]);
  assert.equal((await watchdog(s.options())).checked[0]!.clearedStallId, 1, "the fixture clock keeps the idle budget below its limit");
  s.setSemon([semonStall(a.branch, "no-output", null)]);
  s.store.beginTurn(a.assignmentId);
  const later = new Date(NOW.getTime() + MINUTES * 60_000);
  assert.equal((await watchdog({ ...s.options(), now: () => later })).checked[0]!.stallId, 2);
});

test("Semon's idle verdicts need a turn that has itself been idle; a question counts between turns", async () => {
  const s = setup();
  const a = s.startWorker(1);
  // Between turns: the Worker finished and waits for review. No output is expected, not a stall.
  for (const reason of ["no-output", "process-gone"]) {
    s.setSemon([semonStall(a.branch, reason, null)]);
    assert.equal((await watchdog(s.options())).checked[0]!.stallId, null, reason);
  }
  // A turn that has just started: Semon's clock still counts from the session's last write.
  s.store.beginTurn(a.assignmentId);
  s.setSemon([semonStall(a.branch, "no-output", null)]);
  assert.equal((await watchdog(s.options())).checked[0]!.stallId, null);
  // An unanswered question between turns is the scripted stall, and counts.
  s.store.endTurn(a.assignmentId, null);
  s.setSemon([semonStall(a.branch)]);
  assert.equal((await watchdog(s.options())).checked[0]!.semonReason, "waiting-question");
});

test("an unreadable Semon read fails without clearing open episodes", async () => {
  const s = setup();
  const a = s.startWorker(1);
  s.setSemon([semonStall(a.branch)]);
  await watchdog(s.options());
  s.breakSemon(async () => ({ code: 1, stdout: "", stderr: "no logs" }));
  const pass = await watchdog(s.options());
  assert.deepEqual(pass.failed, [{ what: "semon stalls", error: "semon query stalls exited 1: no logs" }]);
  assert.equal(s.store.getStall(1).clearedAt, null);
});

test("a stall answered wait stays answered for its episode; stop ends the work", () => {
  const s = setup();
  const a = s.startWorker(1);
  const b = s.startWorker(2);
  const first = s.store.recordStall({ assignmentId: a.assignmentId, semonReason: "waiting-question", detail: "?" });
  assert.deepEqual(s.store.answerStall(first.id, "wait", null), { replyId: null, ticketId: a.ticketId, ticketEnded: false });
  const second = s.store.recordStall({ assignmentId: b.assignmentId, semonReason: "no-output", detail: null });
  assert.deepEqual(s.store.answerStall(second.id, "stop", "The Worker stopped."), { replyId: `stall-${second.id}`, ticketId: b.ticketId, ticketEnded: true });
});
