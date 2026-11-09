import assert from "node:assert/strict";
import { test } from "node:test";

import { budgetReached, budgetUsage, budgetWarning, DEFAULT_BUDGET, loadBudgetDefaults } from "./budgets.ts";
import type { TicketBudget } from "../store/store.ts";

const OPENED = "2026-11-09T12:00:00.000Z";
const NOW = new Date("2026-11-09T12:48:00.000Z");

function budget(overrides: Partial<TicketBudget> = {}): TicketBudget {
  return {
    ticketId: 1, openedAt: OPENED, timeMinutes: 60, idleMinutes: 30, turnLimit: 10,
    tokenLimit: 100, escalationLimit: 0, escalations: 0, turns: 8,
    inputTokens: 40, cachedInputTokens: 30, outputTokens: 10,
    lastActivityAt: "2026-11-09T12:18:00.000Z", ...overrides,
  };
}

test("budget accounting excludes cached input from token limits but keeps it in stored usage", () => {
  assert.equal(budgetUsage(budget(), "tokens", NOW), 50);
  assert.equal(budgetWarning(budget(), NOW), "time");
  assert.equal(budgetReached(budget(), NOW), "idle");
  assert.equal(budgetReached(budget({ idleMinutes: 60, escalationLimit: 1, escalations: 1 }), NOW), "escalations");
  assert.equal(budgetWarning(budget({ timeMinutes: 120, idleMinutes: 60 }), NOW), "turns");
});

test("budget defaults are documented constants and env values are validated", () => {
  assert.deepEqual(loadBudgetDefaults({}), DEFAULT_BUDGET);
  assert.deepEqual(loadBudgetDefaults({ OSTROM_BUDGET_TIME_MIN: "12", OSTROM_BUDGET_ESCALATIONS: "2" }), {
    ...DEFAULT_BUDGET, timeMinutes: 12, escalations: 2,
  });
  assert.throws(() => loadBudgetDefaults({ OSTROM_BUDGET_TOKENS: "0" }), /OSTROM_BUDGET_TOKENS/);
  assert.throws(() => loadBudgetDefaults({ OSTROM_BUDGET_ESCALATIONS: "-1" }), /OSTROM_BUDGET_ESCALATIONS/);
});
