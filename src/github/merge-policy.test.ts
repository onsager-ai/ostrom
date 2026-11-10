import assert from "node:assert/strict";
import { test } from "node:test";
import { readFileSync } from "node:fs";

import { countApprovals, effectiveReviews, mergeEligibility, type EligibilityInput } from "./merge-policy.ts";
import { readMergeEvidence, type MergeEvidence, type MergeReview } from "./merge-reader.ts";

const review = (id: string, state: string, commitOid = "head", author = "alice"): MergeReview => ({ id, state, submittedAt: "2026-11-10T00:00:00Z", author: { id: `U_${author}`, login: author, type: "User" }, commitOid });

function evidence(): MergeEvidence {
  return {
    pr: { number: 1, state: "OPEN", draft: false, mergeable: "MERGEABLE", mergeStateStatus: "CLEAN", reviewDecision: "APPROVED", authorLogin: "author", head: { ref: "b", sha: "head", repository: "o/r" }, base: { ref: "main", sha: "base", repository: "o/r" } },
    rollup: { oid: "head", state: "SUCCESS", contexts: [{ __typename: "CheckRun", databaseId: 10, name: "check", status: "COMPLETED", conclusion: "SUCCESS", checkSuite: { databaseId: 1, app: { databaseId: 15368 } } }], associatedPrNumbers: [1], suites: [{ databaseId: 1, status: "COMPLETED", conclusion: "SUCCESS", repository: { nameWithOwner: "o/r" }, matchingPullRequests: { nodes: [{ number: 1 }], pageInfo: { hasNextPage: false } }, workflowRun: { event: "pull_request", runAttempt: 1, workflow: { databaseId: 1 } } }] },
    threads: [], reviews: [], rest: { pull: {}, repository: { allow_squash_merge: true }, protection: {}, rules: [] },
  };
}
function input(): EligibilityInput {
  return { evidence: evidence(), policy: { strategy: "unattended" }, expected: { head: "head", base: "base" }, local: { issueOpen: true, ticketOpen: true, pausedOrStopping: false, stalledOrFailed: false, budgetReached: false, workerIdle: true, readyReplySent: true, quietUntil: new Date("2026-11-10T00:00:00Z"), now: new Date("2026-11-10T00:10:00Z") } };
}

const unprotected = JSON.parse(readFileSync(new URL("./fixtures/merge-reader-unprotected.json", import.meta.url), "utf8")) as {
  http_status: number; body: { message: string; status: string }; stderr: string;
};

// Exercise the actual reader error path, then apply its normalized requirements to otherwise
// eligible evidence. The recorded sandbox PR is closed and is not itself a merge candidate.
async function requirements(protectionError: string, rules: unknown = []) {
  const recorded = (name: string): unknown => JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8"));
  const observed = await readMergeEvidence(async (args) => {
    if (args[1] === "graphql") return JSON.stringify(recorded("merge-reader-graphql.json"));
    const path = args.at(-1)!;
    if (path.endsWith("/protection")) throw new Error(protectionError);
    if (path.includes("/rules/")) {
      if (rules instanceof Error) throw rules;
      return JSON.stringify(rules);
    }
    if (path.endsWith("/pulls/8")) return JSON.stringify(recorded("merge-reader-pr.json"));
    return JSON.stringify(recorded("merge-reader-repository.json"));
  }, "onsager-ai/ostrom-sandbox", 8);
  const value = input();
  value.evidence.rest.protection = observed.rest.protection;
  value.evidence.rest.rules = observed.rest.rules;
  return value;
}

test("recorded Branch not protected 404 passes classic protection while rules and other requirements still apply", async () => {
  assert.equal(unprotected.http_status, 404);
  assert.equal(unprotected.body.status, "404");
  assert.equal(unprotected.stderr, `gh: ${unprotected.body.message} (HTTP ${unprotected.http_status})`);
  const error = `gh api failed: ${unprotected.stderr}`;
  const value = await requirements(error);
  assert.deepEqual(value.evidence.rest.protection, {});
  assert.deepEqual(mergeEligibility(value), { eligible: true });
  const ruled = await requirements(error, [{ type: "required_status_checks", parameters: { required_status_checks: [{ context: "missing", integration_id: 15368 }] } }]);
  assert.deepEqual(mergeEligibility(ruled), { eligible: false, reason: "required check missing is missing or has the wrong app" });
  const deniedRules = await requirements(error, new Error(error));
  assert.deepEqual(mergeEligibility(deniedRules), { eligible: false, reason: "branch requirements cannot be established" });
  value.evidence.rest.repository["allow_squash_merge"] = false;
  assert.deepEqual(mergeEligibility(value), { eligible: false, reason: "repository does not allow squash merging" });
});

