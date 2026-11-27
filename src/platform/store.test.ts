import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";
import type { TestContext } from "node:test";

import { unavailableSpendSource } from "./spend.ts";
import { DecisionStateError, PlatformStore, PlatformValidationError } from "./store.ts";
import type { CreateDecisionInput } from "./store.ts";

const request = {
  title: "Approve the weekly plan",
  authority: "owner",
  category: "weekly_plan",
  urgency: "blocking_today",
  recommendation: "Approve",
  requestedBy: "lane-manager",
  evidence: { measured: 3 },
  options: ["Approve", "Defer"],
} as const satisfies Omit<CreateDecisionInput, "options"> & { options: readonly string[] };

function decisionInput(id?: string): CreateDecisionInput {
  return { ...request, options: [...request.options], ...(id === undefined ? {} : { id }) };
}

function memory(t: TestContext, now: () => string = () => "2026-11-27T04:00:00Z"): PlatformStore {
  const store = PlatformStore.open(":memory:", { now });
  t.after(() => store.close());
  return store;
}

function disk(t: TestContext): { store: PlatformStore; db: DatabaseSync; path: string } {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-platform-store-"));
  const path = join(directory, "platform.sqlite");
  const store = PlatformStore.open(path);
  const db = new DatabaseSync(path);
  t.after(() => {
    db.close();
    store.close();
    rmSync(directory, { recursive: true, force: true });
  });
  return { store, db, path };
}

test("decisions validate input, persist evidence, filter the inbox, and record the request", (t) => {
  const store = memory(t);
  const lane = store.createLane({ slug: "product", name: "Product" });
  const input = decisionInput();
  for (const field of ["title", "recommendation", "requestedBy"] as const) {
    assert.throws(() => store.createDecision({ ...input, [field]: "  " }), PlatformValidationError);
  }
  assert.throws(() => store.createDecision({ ...input, laneId: "unknown" }), PlatformValidationError);
  assert.deepEqual(store.listDecisions(), []);

  const entry = store.createDecision({ ...input, laneId: lane.id });
  assert.match(entry.id, /^dec_[0-9a-f]{12}$/);
  assert.equal(entry.status, "pending");
  assert.equal(entry.createdAt, "2026-11-27T04:00:00.000Z");
  assert.deepEqual(entry.evidence, input.evidence);
  assert.deepEqual(entry.options, input.options);
  assert.deepEqual(store.getDecision(entry.id), entry);
  assert.deepEqual(store.listDecisionEvents(entry.id).map(({ kind, actor }) => [kind, actor]), [["created", "lane-manager"]]);

  const other = store.createDecision({ ...decisionInput("dec_other"), authority: "chief_pm", urgency: "digest" });
  assert.deepEqual(store.listDecisions({ status: "pending", authority: "owner", urgency: "blocking_today", laneId: lane.id }), [entry]);
  assert.deepEqual(store.listDecisions({ laneId: null }), [other]);
  assert.deepEqual(store.listDecisions({ status: "approved" }), []);
});

test("decision IDs and lane slugs reject dot segments, separators, and other unsafe values", (t) => {
  const store = memory(t);
  for (const id of [".", "..", "one/two", "one\\two", "one two", "a".repeat(129)]) {
    assert.throws(() => store.createDecision(decisionInput(id)), PlatformValidationError);
    assert.throws(() => store.getDecision(id), PlatformValidationError);
    assert.throws(() => store.createLane({ slug: id, name: "Product" }), PlatformValidationError);
    assert.throws(() => store.getLaneBySlug(id), PlatformValidationError);
  }
  const id = "Product.2026_w48-1";
  assert.equal(store.createDecision(decisionInput(id)).id, id);
  const lane = store.createLane({ slug: id, name: "Product" });
  assert.deepEqual(store.getLaneBySlug(id), { ...lane });
  assert.deepEqual(store.listLanes(), [{ ...lane }]);
});

