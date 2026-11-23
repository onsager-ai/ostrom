// Deterministic unattended merge and restart reconciliation. The Router never authorizes a merge.
import { budgetReached } from "../budget/budgets.ts";
import { isApprovalOnly } from "./approval.ts";
import type { Workers } from "../worker/workers.ts";
import type { Assignment, MergeCandidate, Store } from "../store/store.ts";
import { queueStatusCard } from "./cards.ts";
import { ghMergeGet, ghSquashMerge, type GhRunner } from "./gh.ts";
import { mergeEligibility, readyPolicySentence, type ApprovalIdentity, type MergePolicy } from "./merge-policy.ts";
import { failedChecks, readMergeEvidence, reviewersToReRequest, type MergeEvidence } from "./merge-reader.ts";

const QUIET_MS = 10 * 60_000;
const RETRY_MS = 30_000;

type Json = Record<string, unknown>;
interface Observation { evidence: MergeEvidence; issueOpen: boolean; activity: string; owned: boolean; unjudgedApprovalCommentary: boolean; }
export interface MergePassOptions {
  store: Store; workers: Workers; gh: GhRunner; repo: string; policy: MergePolicy; base?: string; now?: () => Date;
  /** Failed prerequisite passes suppress sends, but reconciliation must still run. */
  allowSend?: boolean;
  /** Dedicated App bot resolved at startup; shared ambient actors cannot prove attribution. */
  appBotId?: number;
  /** Intake and handle newly arrived controls before the final direct evidence read. */
  refreshBeforeSend?: () => Promise<void>;
  log?: (line: string) => void;
}
export interface MergePassResult {
  merged: number[]; waiting: { assignmentId: number; reason: string }[];
  reconciled: number[]; failed: { assignmentId: number; error: string }[];
}

export async function mergePass(options: MergePassOptions): Promise<MergePassResult> {
  const result: MergePassResult = { merged: [], waiting: [], reconciled: [], failed: [] };
  for (const attempt of options.store.unresolvedMergeAttempts(options.repo)) {
    const candidate = options.store.mergeCandidateById(attempt.candidateId);
    try {
      const evidence = await readMergeEvidence(options.gh, options.repo, attempt.prNumber);
      if (evidence.pr.state === "MERGED") {
        // A stored positive response is evidence of our request; a matching shared actor is not.
        if (attempt.responseSha !== null && attempt.responseSha !== mergeSha(evidence)) throw new Error("confirmed merge SHA differs from response");
        complete(options, candidate, evidence, attempt.id, attempt.responseSha === null ? "unknown" : "ostrom");
      } else {
        options.store.finishMergeAttempt(attempt.id, "failed", {
          error: "merge was not confirmed", nextRetryAt: retry(options, attempt.attemptNumber),
        });
        options.workers.release(candidate.assignmentId);
      }
      result.reconciled.push(attempt.id);
    } catch (error) { result.failed.push({ assignmentId: candidate.assignmentId, error: message(error) }); }
  }
  const unresolved = new Set(options.store.unresolvedMergeAttempts(options.repo).map((attempt) => attempt.candidateId));
  for (const assignment of options.store.runningAssignments()) {
    const candidate = options.store.mergeCandidateForAssignment(assignment.id);
    const unsettled = candidate !== null && unresolved.has(candidate.id);
    if (!unsettled && options.store.reservationHeld(assignment.id)) options.workers.release(assignment.id);
    try {
      await recordDispatchedRuns(options, assignment);
      if (unsettled) continue;
      const pr = options.store.ticketPullRequest(assignment.ticketId);
      if (pr === null) deliverDispatchedRun(options, assignment.id);
      else await consider(options, assignment.id, pr.number, result);
    } catch (error) { result.failed.push({ assignmentId: assignment.id, error: message(error) }); }
  }
  return result;
}

