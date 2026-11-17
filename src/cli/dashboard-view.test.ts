import assert from "node:assert/strict";
import { test } from "node:test";
import type { OperatorTicket, OperatorTicketDetail } from "../store/store.ts";
import { ageLabel, attentionItems, budgetMeters, createRefresh, githubUrl, localTime, phaseSummary, policyLabel, ticketPhase, ticketSummary } from "./dashboard-view.js";

function ticket(overrides: Partial<OperatorTicket> = {}): OperatorTicket {
  return {
    id: 2, state: "open", pauseCause: null, assignment: null, latestFailure: null,
    pullRequests: ["https://github.com/org/repo/pull/689"], mergeCandidate: null,
    budget: { timeMinutes: 60, turnLimit: 20, tokenLimit: 1000 },
    budgetUsage: { time: 12, turns: 2, tokens: 100, idle: 0, escalations: 0 },
    ...overrides,
  } as OperatorTicket;
}
function blocked(reason: string, quietDeadline: string | null = null) {
  return ticket({ mergeCandidate: { policy: "{}", blockingReason: reason, quietDeadline } });
}

test("phase priority is paused, failed, current turn, no PR, then merge reason", () => {
  const assignment = { outcome: "failed", lastError: "resume failed", turnStartedAt: "2026-11-16T00:00:00Z", turns: 3 } as OperatorTicket["assignment"];
  const paused = ticketPhase(ticket({ state: "paused", pauseCause: "time limit", assignment }));
  assert.equal(paused.label, "Paused"); assert.equal(paused.detail, "time limit");
  assert.equal(ticketPhase(ticket({ assignment })).detail, "resume failed");
  const failure = { kind: "dispatch", summary: "dispatch refused", at: "2026-11-16T00:00:00Z" } as const;
  assert.equal(ticketPhase(ticket({ latestFailure: failure })).label, "Failed");
  assert.equal(ticketPhase(ticket({ latestFailure: failure })).detail, "dispatch refused");
  assert.equal(ticketPhase(ticket({ state: "ended", latestFailure: failure })).label, "Finished");
  const running = ticketPhase(ticket({ assignment: { ...assignment, outcome: null } as OperatorTicket["assignment"] }));
  assert.equal(running.label, "Working"); assert.equal(running.detail, "Turn 3 of 20");
  assert.equal(ticketPhase(ticket({ pullRequests: [] })).label, "Working, no PR yet");
  assert.equal(ticketPhase({ ...blocked("policy approvals are not satisfied"), pullRequests: [] }).key, "working");
});

test("stored merge reasons map to actionable phases, including unknown reasons", () => {
  assert.equal(ticketPhase(blocked("policy approvals are not satisfied")).label, "Waiting for approval");
  assert.match(ticketPhase(blocked("policy approvals are not satisfied")).detail, /Only submitted reviews count/);
  const stale = "Approved on 0e65021; the head is now 53975b6. Approve again to merge.";
  assert.deepEqual(ticketPhase(blocked(stale)), { key: "approval", label: "Approval is out of date", tone: "amber", detail: stale });
  const deadline = "2026-11-16T15:40:00Z";
  assert.equal(ticketPhase(blocked("quiet period has not elapsed", deadline)).label, `Approved, merges after ${localTime(deadline)}`);
  assert.equal(ticketPhase(blocked("quiet period has not elapsed")).label, "Approved, waiting for quiet period");
  assert.equal(ticketPhase(blocked("pull request is not clean at the captured head and base")).label, "Waiting for CI or a clean merge");
  assert.equal(ticketPhase(blocked("Worker is not idle")).label, "Worker revising");
  assert.deepEqual(ticketPhase(blocked("new requirement")), { key: "blocked", label: "Blocked", tone: "neutral", detail: "new requirement" });
  assert.equal(ticketPhase(ticket()).label, "Ready to merge");
  assert.equal(ticketPhase(ticket({ mergeCandidate: { policy: "{}", blockingReason: null, quietDeadline: null } })).label, "Ready to merge");
  assert.equal(phaseSummary([blocked("policy approvals are not satisfied"), blocked("policy approvals are not satisfied"), ticket({ pullRequests: [] }), ticket({ state: "ended" })]), "2 waiting for approval · 1 working");
  assert.equal(phaseSummary([]), "No active tickets");
});

