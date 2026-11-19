import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

import { FakeGitHub } from "./fake-github.ts";
import { Store } from "../store/store.ts";
import { watchdog } from "../watchdog/watchdog.ts";
import { advanceStuckLadder } from "../watchdog/stuck.ts";
import { routeCandidates, type Batch } from "../router/router.ts";
import { FakeRunner } from "../worker/fake-runner.ts";
import type { Workers } from "../worker/workers.ts";
import { ghHttpStatus, mergePass, type MergePassOptions } from "./merge-pass.ts";
import type { MergePolicy } from "./merge-policy.ts";
import type { GhRunner } from "./gh.ts";
import { sendDueReplies } from "./outbox.ts";

const MINUTE = 60_000;
const pageInfo = { hasNextPage: false, endCursor: null };
const connection = (nodes: unknown[]) => ({ nodes, pageInfo });
function review(overrides: { login?: string; type?: string; head?: string; at?: string; state?: string; id?: string; body?: string; inlineComments?: number } = {}) {
  return {
    id: overrides.id ?? "R1", state: overrides.state ?? "APPROVED", submittedAt: overrides.at ?? "2026-11-10T12:00:00.000Z",
    body: overrides.body ?? "", comments: { totalCount: overrides.inlineComments ?? 0 },
    author: { __typename: overrides.type ?? "User", id: "U1", login: overrides.login ?? "reviewer" }, commit: { oid: overrides.head ?? "head" },
  };
}

