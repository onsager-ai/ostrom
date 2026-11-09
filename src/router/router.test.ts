import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { FakeGitHub } from "../github/fake-github.ts";
import { pollOnce } from "../github/poller.ts";
import { Store } from "../store/store.ts";
import { FakeRunner } from "../worker/fake-runner.ts";
import { routeFollowUps } from "../worker/follow-up.ts";
import { Workers } from "../worker/workers.ts";
import { T3Cli, type T3Exit, type T3Invocation } from "../t3/runner.ts";
import {
  bodyPath,
  parseRouterAnswer,
  routeCandidates,
  RouterOutputError,
  RouterRunError,
  type Batch,
  type RouteOptions,
  type RouterAnswer,
} from "./router.ts";

const workerConfig = { repo: "/repo", base: "main", worktrees: "/wt", harness: "codex" as const, model: "gpt-6-luna", machine: "host" };
const router = { project: "/work/ostrom", provider: "claudeAgent", model: "claude-haiku-5-5" };
const shownThread = () => JSON.parse(readFileSync(new URL("../t3/fixtures/show.json", import.meta.url), "utf8"));
const ok = (value: unknown): T3Exit => ({ code: 0, stdout: JSON.stringify(value), stderr: "" });

/** The batch a Router message carries, as the model would read it. */
function batchOf(message: string): Batch {
  const marker = "The batch:\n";
  return JSON.parse(message.slice(message.indexOf(marker) + marker.length));
}

const acceptAll = (batch: Batch): RouterAnswer => ({
  decisions: batch.threads.map((thread) => ({
    thread: thread.thread,
    triggers: thread.candidates.map((candidate) => candidate.trigger),
    verdict: "accept" as const,
    reason: "fix the README typo",
    reply: "On it: a Worker is starting.",
  })),
  outcomes: batch.outcomes.map((outcome) => ({ key: outcome.key, reply: `Update: ${outcome.event}` })),
  stalls: batch.stalls.map((stall) => ({ key: stall.key, action: "wait" as const, reply: null })),
});

/**
 * A stand-in `t3`. `start --wait` and `send --wait` are Router turns answered by `answer`;
 * `show` reads a Router thread, and a thread it does not know
 * is gone, worded as T3 words it at the pin.
 */
class FakeT3 {
  readonly calls: T3Invocation[] = [];
  readonly routers = new Set<string>();
  answer: (batch: Batch) => RouterAnswer | string = acceptAll;
  routerStatus: "completed" | "failed" = "completed";
  showExit: T3Exit | undefined;
  #threads = 0;
  #runs = 0;

  get routerTurns(): T3Invocation[] {
    return this.calls.filter((call) => call.args.includes("--wait"));
  }

  readonly cli = new T3Cli(async (invocation) => {
    this.calls.push(invocation);
    const [, verb] = invocation.args;
    if (verb === "start" || verb === "send") {
      let threadId: string;
      if (verb === "start") {
        threadId = `mcp:router-${++this.#threads}`;
        this.routers.add(threadId);
      } else {
        threadId = invocation.args[4]!;
        if (!this.routers.has(threadId)) return { code: 1, stdout: "", stderr: `Thread ${threadId} was not found.\n` };
      }
      const runId = `run:${++this.#runs}`;
      if (this.routerStatus === "failed") {
        return { code: 1, stdout: JSON.stringify({ threadId, runId, status: "failed", reply: null, error: "boom" }), stderr: "" };
      }
      const answer = this.answer(batchOf(invocation.stdin!));
      const reply = typeof answer === "string" ? answer : JSON.stringify(answer);
      return ok({ threadId, runId, status: "completed", reply, error: null });
    }
    assert.equal(verb, "show");
    if (this.showExit !== undefined) return this.showExit;
    const threadId = invocation.args.at(-1)!;
    const fields = this.routers.has(threadId) ? { status: "completed" } : undefined;
    if (fields === undefined) return { code: 1, stdout: "", stderr: `Thread ${threadId} is no longer available.\n` };
    const value = shownThread();
    Object.assign(value.thread, { threadId, linkedPullRequest: null, pullRequests: [] }, fields);
    return ok(value);
  });
}

const NOW = new Date("2026-11-09T12:00:00.000Z");

