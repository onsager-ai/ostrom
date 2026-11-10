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

export function readyPolicySentence(policy: MergePolicy): string {
  if (policy.strategy === "human") return "Ostrom is waiting for a human merge.";
  const approvals = policy.strategy === "approvals" ? ` and ${policy.approvals} eligible approval(s) on the current head` : "";
  return `Ostrom will squash-merge automatically after ten quiet minutes from this notice when all checks and policy requirements${approvals} hold.`;
}

/** The newest decisive submitted review controls each reviewer. */
export function effectiveReviews(reviews: readonly MergeReview[]): Map<string, MergeReview> {
  const effective = new Map<string, MergeReview>();
  for (const review of reviews) {
    const author = review.author;
    if (review.submittedAt === null || author === null || author.id === null) continue;
    if (review.state === "DISMISSED") { effective.delete(author.id); continue; }
    if (review.state === "APPROVED" || review.state === "CHANGES_REQUESTED") effective.set(author.id, review);
  }
  return effective;
}

export interface ApprovalIdentity { id: string; login: string; permission: "write" | "maintain" | "admin" | "read" | "none"; bot: boolean }
export function countApprovals(reviews: readonly MergeReview[], head: string, authorLogin: string, identities: ReadonlyMap<string, ApprovalIdentity>, allowlist: readonly string[]): number {
  const approved = effectiveReviews(reviews);
  const allowed = new Set(allowlist.map((login) => login.toLowerCase()));
  let count = 0;
  for (const review of approved.values()) {
    const author = review.author;
    if (review.state !== "APPROVED" || review.commitOid !== head || author === null || author.id === null) continue;
    const identity = identities.get(author.id);
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
  if (evidence.pr.head.repository !== evidence.pr.base.repository) return { eligible: false, reason: "pull request head is from a fork" };
  if (evidence.rollup === null || evidence.rollup.oid !== input.expected.head || evidence.rollup.state !== "SUCCESS") return { eligible: false, reason: "CI rollup is not successful for the captured head" };
  if (evidence.rollup.suites.some((suite) => incompleteSuite(suite))) return { eligible: false, reason: "a check suite is not complete" };
  if (!evidence.rollup.associatedPrNumbers.includes(evidence.pr.number) || !successfulWorkflow(evidence)) return { eligible: false, reason: "no successful pull_request workflow is evidenced for this PR" };
  if (unavailable(evidence.rest.protection) || unavailable(evidence.rest.rules)) return { eligible: false, reason: "branch requirements cannot be established" };
  const requirements = requiredEvidence(input);
  if (requirements !== null) return { eligible: false, reason: requirements };
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

type Json = Record<string, unknown>;
function record(value: unknown): Json { return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Json : {}; }

function successfulWorkflow(evidence: MergeEvidence): boolean {
  const rollup = evidence.rollup!;
  const latest = new Map<number, Json>();
  for (const value of rollup.suites) {
    const suite = record(value);
    const run = record(suite["workflowRun"]);
    if (suite["workflowRun"] === null) continue;
    const workflow = record(run["workflow"]);
    if (typeof workflow["databaseId"] !== "number" || typeof suite["databaseId"] !== "number" || typeof run["runAttempt"] !== "number") return false;
    const old = latest.get(workflow["databaseId"]);
    if (old === undefined || Number(old["databaseId"]) < suite["databaseId"]) latest.set(workflow["databaseId"], suite);
    else if (old["databaseId"] === suite["databaseId"]) return false;
  }
  if ([...latest.values()].some((suite) => suite["status"] !== "COMPLETED" || !["SUCCESS", "NEUTRAL", "SKIPPED"].includes(String(suite["conclusion"])))) return false;
  return [...latest.values()].some((suite) => record(suite["workflowRun"])["event"] === "pull_request" && suite["conclusion"] === "SUCCESS" &&
    rollup.contexts.some((value) => {
      const check = record(value);
      return check["__typename"] === "CheckRun" && check["status"] === "COMPLETED" && check["conclusion"] === "SUCCESS" && record(check["checkSuite"])["databaseId"] === suite["databaseId"];
    }));
}

function requiredEvidence(input: EligibilityInput): string | null {
  const { evidence } = input;
  if (evidence.rest.repository["allow_squash_merge"] !== true) return "repository does not allow squash merging";
  const protection = record(evidence.rest.protection);
  const rules = evidence.rest.rules;
  if (!Array.isArray(rules)) return "active branch rules cannot be established";
  const checks: { name: string; app: number | null }[] = [];
  let approvals = 0;
  const reviewRequirements = (value: unknown): string | null => {
    const review = record(value);
    // Code-owner selection and last-push identity need evidence this reader does not expose.
    if (review["require_code_owner_reviews"] === true || review["require_last_push_approval"] === true) return "unsupported review protection requires operator action";
    approvals = Math.max(approvals, Number(review["required_approving_review_count"] ?? 0));
    return null;
  };
  const classicReview = reviewRequirements(protection["required_pull_request_reviews"]);
  if (classicReview !== null) return classicReview;
  const classic = record(protection["required_status_checks"]);
  if (classic["strict"] === true && evidence.pr.mergeStateStatus !== "CLEAN") return "base must be up to date";
  const append = (value: unknown, nameKey: string, appKey: string): boolean => {
    if (!Array.isArray(value)) return value === undefined;
    for (const item of value) {
      const row = record(item);
      const name = row[nameKey];
      if (typeof name !== "string" || (row[appKey] !== undefined && row[appKey] !== null && typeof row[appKey] !== "number")) return false;
      checks.push({ name, app: typeof row[appKey] === "number" && Number(row[appKey]) >= 0 ? row[appKey] as number : null });
    }
    return true;
  };
  if (!append(classic["checks"], "context", "app_id")) return "required checks cannot be established";
  if (Array.isArray(classic["contexts"])) for (const name of classic["contexts"]) {
    if (typeof name !== "string") return "required check context is invalid";
    if (!checks.some((check) => check.name === name)) checks.push({ name, app: null });
  }
  for (const value of rules) {
    const rule = record(value);
    const params = record(rule["parameters"]);
    switch (rule["type"]) {
      case "required_status_checks":
        if (!append(params["required_status_checks"], "context", "integration_id")) return "required ruleset checks cannot be established";
        break;
      case "pull_request": {
        const reason = reviewRequirements(params);
        if (reason !== null) return reason;
        if (Array.isArray(params["allowed_merge_methods"]) && !params["allowed_merge_methods"].includes("squash")) return "rules prohibit squash merging";
        break;
      }
      case "non_fast_forward": case "deletion": case "required_linear_history": break;
      default: return `unsupported branch rule ${String(rule["type"])} requires operator action`;
    }
  }
  for (const value of evidence.rollup!.contexts) {
    const check = record(value);
    if (check["__typename"] === "CheckRun") {
      if (check["status"] !== "COMPLETED" || !["SUCCESS", "NEUTRAL", "SKIPPED"].includes(String(check["conclusion"]))) return "a head check is not successful";
    } else if (check["__typename"] !== "StatusContext" || check["state"] !== "SUCCESS") return "a head status is not successful";
  }
  for (const required of checks) {
    if (!evidence.rollup!.contexts.some((value) => {
      const check = record(value);
      return check["__typename"] === "CheckRun" ? check["name"] === required.name && (required.app === null || record(record(check["checkSuite"])["app"])["databaseId"] === required.app) :
        check["__typename"] === "StatusContext" && check["context"] === required.name && required.app === null;
    })) return `required check ${required.name} is missing or has the wrong app`;
  }
  if (approvals > 0 && (input.approvals === undefined || countApprovals(evidence.reviews, input.expected.head, input.approvals.authorLogin, input.approvals.identities, []) < approvals)) return "repository approval requirements are not satisfied";
  return null;
}

function incompleteSuite(value: unknown): boolean {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const suite = value as Record<string, unknown>;
  const workflow = suite["workflowRun"];
  return typeof workflow === "object" && workflow !== null && !Array.isArray(workflow) && suite["status"] !== "COMPLETED";
}

function unavailable(value: unknown): boolean {
  return typeof value === "object" && value !== null && !Array.isArray(value) && "unavailable" in value;
}
