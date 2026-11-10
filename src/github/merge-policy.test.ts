import assert from "node:assert/strict";
import { test } from "node:test";

import { countApprovals, effectiveReviews, mergeEligibility, type EligibilityInput } from "./merge-policy.ts";
import type { MergeEvidence, MergeReview } from "./merge-reader.ts";

const review = (id: string, state: string, commitOid = "head", author = "alice"): MergeReview => ({ id, state, submittedAt: "2026-11-10T00:00:00Z", author: { id: `U_${author}`, login: author, type: "User" }, commitOid });

function evidence(): MergeEvidence {
  return {
    pr: { number: 1, state: "OPEN", draft: false, mergeable: "MERGEABLE", mergeStateStatus: "CLEAN", reviewDecision: "APPROVED", authorLogin: "author", head: { ref: "b", sha: "head", repository: "o/r" }, base: { ref: "main", sha: "base", repository: "o/r" } },
    rollup: { oid: "head", state: "SUCCESS", contexts: [], associatedPrNumbers: [1], suites: [{ status: "COMPLETED", conclusion: "SUCCESS", workflowRun: { event: "pull_request" } }] },
    threads: [], reviews: [], rest: { pull: {}, repository: { allow_squash_merge: true }, protection: {}, rules: {} },
  };
}
function input(): EligibilityInput {
  return { evidence: evidence(), policy: { strategy: "unattended" }, expected: { head: "head", base: "base" }, local: { issueOpen: true, ticketOpen: true, pausedOrStopping: false, stalledOrFailed: false, budgetReached: false, workerIdle: true, readyReplySent: true, quietUntil: new Date("2026-11-10T00:00:00Z"), now: new Date("2026-11-10T00:10:00Z") } };
}

test("reviews reduce decisive state in order", () => {
  const reduced = effectiveReviews([review("1", "CHANGES_REQUESTED"), review("2", "COMMENTED"), review("3", "APPROVED"), review("4", "DISMISSED")]);
  assert.equal(reduced.size, 0);
  assert.equal(effectiveReviews([review("1", "APPROVED"), review("2", "CHANGES_REQUESTED")]).get("U_alice")?.state, "CHANGES_REQUESTED");
});

test("approvals count distinct current-head eligible reviewers", () => {
  const identities = new Map([
    ["U_alice", { id: "U_alice", login: "alice", permission: "write" as const, bot: false }],
    ["U_bot", { id: "U_bot", login: "bot", permission: "admin" as const, bot: true }],
  ]);
  assert.equal(countApprovals([review("1", "APPROVED"), review("2", "APPROVED", "old", "bob"), review("3", "APPROVED", "head", "bot")], "head", "author", identities, []), 1);
  assert.equal(countApprovals([review("1", "APPROVED")], "head", "author", identities, ["nobody"]), 0);
});

test("eligibility reports every blocker in design order", () => {
  const cases: [string, (value: ReturnType<typeof input>) => void][] = [
    ["local work is not mergeable", (v) => { v.local.issueOpen = false; }],
    ["Worker is not idle", (v) => { v.local.workerIdle = false; }],
    ["pull request is not clean at the captured head and base", (v) => { v.evidence.pr.draft = true; }],
    ["pull request head is from a fork", (v) => { v.evidence.pr.head.repository = "fork/r"; }],
    ["CI rollup is not successful for the captured head", (v) => { v.evidence.rollup!.state = "FAILURE"; }],
    ["no successful pull_request workflow is evidenced for this PR", (v) => { v.evidence.rollup!.suites = []; }],
    ["a check suite is not complete", (v) => { v.evidence.rollup!.suites = [
      { status: "COMPLETED", conclusion: "SUCCESS", workflowRun: { event: "pull_request" } },
      { status: "QUEUED", conclusion: null, workflowRun: { event: "pull_request" } },
    ]; }],
    ["branch requirements cannot be established", (v) => { v.evidence.rest.protection = { unavailable: "403" }; }],
    ["review requirements are not satisfied", (v) => { v.evidence.threads = [{ id: "t", resolved: false }]; }],
    ["quiet period has not elapsed", (v) => { v.local.readyReplySent = false; }],
  ];
  for (const [reason, change] of cases) { const value = input(); change(value); assert.deepEqual(mergeEligibility(value), { eligible: false, reason }); }
  assert.deepEqual(mergeEligibility(input()), { eligible: true });
  const fixtureSuites = input();
  fixtureSuites.evidence.rollup!.suites = [
    { status: "QUEUED", conclusion: null, workflowRun: null },
    { status: "COMPLETED", conclusion: "SUCCESS", workflowRun: { event: "pull_request" } },
  ];
  assert.deepEqual(mergeEligibility(fixtureSuites), { eligible: true });
});

test("approval and human policies are blockers", () => {
  const approvals = input();
  approvals.policy = { strategy: "approvals", approvals: 1, approvers: [] };
  assert.equal(mergeEligibility(approvals).eligible, false);
  const human = input(); human.policy = { strategy: "human" };
  assert.deepEqual(mergeEligibility(human), { eligible: false, reason: "waiting for a human merge" });
});
