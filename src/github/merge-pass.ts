// Deterministic unattended merge and restart reconciliation. The Router never authorizes this pass.

import { budgetReached } from "../budget/budgets.ts";
import type { Workers } from "../worker/workers.ts";
import type { MergeCandidate, Store } from "../store/store.ts";
import { ghMergeGet, ghSquashMerge, type GhRunner } from "./gh.ts";
import { countApprovals, mergeEligibility, type ApprovalIdentity, type MergePolicy } from "./merge-policy.ts";
import { readMergeEvidence, type MergeEvidence } from "./merge-reader.ts";

const QUIET_MS = 10 * 60_000;
const RETRY_MS = 30_000;

export interface MergePassOptions { store: Store; workers: Workers; gh: GhRunner; repo: string; policy: MergePolicy; now?: () => Date; }
export interface MergePassResult { merged: number[]; waiting: { assignmentId: number; reason: string }[]; reconciled: number[]; failed: { assignmentId: number; error: string }[]; }

/** Reconcile unresolved requests and crash-held reservations before evaluating any new request. */
export async function mergePass(options: MergePassOptions): Promise<MergePassResult> {
  const result: MergePassResult = { merged: [], waiting: [], reconciled: [], failed: [] };
  const unresolved = options.store.unresolvedMergeAttempts(options.repo);
  const held = new Set(unresolved.map((attempt) => options.store.mergeCandidateById(attempt.candidateId).assignmentId));
  for (const attempt of unresolved) {
    const candidate = options.store.mergeCandidateById(attempt.candidateId);
    try {
      const evidence = await readMergeEvidence(options.gh, options.repo, attempt.prNumber);
      if (evidence.pr.state === "MERGED") {
        // A pending row survived a restart without an observed response, so origin remains unknown.
        complete(options, candidate, evidence, attempt.id, attempt.state === "confirmed" ? "ostrom" : "unknown");
        options.store.finishMergeAttempt(attempt.id, "confirmed", mergeSha(evidence) === null ? {} : { sha: mergeSha(evidence)! });
      } else {
        options.store.finishMergeAttempt(attempt.id, "failed", { error: "merge was not confirmed", nextRetryAt: retry(options, attempt.attemptNumber) });
        options.workers.release(candidate.assignmentId);
      }
      result.reconciled.push(attempt.id);
    } catch (error) { result.failed.push({ assignmentId: candidate.assignmentId, error: message(error) }); }
  }
  for (const assignment of options.store.runningAssignments()) {
    if (options.store.reservationHeld(assignment.id) && !held.has(assignment.id)) options.workers.release(assignment.id);
    const pr = options.store.ticketPullRequest(assignment.ticketId);
    if (pr === null) continue;
    try { await consider(options, assignment.id, pr.number, result); }
    catch (error) { result.failed.push({ assignmentId: assignment.id, error: message(error) }); }
  }
  return result;
}