function setup(dbPath = ":memory:") {
  const now = () => NOW;
  const github = new FakeGitHub(now);
  github.issue(7);
  github.issue(8);
  const t3 = new FakeT3();
  const runner = new FakeRunner();
  let store = Store.open(dbPath, { now });
  const addWorktree = async (_repo: string, _path: string, _base: string, ticket: number, assignment: number) =>
    `ostrom/ticket-${ticket}-${assignment}`;
  const workers = () => new Workers({ store, config: workerConfig, runners: { claude: runner, codex: runner }, addWorktree });
  const options = (): RouteOptions => ({ store, gh: github.gh, t3: t3.cli, repo: github.repo, workers: workers(), router });
  const comment = (body: string, minute = 58, issue = 7) =>
    github.comment(issue, { login: "alice", body, at: new Date(`2026-11-09T11:${minute}:00Z`) });
  const poll = () => pollOnce({ store, gh: github.gh, repo: github.repo, allowlist: ["alice"], now });
  const reopen = () => {
    store.close();
    store = Store.open(dbPath, { now });
  };
  return { github, t3, runner, workers, store: () => store, options, comment, poll, reopen };
}

const pr = (number: number, state: string) => ({
  host: "github.com",
  repository: "onsager-ai/ostrom",
  number,
  url: `https://github.com/onsager-ai/ostrom/pull/${number}`,
  source: "created",
  state,
});

test("an accept records a ticket, dispatches a Worker and queues the Router's acknowledgement", async () => {
  const s = setup();
  s.comment("please fix the typo in the README");
  await s.poll();
  const pass = await routeCandidates(s.options());

  assert.deepEqual(pass.router, { t3ThreadId: "mcp:router-1", started: true });
  assert.equal(s.store().routerThread(), "mcp:router-1");
  const sent = batchOf(s.t3.routerTurns[0]!.stdin!);
  assert.equal(sent.threads[0]?.candidates[0]?.excerpt, "please fix the typo in the README");
  assert.deepEqual(s.t3.routerTurns[0]!.args.slice(0, 8), [
    "thread", "start", "--title", "Ostrom Router", "--provider", "claudeAgent", "--model", "claude-haiku-5-5",
  ]);

  assert.equal(pass.decisions.length, 1);
  const decision = pass.decisions[0]!;
  assert.equal(decision.verdict, "accept");
  assert.equal(decision.replyId, `decision-${decision.decisionId}`);
  assert.deepEqual(pass.dispatched.map((d) => d.ticketId), [decision.ticketId]);
  assert.equal(s.store().runningAssignments()[0]?.sessionId, "sess-1");
  assert.deepEqual(
    s.store().dueReplies().map((reply) => [reply.id, reply.body]),
    [[decision.replyId, "On it: a Worker is starting."]],
  );
  assert.deepEqual(s.store().undecidedTriggers(), []);
  assert.deepEqual(pass.failed, []);

  // Nothing new and no outcome: the Router is not asked again.
  const again = await routeCandidates(s.options());
  assert.equal(again.router, null);
  assert.equal(s.t3.routerTurns.length, 1);
});

test("an ignore is recorded with no ticket, no Worker and no reply", async () => {
  const s = setup();
  s.comment("thanks!");
  await s.poll();
  s.t3.answer = (batch) => ({
    decisions: [
      {
        thread: batch.threads[0]!.thread,
        triggers: batch.threads[0]!.candidates.map((c) => c.trigger),
        verdict: "ignore",
        reason: "a thank-you, no request",
        reply: null,
      },
    ],
    outcomes: [],
    stalls: [],
  });
  const pass = await routeCandidates(s.options());
  assert.deepEqual(pass.decisions.map((d) => [d.verdict, d.ticketId, d.replyId]), [["ignore", null, null]]);
  assert.deepEqual(pass.dispatched, []);
  assert.deepEqual(s.store().dueReplies(), []);
  assert.equal(s.t3.calls.filter((call) => call.args[1] === "start" && !call.args.includes("--wait")).length, 0);
  assert.deepEqual(s.store().undecidedTriggers(), []);
});