test("headlines use the latest decision and keep the full summary", () => {
  const full = "Document store precedence ".repeat(8);
  const detail = { decisions: [{ id: 2, reason: full, decidedAt: "2026-11-16T01:00:00Z" }, { id: 1, reason: "old", decidedAt: "2026-11-16T00:00:00Z" }], context: { reason: "original" } } as OperatorTicketDetail;
  const summary = ticketSummary(detail);
  assert.equal(summary.full, full); assert.ok(summary.short.length <= 120); assert.match(summary.short, /…$/);
  assert.equal(ticketSummary({ ...detail, decisions: [] }).full, "original");
});

test("budget meters use fresh token totals and highlight at 80 percent", () => {
  const meters = budgetMeters(ticket({ budgetUsage: { time: 48, turns: 16, tokens: 801, idle: 0, escalations: 0 } }));
  assert.deepEqual(meters.map(m => m.text), ["48 / 60 min", "16 / 20", "80.1% of limit"]);
  assert.ok(meters.every(m => m.warning));
  assert.ok(budgetMeters(ticket()).every(m => !m.warning));
  const unlimited = budgetMeters(ticket({ budget: { timeMinutes: 0, turnLimit: 0, tokenLimit: 0 } as OperatorTicket["budget"] }));
  assert.ok(unlimited.every(m => !m.warning && m.percent === null));
  assert.equal(unlimited[2]!.text, "No limit");
});

test("attention includes only actionable items and heat starts at 75 C", () => {
  const status = { stuckTickets: [{ ticketId: 3, cause: "no progress", detectedAt: "now" }], watchdog: { lastSuccess: false, error: "read failed" }, machine: { temperature: null as number | null } };
  const rows = attentionItems(status, [{ id: 4, state: "paused", pauseCause: "budget exhausted", budgetWarning: "tokens" }]);
  assert.deepEqual(rows.map(row => row.text), ["Stalled: no progress", "Paused: budget exhausted", "Budget warning: tokens", "Watchdog read failed: read failed"]);
  const healthy = { stuckTickets: [], watchdog: { lastSuccess: true, error: null }, machine: { temperature: 74 } };
  assert.deepEqual(attentionItems(healthy, []), []);
  assert.deepEqual(attentionItems({ ...healthy, machine: null }, []), []);
  assert.equal(attentionItems({ ...healthy, machine: { temperature: 75 } }, [])[0]!.text, "Machine hot: 75 °C");
  assert.equal(attentionItems(healthy, [{ id: 2, state: "open", pauseCause: null, budgetWarning: null, latestFailure: { kind: "assignment", summary: "failed turn", at: "now" } }])[0]!.text, "Failed: failed turn");
  assert.deepEqual(attentionItems(healthy, [{ id: 2, state: "ended", pauseCause: null, budgetWarning: "time" }]), []);
});

test("merge policy is shown as words, not stored JSON", () => {
  assert.equal(policyLabel('{"strategy":"approvals","approvals":1,"approvers":["tikazyq"]}'), "approvals: 1 from tikazyq");
  assert.equal(policyLabel('{"strategy":"unattended"}'), "unattended");
  assert.equal(policyLabel("not json"), "not json");
});

test("relative timestamps and GitHub links are safely projected", () => {
  const stamp = "2026-11-16T00:00:00Z";
  assert.equal(ageLabel(stamp, Date.parse(stamp) + 120000), "2 min ago");
  assert.equal(ageLabel(stamp, Date.parse(stamp) + 3600000), "1 hr ago");
  assert.equal(ageLabel(stamp, Date.parse(stamp) + 86400000), "1 day ago");
  assert.equal(ageLabel(stamp, Date.parse(stamp) - 1), "Just now");
  assert.equal(ageLabel(null), "No completed turn yet");
  assert.equal(localTime(stamp), new Date(stamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }));
  assert.equal(githubUrl("https://github.com/org/repo/issues/1"), "https://github.com/org/repo/issues/1");
  assert.equal(githubUrl("javascript:alert(1)"), null);
  assert.equal(githubUrl("https://evil.example/path"), null);
});

test("refresh does not overlap, retains stale data and recovers", async () => {
  let resolve!: (value: string) => void;
  let calls = 0; const successes: string[] = []; const failures: unknown[] = [];
  const refresh = createRefresh(() => { calls++; return calls === 1 ? new Promise<string>((r) => { resolve = r; }) : calls === 2 ? Promise.reject(new Error("down")) : Promise.resolve("recovered"); }, (value) => successes.push(value), (snapshot, error) => failures.push([snapshot, error]));
  const first = refresh(); await refresh(); assert.equal(calls, 1); resolve("snapshot"); await first; await refresh(); await refresh();
  assert.deepEqual(successes, ["snapshot", "recovered"]); assert.equal((failures[0] as unknown[])[0], "snapshot");
});