async function consider(options: MergePassOptions, assignmentId: number, prNumber: number, result: MergePassResult): Promise<void> {
  let observation = await observe(options, assignmentId, prNumber);
  let candidate = candidateFor(options, assignmentId, prNumber, observation);
  if (observation.evidence.pr.state === "MERGED" && observation.owned) {
    complete(options, candidate, observation.evidence, undefined, "external");
    result.merged.push(assignmentId);
    return;
  }
  if (observation.issueOpen) deliverDispatchedRun(options, assignmentId);
  await reRequestReviews(options, assignmentId, observation);
  // New PRs wait for the Router's pr_opened reply. Only an old, untracked opened reply needs a
  // separate first-enable notice: reusing its ID would never produce a fresh delivery timestamp.
  const openedId = `outcome-a${assignmentId}-pr${prNumber}-opened`;
  const tracked = options.store.readyReplyId(assignmentId);
  const lostDeliveryTracking = tracked === openedId && options.store.readyReplySentAt(assignmentId) === null && options.store.replySentAt(openedId) !== null;
  if ((tracked === null && options.store.hasReply(openedId)) || lostDeliveryTracking) {
    const id = `ready-a${assignmentId}-pr${prNumber}`;
    const context = options.store.ticketContext(options.store.getAssignment(assignmentId).ticketId);
    options.store.enqueueReply({ id, threadId: context.thread.id,
      body: `Pull request ready: https://github.com/${options.repo}/pull/${prNumber}\n\n${readyPolicySentence(options.policy)}` });
    options.store.trackReadyReply(assignmentId, id, lostDeliveryTracking);
    candidate = candidateFor(options, assignmentId, prNumber, observation);
  }
  const wait = (reason: string) => {
    const { pr } = observation.evidence;
    if (pr.state === "OPEN" && (pr.mergeStateStatus === "DIRTY" || pr.mergeable === "CONFLICTING")) {
      reason = `Conflicts with ${pr.base.ref}; the Worker needs to merge ${pr.base.ref}.`;
      deliverConflictTurn(options, assignmentId, observation);
    } else if (pr.state === "OPEN") {
      deliverCiFixTurn(options, assignmentId, observation);
    }
    saveReason(options, candidate, reason);
    result.waiting.push({ assignmentId, reason });
  };
  if (options.allowSend === false) { wait("an earlier prerequisite pass failed"); return; }
  if (observation.evidence.pr.state === "OPEN" &&
      (observation.evidence.pr.mergeStateStatus === "DIRTY" || observation.evidence.pr.mergeable === "CONFLICTING")) {
    wait("pull request conflicts with its base");
    return;
  }
  const retryAt = options.store.mergeRetryAt(options.repo, prNumber);
  if (retryAt !== null && Date.parse(retryAt) > clock(options).getTime()) { wait(`merge retry after ${retryAt}`); return; }
  const eligibility = await eligibilityFor(options, assignmentId, observation, candidate);
  if (!eligibility.eligible) { wait(eligibility.reason); return; }

  await options.refreshBeforeSend?.();
  observation = await observe(options, assignmentId, prNumber);
  await reRequestReviews(options, assignmentId, observation);
  candidate = candidateFor(options, assignmentId, prNumber, observation);
  if (observation.evidence.pr.state === "MERGED" && observation.owned) {
    complete(options, candidate, observation.evidence, undefined, "external");
    result.merged.push(assignmentId);
    return;
  }
  const actor = options.appBotId === undefined ? object(await ghMergeGet(options.gh, "user")) : { id: options.appBotId };
  if (typeof actor["id"] !== "number") throw new Error("GitHub did not identify the authenticated merge actor");
  const final = await eligibilityFor(options, assignmentId, observation, candidate);
  if (!final.eligible) { wait(final.reason); return; }
  // No await between the final local check, reservation, durable attempt and HTTP send. Controls
  // accepted locally before this boundary win. Controls after it report the in-flight reservation.
  if (!options.store.reserveAssignment(assignmentId)) { wait("Worker is not idle"); return; }
  let attempt;
  try { attempt = options.store.createMergeAttempt(candidate, actor["id"]); }
  catch (error) { options.workers.release(assignmentId); throw error; }
  let response: Json;
  try {
    response = object(await ghSquashMerge(options.gh, options.repo, prNumber, candidate.headSha));
  } catch (error) {
    const errorText = message(error);
    const status = ghHttpStatus(errorText);
    // 408 and 429 can be returned by an intermediary after forwarding the request. Reconcile
    // these, all 5xx, timeouts and transport failures before allowing another request or turn.
    const definite = status !== null && status >= 400 && status < 500 && status !== 408 && status !== 429;
    options.store.finishMergeAttempt(attempt.id, definite ? "failed" : "unknown", {
      error: errorText, ...(definite ? { nextRetryAt: retry(options, attempt.attemptNumber) } : {}),
    });
    if (definite) options.workers.release(assignmentId);
    saveReason(options, candidate, definite ? errorText : "merge result unknown; awaiting reconciliation");
    result.failed.push({ assignmentId, error: errorText });
    return;
  }
  if (response["merged"] === false) {
    const error = String(response["message"] ?? "GitHub refused merge");
    options.store.finishMergeAttempt(attempt.id, "failed", { error, nextRetryAt: retry(options, attempt.attemptNumber) });
    options.workers.release(assignmentId);
    saveReason(options, candidate, error);
    result.failed.push({ assignmentId, error });
    return;
  }
  // A malformed success body is ambiguous too. Store an acknowledged SHA before the confirming
  // read, so a crash there preserves attribution without prematurely marking the request confirmed.
  const sha = response["merged"] === true && typeof response["sha"] === "string" && response["sha"] !== "" ? response["sha"] : null;
  options.store.finishMergeAttempt(attempt.id, "unknown", sha === null ? { error: "invalid merge response" } : { sha });
  try {
    const confirmed = await readMergeEvidence(options.gh, options.repo, prNumber);
    if (confirmed.pr.state !== "MERGED") throw new Error("merge response could not be confirmed");
    if (sha !== null && mergeSha(confirmed) !== sha) throw new Error("confirmed merge SHA differs from response");
    complete(options, candidate, confirmed, attempt.id, sha === null ? "unknown" : "ostrom");
    result.merged.push(assignmentId);
  } catch (error) { result.failed.push({ assignmentId, error: message(error) }); }
}

