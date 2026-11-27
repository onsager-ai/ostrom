export const decisionAuthorities = ["owner", "chief_pm"] as const;
export const decisionCategories = ["spending", "credentials", "infra_network", "release", "direction", "policy_exception", "weekly_plan", "owner_action", "routine"] as const;
export const decisionUrgencies = ["incident", "blocking_today", "digest"] as const;
export const decisionStatuses = ["pending", "approved", "rejected", "superseded", "expired"] as const;
export const policyRuleModes = ["observe", "gate"] as const;
export const frictionKinds = ["correction", "repeated_ask", "owner_intervention", "gate_block", "policy_break"] as const;
export const frictionStatuses = ["open", "selected", "kept", "reverted"] as const;
export const connectorKinds = ["t3", "semon", "github", "magpie", "kiro", "antigravity", "codex"] as const;
export const laneStatuses = ["active", "paused", "archived"] as const;
export const budgetScopes = ["team", "lane", "agent"] as const;
export const budgetPeriods = ["daily", "weekly", "monthly"] as const;
export const spendSources = ["semon", "magpie"] as const;
export const agentRoles = ["chief_pm", "manager", "worker", "sentinel"] as const;
export const agentStatuses = ["active", "idle", "waiting", "offline", "unknown"] as const;
export const runStatuses = ["running", "waiting_input", "waiting_approval", "completed", "failed", "interrupted", "cancelled", "stalled"] as const;

export type DecisionAuthority = typeof decisionAuthorities[number];
export type DecisionCategory = typeof decisionCategories[number];
export type DecisionUrgency = typeof decisionUrgencies[number];
export type DecisionStatus = typeof decisionStatuses[number];
export type PolicyRuleMode = typeof policyRuleModes[number];
export type FrictionKind = typeof frictionKinds[number];
export type FrictionStatus = typeof frictionStatuses[number];
export type ConnectorKind = typeof connectorKinds[number];
export type LaneStatus = typeof laneStatuses[number];
export type BudgetScope = typeof budgetScopes[number];
export type BudgetPeriod = typeof budgetPeriods[number];
export type SpendSourceKind = typeof spendSources[number];
export type AgentRole = typeof agentRoles[number];
export type AgentStatus = typeof agentStatuses[number];
export type RunStatus = typeof runStatuses[number];

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export interface Lane {
  id: string;
  slug: string;
  name: string;
  status: LaneStatus;
  ownerAgentId: string | null;
  priorityRank: number;
  createdAt: string;
  updatedAt: string;
}

export interface Goal {
  id: string;
  laneId: string;
  statement: string;
  metricName: string;
  metricUnit: string | null;
  metricCurrent: number | null;
  metricTarget: number;
  weekStart: string;
  weeklyPlan: JsonValue;
  approvedDecisionId: string | null;
  updatedAt: string;
}

export interface Decision {
  id: string;
  laneId: string | null;
  authority: DecisionAuthority;
  category: DecisionCategory;
  urgency: DecisionUrgency;
  title: string;
  evidence: JsonValue;
  recommendation: string;
  options: JsonValue;
  status: DecisionStatus;
  requestedBy: string;
  decidedBy: string | null;
  answer: JsonValue;
  rationale: string | null;
  supersededById: string | null;
  createdAt: string;
  decidedAt: string | null;
}

export interface DecisionEvent {
  id: number;
  decisionId: string;
  kind: "created" | "answered" | "superseded" | "expired";
  actor: string;
  detail: JsonValue;
  createdAt: string;
}

export interface DecisionVerification {
  valid: boolean;
  status: DecisionStatus | "not_found";
  reason: string;
  decision: Decision | null;
}

export interface PolicyRule {
  id: string;
  code: string;
  sourceRef: string;
  mode: PolicyRuleMode;
  params: JsonValue;
  enabled: boolean;
  updatedAt: string;
}

export interface PolicyEvent {
  id: string;
  ruleCode: string;
  laneId: string | null;
  summary: string;
  detail: JsonValue;
  createdAt: string;
}

export interface FrictionEntry {
  id: string;
  kind: FrictionKind;
  summary: string;
  status: FrictionStatus;
  laneId: string | null;
  ruleCode: string | null;
  createdAt: string;
}

export interface Budget {
  id: string;
  scopeKind: BudgetScope;
  scopeId: string | null;
  period: BudgetPeriod;
  capUsd: number;
  warnPct: number;
  spendSource: SpendSourceKind;
  enabled: boolean;
  updatedAt: string;
}

export interface SpendSnapshot {
  sessionUid: string;
  source: SpendSourceKind;
  inputTokens: number | null;
  outputTokens: number | null;
  cachedTokens: number | null;
  usdEst: number | null;
  sourceReportedUsd: number | null;
  billedUsd: number | null;
  coverage: string | null;
  snapshotId: string | null;
  asOf: string | null;
  observedAt: string;
}

export interface Agent {
  id: string;
  role: AgentRole;
  laneId: string | null;
  connectorKind: ConnectorKind;
  hostId: string;
  externalRef: string;
  label: string;
  status: AgentStatus;
  lastSeenAt: string | null;
  updatedAt: string;
}

export interface SemonSessionIdentity {
  originNamespace: string;
  nativeHarness: string;
  nativeSessionId: string;
}

export function isSemonKey(value: unknown): value is string {
  return typeof value === "string" && /^[\w.:@/+=-]{1,256}$/.test(value);
}

export interface Run {
  id: string;
  agentId: string;
  laneId: string | null;
  connectorKind: ConnectorKind;
  hostId: string;
  externalRunRef: string;
  status: RunStatus;
  model: string | null;
  startedAt: string | null;
  completedAt: string | null;
  failureCount: number;
  waitingDetail: JsonValue;
  semonSessionUid: string | null;
  semonIdentity: SemonSessionIdentity | null;
  updatedAt: string;
}

export interface T3HostState {
  hostId: string;
  label: string;
  mcpUrl: string;
  originNamespace: string | null;
  environmentId: string | null;
  serverVersion: string | null;
  tokenExpiresAt: string | null;
  connectionState: "connecting" | "connected" | "stale" | "auth_expired" | "disconnected";
  lastReconciledAt: string | null;
  lastEventAt: string | null;
  lastError: string | null;
  consecutiveFailures: number;
  failureStreak: number;
  partialCoverage: boolean;
  updatedAt: string;
}

export interface T3PendingWait {
  hostId: string;
  threadId: string;
  requestId: string;
  kind: "user_input" | "approval";
  questions: JsonValue;
  occurredAt: string;
  resolvedAt: string | null;
  resolution: string | null;
}

export interface T3ThreadPr {
  hostId: string;
  threadId: string;
  prHost: string;
  repository: string;
  number: number;
  url: string;
  source: string;
  state: "open" | "closed" | "merged" | null;
  watching: boolean;
  title: string | null;
  headBranch: string | null;
  baseBranch: string | null;
  isDraft: boolean | null;
  lastWakeOutcome: string | null;
  lastWakeSummary: string | null;
  lastWakeAt: string | null;
}

export interface GitHubPrObservation {
  headSha: string | null;
  rollupState: "pending" | "success" | "failure" | "error" | "closed";
  totalChecks: number;
  failedChecks: number;
}

export interface GitHubPrCheck extends GitHubPrObservation {
  repository: string;
  number: number;
  consecutiveFailures: number;
  failureStreak: number;
  lastCheckedAt: string;
  lastError: string | null;
  updatedAt: string;
}

export interface SpendSummaryFilter {
  scopeKind: BudgetScope;
  scopeId: string | null;
  period: BudgetPeriod;
  source: SpendSourceKind;
}
