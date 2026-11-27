import { randomBytes } from "node:crypto";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { DatabaseSync, type SQLInputValue } from "node:sqlite";

import { migrate } from "./migrations.ts";
import {
  agentRoles, agentStatuses, budgetPeriods, budgetScopes, connectorKinds, decisionAuthorities,
  decisionCategories, decisionStatuses, decisionUrgencies, frictionKinds, frictionStatuses,
  laneStatuses, runStatuses, spendSources,
} from "./types.ts";
import type {
  Agent, Budget, Decision, DecisionEvent, DecisionStatus, DecisionVerification, FrictionEntry,
  FrictionStatus, Goal, JsonValue, Lane, LaneStatus, PolicyEvent, PolicyRule, Run, SpendSnapshot,
} from "./types.ts";

export class PlatformValidationError extends Error {}
export class PlatformNotFoundError extends Error {}

export class DecisionStateError extends Error {
  constructor(id: string, status: DecisionStatus, action: string) {
    super(`cannot ${action} decision ${id} with status ${status}`);
    this.name = "DecisionStateError";
  }
}

type OptionalFields<T, K extends keyof T> = Omit<T, K> & Partial<Pick<T, K>>;

export type CreateLaneInput = OptionalFields<Omit<Lane, "createdAt" | "updatedAt">, "id" | "status" | "ownerAgentId" | "priorityRank">;
export type SetGoalInput = OptionalFields<Omit<Goal, "updatedAt">, "id" | "metricUnit" | "metricCurrent" | "weekStart" | "weeklyPlan" | "approvedDecisionId">;
export type CreateDecisionInput = Pick<Decision, "authority" | "category" | "urgency" | "title" | "recommendation" | "requestedBy">
  & Partial<Pick<Decision, "id" | "laneId" | "evidence" | "options">>;
export type DecisionFilter = Partial<Pick<Decision, "status" | "authority" | "urgency" | "laneId">>;
export interface AnswerDecisionInput {
  status: "approved" | "rejected";
  decidedBy: string;
  answer: JsonValue;
  rationale?: string | null;
}
interface DecisionAction {
  actor: string;
  rationale?: string | null;
}
export interface SupersedeDecisionInput extends DecisionAction {
  supersededById: string;
}

export type RecordPolicyEventInput = OptionalFields<Omit<PolicyEvent, "createdAt">, "id" | "laneId" | "detail">;
export type RecordFrictionInput = OptionalFields<Omit<FrictionEntry, "createdAt">, "id" | "laneId" | "ruleCode" | "status">;
export type UpsertBudgetInput = OptionalFields<Omit<Budget, "updatedAt">, "id" | "scopeId" | "enabled">;
export type UpsertSpendSnapshotInput = Pick<SpendSnapshot, "sessionUid" | "source"> & Partial<Omit<SpendSnapshot, "sessionUid" | "source">>;
export type UpsertAgentInput = OptionalFields<Omit<Agent, "updatedAt">, "laneId" | "lastSeenAt">;
export type UpsertRunInput = OptionalFields<Omit<Run, "updatedAt">,
  "laneId" | "model" | "startedAt" | "completedAt" | "failureCount" | "waitingDetail" | "semonSessionUid">;

const laneColumns = `id, slug, name, status, owner_agent_id AS ownerAgentId,
  priority_rank AS priorityRank, created_at AS createdAt, updated_at AS updatedAt`;
const goalColumns = `id, lane_id AS laneId, statement, metric_name AS metricName,
  metric_unit AS metricUnit, metric_current AS metricCurrent, metric_target AS metricTarget,
  week_start AS weekStart, weekly_plan_json AS weeklyPlan, approved_decision_id AS approvedDecisionId,
  updated_at AS updatedAt`;
const decisionColumns = `id, lane_id AS laneId, authority, category, urgency, title,
  evidence_json AS evidence, recommendation, options_json AS options, status, requested_by AS requestedBy,
  decided_by AS decidedBy, answer_json AS answer, rationale, superseded_by_id AS supersededById,
  created_at AS createdAt, decided_at AS decidedAt`;
const policyEventColumns = `id, rule_code AS ruleCode, lane_id AS laneId, summary,
  detail_json AS detail, created_at AS createdAt`;