test("decisions are answered once and verification allows only an active approval", (t) => {
  const store = memory(t);
  const original = store.createDecision(decisionInput("dec_original"));
  const replacement = store.createDecision(decisionInput("dec_replacement"));
  const expires = store.createDecision(decisionInput("dec_expires"));
  assert.deepEqual(store.verifyDecision(original.id), {
    valid: false, status: "pending", reason: "decision dec_original is pending", decision: original,
  });
  assert.deepEqual(store.verifyDecision("missing"), {
    valid: false, status: "not_found", reason: "decision missing not found", decision: null,
  });

  const approval = { status: "approved", decidedBy: "owner", answer: { allowed: true }, rationale: "Reviewed" } as const;
  const answered = store.answerDecision(original.id, approval);
  assert.equal(answered.decidedAt, "2026-11-27T04:00:00.000Z");
  assert.equal(answered.decidedBy, approval.decidedBy);
  assert.deepEqual(answered.answer, approval.answer);
  assert.equal(answered.rationale, approval.rationale);
  assert.equal(store.verifyDecision(original.id).valid, true);
  assert.throws(() => store.answerDecision(original.id, approval), DecisionStateError);
  assert.throws(() => store.expireDecision(original.id, { actor: "owner" }), DecisionStateError);

  const rejected = store.answerDecision(replacement.id, { status: "rejected", decidedBy: "owner", answer: "Wait" });
  assert.equal(store.verifyDecision(rejected.id).valid, false);
  assert.throws(() => store.supersedeDecision(original.id, { supersededById: "missing", actor: "owner" }), PlatformValidationError);
  assert.throws(() => store.supersedeDecision(original.id, { supersededById: original.id, actor: "owner" }), PlatformValidationError);
  const superseded = store.supersedeDecision(original.id, { supersededById: replacement.id, actor: "lead", rationale: "Revisited" });
  assert.deepEqual(superseded, { ...answered, status: "superseded", supersededById: replacement.id });
  assert.equal(store.verifyDecision(original.id).valid, false);
  assert.throws(() => store.supersedeDecision(original.id, { supersededById: replacement.id, actor: "owner" }), DecisionStateError);
  assert.throws(() => store.answerDecision(original.id, approval), DecisionStateError);
  const events = store.listDecisionEvents(original.id);
  assert.deepEqual(events.map((event) => event.kind), ["created", "answered", "superseded"]);
  assert.deepEqual(events.at(-1)?.detail, { supersededById: replacement.id, rationale: "Revisited" });

  const expired = store.expireDecision(expires.id, { actor: "lead", rationale: "Window closed" });
  assert.equal(expired.status, "expired");
  assert.equal(store.verifyDecision(expires.id).valid, false);
  assert.deepEqual(store.listDecisionEvents(expires.id).at(-1)?.detail, { rationale: "Window closed" });
  assert.throws(() => store.answerDecision(expires.id, approval), DecisionStateError);
  assert.throws(() => store.supersedeDecision(expires.id, { supersededById: replacement.id, actor: "owner" }), DecisionStateError);

  const pending = store.createDecision(decisionInput());
  const another = store.createDecision(decisionInput());
  assert.equal(store.supersedeDecision(rejected.id, { supersededById: pending.id, actor: "lead" }).status, "superseded");
  assert.equal(store.supersedeDecision(pending.id, { supersededById: another.id, actor: "lead" }).status, "superseded");
});

