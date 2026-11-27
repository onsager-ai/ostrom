import { pendingDecisions, readPlatformHealth } from "./policy-observer.ts";
import type { PlatformStore } from "./store.ts";
import type { JsonValue } from "./types.ts";

export const digestWindows = ["01:00Z", "13:00Z", "weekly"] as const;
export type DigestWindow = typeof digestWindows[number];

export function isDigestWindow(value: string): value is DigestWindow {
  return digestWindows.some((window) => window === value);
}

function markdownText(value: string): string {
  return value.replace(/[\r\n]+/g, " ").replace(/[\\`*_{}\[\]<>#+!|]/g, "\\$&");
}

function data(value: JsonValue): string {
  return markdownText(JSON.stringify(value));
}

function spendLine(spend: { usdEst: number | null; sourceReportedUsd: number | null; billedUsd: number | null;
  coverage: string | null; unpricedCount: number; sessionCount: number }): string {
  return `Estimated USD ${spend.usdEst ?? "unknown"} · Reported USD ${spend.sourceReportedUsd ?? "unknown"} · `
    + `Billed USD ${spend.billedUsd ?? "unknown"} · Coverage ${markdownText(spend.coverage ?? "unknown")} · `
    + `${spend.unpricedCount} of ${spend.sessionCount} sessions unpriced`;
}

function bullets(lines: string[]): string {
  return lines.length === 0 ? "None." : lines.map((line) => `- ${line}`).join("\n");
}

export function generateOwnerDigest(store: PlatformStore, { window, now }: { window: DigestWindow; now: Date | string }) {
  const generatedAt = new Date(now).toISOString();
  const decisions = pendingDecisions(store);
  const lanes = store.listLanes().map((lane) => ({ ...lane, goal: store.getLatestGoal(lane.id) }));
  const health = readPlatformHealth(store, { now });
  const prs = store.listT3ThreadPrs().filter((pr) => pr.lastWakeAt !== null);
  const { policyAlerts, githubPrs, budgets, spend } = health;
  const friction = store.listFriction({ status: "open" });
  const days = window === "weekly" ? 7 : 2;
  const today = Date.parse(`${generatedAt.slice(0, 10)}T00:00:00Z`);
  const since = new Date(today - (days - 1) * 86_400_000).toISOString();
  const counts = new Map(store.countOwnerInterventionsByDay(since).map(({ day, count }) => [day, count]));
  const interventionsByDay = Array.from({ length: days }, (_, index) => {
    const day = new Date(Date.parse(since) + index * 86_400_000).toISOString().slice(0, 10);
    return { day, count: counts.get(day) ?? 0 };
  });

  const sections = {
    decisions, lanes, agents: health.agents, activeRuns: health.activeRuns, stalledRuns: health.stalledRuns,
    waits: health.waits, prs, policyAlerts, interventionsByDay, openFrictionCount: friction.length,
    topOpenFriction: friction[0] ?? null, budgets, spend, githubPrs,
  };
  const summary = `${health.pendingOwnerDecisions.length} pending owner decisions · ${health.stalledRuns.length} stalled runs · `
    + `${health.waits.length} open waits · ${policyAlerts.length} policy alerts`;
  const stalledIds = new Set(health.stalledRuns.map(({ id }) => id));
  const agentLabels = new Map(health.agents.map(({ id, label }) => [id, label]));
  const markdown = [
    `# Owner Digest · ${window}\n\nGenerated ${generatedAt}\n\n${summary}`,
    "## Open Owner Actions & Pending Decisions",
    bullets(decisions.map((decision) => `**${markdownText(decision.title)}** · ${markdownText(decision.id)} · `
      + `${decision.urgency} · ${decision.category} · ${decision.authority}\n`
      + `  Recommendation: ${markdownText(decision.recommendation)}\n  Evidence: ${data(decision.evidence)}`)),
    "## Lanes, Goals & Weekly Plans",
    bullets(lanes.map(({ name, goal }) => goal === null ? `${markdownText(name)} · No goal`
      : `${markdownText(name)} · ${markdownText(goal.statement)} · ${markdownText(goal.metricName)} `
        + `${goal.metricCurrent ?? "unknown"}/${goal.metricTarget} ${markdownText(goal.metricUnit ?? "")}\n`
        + `  Week of ${goal.weekStart} · ${markdownText(goal.approvedDecisionId ?? "unapproved")} · Plan ${data(goal.weeklyPlan)}`)),
    "## Cross-Host Agents, Runs & Waits",
    bullets(health.agents.map((agent) => `${markdownText(agent.hostId)} · ${markdownText(agent.label)} · ${agent.status}`)),
    "### Active and stalled runs",
    bullets(health.activeRuns.map((run) => `${markdownText(run.hostId)} · ${markdownText(agentLabels.get(run.agentId) ?? run.agentId)} · `
      + `${markdownText(run.externalRunRef)} · ${stalledIds.has(run.id) ? "stalled" : run.status}`)),
    "### Pending waits",
    bullets(health.waits.map((wait) => `${markdownText(wait.hostId)} · ${markdownText(wait.threadId)} · `
      + `${wait.kind === "approval" ? "approval" : `user_input · ${data(wait.questions)}`} · ${markdownText(wait.requestId)}`)),
    "### PR watch wakes",
    bullets(prs.map((pr) => `${markdownText(pr.hostId)} · ${markdownText(pr.repository)} #${pr.number} · `
      + `${markdownText(pr.lastWakeOutcome ?? "")} · ${markdownText(pr.lastWakeSummary ?? "")}`)),
    "### GitHub PR CI",
    bullets(githubPrs.map((pr) => `${markdownText(pr.repository)} #${pr.number} · ${pr.rollupState} · `
      + `${pr.failedChecks}/${pr.totalChecks} failed · ${pr.consecutiveFailures} consecutive failing heads · ${pr.lastCheckedAt}`)),
    "## Policy Alerts & Friction",
    bullets(policyAlerts.map((event) => `${event.ruleCode} · ${markdownText(event.summary)}`)),
    `Open friction items: ${friction.length}. Top open item: ${markdownText(friction[0]?.summary ?? "None")}.`,
    `Owner interventions by day: ${interventionsByDay.map(({ day, count }) => `${day} (${count})`).join(", ")}.`,
    "## Budgets & Spend",
    bullets(sections.budgets.map((budget) => `${budget.scopeKind} ${markdownText(budget.scopeId ?? "")} · `
      + `${budget.period} cap USD ${budget.capUsd}${budget.enabled ? "" : " · disabled"} · `
      + (budget.spend.status === "available" ? spendLine(budget.spend) : "Spend unavailable"))),
    spend.status === "available" ? `Team monthly · ${spendLine(spend)}` : "Spend: unavailable.",
  ].join("\n\n");
  return { generatedAt, window, summary, markdown, sections };
}
