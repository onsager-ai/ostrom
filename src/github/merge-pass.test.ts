import assert from "node:assert/strict";
import { test } from "node:test";

import { Store } from "../store/store.ts";
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
function review(overrides: { login?: string; type?: string; head?: string; at?: string; state?: string; id?: string } = {}) {
  return {
    id: overrides.id ?? "R1", state: overrides.state ?? "APPROVED", submittedAt: overrides.at ?? "2026-11-10T12:00:00.000Z",
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
    merged: false, head: "head", issueState: "open", permission: "write", reviews: [review()], comments: [] as unknown[],
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
        number: 2, state: remote.merged ? "MERGED" : "OPEN", isDraft: false, mergeable: "MERGEABLE", mergeStateStatus: "CLEAN",
        reviewDecision: "APPROVED", author: { login: "author" }, headRefName: "b", headRefOid: remote.head, baseRefName: "main", baseRefOid: "base",
        reviewThreads: connection([]), reviews: connection(remote.reviews), commits: { nodes: [{ commit: {
          oid: remote.head, associatedPullRequests: connection([{ number: 2 }]), statusCheckRollup: { state: "SUCCESS", contexts: connection([check]) },
          checkSuites: connection([{ databaseId: 1, status: "COMPLETED", conclusion: "SUCCESS",
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
      head: { ref: "b", sha: remote.head, repo: { full_name: "o/r" } }, base: { ref: "main", sha: "base", repo: { full_name: "o/r" } },
      merge_commit_sha: remote.merged ? "merge" : null, merged_by: remote.merged ? remote.mergedBy : null,
      merged_at: remote.merged ? "2026-11-10T12:11:00.000Z" : null,
    });
    throw new Error(`unexpected gh call ${JSON.stringify(args)}`);
  };
  const released: number[] = [];
  const cancelled: number[] = [];
  const workers = { release(id: number) { released.push(id); store.releaseAssignmentReservation(id); },
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
  return { store, assignment, ticket, issue, pr, source, remote, puts, calls, options, route, send, attempts, replies, runner, released, cancelled,
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
  assert.match(body, /@marvin/);
  assert.match(body, /https:\/\/github.com\/o\/r\/commit\/merge/);
  if (origin === "ostrom") assert.match(body, /Ostrom .* merged/);
  else assert.doesNotMatch(body, /Ostrom .* merged/);
  if (origin === "unknown") assert.match(body, /whether Ostrom caused this merge is unknown/);
}

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
  assert.match(card, /Waiting for a human merge/);
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

for (const [name, change] of [
  ["read-only reviewer", (f: Fixture) => { f.remote.permission = "read"; }],
  ["bot reviewer", (f: Fixture) => { f.remote.reviews = [review({ type: "Bot" })]; }],
  ["PR author", (f: Fixture) => { f.remote.reviews = [review({ login: "author" })]; }],
  ["old-head approval", (f: Fixture) => { f.remote.reviews = [review({ head: "old" })]; }],
] as const) test(`${name} does not satisfy approval policy`, async () => {
  const f = fixture({ strategy: "approvals", approvals: 1, approvers: [] });
  change(f);
  await ready(f);
  await mergePass(f.options());
  assert.equal(f.puts.length, 0);
  assert.equal(f.store.mergeCandidateForAssignment(f.assignment)!.blockingReason, "policy approvals are not satisfied");
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
  if (reason === "local pause at final refresh") options.refreshBeforeSend = async () => { f.store.pauseTicket(f.ticket); };
  if (reason === "activity at final refresh") options.refreshBeforeSend = async () => { f.remote.comments.push({ id: 4, body: "wait", updated_at: f.now().toISOString() }); };
  await mergePass(options);
  assert.equal(f.puts.length, 0);
  unmerged(f);
});

test("gh HTTP parser pins observed gh 2.98.0 stderr wrapped by execGh", () => {
  assert.equal(ghHttpStatus("gh api failed: gh: Not Found (HTTP 404)"), 404);
  assert.equal(ghHttpStatus("gh api failed: gh: Pull Request is not mergeable (HTTP 405)"), 405);
  assert.equal(ghHttpStatus("gh api failed: context deadline exceeded"), null);
});