test("raw SQLite rejects mutation of decided fields and replacement or mutation of audit events", (t) => {
  const { store, db, path } = disk(t);
  const original = store.createDecision(decisionInput("original"));
  const replacement = store.createDecision(decisionInput("replacement"));
  store.createDecision(decisionInput("expired"));
  store.expireDecision("expired", { actor: "owner" });
  const prior = store.answerDecision(original.id, { status: "approved", decidedBy: "owner", answer: "yes" });

  const assignments = [
    "id = 'renamed'", "lane_id = 'other'", "authority = 'chief_pm'", "category = 'routine'", "urgency = 'digest'",
    "title = 'different'", "evidence_json = 'null'", "recommendation = 'different'", "options_json = '[]'",
    "requested_by = 'other'", "decided_by = 'other'", "answer_json = 'null'", "rationale = 'changed'",
    "created_at = 'different'", "decided_at = 'different'",
  ];
  for (const assignment of assignments) {
    assert.throws(() => db.exec(`UPDATE decision SET status = 'superseded', superseded_by_id = 'replacement',
      ${assignment} WHERE id = 'original'`), /decision is immutable/);
  }
  assert.throws(() => db.exec("UPDATE decision SET status = 'superseded' WHERE id = 'original'"), /immutable/);
  assert.throws(() => db.exec("UPDATE decision SET status = 'pending' WHERE id = 'original'"), /immutable/);
  assert.throws(() => db.exec("DELETE FROM decision WHERE id = 'original'"), /cannot be deleted/);
  assert.throws(() => db.exec("INSERT OR REPLACE INTO decision SELECT * FROM decision WHERE id = 'original'"), /already exists/);
  assert.throws(() => db.exec("UPDATE OR REPLACE decision SET id = 'original' WHERE id = 'replacement'"), /immutable/);
  assert.throws(() => db.exec("UPDATE decision_event SET actor = 'other'"), /append-only/);
  assert.throws(() => db.exec("DELETE FROM decision_event"), /append-only/);
  assert.throws(() => db.exec("INSERT OR REPLACE INTO decision_event SELECT * FROM decision_event LIMIT 1"), /append-only/);
  assert.deepEqual(store.getDecision(original.id), prior);

  assert.equal(store.supersedeDecision(original.id, { supersededById: replacement.id, actor: "owner" }).status, "superseded");
  assert.throws(() => db.exec("UPDATE decision SET title = title WHERE id IN ('expired', 'original')"), /immutable/);
  assert.equal(db.prepare("PRAGMA journal_mode").get()?.["journal_mode"], "wal");

  const reopened = PlatformStore.open(path);
  t.after(() => reopened.close());
  assert.equal(reopened.getDecision(original.id)?.status, "superseded");
  assert.equal(reopened.listPolicyRules().length, 5);
});

test("a failed event insert rolls back decision creation and answer", (t) => {
  const { store, db } = disk(t);
  const original = store.createDecision(decisionInput());
  db.exec(`CREATE TRIGGER reject_event BEFORE INSERT ON decision_event
    BEGIN SELECT RAISE(ABORT, 'event insert failed'); END`);
  assert.throws(() => store.createDecision(decisionInput("missing")), /event insert failed/);
  assert.equal(store.getDecision("missing"), null);
  assert.throws(() => store.answerDecision(original.id, {
    status: "approved", decidedBy: "owner", answer: "yes",
  }), /event insert failed/);
  assert.deepEqual(store.getDecision(original.id), original);
  assert.deepEqual(store.listDecisionEvents(original.id).map(({ kind }) => kind), ["created"]);
});