const frictionColumns = `id, kind, summary, status, lane_id AS laneId, rule_code AS ruleCode,
  created_at AS createdAt`;
const budgetColumns = `id, scope_kind AS scopeKind, scope_id AS scopeId, period,
  cap_usd AS capUsd, warn_pct AS warnPct, spend_source AS spendSource, enabled, updated_at AS updatedAt`;
const spendColumns = `session_uid AS sessionUid, source, input_tokens AS inputTokens,
  output_tokens AS outputTokens, cached_tokens AS cachedTokens, usd_est AS usdEst,
  source_reported_usd AS sourceReportedUsd, billed_usd AS billedUsd, coverage,
  snapshot_id AS snapshotId, as_of AS asOf, observed_at AS observedAt`;
const agentColumns = `id, role, lane_id AS laneId, connector_kind AS connectorKind,
  host_id AS hostId, external_ref AS externalRef, label, status, last_seen_at AS lastSeenAt,
  updated_at AS updatedAt`;
const runColumns = `id, agent_id AS agentId, lane_id AS laneId, connector_kind AS connectorKind,
  host_id AS hostId, external_run_ref AS externalRunRef, status, model, started_at AS startedAt,
  completed_at AS completedAt, failure_count AS failureCount, waiting_detail_json AS waitingDetail,
  semon_session_uid AS semonSessionUid, updated_at AS updatedAt`;

type JsonColumns<T, K extends keyof T> = Omit<T, K> & Record<K, string>;
type DecisionRow = JsonColumns<Decision, "evidence" | "options" | "answer">;
type GoalRow = JsonColumns<Goal, "weeklyPlan">;
type DecisionEventRow = JsonColumns<DecisionEvent, "detail">;
type PolicyRuleRow = JsonColumns<Omit<PolicyRule, "enabled"> & { enabled: number }, "params">;
type PolicyEventRow = JsonColumns<PolicyEvent, "detail">;
type BudgetRow = Omit<Budget, "enabled"> & { enabled: number };
type RunRow = JsonColumns<Run, "waitingDetail">;

function newId(prefix: string): string {
  return `${prefix}_${randomBytes(6).toString("hex")}`;
}

function nonEmpty(value: unknown, field: string): asserts value is string {
  if (typeof value !== "string" || value.trim() === "") {
    throw new PlatformValidationError(`${field} must be non-empty text`);
  }
}

function optionalText(value: unknown, field: string): void {
  if (value !== undefined && value !== null && typeof value !== "string") {
    throw new PlatformValidationError(`${field} must be text`);
  }
}

function choice(value: unknown, values: readonly string[], field: string): void {
  if (typeof value !== "string" || !values.includes(value)) {
    throw new PlatformValidationError(`${field} must be one of ${values.join(", ")}`);
  }
}

function number(value: unknown, field: string, { integer = false, min = -Infinity, max = Infinity } = {}): void {
  if (typeof value !== "number" || !Number.isFinite(value) || value < min || value > max
    || (integer && !Number.isSafeInteger(value))) {
    throw new PlatformValidationError(`invalid ${field}`);
  }
}

function optionalNumber(value: unknown, field: string, range?: { integer?: boolean; min?: number }): void {
  if (value !== null && value !== undefined) number(value, field, range);
}

function optionalBoolean(value: unknown, field: string): void {
  if (value !== undefined && typeof value !== "boolean") throw new PlatformValidationError(`${field} must be boolean`);
}

function encode(value: JsonValue, field: string): string {
  try {
    const encoded = JSON.stringify(value, (_key, item: unknown) => {
      if (item === undefined || (typeof item === "number" && !Number.isFinite(item))) {
        throw new Error("invalid JSON value");
      }
      return item;
    });
    if (encoded !== undefined) return encoded;
  } catch {
    // Both invalid JSON values and serialization failures are input errors.
  }
  throw new PlatformValidationError(`${field} must be JSON`);
}

function decode(value: string): JsonValue {
  return JSON.parse(value) as JsonValue;
}

function decisionFromRow(row: DecisionRow): Decision {
  return { ...row, evidence: decode(row.evidence), options: decode(row.options), answer: decode(row.answer) };
}