test("invalid Router output is a loud failure and its candidates stay undecided", async () => {
  const s = setup();
  s.comment("please fix the typo");
  await s.poll();
  s.t3.answer = (batch) => `Sure! ${JSON.stringify(acceptAll(batch))}`;
  await assert.rejects(routeCandidates(s.options()), RouterOutputError);
  assert.equal(s.store().undecidedTriggers().length, 1);
  assert.deepEqual(s.store().dueReplies(), []);
  assert.deepEqual(s.store().undispatchedTickets(), []);

  // The next valid answer decides the same candidate.
  s.t3.answer = acceptAll;
  const pass = await routeCandidates(s.options());
  assert.equal(pass.decisions.length, 1);
});

test("the answer must decide each candidate once, on its thread, and answer exactly the outcomes", () => {
  const batch: Batch = {
    threads: [
      {
        thread: 1,
        kind: "issue",
        number: 7,
        url: "u",
        candidates: [
          { trigger: 10, kind: "issue_comment", actor: "a", url: "u", occurredAt: "t", excerpt: "x", truncated: false },
          { trigger: 11, kind: "issue_comment", actor: "a", url: "u", occurredAt: "t", excerpt: "y", truncated: false },
        ],
      },
    ],
    outcomes: [
      {
        key: "outcome-a1-failed",
        event: "failed",
        terminal: true,
        assignmentId: 1,
        ticketId: 1,
        thread: 1,
        threadUrl: "u",
        request: "r",
        pullRequest: null,
        detail: null,
      },
    ],
    stalls: [
      {
        key: "stall-3",
        stallId: 3,
        assignmentId: 1,
        ticketId: 1,
        thread: 1,
        threadUrl: "u",
        request: "r",
        semonReason: "waiting-question",
        t3Reason: null,
        detail: "Which colour?",
        since: "t",
      },
    ],
  };
  const decision = (triggers: number[], extra: Record<string, unknown> = {}) => ({
    thread: 1,
    triggers,
    verdict: "ignore",
    reason: "chatter",
    reply: null,
    ...extra,
  });
  const outcomes = [{ key: "outcome-a1-failed", reply: "It failed." }];
  const stalls = [{ key: "stall-3", action: "wait", reply: null }];
  const parse = (value: unknown) => () => parseRouterAnswer(JSON.stringify(value), batch);

  const parsed = parseRouterAnswer(JSON.stringify({ decisions: [decision([10, 11])], outcomes, stalls }), batch);
  assert.equal(parsed.decisions.length, 1);
  assert.deepEqual(parsed.stalls, [{ key: "stall-3", action: "wait", reply: null }]);
  assert.throws(parse({ decisions: [decision([10])], outcomes, stalls }), /11 are not decided/);
  assert.throws(parse({ decisions: [decision([10, 11]), decision([11])], outcomes, stalls }), /decided twice/);
  assert.throws(parse({ decisions: [decision([10, 11, 12])], outcomes, stalls }), /12 is not a candidate/);
  assert.throws(parse({ decisions: [decision([10, 11], { thread: 2 })], outcomes, stalls }), /is on thread 1, not 2/);
  assert.throws(parse({ decisions: [decision([10, 11], { verdict: "accept" })], outcomes, stalls }), /an accept needs a reply/);
  assert.throws(parse({ decisions: [decision([10, 11], { verdict: "maybe" })], outcomes, stalls }), /verdict/);
  assert.throws(parse({ decisions: [decision([10, 11], { reason: " " })], outcomes, stalls }), /reason/);
  assert.throws(parse({ decisions: [decision([10, 11], { extra: 1 })], outcomes, stalls }), /unexpected field extra/);
  assert.throws(parse({ decisions: [decision([10, 11])], outcomes: [], stalls }), /outcome-a1-failed have no reply/);
  assert.throws(
    parse({ decisions: [decision([10, 11])], outcomes: [...outcomes, { key: "outcome-a9-failed", reply: "x" }], stalls }),
    /not an outcome of this batch/,
  );
  assert.throws(parse({ decisions: [decision([10, 11])], stalls }), /missing field outcomes/);

  // Stalls: exactly the batch's, each wait or stop, a stop with a reply, the reply always present.
  const answer = (stallAnswers: unknown) => ({ decisions: [decision([10, 11])], outcomes, stalls: stallAnswers });
  assert.deepEqual(
    parseRouterAnswer(JSON.stringify(answer([{ key: "stall-3", action: "stop", reply: "Stopped." }])), batch).stalls,
    [{ key: "stall-3", action: "stop", reply: "Stopped." }],
  );
  assert.throws(parse({ decisions: [decision([10, 11])], outcomes }), /missing field stalls/);
  assert.throws(parse(answer([])), /stalls stall-3 have no answer/);
  assert.throws(parse(answer([...stalls, ...stalls])), /stall-3 is answered twice/);
  assert.throws(parse(answer([{ key: "stall-9", action: "wait", reply: null }])), /not a stall of this batch/);
  assert.throws(parse(answer([{ key: "stall-3", action: "stop", reply: null }])), /a stop needs a reply/);
  assert.throws(parse(answer([{ key: "stall-3", action: "end", reply: null }])), /action: expected wait or stop/);
  assert.throws(parse(answer([{ key: "stall-3", action: "wait" }])), /missing field reply/);
  assert.throws(parse(answer([{ key: "stall-3", action: "wait", reply: null, why: "x" }])), /unexpected field why/);
  assert.throws(() => parseRouterAnswer("```json\n{}\n```", batch), /not JSON/);
});