test("weekly goal approvals must be approved for the right category, lane, and plan", (t) => {
  const store = memory(t);
  const lane = store.createLane({ slug: "product", name: "Product", priorityRank: 2 });
  const other = store.createLane({ slug: "ops", name: "Operations", priorityRank: 1 });
  assert.deepEqual(store.listLanes(), [other, lane]);
  assert.deepEqual(store.getLaneById(lane.id), lane);
  assert.deepEqual(store.getLaneBySlug(lane.slug), lane);
  assert.equal(store.updateLaneStatus(other.id, "paused").status, "paused");

  const approved = store.createDecision(decisionInput());
  const rejected = store.createDecision(decisionInput());
  const expires = store.createDecision(decisionInput());
  const superseded = store.createDecision(decisionInput());
  const wrongLane = store.createDecision({ ...decisionInput(), laneId: other.id });
  const wrongCategory = store.createDecision({ ...decisionInput(), laneId: lane.id, category: "release" });
  const approve = (id: string) => store.answerDecision(id, { status: "approved", decidedBy: "owner", answer: "yes" });
  approve(wrongLane.id);
  approve(wrongCategory.id);
  store.answerDecision(rejected.id, { status: "rejected", decidedBy: "owner", answer: null });
  store.expireDecision(expires.id, { actor: "lead" });
  store.supersedeDecision(superseded.id, { supersededById: approved.id, actor: "lead" });
  const goal = { laneId: lane.id, statement: "Ship the release.", metricName: "Releases", metricTarget: 1 };
  for (const id of [approved.id, rejected.id, expires.id, superseded.id, "missing"]) {
    assert.throws(() => store.setGoal({ ...goal, approvedDecisionId: id }), PlatformValidationError);
  }
  for (const id of [wrongLane.id, wrongCategory.id]) {
    assert.throws(() => store.setGoal({ ...goal, approvedDecisionId: id }), /must approve a weekly plan for this lane or the team/);
  }
  assert.deepEqual(store.listGoals(), []);

  approve(approved.id);
  const plan = { ...goal, approvedDecisionId: approved.id, weeklyPlan: ["Test", "Ship"] };
  const current = store.setGoal({ ...plan, metricCurrent: 0 });
  assert.equal(current.weekStart, "2026-11-23");
  assert.equal(current.approvedDecisionId, approved.id);
  assert.equal(current.metricCurrent, 0);
  assert.deepEqual(store.getLatestGoal(lane.id), current);
  assert.deepEqual(store.listGoals(lane.id), [current]);
  const earlier = store.setGoal({ ...goal, weekStart: "2026-11-16" });
  assert.deepEqual(store.listGoals(lane.id), [current, earlier]);
  assert.deepEqual(store.setGoal({ ...plan, metricCurrent: 1 }), { ...current, metricCurrent: 1 });
  for (const update of [{ statement: "Ship two releases." }, { weeklyPlan: ["Ship"] }]) {
    assert.throws(() => store.setGoal({ ...plan, ...update }), /a changed weekly plan requires a new approval/);
    assert.equal(store.getLatestGoal(lane.id)?.approvedDecisionId, approved.id);
  }
  const cleared = store.setGoal({ ...goal, weeklyPlan: ["Ship"] });
  assert.equal(cleared.id, current.id);
  assert.equal(cleared.approvedDecisionId, null);
  assert.equal(store.getLatestGoal(lane.id)?.approvedDecisionId, null);

  const fresh = store.createDecision({ ...decisionInput(), laneId: lane.id });
  approve(fresh.id);
  assert.equal(store.setGoal({ ...goal, weeklyPlan: ["Review", "Ship"], approvedDecisionId: fresh.id }).approvedDecisionId, fresh.id);
  assert.equal(store.setGoal({ ...goal, weeklyPlan: ["Ship"], approvedDecisionId: null }).approvedDecisionId, null);
  assert.throws(() => store.setGoal({ ...goal, weekStart: "2026-02-30" }), PlatformValidationError);
});