async function reRequestReviews(options: MergePassOptions, assignmentId: number, observation: Observation): Promise<void> {
  const { evidence } = observation;
  const { pr } = evidence;
  if (options.allowSend === false || !observation.owned || pr.state !== "OPEN" || pr.draft ||
      !options.store.assignmentWorkIdle(assignmentId, { ignoreQueuedDispatchedRuns: true }) ||
      !options.store.workerTurnPushedHead(assignmentId, pr.head.sha)) return;
  const { ticketId } = options.store.getAssignment(assignmentId);
  for (const reviewer of reviewersToReRequest(evidence)) {
    const ticket = options.store.getTicket(ticketId);
    if (ticket.state !== "open" || ticket.stopRequestedAt !== null) return;
    if (!options.store.recordReviewRequest(options.repo, pr.number, pr.head.sha, reviewer)) continue;
    try {
      await options.gh([
        "api", "--method", "POST", `repos/${options.repo}/pulls/${pr.number}/requested_reviewers`,
        "-f", `reviewers[]=${reviewer}`,
      ]);
    } catch (error) {
      options.log?.(`pull request #${pr.number}: could not re-request review from ${reviewer}: ${message(error)}`);
    }
  }
}

function automaticTurnAllowed(
  options: MergePassOptions, assignmentId: number, ignoreQueuedDispatchedRuns = false,
): boolean {
  const { store } = options;
  const assignment = store.getAssignment(assignmentId);
  const ticket = store.getTicket(assignment.ticketId);
  if (options.allowSend === false || assignment.state !== "running" || assignment.outcome === "failed" ||
      ticket.state !== "open" || ticket.stopRequestedAt !== null ||
      budgetReached(store.ticketBudget(ticket.id), clock(options)) !== null ||
      store.unansweredStalls().some((stall) => stall.assignmentId === assignmentId) ||
      store.runningAssignments().some((other) => other.ticketId === ticket.id &&
        !store.assignmentWorkIdle(other.id, { ignoreQueuedDispatchedRuns }))) return false;
  return true;
}