async function consider(options: MergePassOptions, assignmentId: number, prNumber: number, result: MergePassResult): Promise<void> {
  const now = options.now?.() ?? new Date();
  const assignment = options.store.getAssignment(assignmentId);
  const ticket = options.store.getTicket(assignment.ticketId);
  const evidence = await readMergeEvidence(options.gh, options.repo, prNumber);
  if (evidence.pr.state === "MERGED") {
    complete(options, candidateFor(options, assignmentId, prNumber, evidence, now, "merged externally"), evidence, undefined, "external");
    result.merged.push(assignmentId); return;
  }
  let readySentAt = options.store.readyReplySentAt(assignmentId);
  if (readySentAt === null) {
    const context = options.store.ticketContext(assignment.ticketId);
    const id = `outcome-a${assignmentId}-pr${prNumber}-opened`;
    const mode = options.policy.strategy === "human" ? "Ostrom is waiting for a human merge." : "Ostrom will squash-merge automatically after the ten-minute quiet window when all checks and policy requirements hold.";
    options.store.enqueueReply({ id, threadId: context.thread.id, body: `Pull request ready: ${options.store.getThread(options.store.ticketPullRequest(assignment.ticketId)!.id).url}\n\n${mode}` });
    options.store.trackReadyReply(assignmentId, id);
  }
  const candidate = candidateFor(options, assignmentId, prNumber, evidence, now, null, readySentAt);
  const retryAt = options.store.mergeRetryAt(options.repo, prNumber);
  if (retryAt !== null && Date.parse(retryAt) > now.getTime()) { result.waiting.push({ assignmentId, reason: "merge retry backoff" }); return; }
  const eligibility = await eligibilityFor(options, assignmentId, evidence, candidate, now, readySentAt);
  if (!eligibility.eligible) { candidateFor(options, assignmentId, prNumber, evidence, now, eligibility.reason, readySentAt); result.waiting.push({ assignmentId, reason: eligibility.reason }); return; }
  if (!options.store.reserveAssignment(assignmentId)) { result.waiting.push({ assignmentId, reason: "Worker is not idle" }); return; }
  const fresh = await readMergeEvidence(options.gh, options.repo, prNumber);
  if (fresh.pr.state === "MERGED") { complete(options, candidate, fresh, undefined, "external"); options.workers.release(assignmentId); result.merged.push(assignmentId); return; }
  const final = await eligibilityFor(options, assignmentId, fresh, candidate, now, readySentAt);
  if (!final.eligible) { options.workers.release(assignmentId); candidateFor(options, assignmentId, prNumber, fresh, now, final.reason, readySentAt); result.waiting.push({ assignmentId, reason: final.reason }); return; }
  const attempt = options.store.createMergeAttempt(candidate);
  try {
    const response = object(await ghSquashMerge(options.gh, options.repo, prNumber, candidate.headSha));
    if (response["merged"] !== true || typeof response["sha"] !== "string") {
      options.store.finishMergeAttempt(attempt.id, "failed", { error: String(response["message"] ?? "GitHub refused merge"), nextRetryAt: retry(options, attempt.attemptNumber) });
      options.workers.release(assignmentId); result.waiting.push({ assignmentId, reason: "GitHub refused merge" }); return;
    }
    const confirmed = await readMergeEvidence(options.gh, options.repo, prNumber);
    if (confirmed.pr.state !== "MERGED") throw new Error("merge response could not be confirmed");
    options.store.finishMergeAttempt(attempt.id, "confirmed", { sha: response["sha"] });
    complete(options, candidate, confirmed, attempt.id, "ostrom", response["sha"]); result.merged.push(assignmentId);
  } catch (error) {
    const errorText = message(error);
    const status = /HTTP (\d{3})\b/.exec(errorText);
    options.store.finishMergeAttempt(attempt.id, status === null ? "unknown" : "failed", { error: errorText, nextRetryAt: status === null ? undefined : retry(options, attempt.attemptNumber) });
    options.workers.release(assignmentId); result.failed.push({ assignmentId, error: errorText });
  }
}

async function eligibilityFor(options: MergePassOptions, assignmentId: number, evidence: MergeEvidence, candidate: MergeCandidate, now: Date, readySentAt: string | null) {
  const assignment = options.store.getAssignment(assignmentId); const ticket = options.store.getTicket(assignment.ticketId);
  const approvals = options.policy.strategy === "approvals" ? { authorLogin: evidence.pr.authorLogin ?? "", identities: await approvalIdentities(options, evidence) } : undefined;
  return mergeEligibility({ evidence, policy: options.policy, expected: { head: candidate.headSha, base: candidate.baseSha }, approvals,
    local: { issueOpen: ticket.state !== "ended", ticketOpen: ticket.state === "open", pausedOrStopping: ticket.state === "paused" || ticket.stopRequestedAt !== null,
      stalledOrFailed: assignment.outcome === "failed" || assignment.stallRecoveryAttempts > 0, budgetReached: budgetReached(options.store.ticketBudget(ticket.id), now) !== null,
      workerIdle: options.store.assignmentWorkIdle(assignmentId), readyReplySent: readySentAt !== null,
      quietUntil: candidate.quietDeadline === null ? null : new Date(candidate.quietDeadline), now } });
}