test("seeded policy rules observe, and the friction count uses UTC days and owner interventions only", (t) => {
  let now = "2026-11-27T23:59:59Z";
  const store = memory(t, () => now);
  const rules = store.listPolicyRules();
  assert.deepEqual(rules.map(({ code }) => code), [
    "authority_routing", "host_limits", "pr_green_ci", "registered_automation_only", "two_failure_stop",
  ]);
  assert.ok(rules.every((rule) => rule.mode === "observe" && rule.enabled && rule.sourceRef.startsWith("team-policy.md")));
  const policyEvent = store.recordPolicyEvent({ ruleCode: "host_limits", summary: "Thermal gate reached", detail: { code: 75 } });
  assert.deepEqual(store.listPolicyEvents({ ruleCode: "host_limits" }), [policyEvent]);
  assert.deepEqual(store.listPolicyEvents({ ruleCode: "two_failure_stop" }), []);

  const entry = store.recordFriction({ kind: "owner_intervention", summary: "First" });
  store.recordFriction({ kind: "owner_intervention", summary: "Second", status: "selected" });
  now = "2026-11-28T08:00:00+08:00";
  store.recordFriction({ kind: "owner_intervention", summary: "Third" });
  store.recordFriction({ kind: "correction", summary: "Clarified", ruleCode: "authority_routing" });
  assert.deepEqual(store.countOwnerInterventionsByDay(), [{ day: "2026-11-27", count: 2 }, { day: "2026-11-28", count: 1 }]);
  assert.deepEqual(store.countOwnerInterventionsByDay("2026-11-28T00:00Z"), [{ day: "2026-11-28", count: 1 }]);
  assert.equal(store.listFriction({ status: "open" }).at(-1)?.id, entry.id);
});

test("budgets are scoped by period and updates preserve their identity", (t) => {
  const store = memory(t);
  const input = { scopeKind: "team", period: "weekly", capUsd: 10, warnPct: 80, spendSource: "semon" } as const;
  const initial = store.upsertBudget(input);
  const updated = store.upsertBudget({ ...input, capUsd: 12, enabled: false });
  assert.deepEqual(updated, { ...initial, capUsd: 12, enabled: false });
  const lane = store.createLane({ slug: "ops", name: "Operations" });
  const other = store.upsertBudget({ ...input, scopeKind: "lane", scopeId: lane.id });
  assert.deepEqual(store.listBudgets(), [other, updated]);
  assert.throws(() => store.upsertBudget({ ...input, warnPct: 101 }), PlatformValidationError);
});

test("unpriced spend stays null and Semon session UIDs stay separate from runtime references", async (t) => {
  const store = memory(t);
  assert.deepEqual(await unavailableSpendSource().getSnapshot("session-1"), { status: "unavailable", snapshot: null });
  const agent = store.upsertAgent({
    id: "agent-1", role: "manager", connectorKind: "t3", hostId: "host-1", externalRef: "thread-1",
    label: "Manager", status: "active",
  });
  assert.equal(Object.getPrototypeOf(agent), Object.prototype);
  assert.deepEqual(store.listAgents(), [{ ...agent }]);
  const input = {
    id: "run-1", agentId: agent.id, connectorKind: "t3", hostId: "host-1", externalRunRef: "t3-run-1",
    status: "waiting_input", waitingDetail: { reason: "Decision pending" },
  } as const;
  const run = store.upsertRun(input);
  assert.equal(run.semonSessionUid, null);
  const linked = store.linkRunSemonSession(run.id, "semon-session-1");
  assert.equal(linked.externalRunRef, input.externalRunRef);
  assert.equal(linked.semonSessionUid, "semon-session-1");
  assert.equal(store.upsertRun({ ...input, status: "completed" }).semonSessionUid, linked.semonSessionUid);
  assert.equal(store.listRuns()[0]?.externalRunRef, input.externalRunRef);
  assert.equal(store.getSpendSnapshot(input.externalRunRef), null);

  const snapshot = store.upsertSpendSnapshot({ sessionUid: "semon-session-1", source: "semon", sourceReportedUsd: 0 });
  assert.equal(snapshot.usdEst, null);
  assert.equal(snapshot.sourceReportedUsd, 0);
  assert.equal(snapshot.billedUsd, null);
  assert.deepEqual(store.getSpendSnapshot(snapshot.sessionUid), snapshot);
  const updated = store.upsertSpendSnapshot({ sessionUid: snapshot.sessionUid, source: "semon", billedUsd: 0 });
  assert.equal(updated.sourceReportedUsd, null);
  assert.equal(updated.billedUsd, 0);
  assert.equal(store.upsertRun({ ...input, externalRunRef: "different-external-run" }).semonSessionUid, null);
});