for (const error of [
  "gh api failed: gh: Not Found (HTTP 404)",
  "gh api failed: gh: Resource not accessible by integration (HTTP 403)",
  "gh api failed: gh: Branch not protected (HTTP 403)",
  "gh api failed: gh: Branch not protected (HTTP 500)",
  "gh api failed: socket hang up",
]) test(`protection read remains blocking for ${error}`, async () => {
  const value = await requirements(error);
  assert.deepEqual(value.evidence.rest.protection, { unavailable: error });
  assert.deepEqual(mergeEligibility(value), { eligible: false, reason: "branch requirements cannot be established" });
});

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
    ["a check suite is not complete", (v) => { v.evidence.rollup!.suites.push(
      { databaseId: 2, status: "QUEUED", conclusion: null, workflowRun: { event: "pull_request", runAttempt: 1, workflow: { databaseId: 2 } } },
    ); }],
    ["branch requirements cannot be established", (v) => { v.evidence.rest.protection = { unavailable: "403" }; }],
    ["review requirements are not satisfied", (v) => { v.evidence.threads = [{ id: "t", resolved: false }]; }],
    ["quiet period has not elapsed", (v) => { v.local.readyReplySent = false; }],
  ];
  for (const [reason, change] of cases) { const value = input(); change(value); assert.deepEqual(mergeEligibility(value), { eligible: false, reason }); }
  assert.deepEqual(mergeEligibility(input()), { eligible: true });
  const fixtureSuites = input();
  fixtureSuites.evidence.rollup!.suites.push({ databaseId: 2, status: "QUEUED", conclusion: null, workflowRun: null });
  assert.deepEqual(mergeEligibility(fixtureSuites), { eligible: true });
});

test("checks and protection requirements are enforced independently of CLEAN", () => {
  const cases: [string, (value: EligibilityInput) => void][] = [
    ["repository does not allow squash merging", (v) => { v.evidence.rest.repository["allow_squash_merge"] = false; }],
    ["required check missing is missing or has the wrong app", (v) => { v.evidence.rest.protection = { required_status_checks: { checks: [{ context: "missing", app_id: 15368 }] } }; }],
    ["required check check is missing or has the wrong app", (v) => { v.evidence.rest.protection = { required_status_checks: { checks: [{ context: "check", app_id: 999 }] } }; }],
    ["repository approval requirements are not satisfied", (v) => { v.evidence.rest.protection = { required_pull_request_reviews: { required_approving_review_count: 1 } }; }],
    ["unsupported review protection requires operator action", (v) => { v.evidence.rest.protection = { required_pull_request_reviews: { require_code_owner_reviews: true } }; }],
    ["unsupported branch rule merge_queue requires operator action", (v) => { v.evidence.rest.rules = [{ type: "merge_queue" }]; }],
    ["rules prohibit squash merging", (v) => { v.evidence.rest.rules = [{ type: "pull_request", parameters: { allowed_merge_methods: ["merge"] } }]; }],
    ["a head check is not successful", (v) => { v.evidence.rollup!.contexts.push({ __typename: "CheckRun", status: "QUEUED", conclusion: null }); }],
  ];
  for (const [reason, change] of cases) {
    const value = input(); change(value);
    assert.deepEqual(mergeEligibility(value), { eligible: false, reason });
  }
  const required = input();
  required.evidence.rest.rules = [{ type: "required_status_checks", parameters: { required_status_checks: [{ context: "check", integration_id: 15368 }] } }];
  assert.deepEqual(mergeEligibility(required), { eligible: true });
});

test("newer failed workflow cannot be masked by old success and suite provenance must match this PR", () => {
  const failed = input();
  failed.evidence.rollup!.suites.push({ databaseId: 2, status: "COMPLETED", conclusion: "FAILURE", workflowRun: { event: "pull_request", runAttempt: 2, workflow: { databaseId: 1 } } });
  assert.equal(mergeEligibility(failed).eligible, false);
  for (const suite of [
    { databaseId: 1, status: "COMPLETED", conclusion: "SUCCESS", workflowRun: { event: "push", runAttempt: 1, workflow: { databaseId: 1 } } },
    { databaseId: 1, status: "COMPLETED", conclusion: "SUCCESS", repository: { nameWithOwner: "other/r" }, matchingPullRequests: { nodes: [{ number: 1 }], pageInfo: { hasNextPage: false } }, workflowRun: { event: "pull_request", runAttempt: 1, workflow: { databaseId: 1 } } },
    { databaseId: 1, status: "COMPLETED", conclusion: "SUCCESS", repository: { nameWithOwner: "o/r" }, matchingPullRequests: { nodes: [{ number: 99 }], pageInfo: { hasNextPage: false } }, workflowRun: { event: "pull_request", runAttempt: 1, workflow: { databaseId: 1 } } },
    { databaseId: 1, status: "COMPLETED", conclusion: "SUCCESS", repository: { nameWithOwner: "o/r" }, matchingPullRequests: { nodes: [{ number: 1 }], pageInfo: { hasNextPage: true } }, workflowRun: { event: "pull_request", runAttempt: 1, workflow: { databaseId: 1 } } },
  ]) {
    const value = input(); value.evidence.rollup!.suites = [suite];
    assert.equal(mergeEligibility(value).eligible, false);
  }
});

test("approval and human policies are blockers", () => {
  const approvals = input();
  approvals.policy = { strategy: "approvals", approvals: 1, approvers: [] };
  assert.equal(mergeEligibility(approvals).eligible, false);
  const human = input(); human.policy = { strategy: "human" };
  assert.deepEqual(mergeEligibility(human), { eligible: false, reason: "waiting for a human merge" });
});
