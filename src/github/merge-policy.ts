import type { MergeEvidence, MergeReview } from "./merge-reader.ts";

export type MergePolicy =
  | { strategy: "unattended" }
  | { strategy: "human" }
  | { strategy: "approvals"; approvals: number; approvers: readonly string[] };

export function loadMergePolicy(env: Record<string, string | undefined>): MergePolicy {
  const strategy = env["OSTROM_MERGE_STRATEGY"]?.trim() || "unattended";
  const rawApprovals = env["OSTROM_MERGE_APPROVALS"]?.trim() ?? "";
  const rawApprovers = env["OSTROM_MERGE_APPROVERS"]?.trim() ?? "";
  if (strategy !== "unattended" && strategy !== "human" && strategy !== "approvals") {
    throw new Error(`OSTROM_MERGE_STRATEGY must be unattended, human, or approvals, got ${JSON.stringify(strategy)}`);
  }
  if (strategy !== "approvals") {
    if (rawApprovals !== "" || rawApprovers !== "") throw new Error("OSTROM_MERGE_APPROVALS and OSTROM_MERGE_APPROVERS require OSTROM_MERGE_STRATEGY=approvals");
    return { strategy };
  }
  if (!/^[1-9]\d*$/.test(rawApprovals)) throw new Error("OSTROM_MERGE_APPROVALS must be a positive whole number for approvals strategy");
  const approvers = rawApprovers === "" ? [] : rawApprovers.split(",").map((login) => login.trim());
  if (approvers.some((login) => login === "")) throw new Error("OSTROM_MERGE_APPROVERS must not contain empty logins");
  const unique = new Set(approvers.map((login) => login.toLowerCase()));
  if (unique.size !== approvers.length) throw new Error("OSTROM_MERGE_APPROVERS must not contain duplicate logins");
  return { strategy, approvals: Number(rawApprovals), approvers };
}

/** The newest decisive submitted review controls each reviewer. */
export function effectiveReviews(reviews: readonly MergeReview[]): Map<string, MergeReview> {
  const effective = new Map<string, MergeReview>();
  for (const review of reviews) {
    if (review.submittedAt === null || review.author?.id === null) continue;
    if (review.state === "DISMISSED") { effective.delete(review.author.id); continue; }
    if (review.state === "APPROVED" || review.state === "CHANGES_REQUESTED") effective.set(review.author.id, review);
  }
  return effective;
}

export interface ApprovalIdentity { id: string; login: string; permission: "write" | "maintain" | "admin" | "read" | "none"; bot: boolean }
export function countApprovals(reviews: readonly MergeReview[], head: string, authorLogin: string, identities: ReadonlyMap<string, ApprovalIdentity>, allowlist: readonly string[]): number {
  const approved = effectiveReviews(reviews);
  const allowed = new Set(allowlist.map((login) => login.toLowerCase()));
  let count = 0;
  for (const review of approved.values()) {
    if (review.state !== "APPROVED" || review.commitOid !== head || review.author?.id === null) continue;
    const identity = identities.get(review.author.id);
    if (identity === undefined || identity.bot || identity.login.toLowerCase() === authorLogin.toLowerCase()) continue;
    if (identity.permission !== "write" && identity.permission !== "maintain" && identity.permission !== "admin") continue;
    if (allowed.size > 0 && !allowed.has(identity.login.toLowerCase())) continue;
    count++;
  }
  return count;
}

export interface EligibilityInput {
  evidence: MergeEvidence;
  policy: MergePolicy;
  expected: { head: string; base: string };
  local: { issueOpen: boolean; ticketOpen: boolean; pausedOrStopping: boolean; stalledOrFailed: boolean; budgetReached: boolean; workerIdle: boolean; readyReplySent: boolean; quietUntil: Date | null; now: Date };
  approvals?: { authorLogin: string; identities: ReadonlyMap<string, ApprovalIdentity> };
}

/** Returns the first blocking reason in the order specified by docs/merging.md. */
export function mergeEligibility(input: EligibilityInput): { eligible: true } | { eligible: false; reason: string } {
  const { evidence, local } = input;
  if (!local.issueOpen || !local.ticketOpen || local.pausedOrStopping || local.stalledOrFailed || local.budgetReached) return { eligible: false, reason: "local work is not mergeable" };
  if (!local.workerIdle) return { eligible: false, reason: "Worker is not idle" };
  if (evidence.pr.state !== "OPEN" || evidence.pr.draft || evidence.pr.head.sha !== input.expected.head || evidence.pr.base.sha !== input.expected.base || evidence.pr.mergeable !== "MERGEABLE" || evidence.pr.mergeStateStatus !== "CLEAN") return { eligible: false, reason: "pull request is not clean at the captured head and base" };
  if (evidence.rollup === null || evidence.rollup.oid !== input.expected.head || evidence.rollup.state !== "SUCCESS") return { eligible: false, reason: "CI rollup is not successful for the captured head" };
  if (!evidence.rollup.associatedPrNumbers.includes(evidence.pr.number) || !evidence.rollup.suites.some((suite) => successfulPullRequestSuite(suite))) return { eligible: false, reason: "no successful pull_request workflow is evidenced for this PR" };
  if (evidence.rollup.suites.some((suite) => incompleteSuite(suite))) return { eligible: false, reason: "a check suite is not complete" };
  if (unavailable(evidence.rest.protection) || unavailable(evidence.rest.rules)) return { eligible: false, reason: "branch requirements cannot be established" };
  const effective = effectiveReviews(evidence.reviews);
  if ([...effective.values()].some((review) => review.state === "CHANGES_REQUESTED") || evidence.threads.some((thread) => !thread.resolved) || evidence.pr.reviewDecision === "CHANGES_REQUESTED" || evidence.pr.reviewDecision === "REVIEW_REQUIRED") return { eligible: false, reason: "review requirements are not satisfied" };
  if (input.policy.strategy === "approvals") {
    const approval = input.approvals;
    if (approval === undefined || countApprovals(evidence.reviews, input.expected.head, approval.authorLogin, approval.identities, input.policy.approvers) < input.policy.approvals) return { eligible: false, reason: "policy approvals are not satisfied" };
  }
  if (input.policy.strategy === "human") return { eligible: false, reason: "waiting for a human merge" };
  if (!local.readyReplySent || local.quietUntil === null || local.quietUntil > local.now) return { eligible: false, reason: "quiet period has not elapsed" };
  return { eligible: true };
}

function successfulPullRequestSuite(value: unknown): boolean {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const suite = value as Record<string, unknown>;
  const workflow = suite["workflowRun"];
  return suite["status"] === "COMPLETED" && suite["conclusion"] === "SUCCESS" &&
    typeof workflow === "object" && workflow !== null && !Array.isArray(workflow) && (workflow as Record<string, unknown>)["event"] === "pull_request";
}

function incompleteSuite(value: unknown): boolean {
  return typeof value === "object" && value !== null && !Array.isArray(value) && (value as Record<string, unknown>)["status"] !== "COMPLETED";
}

function unavailable(value: unknown): boolean {
  return typeof value === "object" && value !== null && !Array.isArray(value) && "unavailable" in value;
}