function goalFromRow(row: GoalRow): Goal {
  return { ...row, weeklyPlan: decode(row.weeklyPlan) };
}

function runFromRow(row: RunRow): Run {
  return { ...row, waitingDetail: decode(row.waitingDetail) };
}

function weekOf(iso: string): string {
  const date = new Date(iso);
  date.setUTCDate(date.getUTCDate() - ((date.getUTCDay() + 6) % 7));
  return date.toISOString().slice(0, 10);
}

export class PlatformStore {
  private readonly db: DatabaseSync;
  private readonly clock: () => Date | string;

  private constructor(db: DatabaseSync, clock: () => Date | string) {
    this.db = db;
    this.clock = clock;
  }

  static open(path: string, opts: { now?: () => Date | string } = {}): PlatformStore {
    if (path !== ":memory:") mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
    const db = new DatabaseSync(path);
    try {
      db.exec("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL");
      migrate(db);
      return new PlatformStore(db, opts.now ?? (() => new Date()));
    } catch (error) {
      db.close();
      throw error;
    }
  }

  close(): void {
    this.db.close();
  }

  private now(): string {
    return new Date(this.clock().valueOf()).toISOString();
  }

  // Every SELECT lists its columns explicitly. Only the checked schema is read into these row types.
  private get<T>(sql: string, ...args: SQLInputValue[]): T | null {
    return (this.db.prepare(sql).get(...args) as unknown as T | undefined) ?? null;
  }

  private all<T>(sql: string, ...args: SQLInputValue[]): T[] {
    return this.db.prepare(sql).all(...args) as unknown as T[];
  }

  private write<T>(sql: string, ...args: SQLInputValue[]): T {
    const row = this.get<T>(sql, ...args);
    if (row === null) throw new Error("platform write returned no row");
    return row;
  }

