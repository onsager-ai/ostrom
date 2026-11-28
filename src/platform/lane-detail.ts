import { readSpendSummary } from "./budget-observer.ts";
import type { PlatformStore } from "./store.ts";
import type { Agent, Lane, T3ThreadPr } from "./types.ts";

const dayMs = 86_400_000;
const clockGraceMs = 60_000;
const repositoryAliases = new Map([
  ["agent-workbench", "awb"], ["crawlab-pro", "crawlab"], ["crawlab-docs", "crawlab"],
]);
const prKey = (repository: string, number: number) => `${repository.toLowerCase()}#${number}`;
const threadKey = (hostId: string, threadId: string) => JSON.stringify([hostId, threadId]);

function linkedPullRequests(store: PlatformStore, lane: Lane, agents: Agent[]) {
  const slug = lane.slug.toLowerCase();
  const threads = new Set<string>();
  for (const agent of agents) {
    if (agent.connectorKind === "t3") {
      const threadId = agent.externalRef.slice(agent.externalRef.indexOf("/") + 1);
      threads.add(threadKey(agent.hostId, threadId));
    }
  }

  // Store order puts the latest wake first. Another link can supply PR metadata for a wake-only row.
  const links = new Map<string, { metadata: T3ThreadPr; wake: T3ThreadPr }>();
  for (const pr of store.listT3ThreadPrs()) {
    if (pr.prHost.toLowerCase() !== "github.com" || !/^[a-z\d-]+\/[\w.-]+$/i.test(pr.repository)
      || !Number.isSafeInteger(pr.number) || pr.number <= 0) continue;
    const repo = pr.repository.slice(pr.repository.indexOf("/") + 1).toLowerCase();
    if (repo === "." || repo === "..") continue;
    if (repo !== slug && repositoryAliases.get(repo) !== slug && !threads.has(threadKey(pr.hostId, pr.threadId))) continue;
    const key = prKey(pr.repository, pr.number);
    const existing = links.get(key);
    if (existing === undefined) {
      links.set(key, { metadata: pr, wake: pr });
    } else {
      const current = existing.metadata;
      const preferred = (pr.state === "merged" && current.state !== "merged")
        || (current.state === null && pr.state !== null) ? pr : current;
      existing.metadata = { ...preferred, title: preferred.title ?? current.title ?? pr.title };
    }
  }

  const checks = new Map(store.listGitHubPrChecks().map((pr) => [prKey(pr.repository, pr.number), pr]));
  return Array.from(links, ([key, { metadata, wake }]) => {
    const { repository, number, title, state } = metadata;
    const check = checks.get(key);
    return {
      repository, number, title, state, url: `https://github.com/${repository}/pull/${number}`,
      isDraft: check?.isDraft ?? metadata.isDraft,
      rollupState: check?.rollupState ?? null, failedChecks: check?.failedChecks ?? null,
      totalChecks: check?.totalChecks ?? null, consecutiveFailures: check?.consecutiveFailures ?? null,
      lastCheckedAt: check?.lastCheckedAt ?? null, lastError: check?.lastError ?? null,
      lastWakeAt: wake.lastWakeAt, lastWakeOutcome: wake.lastWakeOutcome,
    };
  });
}

type TimelineKind = "goal_updated" | "goal_completed" | "decision_created" | "decision_answered"
  | "policy_alert" | "pr_wake" | "pr_merged";

