import { evaluateBudgets, readBudgetsAndSpend } from "./budget-observer.ts";
import { goalStaleKey } from "./store.ts";
import type { PlatformStore } from "./store.ts";
import type { Decision, Run } from "./types.ts";

const liveStatuses = new Set(["running", "waiting_input", "waiting_approval", "stalled"]);
const ownerCategories = new Set<Decision["category"]>([
  "spending", "credentials", "infra_network", "release", "direction", "policy_exception", "weekly_plan", "owner_action",
]);
const urgencyOrder = { incident: 0, blocking_today: 1, digest: 2 };

export function pendingDecisions(store: PlatformStore): Decision[] {
  return store.listDecisions({ status: "pending" }).sort((a, b) =>
    urgencyOrder[a.urgency] - urgencyOrder[b.urgency] || a.createdAt.localeCompare(b.createdAt) || a.id.localeCompare(b.id));
}

interface HealthOptions {
  now: Date | string;
  stallIdleMinutes?: number;
  tokenWarnDays?: number;
  goalStaleHours?: number;
}

export function readPlatformHealth(store: PlatformStore, {
  now, stallIdleMinutes = 30, tokenWarnDays = 7,
}: HealthOptions) {
  const timestamp = new Date(now).getTime();
  const hosts = store.listT3HostStates();
  const agents = store.listAgents();
  const agentById = new Map(agents.map((agent) => [agent.id, agent]));
  const activeRuns = store.listRuns().filter((run) => liveStatuses.has(run.status));
  const stalledRuns = activeRuns.filter((run) => {
    if (run.status === "stalled") return true;
    if (run.status !== "running") return false;
    const lastSeen = agentById.get(run.agentId)?.lastSeenAt;
    const elapsed = timestamp - Math.max(Date.parse(run.startedAt ?? run.updatedAt),
      Date.parse(lastSeen ?? (run.connectorKind === "t3" ? run.startedAt ?? run.updatedAt : run.updatedAt)));
    return elapsed >= stallIdleMinutes * 60_000;
  });
  const tokenExpiryWarnings = hosts.flatMap((host) => {
    if (host.tokenExpiresAt === null) return [];
    const remainingMs = Date.parse(host.tokenExpiresAt) - timestamp;
    return remainingMs > 0 && remainingMs <= tokenWarnDays * 86_400_000
      ? [{ hostId: host.hostId, expiresAt: host.tokenExpiresAt, remainingMs }] : [];
  });
  return {
    hosts, agents, activeRuns, stalledRuns, tokenExpiryWarnings,
    githubPrs: store.listGitHubPrChecks(),
    policyAlerts: store.listOpenPolicyEvents(),
    ...readBudgetsAndSpend(store, now),
    waits: store.listOpenT3Waits(),
    unhealthyHosts: hosts.filter((host) => ["stale", "disconnected", "auth_expired"].includes(host.connectionState)),
    pendingOwnerDecisions: pendingDecisions(store).filter((decision) => decision.authority === "owner"),
  };
}

function runTime(run: Run): string {
  return run.startedAt ?? run.completedAt ?? run.updatedAt;
}

