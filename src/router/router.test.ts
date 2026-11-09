import assert from "node:assert/strict";
import { test } from "node:test";

import { FakeGitHub } from "../github/fake-github.ts";
import { pollOnce } from "../github/poller.ts";
import { Store } from "../store/store.ts";
import { FakeRunner } from "../worker/fake-runner.ts";
import { routeFollowUps } from "../worker/follow-up.ts";
import { Workers } from "../worker/workers.ts";
import {
  bodyPath,
  parseRouterAnswer,
  routeCandidates,
  RouterOutputError,
  type Batch,
  type RouteOptions,
  type RouterAnswer,
} from "./router.ts";

const workerConfig = { repo: "/repo", base: "main", worktrees: "/wt", harness: "codex" as const, model: "gpt-6-luna", machine: "host" };
const router = { harness: "codex" as const, model: "gpt-6-luna" };

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


const NOW = new Date("2026-11-09T12:00:00.000Z");

function setup(dbPath = ":memory:") {
  const now = () => NOW;
  const github = new FakeGitHub(now);
  github.issue(7);
  github.issue(8);
  const runner = new FakeRunner();
  let answer: (batch: Batch) => RouterAnswer | string = acceptAll;
  const start = runner.start.bind(runner);
  runner.start = (options) => {
    const session = start(options);
    if (options.sandbox === "read-only") {
      session.reply = (prompt) => {
        const next = answer(batchOf(prompt));
        return typeof next === "string" ? next : JSON.stringify(next);
      };
    }
    return session;
  };
  let store = Store.open(dbPath, { now });
  const addWorktree = async (_repo: string, _path: string, _base: string, ticket: number, assignment: number) =>
    `ostrom/ticket-${ticket}-${assignment}`;
  const workers = () => new Workers({ store, config: workerConfig, runners: { claude: runner, codex: runner }, addWorktree });
  const options = (): RouteOptions => ({
    store,
    gh: github.gh,
    repo: github.repo,
    workers: workers(),
    router,
    runners: { claude: runner, codex: runner },
  });
  const comment = (body: string, minute = 58, issue = 7) =>
    github.comment(issue, { login: "alice", body, at: new Date(`2026-11-09T11:${minute}:00Z`) });
  const poll = () => pollOnce({ store, gh: github.gh, repo: github.repo, allowlist: ["alice"], now });
  const reopen = () => {
    store.close();
    store = Store.open(dbPath, { now });
  };
  return {
    github,
    runner,
    routerSessions: () => runner.sessions.filter((session) => session.options.sandbox === "read-only"),
    get answer() { return answer; },
    set answer(value: (batch: Batch) => RouterAnswer | string) { answer = value; },
    workers,
    store: () => store,
    options,
    comment,
    poll,
    reopen,
  };
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

  const [session] = s.routerSessions();
  assert.deepEqual(pass.router, { sessionId: session?.id });
  assert.equal(session?.options.sandbox, "read-only");
  assert.equal(session?.options.model, "gpt-6-luna");
  const sent = batchOf(session!.prompts[0]!);
  assert.equal(sent.threads[0]?.candidates[0]?.excerpt, "please fix the typo in the README");

  assert.equal(pass.decisions.length, 1);
  const decision = pass.decisions[0]!;
  assert.equal(decision.verdict, "accept");
  assert.equal(decision.replyId, `decision-${decision.decisionId}`);
  assert.deepEqual(pass.dispatched.map((d) => d.ticketId), [decision.ticketId]);
  const workerSession = s.runner.sessions.find((candidate) => candidate.options.sandbox !== "read-only");
  assert.equal(s.store().runningAssignments()[0]?.sessionId, workerSession?.id);
  assert.deepEqual(
    s.store().dueReplies().map((reply) => [reply.id, reply.body]).filter(([id]) => id !== `card-thread-${decision.thread}`),
    [[decision.replyId, "On it: a Worker is starting."]],
  );
  assert.deepEqual(s.store().undecidedTriggers(), []);
  assert.deepEqual(pass.failed, []);

  // Nothing new and no outcome: the Router is not asked again.
  const again = await routeCandidates(s.options());
  assert.equal(again.router, null);
  assert.equal(s.routerSessions().length, 1);
});