  private transaction<T>(operation: () => T): T {
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const result = operation();
      this.db.exec("COMMIT");
      return result;
    } catch (error) {
      this.db.exec("ROLLBACK");
      throw error;
    }
  }

  private validateLane(id: string | null | undefined): void {
    if (id == null) return;
    nonEmpty(id, "laneId");
    if (this.getLaneById(id) === null) throw new PlatformValidationError(`unknown lane ${id}`);
  }

  private appendDecisionEvent(decisionId: string, kind: DecisionEvent["kind"], actor: string, detail: JsonValue, time: string): void {
    this.db.prepare(`INSERT INTO decision_event (decision_id, kind, actor, detail_json, created_at)
      VALUES (?, ?, ?, ?, ?)`).run(decisionId, kind, actor, encode(detail, "detail"), time);
  }

  createDecision(input: CreateDecisionInput): Decision {
    nonEmpty(input.title, "title");
    nonEmpty(input.recommendation, "recommendation");
    nonEmpty(input.requestedBy, "requestedBy");
    choice(input.authority, decisionAuthorities, "authority");
    choice(input.category, decisionCategories, "category");
    choice(input.urgency, decisionUrgencies, "urgency");
    const id = input.id === undefined ? newId("dec") : input.id;
    nonEmpty(id, "id");
    const time = this.now();
    return this.transaction(() => {
      this.validateLane(input.laneId);
      const row = this.write<DecisionRow>(`INSERT INTO decision (
        id, lane_id, authority, category, urgency, title, evidence_json, recommendation,
        options_json, status, requested_by, answer_json, created_at
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 'null', ?) RETURNING ${decisionColumns}`,
      id, input.laneId ?? null, input.authority, input.category, input.urgency, input.title,
      encode(input.evidence ?? null, "evidence"), input.recommendation,
      encode(input.options === undefined ? [] : input.options, "options"), input.requestedBy, time);
      this.appendDecisionEvent(id, "created", input.requestedBy, null, time);
      return decisionFromRow(row);
    });
  }

  answerDecision(id: string, input: AnswerDecisionInput): Decision {
    choice(input.status, ["approved", "rejected"], "status");
    nonEmpty(input.decidedBy, "decidedBy");
    optionalText(input.rationale, "rationale");
    const answer = encode(input.answer, "answer");
    return this.transaction(() => {
      this.requireDecisionState(id, ["pending"], "answer");
      const time = this.now();
      const row = this.write<DecisionRow>(`UPDATE decision
        SET status = ?, decided_by = ?, answer_json = ?, rationale = ?, decided_at = ?
        WHERE id = ? RETURNING ${decisionColumns}`,
      input.status, input.decidedBy, answer, input.rationale ?? null, time, id);
      this.appendDecisionEvent(id, "answered", input.decidedBy, { status: input.status }, time);
      return decisionFromRow(row);
    });
  }

  supersedeDecision(id: string, input: SupersedeDecisionInput): Decision {
    nonEmpty(input.actor, "actor");
    nonEmpty(input.supersededById, "supersededById");
    optionalText(input.rationale, "rationale");
    if (input.supersededById === id) throw new PlatformValidationError("a decision cannot supersede itself");
    return this.transaction(() => {
      this.requireDecisionState(id, ["pending", "approved", "rejected"], "supersede");
      if (this.getDecision(input.supersededById) === null) {
        throw new PlatformValidationError(`unknown replacement decision ${input.supersededById}`);
      }
      const row = this.write<DecisionRow>(`UPDATE decision SET status = 'superseded', superseded_by_id = ?
        WHERE id = ? RETURNING ${decisionColumns}`, input.supersededById, id);
      this.appendDecisionEvent(id, "superseded", input.actor,
        { supersededById: input.supersededById, rationale: input.rationale ?? null }, this.now());
      return decisionFromRow(row);
    });
  }

  expireDecision(id: string, input: DecisionAction): Decision {
    nonEmpty(input.actor, "actor");
    optionalText(input.rationale, "rationale");
    return this.transaction(() => {
      this.requireDecisionState(id, ["pending"], "expire");
      const row = this.write<DecisionRow>(`UPDATE decision SET status = 'expired'
        WHERE id = ? RETURNING ${decisionColumns}`, id);
      this.appendDecisionEvent(id, "expired", input.actor, { rationale: input.rationale ?? null }, this.now());
      return decisionFromRow(row);
    });
  }

  private requireDecisionState(id: string, allowed: readonly DecisionStatus[], action: string): void {
    const decision = this.getDecision(id);
    if (decision === null) throw new PlatformNotFoundError(`decision ${id} not found`);
    if (!allowed.includes(decision.status)) throw new DecisionStateError(id, decision.status, action);
  }

  getDecision(id: string): Decision | null {
    const row = this.get<DecisionRow>(`SELECT ${decisionColumns} FROM decision WHERE id = ?`, id);
    return row === null ? null : decisionFromRow(row);
  }

  listDecisions(filter: DecisionFilter = {}): Decision[] {
    const { status, authority, urgency, laneId } = filter;
    if (status !== undefined) choice(status, decisionStatuses, "status");
    if (authority !== undefined) choice(authority, decisionAuthorities, "authority");
    if (urgency !== undefined) choice(urgency, decisionUrgencies, "urgency");
    optionalText(laneId, "laneId");
    return this.all<DecisionRow>(`SELECT ${decisionColumns} FROM decision
      WHERE status = coalesce(?, status) AND authority = coalesce(?, authority)
        AND urgency = coalesce(?, urgency) AND (? = 0 OR lane_id IS ?)
      ORDER BY created_at DESC, id`, status ?? null, authority ?? null, urgency ?? null,
    laneId === undefined ? 0 : 1, laneId ?? null).map(decisionFromRow);
  }

  listDecisionEvents(decisionId: string): DecisionEvent[] {
    return this.all<DecisionEventRow>(`SELECT id, decision_id AS decisionId, kind, actor,
      detail_json AS detail, created_at AS createdAt FROM decision_event WHERE decision_id = ? ORDER BY id`,
    decisionId).map((row) => ({ ...row, detail: decode(row.detail) }));
  }

  verifyDecision(id: string): DecisionVerification {
    const decision = this.getDecision(id);
    if (decision === null) return { valid: false, status: "not_found", reason: `decision ${id} not found`, decision };
    return {
      valid: decision.status === "approved",
      status: decision.status,
      reason: `decision ${id} is ${decision.status}`,
      decision,
    };
  }

  createLane(input: CreateLaneInput): Lane {
    nonEmpty(input.slug, "slug");
    if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(input.slug)) {
      throw new PlatformValidationError("slug must use lowercase letters, numbers, and single hyphens");
    }
    nonEmpty(input.name, "name");
    const id = input.id === undefined ? newId("lane") : input.id;
    const status = input.status === undefined ? "active" : input.status;
    const priority = input.priorityRank === undefined ? 0 : input.priorityRank;
    nonEmpty(id, "id");
    choice(status, laneStatuses, "status");
    number(priority, "priorityRank", { integer: true });
    optionalText(input.ownerAgentId, "ownerAgentId");
    const time = this.now();
    return this.write<Lane>(`INSERT INTO lane (id, slug, name, status, owner_agent_id, priority_rank, created_at, updated_at)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING ${laneColumns}`,
    id, input.slug, input.name, status, input.ownerAgentId ?? null, priority, time, time);
  }

  getLaneBySlug(slug: string): Lane | null {
    return this.get<Lane>(`SELECT ${laneColumns} FROM lane WHERE slug = ?`, slug);
  }

  getLaneById(id: string): Lane | null {
    return this.get<Lane>(`SELECT ${laneColumns} FROM lane WHERE id = ?`, id);
  }

  listLanes(): Lane[] {
    return this.all<Lane>(`SELECT ${laneColumns} FROM lane ORDER BY priority_rank, slug`);
  }

  updateLaneStatus(id: string, status: LaneStatus): Lane {
    choice(status, laneStatuses, "status");
    const row = this.get<Lane>(`UPDATE lane SET status = ?, updated_at = ?
      WHERE id = ? RETURNING ${laneColumns}`, status, this.now(), id);
    if (row === null) throw new PlatformNotFoundError(`lane ${id} not found`);
    return row;
  }

  setGoal(input: SetGoalInput): Goal {
    nonEmpty(input.laneId, "laneId");
    nonEmpty(input.statement, "statement");
    nonEmpty(input.metricName, "metricName");
    number(input.metricTarget, "metricTarget");
    optionalNumber(input.metricCurrent, "metricCurrent");
    optionalText(input.metricUnit, "metricUnit");
    const id = input.id === undefined ? newId("goal") : input.id;
    nonEmpty(id, "id");
    const time = this.now();
    const week = input.weekStart === undefined ? weekOf(time) : input.weekStart;
    if (typeof week !== "string" || !/^\d{4}-\d{2}-\d{2}$/.test(week)) {
      throw new PlatformValidationError("weekStart must be a YYYY-MM-DD date");
    }
    const date = new Date(week);
    if (!Number.isFinite(date.getTime()) || date.toISOString().slice(0, 10) !== week) {
      throw new PlatformValidationError("weekStart must be a valid date");
    }
    return this.transaction(() => {
      this.validateLane(input.laneId);
      if (input.approvedDecisionId != null) {
        nonEmpty(input.approvedDecisionId, "approvedDecisionId");
        if (this.getDecision(input.approvedDecisionId)?.status !== "approved") {
          throw new PlatformValidationError(`decision ${input.approvedDecisionId} must be approved`);
        }
      }
      const row = this.write<GoalRow>(`INSERT INTO goal (id, lane_id, statement, metric_name, metric_unit,
        metric_current, metric_target, week_start, weekly_plan_json, approved_decision_id, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (lane_id, week_start) DO UPDATE SET statement = excluded.statement,
          metric_name = excluded.metric_name, metric_unit = excluded.metric_unit,
          metric_current = excluded.metric_current, metric_target = excluded.metric_target,
          weekly_plan_json = excluded.weekly_plan_json, approved_decision_id = excluded.approved_decision_id,
          updated_at = excluded.updated_at
        RETURNING ${goalColumns}`, id, input.laneId, input.statement, input.metricName, input.metricUnit ?? null,
      input.metricCurrent ?? null, input.metricTarget, week, encode(input.weeklyPlan ?? null, "weeklyPlan"),
      input.approvedDecisionId ?? null, time);
      return goalFromRow(row);
    });
  }

  getLatestGoal(laneId: string): Goal | null {
    const row = this.get<GoalRow>(`SELECT ${goalColumns} FROM goal WHERE lane_id = ?
      ORDER BY week_start DESC LIMIT 1`, laneId);
    return row === null ? null : goalFromRow(row);
  }

  listGoals(laneId?: string): Goal[] {
    return this.all<GoalRow>(`SELECT ${goalColumns} FROM goal WHERE lane_id = coalesce(?, lane_id)
      ORDER BY week_start DESC, lane_id`, laneId ?? null).map(goalFromRow);
  }

  listPolicyRules(): PolicyRule[] {
    return this.all<PolicyRuleRow>(`SELECT id, code, source_ref AS sourceRef, mode,
      params_json AS params, enabled, updated_at AS updatedAt FROM policy_rule ORDER BY code`)
      .map((row) => ({ ...row, params: decode(row.params), enabled: row.enabled === 1 }));
  }

  recordPolicyEvent(input: RecordPolicyEventInput): PolicyEvent {
    const id = input.id === undefined ? newId("pol") : input.id;
    nonEmpty(id, "id");
    nonEmpty(input.ruleCode, "ruleCode");
    nonEmpty(input.summary, "summary");
    this.validateLane(input.laneId);
    const row = this.write<PolicyEventRow>(`INSERT INTO policy_event (id, rule_code, lane_id, summary, detail_json, created_at)
      VALUES (?, ?, ?, ?, ?, ?) RETURNING ${policyEventColumns}`,
    id, input.ruleCode, input.laneId ?? null, input.summary, encode(input.detail ?? null, "detail"), this.now());
    return { ...row, detail: decode(row.detail) };
  }

  listPolicyEvents(filter: { ruleCode?: string; laneId?: string } = {}): PolicyEvent[] {
    return this.all<PolicyEventRow>(`SELECT ${policyEventColumns} FROM policy_event
      WHERE rule_code = coalesce(?, rule_code) AND (? IS NULL OR lane_id = ?)
      ORDER BY created_at DESC, rowid DESC`, filter.ruleCode ?? null, filter.laneId ?? null, filter.laneId ?? null)
      .map((row) => ({ ...row, detail: decode(row.detail) }));
  }

  recordFriction(input: RecordFrictionInput): FrictionEntry {
    const id = input.id === undefined ? newId("fric") : input.id;
    const status = input.status === undefined ? "open" : input.status;
    nonEmpty(id, "id");
    nonEmpty(input.summary, "summary");
    choice(input.kind, frictionKinds, "kind");
    choice(status, frictionStatuses, "status");
    optionalText(input.ruleCode, "ruleCode");
    this.validateLane(input.laneId);
    return this.write<FrictionEntry>(`INSERT INTO friction_entry (id, kind, summary, status, lane_id, rule_code, created_at)
      VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING ${frictionColumns}`,
    id, input.kind, input.summary, status, input.laneId ?? null, input.ruleCode ?? null, this.now());
  }

  listFriction(filter: { status?: FrictionStatus } = {}): FrictionEntry[] {
    if (filter.status !== undefined) choice(filter.status, frictionStatuses, "status");
    return this.all<FrictionEntry>(`SELECT ${frictionColumns} FROM friction_entry
      WHERE status = coalesce(?, status) ORDER BY created_at DESC, rowid DESC`, filter.status ?? null);
  }

  countOwnerInterventionsByDay(sinceIso?: string): { day: string; count: number }[] {
    const since = sinceIso === undefined ? null : new Date(sinceIso);
    if (since !== null && !Number.isFinite(since.getTime())) throw new PlatformValidationError("invalid sinceIso");
    return this.all<{ day: string; count: number }>(`SELECT substr(created_at, 1, 10) AS day, count(*) AS count
      FROM friction_entry WHERE kind = 'owner_intervention' AND (? IS NULL OR created_at >= ?)
      GROUP BY day ORDER BY day`, since?.toISOString() ?? null, since?.toISOString() ?? null);
  }

  upsertBudget(input: UpsertBudgetInput): Budget {
    const id = input.id === undefined ? newId("budget") : input.id;
    nonEmpty(id, "id");
    choice(input.scopeKind, budgetScopes, "scopeKind");
    choice(input.period, budgetPeriods, "period");
    choice(input.spendSource, spendSources, "spendSource");
    number(input.capUsd, "capUsd", { min: 0 });
    number(input.warnPct, "warnPct", { min: 0, max: 100 });
    optionalBoolean(input.enabled, "enabled");
    if (input.scopeKind === "team") {
      if (input.scopeId != null) throw new PlatformValidationError("team budgets do not accept scopeId");
    } else {
      nonEmpty(input.scopeId, "scopeId");
      if (input.scopeKind === "lane") this.validateLane(input.scopeId);
      else if (this.get<{ id: string }>("SELECT id FROM agent WHERE id = ?", input.scopeId) === null) {
        throw new PlatformValidationError(`unknown agent ${input.scopeId}`);
      }
    }
    const row = this.write<BudgetRow>(`INSERT INTO budget (id, scope_kind, scope_id, period, cap_usd, warn_pct, spend_source, enabled, updated_at)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (scope_kind, coalesce(scope_id, ''), period) DO UPDATE SET cap_usd = excluded.cap_usd,
        warn_pct = excluded.warn_pct, spend_source = excluded.spend_source, enabled = excluded.enabled,
        updated_at = excluded.updated_at
      RETURNING ${budgetColumns}`,
    id, input.scopeKind, input.scopeId ?? null, input.period, input.capUsd, input.warnPct,
    input.spendSource, input.enabled === false ? 0 : 1, this.now());
    return { ...row, enabled: row.enabled === 1 };
  }

  listBudgets(): Budget[] {
    return this.all<BudgetRow>(`SELECT ${budgetColumns} FROM budget ORDER BY scope_kind, scope_id, period`)
      .map((row) => ({ ...row, enabled: row.enabled === 1 }));
  }

  upsertSpendSnapshot(input: UpsertSpendSnapshotInput): SpendSnapshot {
    nonEmpty(input.sessionUid, "sessionUid");
    choice(input.source, spendSources, "source");
    optionalNumber(input.inputTokens, "inputTokens", { integer: true, min: 0 });
    optionalNumber(input.outputTokens, "outputTokens", { integer: true, min: 0 });
    optionalNumber(input.cachedTokens, "cachedTokens", { integer: true, min: 0 });
    optionalNumber(input.usdEst, "usdEst");
    optionalNumber(input.sourceReportedUsd, "sourceReportedUsd");
    optionalNumber(input.billedUsd, "billedUsd");
    optionalText(input.coverage, "coverage");
    optionalText(input.snapshotId, "snapshotId");
    optionalText(input.asOf, "asOf");
    optionalText(input.observedAt, "observedAt");
    return this.write<SpendSnapshot>(`INSERT INTO spend_snapshot (
      session_uid, source, input_tokens, output_tokens, cached_tokens, usd_est, source_reported_usd, billed_usd,
      coverage, snapshot_id, as_of, observed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (session_uid) DO UPDATE SET source = excluded.source, input_tokens = excluded.input_tokens,
        output_tokens = excluded.output_tokens, cached_tokens = excluded.cached_tokens, usd_est = excluded.usd_est,
        source_reported_usd = excluded.source_reported_usd, billed_usd = excluded.billed_usd,
        coverage = excluded.coverage, snapshot_id = excluded.snapshot_id, as_of = excluded.as_of, observed_at = excluded.observed_at
      RETURNING ${spendColumns}`, input.sessionUid, input.source, input.inputTokens ?? null, input.outputTokens ?? null,
    input.cachedTokens ?? null, input.usdEst ?? null, input.sourceReportedUsd ?? null, input.billedUsd ?? null,
    input.coverage ?? null, input.snapshotId ?? null, input.asOf ?? null, input.observedAt ?? this.now());
  }

  getSpendSnapshot(sessionUid: string): SpendSnapshot | null {
    return this.get<SpendSnapshot>(`SELECT ${spendColumns} FROM spend_snapshot WHERE session_uid = ?`, sessionUid);
  }

  upsertAgent(input: UpsertAgentInput): Agent {
    nonEmpty(input.id, "id");
    nonEmpty(input.hostId, "hostId");
    nonEmpty(input.externalRef, "externalRef");
    nonEmpty(input.label, "label");
    choice(input.role, agentRoles, "role");
    choice(input.status, agentStatuses, "status");
    choice(input.connectorKind, connectorKinds, "connectorKind");
    optionalText(input.lastSeenAt, "lastSeenAt");
    this.validateLane(input.laneId);
    return this.write<Agent>(`INSERT INTO agent (id, role, lane_id, connector_kind, host_id, external_ref, label, status,
      last_seen_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (id) DO UPDATE SET role = excluded.role, lane_id = excluded.lane_id,
        connector_kind = excluded.connector_kind, host_id = excluded.host_id, external_ref = excluded.external_ref,
        label = excluded.label, status = excluded.status, last_seen_at = excluded.last_seen_at, updated_at = excluded.updated_at
      RETURNING ${agentColumns}`, input.id, input.role, input.laneId ?? null, input.connectorKind, input.hostId,
    input.externalRef, input.label, input.status, input.lastSeenAt ?? null, this.now());
  }

  listAgents(): Agent[] {
    return this.all<Agent>(`SELECT ${agentColumns} FROM agent ORDER BY id`);
  }

  upsertRun(input: UpsertRunInput): Run {
    nonEmpty(input.id, "id");
    nonEmpty(input.agentId, "agentId");
    nonEmpty(input.hostId, "hostId");
    nonEmpty(input.externalRunRef, "externalRunRef");
    choice(input.connectorKind, connectorKinds, "connectorKind");
    choice(input.status, runStatuses, "status");
    optionalText(input.model, "model");
    optionalText(input.startedAt, "startedAt");
    optionalText(input.completedAt, "completedAt");
    if (input.semonSessionUid != null) nonEmpty(input.semonSessionUid, "semonSessionUid");
    const failures = input.failureCount === undefined ? 0 : input.failureCount;
    number(failures, "failureCount", { integer: true, min: 0 });
    this.validateLane(input.laneId);
    const row = this.write<RunRow>(`INSERT INTO run (id, agent_id, lane_id, connector_kind, host_id, external_run_ref,
      status, model, started_at, completed_at, failure_count, waiting_detail_json, semon_session_uid, updated_at)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (id) DO UPDATE SET agent_id = excluded.agent_id, lane_id = excluded.lane_id,
        connector_kind = excluded.connector_kind, host_id = excluded.host_id, external_run_ref = excluded.external_run_ref,
        status = excluded.status, model = excluded.model, started_at = excluded.started_at,
        completed_at = excluded.completed_at, failure_count = excluded.failure_count,
        waiting_detail_json = excluded.waiting_detail_json, updated_at = excluded.updated_at,
        semon_session_uid = CASE
          WHEN ? THEN excluded.semon_session_uid
          WHEN run.connector_kind = excluded.connector_kind AND run.host_id = excluded.host_id
            AND run.external_run_ref = excluded.external_run_ref THEN run.semon_session_uid
        END
      RETURNING ${runColumns}`, input.id, input.agentId, input.laneId ?? null, input.connectorKind, input.hostId,
    input.externalRunRef, input.status, input.model ?? null, input.startedAt ?? null, input.completedAt ?? null,
    failures, encode(input.waitingDetail ?? null, "waitingDetail"), input.semonSessionUid ?? null,
    this.now(), input.semonSessionUid === undefined ? 0 : 1);
    return runFromRow(row);
  }

  listRuns(): Run[] {
    return this.all<RunRow>(`SELECT ${runColumns} FROM run ORDER BY id`).map(runFromRow);
  }

  // The caller supplies the exact Semon UID; an external run reference is never used to infer it.
  linkRunSemonSession(runId: string, sessionUid: string): Run {
    nonEmpty(sessionUid, "sessionUid");
    const row = this.get<RunRow>(`UPDATE run SET semon_session_uid = ?, updated_at = ?
      WHERE id = ? RETURNING ${runColumns}`, sessionUid, this.now(), runId);
    if (row === null) throw new PlatformNotFoundError(`run ${runId} not found`);
    return runFromRow(row);
  }
}
