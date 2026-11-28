import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, statSync, symlinkSync } from "node:fs";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { TestContext } from "node:test";

import { PlatformApiClient, PlatformApiError, startPlatformApiServer } from "./api.ts";
import type { readLaneDetail } from "./lane-detail.ts";
import { PlatformStore } from "./store.ts";
import type { T3PrInput, T3ThreadSnapshot } from "./store.ts";
import { spendWindow } from "./types.ts";
import type { Decision, DecisionVerification, Goal, GoalAuditEvent, Lane, SpendRollup } from "./types.ts";

const entry = {
  title: "Release approval", authority: "owner", urgency: "blocking_today",
  category: "release", recommendation: "Release after review", requestedBy: "product",
} as const;

async function fixture(t: TestContext) {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-api-"));
  const socketPath = join(directory, "api.sock");
  const store = PlatformStore.open(":memory:");
  const server = await startPlatformApiServer({ store, socketPath });
  t.after(async () => {
    try {
      await server.close();
      assert.equal(existsSync(socketPath), false);
    } finally {
      store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });
  return { store, socketPath, client: new PlatformApiClient(socketPath) };
}

function rawPost(socketPath: string, path: string, body?: string, length?: number):
  Promise<{ status: number | undefined; contentType: string | undefined; data: unknown }> {
  return new Promise((resolve, reject) => {
    const headers = length === undefined ? { "transfer-encoding": "chunked" } : { "content-length": length };
    const req = request({ socketPath, method: "POST", path, headers }, (response) => {
      response.setEncoding("utf8");
      let text = "";
      response.on("data", (chunk: string) => { text += chunk; });
      response.once("error", reject);
      response.once("end", () => {
        try {
          resolve({
            status: response.statusCode, contentType: response.headers["content-type"],
            data: JSON.parse(text) as unknown,
          });
        } catch (error) {
          reject(error);
        }
      });
    });
    req.once("error", reject);
    req.end(body);
  });
}

test("the Unix socket is private, and the decision API returns 201, 404, and 409", async (t) => {
  const { store, socketPath, client } = await fixture(t);
  assert.equal(statSync(socketPath).mode & 0o777, 0o600);
  assert.deepEqual(await client.request("GET", "/v1/health"), { ok: true });
  await assert.rejects(startPlatformApiServer({ store, socketPath }), { code: "EADDRINUSE" });
  assert.deepEqual(await client.request("GET", "/v1/health"), { ok: true });

  const response = await rawPost(socketPath, "/v1/decisions", JSON.stringify(entry));
  assert.equal(response.status, 201);
  assert.equal(response.contentType, "application/json");
  const pending = response.data as Decision;
  assert.equal(pending.status, "pending");
  assert.deepEqual(await client.request("GET", "/v1/decisions?status=pending"), [pending]);
  assert.deepEqual(await client.request("GET", `/v1/decisions/${pending.id}`), pending);
  await assert.rejects(client.request("GET", "/v1/decisions/missing"), { status: 404 });
  await assert.rejects(client.request("GET", "/v1/unknown"), PlatformApiError);
  assert.equal((await client.request<DecisionVerification>("GET", "/v1/decisions/missing/verify")).status, "not_found");

  const answer = { status: "approved", decidedBy: "owner", answer: { permitted: true } };
  const endpoint = `/v1/decisions/${pending.id}`;
  const approved = await client.request<Decision>("POST", `${endpoint}/answer`, answer);
  assert.deepEqual(approved.answer, answer.answer);
  assert.equal((await client.request<DecisionVerification>("GET", `${endpoint}/verify`)).valid, true);
  await assert.rejects(client.request("POST", `${endpoint}/answer`, answer), { status: 409 });
  const replacement = await client.request<Decision>("POST", "/v1/decisions", { ...entry, id: "replacement" });
  await assert.rejects(client.request("POST", "/v1/decisions", { ...entry, id: "replacement" }), { status: 409 });
  const revoked = await client.request<Decision>("POST", `${endpoint}/supersede`, { supersededById: replacement.id, actor: "owner" });
  assert.deepEqual(revoked, { ...approved, status: "superseded", supersededById: replacement.id });
  assert.equal((await client.request<DecisionVerification>("GET", `${endpoint}/verify`)).valid, false);
  assert.equal((await client.request<Decision>("POST", "/v1/decisions/replacement/decide", answer)).status, "approved");
});

test("the Unix server rejects public and symlinked socket parents before it binds", async (t) => {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-api-parent-"));
  const store = PlatformStore.open(":memory:");
  t.after(() => {
    store.close();
    rmSync(directory, { recursive: true, force: true });
  });
  const publicParent = join(directory, "public");
  mkdirSync(publicParent);
  chmodSync(publicParent, 0o755);
  const publicSocket = join(publicParent, "api.sock");
  await assert.rejects(startPlatformApiServer({ store, socketPath: publicSocket }), /socket parent must be a private directory/);
  assert.equal(existsSync(publicSocket), false);

  const symlink = join(directory, "linked");
  symlinkSync(directory, symlink);
  await assert.rejects(startPlatformApiServer({ store, socketPath: join(symlink, "api.sock") }));
  assert.equal(existsSync(join(directory, "api.sock")), false);
});

test("invalid JSON, values, and oversized bodies return JSON errors", async (t) => {
  const { socketPath, client, store } = await fixture(t);
  const bad = await rawPost(socketPath, "/v1/decisions", "[");
  assert.equal(bad.status, 400);
  assert.equal(bad.contentType, "application/json");
  await assert.rejects(client.request("POST", "/v1/decisions", null), { status: 400 });
  await assert.rejects(client.request("POST", "/v1/decisions", { ...entry, urgency: "soon" }), { status: 400 });
  await assert.rejects(client.request("POST", "/v1/decisions", { ...entry, laneId: "missing" }), { status: 400 });
  await assert.rejects(client.request("GET", "/v1/decisions?status=unknown"), { status: 400 });
  await assert.rejects(client.request("GET", "/v1/decisions/%"), { status: 400 });
  for (const segment of [".", "..", "%2E%2E", "one%2Ftwo"]) {
    await assert.rejects(client.request("GET", `/v1/decisions/${segment}/verify`), { status: 400 });
    await assert.rejects(client.request("GET", `/v1/lanes/${segment}`), { status: 400 });
  }
  const bytes = 1024 * 1024 + 1;
  for (const result of [
    await rawPost(socketPath, "/v1/decisions", undefined, bytes),
    await rawPost(socketPath, "/v1/decisions", " ".repeat(bytes)),
  ]) {
    assert.deepEqual(result, {
      status: 413, contentType: "application/json", data: { error: "request body exceeds 1 MiB" },
    });
  }
  assert.deepEqual(store.listDecisions(), []);
});

test("the goal completion route accepts requests with and without a JSON body", async (t) => {
  const { client, store } = await fixture(t);
  const lane = store.createLane({ slug: "releases", name: "Releases" });
  const lanePath = `/v1/lanes/${lane.slug}`;
  const completePath = `${lanePath}/goal/complete`;
  const input = { laneId: lane.id, statement: "Ship the release", metricName: "Releases", metricCurrent: 1,
    metricTarget: 2, weekStart: "2026-11-23" };
  const goal = store.setGoal(input);

  await assert.rejects(client.request("GET", completePath), { status: 404 });
  await assert.rejects(client.request("POST", completePath, null), { status: 400 });
  await assert.rejects(client.request("POST", completePath, { weekStart: "2026-02-30" }), { status: 400 });
  const completed = await client.request<Goal>("POST", completePath, { weekStart: goal.weekStart, metricCurrent: 2 });
  assert.ok(completed.completedAt);
  assert.deepEqual(completed, { ...goal, completedAt: completed.completedAt, updatedAt: completed.completedAt, metricCurrent: 2 });
  assert.deepEqual(await client.request("GET", lanePath), { ...lane, goal: completed });
  await assert.rejects(client.request("POST", completePath), { status: 404 });

  const replacement = store.setGoal({ ...input, statement: "Ship the next release" });
  assert.deepEqual(await client.request("GET", lanePath), { ...lane, goal: replacement });
  const closedReplacement = await client.request<Goal>("POST", completePath);
  assert.ok(closedReplacement.completedAt);
  assert.equal(closedReplacement.id, replacement.id);
  assert.equal(closedReplacement.metricCurrent, replacement.metricCurrent);
});

test("the goal list returns completed history and filters by lane slug", async (t) => {
  const { store, client } = await fixture(t);
  const product = store.createLane({ slug: "product", name: "Product" });
  const docs = store.createLane({ slug: "docs", name: "Docs" });
  store.createLane({ slug: "empty", name: "Empty" });
  const input = { statement: "Ship", metricName: "Releases", metricTarget: 1, weekStart: "2026-11-23" };
  assert.deepEqual(await client.request("GET", "/v1/goals"), []);

  const older = store.setGoal({ ...input, laneId: docs.id, weekStart: "2026-11-16" });
  store.setGoal({ ...input, laneId: product.id });
  const completed = store.completeGoal(product.id, { metricCurrent: 1 });
  assert.deepEqual(await client.request("GET", "/v1/goals"), [completed, older]);
  assert.deepEqual(await client.request("GET", "/v1/goals?lane=product"), [completed]);
  assert.deepEqual(await client.request("GET", "/v1/goals?lane=docs"), [older]);
  assert.deepEqual(await client.request("GET", "/v1/goals?lane=empty"), []);
  await assert.rejects(client.request("GET", "/v1/goals?lane=unknown"), { status: 404 });
});

test("the owner goal route enforces lane ownership and exposes a filterable audit log", async (t) => {
  const { client, store } = await fixture(t);
  const owner = store.upsertAgent({ id: "manager", role: "manager", connectorKind: "t3", hostId: "east",
    externalRef: "environment/thread-1", label: "Manager", status: "active" });
  const lane = store.createLane({ slug: "releases", name: "Releases", ownerAgentId: owner.id });
  const other = store.createLane({ slug: "unowned", name: "Unowned" });
  const path = `/v1/lanes/${lane.slug}/goal/self`;
  const input = { action: "set", callerThread: "thread-1", statement: "Ship the release", metricName: "Releases", metricTarget: 2 };
  await assert.rejects(client.request("GET", path), { status: 404 });
  await assert.rejects(client.request("POST", path, { ...input, callerThread: "foreign-thread" }), { status: 403 });
  await assert.rejects(client.request("POST", "/v1/lanes/unowned/goal/self", input), { status: 403 });
  await assert.rejects(client.request("POST", "/v1/lanes/missing/goal/self", input), { status: 404 });
  await assert.rejects(client.request("POST", path, { ...input, action: "delete" }), { status: 400 });
  await assert.rejects(client.request("POST", path, { ...input, callerThread: 1 }), { status: 400 });
  assert.deepEqual(await client.request("GET", "/v1/goals/audit"), []);

  type OwnerResult = { goal: Goal; audit: GoalAuditEvent };
  const set = await client.request<OwnerResult>("POST", path, { ...input, laneId: lane.id, laneSlug: lane.slug });
  assert.equal(set.goal.laneId, lane.id);
  assert.equal(set.audit.action, "set");
  const otherGoal = store.setGoal({ laneId: other.id, statement: "Triage incoming work", metricName: "Items", metricTarget: 4 });
  for (const action of ["set", "complete"]) {
    for (const conflict of [{ laneId: other.id }, { laneSlug: other.slug }]) {
      await assert.rejects(client.request("POST", path, { ...input, ...conflict, action, metricCurrent: 2 }),
        { status: 403, message: /goal request lane does not match route lane/ });
    }
  }
  assert.deepEqual(store.listGoals(lane.id), [set.goal]);
  assert.deepEqual(store.listGoals(other.id), [otherGoal]);
  assert.deepEqual(store.listGoalAuditEvents(), [set.audit]);

  const complete = await client.request<OwnerResult>("POST", path,
    { action: "complete", callerThread: owner.externalRef, weekStart: set.goal.weekStart, metricCurrent: 2 });
  assert.equal(complete.goal.id, set.goal.id);
  assert.equal(complete.goal.metricCurrent, 2);
  assert.equal(complete.audit.action, "complete");
  assert.deepEqual(await client.request("GET", "/v1/goals/audit"), [complete.audit, set.audit]);
  assert.deepEqual(await client.request("GET", `/v1/goals/audit?lane=${lane.slug}`), [complete.audit, set.audit]);
  assert.deepEqual(await client.request("GET", "/v1/goals/audit?lane=unowned"), []);
  await assert.rejects(client.request("GET", "/v1/goals/audit?lane=missing"), { status: 404 });
});

test("registry, policy, friction, budget, agent, and run endpoints use the same stored records", async (t) => {
  const { client, store } = await fixture(t);
  const lane = await client.request<Lane>("POST", "/v1/lanes", { slug: "platform", name: "Platform" });
  assert.deepEqual(await client.request("GET", "/v1/lanes"), [lane]);
  assert.deepEqual(await client.request("GET", "/v1/lanes/platform"), { ...lane, goal: null });
  await assert.rejects(client.request("GET", "/v1/lanes/missing"), { status: 404 });

  const goalRequest = { statement: "Record decisions.", metricName: "Decisions", metricTarget: 2, metricCurrent: 0 };
  const pending = await client.request<Decision>("POST", "/v1/decisions", { ...entry, laneId: lane.id });
  await assert.rejects(client.request("PUT", "/v1/lanes/platform/goal", { ...goalRequest, approvedDecisionId: pending.id }), { status: 400 });
  const goal = await client.request<Goal>("PUT", "/v1/lanes/platform/goal", goalRequest);
  assert.equal(goal.laneId, lane.id);
  assert.equal(goal.metricCurrent, 0);
  assert.deepEqual(await client.request("GET", "/v1/lanes/platform"), { ...lane, goal });

  assert.deepEqual(await client.request("GET", "/v1/policy/rules"), store.listPolicyRules());
  const policy = store.recordPolicyEvent({ ruleCode: "authority_routing", summary: "Review routing", laneId: lane.id });
  assert.deepEqual(await client.request("GET", "/v1/policy/events?ruleCode=authority_routing"), [policy]);
  const friction = await client.request("POST", "/v1/friction", {
    kind: "owner_intervention", summary: "Clarify routing", laneId: lane.id, ruleCode: "authority_routing",
  });
  assert.deepEqual(await client.request("GET", "/v1/friction?status=open"), [friction]);
  assert.deepEqual(await client.request("GET", "/v1/friction?status=kept"), []);
  const budget = await client.request("PUT", "/v1/budgets", {
    scopeKind: "lane", scopeId: lane.id, period: "weekly", capUsd: 20, warnPct: 75, spendSource: "semon",
  });
  assert.deepEqual(await client.request("GET", "/v1/budgets"), [budget]);
  const agent = store.upsertAgent({ id: "manager", role: "manager", laneId: lane.id, connectorKind: "t3",
    hostId: "host", externalRef: "external-agent", label: "Manager", status: "idle" });
  const lanePath = "/v1/lanes/platform";
  const update = { name: "Platform Work", status: "paused", ownerAgentId: agent.id, priorityRank: 5 };
  const updatedLane = await client.request<Lane>("PUT", lanePath, update);
  assert.deepEqual(updatedLane, { ...lane, ...update, updatedAt: updatedLane.updatedAt });
  assert.deepEqual(await client.request("GET", lanePath), { ...updatedLane, goal });
  const cleared = await client.request<Lane>("PUT", lanePath, { ownerAgentId: null });
  assert.deepEqual(cleared, { ...updatedLane, ownerAgentId: null, updatedAt: cleared.updatedAt });
  for (const invalid of [{ name: " " }, { status: "blocked" }, { ownerAgentId: "" }, { ownerAgentId: "missing" },
    { priorityRank: 0.5 }, { priorityRank: null }]) {
    await assert.rejects(client.request("PUT", lanePath, invalid), { status: 400 });
  }
  await assert.rejects(client.request("PUT", "/v1/lanes/missing", update), { status: 404 });
  assert.deepEqual(await client.request("GET", lanePath), { ...cleared, goal });
  const run = store.upsertRun({ id: "run", agentId: agent.id, laneId: lane.id, connectorKind: "t3", hostId: "host",
    externalRunRef: "external-run", status: "completed" });
  assert.deepEqual(await client.request("GET", "/v1/agents"), [agent]);
  assert.deepEqual(await client.request("GET", "/v1/runs"), [run]);
  const link = "/v1/runs/run/semon-session";
  await assert.rejects(client.request("PUT", link, { sessionUid: "bad uid" }), { status: 400 });
  await assert.rejects(client.request("PUT", "/v1/runs/missing/semon-session", { sessionUid: "uid-1" }), { status: 404 });
  assert.deepEqual(await client.request("PUT", link, { sessionUid: "uid-1" }),
    { ...store.listRuns()[0], semonSessionUid: "uid-1" });
  assert.equal(store.listRuns()[0]?.semonSessionUid, "uid-1");
});

test("lane detail returns scoped live records, joined PR checks and a bounded timeline", async (t) => {
  const { client, store } = await fixture(t);
  const now = new Date();
  const at = (hoursAgo: number) => new Date(now.getTime() - hoursAgo * 3_600_000).toISOString();
  const lane = store.createLane({ slug: "crawlab", name: "Crawlab" });
  const other = store.createLane({ slug: "other", name: "Other" });
  const host = { hostId: "fixture-host", label: "Fixture host", mcpUrl: "https://fixture.example/mcp",
    originNamespace: null, connectionState: "connected" as const };
  const pr = (threadId: string, repository: string, number: number): T3PrInput => ({
    hostId: host.hostId, threadId, prHost: "github.com", repository, number,
    url: `https://github.com/${repository}/pull/${number}`, source: "manual", state: "open", watching: true,
    title: `Change ${number}`, headBranch: null, baseBranch: null, isDraft: false,
  });
  const alias = pr("foreign-thread", "org/Crawlab-Pro", 12);
  const ownerPr = pr("owner-thread", "org/tools", 9);
  const merged: T3PrInput = { ...pr("foreign-thread", "org/crawlab-docs", 3), state: "merged" };
  const snapshots: T3ThreadSnapshot[] = [{
    threadId: "owner-thread",
    agent: { id: "manager", role: "manager", connectorKind: "t3", hostId: host.hostId,
      externalRef: "environment/owner-thread", label: "Lane Manager", status: "active", lastSeenAt: at(1) },
    runs: [{ id: "owner-run", agentId: "manager", connectorKind: "t3", hostId: host.hostId,
      externalRunRef: "private-owner-run", status: "running", model: "openai/gpt-6.2-sol", startedAt: at(1) }],
    prs: [ownerPr, pr("owner-thread", "org/crawlab-pro", alias.number)],
  }, {
    threadId: "worker-thread",
    agent: { id: "worker", role: "worker", connectorKind: "t3", hostId: host.hostId, laneId: lane.id,
      externalRef: "environment/worker-thread", label: "Lane Worker", status: "idle", lastSeenAt: at(2) },
    runs: [{ id: "legacy-run", agentId: "worker", connectorKind: "t3", hostId: host.hostId,
      externalRunRef: "private-legacy-run", status: "stalled", startedAt: at(2) }],
  }, {
    threadId: "foreign-thread",
    agent: { id: "foreign", role: "worker", connectorKind: "t3", hostId: host.hostId, laneId: other.id,
      externalRef: "environment/foreign-thread", label: "Other Worker", status: "waiting", lastSeenAt: at(1) },
    runs: [{ id: "foreign-run", agentId: "foreign", connectorKind: "t3", hostId: host.hostId,
      externalRunRef: "private-foreign-run", status: "waiting_approval", startedAt: at(1) }],
    prs: [alias, merged, pr("foreign-thread", "org/unrelated", 99)],
  }];
  store.reconcileT3Host(host, snapshots, { observedAt: now.toISOString() });
  store.upsertAgent({ id: "old-idle", role: "worker", laneId: lane.id, connectorKind: "codex", hostId: host.hostId,
    externalRef: "private-idle-ref", label: "Old idle", status: "idle", lastSeenAt: at(25) });
  store.upsertRun({ id: "lane-explicit-run", agentId: "foreign", laneId: lane.id, connectorKind: "t3", hostId: host.hostId,
    externalRunRef: "private-explicit-run", status: "waiting_input", startedAt: at(3) });
  const ownedLane = store.updateLane(lane.slug, { ownerAgentId: "manager" });
  const goalInput = { statement: "Ship the first update", metricName: "Releases", metricTarget: 1 };
  store.setGoalAsOwner(lane.slug, "owner-thread", goalInput);
  const completed = store.completeGoalAsOwner(lane.slug, "owner-thread", { metricCurrent: 1 }).goal;
  const current = store.setGoalAsOwner(lane.slug, "owner-thread", { ...goalInput, statement: "Ship the second update" }).goal;
  store.setGoal({ ...goalInput, laneId: other.id, statement: "Another lane goal" });

  for (const sha of ["a", "b"]) store.recordGitHubPrCheck("org/crawlab-pro", alias.number, {
    headSha: sha.repeat(40), isDraft: true, rollupState: "failure", totalChecks: 25, failedChecks: 1,
  });
  store.recordT3PrWake({ ...alias, outcome: "failed", summary: "Checks failed", occurredAt: at(3), watchEnded: false });
  store.recordT3PrWake({ ...merged, outcome: "completed", summary: "Merged", occurredAt: at(4), watchEnded: true });

  const pending = store.createDecision({ ...entry, laneId: lane.id });
  const answered = store.answerDecision(store.createDecision({ ...entry, laneId: lane.id }).id,
    { status: "approved", decidedBy: "owner", answer: "Release" });
  store.createDecision({ ...entry, laneId: other.id });
  const alert = store.recordPolicyEvent({ ruleCode: "pr_green_ci", summary: "Checks failing", laneId: lane.id });
  store.recordPolicyEvent({ ruleCode: "pr_green_ci", summary: "Unrelated alert", laneId: other.id });
  store.recordDedupedPolicyEvent({ ruleCode: "pr_green_ci", summary: "Earlier failure", laneId: lane.id }, "earlier", at(5));
  store.resolveInactivePolicyEvents("pr_green_ci", [], at(4));
  const friction = store.recordFriction({ kind: "correction", laneId: lane.id, summary: "Clarify next step" });
  store.recordFriction({ kind: "correction", laneId: other.id, summary: "Unrelated friction" });
  store.upsertRun({ id: "priced-finished-run", agentId: "worker", laneId: lane.id, connectorKind: "t3", hostId: host.hostId,
    externalRunRef: "private-priced-run", status: "completed", startedAt: now.toISOString(), semonSessionUid: "fixture-session" });
  store.upsertSpendSnapshot({ sessionUid: "fixture-session", source: "semon", usdEst: 1, coverage: "complete" });

  const path = "/v1/lanes/%63rawlab/detail";
  type Detail = ReturnType<typeof readLaneDetail>;
  const detail = await client.request<Detail>("GET", path);
  assert.deepEqual(detail.lane, ownedLane);
  assert.deepEqual(detail.goal, current);
  assert.deepEqual(detail.goalHistory, [current, completed]);
  assert.deepEqual(detail.prs.map(({ number }) => number).sort((a, b) => a - b), [3, 9, 12]);
  const checks = detail.prs.find(({ number }) => number === alias.number);
  assert(checks);
  assert.equal(checks.rollupState, "failure");
  assert.equal(checks.failedChecks, 1);
  assert.equal(checks.totalChecks, 25);
  assert.equal(checks.consecutiveFailures, 2);
  assert.equal(checks.isDraft, true);
  assert.equal(checks.lastError, null);
  assert(checks.lastCheckedAt);
  assert.equal(checks.lastWakeAt, at(3));
  assert.deepEqual(detail.activeRuns.map(({ agentLabel, isLaneOwner, status, model, startedAt }) =>
    [agentLabel, isLaneOwner, status, model, startedAt]), [
    ["Lane Manager", true, "running", "openai/gpt-6.2-sol", at(1)],
    ["Lane Worker", false, "unknown", null, at(2)],
    ["Other Worker", false, "waiting_input", null, at(3)],
  ]);
  assert.deepEqual(detail.recentAgents, [
    { id: "manager", label: "Lane Manager", role: "manager", status: "active", lastSeenAt: at(1) },
    { id: "worker", label: "Lane Worker", role: "worker", status: "idle", lastSeenAt: at(2) },
  ]);
  assert.deepEqual(new Set(detail.decisions.map(({ id }) => id)), new Set([pending.id, answered.id]));
  assert.deepEqual(detail.alerts, [alert]);
  assert.deepEqual(detail.friction, [friction]);
  assert.equal(detail.spend.status, "unavailable");
  assert.equal(detail.spend.preferredUsd, null);
  assert.equal(detail.spend.scopeId, lane.id);
  assert.equal(detail.spend.period, "weekly");
  assert.deepEqual(new Set(detail.timeline.map(({ kind }) => kind)), new Set([
    "goal_updated", "goal_completed", "decision_created", "decision_answered", "policy_alert", "pr_wake", "pr_merged",
  ]));
  assert.deepEqual(detail.timeline.slice(-3).map(({ at: timestamp }) => timestamp), [at(3), at(4), at(5)]);
  assert.doesNotMatch(JSON.stringify(detail), /externalRef|externalRunRef|private-|callerThread|owner-thread|fixture\.example|Unrelated|Another lane goal/);

  for (let i = 0; i < 26; i++) {
    store.recordPolicyEvent({ ruleCode: "pr_green_ci", summary: `Recent alert ${i}`, laneId: lane.id });
  }
  const rollupObserved = new Date();
  store.upsertSpendRollup({ scopeKind: "lane", scopeId: lane.id, period: "weekly", source: "semon",
    ...spendWindow("weekly", rollupObserved), observedAt: rollupObserved.toISOString(), sessionCount: 2,
    unpricedCount: 0, incompleteCount: 0, usdEst: 8, sourceReportedUsd: null, billedUsd: null, preferredUsd: 8,
    coverage: "complete", snapshotId: null, asOf: null });
  const updated = await client.request<Detail>("GET", path);
  assert.equal(updated.timeline.length, 25);
  assert.equal(updated.spend.status, "available");
  assert.equal(updated.spend.preferredUsd, 8);
  await assert.rejects(client.request("GET", "/v1/lanes/unknown/detail"), { status: 404 });
  await assert.rejects(client.request("PUT", path, {}), { status: 404 });
});

test("the Unix API writes budgets and exposes spend and GitHub PR observations", async (t) => {
  const { store, client } = await fixture(t);
  const budget = await client.request("PUT", "/v1/budgets", {
    scopeKind: "team", period: "daily", capUsd: 10, warnPct: 80, spendSource: "semon",
  });
  assert.deepEqual(await client.request("GET", "/v1/budgets"), [budget]);
  await assert.rejects(client.request("GET", "/v1/spend/summary?scopeKind=invalid"), { status: 400 });
  await assert.rejects(client.request("GET", "/v1/spend/summary?scopeKind=lane"), { status: 400 });
  const spend = await client.request<{ status: string; billedUsd: number | null }>("GET", "/v1/spend/summary");
  assert.equal(spend.status, "unavailable");
  assert.equal(spend.billedUsd, null);

  store.recordGitHubPrCheck("org/repo", 7, {
    headSha: "a".repeat(40), isDraft: false, rollupState: "failure", totalChecks: 2, failedChecks: 1,
  });
  assert.deepEqual(await client.request("GET", "/v1/github/prs"), store.listGitHubPrChecks());
  await assert.rejects(client.request("PUT", "/v1/github/prs", {}), { status: 404 });
});

test("the Unix API upserts individual rollups and applies a batch atomically", async (t) => {
  const { store, socketPath, client } = await fixture(t);
  const now = new Date();
  const rollup: SpendRollup = {
    scopeKind: "team", scopeId: null, period: "monthly", source: "semon", ...spendWindow("monthly", now),
    observedAt: now.toISOString(), sessionCount: 2, unpricedCount: 3, incompleteCount: 4,
    usdEst: 8, sourceReportedUsd: null, billedUsd: null, preferredUsd: 8, coverage: "partial", snapshotId: "generation", asOf: null,
  };
  const route = "/v1/spend/rollups";
  const posted = await rawPost(socketPath, route, JSON.stringify(rollup));
  assert.equal(posted.status, 200);
  assert.deepEqual(posted.data, rollup);
  const summary = await client.request<{ status: string; preferredUsd: number; coverage: string }>("GET", "/v1/spend/summary");
  assert.equal(summary.status, "available");
  assert.equal(summary.preferredUsd, 8);
  assert.equal(summary.coverage, "partial");

  const weekly: SpendRollup = { ...rollup, ...spendWindow("weekly", now), period: "weekly" };
  const updated = { ...rollup, usdEst: 9, preferredUsd: 9 };
  await assert.rejects(client.request("POST", route, [updated, { ...weekly, incompleteCount: -1 }]), { status: 400 });
  assert.deepEqual(store.getSpendRollup(rollup, rollup.since), rollup);
  assert.equal(store.getSpendRollup(weekly, weekly.since), null);
  assert.deepEqual(await client.request("POST", route, [updated, weekly]), [updated, weekly]);
  assert.deepEqual(store.getSpendRollup(weekly, weekly.since), weekly);
  await assert.rejects(client.request("POST", route, null), { status: 400 });
});
