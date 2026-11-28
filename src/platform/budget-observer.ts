import type { PlatformStore } from "./store.ts";
import type { PolicyEvent, SpendSummaryFilter } from "./types.ts";

type SpendSummary = Omit<ReturnType<PlatformStore["summarizeSpend"]>, "coverage"> & {
  status: "available" | "unavailable";
  coverage: "complete" | "partial" | "unknown";
  sourceCoverage: string | null;
  observedAt?: string;
  asOf?: string | null;
  snapshotId?: string | null;
};

export function readSpendSummary(store: PlatformStore, filter: SpendSummaryFilter, now: Date | string): SpendSummary {
  const summary = store.summarizeSpend(filter, now);
  if (summary.snapshotCount === 0) {
    const rollup = store.getSpendRollup(filter, summary.since);
    if (rollup !== null) {
      const observedAt = Date.parse(rollup.observedAt);
      const stale = !Number.isFinite(observedAt) || new Date(now).getTime() - observedAt > 24 * 3_600_000;
      const preferredUsd = rollup.preferredUsd ?? rollup.billedUsd ?? rollup.sourceReportedUsd ?? rollup.usdEst;
      const complete = !stale && rollup.incompleteCount === 0 && rollup.unpricedCount === 0 && rollup.coverage === "complete";
      return {
        ...summary, status: "available", sessionCount: rollup.sessionCount, snapshotCount: rollup.sessionCount,
        unpricedCount: rollup.unpricedCount, incompleteCount: rollup.incompleteCount,
        staleSnapshotCount: stale ? rollup.sessionCount : 0,
        usdEst: rollup.usdEst, sourceReportedUsd: rollup.sourceReportedUsd, billedUsd: rollup.billedUsd,
        preferredUsd, coverage: preferredUsd === null ? "unknown" : complete ? "complete" : "partial",
        sourceCoverage: rollup.coverage, observedAt: rollup.observedAt, asOf: rollup.asOf, snapshotId: rollup.snapshotId,
      };
    }
  }
  const status = summary.snapshotCount === 0 ? "unavailable" as const : "available" as const;
  return {
    ...summary, status, sourceCoverage: summary.coverage,
    coverage: summary.preferredUsd === null ? "unknown" as const
      : summary.incompleteCount > 0 || summary.unpricedCount > 0 || summary.incompleteRunCount > 0
        ? "partial" as const : "complete" as const,
  };
}

export function readBudgetsAndSpend(store: PlatformStore, now: Date | string) {
  const budgets = store.listBudgets().map((budget) => ({ ...budget, spend: readSpendSummary(store, {
    scopeKind: budget.scopeKind, scopeId: budget.scopeId, period: budget.period, source: budget.spendSource,
  }, now) }));
  const team = readSpendSummary(store, { scopeKind: "team", scopeId: null, period: "monthly", source: "semon" }, now);
  return { budgets, spend: team };
}

export function evaluateBudgets(store: PlatformStore, now: string, enabled: boolean): PolicyEvent[] {
  const ruleCode = "budget_cap";
  const events: PolicyEvent[] = [];
  const keys: string[] = [];
  if (enabled) {
    for (const budget of readBudgetsAndSpend(store, now).budgets) {
      if (!budget.enabled) continue;
      const { spend } = budget;
      const warning = budget.capUsd * (budget.warnPct / 100);
      const complete = spend.coverage === "complete";
      for (const [level, threshold] of [["warning", warning], ["cap", budget.capUsd]] as const) {
        const key = JSON.stringify([budget.id, level]);
        if (spend.preferredUsd === null || spend.preferredUsd < threshold) {
          // Missing prices cannot establish recovery even after a new period begins.
          if (!complete && store.hasOpenPolicyEvent(ruleCode, key)) keys.push(key);
          continue;
        }
        keys.push(key);
        const event = store.recordDedupedPolicyEvent({
          ruleCode, laneId: budget.scopeKind === "lane" ? budget.scopeId : null,
          summary: `${budget.scopeKind} ${budget.period} budget ${level} reached (USD ${spend.preferredUsd} of ${threshold})`,
          detail: { budgetId: budget.id, level, threshold, ...spend },
        }, key, now);
        if (event !== null) events.push(event);
      }
    }
  }
  store.resolveInactivePolicyEvents(ruleCode, keys, now);
  return events;
}
