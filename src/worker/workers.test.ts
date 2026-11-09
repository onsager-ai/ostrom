import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { Store } from "../store/store.ts";
import { FakeRunner } from "./fake-runner.ts";
import { workerBrief } from "./dispatch.ts";
import { INTERRUPTED_PROMPT, Workers, type WorkersDeps } from "./workers.ts";

const REPO = "onsager-ai/ostrom";
const config = { repo: "/repo", base: "main", worktrees: "/wt", harness: "codex" as const, model: "gpt-6-luna", machine: "host" };

function fixture(extra: Partial<WorkersDeps> = {}, harness: "claude" | "codex" = "codex") {
  const store = Store.open(":memory:", { now: () => new Date("2026-11-09T12:00:00.000Z") });
  const source = store.ensureSource("github", REPO);
  const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: `https://github.com/${REPO}/issues/7` });
  const trigger = store.insertTrigger({
    sourceId: source.id, sourceEventId: "IC_1", threadId: issue, kind: "issue_comment", actor: "alice",
    url: `https://github.com/${REPO}/issues/7#issuecomment-1`, occurredAt: new Date("2026-11-09T11:59:00.000Z"),
  }).id;
  const { ticketId } = store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "fix the README typo" });
  const runner = new FakeRunner();
  const worktrees: string[][] = [];
  const addWorktree: WorkersDeps["addWorktree"] = async (repo, path, base, ticket, assignment) => {
    worktrees.push([repo, path, base]);
    return `ostrom/ticket-${ticket}-${assignment}`;
  };
  const make = () => new Workers({ store, config: { ...config, harness }, runners: { claude: runner, codex: runner }, addWorktree, ...extra });
  return { store, runner, worktrees, ticketId: ticketId!, workers: make(), make };
}

test("dispatch makes the worktree, starts the session with the brief and stores its id", async () => {
  const { store, runner, worktrees, ticketId, workers } = fixture();
  runner.hold = true;
  const dispatched = await workers.dispatch(ticketId);
  assert.deepEqual(dispatched, { assignmentId: 1, sessionId: "sess-1", branch: `ostrom/ticket-${ticketId}-1` });
  assert.deepEqual(worktrees, [["/repo", `/wt/ticket-${ticketId}-1`, "main"]]);
  const a = store.getAssignment(1);
  assert.deepEqual(
    [a.state, a.harness, a.model, a.sessionId, a.worktree, a.branch, a.machine, a.turns],
    ["running", "codex", "gpt-6-luna", "sess-1", `/wt/ticket-${ticketId}-1`, dispatched.branch, "host", 1],
  );
  assert.notEqual(a.turnStartedAt, null, "the first turn is still running");
  const session = runner.sessions[0]!;
  assert.equal(session.options.cwd, `/wt/ticket-${ticketId}-1`);
  assert.equal(session.options.model, "gpt-6-luna");
  assert.deepEqual(session.prompts, [workerBrief(store.ticketContext(ticketId), dispatched.branch, "main")]);

  session.release();
  await workers.drain();
  const done = store.getAssignment(1);
  assert.deepEqual([done.turnStartedAt, done.inputTokens, done.cachedInputTokens, done.outputTokens, done.outcome], [null, 10, 5, 2, null]);
});

test("the session starts on the configured harness", async () => {
  const claude = new FakeRunner();
  const codex = new FakeRunner();
  const { store, ticketId } = fixture();
  const workers = new Workers({ store, config: { ...config, harness: "claude", model: "claude-haiku-5-5" }, runners: { claude, codex }, addWorktree: async () => "b" });
  await workers.dispatch(ticketId);
  await workers.drain();
  assert.equal(claude.sessions.length, 1);
  assert.equal(codex.sessions.length, 0);
  assert.equal(store.getAssignment(1).harness, "claude");
});

test("a session that does not start ends the assignment with the reason and rethrows", async () => {
  const { store, runner, ticketId, workers } = fixture();
  runner.startFailure = new Error("no login");
  await assert.rejects(workers.dispatch(ticketId), /no login/);
  const a = store.getAssignment(1);
  assert.deepEqual([a.state, a.endReason, a.sessionId], ["ended", "no login", null]);
  assert.equal(store.abandonedAssignments().length, 1);
});

test("a worktree that cannot be made ends the assignment with the reason", async () => {
  const { store, ticketId, workers } = fixture({ addWorktree: async () => { throw new Error("branch exists"); } });
  await assert.rejects(workers.dispatch(ticketId), /branch exists/);
  assert.equal(store.getAssignment(1).endReason, "branch exists");
});

test("dispatch refuses an ended ticket without starting anything", async () => {
  const { store, runner, ticketId, workers } = fixture();
  store.endTicket(ticketId);
  await assert.rejects(workers.dispatch(ticketId), /takes no assignment/);
  assert.deepEqual(runner.sessions, []);
});