async function recordDispatchedRuns(options: MergePassOptions, assignment: Assignment): Promise<void> {
  if (assignment.branch === null || assignment.startedAt === null) return;
  const path = `repos/${options.repo}/actions/runs`;
  const branch = encodeURIComponent(assignment.branch);
  const response = object(await ghMergeGet(options.gh, `${path}?branch=${branch}&status=completed&per_page=20`));
  const startedAt = Date.parse(assignment.startedAt);
  for (const run of rows(response, "workflow_runs")) {
    // Only runs a Worker started on its branch; push and pull_request runs are the PR's own CI.
    if (run["event"] !== "workflow_dispatch") continue;
    const { id, name, event, conclusion, updated_at: updatedAt, head_sha: head, html_url: url } = run;
    if (typeof id !== "number" || typeof event !== "string" || typeof name !== "string" || typeof conclusion !== "string" ||
        typeof head !== "string" || typeof url !== "string" || typeof updatedAt !== "string" || !Number.isFinite(Date.parse(updatedAt))) {
      throw new Error("GitHub returned incomplete workflow run evidence");
    }
    // The completed runs API exposes updated_at, rather than a separate completion timestamp.
    if (Date.parse(updatedAt) <= startedAt) continue;
    const deliveryKey = `dispatched-run-a${assignment.id}-${id}`;
    if (options.store.queuedTurn(assignment.id, deliveryKey) !== null) continue;
    let failedJobs = "";
    if (conclusion === "failure") {
      const jobs = rows(object(await ghMergeGet(options.gh, `${path}/${id}/jobs?per_page=100`)), "jobs");
      const names = jobs.flatMap((job) => {
        if (job["conclusion"] !== "failure") return [];
        if (typeof job["name"] !== "string") throw new Error("GitHub failed job is missing its name");
        return [job["name"]];
      });
      if (names.length > 0) failedJobs = `\nFailed jobs:\n${names.map((job) => `- ${job}`).join("\n")}`;
    }
    // Record while blocked so delivery does not depend on the run staying in GitHub's latest 20.
    const current = options.store.getAssignment(assignment.id);
    if (current.state !== "running" || current.outcome === "failed" || options.store.getTicket(current.ticketId).state === "ended") return;
    options.store.enqueueTurn({ assignmentId: assignment.id, deliveryKey, kind: "dispatched_run",
      prompt: `Workflow ${name} completed with conclusion ${conclusion} at head ${head}.\nRun and logs: ${url}${failedJobs}\n\n` +
        "Read the run results and follow up on the ticket as needed." });
  }
}

function rows(response: Json, key: string): Json[] {
  const value = response[key];
  if (!Array.isArray(value)) throw new Error(`GitHub response is missing ${key}`);
  return value.map(object);
}

function deliverDispatchedRun(options: MergePassOptions, assignmentId: number): void {
  const turn = options.store.unfinishedTurns(assignmentId).find((item) => item.kind === "dispatched_run" && item.state === "queued");
  if (turn === undefined) return;
  const allowed = () => automaticTurnAllowed(options, assignmentId, true);
  if (allowed()) options.workers.wakeDispatchedRun(assignmentId, turn.deliveryKey, allowed);
}

