import { randomBytes } from "node:crypto";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { isDeepStrictEqual } from "node:util";

import { migrate } from "./migrations.ts";
import {
  agentRoles, agentStatuses, budgetPeriods, budgetScopes, connectorKinds, decisionAuthorities,
  decisionCategories, decisionStatuses, decisionUrgencies, frictionKinds, frictionStatuses,
  isSemonKey, laneStatuses, runStatuses, spendSources, spendWindow,
} from "./types.ts";
import type {
  Agent, Budget, Decision, DecisionEvent, DecisionStatus, DecisionVerification, FrictionEntry,
  FrictionStatus, Goal, JsonValue, Lane, LaneStatus, PolicyEvent, PolicyRule, Run, SpendRollup, SpendSnapshot,
  GitHubPrCheck, GitHubPrObservation, SemonSessionIdentity, SpendSummaryFilter, T3HostState, T3PendingWait, T3ThreadPr,
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
export type UpdateLaneInput = Partial<Pick<Lane, "name" | "status" | "ownerAgentId" | "priorityRank">>;
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
  "laneId" | "model" | "startedAt" | "completedAt" | "failureCount" | "waitingDetail" | "semonSessionUid" | "semonIdentity">;

export type T3HostUpdate = Pick<T3HostState, "hostId" | "label" | "mcpUrl" | "originNamespace" | "connectionState">
  & Partial<Omit<T3HostState, "hostId" | "label" | "mcpUrl" | "originNamespace" | "connectionState" | "updatedAt">>;
export type T3WaitInput = Omit<T3PendingWait, "resolvedAt" | "resolution">;
export type T3PrInput = Omit<T3ThreadPr, "lastWakeOutcome" | "lastWakeSummary" | "lastWakeAt">;
export type T3PrWake = Pick<T3ThreadPr, "hostId" | "threadId" | "prHost" | "repository" | "number"> & {
  outcome: string;
  summary: string;
  occurredAt: string;
  watchEnded: boolean;
};
export interface T3ThreadSnapshot {
  threadId: string;
  agent: UpsertAgentInput;
  runs: Omit<UpsertRunInput, "semonSessionUid">[];
  waits?: T3WaitInput[];
  prs?: T3PrInput[];
}

const t3HostColumns = `host_id AS hostId, label, mcp_url AS mcpUrl, web_url AS webUrl, origin_namespace AS originNamespace,
  environment_id AS environmentId, server_version AS serverVersion, token_expires_at AS tokenExpiresAt,
  connection_state AS connectionState, last_reconciled_at AS lastReconciledAt, last_event_at AS lastEventAt,
  last_error AS lastError, consecutive_failures AS consecutiveFailures,
  partial_coverage AS partialCoverage,
  updated_at AS updatedAt`;
type T3HostRow = Omit<T3HostState, "partialCoverage"> & { partialCoverage: number };
const t3WaitColumns = `host_id AS hostId, thread_id AS threadId, request_id AS requestId, kind,
  questions_json AS questions, occurred_at AS occurredAt, resolved_at AS resolvedAt, resolution`;
const t3PrColumns = `host_id AS hostId, thread_id AS threadId, pr_host AS prHost, repository, number,
  url, source, state, watching, title, head_branch AS headBranch, base_branch AS baseBranch, is_draft AS isDraft,
  last_wake_outcome AS lastWakeOutcome, last_wake_summary AS lastWakeSummary, last_wake_at AS lastWakeAt`;

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
const spendRollupColumns = `scope_kind AS scopeKind, nullif(scope_id, '') AS scopeId, period, since, until, source,
  session_count AS sessionCount, unpriced_count AS unpricedCount, incomplete_count AS incompleteCount,
  usd_est AS usdEst, source_reported_usd AS sourceReportedUsd, billed_usd AS billedUsd, preferred_usd AS preferredUsd,
  coverage, snapshot_id AS snapshotId, as_of AS asOf, observed_at AS observedAt`;
const agentColumns = `id, role, lane_id AS laneId, connector_kind AS connectorKind,
  host_id AS hostId, external_ref AS externalRef, label, status, last_seen_at AS lastSeenAt,
  updated_at AS updatedAt`;
const runColumns = `id, agent_id AS agentId, lane_id AS laneId, connector_kind AS connectorKind,
  host_id AS hostId, external_run_ref AS externalRunRef, status, model, started_at AS startedAt,
  completed_at AS completedAt, failure_count AS failureCount, waiting_detail_json AS waitingDetail,
  semon_session_uid AS semonSessionUid, semon_identity_json AS semonIdentity, updated_at AS updatedAt`;

type JsonColumns<T, K extends keyof T> = Omit<T, K> & Record<K, string>;
type DecisionRow = JsonColumns<Decision, "evidence" | "options" | "answer">;
type GoalRow = JsonColumns<Goal, "weeklyPlan">;
type DecisionEventRow = JsonColumns<DecisionEvent, "detail">;
type PolicyRuleRow = JsonColumns<Omit<PolicyRule, "enabled"> & { enabled: number }, "params">;
type PolicyEventRow = JsonColumns<PolicyEvent, "detail">;
type BudgetRow = Omit<Budget, "enabled"> & { enabled: number };
type RunRow = JsonColumns<Run, "waitingDetail" | "semonIdentity">;

function newId(prefix: string): string {
  return `${prefix}_${randomBytes(6).toString("hex")}`;
}

function nonEmpty(value: unknown, field: string): asserts value is string {
  if (typeof value !== "string" || value.trim() === "") {
    throw new PlatformValidationError(`${field} must be non-empty text`);
  }
}

function pathIdentifier(value: unknown, field: string): void {
  // Starting with a letter or number excludes dot segments. The remaining characters exclude separators.
  if (typeof value !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(value)) {
    throw new PlatformValidationError(`invalid ${field}: expected 1–128 letters, numbers, dots, underscores or hyphens, starting with a letter or number`);
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

function isoTimestamp(value: string, field: string): string {
  if (typeof value !== "string"
    || !/^\d{4}-\d{2}-\d{2}T(?:[01]\d|2[0-3]):[0-5]\d:[0-5]\d(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/.test(value)
    || new Date(`${value.slice(0, 10)}T00:00:00Z`).toJSON()?.slice(0, 10) !== value.slice(0, 10)) {
    throw new PlatformValidationError(`invalid ${field}: expected an ISO timestamp`);
  }
  const normalized = new Date(value).toJSON();
  if (normalized === null) throw new PlatformValidationError(`invalid ${field}: expected an ISO timestamp`);
  return normalized;
}

function validateSpendFilter({ scopeKind, scopeId, period, source }: SpendSummaryFilter): void {
  choice(scopeKind, budgetScopes, "scopeKind");
  choice(period, budgetPeriods, "period");
  choice(source, spendSources, "source");
  if (scopeKind !== "team") nonEmpty(scopeId, "scopeId");
  else if (scopeId !== null) throw new PlatformValidationError("team spend does not accept scopeId");
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
  return { ...row, waitingDetail: decode(row.waitingDetail), semonIdentity: JSON.parse(row.semonIdentity) as Run["semonIdentity"] };
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

  // Every query lists its columns explicitly. Copy SQLite's null-prototype rows at this boundary.
  private get<T>(sql: string, ...args: SQLInputValue[]): T | null {
    const row = this.db.prepare(sql).get(...args);
    return row === undefined ? null : { ...row } as unknown as T;
  }

  private all<T>(sql: string, ...args: SQLInputValue[]): T[] {
    return this.db.prepare(sql).all(...args).map((row) => ({ ...row }) as unknown as T);
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
    pathIdentifier(id, "decision ID");
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

  // M1 trusts the operator/Chief PM OS account with the private write socket (0700/0600)
  // and direct SQLite access. decidedBy is an operator attestation. Derive caller identity
  // before allowing Manager writes from other principals.
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
    pathIdentifier(input.supersededById, "replacement decision ID");
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
    pathIdentifier(id, "decision ID");
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
    pathIdentifier(decisionId, "decision ID");
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
    pathIdentifier(input.slug, "lane slug");
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
    pathIdentifier(slug, "lane slug");
    return this.get<Lane>(`SELECT ${laneColumns} FROM lane WHERE slug = ?`, slug);
  }

  updateLane(slug: string, { name, status, ownerAgentId, priorityRank }: UpdateLaneInput): Lane {
    pathIdentifier(slug, "lane slug");
    if (name !== undefined) nonEmpty(name, "name");
    if (status !== undefined) choice(status, laneStatuses, "status");
    if (ownerAgentId !== undefined && ownerAgentId !== null) nonEmpty(ownerAgentId, "ownerAgentId");
    if (priorityRank !== undefined) number(priorityRank, "priorityRank", { integer: true });
    const row = this.get<Lane>(`UPDATE lane SET name = coalesce(?, name), status = coalesce(?, status),
      owner_agent_id = CASE WHEN ? THEN ? ELSE owner_agent_id END, priority_rank = coalesce(?, priority_rank),
      updated_at = ? WHERE slug = ? RETURNING ${laneColumns}`,
    name ?? null, status ?? null, Number(ownerAgentId !== undefined), ownerAgentId ?? null, priorityRank ?? null,
    this.now(), slug);
    if (row === null) throw new PlatformNotFoundError(`lane ${slug} not found`);
    return row;
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
    const weeklyPlan = encode(input.weeklyPlan ?? null, "weeklyPlan");
    const metricUnit = input.metricUnit ?? null;
    const approvedDecisionId = input.approvedDecisionId ?? null;
    return this.transaction(() => {
      this.validateLane(input.laneId);
      if (approvedDecisionId !== null) {
        const decision = this.getDecision(approvedDecisionId);
        if (decision?.status !== "approved") {
          throw new PlatformValidationError(`decision ${approvedDecisionId} must be approved`);
        }
        if (decision.category !== "weekly_plan" || (decision.laneId !== null && decision.laneId !== input.laneId)) {
          throw new PlatformValidationError(`decision ${approvedDecisionId} must approve a weekly plan for this lane or the team`);
        }
        const previous = this.get<GoalRow>(`SELECT ${goalColumns} FROM goal WHERE lane_id = ? AND week_start = ?`, input.laneId, week);
        if (previous?.approvedDecisionId === approvedDecisionId
          && (previous.statement !== input.statement || previous.metricName !== input.metricName
            || previous.metricUnit !== metricUnit || previous.metricTarget !== input.metricTarget
            || !isDeepStrictEqual(decode(previous.weeklyPlan), decode(weeklyPlan)))) {
          throw new PlatformValidationError("a changed weekly plan requires a new approval");
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
        RETURNING ${goalColumns}`, id, input.laneId, input.statement, input.metricName, metricUnit,
      input.metricCurrent ?? null, input.metricTarget, week, weeklyPlan, approvedDecisionId, time);
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
        coverage = excluded.coverage, snapshot_id = excluded.snapshot_id, as_of = excluded.as_of, observed_at = excluded.observed_at, refresh_failed = 0
      RETURNING ${spendColumns}`, input.sessionUid, input.source, input.inputTokens ?? null, input.outputTokens ?? null,
    input.cachedTokens ?? null, input.usdEst ?? null, input.sourceReportedUsd ?? null, input.billedUsd ?? null,
    input.coverage ?? null, input.snapshotId ?? null, input.asOf ?? null, input.observedAt ?? this.now());
  }

  getSpendSnapshot(sessionUid: string): SpendSnapshot | null {
    return this.get<SpendSnapshot>(`SELECT ${spendColumns} FROM spend_snapshot WHERE session_uid = ?`, sessionUid);
  }

  upsertSpendRollup(input: SpendRollup): SpendRollup {
    validateSpendFilter(input);
    const { scopeKind, scopeId, period, source, sessionCount, unpricedCount, incompleteCount,
      usdEst, sourceReportedUsd, billedUsd, preferredUsd, coverage, snapshotId, asOf } = input;
    const since = isoTimestamp(input.since, "since");
    const until = isoTimestamp(input.until, "until");
    const observedAt = isoTimestamp(input.observedAt, "observedAt");
    if (Date.parse(since) > Date.parse(until)) throw new PlatformValidationError("since must not be after until");
    number(sessionCount, "sessionCount", { integer: true, min: 0 });
    number(unpricedCount, "unpricedCount", { integer: true, min: 0 });
    number(incompleteCount, "incompleteCount", { integer: true, min: 0 });
    if (usdEst !== null) number(usdEst, "usdEst", { min: 0 });
    if (sourceReportedUsd !== null) number(sourceReportedUsd, "sourceReportedUsd", { min: 0 });
    if (billedUsd !== null) number(billedUsd, "billedUsd", { min: 0 });
    optionalNumber(preferredUsd, "preferredUsd", { min: 0 });
    optionalText(coverage, "coverage");
    optionalText(snapshotId, "snapshotId");
    optionalText(asOf, "asOf");
    return this.write<SpendRollup>(`INSERT INTO spend_rollup (scope_kind, scope_id, period, since, until, source,
      session_count, unpriced_count, incomplete_count, usd_est, source_reported_usd, billed_usd, preferred_usd,
      coverage, snapshot_id, as_of, observed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (scope_kind, scope_id, period, since, source) DO UPDATE SET until = excluded.until,
        session_count = excluded.session_count, unpriced_count = excluded.unpriced_count,
        incomplete_count = excluded.incomplete_count, usd_est = excluded.usd_est, source_reported_usd = excluded.source_reported_usd,
        billed_usd = excluded.billed_usd, preferred_usd = excluded.preferred_usd, coverage = excluded.coverage,
        snapshot_id = excluded.snapshot_id, as_of = excluded.as_of, observed_at = excluded.observed_at
      RETURNING ${spendRollupColumns}`, scopeKind, scopeId ?? "", period, since, until, source, sessionCount, unpricedCount,
    incompleteCount, usdEst, sourceReportedUsd, billedUsd, preferredUsd ?? null, coverage ?? null, snapshotId ?? null,
    asOf ?? null, observedAt);
  }

  upsertSpendRollups(inputs: readonly SpendRollup[]): SpendRollup[] {
    return this.transaction(() => inputs.map((input) => this.upsertSpendRollup(input)));
  }

  getSpendRollup(filter: SpendSummaryFilter, since: string): SpendRollup | null {
    validateSpendFilter(filter);
    return this.get<SpendRollup>(`SELECT ${spendRollupColumns} FROM spend_rollup
      WHERE scope_kind = ? AND scope_id = ? AND period = ? AND since = ? AND source = ?`,
    filter.scopeKind, filter.scopeId ?? "", filter.period, isoTimestamp(since, "since"), filter.source);
  }

  markSemonRefreshFailed(sessionUids: readonly string[]): void {
    this.db.prepare(`UPDATE spend_snapshot SET refresh_failed = 1
      WHERE source = 'semon' AND session_uid IN (SELECT value FROM json_each(?))`)
      .run(JSON.stringify(sessionUids));
  }

  summarizeSpend(filter: SpendSummaryFilter, now: Date | string) {
    const { scopeKind, scopeId, period, source } = filter;
    validateSpendFilter(filter);
    const { since, until } = spendWindow(period, now);

    return { ...filter, since, until, ...this.write<{
      sessionCount: number; snapshotCount: number; unpricedCount: number; incompleteCount: number;
      unlinkedRunCount: number; missingSnapshotCount: number; staleSnapshotCount: number; incompleteRunCount: number;
      usdEst: number | null; sourceReportedUsd: number | null; billedUsd: number | null;
      preferredUsd: number | null; estimatedCount: number; reportedCount: number; billedCount: number;
      coverage: string | null;
    }>(`WITH scoped_runs AS (
      SELECT semon_session_uid AS uid, julianday(started_at) AS started_at
      FROM run WHERE (? = 'team' OR (? = 'lane' AND lane_id = ?) OR (? = 'agent' AND agent_id = ?))
    ), sessions AS (
      SELECT uid, min(started_at) AS started_at FROM scoped_runs WHERE uid IS NOT NULL GROUP BY uid
    ), run_coverage AS (
      SELECT count(*) FILTER (WHERE r.uid IS NULL) AS unlinked,
        count(*) FILTER (WHERE r.uid IS NOT NULL AND snapshot.session_uid IS NULL) AS missing,
        count(DISTINCT r.uid) FILTER (WHERE snapshot.refresh_failed = 1) AS stale,
        count(*) FILTER (WHERE snapshot.coverage IS NOT 'complete' OR snapshot.refresh_failed = 1
          OR coalesce(snapshot.billed_usd, snapshot.source_reported_usd, snapshot.usd_est) IS NULL) AS incomplete
      FROM scoped_runs r LEFT JOIN spend_snapshot snapshot ON snapshot.session_uid = r.uid AND snapshot.source = ?
      WHERE r.started_at BETWEEN julianday(?) AND julianday(?)
    )
    SELECT count(*) AS sessionCount, count(s.session_uid) AS snapshotCount,
      (SELECT unlinked FROM run_coverage) AS unlinkedRunCount,
      (SELECT missing FROM run_coverage) AS missingSnapshotCount,
      (SELECT stale FROM run_coverage) AS staleSnapshotCount,
      (SELECT incomplete FROM run_coverage) AS incompleteRunCount,
      count(*) FILTER (WHERE coalesce(billed_usd, source_reported_usd, usd_est) IS NULL) AS unpricedCount,
      count(*) FILTER (WHERE s.coverage IS NOT 'complete' OR s.refresh_failed = 1) AS incompleteCount,
      sum(usd_est) AS usdEst, sum(source_reported_usd) AS sourceReportedUsd, sum(billed_usd) AS billedUsd,
      sum(coalesce(billed_usd, source_reported_usd, usd_est)) AS preferredUsd,
      count(*) FILTER (WHERE billed_usd IS NULL AND source_reported_usd IS NULL AND usd_est IS NOT NULL) AS estimatedCount,
      count(*) FILTER (WHERE billed_usd IS NULL AND source_reported_usd IS NOT NULL) AS reportedCount,
      count(billed_usd) AS billedCount, group_concat(DISTINCT s.coverage) AS coverage
    FROM sessions LEFT JOIN spend_snapshot s ON s.session_uid = sessions.uid AND s.source = ?
    WHERE sessions.started_at >= julianday(?) AND sessions.started_at <= julianday(?)`,
    scopeKind, scopeKind, scopeId, scopeKind, scopeId, source, since, until, source, since, until) };
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
    const identity = input.semonIdentity;
    if (identity != null && (!isSemonKey(identity.originNamespace) || !isSemonKey(identity.nativeHarness)
      || !isSemonKey(identity.nativeSessionId))) throw new PlatformValidationError("invalid Semon session identity");
    const identityJson = identity == null ? "null" : JSON.stringify({
      originNamespace: identity.originNamespace, nativeHarness: identity.nativeHarness, nativeSessionId: identity.nativeSessionId,
    });
    const failures = input.failureCount === undefined ? 0 : input.failureCount;
    number(failures, "failureCount", { integer: true, min: 0 });
    this.validateLane(input.laneId);
    const row = this.write<RunRow>(`INSERT INTO run (id, agent_id, lane_id, connector_kind, host_id, external_run_ref,
      status, model, started_at, completed_at, failure_count, waiting_detail_json, semon_session_uid, semon_identity_json, updated_at)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (id) DO UPDATE SET agent_id = excluded.agent_id, lane_id = excluded.lane_id,
        connector_kind = excluded.connector_kind, host_id = excluded.host_id, external_run_ref = excluded.external_run_ref,
        status = excluded.status, model = excluded.model, started_at = excluded.started_at,
        completed_at = excluded.completed_at, failure_count = excluded.failure_count,
        waiting_detail_json = excluded.waiting_detail_json, updated_at = excluded.updated_at,
        semon_identity_json = CASE
          WHEN ? THEN excluded.semon_identity_json
          WHEN run.connector_kind = excluded.connector_kind AND run.host_id = excluded.host_id
            AND run.external_run_ref = excluded.external_run_ref THEN run.semon_identity_json
          ELSE 'null'
        END,
        semon_session_uid = CASE
          WHEN ? THEN excluded.semon_session_uid
          WHEN run.connector_kind = excluded.connector_kind AND run.host_id = excluded.host_id
            AND run.external_run_ref = excluded.external_run_ref
            AND (? = 0 OR run.semon_identity_json = excluded.semon_identity_json) THEN run.semon_session_uid
        END
      RETURNING ${runColumns}`, input.id, input.agentId, input.laneId ?? null, input.connectorKind, input.hostId,
    input.externalRunRef, input.status, input.model ?? null, input.startedAt ?? null, input.completedAt ?? null,
    failures, encode(input.waitingDetail ?? null, "waitingDetail"), input.semonSessionUid ?? null,
    identityJson, this.now(), Number(identity !== undefined), Number(input.semonSessionUid !== undefined), Number(identity !== undefined));
    return runFromRow(row);
  }

  listRuns(): Run[] {
    return this.all<RunRow>(`SELECT ${runColumns} FROM run ORDER BY id`).map(runFromRow);
  }

  listT3LatestRunRefs(hostId: string): Map<string, string> {
    const rows = this.all<{ agentId: string; externalRunRef: string }>(`SELECT agentId, externalRunRef FROM (
      SELECT agent_id AS agentId, external_run_ref AS externalRunRef,
        row_number() OVER (PARTITION BY agent_id
          ORDER BY coalesce(started_at, completed_at, updated_at) DESC, completed_at DESC, id DESC) AS rank
      FROM run WHERE connector_kind = 't3' AND host_id = ?
    ) WHERE rank = 1`, hostId);
    return new Map(rows.map(({ agentId, externalRunRef }) => [agentId, externalRunRef]));
  }

  // The caller supplies the exact Semon UID; an external run reference is never used to infer it.
  linkRunSemonSession(runId: string, sessionUid: string): Run;
  linkRunSemonSession(runId: string, sessionUid: string, expectedIdentity: SemonSessionIdentity): Run | null;
  linkRunSemonSession(runId: string, sessionUid: string, expectedIdentity?: SemonSessionIdentity): Run | null {
    if (expectedIdentity === undefined && (!/^[\w.:@/+=,[\]"\\-]{1,4096}$/.test(runId) || runId === "." || runId === "..")) {
      throw new PlatformValidationError("invalid run ID");
    }
    if (!isSemonKey(sessionUid)) throw new PlatformValidationError("invalid Semon session UID");
    // Auto-linking uses a compare-and-set so an in-flight lookup cannot overwrite a verified link.
    const expected = expectedIdentity === undefined ? null : JSON.stringify(expectedIdentity);
    const row = this.get<RunRow>(`UPDATE run SET semon_session_uid = ?, updated_at = ?
      WHERE id = ? AND (? IS NULL OR (semon_session_uid IS NULL AND semon_identity_json = ?))
      RETURNING ${runColumns}`, sessionUid, this.now(), runId, expected, expected);
    if (row === null && expectedIdentity === undefined) throw new PlatformNotFoundError(`run ${runId} not found`);
    return row === null ? null : runFromRow(row);
  }

  upsertT3HostState(input: T3HostUpdate): T3HostState {
    const current = this.get<T3HostRow>(`SELECT ${t3HostColumns} FROM t3_host_state WHERE host_id = ?`, input.hostId);
    const state = { webUrl: null, environmentId: null, serverVersion: null, tokenExpiresAt: null, lastReconciledAt: null,
      lastEventAt: null, lastError: null, consecutiveFailures: 0, partialCoverage: false, ...current, ...input };
    const row = this.write<T3HostRow>(`INSERT INTO t3_host_state (host_id, label, mcp_url, web_url, origin_namespace,
      environment_id, server_version, token_expires_at, connection_state, last_reconciled_at, last_event_at,
      last_error, consecutive_failures, partial_coverage, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (host_id) DO UPDATE SET label = excluded.label, mcp_url = excluded.mcp_url,
        web_url = excluded.web_url, origin_namespace = excluded.origin_namespace, environment_id = excluded.environment_id,
        server_version = excluded.server_version, token_expires_at = excluded.token_expires_at,
        connection_state = excluded.connection_state, last_reconciled_at = excluded.last_reconciled_at,
        last_event_at = excluded.last_event_at, last_error = excluded.last_error,
        consecutive_failures = excluded.consecutive_failures, partial_coverage = excluded.partial_coverage,
        updated_at = excluded.updated_at
      RETURNING ${t3HostColumns}`, state.hostId, state.label, state.mcpUrl, state.webUrl, state.originNamespace,
    state.environmentId, state.serverVersion, state.tokenExpiresAt, state.connectionState,
    state.lastReconciledAt, state.lastEventAt, state.lastError, state.consecutiveFailures, Number(state.partialCoverage), this.now());
    return { ...row, partialCoverage: row.partialCoverage === 1 };
  }

  listT3HostStates(): T3HostState[] {
    return this.all<T3HostRow>(`SELECT ${t3HostColumns} FROM t3_host_state ORDER BY host_id`)
      .map((row) => ({ ...row, partialCoverage: row.partialCoverage === 1 }));
  }

  getT3Cursor(hostId: string, eventType: string): string | null {
    return this.get<{ cursor: string }>(
      "SELECT cursor FROM t3_cursor WHERE host_id = ? AND event_type = ?", hostId, eventType)?.cursor ?? null;
  }

  setT3Cursor(hostId: string, eventType: string, cursor: string | null): void {
    if (cursor === null) return;
    this.db.prepare(`INSERT INTO t3_cursor (host_id, event_type, cursor, updated_at) VALUES (?, ?, ?, ?)
      ON CONFLICT (host_id, event_type) DO UPDATE SET cursor = excluded.cursor, updated_at = excluded.updated_at
      WHERE t3_cursor.cursor <> excluded.cursor`).run(hostId, eventType, cursor, this.now());
  }

  clearT3CursorsForHost(hostId: string): void {
    this.db.prepare("DELETE FROM t3_cursor WHERE host_id = ?").run(hostId);
  }

  recordT3Wait(input: T3WaitInput, fromSnapshot = false): void {
    this.db.prepare(`INSERT INTO t3_pending_wait (host_id, thread_id, request_id, kind, questions_json, occurred_at)
      VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (host_id, thread_id, request_id) DO UPDATE SET
        kind = excluded.kind, questions_json = excluded.questions_json,
        occurred_at = CASE WHEN t3_pending_wait.resolved_at IS NULL
          THEN min(t3_pending_wait.occurred_at, excluded.occurred_at) ELSE excluded.occurred_at END,
        resolved_at = NULL, resolution = NULL
      WHERE t3_pending_wait.resolved_at IS NULL OR (? AND t3_pending_wait.resolution = 'reconciled')`).run(
      input.hostId, input.threadId, input.requestId, input.kind, encode(input.questions, "questions"), input.occurredAt,
      Number(fromSnapshot));
  }

  resolveT3Wait(input: Omit<T3WaitInput, "questions">, resolution: string): void {
    // Keep a record even if resolution precedes the matching event on its separate stream.
    this.db.prepare(`INSERT INTO t3_pending_wait (host_id, thread_id, request_id, kind, questions_json,
      occurred_at, resolved_at, resolution) VALUES (?, ?, ?, ?, 'null', ?, ?, ?)
      ON CONFLICT (host_id, thread_id, request_id) DO UPDATE SET
        resolved_at = excluded.resolved_at, resolution = excluded.resolution
      WHERE t3_pending_wait.resolved_at IS NULL OR t3_pending_wait.resolution = 'reconciled'`).run(
      input.hostId, input.threadId, input.requestId, input.kind, input.occurredAt, input.occurredAt, resolution);
  }

  listOpenT3Waits(hostId?: string, threadId?: string): T3PendingWait[] {
    return this.all<JsonColumns<T3PendingWait, "questions">>(`SELECT ${t3WaitColumns} FROM t3_pending_wait
      WHERE resolved_at IS NULL AND host_id = coalesce(?, host_id) AND thread_id = coalesce(?, thread_id)
      ORDER BY occurred_at, host_id, thread_id, request_id`, hostId ?? null, threadId ?? null)
      .map((row) => ({ ...row, questions: decode(row.questions) }));
  }

  listGitHubPrTargets(): { repository: string; number: number; watching: boolean; isDraft: boolean }[] {
    return this.all<{ repository: string; number: number; watching: number; isDraft: number | null }>(`SELECT lower(repository) AS repository,
      number, max(watching) AS watching, max(is_draft) AS isDraft FROM t3_thread_pr WHERE lower(pr_host) = 'github.com'
      AND (state = 'open' OR (state IS NULL AND watching = 1))
      GROUP BY lower(repository), number ORDER BY lower(repository), number`)
      .map((row) => ({ ...row, watching: row.watching === 1, isDraft: row.isDraft === 1 }));
  }

  listGitHubPrChecks(): GitHubPrCheck[] {
    return this.all<Omit<GitHubPrCheck, "isDraft"> & { isDraft: number }>(`SELECT repository, number,
      head_sha AS headSha, is_draft AS isDraft, rollup_state AS rollupState,
      total_checks AS totalChecks, failed_checks AS failedChecks, consecutive_failures AS consecutiveFailures,
      failure_streak AS failureStreak,
      last_checked_at AS lastCheckedAt, last_error AS lastError, updated_at AS updatedAt
      FROM github_pr_check ORDER BY repository, number`).map((row) => ({ ...row, isDraft: row.isDraft === 1 }));
  }

  recordGitHubPrCheck(repository: string, number: number, check: GitHubPrObservation): void {
    this.transaction(() => {
      const prior = this.get<{ failingHeads: string; failureStreak: number }>(`SELECT failing_heads_json AS failingHeads,
        failure_streak AS failureStreak
        FROM github_pr_check WHERE repository = ? AND number = ?`, repository, number);
      const heads = check.rollupState === "success" || check.rollupState === "closed" || prior === null
        ? new Set<string>() : new Set(JSON.parse(prior.failingHeads) as string[]);
      let streak = prior?.failureStreak ?? 0;
      if (check.rollupState === "failure" && check.headSha !== null) {
        if (heads.size === 0) streak++;
        heads.add(check.headSha);
      }
      const now = this.now();
      this.db.prepare(`INSERT INTO github_pr_check (repository, number, head_sha, is_draft, rollup_state, total_checks,
        failed_checks, consecutive_failures, failure_streak, failing_heads_json, last_checked_at, last_error, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (repository, number) DO UPDATE SET
        head_sha = coalesce(excluded.head_sha, github_pr_check.head_sha), rollup_state = excluded.rollup_state,
        is_draft = CASE WHEN excluded.head_sha IS NULL AND github_pr_check.head_sha IS NOT NULL
          THEN github_pr_check.is_draft ELSE excluded.is_draft END,
        total_checks = excluded.total_checks, failed_checks = excluded.failed_checks,
        consecutive_failures = excluded.consecutive_failures, failure_streak = excluded.failure_streak,
        failing_heads_json = excluded.failing_heads_json,
        last_checked_at = excluded.last_checked_at, last_error = excluded.last_error, updated_at = excluded.updated_at`)
        .run(repository, number, check.headSha, Number(check.isDraft), check.rollupState, check.totalChecks, check.failedChecks,
          heads.size, streak, JSON.stringify([...heads]), now, check.rollupState === "error" ? "GitHub PR checks unavailable" : null, now);
    });
  }

  private upsertT3Pr(pr: T3PrInput): void {
    this.db.prepare(`INSERT INTO t3_thread_pr (host_id, thread_id, pr_host, repository, number, url, source,
      state, watching, title, head_branch, base_branch, is_draft) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (host_id, thread_id, pr_host, repository, number) DO UPDATE SET url = excluded.url,
        source = excluded.source, state = excluded.state, watching = excluded.watching, title = excluded.title,
        head_branch = excluded.head_branch, base_branch = excluded.base_branch, is_draft = excluded.is_draft`)
      .run(pr.hostId, pr.threadId, pr.prHost, pr.repository, pr.number, pr.url, pr.source, pr.state,
        Number(pr.watching), pr.title, pr.headBranch, pr.baseBranch, pr.isDraft === null ? null : Number(pr.isDraft));
  }

  recordT3PrWake(wake: T3PrWake): void {
    this.db.prepare(`INSERT INTO t3_thread_pr (host_id, thread_id, pr_host, repository, number, url, source,
      watching, last_wake_outcome, last_wake_summary, last_wake_at) VALUES (?, ?, ?, ?, ?, '', 'watch', ?, ?, ?, ?)
      ON CONFLICT (host_id, thread_id, pr_host, repository, number) DO UPDATE SET
        watching = CASE WHEN ? THEN 0 ELSE t3_thread_pr.watching END,
        last_wake_outcome = excluded.last_wake_outcome, last_wake_summary = excluded.last_wake_summary,
        last_wake_at = excluded.last_wake_at
      WHERE t3_thread_pr.last_wake_at IS NULL OR t3_thread_pr.last_wake_at < excluded.last_wake_at`).run(
      wake.hostId, wake.threadId, wake.prHost, wake.repository, wake.number, Number(!wake.watchEnded),
      wake.outcome, wake.summary, wake.occurredAt, Number(wake.watchEnded));
  }

  listT3ThreadPrs(hostId?: string, threadId?: string): T3ThreadPr[] {
    type Row = Omit<T3ThreadPr, "watching" | "isDraft"> & { watching: number; isDraft: number | null };
    return this.all<Row>(`SELECT ${t3PrColumns} FROM t3_thread_pr
      WHERE host_id = coalesce(?, host_id) AND thread_id = coalesce(?, thread_id)
      ORDER BY last_wake_at DESC, host_id, thread_id, pr_host, repository, number`, hostId ?? null, threadId ?? null)
      .map((row) => ({ ...row, watching: row.watching === 1, isDraft: row.isDraft === null ? null : row.isDraft === 1 }));
  }

  private applyT3Thread(snapshot: T3ThreadSnapshot, observedAt: string): void {
    const { agent, runs, threadId, waits, prs } = snapshot;
    this.upsertAgent(agent);
    for (const run of runs) this.upsertRun(run);
    // If a previous active run has fallen outside T3's bounded history, stop counting it as live.
    this.db.prepare(`UPDATE run SET status = 'interrupted', completed_at = ?, updated_at = ?
      WHERE agent_id = ? AND status IN ('running', 'waiting_input', 'waiting_approval', 'stalled')
        AND id NOT IN (SELECT value FROM json_each(?))`).run(
      observedAt, observedAt, agent.id, JSON.stringify(runs.map((run) => run.id)));
    if (waits !== undefined) {
      for (const wait of waits) this.recordT3Wait(wait, true);
      this.db.prepare(`UPDATE t3_pending_wait SET resolved_at = ?, resolution = 'reconciled'
        WHERE host_id = ? AND thread_id = ? AND resolved_at IS NULL
          AND request_id NOT IN (SELECT value FROM json_each(?))`).run(
        observedAt, agent.hostId, threadId, JSON.stringify(waits.map((wait) => wait.requestId)));
    }
    if (prs !== undefined) {
      this.db.prepare("UPDATE t3_thread_pr SET watching = 0 WHERE host_id = ? AND thread_id = ?")
        .run(agent.hostId, threadId);
      for (const pr of prs) this.upsertT3Pr(pr);
    }
  }

  reconcileT3Host(host: T3HostUpdate, snapshots: T3ThreadSnapshot[], opts: {
    observedAt: string;
    listedAgentIds?: string[];
    listedThreadIds?: string[];
    anchors?: ReadonlyMap<string, string | null>;
    resetCursors?: boolean;
  }): T3HostState {
    return this.transaction(() => {
      const state = this.upsertT3HostState({ ...host, connectionState: "connected", lastReconciledAt: opts.observedAt,
        lastError: null, consecutiveFailures: 0 });
      if (opts.resetCursors) this.clearT3CursorsForHost(host.hostId);
      for (const [name, cursor] of opts.anchors ?? []) {
        if (cursor === null) this.db.prepare("DELETE FROM t3_cursor WHERE host_id = ? AND event_type = ?").run(host.hostId, name);
        else this.setT3Cursor(host.hostId, name, cursor);
      }
      for (const snapshot of snapshots) this.applyT3Thread(snapshot, opts.observedAt);
      if (opts.listedAgentIds !== undefined) {
        const listed = JSON.stringify(opts.listedAgentIds);
        this.db.prepare(`UPDATE agent SET status = 'offline', updated_at = ?
          WHERE host_id = ? AND connector_kind = 't3' AND status <> 'offline'
            AND id NOT IN (SELECT value FROM json_each(?))`).run(opts.observedAt, host.hostId, listed);
        this.db.prepare(`UPDATE run SET status = 'interrupted', completed_at = ?, updated_at = ?
          WHERE host_id = ? AND connector_kind = 't3' AND status IN ('running', 'waiting_input', 'waiting_approval', 'stalled')
            AND agent_id NOT IN (SELECT value FROM json_each(?))`).run(opts.observedAt, opts.observedAt, host.hostId, listed);
      }
      if (opts.listedThreadIds !== undefined) {
        const listed = JSON.stringify(opts.listedThreadIds);
        this.db.prepare(`UPDATE t3_pending_wait SET resolved_at = ?, resolution = 'reconciled'
          WHERE host_id = ? AND resolved_at IS NULL AND thread_id NOT IN (SELECT value FROM json_each(?))`)
          .run(opts.observedAt, host.hostId, listed);
        this.db.prepare(`UPDATE t3_thread_pr SET watching = 0
          WHERE host_id = ? AND watching = 1 AND thread_id NOT IN (SELECT value FROM json_each(?))`)
          .run(host.hostId, listed);
      }
      return state;
    });
  }

  applyT3Event(hostId: string, eventType: string, cursor: string | null, occurredAt: string,
    snapshot: T3ThreadSnapshot | undefined, apply: () => void): void {
    this.transaction(() => {
      if (snapshot !== undefined) this.applyT3Thread(snapshot, this.now());
      apply();
      this.setT3Cursor(hostId, eventType, cursor);
      this.db.prepare(`UPDATE t3_host_state SET last_event_at = ?, updated_at = ?
        WHERE host_id = ? AND (last_event_at IS NULL OR last_event_at < ?)`)
        .run(occurredAt, this.now(), hostId, occurredAt);
    });
  }

  // Event handling calls this inside applyT3Event's transaction so removal and cursor commit together.
  markT3ThreadRemoved(hostId: string, threadId: string, agentId: string): void {
    const now = this.now();
    this.db.prepare("UPDATE agent SET status = 'offline', updated_at = ? WHERE id = ? AND status <> 'offline'")
      .run(now, agentId);
    this.db.prepare(`UPDATE run SET status = 'interrupted', completed_at = ?, updated_at = ?
      WHERE agent_id = ? AND status IN ('running', 'waiting_input', 'waiting_approval', 'stalled')`).run(now, now, agentId);
    this.db.prepare(`UPDATE t3_pending_wait SET resolved_at = ?, resolution = 'reconciled'
      WHERE host_id = ? AND thread_id = ? AND resolved_at IS NULL`).run(now, hostId, threadId);
    this.db.prepare("UPDATE t3_thread_pr SET watching = 0 WHERE host_id = ? AND thread_id = ? AND watching = 1")
      .run(hostId, threadId);
  }

  markRunStalled(id: string): void {
    this.db.prepare("UPDATE run SET status = 'stalled', updated_at = ? WHERE id = ? AND status = 'running'")
      .run(this.now(), id);
  }

  hasOpenPolicyEvent(ruleCode: string, dedupeKey: string): boolean {
    return this.get("SELECT 1 FROM policy_event WHERE rule_code = ? AND dedupe_key = ? AND resolved_at IS NULL",
      ruleCode, dedupeKey) !== null;
  }

  recordDedupedPolicyEvent(input: RecordPolicyEventInput, key: string, at: string, friction = false): PolicyEvent | null {
    return this.transaction(() => {
      const row = this.get<PolicyEventRow>(`INSERT INTO policy_event
        (id, rule_code, lane_id, summary, detail_json, created_at, dedupe_key) VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (rule_code, dedupe_key) WHERE resolved_at IS NULL AND dedupe_key IS NOT NULL DO NOTHING
        RETURNING ${policyEventColumns}`, newId("pol"), input.ruleCode, input.laneId ?? null, input.summary,
      encode(input.detail ?? null, "detail"), at, key);
      if (row === null) return null;
      if (friction) this.db.prepare(`INSERT INTO friction_entry (id, kind, summary, status, lane_id, rule_code, created_at)
        VALUES (?, 'policy_break', ?, 'open', ?, ?, ?)`).run(
        newId("fric"), row.summary, row.laneId, row.ruleCode, at);
      return { ...row, detail: decode(row.detail) };
    });
  }

  resolveInactivePolicyEvents(ruleCode: string, activeKeys: readonly string[], at: string): void {
    this.db.prepare(`UPDATE policy_event SET resolved_at = ? WHERE rule_code = ? AND resolved_at IS NULL
      AND dedupe_key IS NOT NULL AND dedupe_key NOT IN (SELECT value FROM json_each(?))`)
      .run(at, ruleCode, JSON.stringify(activeKeys));
  }

  listOpenPolicyEvents(): PolicyEvent[] {
    return this.all<PolicyEventRow>(`SELECT ${policyEventColumns} FROM policy_event WHERE resolved_at IS NULL
      ORDER BY created_at DESC, rowid DESC`).map((row) => ({ ...row, detail: decode(row.detail) }));
  }

}