test("a turn that fails records a failed outcome; the next good turn clears it", async () => {
  const { store, runner, ticketId, workers } = fixture();
  runner.hold = false;
  await workers.dispatch(ticketId);
  await workers.drain();
  const session = () => runner.sessions.at(-1)!;
  session().failWith = new Error("boom");
  workers.deliver(1, "k1", "go", () => {});
  await workers.drain();
  assert.equal(store.getAssignment(1).outcome, "failed");
  assert.equal(store.getAssignment(1).turnStartedAt, null);
  session().failWith = undefined;
  workers.deliver(1, "k2", "again", () => {});
  await workers.drain();
  assert.equal(store.getAssignment(1).outcome, null);
  assert.equal(store.getAssignment(1).turns, 3);
});

test("a first turn that fails after the session started is a failed outcome, not an abandon", async () => {
  const { store, runner, ticketId, workers } = fixture();
  runner.failAfterId = new Error("crashed");
  await workers.dispatch(ticketId);
  await workers.drain();
  const a = store.getAssignment(1);
  assert.deepEqual([a.state, a.outcome, a.turnStartedAt], ["running", "failed", null]);
});

test("restart recovery resumes running sessions by id and sends the interrupted one a continue turn", async () => {
  const { store, runner, ticketId, workers, make } = fixture();
  runner.hold = true;
  await workers.dispatch(ticketId); // the first turn never finishes: ostromd "dies" here
  const second = store.createAssignment(store.getAssignment(1).ticketId);
  store.configureNativeAssignment(second, { harness: "codex", model: "m", worktree: "/w2", branch: "b2", machine: "h" });
  store.startNativeAssignment(second, "idle-session");
  const stuck = store.createAssignment(store.getAssignment(1).ticketId);
  store.configureNativeAssignment(stuck, { harness: "codex", model: "m", worktree: "/w3", branch: "b3", machine: "h" });

  const restarted = make();
  runner.hold = false;
  const pass = restarted.recover();
  assert.deepEqual(pass, { resumed: [1, second], interrupted: [1], abandoned: [stuck] });
  await restarted.drain();
  const resumed = runner.sessions.filter((s) => s.resumed);
  assert.deepEqual(resumed.map((s) => s.id), ["sess-1", "idle-session"]);
  assert.deepEqual(resumed[0]?.prompts, [INTERRUPTED_PROMPT]);
  assert.deepEqual(resumed[1]?.prompts, []);
  const a = store.getAssignment(1);
  assert.deepEqual([a.interruptions, a.turnStartedAt, a.state], [1, null, "running"]);
  assert.equal(store.getAssignment(second).interruptions, 0);
  assert.equal(store.getAssignment(stuck).state, "ended");
  assert.match(store.getAssignment(stuck).endReason!, /before the session started/);
});

function rollout(model: string | null, thread = "sess-1") {
  const dir = mkdtempSync(join(tmpdir(), "workers-"));
  if (model !== null) {
    mkdirSync(join(dir, "2026", "11", "09"), { recursive: true });
    writeFileSync(
      join(dir, "2026", "11", "09", `rollout-2026-11-09T01-02-03-${thread}.jsonl`),
      JSON.stringify({ type: "turn_context", payload: { model } }) + "\n",
    );
  }
  return dir;
}

test("Codex: the recorded model matches, so the assignment carries on", async () => {
  const dir = rollout("gpt-6-luna");
  try {
    const { store, ticketId, workers } = fixture({ codexSessions: dir });
    await workers.dispatch(ticketId);
    await workers.drain();
    assert.equal(store.getAssignment(1).state, "running");
  } finally {
    rmSync(dir, { recursive: true });
  }
});

test("Codex: a different model or an unreadable record ends the assignment with a recorded reason", async () => {
  for (const [model, reason] of [["gpt-6-sol", /model mismatch: asked for gpt-6-luna, ran gpt-6-sol/], [null, /model unverified: no rollout record/]] as const) {
    const dir = rollout(model);
    try {
      const { store, ticketId, workers } = fixture({ codexSessions: dir });
      await workers.dispatch(ticketId);
      await workers.drain();
      const a = store.getAssignment(1);
      assert.equal(a.state, "ended");
      assert.match(a.endReason!, reason);
      assert.equal(store.abandonedAssignments().length, 1, "the Router owes a reply");
    } finally {
      rmSync(dir, { recursive: true });
    }
  }
});

test("Claude turns are not checked against a rollout", async () => {
  const { store, ticketId, workers } = fixture({ codexSessions: "/does/not/exist" }, "claude");
  await workers.dispatch(ticketId);
  await workers.drain();
  assert.equal(store.getAssignment(1).state, "running");
});