async function approvalIdentities(options: MergePassOptions, evidence: MergeEvidence): Promise<ReadonlyMap<string, ApprovalIdentity>> {
  const result = new Map<string, ApprovalIdentity>();
  for (const review of evidence.reviews) {
    const author = review.author;
    if (author === null || author.id === null || author.login === null || review.state !== "APPROVED" || review.commitOid !== evidence.pr.head.sha) continue;
    const permission = object(await ghMergeGet(options.gh, `repos/${options.repo}/collaborators/${encodeURIComponent(author.login)}/permission`));
    const role = permission["role_name"];
    result.set(author.id, { id: author.id, login: author.login, permission: role === "write" || role === "maintain" || role === "admin" ? role : "read", bot: author.type === "Bot" });
  }
  return result;
}

function candidateFor(options: MergePassOptions, assignmentId: number, prNumber: number, evidence: MergeEvidence, now: Date, blockingReason: string | null, readySentAt: string | null = options.store.readyReplySentAt(assignmentId)): MergeCandidate {
  const old = options.store.mergeCandidate(assignmentId, options.repo, prNumber); const policy = JSON.stringify(options.policy);
  const activityAt = latest(options.store.ticketActivityAt(options.store.getAssignment(assignmentId).ticketId), evidence.reviews.map((review) => review.submittedAt).filter((value): value is string => value !== null));
  const fingerprint = `${evidence.pr.head.sha}:${evidence.pr.base.sha}:${activityAt ?? "observed"}:${evidence.reviews.map((r) => `${r.id}:${r.state}:${r.submittedAt}`).join(",")}:${evidence.threads.map((t) => `${t.id}:${t.resolved}`).join(",")}`;
  const changed = old === null || old.headSha !== evidence.pr.head.sha || old.baseSha !== evidence.pr.base.sha || old.policy !== policy || old.readySentAt !== readySentAt || old.activityFingerprint !== fingerprint;
  const start = Math.max(now.getTime(), readySentAt === null ? 0 : Date.parse(readySentAt), activityAt === null ? 0 : Date.parse(activityAt));
  return options.store.upsertMergeCandidate({ assignmentId, repository: options.repo, prNumber, headSha: evidence.pr.head.sha, baseSha: evidence.pr.base.sha, policy,
    activityFingerprint: fingerprint, activityAt: activityAt === null ? null : new Date(activityAt), readyReplyId: old?.readyReplyId ?? null,
    readySentAt: readySentAt === null ? null : new Date(readySentAt), quietDeadline: changed ? new Date(start + QUIET_MS) : old.quietDeadline === null ? null : new Date(old.quietDeadline), blockingReason });
}

function complete(options: MergePassOptions, candidate: MergeCandidate, evidence: MergeEvidence, attemptId: number | undefined, origin: "ostrom" | "external" | "unknown", responseSha?: string): void {
  const assignment = options.store.getAssignment(candidate.assignmentId); const context = options.store.ticketContext(assignment.ticketId); const pull = evidence.rest.pull;
  const mergedBy = typeof pull["merged_by"] === "object" && pull["merged_by"] !== null ? (pull["merged_by"] as Record<string, unknown>)["login"] : null;
  const merge = responseSha ?? mergeSha(evidence); const pr = options.store.ticketPullRequest(assignment.ticketId)!;
  options.store.recordMergeAndOutcome({ repository: candidate.repository, prNumber: candidate.prNumber, headSha: candidate.headSha, mergeSha: merge, mergedAt: new Date(), mergedByLogin: typeof mergedBy === "string" ? mergedBy : null, origin, ...(attemptId === undefined ? {} : { attemptId }), assignmentId: assignment.id, threadId: context.thread.id,
    body: `Done: ${typeof mergedBy === "string" ? `@${mergedBy} merged` : "the pull request was merged"} ${pr.url}${merge === null ? "." : ` as ${merge}.`}` });
}
function retry(options: MergePassOptions, attempt: number): Date { return new Date((options.now?.() ?? new Date()).getTime() + RETRY_MS * 2 ** Math.min(attempt - 1, 6)); }
function latest(local: string | null, remote: readonly string[]): string | null { return [local, ...remote].filter((value): value is string => value !== null).sort().at(-1) ?? null; }
function mergeSha(evidence: MergeEvidence): string | null { const sha = evidence.rest.pull["merge_commit_sha"]; return typeof sha === "string" ? sha : null; }
function object(value: unknown): Record<string, unknown> { if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("GitHub response is not an object"); return value as Record<string, unknown>; }
function message(error: unknown): string { return error instanceof Error ? error.message : String(error); }
