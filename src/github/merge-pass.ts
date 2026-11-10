// Deterministic unattended merge and restart reconciliation. The Router never authorizes this pass.

import { budgetReached } from "../budget/budgets.ts";
import type { Workers } from "../worker/workers.ts";
import type { MergeCandidate, Store } from "../store/store.ts";
import { ghSquashMerge, type GhRunner } from "./gh.ts";
import { mergeEligibility, type MergePolicy } from "./merge-policy.ts";
import { readMergeEvidence, type MergeEvidence } from "./merge-reader.ts";

const QUIET_MS = 10 * 60_000;

export interface MergePassOptions {
  store: Store;
  workers: Workers;
  gh: GhRunner;
  repo: string;
  policy: MergePolicy;
  now?: () => Date;
}

export interface MergePassResult {
  merged: number[];
  waiting: { assignmentId: number; reason: string }[];
  reconciled: number[];
  failed: { assignmentId: number; error: string }[];
}

/** Reconcile unresolved requests first; a crash-held reservation can never block later work forever. */
export async function mergePass(options: MergePassOptions): Promise<MergePassResult> {
  const result: MergePassResult = { merged: [], waiting: [], reconciled: [], failed: [] };
  for (const attempt of options.store.unresolvedMergeAttempts(options.repo)) {
    const candidate = options.store.mergeCandidateById(attempt.candidateId);
    try {
      const evidence = await readMergeEvidence(options.gh, options.repo, attempt.prNumber);
      if (evidence.pr.state === "MERGED") {
        complete(options, candidate, evidence, attempt.id, attempt.state === "pending" ? "ostrom" : "unknown");
        if (attempt.state === "pending") {
          const sha = mergeSha(evidence);
          options.store.finishMergeAttempt(attempt.id, "confirmed", sha === null ? {} : { sha });
        }
        result.reconciled.push(attempt.id);
      } else {
        if (attempt.state === "pending") options.store.finishMergeAttempt(attempt.id, "unknown", { error: "merge response was not durably observed" });
        options.workers.release(candidate.assignmentId);
        result.reconciled.push(attempt.id);
      }
    } catch (error) {
      result.failed.push({ assignmentId: candidate.assignmentId, error: message(error) });
    }
  }

  for (const assignment of options.store.runningAssignments()) {
    try {
      const prs = [options.store.ticketPullRequest(assignment.ticketId)].filter((value): value is NonNullable<typeof value> => value !== null);
      for (const pr of prs) {
        const existing = options.store.mergeCandidate(assignment.id, options.repo, pr.number);
        if (existing !== null && options.store.unresolvedMergeAttempts(options.repo).some((attempt) => attempt.candidateId === existing.id)) continue;
        await consider(options, assignment.id, pr.number, result);
      }
    } catch (error) {
      result.failed.push({ assignmentId: assignment.id, error: message(error) });
    }
  }
  return result;
}

