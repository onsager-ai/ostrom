// Per-ticket budget limits and accounting. Limits come from the environment when a ticket opens;
// a later operator extension changes only that ticket's stored limits.

import type { Store, TicketBudget } from "../store/store.ts";

export const BUDGET_LIMITS = ["time", "idle", "turns", "tokens", "escalations"] as const;
export type BudgetLimit = (typeof BUDGET_LIMITS)[number];

export interface BudgetDefaults {
  timeMinutes: number;
  idleMinutes: number;
  turns: number;
  tokens: number;
  escalations: number;
}

export const DEFAULT_BUDGET: BudgetDefaults = {
  timeMinutes: 8 * 60,
  idleMinutes: 30,
  turns: 80,
  tokens: 1_000_000,
  escalations: 0,
};

/** Reads milestone-1 budget defaults. Every value is a positive whole number except escalations. */
export function loadBudgetDefaults(env: Record<string, string | undefined>): BudgetDefaults {
  return {
    timeMinutes: integer(env, "OSTROM_BUDGET_TIME_MIN", DEFAULT_BUDGET.timeMinutes, false),
    idleMinutes: integer(env, "OSTROM_BUDGET_IDLE_MIN", DEFAULT_BUDGET.idleMinutes, false),
    turns: integer(env, "OSTROM_BUDGET_TURNS", DEFAULT_BUDGET.turns, false),
    tokens: integer(env, "OSTROM_BUDGET_TOKENS", DEFAULT_BUDGET.tokens, false),
    escalations: integer(env, "OSTROM_BUDGET_ESCALATIONS", DEFAULT_BUDGET.escalations, true),
  };
}

export function budgetWarning(budget: TicketBudget, now: Date): BudgetLimit | null {
  return BUDGET_LIMITS.find((limit) => budgetLimit(budget, limit) > 0 && budgetUsage(budget, limit, now) * 100 >= budgetLimit(budget, limit) * 80) ?? null;
}

export function budgetReached(budget: TicketBudget, now: Date): BudgetLimit | null {
  return BUDGET_LIMITS.find((limit) => budgetLimit(budget, limit) > 0 && budgetUsage(budget, limit, now) >= budgetLimit(budget, limit)) ?? null;
}

/** Limits that can change only when a Worker has completed a native turn. Time and idle are Watchdog signals. */
export function turnBudgetReached(budget: TicketBudget, now: Date): BudgetLimit | null {
  return (["turns", "tokens", "escalations"] as const).find(
    (limit) => budgetLimit(budget, limit) > 0 && budgetUsage(budget, limit, now) >= budgetLimit(budget, limit),
  ) ?? null;
}

export function budgetUsage(budget: TicketBudget, limit: BudgetLimit, now: Date): number {
  switch (limit) {
    case "time":
      return Math.max(0, Math.floor((now.getTime() - Date.parse(budget.openedAt)) / 60_000));
    // Idle time is how long the running turn has gone without finishing. Between turns the Worker
    // waits on people (a review, an answer), which is not idle; Semon's verdicts follow the same rule.
    case "idle":
      return budget.turnStartedAt === null ? 0 : Math.max(0, Math.floor((now.getTime() - Date.parse(budget.turnStartedAt)) / 60_000));
    case "turns": return budget.turns;
    // Cached input remains recorded for operators but does not consume the ticket token budget.
    case "tokens": return budget.inputTokens + budget.outputTokens;
    case "escalations": return budget.escalations;
  }
}

export function budgetLimit(budget: TicketBudget, limit: BudgetLimit): number {
  switch (limit) {
    case "time": return budget.timeMinutes;
    case "idle": return budget.idleMinutes;
    case "turns": return budget.turnLimit;
    case "tokens": return budget.tokenLimit;
    case "escalations": return budget.escalationLimit;
  }
}

function integer(env: Record<string, string | undefined>, name: string, fallback: number, zero: boolean): number {
  const value = env[name]?.trim();
  if (value === undefined || value === "") return fallback;
  if (!new RegExp(zero ? "^\\d+$" : "^[1-9]\\d*$").test(value)) {
    throw new Error(`${name} must be ${zero ? "a non-negative" : "a positive"} whole number, got ${JSON.stringify(value)}`);
  }
  return Number(value);
}