test("an ignore is recorded with no ticket, no Worker and no reply", async () => {
  const s = setup();
  s.comment("thanks!");
  await s.poll();
  s.answer = (batch) => ({
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
  assert.equal(s.routerSessions().length, 1);
  assert.deepEqual(s.store().undecidedTriggers(), []);
});

test("invalid Router output is a loud failure and its candidates stay undecided", async () => {
  const s = setup();
  s.comment("please fix the typo");
  await s.poll();
  s.answer = (batch) => `Sure! ${JSON.stringify(acceptAll(batch))}`;
  await assert.rejects(routeCandidates(s.options()), RouterOutputError);
  assert.equal(s.store().undecidedTriggers().length, 1);
  assert.deepEqual(s.store().dueReplies(), []);
  assert.deepEqual(s.store().undispatchedTickets(), []);

  // The next valid answer decides the same candidate.
  s.answer = acceptAll;
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

test("a Router failure records nothing, and the next fresh session judges the same candidate", async () => {
  const s = setup();
  s.comment("please fix the typo");
  const polled = await s.poll();
  s.runner.startFailure = new Error("router unavailable");
  await assert.rejects(routeCandidates(s.options()), /router unavailable/);
  assert.equal(s.store().undecidedTriggers().length, 1);

  s.runner.startFailure = undefined;
  const pass = await routeCandidates(s.options());
  assert.deepEqual(pass.decisions.map((d) => d.triggers), [[polled.candidates[0]!.triggerId]]);
  assert.deepEqual(pass.router, { sessionId: "sess-2" });
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
  const turns = () => s.routerSessions().length;

  s.github.pulls.push({ branch, number: 12, state: "open" });
  const opened = await routeCandidates(s.options());
  assert.deepEqual(opened.outcomes, [
    { key: `outcome-a${assignmentId}-pr12-opened`, event: "pr_opened", ticketId, ticketEnded: false },
  ]);
  assert.equal(batchOf(s.routerSessions().at(-1)!.prompts[0]!).outcomes[0]?.pullRequest, pr(12, "open").url);
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
  let routerAnswered = false;
  const start = s.runner.start.bind(s.runner);
  s.runner.start = (options) => {
    if (options.sandbox === "read-only") {
      const session = start(options);
      const send = session.send.bind(session);
      session.send = async (prompt) => {
        const result = await send(prompt);
        routerAnswered = true;
        return result;
      };
      return session;
    }
    if (routerAnswered) {
      s.runner.startFailure = new Error("Project not found.");
      const worker = start(options);
      s.runner.startFailure = undefined;
      return worker;
    }
    return start(options);
  };
  const first = await routeCandidates(s.options());
  assert.match(first.failed[0]?.error ?? "", /Project not found/);
  const ticketId = first.decisions[0]!.ticketId!;

  const next = await routeCandidates(s.options());
  assert.equal(next.outcomes.length, 1);
  assert.equal(next.outcomes[0]?.event, "failed");
  assert.equal(next.outcomes[0]?.ticketEnded, true);
  assert.match(batchOf(s.routerSessions().at(-1)!.prompts[0]!).outcomes[0]?.detail ?? "", /Project not found/);
  assert.equal(s.store().getTicket(ticketId).state, "ended");
  assert.deepEqual(next.failed, []);
  assert.equal((await routeCandidates(s.options())).router, null);
});

test("each batch starts a fresh Router session", async () => {
  const s = setup();
  s.comment("please fix the typo", 57);
  await s.poll();
  await routeCandidates(s.options());

  s.comment("and please fix the CONTRIBUTING typo", 58, 8);
  await s.poll();
  const pass = await routeCandidates(s.options());
  assert.deepEqual(pass.router, { sessionId: "sess-3" });
  assert.equal(s.routerSessions().length, 2);
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
  const turns = s.routerSessions().length;
  assert.equal((await routeCandidates(s.options())).router, null);
  assert.equal(s.routerSessions().length, turns);

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
  const [deleted, long] = batchOf(s.routerSessions()[0]!.prompts[0]!).threads[0]!.candidates;
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
  const turns = () => s.routerSessions().length;
  assert.equal(turns(), 1);

  const waiting = s.store().recordStall({ assignmentId: a!.assignmentId, semonReason: "waiting-question", detail: "Which file?" });
  s.answer = (batch) => ({
    decisions: [],
    outcomes: [],
    stalls: batch.stalls.map((stall) => ({ key: stall.key, action: "wait" as const, reply: "The Worker asks: which file?" })),
  });
  const flagged = await routeCandidates(s.options());
  const sent = batchOf(s.routerSessions().at(-1)!.prompts[0]!) as unknown as { stalls: Record<string, unknown>[] };
  assert.deepEqual(sent.stalls, [{
    key: `stall-${waiting.id}`,
    thread: first.decisions[0]!.thread,
    threadUrl: "https://github.com/onsager-ai/ostrom/issues/7",
    request: "fix the README typo",
    signals: { semon: "waiting-question" },
    detail: "Which file?",
    since: NOW.toISOString(),
  }]);
  assert.deepEqual(flagged.stalls, [{
    key: `stall-${waiting.id}`, action: "wait", ticketId: a!.ticketId, replyId: `stall-${waiting.id}`, ticketEnded: false,
  }]);
  assert.equal(s.store().getAssignment(a!.assignmentId).state, "running");
  assert.equal(s.store().dueReplies().find((r) => r.id === `stall-${waiting.id}`)?.body, "The Worker asks: which file?");

  // The same episode is not flagged again, so the Router is not asked.
  s.store().recordStall({ assignmentId: a!.assignmentId, semonReason: "waiting-question", detail: null });
  assert.equal((await routeCandidates(s.options())).router, null);
  assert.equal(turns(), 2);

  // A Router answer that does not check out records nothing; the stall stays flagged.
  const gone = s.store().recordStall({ assignmentId: b!.assignmentId, semonReason: "process-gone", detail: null });
  s.answer = (batch) => ({
    decisions: [],
    outcomes: [],
    stalls: batch.stalls.map((stall) => ({ key: stall.key, action: "stop" as const, reply: null })),
  });
  await assert.rejects(routeCandidates(s.options()), /a stop needs a reply/);
  assert.deepEqual(s.store().unansweredStalls().map((stall) => stall.id), [gone.id]);
  assert.equal(s.store().getAssignment(b!.assignmentId).state, "running");

  s.answer = (batch) => ({
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
