import { PlatformApiClient } from "./api.ts";
import type { generateOwnerDigest, DigestWindow } from "./digest.ts";
import type { readPlatformHealth } from "./policy-observer.ts";
import { decisionAuthorities, decisionCategories, decisionUrgencies, laneStatuses } from "./types.ts";
import type { Decision, Goal, JsonValue, Lane } from "./types.ts";

type Health = ReturnType<typeof readPlatformHealth>;
type Digest = ReturnType<typeof generateOwnerDigest>;
type ReadPath = "/v1/health/summary" | "/v1/spend/summary" | "/v1/lanes" |
  "/v1/decisions?status=pending" | `/v1/digest?window=${DigestWindow}`;

const policyCodes = ["authority_routing", "budget_cap", "pr_green_ci", "two_failure_stop"] as const;
const finite = (value: number | null) => Number.isFinite(value) ? value : null;
const oneOf = (value: string, allowed: readonly string[]) => allowed.includes(value) ? value : null;
const timestamp = (value: string) =>
  /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/.test(value) ? value : null;
const markdownText = (value: string | null) =>
  (value ?? "").replace(/[\r\n]+/g, " ").replace(/[\\`*_{}\[\]<>#+!|]/g, "\\$&");

// Display fields can themselves contain references to the private records. Remove known
// identifiers and common URL, path, opaque ID and bearer formats before mapping them.
function textSanitizer(health: Health, digest: Digest, lanes: Lane[], decisions: Decision[]) {
  const denied = new Set<string>();
  const add = (...values: (string | null | undefined)[]) => {
    for (const value of values) if (typeof value === "string" && value.length > 1) denied.add(value.toLowerCase());
  };
  function addEvidence(value: JsonValue): void {
    const pending = [value];
    while (pending.length > 0) {
      const item = pending.pop();
      if (typeof item === "string" && item.length >= 4) add(item);
      else if (item !== null && typeof item === "object") {
        for (const child of Object.values(item)) pending.push(child);
      }
    }
  }
  for (const lane of lanes) add(lane.ownerAgentId);
  for (const lane of digest.sections.lanes) add(lane.ownerAgentId);
  for (const host of health.hosts) {
    add(host.hostId, host.label, host.mcpUrl, host.originNamespace, host.environmentId, host.tokenExpiresAt, host.lastError);
    add(URL.parse(host.mcpUrl)?.hostname);
    add(host.webUrl, URL.parse(host.webUrl ?? "")?.hostname);
  }
  for (const agent of health.agents) add(agent.label, agent.id, agent.externalRef);
  for (const run of [...health.activeRuns, ...health.stalledRuns]) add(run.externalRunRef, run.semonSessionUid, run.semonIdentity?.nativeSessionId);
  for (const wait of health.waits) add(wait.threadId, wait.requestId);
  for (const pr of digest.sections.prs) add(pr.threadId, pr.repository, pr.headBranch, pr.url, pr.title, pr.lastWakeSummary);
  for (const pr of health.githubPrs) add(pr.repository);
  for (const decision of [...decisions, ...health.pendingOwnerDecisions]) {
    add(decision.requestedBy, decision.decidedBy);
    addEvidence(decision.evidence);
  }
  return (value: string): string => {
    if (typeof value !== "string") return "";
    const lower = value.toLowerCase();
    for (const secret of denied) if (lower.includes(secret)) return "[redacted]";
    return value
      .replace(/\b(?:bearer\s+|(?:token|secret)\s*[:=]\s*)\S+|\b(?:gh[pousr]_|sk-|github_pat_)[\w-]+/gi, "[redacted]")
      .replace(/\b[A-Za-z0-9_-]{32,}\b|\b[\da-f]{8}(?:-[\da-f]{4}){3}-[\da-f]{12}\b/gi, "[redacted]")
      .replace(/\b[a-z][a-z0-9+.-]*:\/\/[^\s<>]+|(?:[\w.+-]+@)?(?:[\w-]+\.)+[a-z]{2,}\b|(?:[\w~.-]*\/)+[\w./-]+/gi, "[redacted]");
  };
}

function withGoal(goal: Goal | null, clean: (value: string) => string) {
  return goal === null ? null : {
    statement: clean(goal.statement),
    metricName: clean(goal.metricName),
    metricCurrent: finite(goal.metricCurrent),
    metricTarget: finite(goal.metricTarget),
    metricUnit: goal.metricUnit === null ? null : clean(goal.metricUnit),
    weekStart: /^\d{4}-\d\d-\d\d$/.test(goal.weekStart) ? goal.weekStart : null,
    approved: typeof goal.approvedDecisionId === "string" && goal.approvedDecisionId.length > 0,
  };
}

export function createStatusReader(socketPath: string) {
  const client = new PlatformApiClient(socketPath);
  const get = <T>(path: ReadPath) => client.request<T>("GET", path);
  const health = () => get<Health>("/v1/health/summary");
  const spend = () => get<Health["spend"]>("/v1/spend/summary");
  const digest = (window: DigestWindow) => get<Digest>(`/v1/digest?window=${window}`);

  function summary(snapshot: Health, spendSnapshot: Health["spend"], generatedAt: string) {
    return {
      pendingOwnerDecisions: snapshot.pendingOwnerDecisions.length,
      activeRuns: snapshot.activeRuns.length,
      stalledRuns: snapshot.stalledRuns.length,
      openWaits: snapshot.waits.length,
      openPolicyAlerts: snapshot.policyAlerts.length,
      trackedAgents: snapshot.agents.length,
      connectedHosts: snapshot.hosts.filter(({ connectionState }) => connectionState === "connected").length,
      totalHosts: snapshot.hosts.length,
      spendStatus: spendSnapshot.status === "available" ? "available" : "unavailable",
      generatedAt,
    };
  }

  async function readDetails(window: DigestWindow) {
    const [snapshot, rawDigest, lanes, decisions] = await Promise.all([
      health(), digest(window), get<Lane[]>("/v1/lanes"), get<Decision[]>("/v1/decisions?status=pending"),
    ]);
    const clean = textSanitizer(snapshot, rawDigest, lanes, [...decisions, ...rawDigest.sections.decisions]);
    // The list route has no goals. The digest's structured lane records include them.
    const goals = new Map(rawDigest.sections.lanes.map(({ id, goal }) => [id, goal]));
    const slugs = new Map(lanes.map(({ id, slug }) => [id, clean(slug)]));
    return {
      snapshot,
      lanes: lanes.map(({ id, slug, name, status, priorityRank }) => ({
        slug: clean(slug), name: clean(name), status: oneOf(status, laneStatuses),
        priorityRank: finite(priorityRank), goal: withGoal(goals.get(id) ?? null, clean),
      })),
      decisions: decisions.filter(({ status }) => status === "pending").map((decision) => ({
        id: /^dec_[0-9a-f]{12}$/.test(decision.id) ? decision.id : null,
        laneSlug: decision.laneId === null ? null : slugs.get(decision.laneId) ?? null,
        authority: oneOf(decision.authority, decisionAuthorities),
        category: oneOf(decision.category, decisionCategories),
        urgency: oneOf(decision.urgency, decisionUrgencies),
        title: clean(decision.title),
        recommendation: clean(decision.recommendation),
        options: Array.isArray(decision.options) ? decision.options.flatMap((option) =>
          typeof option === "string" ? [clean(option)] : []) : [],
        createdAt: timestamp(decision.createdAt),
      })),
    };
  }

  return {
    async statusSummary() {
      const [snapshot, spendSnapshot] = await Promise.all([health(), spend()]);
      return summary(snapshot, spendSnapshot, new Date().toISOString());
    },
    async listLanes() {
      return (await readDetails("01:00Z")).lanes;
    },
    async listOpenDecisions() {
      return (await readDetails("01:00Z")).decisions;
    },
    async readLatestDigest(window: DigestWindow) {
      const [{ snapshot, lanes, decisions }, spendSnapshot] = await Promise.all([readDetails(window), spend()]);
      const status = summary(snapshot, spendSnapshot, new Date().toISOString());
      const alerts = policyCodes.map((code) => ({
        code, count: snapshot.policyAlerts.filter(({ ruleCode }) => code === ruleCode).length,
      })).filter(({ count }) => count > 0);
      const markdown = [
        `# Ostrom digest · ${window}`,
        `Generated ${status.generatedAt}`,
        `${status.pendingOwnerDecisions} pending owner decisions · ${status.activeRuns} active runs · ${status.stalledRuns} stalled runs · ${status.openWaits} open waits · ${status.openPolicyAlerts} policy alerts`,
        `Spend data: ${status.spendStatus}`,
        "## Open decisions",
        ...decisions.map(({ title, recommendation, options }) =>
          `- ${markdownText(title)} · ${markdownText(recommendation)}${options.map((option) => ` · ${markdownText(option)}`).join("")}`),
        "## Lanes",
        ...lanes.map(({ name, status, goal }) => `- ${markdownText(name)} · ${status ?? ""}${goal
          ? ` · ${markdownText(goal.statement)} · ${markdownText(goal.metricName)} ${goal.metricCurrent ?? "unknown"}/${goal.metricTarget ?? "unknown"} ${markdownText(goal.metricUnit)}`
          : ""}`),
        "## Policy alerts",
        ...alerts.map(({ code, count }) => `- ${markdownText(code)} · ${count}`),
      ].join("\n");
      return { window, summary: status, markdown };
    },
  };
}