function fixture(policy: MergePolicy = { strategy: "unattended" }) {
  let now = new Date("2026-11-10T12:00:00.000Z");
  const store = Store.open(":memory:", { now: () => now });
  const source = store.ensureSource("github", "o/r");
  const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 1, url: "https://github.com/o/r/issues/1" });
  const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: "I1", threadId: issue, kind: "issue_comment", actor: "alice",
    url: "https://github.com/o/r/issues/1#issuecomment-1", occurredAt: now }).id;
  const ticket = store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "work" }).ticketId!;
  const assignment = store.createAssignment(ticket);
  store.configureNativeAssignment(assignment, { harness: "claude", model: "m", worktree: "/w", branch: "b", machine: "h" });
  store.startNativeAssignment(assignment, "s");
  const pr = store.ensureThread({ sourceId: source.id, kind: "pull_request", number: 2, url: "https://github.com/o/r/pull/2" });
  store.subscribe(ticket, pr);
  const remote = {
    merged: false, head: "head", baseSha: "base", mergeable: "MERGEABLE", mergeStateStatus: "CLEAN", base: "main", issueState: "open", permission: "write", reviews: [review()], comments: [] as unknown[],
    putError: null as string | null, mergeBeforeError: false, mergedBy: { id: 42, login: "marvin" },
    refuse: false, failConfirmation: false, rules: [] as unknown[], protection: {} as Record<string, unknown>,
    openPulls: [2], onFinalRead: null as (() => void) | null,
  };
  const puts: string[][] = [];
  const calls: string[][] = [];
  let graphqlReads = 0;
  const gh: GhRunner = async (args) => {
    calls.push([...args]);
    const path = args.find((arg) => arg.startsWith("repos/")) ?? args.at(-1)!;
    if (args.includes("PUT")) {
      puts.push([...args]);
      if (remote.mergeBeforeError) remote.merged = true;
      if (remote.putError !== null) throw new Error(remote.putError);
      if (remote.refuse) return JSON.stringify({ merged: false, message: "not mergeable" });
      remote.merged = true;
      return JSON.stringify({ merged: true, sha: "merge" });
    }
    if (args[1] === "graphql") {
      if (remote.failConfirmation && puts.length > 0) throw new Error("confirmation unavailable");
      graphqlReads++;
      remote.onFinalRead?.();
      const check = { __typename: "CheckRun", databaseId: 10, name: "check", status: "COMPLETED", conclusion: "SUCCESS",
        checkSuite: { databaseId: 1, app: { databaseId: 15368 } } };
      return JSON.stringify({ data: { repository: { pullRequest: {
        number: 2, state: remote.merged ? "MERGED" : "OPEN", isDraft: false, mergeable: remote.mergeable, mergeStateStatus: remote.mergeStateStatus,
        reviewDecision: "APPROVED", author: { login: "author" }, headRefName: "b", headRefOid: remote.head, baseRefName: remote.base, baseRefOid: remote.baseSha,
        reviewThreads: connection([]), reviews: connection(remote.reviews), commits: { nodes: [{ commit: {
          oid: remote.head, associatedPullRequests: connection([{ number: 2 }]), statusCheckRollup: { state: "SUCCESS", contexts: connection([check]) },
          checkSuites: connection([{ databaseId: 1, status: "COMPLETED", conclusion: "SUCCESS", repository: { nameWithOwner: "o/r" }, matchingPullRequests: connection([{ number: 2 }]),
            workflowRun: { event: "pull_request", runAttempt: 1, workflow: { databaseId: 1, name: "ci" } } }]),
        } }] },
      } } } });
    }
    if (path === "user") return JSON.stringify({ id: 42, login: "marvin" });
    if (path.endsWith("/permission")) return JSON.stringify({ permission: remote.permission });
    if (path === "repos/o/r") return JSON.stringify({ allow_squash_merge: true });
    if (path.includes("/protection")) return JSON.stringify(remote.protection);
    if (path.includes("/rules/")) return JSON.stringify(remote.rules);
    if (path.includes("/comments")) {
      if (args.includes("POST")) return JSON.stringify({ id: 99 });
      if (args.includes("PATCH")) return "{}";
      return JSON.stringify(remote.comments);
    }
    if (path === "repos/o/r/issues/1") return JSON.stringify({ state: remote.issueState, body: "work", title: "work" });
    if (path.includes("/pulls?")) return JSON.stringify(remote.openPulls.map((number) => ({ number, html_url: `https://github.com/o/r/pull/${number}`,
      state: remote.merged ? "closed" : "open", merged_at: remote.merged ? now.toISOString() : null })));
    if (path === "repos/o/r/pulls/2") return JSON.stringify({
      head: { ref: "b", sha: remote.head, repo: { full_name: "o/r" } }, base: { ref: remote.base, sha: remote.baseSha, repo: { full_name: "o/r" } },
      merge_commit_sha: remote.merged ? "merge" : null, merged_by: remote.merged ? remote.mergedBy : null,
      merged_at: remote.merged ? "2026-11-10T12:11:00.000Z" : null,
    });
    throw new Error(`unexpected gh call ${JSON.stringify(args)}`);
  };
  const released: number[] = [];
  const cancelled: number[] = [];
  const deliveries: { key: string; prompt: string }[] = [];
  const workers = { deliver(id: number, key: string, prompt: string) {
      const added = store.enqueueTurn({ assignmentId: id, deliveryKey: key, prompt, kind: key.startsWith("ci-fix-") ? "ci_fix" : "conflict" });
      if (!added.duplicate) deliveries.push({ key, prompt });
      return !added.duplicate;
    }, release(id: number) { released.push(id); store.releaseAssignmentReservation(id); },
    cancel(id: number) { cancelled.push(id); return false; }, wake() {} } as unknown as Workers;
  const options = (): MergePassOptions => ({ store, workers, gh, repo: "o/r", policy, now: () => now });
  const runner = new FakeRunner();
  const start = runner.start.bind(runner);
  runner.start = (opts) => {
    const session = start(opts);
    session.reply = (prompt) => {
      const batch = JSON.parse(prompt.split("The batch:\n")[1]!) as Batch;
      return JSON.stringify({ decisions: [], stalls: [], outcomes: batch.outcomes.map((outcome) => ({ key: outcome.key, reply: `Router: ${outcome.event}` })) });
    };
    return session;
  };
  const route = (routerGh: GhRunner = gh) => routeCandidates({ store, workers, gh: routerGh, repo: "o/r", mergePolicy: policy,
    router: { harness: "claude", model: "m" }, runners: { claude: runner, codex: runner } });
  const send = () => sendDueReplies({ store, gh, repo: "o/r", now: () => now, batchWindowMs: 0, commentGapMs: 0 });
  const attempts = () => store.mergeAttempts("o/r", 2);
  const replies = () => store.pendingReplies().filter((reply) => !reply.id.startsWith("card-"));
  return { store, assignment, ticket, issue, pr, source, remote, puts, calls, options, route, send, attempts, replies, runner, released, cancelled, deliveries,
    now: () => now, advance(ms: number) { now = new Date(now.getTime() + ms); }, reads: () => graphqlReads };
}
type Fixture = ReturnType<typeof fixture>;
async function ready(f: Fixture) {
  assert.deepEqual((await mergePass(f.options())).failed, []);
  assert.equal(f.store.hasReply(`outcome-a${f.assignment}-pr2-opened`), false, "merge pass leaves the new PR reply to Router");
  await f.route();
  const sent = await f.send();
  assert.deepEqual(sent.failed, []);
  assert.equal(f.store.readyReplySentAt(f.assignment), f.now().toISOString());
  await mergePass(f.options());
  f.advance(11 * MINUTE);
}
function unmerged(f: Fixture) {
  assert.deepEqual(f.attempts(), []);
  assert.equal(f.store.prMerge("o/r", 2), null);
  assert.equal(f.store.reservationHeld(f.assignment), false);
  assert.equal(f.store.hasReply(`outcome-a${f.assignment}-pr2-merged`), false);
}
function done(f: Fixture, origin: "ostrom" | "external" | "unknown") {
  assert.deepEqual(f.store.prMerge("o/r", 2), { origin, mergedById: 42, mergedByLogin: "marvin", mergeSha: "merge", mergedAt: "2026-11-10T12:11:00.000Z" });
  assert.equal(f.store.getTicket(f.ticket).state, "ended");
  assert.equal(f.store.getAssignment(f.assignment).state, "ended");
  assert.equal(f.store.reservationHeld(f.assignment), false);
  const replies = f.replies();
  assert.deepEqual(replies.filter((r) => r.id.endsWith("-merged")).map((r) => r.id), [`outcome-a${f.assignment}-pr2-merged`]);
  const body = replies.find((r) => r.id.endsWith("-merged"))!.body;
  assert.match(f.store.statusCardBody(f.issue)!, /Ostrom: Done/);
  assert.doesNotMatch(f.store.statusCardBody(f.issue)!, /Merge:.*waiting|Merge:.*blocked/i);
  assert.match(body, /@marvin/);
  assert.match(body, /https:\/\/github.com\/o\/r\/commit\/merge/);
  if (origin === "ostrom") assert.match(body, /Ostrom .* merged/);
  else assert.doesNotMatch(body, /Ostrom .* merged/);
  if (origin === "unknown") assert.match(body, /whether Ostrom caused this merge is unknown/);
}

test("existing merge evidence reports conflicts on the card and clears them on the next clean read", async () => {
  for (const strategy of ["unattended", "human"] as const) {
    for (const conflict of ["DIRTY", "CONFLICTING"]) {
      const f = fixture({ strategy });
      try {
        f.remote.base = "release";
        if (conflict === "DIRTY") f.remote.mergeStateStatus = conflict;
        else f.remote.mergeable = conflict;
        const result = await mergePass({ ...f.options(), base: "release" });
        assert.deepEqual(result.failed, []);
        assert.equal(result.waiting[0]?.reason, "Conflicts with release; the Worker needs to merge release.");
        assert.match(f.store.statusCardBody(f.issue)!, /Conflicts with release; the Worker needs to merge release\./);
        assert.equal(f.reads(), 1, "the conflict uses the existing evidence read");
        assert.equal(f.puts.length, 0);
        f.remote.mergeStateStatus = "CLEAN";
        f.remote.mergeable = "MERGEABLE";
        await mergePass({ ...f.options(), base: "release" });
        assert.doesNotMatch(f.store.statusCardBody(f.issue)!, /Conflicts with/);
      } finally { f.store.close(); }
    }
  }
});