async function consider(options: MergePassOptions, assignmentId: number, prNumber: number, result: MergePassResult): Promise<void> {
  const now = options.now?.() ?? new Date();
  const assignment = options.store.getAssignment(assignmentId);
  const ticket = options.store.getTicket(assignment.ticketId);
  const evidence = await readMergeEvidence(options.gh, options.repo, prNumber);
  if (evidence.pr.state === "MERGED") {
    const candidate = candidateFor(options, assignmentId, prNumber, evidence, now, "merged externally");
    complete(options, candidate, evidence, undefined, "external");
    result.merged.push(assignmentId);
    return;
  }
  const readySentAt = options.store.readyReplySentAt(assignmentId);
  const candidate = candidateFor(options, assignmentId, prNumber, evidence, now, null, readySentAt);
  const eligibility = mergeEligibility({
    evidence,
    policy: options.policy,
    expected: { head: candidate.headSha, base: candidate.baseSha },
    local: {
      issueOpen: ticket.state !== "ended",
      ticketOpen: ticket.state === "open",
      pausedOrStopping: ticket.state === "paused" || ticket.stopRequestedAt !== null,
      stalledOrFailed: assignment.outcome === "failed" || assignment.stallRecoveryAttempts > 0,
      budgetReached: budgetReached(options.store.ticketBudget(ticket.id), now) !== null,
      workerIdle: options.store.assignmentWorkIdle(assignmentId),
      readyReplySent: readySentAt !== null,
      quietUntil: candidate.quietDeadline === null ? null : new Date(candidate.quietDeadline),
      now,
    },
  });
  if (!eligibility.eligible) {
    candidateFor(options, assignmentId, prNumber, evidence, now, eligibility.reason, readySentAt);
    result.waiting.push({ assignmentId, reason: eligibility.reason });
    return;
  }
  if (!options.store.reserveAssignment(assignmentId)) {
    result.waiting.push({ assignmentId, reason: "Worker is not idle" });
    return;
  }
  const fresh = await readMergeEvidence(options.gh, options.repo, prNumber);
  if (fresh.pr.state === "MERGED") {
    complete(options, candidate, fresh, undefined, "external");
    options.workers.release(assignmentId);
    result.merged.push(assignmentId);
    return;
  }
  if (fresh.pr.head.sha !== candidate.headSha || fresh.pr.base.sha !== candidate.baseSha) {
    options.workers.release(assignmentId);
    candidateFor(options, assignmentId, prNumber, fresh, now, "pull request changed", readySentAt);
    result.waiting.push({ assignmentId, reason: "pull request changed" });
    return;
  }
  const attempt = options.store.createMergeAttempt(candidate);
  let settled = false;
  try {
    const response = object(await ghSquashMerge(options.gh, options.repo, prNumber, candidate.headSha));
    if (response["merged"] !== true || typeof response["sha"] !== "string") {
      options.store.finishMergeAttempt(attempt.id, "failed", { error: String(response["message"] ?? "GitHub refused merge") });
      settled = true;
      options.workers.release(assignmentId);
      result.waiting.push({ assignmentId, reason: "GitHub refused merge" });
      return;
    }
    const confirmed = await readMergeEvidence(options.gh, options.repo, prNumber);
    if (confirmed.pr.state !== "MERGED") throw new Error("GitHub accepted merge but confirmation read is not merged");
    options.store.finishMergeAttempt(attempt.id, "confirmed", { sha: response["sha"] });
    settled = true;
    complete(options, candidate, confirmed, attempt.id, "ostrom", response["sha"]);
    result.merged.push(assignmentId);
  } catch (error) {
    if (!settled) options.store.finishMergeAttempt(attempt.id, "unknown", { error: message(error) });
    options.workers.release(assignmentId);
    result.failed.push({ assignmentId, error: message(error) });
  }
}

function candidateFor(options: MergePassOptions, assignmentId: number, prNumber: number, evidence: MergeEvidence, now: Date, blockingReason: string | null, readySentAt: string | null = options.store.readyReplySentAt(assignmentId)): MergeCandidate {
  const old = options.store.mergeCandidate(assignmentId, options.repo, prNumber);
  const policy = JSON.stringify(options.policy);
  const changed = old === null || old.headSha !== evidence.pr.head.sha || old.baseSha !== evidence.pr.base.sha || old.policy !== policy || old.readySentAt !== readySentAt;
  const started = readySentAt === null ? now : new Date(readySentAt);
  const quietDeadline = changed ? new Date(Math.max(now.getTime(), started.getTime()) + QUIET_MS) : old.quietDeadline === null ? null : new Date(old.quietDeadline);
  return options.store.upsertMergeCandidate({
    assignmentId, repository: options.repo, prNumber, headSha: evidence.pr.head.sha, baseSha: evidence.pr.base.sha, policy,
    activityFingerprint: `${evidence.pr.head.sha}:${evidence.pr.base.sha}:${evidence.reviews.length}:${evidence.threads.length}`,
    activityAt: now, readyReplyId: old?.readyReplyId ?? null, readySentAt: readySentAt === null ? null : new Date(readySentAt),
    quietDeadline, blockingReason,
  });
}

function complete(options: MergePassOptions, candidate: MergeCandidate, evidence: MergeEvidence, attemptId: number | undefined, origin: "ostrom" | "external" | "unknown", responseSha?: string): void {
  const assignment = options.store.getAssignment(candidate.assignmentId);
  const thread = options.store.ticketContext(assignment.ticketId).thread;
  const sha = responseSha ?? mergeSha(evidence);
  options.store.recordMergeAndOutcome({
    repository: candidate.repository, prNumber: candidate.prNumber, headSha: candidate.headSha, mergeSha: sha,
    mergedAt: new Date(), mergedByLogin: null, origin, ...(attemptId === undefined ? {} : { attemptId }), assignmentId: assignment.id,
    threadId: thread.id, body: `Done: ${origin === "ostrom" ? "Ostrom squash-merged" : "the pull request was merged externally"} ${thread.url}.`,
  });
}

function mergeSha(evidence: MergeEvidence): string | null {
  const sha = evidence.rest.pull["merge_commit_sha"];
  return typeof sha === "string" ? sha : null;
}
function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("GitHub merge response is not an object");
  return value as Record<string, unknown>;
}
function message(error: unknown): string { return error instanceof Error ? error.message : String(error); }