function deliverCiFixTurn(options: MergePassOptions, assignmentId: number, observation: Observation): void {
  const failures = failedChecks(observation.evidence);
  if (failures.length === 0 || !observation.owned || !observation.issueOpen || !automaticTurnAllowed(options, assignmentId)) return;
  const { pr } = observation.evidence;
  const checks = failures.map((check) => `- ${check.name}: ${check.url ?? `https://github.com/${options.repo}/pull/${pr.number}/checks`}`).join("\n");
  options.workers.deliver(assignmentId, `ci-fix-a${assignmentId}-${pr.head.sha}`,
    `CI checks failed on pull request #${pr.number} at head ${pr.head.sha}:\n${checks}\n\n` +
    `Fetch origin/${pr.base.ref} and compare these failures with the current origin/${pr.base.ref} head before changing code. ` +
    `If the same check fails on origin/${pr.base.ref}, report that evidence so you do not chase a failure this PR did not cause. ` +
    `Check the current PR head before acting; fix the failures that still apply, then commit and push to the existing PR branch ${pr.head.ref}. Do not open a new PR.`,
    () => {});
}

function deliverConflictTurn(options: MergePassOptions, assignmentId: number, observation: Observation): void {
  if (!observation.owned || !observation.issueOpen || !automaticTurnAllowed(options, assignmentId)) return;
  const { pr } = observation.evidence;
  // The durable queue retains finished keys, so an unresolved conflict is not retried until the
  // head or base moves. No await separates the local eligibility check from durable delivery.
  const key = `conflict-a${assignmentId}-${pr.head.sha}-${pr.base.sha}`;
  options.workers.deliver(assignmentId, key,
    `Pull request #${pr.number} conflicts with its base branch ${pr.base.ref}. Fetch origin/${pr.base.ref}, then merge the latest origin/${pr.base.ref} into the existing PR branch ${pr.head.ref} using git merge, ` +
    "resolve the conflicts keeping both sides' intent, and keep every entry in append-only files such as CHANGELOG.md. " +
    "Commit and push the result to the existing PR branch; do not open a new PR. This is a local branch merge, not permission to merge the pull request on GitHub.",
    () => {});
}

async function observe(options: MergePassOptions, assignmentId: number, prNumber: number): Promise<Observation> {
  const assignment = options.store.getAssignment(assignmentId);
  const context = options.store.ticketContext(assignment.ticketId);
  const evidence = await readMergeEvidence(options.gh, options.repo, prNumber);
  const issue = object(await ghMergeGet(options.gh, `repos/${options.repo}/issues/${context.thread.number}`));
  const threadNumbers = [...new Set([context.thread.number, prNumber])];
  const comments: Json[] = [];
  for (const number of threadNumbers) comments.push(...await pages(options, `repos/${options.repo}/issues/${number}/comments`));
  comments.push(...await pages(options, `repos/${options.repo}/pulls/${prNumber}/comments`));
  const pulls = await pages(options, `repos/${options.repo}/pulls?head=${encodeURIComponent(`${options.repo.split("/")[0]}:${assignment.branch}`)}&state=open`);
  const owners = options.store.runningAssignments().filter((other) => other.branch === assignment.branch && options.store.getTicket(other.ticketId).state !== "ended");
  const pull = evidence.rest.pull;
  const unjudgedApprovalCommentary = evidence.reviews.some((review) => review.state === "APPROVED" && review.inlineComments === 0 &&
    review.body.trim() !== "" && !isApprovalOnly(review.state, review.body, review.inlineComments) &&
    !options.store.hasApprovalReceipt(context.thread.sourceId, review.id) && !options.store.hasTriggerEvent(context.thread.sourceId, review.id));
  const activity = JSON.stringify({
    local: options.store.ticketActivityAt(assignment.ticketId),
    issue: [issue["state"], issue["title"], issue["body"], issue["labels"]],
    pr: [pull["title"], pull["body"], pull["draft"], pull["requested_reviewers"], pull["labels"]],
    comments: comments.filter((c) => typeof c["body"] !== "string" || !c["body"].includes("<!-- ostrom:")).map((c) => [c["id"], c["body"], c["updated_at"]]),
    reviews: evidence.reviews.filter((r) => !isApprovalOnly(r.state, r.body, r.inlineComments) &&
      !(r.state === "APPROVED" && r.inlineComments === 0 && r.body.trim() !== "" &&
        !options.store.hasTriggerEvent(context.thread.sourceId, r.id)))
      .map((r) => [r.id, r.state, r.submittedAt, r.commitOid, r.body, r.inlineComments]),
    threads: evidence.threads,
  });
  return { evidence, issueOpen: issue["state"] === "open", activity, unjudgedApprovalCommentary,
    owned: owners.length === 1 && (evidence.pr.state === "MERGED" || (pulls.length === 1 && pulls[0]!["number"] === prNumber)) && evidence.pr.head.ref === assignment.branch &&
      evidence.pr.head.repository === options.repo && evidence.pr.base.repository === options.repo && evidence.pr.base.ref === (options.base ?? "main") };
}

async function pages(options: MergePassOptions, path: string): Promise<Json[]> {
  const rows: Json[] = [];
  for (let page = 1; ; page++) {
    const values = await ghMergeGet(options.gh, `${path}${path.includes("?") ? "&" : "?"}per_page=100&page=${page}`);
    if (!Array.isArray(values)) throw new Error(`GitHub did not return a list for ${path}`);
    rows.push(...values.map(object));
    if (values.length < 100) return rows;
  }
}

async function eligibilityFor(options: MergePassOptions, assignmentId: number, observation: Observation, candidate: MergeCandidate) {
  if (!observation.owned) return { eligible: false as const, reason: "pull request ownership or base is ambiguous" };
  if (options.policy.strategy === "human") return { eligible: false as const, reason: "waiting for a human merge" };
  if (observation.unjudgedApprovalCommentary) return { eligible: false as const, reason: "approval commentary not yet judged" };
  const { evidence } = observation;
  const approvals = { authorLogin: evidence.pr.authorLogin ?? "", identities: await approvalIdentities(options, evidence) };
  // Permission reads yield to the operator API; reread local controls after the last await.
  const assignment = options.store.getAssignment(assignmentId);
  const ticket = options.store.getTicket(assignment.ticketId);
  const now = clock(options);
  return mergeEligibility({ evidence, policy: options.policy, expected: { head: candidate.headSha, base: candidate.baseSha }, approvals,
    local: { issueOpen: observation.issueOpen, ticketOpen: ticket.state === "open", pausedOrStopping: ticket.state === "paused" || ticket.stopRequestedAt !== null,
      stalledOrFailed: assignment.outcome === "failed" || options.store.unansweredStalls().some((stall) => stall.assignmentId === assignmentId),
      budgetReached: budgetReached(options.store.ticketBudget(ticket.id), now) !== null,
      workerIdle: options.store.assignmentWorkIdle(assignmentId), readyReplySent: candidate.readySentAt !== null,
      quietUntil: candidate.quietDeadline === null ? null : new Date(candidate.quietDeadline), now } });
}

async function approvalIdentities(options: MergePassOptions, evidence: MergeEvidence): Promise<ReadonlyMap<string, ApprovalIdentity>> {
  const identities = new Map<string, ApprovalIdentity>();
  for (const review of evidence.reviews) {
    const author = review.author;
    if (author === null || author.id === null || author.login === null || identities.has(author.id) || review.state !== "APPROVED" || review.commitOid === null) continue;
    const permission = object(await ghMergeGet(options.gh, `repos/${options.repo}/collaborators/${encodeURIComponent(author.login)}/permission`));
    const role = permission["permission"] ?? permission["role_name"];
    identities.set(author.id, { id: author.id, login: author.login,
      permission: role === "write" || role === "maintain" || role === "admin" ? role : "read", bot: author.type !== "User" });
  }
  return identities;
}

function candidateFor(options: MergePassOptions, assignmentId: number, prNumber: number, observation: Observation): MergeCandidate {
  const old = options.store.mergeCandidate(assignmentId, options.repo, prNumber);
  const policy = JSON.stringify(options.policy);
  const readySentAt = options.store.readyReplySentAt(assignmentId);
  const { evidence } = observation;
  const policyChanged = old !== null && old.policy !== policy;
  const changed = old === null || old.headSha !== evidence.pr.head.sha || old.baseSha !== evidence.pr.base.sha || policyChanged || old.readySentAt !== readySentAt || old.activityFingerprint !== observation.activity;
  const now = clock(options);
  return options.store.upsertMergeCandidate({ assignmentId, repository: options.repo, prNumber, headSha: evidence.pr.head.sha, baseSha: evidence.pr.base.sha, policy,
    activityFingerprint: observation.activity, activityAt: changed ? now : old.activityAt === null ? null : new Date(old.activityAt),
    readyReplyId: options.store.readyReplyId(assignmentId), readySentAt: readySentAt === null ? null : new Date(readySentAt),
    quietDeadline: readySentAt === null ? null : changed ? new Date(Math.max(now.getTime(), Date.parse(readySentAt)) + QUIET_MS) : old.quietDeadline === null ? null : new Date(old.quietDeadline),
    blockingReason: old?.blockingReason ?? null });
}

function saveReason(options: MergePassOptions, candidate: MergeCandidate, reason: string): void {
  options.store.upsertMergeCandidate({ ...candidate, blockingReason: reason,
    activityAt: candidate.activityAt === null ? null : new Date(candidate.activityAt),
    readySentAt: candidate.readySentAt === null ? null : new Date(candidate.readySentAt),
    quietDeadline: candidate.quietDeadline === null ? null : new Date(candidate.quietDeadline) });
  queueStatusCard(options.store, options.store.getAssignment(candidate.assignmentId).ticketId, clock(options));
}

function complete(options: MergePassOptions, candidate: MergeCandidate, evidence: MergeEvidence, attemptId: number | undefined, origin: "ostrom" | "external" | "unknown"): void {
  const assignment = options.store.getAssignment(candidate.assignmentId);
  const context = options.store.ticketContext(assignment.ticketId);
  const pull = evidence.rest.pull;
  const actor = pull["merged_by"] === null ? {} : object(pull["merged_by"]);
  const mergedByLogin = typeof actor["login"] === "string" ? actor["login"] : null;
  const mergedById = typeof actor["id"] === "number" ? actor["id"] : null;
  if (options.appBotId !== undefined && mergedById === options.appBotId) origin = "ostrom";
  const sha = mergeSha(evidence);
  const mergedAt = pull["merged_at"];
  if (sha === null || typeof mergedAt !== "string" || !Number.isFinite(Date.parse(mergedAt))) throw new Error("merged PR has no merge SHA or timestamp");
  const who = origin === "ostrom" ? `Ostrom (GitHub actor ${mergedByLogin === null ? "unknown" : `@${mergedByLogin}`}) merged` :
    mergedByLogin === null ? "GitHub reports a merge of" : `@${mergedByLogin} merged`;
  const attribution = origin === "unknown" ? " The merge request's result was lost; whether Ostrom caused this merge is unknown." : "";
  // End native work before committing the terminal observation. Queued work is finished atomically
  // with the result, outcome, reservation release and ticket end in the store.
  options.workers.cancel(assignment.id);
  options.store.recordMergeAndOutcome({ repository: candidate.repository, prNumber: candidate.prNumber, headSha: evidence.pr.head.sha,
    mergeSha: sha, mergedAt: new Date(mergedAt), mergedByLogin, mergedById, origin, ...(attemptId === undefined ? {} : { attemptId }),
    assignmentId: assignment.id, threadId: context.thread.id,
    body: `Done: ${who} https://github.com/${candidate.repository}/pull/${candidate.prNumber}. Merge commit: https://github.com/${candidate.repository}/commit/${sha}.${attribution}`,
    evidence: JSON.stringify({ observedAt: clock(options).toISOString(), head: evidence.pr.head.sha, base: evidence.pr.base.sha,
      reviews: evidence.reviews.map((r) => r.id), suites: evidence.rollup?.suites, contexts: evidence.rollup?.contexts,
      protection: evidence.rest.protection, rules: evidence.rest.rules, squash: evidence.rest.repository["allow_squash_merge"] }) });
  queueStatusCard(options.store, assignment.ticketId, clock(options));
  options.workers.release(assignment.id);
}

/** gh 2.98.0 GET probe: stderr is `gh: Not Found (HTTP 404)`; execGh prefixes `gh api failed: `. */
export function ghHttpStatus(error: string): number | null {
  const match = /\(HTTP (\d{3})\)/.exec(error);
  return match === null ? null : Number(match[1]);
}
function retry(options: MergePassOptions, attempt: number): Date { return new Date(clock(options).getTime() + RETRY_MS * 2 ** Math.min(attempt - 1, 6)); }
function clock(options: MergePassOptions): Date { return options.now?.() ?? new Date(); }
function mergeSha(evidence: MergeEvidence): string | null { const sha = evidence.rest.pull["merge_commit_sha"]; return typeof sha === "string" && sha !== "" ? sha : null; }
function object(value: unknown): Json { if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("GitHub response is not an object"); return value as Json; }
function message(error: unknown): string { return error instanceof Error ? error.message : String(error); }