export function readLaneDetail(store: PlatformStore, lane: Lane, now: Date) {
  const timestamp = now.getTime();
  const timeLimit = timestamp + clockGraceMs;
  const recent = (at: string | null, days: number) => {
    const time = Date.parse(at ?? "");
    return time >= timestamp - days * dayMs && time <= timeLimit;
  };
  const goal = store.getLatestGoal(lane.id);
  const goalHistory = store.listGoals(lane.id);
  const agents = store.listAgents();
  const belongs = ({ id, laneId }: Agent) => laneId === lane.id || id === lane.ownerAgentId;
  const laneAgents = agents.filter(belongs);
  const agentsById = new Map(agents.map((agent) => [agent.id, agent]));
  const recentAgents = laneAgents.filter(({ status, lastSeenAt }) => status === "active" || status === "waiting" || recent(lastSeenAt, 1))
    .map(({ id, label, role, status, lastSeenAt }) => ({ id, label, role, status, lastSeenAt }));
  const liveRuns = store.listRuns().flatMap((run) => {
    const agent = agentsById.get(run.agentId);
    const { status, startedAt, model } = run;
    if (agent === undefined || (run.laneId !== null ? run.laneId !== lane.id : !belongs(agent)) || run.completedAt !== null
      || (status !== "running" && status !== "waiting_input" && status !== "waiting_approval" && status !== "stalled")) return [];
    const view = { agentLabel: agent.label, role: agent.role, isLaneOwner: agent.id === lane.ownerAgentId,
      status: status === "stalled" ? "unknown" as const : status, model, startedAt,
      harness: run.semonIdentity?.nativeHarness ?? run.connectorKind };
    return [{ run, view, time: Date.parse(startedAt ?? run.updatedAt) || 0 }];
  });
  liveRuns.sort((a, b) => b.time - a.time || b.run.id.localeCompare(a.run.id));
  const newestByAgent = new Map<string, typeof liveRuns[number]["view"]>();
  for (const { run, view } of liveRuns) if (!newestByAgent.has(run.agentId)) newestByAgent.set(run.agentId, view);
  const activeRuns = [...newestByAgent.values()];
  const prs = linkedPullRequests(store, lane, laneAgents);
  const decisions = store.listDecisions({ laneId: lane.id }).filter((decision) => decision.status === "pending"
    || recent(decision.createdAt, 7) || recent(decision.decidedAt, 7));
  const alerts = store.listOpenPolicyEvents().filter(({ laneId }) => laneId === lane.id);
  const friction = store.listFriction({ status: "open" }).filter(({ laneId }) => laneId === lane.id);
  const spend = readSpendSummary(store, { scopeKind: "lane", scopeId: lane.id, period: "weekly", source: "semon" }, now, { rollupOnly: true });

  const timeline: { kind: TimelineKind; at: string; summary: string }[] = [];
  function add(kind: TimelineKind, at: string | null, summary: string) {
    if (at !== null && Number.isFinite(Date.parse(at))) timeline.push({ kind, at, summary });
  }
  for (const entry of goalHistory) {
    if (entry.updatedAt !== entry.completedAt) add("goal_updated", entry.updatedAt, `Goal updated · ${entry.statement}`);
    add("goal_completed", entry.completedAt, `Goal completed · ${entry.statement}`);
  }
  for (const decision of decisions) {
    add("decision_created", decision.createdAt, `Decision requested · ${decision.title}`);
    add("decision_answered", decision.decidedAt, `Decision answered · ${decision.title}`);
  }
  for (const alert of alerts) add("policy_alert", alert.createdAt, alert.summary);
  const openIds = new Set(alerts.map(({ id }) => id));
  for (const alert of store.listPolicyEvents({ laneId: lane.id })) {
    if (!openIds.has(alert.id) && recent(alert.createdAt, 7)) add("policy_alert", alert.createdAt, alert.summary);
  }
  for (const pr of prs) {
    const name = `${pr.repository} #${pr.number}${pr.title === null ? "" : ` · ${pr.title}`}`;
    if (pr.state === "merged") add("pr_merged", pr.lastWakeAt, `PR merged · ${name}`);
    else add("pr_wake", pr.lastWakeAt, `PR watch woke · ${name}${pr.lastWakeOutcome ? ` · ${pr.lastWakeOutcome}` : ""}`);
  }
  timeline.sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
  return { lane, goal, goalHistory, prs, activeRuns, recentAgents, spend, decisions, alerts, friction, timeline: timeline.slice(0, 25) };
}