test("a crash between poll and routing loses nothing: the trigger is a candidate on the next pass", async () => {
  const dir = mkdtempSync(join(tmpdir(), "ostrom-router-"));
  try {
    const s = setup(join(dir, "ostrom.sqlite"));
    s.comment("please fix the typo");
    const polled = await s.poll();
    assert.equal(polled.candidates.length, 1);
    s.reopen(); // the poll's in-memory candidates are gone with the process

    s.t3.routerStatus = "failed";
    await assert.rejects(routeCandidates(s.options()), RouterRunError);
    assert.equal(s.store().undecidedTriggers().length, 1);
    s.reopen();

    s.t3.routerStatus = "completed";
    const pass = await routeCandidates(s.options());
    assert.deepEqual(pass.decisions.map((d) => d.triggers), [[polled.candidates[0]!.triggerId]]);
    // The failed first run still created the Router thread; the second pass reused it.
    assert.deepEqual(pass.router, { t3ThreadId: "mcp:router-1", started: false });
    s.store().close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an accept whose dispatch a crash cut off is dispatched on the next pass", async () => {
  const s = setup();
  s.comment("please fix the typo");
  await s.poll();
  const [trigger] = s.store().undecidedTriggers();
  const { ticketId } = s.store().recordDecision({
    threadId: trigger!.threadId,
    triggerIds: [trigger!.id],
    verdict: "accept",
    reason: "fix it",
    reply: "On it.",
  });
  const pass = await routeCandidates(s.options());
  assert.equal(pass.router, null);
  assert.deepEqual(pass.dispatched.map((d) => d.ticketId), [ticketId]);
});

test("each outcome gets one reply, and a merged PR ends the assignment and the ticket", async () => {
  const s = setup();
  s.comment("please fix the typo");
  await s.poll();
  const first = await routeCandidates(s.options());
  const ticketId = first.decisions[0]!.ticketId!;
  const { assignmentId, branch } = first.dispatched[0]!;
  const turns = () => s.t3.routerTurns.length;

  s.github.pulls.push({ branch, number: 12, state: "open" });
  const opened = await routeCandidates(s.options());
  assert.deepEqual(opened.outcomes, [
    { key: `outcome-a${assignmentId}-pr12-opened`, event: "pr_opened", ticketId, ticketEnded: false },
  ]);
  assert.equal(batchOf(s.t3.routerTurns.at(-1)!.stdin!).outcomes[0]?.pullRequest, pr(12, "open").url);
  assert.equal(s.store().getTicket(ticketId).state, "open");
  assert.equal(turns(), 2);

  // The same state again owes nothing: the Router is not asked.
  assert.equal((await routeCandidates(s.options())).router, null);
  assert.equal(turns(), 2);

  s.github.pulls[0]!.state = "merged";
  const merged = await routeCandidates(s.options());
  assert.deepEqual(merged.outcomes, [
    { key: `outcome-a${assignmentId}-pr12-merged`, event: "pr_merged", ticketId, ticketEnded: true },
  ]);
  assert.equal(s.store().getAssignment(assignmentId).state, "ended");
  assert.equal(s.store().getTicket(ticketId).state, "ended");
  assert.equal(turns(), 3);

  assert.equal((await routeCandidates(s.options())).router, null);
  assert.equal(turns(), 3);
  const outcomeReplies = s.store().dueReplies().filter((reply) => reply.id.startsWith("outcome-"));
  assert.deepEqual(outcomeReplies.map((reply) => reply.body).sort(), ["Update: pr_merged", "Update: pr_opened"]);
});

test("a Worker that failed to start gets one failure reply, and its ticket ends", async () => {
  const s = setup();
  s.comment("please fix the typo");
  await s.poll();
  s.runner.startFailure = new Error("Project not found.");
  const first = await routeCandidates(s.options());
  assert.match(first.failed[0]?.error ?? "", /Project not found/);
  const ticketId = first.decisions[0]!.ticketId!;

  const next = await routeCandidates(s.options());
  assert.equal(next.outcomes.length, 1);
  assert.equal(next.outcomes[0]?.event, "failed");
  assert.equal(next.outcomes[0]?.ticketEnded, true);
  assert.match(batchOf(s.t3.routerTurns.at(-1)!.stdin!).outcomes[0]?.detail ?? "", /Project not found/);
  assert.equal(s.store().getTicket(ticketId).state, "ended");
  assert.deepEqual(next.failed, []);
  assert.equal((await routeCandidates(s.options())).router, null);
});

test("a Router thread T3 no longer has is replaced by a new one; any other error is not", async () => {
  const s = setup();
  s.comment("please fix the typo", 57);
  await s.poll();
  await routeCandidates(s.options());
  assert.equal(s.store().routerThread(), "mcp:router-1");

  s.t3.routers.delete("mcp:router-1");
  s.comment("and please fix the CONTRIBUTING typo", 58, 8);
  await s.poll();
  s.t3.showExit = { code: 1, stdout: "", stderr: "connect ECONNREFUSED 127.0.0.1:3773" };
  await assert.rejects(routeCandidates(s.options()), /ECONNREFUSED/);
  assert.equal(s.store().routerThread(), "mcp:router-1");

  s.t3.showExit = undefined;
  const pass = await routeCandidates(s.options());
  assert.equal(pass.router?.started, true);
  assert.equal(s.store().routerThread(), pass.router?.t3ThreadId);
  assert.notEqual(pass.router?.t3ThreadId, "mcp:router-1");
  assert.equal(pass.decisions.length, 1);
});

test("a follow-up on the accepted issue goes to its Worker, not to the Router", async () => {
  const s = setup();
  s.comment("please fix the typo", 57);
  await s.poll();
  const first = await routeCandidates(s.options());
  const { ticketId } = first.decisions[0]!;
  const { assignmentId } = first.dispatched[0]!;
  const issue = first.decisions[0]!.thread;
  assert.deepEqual(s.store().ticketsForThread(issue).map((t) => t.id), [ticketId]);
  // The accepted trigger has its decision: it is neither a candidate nor owed to the Worker.
  assert.deepEqual(s.store().pendingDeliveries(), []);

  s.comment("also the one in CONTRIBUTING.md", 58);
  await s.poll();
  assert.deepEqual(s.store().undecidedTriggers(), []);
  const turns = s.t3.routerTurns.length;
  assert.equal((await routeCandidates(s.options())).router, null);
  assert.equal(s.t3.routerTurns.length, turns);

  // The follow-up pass queues it as a turn on the Worker's session.
  const workers = s.workers();
  const routed = routeFollowUps(s.store(), workers);
  assert.deepEqual(routed.failed, []);
  assert.deepEqual(routed.queued.map((d) => [d.ticketId, d.assignmentId]), [[ticketId, assignmentId]]);
  await workers.drain();
  assert.equal(s.runner.sessions.at(-1)?.prompts.length, 1);
  assert.deepEqual(routeFollowUps(s.store(), workers), { queued: [], failed: [] });
});

test("a trigger's body is fetched by the id in its URL", () => {
  const repo = "o/r";
  const url = (fragment: string) => `https://github.com/o/r/pull/3${fragment}`;
  assert.equal(bodyPath(repo, { kind: "issue_comment", url: url("#issuecomment-5") }, 3), "repos/o/r/issues/comments/5");
  assert.equal(bodyPath(repo, { kind: "pull_request_review_comment", url: url("#discussion_r6") }, 3), "repos/o/r/pulls/comments/6");
  assert.equal(bodyPath(repo, { kind: "pull_request_review", url: url("#pullrequestreview-7") }, 3), "repos/o/r/pulls/3/reviews/7");
  assert.throws(() => bodyPath(repo, { kind: "issue_comment", url: url("#discussion_r6") }, 3), /does not name/);
});

test("a comment deleted since intake reaches the Router as deleted; long bodies are cut", async () => {
  const s = setup();
  const gone = s.comment("short-lived", 57);
  s.comment("x".repeat(1000), 58);
  await s.poll();
  s.github.issueComments.splice(s.github.issueComments.indexOf(gone), 1);
  await routeCandidates(s.options());
  const [deleted, long] = batchOf(s.t3.routerTurns[0]!.stdin!).threads[0]!.candidates;
  assert.match(deleted!.excerpt, /deleted from GitHub/);
  assert.equal(long!.truncated, true);
  assert.equal(long!.excerpt.length, 601);
});

test("a flagged stall reaches the Router once; wait leaves the Worker, stop ends the work", async () => {
  const s = setup();
  s.comment("please fix the typo", 57);
  s.comment("and the CONTRIBUTING typo", 58, 8);
  await s.poll();
  const first = await routeCandidates(s.options());
  const [a, b] = first.dispatched;
  const turns = () => s.t3.routerTurns.length;
  assert.equal(turns(), 1);

  const waiting = s.store().recordStall({ assignmentId: a!.assignmentId, semonReason: "waiting-question", t3Reason: "waiting-request", detail: "Which file?" });
  s.t3.answer = (batch) => ({
    decisions: [],
    outcomes: [],
    stalls: batch.stalls.map((stall) => ({ key: stall.key, action: "wait" as const, reply: "The Worker asks: which file?" })),
  });
  const flagged = await routeCandidates(s.options());
  const sent = batchOf(s.t3.routerTurns.at(-1)!.stdin!) as unknown as { stalls: Record<string, unknown>[] };
  assert.deepEqual(sent.stalls, [{
    key: `stall-${waiting.id}`,
    thread: first.decisions[0]!.thread,
    threadUrl: "https://github.com/onsager-ai/ostrom/issues/7",
    request: "fix the README typo",
    signals: { semon: "waiting-question", t3: "waiting-request" },
    detail: "Which file?",
    since: NOW.toISOString(),
  }]);
  assert.deepEqual(flagged.stalls, [{
    key: `stall-${waiting.id}`, action: "wait", ticketId: a!.ticketId, replyId: `stall-${waiting.id}`, ticketEnded: false,
  }]);
  assert.equal(s.store().getAssignment(a!.assignmentId).state, "running");
  assert.equal(s.store().dueReplies().find((r) => r.id === `stall-${waiting.id}`)?.body, "The Worker asks: which file?");

  // The same episode is not flagged again, so the Router is not asked.
  s.store().recordStall({ assignmentId: a!.assignmentId, semonReason: "waiting-question", t3Reason: null, detail: null });
  assert.equal((await routeCandidates(s.options())).router, null);
  assert.equal(turns(), 2);

  // A Router answer that does not check out records nothing; the stall stays flagged.
  const gone = s.store().recordStall({ assignmentId: b!.assignmentId, semonReason: "process-gone", t3Reason: null, detail: null });
  s.t3.answer = (batch) => ({
    decisions: [],
    outcomes: [],
    stalls: batch.stalls.map((stall) => ({ key: stall.key, action: "stop" as const, reply: null })),
  });
  await assert.rejects(routeCandidates(s.options()), /a stop needs a reply/);
  assert.deepEqual(s.store().unansweredStalls().map((stall) => stall.id), [gone.id]);
  assert.equal(s.store().getAssignment(b!.assignmentId).state, "running");

  s.t3.answer = (batch) => ({
    decisions: [],
    outcomes: [],
    stalls: batch.stalls.map((stall) => ({ key: stall.key, action: "stop" as const, reply: "The Worker died; work stopped." })),
  });
  const stopped = await routeCandidates(s.options());
  assert.deepEqual(stopped.stalls.map((stall) => [stall.action, stall.ticketEnded]), [["stop", true]]);
  assert.equal(s.store().getAssignment(b!.assignmentId).state, "ended");
  assert.equal(s.store().getTicket(b!.ticketId).state, "ended");
  assert.equal(s.store().getTicket(a!.ticketId).state, "open");
});
