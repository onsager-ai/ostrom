import assert from "node:assert/strict";
import { test } from "node:test";
import { ageLabel, attentionItems, createRefresh, githubUrl, policyLabel } from "./dashboard-view.js";

test("merge policy is shown as words, not stored JSON", () => {
  assert.equal(policyLabel('{"strategy":"approvals","approvals":1,"approvers":["tikazyq"]}'), "approvals: 1 from tikazyq");
  assert.equal(policyLabel('{"strategy":"unattended"}'), "unattended");
  assert.equal(policyLabel("not json"), "not json");
});

test("attention list includes stated operator causes and unavailable temperature", () => {
  const rows = attentionItems({ stuckTickets: [{ ticketId: 3, cause: "no progress", detectedAt: "now" }], watchdog: { lastSuccess: false, error: "read failed" }, machine: { temperature: null } }, [{ id: 4, state: "paused", pauseCause: "budget exhausted", budgetWarning: "tokens" }]);
  assert.deepEqual(rows.map((row) => row.text), ["Ticket 3: stuck — no progress (now)", "Ticket 4: paused — budget exhausted", "Ticket 4: budget warning (tokens)", "Watchdog read failed: read failed", "Temperature: unavailable"]);
});

test("timestamps and GitHub links are safely projected", () => {
  assert.match(ageLabel("2026-11-16T00:00:00Z", Date.parse("2026-11-16T00:01:00Z")), /\(1m\)$/);
  assert.equal(githubUrl("https://github.com/org/repo/issues/1"), "https://github.com/org/repo/issues/1");
  assert.equal(githubUrl("javascript:alert(1)"), null);
  assert.equal(githubUrl("https://evil.example/path"), null);
});

test("refresh does not overlap and retains the last successful snapshot", async () => {
  let resolve!: (value: string) => void;
  let calls = 0; const successes: string[] = []; const failures: unknown[] = [];
  const refresh = createRefresh(() => { calls++; return calls === 1 ? new Promise<string>((r) => { resolve = r; }) : Promise.reject(new Error("down")); }, (value) => successes.push(value), (snapshot, error) => failures.push([snapshot, error]));
  const first = refresh(); await refresh(); assert.equal(calls, 1); resolve("snapshot"); await first; await refresh();
  assert.deepEqual(successes, ["snapshot"]); assert.equal((failures[0] as unknown[])[0], "snapshot");
});