test("conflicts deliver once per head and base, retain the wait after completion, and retry after base moves", async () => {
  for (const conflict of ["DIRTY", "CONFLICTING"]) {
    const f = fixture({ strategy: "approvals", approvals: 1, approvers: [] });
    try {
      if (conflict === "DIRTY") f.remote.mergeStateStatus = conflict;
      else f.remote.mergeable = conflict;
      assert.deepEqual((await mergePass(f.options())).failed, []);
      assert.equal(f.deliveries.length, 1);
      const { key, prompt } = f.deliveries[0]!;
      assert.equal(key, `conflict-a${f.assignment}-head-base`);
      assert.match(prompt, /Fetch origin\/main, then merge the latest origin\/main into the existing PR branch b using git merge/);
      assert.match(prompt, /both sides' intent/);
      assert.match(prompt, /keep every entry.*CHANGELOG\.md/);
      assert.match(prompt, /push.*do not open a new PR/);
      await mergePass(f.options());
      assert.equal(f.deliveries.length, 1, "queued conflict is not delivered again");
      f.store.beginQueuedTurn(f.assignment, key, "s:1");
      f.store.finishQueuedTurn(f.assignment, key);
      assert.equal(f.store.assignmentWorkIdle(f.assignment), true);
      await mergePass(f.options());
      assert.equal(f.deliveries.length, 1, "finished conflict key remains deduplicated");
      assert.match(f.store.statusCardBody(f.issue)!, /Conflicts with main; the Worker needs to merge main\./);
      f.remote.baseSha = "base-next";
      await mergePass(f.options());
      assert.equal(f.deliveries.length, 2);
      assert.equal(f.deliveries[1]!.key, `conflict-a${f.assignment}-head-base-next`);
      f.store.beginQueuedTurn(f.assignment, f.deliveries[1]!.key, "s:2");
      f.store.finishQueuedTurn(f.assignment, f.deliveries[1]!.key);
      f.remote.head = "head-next";
      await mergePass(f.options());
      assert.equal(f.deliveries.length, 3);
      assert.equal(f.puts.length, 0);
    } finally { f.store.close(); }
  }
});

test("conflict recovery respects ticket state, durable idle work, budgets, stalls and prerequisite failures", async () => {
  const blockers: Record<string, (f: Fixture) => void> = {
    paused: (f) => f.store.pauseTicket(f.ticket, "operator"),
    stopping: (f) => f.store.requestStop(f.ticket),
    ended: (f) => f.store.endTicket(f.ticket),
    busy: (f) => { f.store.beginTurn(f.assignment); },
    queued: (f) => { f.store.enqueueTurn({ assignmentId: f.assignment, deliveryKey: "other", kind: "follow_up", prompt: "work" }); },
    budget: (f) => {
      f.store.setTicketBudget(f.ticket, { timeMinutes: 1, idleMinutes: 30, turns: 20, tokens: 1000, escalations: 0 });
      f.store.beginTurn(f.assignment);
      f.store.startWorkerTurn(f.assignment, "dispatch");
      f.advance(2 * MINUTE);
      f.store.finishWorkerTurn(f.assignment, "completed", null);
      f.store.endTurn(f.assignment, null);
    },
    stalled: (f) => { f.store.recordStall({ assignmentId: f.assignment, semonReason: "idle", detail: null }); },
    closedIssue: (f) => { f.remote.issueState = "closed"; },
    ambiguousOwnership: (f) => { f.remote.openPulls = [2, 3]; },
    clean: (f) => { f.remote.mergeStateStatus = "CLEAN"; },
  };
  for (const [name, block] of Object.entries(blockers)) {
    const f = fixture();
    try {
      f.remote.mergeStateStatus = "DIRTY";
      block(f);
      assert.deepEqual((await mergePass(f.options())).failed, [], name);
      assert.equal(f.deliveries.length, 0, name);
    } finally { f.store.close(); }
  }
  const f = fixture();
  try {
    f.remote.mergeStateStatus = "DIRTY";
    await mergePass({ ...f.options(), allowSend: false });
    assert.equal(f.deliveries.length, 0);
  } finally { f.store.close(); }
});

async function ciFixture() {
  const f = fixture({ strategy: "human" });
  const github = new FakeGitHub(f.now, "o/r");
  github.issue(1);
  github.issue(2, true);
  github.pulls.push({ branch: "b", number: 2, state: "open" });
  const pull = JSON.parse(await f.options().gh(["api", "--method", "GET", "repos/o/r/pulls/2"]));
  const evidence = JSON.parse(await f.options().gh(["api", "graphql"])).data.repository.pullRequest;
  github.mergePulls.set(2, { pull, evidence });
  const commit = evidence.commits.nodes[0].commit;
  const check = commit.statusCheckRollup.contexts.nodes[0];
  Object.assign(check, { conclusion: "FAILURE", detailsUrl: "https://github.com/o/r/actions/runs/3/job/4" });
  const options = () => ({ ...f.options(), gh: github.gh });
  return { ...f, github, evidence, commit, check, options };
}

test("failed CI wakes once per head, including after completion; passing CI never wakes", async () => {
  const f = await ciFixture();
  try {
    assert.deepEqual((await mergePass(f.options())).failed, []);
    assert.equal(f.deliveries.length, 1);
    const { key, prompt } = f.deliveries[0]!;
    assert.equal(key, `ci-fix-a${f.assignment}-head`);
    assert.match(prompt, /check: https:\/\/github.com\/o\/r\/actions\/runs\/3\/job\/4/);
    assert.match(prompt, /Fetch origin\/main and compare these failures with the current origin\/main head/);
    assert.equal(f.store.queuedTurn(f.assignment, key)?.kind, "ci_fix");
    await mergePass(f.options());
    assert.equal(f.deliveries.length, 1);
    f.store.beginQueuedTurn(f.assignment, key, "s:1");
    f.store.finishQueuedTurn(f.assignment, key);
    await mergePass(f.options());
    assert.equal(f.deliveries.length, 1);
    f.evidence.headRefOid = f.commit.oid = "next-head";
    await mergePass(f.options());
    assert.equal(f.deliveries.length, 2);
    assert.equal(f.deliveries[1]!.key, `ci-fix-a${f.assignment}-next-head`);
    f.store.beginQueuedTurn(f.assignment, f.deliveries[1]!.key, "s:2");
    f.store.finishQueuedTurn(f.assignment, f.deliveries[1]!.key);
    f.evidence.headRefOid = f.commit.oid = "passing-head";
    f.check.conclusion = "SUCCESS";
    await mergePass(f.options());
    assert.equal(f.deliveries.length, 2);
  } finally { f.store.close(); }
});

test("CI wakes respect controls, idle work, ownership, and current failures", async () => {
  const blockers: Record<string, (f: Awaited<ReturnType<typeof ciFixture>>) => void> = {
    paused: (f) => f.store.pauseTicket(f.ticket, "operator"),
    ended: (f) => f.store.endTicket(f.ticket),
    stopping: (f) => f.store.requestStop(f.ticket),
    running: (f) => { f.store.beginTurn(f.assignment); },
    queued: (f) => { f.store.enqueueTurn({ assignmentId: f.assignment, deliveryKey: "other", kind: "review", prompt: "work" }); },
    stalled: (f) => { f.store.recordStall({ assignmentId: f.assignment, semonReason: "idle", detail: null }); },
    closed: (f) => f.github.close(1, f.now()),
    ownership: (f) => { f.evidence.headRefName = "other"; },
    pending: (f) => { f.check.status = "IN_PROGRESS"; },
    staleHead: (f) => { f.commit.oid = "old-head"; },
  };
  for (const [name, block] of Object.entries(blockers)) {
    const f = await ciFixture();
    try {
      block(f);
      assert.deepEqual((await mergePass(f.options())).failed, [], name);
      assert.equal(f.deliveries.length, 0, name);
    } finally { f.store.close(); }
  }
  const f = await ciFixture();
  try {
    await mergePass({ ...f.options(), allowSend: false });
    assert.equal(f.deliveries.length, 0);
  } finally { f.store.close(); }
});

test("failed legacy statuses carry their log link", async () => {
  const f = await ciFixture();
  try {
    f.commit.statusCheckRollup.contexts.nodes = [{ __typename: "StatusContext", context: "external", state: "ERROR", targetUrl: "https://ci.example/run/7" }];
    assert.deepEqual((await mergePass(f.options())).failed, []);
    assert.match(f.deliveries[0]!.prompt, /external: https:\/\/ci.example\/run\/7/);
  } finally { f.store.close(); }
});

test("review wait reaches time budget, extension resumes, and merge needs no human comment", async () => {
  const f = fixture();
  f.store.setTicketBudget(f.ticket, { timeMinutes: 60, idleMinutes: 30, turns: 20, tokens: 1_000_000, escalations: 0 });
  await ready(f);
  f.store.beginTurn(f.assignment);
  f.store.startWorkerTurn(f.assignment, "follow_up");
  f.advance(60 * MINUTE);
  f.store.finishWorkerTurn(f.assignment, "completed", null);
  f.store.endTurn(f.assignment, null);
  await watchdog({ store: f.store, idleMinutes: 20, now: f.now,
    semon: async () => ({ code: 0, stdout: JSON.stringify({ idle_minutes: 20, stalls: [] }), stderr: "" }) });
  assert.equal(f.store.getTicket(f.ticket).pauseCause, "budget reached: time");
  assert.deepEqual(await advanceStuckLadder(f.store, f.options().workers), { recovered: [], published: [] });
  assert.deepEqual(f.store.unansweredStalls(), []);
  assert.match(f.store.statusCardBody(f.issue)!, /Ostrom: Paused/);
  await mergePass(f.options());
  assert.equal(f.puts.length, 0);
  f.store.extendTicketBudget(f.ticket, { timeMinutes: 60 });
  assert.equal(f.store.getTicket(f.ticket).state, "open");
  // The follow-up turn reset the quiet window; waiting it out spends no time budget.
  f.advance(11 * MINUTE);
  const result = await mergePass(f.options());
  assert.deepEqual(result.failed, []);
  assert.equal(f.puts.length, 1);
  done(f, "ostrom");
});

test("one SHA-guarded squash PUT confirms an Ostrom merge and ends the ticket once", async () => {
  const f = fixture();
  await ready(f);
  const result = await mergePass(f.options());
  assert.deepEqual(result.failed, []);
  await mergePass(f.options());
  assert.equal(f.puts.length, 1);
  assert.ok(f.puts[0]!.includes("sha=head"));
  assert.ok(f.puts[0]!.includes("merge_method=squash"));
  assert.ok(f.puts[0]!.includes("X-GitHub-Api-Version: 2022-11-28"));
  const [attempt] = f.attempts();
  assert.equal(attempt!.headSha, "head");
  assert.equal(attempt!.actorId, 42);
  assert.equal(attempt!.responseSha, "merge");
  assert.deepEqual([attempt!.state, attempt!.error, attempt!.nextRetryAt], ["confirmed", null, null]);
  done(f, "ostrom");
  assert.match(f.store.statusCardBody(f.issue)!, /Ostrom: Done/);
});

test("human policy leaves a waiting_for_human_merge card and never sends PUT", async () => {
  const f = fixture({ strategy: "human" });
  await ready(f);
  await mergePass(f.options());
  assert.equal(f.puts.length, 0);
  unmerged(f);
  const candidate = f.store.mergeCandidateForAssignment(f.assignment)!;
  assert.equal(candidate.blockingReason, "waiting for a human merge");
  const card = f.store.statusCardBody(f.issue)!;
  assert.match(card, /Ostrom: Waiting for a human merge/);
  assert.match(card, /Merge: policy human; head head; waiting for a human merge/);
  assert.match(card, /policy human/);
  assert.match(card, /pause/);
  assert.match(card, /stop/);
});

test("current-head approval by a write-permission person satisfies approval policy", async () => {
  const f = fixture({ strategy: "approvals", approvals: 1, approvers: [] });
  await ready(f);
  await mergePass(f.options());
  assert.equal(f.puts.length, 1);
  assert.deepEqual(f.attempts().map((a) => [a.state, a.error, a.nextRetryAt]), [["confirmed", null, null]]);
  done(f, "ostrom");
});

for (const [name, change, reason] of [
  ["read-only reviewer", (f: Fixture) => { f.remote.permission = "read"; }, undefined],
  ["bot reviewer", (f: Fixture) => { f.remote.reviews = [review({ type: "Bot" })]; }, undefined],
  ["PR author", (f: Fixture) => { f.remote.reviews = [review({ login: "author" })]; }, undefined],
  ["old-head approval", (f: Fixture) => { f.remote.reviews = [review({ head: "old" })]; },
    "Approved on old; the head is now head. Approve again to merge."],
] as const) test(`${name} does not satisfy approval policy`, async () => {
  const f = fixture({ strategy: "approvals", approvals: 1, approvers: [] });
  change(f);
  await ready(f);
  await mergePass(f.options());
  assert.equal(f.puts.length, 0);
  assert.equal(f.store.mergeCandidateForAssignment(f.assignment)!.blockingReason, reason ?? "policy approvals are not satisfied");
  unmerged(f);
});

for (const body of ["", "LGTM!", " looks good. "]) test(`approval without feedback counts without restarting quiet: ${body}`, async () => {
  const f = fixture({ strategy: "approvals", approvals: 1, approvers: [] });
  f.remote.reviews = [];
  await ready(f);
  const deadline = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline;
  f.remote.reviews.push(review({ body, at: f.now().toISOString() }));
  const result = await mergePass(f.options());
  assert.deepEqual(result.failed, []);
  assert.equal(f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline, deadline);
  assert.equal(f.puts.length, 1);
  done(f, "ostrom");
});

test("Router-excluded approval commentary does not restart quiet and still satisfies approval policy", async () => {
  const f = fixture({ strategy: "approvals", approvals: 1, approvers: [] });
  f.remote.reviews = [];
  await ready(f);
  const deadline = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline;
  f.remote.reviews.push(review({ id: "R2", body: "The requested check is clean. CI is green.", at: f.now().toISOString() }));
  const source = f.store.ensureSource("github", "o/r");
  f.store.recordIntakeReceipt({ sourceId: source.id, sourceEventId: "R2", url: "https://github.com/o/r/pull/2#pullrequestreview-2", reason: "approval without feedback" });
  const result = await mergePass(f.options());
  assert.deepEqual(result.failed, []);
  assert.equal(f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline, deadline);
  assert.equal(f.puts.length, 1);
  done(f, "ostrom");
});

for (const feedback of [{ inlineComments: 1 }, { state: "CHANGES_REQUESTED" }]) test(`a receipt cannot hide new review feedback: ${JSON.stringify(feedback)}`, async () => {
  const f = fixture();
  await ready(f);
  const previous = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline!;
  const source = f.store.ensureSource("github", "o/r");
  f.store.recordIntakeReceipt({ sourceId: source.id, sourceEventId: "R2", url: "https://github.com/o/r/pull/2#pullrequestreview-2", reason: "approval without feedback" });
  f.remote.reviews.push(review({ id: "R2", body: "Checked everything", at: f.now().toISOString(), ...feedback }));
  const result = await mergePass(f.options());
  assert.deepEqual(result.failed, []);
  assert.ok(f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline! > previous);
  assert.equal(f.puts.length, 0);
});

for (const feedback of [{ body: "Please rename this" }, { inlineComments: 1 }]) test(`approval with feedback restarts quiet: ${JSON.stringify(feedback)}`, async () => {
  const f = fixture();
  await ready(f);
  const previous = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline!;
  f.remote.reviews.push(review({ id: "R2", at: f.now().toISOString(), ...feedback }));
  if (feedback.body !== undefined) f.store.insertTrigger({ sourceId: f.source.id, sourceEventId: "R2", threadId: f.pr,
    kind: "pull_request_review", actor: "reviewer", url: "https://github.com/o/r/pull/2#pullrequestreview-2", occurredAt: f.now() });
  const result = await mergePass(f.options());
  assert.deepEqual(result.failed, []);
  assert.ok(f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline! > previous);
  assert.equal(f.puts.length, 0);
});

test("unjudged approval commentary holds the merge without restarting quiet when judged no-feedback", async () => {
  const f = fixture();
  await ready(f);
  f.remote.reviews.push(review({ id: "R2", body: "Checked both headers", at: f.now().toISOString() }));
  const before = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline;
  await mergePass(f.options());
  const waiting = f.store.mergeCandidateForAssignment(f.assignment)!;
  assert.equal(waiting.blockingReason, "approval commentary not yet judged");
  assert.equal(waiting.quietDeadline, before);
  assert.equal(f.puts.length, 0);
  f.advance(MINUTE);
  f.store.recordIntakeReceipt({ sourceId: f.source.id, sourceEventId: "R2", url: "https://github.com/o/r/pull/2#pullrequestreview-2", reason: "approval without feedback" });
  await mergePass(f.options());
  const after = f.store.mergeCandidateForAssignment(f.assignment)!;
  assert.equal(after.quietDeadline, before);
  assert.equal(f.puts.length, 1);
  done(f, "ostrom");
});

test("a human comment after the last head restarts the quiet window", async () => {
  const f = fixture();
  await ready(f);
  const previous = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline!;
  f.remote.comments.push({ id: 100, body: "Please take another look", updated_at: f.now().toISOString() });
  await mergePass(f.options());
  const deadline = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline!;
  assert.ok(deadline > previous);
  assert.equal(deadline, new Date(f.now().getTime() + 10 * MINUTE).toISOString());
  assert.equal(f.puts.length, 0);
  unmerged(f);
});

for (const kind of ["comment trigger", "submitted review", "direct comment edit"] as const) test(`${kind} during the quiet window pushes its deadline out`, async () => {
  const f = fixture();
  await ready(f);
  const previous = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline!;
  if (kind === "comment trigger") {
    const trigger = f.store.insertTrigger({ sourceId: f.source.id, sourceEventId: "C2", threadId: f.issue, kind: "issue_comment", actor: "alice",
      url: "https://github.com/o/r/issues/1#issuecomment-2", occurredAt: f.now() }).id;
    f.store.recordDelivery(trigger, f.assignment, "s:1");
  } else if (kind === "submitted review") f.remote.reviews.push(review({ id: "R2", state: "COMMENTED", at: f.now().toISOString() }));
  else f.remote.comments.push({ id: 4, body: "changed", updated_at: f.now().toISOString() });
  await mergePass(f.options());
  const deadline = f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline!;
  assert.ok(deadline > previous);
  assert.equal(deadline, new Date(f.now().getTime() + 10 * MINUTE).toISOString());
  assert.equal(f.puts.length, 0);
  unmerged(f);
  f.advance(10 * MINUTE);
  await mergePass(f.options());
  done(f, "ostrom");
});

test("a changed head restarts the window without any PUT", async () => {
  const f = fixture();
  await ready(f);
  f.remote.head = "new-head";
  await mergePass(f.options());
  const candidate = f.store.mergeCandidateForAssignment(f.assignment)!;
  assert.equal(candidate.headSha, "new-head");
  assert.equal(candidate.quietDeadline, new Date(f.now().getTime() + 10 * MINUTE).toISOString());
  assert.equal(f.puts.length, 0);
  unmerged(f);
});

for (const status of [400, 401, 403, 404, 405, 409, 422]) test(`HTTP ${status} refusal fails the actual PUT and backs off before retry`, async () => {
  const f = fixture();
  await ready(f);
  f.remote.putError = `gh api failed: gh: refused (HTTP ${status})`;
  assert.equal((await mergePass(f.options())).failed.length, 1);
  assert.equal(f.puts.length, 1, "the request actually reached PUT");
  const first = f.attempts()[0]!;
  assert.deepEqual([first.state, first.error, first.nextRetryAt], ["failed", f.remote.putError, new Date(f.now().getTime() + 30_000).toISOString()]);
  assert.equal(f.store.prMerge("o/r", 2), null);
  assert.equal(f.store.reservationHeld(f.assignment), false);
  assert.equal(f.store.hasReply(`outcome-a${f.assignment}-pr2-merged`), false);
  f.advance(29_999);
  await mergePass(f.options());
  assert.equal(f.puts.length, 1);
  f.remote.putError = null;
  f.advance(1);
  await mergePass(f.options());
  assert.equal(f.puts.length, 2);
  assert.deepEqual(f.attempts().map((a) => a.state), ["failed", "confirmed"]);
  done(f, "ostrom");
});

test("dedicated App bot attributes a lost response and avoids the user endpoint", async () => {
  const f = fixture();
  await ready(f);
  f.calls.length = 0;
  f.remote.putError = "gh api failed: socket hang up";
  f.remote.mergeBeforeError = true;
  await mergePass({ ...f.options(), appBotId: 42 });
  assert.equal(f.calls.some((args) => args.at(-1) === "user"), false);
  await mergePass({ ...f.options(), appBotId: 42 });
  done(f, "ostrom");
});

test("a different merge actor stays unknown with App authentication", async () => {
  const f = fixture();
  await ready(f);
  f.remote.putError = "gh api failed: timeout";
  f.remote.mergeBeforeError = true;
  await mergePass({ ...f.options(), appBotId: 99 });
  await mergePass({ ...f.options(), appBotId: 99 });
  done(f, "unknown");
});

for (const error of ["gh api failed: socket hang up", "gh api failed: timeout", "gh api failed: gh: server failure (HTTP 503)", "gh api failed: gh: timeout (HTTP 408)", "gh api failed: gh: throttled (HTTP 429)"]) {
  test(`${error}: lost result reconciles MERGED as unknown origin`, async () => {
    const f = fixture();
    await ready(f);
    f.remote.putError = error;
    f.remote.mergeBeforeError = true;
    await mergePass(f.options());
    assert.equal(f.puts.length, 1);
    assert.deepEqual(f.attempts().map((a) => [a.state, a.error, a.nextRetryAt]), [["unknown", error, null]]);
    assert.equal(f.store.reservationHeld(f.assignment), true);
    assert.equal(f.store.prMerge("o/r", 2), null);
    assert.equal(f.store.hasReply(`outcome-a${f.assignment}-pr2-merged`), false);
    await mergePass(f.options());
    await mergePass(f.options());
    assert.equal(f.puts.length, 1);
    assert.deepEqual(f.attempts().map((a) => [a.state, a.error, a.nextRetryAt]), [["confirmed", null, null]]);
    done(f, "unknown");
  });
  test(`${error}: OPEN reconciliation changes unknown to failed with backoff`, async () => {
    const f = fixture();
    await ready(f);
    f.remote.putError = error;
    await mergePass(f.options());
    assert.deepEqual(f.attempts().map((a) => [a.state, a.error, a.nextRetryAt]), [["unknown", error, null]]);
    await mergePass(f.options());
    assert.deepEqual(f.attempts().map((a) => [a.state, a.error, a.nextRetryAt]), [["failed", "merge was not confirmed", new Date(f.now().getTime() + 30_000).toISOString()]]);
    assert.equal(f.puts.length, 1);
    assert.equal(f.store.reservationHeld(f.assignment), false);
    assert.equal(f.store.prMerge("o/r", 2), null);
    assert.equal(f.store.hasReply(`outcome-a${f.assignment}-pr2-merged`), false);
  });
}

test("a merged PR with no attempt records external actor and queues its outcome once", async () => {
  const f = fixture();
  f.remote.merged = true;
  await mergePass(f.options());
  await mergePass(f.options());
  assert.equal(f.puts.length, 0);
  assert.deepEqual(f.attempts(), []);
  done(f, "external");
});

test("external merge cancels a never-started recovery and completes exactly once", async () => {
  const f = fixture();
  f.store.enqueueTurn({ assignmentId: f.assignment, deliveryKey: "recovery-1", kind: "recovery", prompt: "recover" });
  f.remote.merged = true;
  assert.deepEqual((await mergePass(f.options())).failed, []);
  assert.deepEqual((await mergePass(f.options())).failed, []);
  const turn = f.store.queuedTurn(f.assignment, "recovery-1")!;
  assert.equal(turn.state, "cancelled");
  assert.equal(turn.startedAt, null);
  assert.equal(turn.turnId, null);
  assert.notEqual(turn.finishedAt, null);
  assert.deepEqual(f.store.unfinishedTurns(f.assignment), []);
  assert.equal(f.puts.length, 0);
  done(f, "external");
});

test("crash between reservation and attempt creation releases its orphan reservation", async () => {
  const f = fixture();
  assert.equal(f.store.reserveAssignment(f.assignment), true);
  await mergePass(f.options());
  assert.ok(f.released.includes(f.assignment));
  assert.equal(f.puts.length, 0);
  unmerged(f);
});

test("Router pendingOutcomes skips the reconciliation reply and asks no model for a duplicate", async () => {
  const f = fixture();
  f.remote.merged = true;
  // pendingOutcomes captures runningAssignments before awaiting GitHub. Reconcile while that read
  // is in flight, so the loop really reaches its existing-outcome skip for this same assignment.
  const result = await f.route(async (args) => {
    if (args.some((arg) => arg.includes("/pulls?"))) await mergePass(f.options());
    return f.options().gh(args);
  });
  assert.equal(result.router, null);
  assert.deepEqual(result.outcomes, []);
  assert.equal(f.runner.sessions.length, 0);
  assert.deepEqual(f.replies().map((r) => r.id), [`outcome-a${f.assignment}-pr2-merged`]);
  done(f, "external");
});

test("first enable posts a distinct notice after an opened reply was sent before tracking existed", async () => {
  const f = fixture();
  const opened = `outcome-a${f.assignment}-pr2-opened`;
  f.store.enqueueReply({ id: opened, threadId: f.issue, body: "old opened reply" });
  f.store.markReplySent(opened);
  f.advance(60 * MINUTE);
  await mergePass(f.options());
  const notice = `ready-a${f.assignment}-pr2`;
  assert.equal(f.store.readyReplyId(f.assignment), notice);
  assert.equal(f.store.readyReplySentAt(f.assignment), null);
  assert.equal(f.store.mergeCandidateForAssignment(f.assignment)!.quietDeadline, null);
  assert.deepEqual(f.replies().map((r) => r.id), [notice]);
  await f.send();
  assert.equal(f.store.readyReplySentAt(f.assignment), f.now().toISOString());
  await mergePass(f.options());
  const candidate = f.store.mergeCandidateForAssignment(f.assignment)!;
  assert.equal(candidate.readyReplyId, notice);
  assert.equal(candidate.quietDeadline, new Date(f.now().getTime() + 10 * MINUTE).toISOString());
  unmerged(f);
  f.advance(10 * MINUTE);
  await mergePass(f.options());
  assert.equal(f.puts.length, 1);
  done(f, "ostrom");
});

test("merge-enabled migration refreshes previously tracked, delivered opened replies", async () => {
  const dir = mkdtempSync(join(tmpdir(), "ostrom-merge-upgrade-"));
  const path = join(dir, "store.db");
  // Populate the pre-enable tracking state through public methods, then reopen at migration 26
  // so the real migration, rather than a hand-written equivalent, upgrades those durable rows.
  let now = new Date("2026-11-10T12:00:00.000Z");
  let store = Store.open(path, { now: () => now });
  const source = store.ensureSource("github", "o/r");
  const thread = store.ensureThread({ sourceId: source.id, kind: "issue", number: 1, url: "https://github.com/o/r/issues/1" });
  const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: "old", threadId: thread, kind: "issue_comment", actor: "alice", url: "https://github.com/o/r/issues/1#issuecomment-1", occurredAt: new Date() }).id;
  const ticket = store.recordDecision({ threadId: thread, triggerIds: [trigger], verdict: "accept", reason: "work" }).ticketId!;
  const assignment = store.createAssignment(ticket);
  store.configureNativeAssignment(assignment, { harness: "claude", model: "m", worktree: "/w", branch: "b", machine: "h" });
  store.startNativeAssignment(assignment, "s");
  const opened = `outcome-a${assignment}-pr2-opened`;
  store.enqueueReply({ id: opened, threadId: thread, body: "previous ready reply" });
  store.trackReadyReply(assignment, opened);
  store.markReplySent(opened);
  assert.notEqual(store.readyReplySentAt(assignment), null);
  store.close();
  const upgrade = new DatabaseSync(path);
  upgrade.exec("DROP TABLE selection_decision; DROP TABLE model_objective; DROP TABLE model_selection; DROP TABLE model_discovery; DROP TABLE turn; DROP INDEX intake_receipt_recent; DROP TABLE intake_receipt;");
  upgrade.prepare("DELETE FROM schema_version WHERE version >= 27").run();
  upgrade.close();
  store = Store.open(path, { now: () => now });
  try {
    assert.equal(store.readyReplyId(assignment), null);
    assert.equal(store.hasReply(opened), true);
    const pr = store.ensureThread({ sourceId: source.id, kind: "pull_request", number: 2, url: "https://github.com/o/r/pull/2" });
    store.subscribe(ticket, pr);
    const f = fixture();
    const options: MergePassOptions = { ...f.options(), store, now: () => now,
      workers: { release(id: number) { store.releaseAssignmentReservation(id); }, cancel() { return false; } } as unknown as Workers };
    await mergePass(options);
    const notice = `ready-a${assignment}-pr2`;
    assert.equal(store.readyReplyId(assignment), notice);
    assert.equal(store.readyReplySentAt(assignment), null);
    store.markReplySent(notice);
    await mergePass(options);
    now = new Date(now.getTime() + 10 * MINUTE);
    assert.deepEqual((await mergePass(options)).failed, []);
    assert.equal(f.puts.length, 1);
    assert.equal(store.prMerge("o/r", 2)!.origin, "ostrom");
    f.store.close();
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test("first enable repairs tracking attached after an already-sent opened reply", async () => {
  const f = fixture();
  const opened = `outcome-a${f.assignment}-pr2-opened`;
  f.store.enqueueReply({ id: opened, threadId: f.issue, body: "old reply" });
  f.store.markReplySent(opened);
  f.store.trackReadyReply(f.assignment, opened);
  assert.equal(f.store.readyReplySentAt(f.assignment), null);
  await mergePass(f.options());
  assert.equal(f.store.readyReplyId(f.assignment), `ready-a${f.assignment}-pr2`);
  await f.send();
  await mergePass(f.options());
  f.advance(10 * MINUTE);
  await mergePass(f.options());
  done(f, "ostrom");
});

test("Router owns a brand-new opened ready reply and appends the policy sentence", async () => {
  const f = fixture();
  await mergePass(f.options());
  assert.deepEqual(f.replies(), []);
  await f.route();
  const [reply] = f.replies();
  assert.equal(reply!.id, `outcome-a${f.assignment}-pr2-opened`);
  assert.match(reply!.body, /^Router: pr_opened/);
  assert.match(reply!.body, /Ostrom will squash-merge automatically/);
  await mergePass(f.options());
  assert.equal(f.store.hasReply(`ready-a${f.assignment}-pr2`), false);
  assert.equal(f.store.readyReplyId(f.assignment), reply!.id);
  unmerged(f);
});

test("unchanged waiting card queues no edit after delivery or after repeated passes", async () => {
  const f = fixture({ strategy: "human" });
  await ready(f);
  await mergePass(f.options());
  await f.send();
  assert.deepEqual(f.store.pendingReplies(), []);
  f.advance(MINUTE);
  await mergePass(f.options());
  await mergePass(f.options());
  assert.deepEqual(f.store.pendingReplies(), []);
  assert.equal(f.puts.length, 0);
  unmerged(f);
});

test("a positive merge response followed by failed confirmation retains Ostrom attribution on reconciliation", async () => {
  const f = fixture();
  await ready(f);
  f.remote.failConfirmation = true;
  await mergePass(f.options());
  assert.deepEqual(f.attempts().map((a) => [a.state, a.responseSha, a.nextRetryAt]), [["unknown", "merge", null]]);
  f.remote.failConfirmation = false;
  await mergePass(f.options());
  assert.equal(f.puts.length, 1);
  done(f, "ostrom");
});

test("HTTP 200 merged=false fails the pass with backoff", async () => {
  const f = fixture();
  await ready(f);
  f.remote.refuse = true;
  assert.equal((await mergePass(f.options())).failed.length, 1);
  assert.deepEqual(f.attempts().map((a) => [a.state, a.error, a.nextRetryAt]), [["failed", "not mergeable", new Date(f.now().getTime() + 30_000).toISOString()]]);
  assert.equal(f.store.reservationHeld(f.assignment), false);
});

for (const reason of ["closed issue", "failed prerequisite", "ambiguous branch ownership", "local pause at final refresh", "activity at final refresh"] as const) test(`${reason} suppresses PUT`, async () => {
  const f = fixture();
  await ready(f);
  const options = f.options();
  if (reason === "closed issue") f.remote.issueState = "closed";
  if (reason === "failed prerequisite") options.allowSend = false;
  if (reason === "ambiguous branch ownership") f.remote.openPulls = [2, 3];
  if (reason === "local pause at final refresh") options.refreshBeforeSend = async () => { f.store.pauseTicket(f.ticket, "test pause"); };
  if (reason === "activity at final refresh") options.refreshBeforeSend = async () => { f.remote.comments.push({ id: 4, body: "wait", updated_at: f.now().toISOString() }); };
  await mergePass(options);
  assert.equal(f.puts.length, 0);
  unmerged(f);
});

test("pause accepted while final reviewer permission read is pending prevents PUT", async () => {
  const f = fixture();
  await ready(f);
  let permissionReads = 0;
  const options = f.options();
  const gh = options.gh;
  options.gh = async (args) => {
    if (args.some((arg) => arg.endsWith("/permission")) && ++permissionReads === 2) f.store.pauseTicket(f.ticket, "test pause");
    return gh(args);
  };
  await mergePass(options);
  assert.equal(permissionReads, 2);
  assert.equal(f.puts.length, 0);
  unmerged(f);
});

test("gh HTTP parser pins observed gh 2.98.0 stderr wrapped by execGh", () => {
  assert.equal(ghHttpStatus("gh api failed: gh: Not Found (HTTP 404)"), 404);
  assert.equal(ghHttpStatus("gh api failed: gh: Pull Request is not mergeable (HTTP 405)"), 405);
  assert.equal(ghHttpStatus("gh api failed: context deadline exceeded"), null);
});