export function evaluatePolicyAndHealth(store: PlatformStore, options: HealthOptions & { markStalled?: boolean }) {
  const now = new Date(options.now).toISOString();
  const timestamp = Date.parse(now);
  const enabled = new Set(store.listPolicyRules().filter((rule) => rule.enabled && rule.mode === "observe").map((rule) => rule.code));
  const failureRule = "two_failure_stop";
  const authorityRule = "authority_routing";
  const failureKeys: string[] = [];
  const authorityKeys: string[] = [];
  const ciKeys: string[] = [];
  const goalKeys: string[] = [];
  const events = [];

  if (enabled.has(failureRule)) {
    const currentRuns: Run[] = [];
    const settledByAgent = new Map<string, Run[]>();
    for (const run of store.listRuns()) {
      if (liveStatuses.has(run.status)) {
        currentRuns.push(run);
      } else {
        if (!settledByAgent.has(run.agentId)) settledByAgent.set(run.agentId, []);
        settledByAgent.get(run.agentId)!.push(run);
      }
    }
    for (const agent of store.listAgents()) {
      const settled = settledByAgent.get(agent.id);
      if (!settled?.length) continue;
      settled.sort((a, b) => runTime(a).localeCompare(runTime(b))
        || (a.completedAt ?? "").localeCompare(b.completedAt ?? "") || a.id.localeCompare(b.id));
      currentRuns.push(settled.at(-1)!);
      const start = settled.findLastIndex((run) => run.status !== "failed") + 1;
      const first = settled[start];
      const second = settled[start + 1];
      if (first === undefined || second === undefined) continue;
      const key = `streak:${agent.id}:${second.id}`;
      failureKeys.push(key);
      const event = store.recordDedupedPolicyEvent({
        ruleCode: failureRule, laneId: agent.laneId, summary: `${agent.label} failed two consecutive runs`,
        detail: { agentId: agent.id, runIds: [first.id, second.id] },
      }, key, now, true);
      if (event !== null) events.push(event);
    }
    for (const run of currentRuns) {
      if (run.failureCount < 2) continue;
      const key = `run:${run.id}`;
      failureKeys.push(key);
      const event = store.recordDedupedPolicyEvent({
        ruleCode: failureRule, laneId: run.laneId,
        summary: `Run ${run.externalRunRef} recorded ${run.failureCount} failures`,
        detail: { runId: run.id, agentId: run.agentId, failureCount: run.failureCount },
      }, key, now, true);
      if (event !== null) events.push(event);
    }
  }

  if (enabled.has(authorityRule)) {
    for (const decision of pendingDecisions(store)) {
      if (decision.authority !== "chief_pm" || !ownerCategories.has(decision.category)) continue;
      authorityKeys.push(decision.id);
      const event = store.recordDedupedPolicyEvent({
        ruleCode: authorityRule, laneId: decision.laneId,
        summary: `Decision ${decision.id} requires owner authority`,
        detail: { decisionId: decision.id, category: decision.category, actual: decision.authority, required: "owner" },
      }, decision.id, now);
      if (event !== null) events.push(event);
    }
  }

  const ciRule = "pr_green_ci";
  const targets = new Map(store.listGitHubPrTargets().map((pr) => [`${pr.repository}#${pr.number}`, pr]));
  const lanes = store.listLanes();
  const laneBySlug = new Map(lanes.map((lane) => [lane.slug.toLocaleLowerCase(), lane]));
  const laneAliases: Record<string, string> = {
    "agent-workbench": "awb", "crawlab-pro": "crawlab", "crawlab-docs": "crawlab",
  };
  const threadLane = new Map<string, string>();
  const agents = store.listAgents();
  for (const pr of store.listT3ThreadPrs()) {
    const agent = agents.find((candidate) => candidate.hostId === pr.hostId
      && candidate.externalRef.endsWith(`/${pr.threadId}`));
    if (agent?.laneId !== null && agent?.laneId !== undefined) {
      threadLane.set(`${pr.repository.toLocaleLowerCase()}#${pr.number}`, agent.laneId);
    }
  }
  for (const pr of store.listGitHubPrChecks()) {
    const name = `${pr.repository}#${pr.number}`;
    const target = targets.get(name);
    if (target === undefined || pr.isDraft || pr.rollupState === "closed" || pr.rollupState === "success") continue;
    const key = `github:${name}`;
    const repositoryName = pr.repository.split("/").at(-1)?.toLocaleLowerCase() ?? "";
    const lane = laneBySlug.get(repositoryName)
      ?? laneBySlug.get(laneAliases[repositoryName] ?? "")
      ?? lanes.find((candidate) => candidate.id === threadLane.get(`${pr.repository.toLocaleLowerCase()}#${pr.number}`));
    const detail = { ...pr, laneSlug: lane?.slug ?? null };
    const cause = pr.rollupState === "error"
      ? (pr.lastError ? `CI error: ${pr.lastError}` : "CI could not be checked")
      : pr.totalChecks > 0 && pr.failedChecks > 0 ? `${pr.failedChecks}/${pr.totalChecks} checks failed` : "CI failed";
    const prefix = lane === undefined ? "" : `${lane.name} · `;
    const streakKey = `${key}:${pr.failureStreak}`;
    const twoFailureStop = enabled.has(failureRule) && pr.consecutiveFailures >= 2
      && (target.watching || store.hasOpenPolicyEvent(failureRule, streakKey));
    if (twoFailureStop) {
      failureKeys.push(streakKey);
      const event = store.recordDedupedPolicyEvent({
        ruleCode: failureRule, laneId: lane?.id ?? null,
        summary: `${prefix}${name}: ${cause} (${pr.consecutiveFailures} consecutive heads)`, detail,
      }, streakKey, now, true);
      if (event !== null) events.push(event);
    }
    if (!twoFailureStop && enabled.has(ciRule) && (
      pr.rollupState === "failure" || pr.rollupState === "error"
      || (pr.rollupState === "pending" && store.hasOpenPolicyEvent(ciRule, key))
    )) {
      ciKeys.push(key);
      if (pr.rollupState !== "pending") {
        const event = store.recordDedupedPolicyEvent({
          ruleCode: ciRule, laneId: lane?.id ?? null, summary: `${prefix}${name}: ${cause}`, detail,
        }, key, now);
        if (event !== null) events.push(event);
      }
    }
  }
  if (enabled.has("goal_stale")) {
    const configuredHours = store.listPolicyRules().find((rule) => rule.code === "goal_stale")?.params;
    const policyHours = configuredHours !== null && typeof configuredHours === "object" && !Array.isArray(configuredHours)
      ? configuredHours.staleHours : undefined;
    const staleHours = options.goalStaleHours ?? (typeof policyHours === "number" && Number.isFinite(policyHours) && policyHours > 0
      ? policyHours : 24);
    for (const lane of store.listLanes()) {
      if (lane.status !== "active") continue;
      const goal = store.getLatestGoal(lane.id);
      if (goal === null || goal.completedAt !== null || (goal.metricTarget > 0 && goal.metricCurrent !== null
        && goal.metricCurrent >= goal.metricTarget)) continue;
      const updatedMs = Date.parse(goal.updatedAt);
      if (!Number.isFinite(updatedMs) || timestamp - updatedMs < staleHours * 3_600_000) continue;
      const key = goalStaleKey(lane.id, goal.id);
      goalKeys.push(key);
      const event = store.recordDedupedPolicyEvent({
        ruleCode: "goal_stale", laneId: lane.id,
        summary: `${lane.name} goal stale (no progress for ${staleHours}h+)`,
        detail: { laneSlug: lane.slug, goalId: goal.id, updatedAt: goal.updatedAt, staleHours },
      }, key, now);
      if (event !== null) events.push(event);
    }
  }
  store.resolveInactivePolicyEvents("goal_stale", goalKeys, now);
  store.resolveInactivePolicyEvents(ciRule, ciKeys, now);
  events.push(...evaluateBudgets(store, now, enabled.has("budget_cap")));
  store.resolveInactivePolicyEvents(failureRule, failureKeys, now);
  store.resolveInactivePolicyEvents(authorityRule, authorityKeys, now);
  const health = readPlatformHealth(store, options);
  if (options.markStalled) for (const run of health.stalledRuns) store.markRunStalled(run.id);
  return { ...health, events };
}
