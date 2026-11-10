// Deterministic unattended merge and restart reconciliation. The Router never authorizes a merge.
import { budgetReached } from "../budget/budgets.ts";
import type { Workers } from "../worker/workers.ts";
import type { MergeCandidate, Store } from "../store/store.ts";
import { queueStatusCard } from "./cards.ts";
import { ghMergeGet, ghSquashMerge, type GhRunner } from "./gh.ts";
import { mergeEligibility, readyPolicySentence, type ApprovalIdentity, type MergePolicy } from "./merge-policy.ts";
import { readMergeEvidence, type MergeEvidence } from "./merge-reader.ts";

const QUIET_MS = 10 * 60_000;
const RETRY_MS = 30_000;

type Json = Record<string, unknown>;
interface Observation { evidence: MergeEvidence; issueOpen: boolean; activity: string; owned: boolean; }
export interface MergePassOptions {
  store: Store; workers: Workers; gh: GhRunner; repo: string; policy: MergePolicy; base?: string; now?: () => Date;
  /** Failed prerequisite passes suppress sends, but reconciliation must still run. */
  allowSend?: boolean;
  /** Intake and handle newly arrived controls before the final direct evidence read. */
  refreshBeforeSend?: () => Promise<void>;
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
    if (candidate !== null && unresolved.has(candidate.id)) continue;
    if (options.store.reservationHeld(assignment.id)) options.workers.release(assignment.id);
    const pr = options.store.ticketPullRequest(assignment.ticketId);
    if (pr === null) continue;
    try { await consider(options, assignment.id, pr.number, result); }
    catch (error) { result.failed.push({ assignmentId: assignment.id, error: message(error) }); }
  }
  return result;
}

async function consider(options: MergePassOptions, assignmentId: number, prNumber: number, result: MergePassResult): Promise<void> {
  let observation = await observe(options, assignmentId, prNumber);
  let candidate = candidateFor(options, assignmentId, prNumber, observation);
  if (observation.evidence.pr.state === "MERGED") {
    complete(options, candidate, observation.evidence, undefined, "external");
    result.merged.push(assignmentId);
    return;
  }
  // New PRs wait for the Router's pr_opened reply. Only an old, untracked opened reply needs a
  // separate first-enable notice: reusing its ID would never produce a fresh delivery timestamp.
  const openedId = `outcome-a${assignmentId}-pr${prNumber}-opened`;
  if (options.store.readyReplyId(assignmentId) === null && options.store.hasReply(openedId)) {
    const id = `ready-a${assignmentId}-pr${prNumber}`;
    const context = options.store.ticketContext(options.store.getAssignment(assignmentId).ticketId);
    options.store.enqueueReply({ id, threadId: context.thread.id,
      body: `Pull request ready: https://github.com/${options.repo}/pull/${prNumber}\n\n${readyPolicySentence(options.policy)}` });
    options.store.trackReadyReply(assignmentId, id);
    candidate = candidateFor(options, assignmentId, prNumber, observation);
  }
  const wait = (reason: string) => {
    saveReason(options, candidate, reason);
    result.waiting.push({ assignmentId, reason });
  };
  if (options.allowSend === false) { wait("an earlier prerequisite pass failed"); return; }
  const retryAt = options.store.mergeRetryAt(options.repo, prNumber);
  if (retryAt !== null && Date.parse(retryAt) > clock(options).getTime()) { wait(`merge retry after ${retryAt}`); return; }
  const eligibility = await eligibilityFor(options, assignmentId, observation, candidate);
  if (!eligibility.eligible) { wait(eligibility.reason); return; }

  await options.refreshBeforeSend?.();
  observation = await observe(options, assignmentId, prNumber);
  candidate = candidateFor(options, assignmentId, prNumber, observation);
  if (observation.evidence.pr.state === "MERGED") {
    complete(options, candidate, observation.evidence, undefined, "external");
    result.merged.push(assignmentId);
    return;
  }
  const actor = object(await ghMergeGet(options.gh, "user"));
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
  const activity = JSON.stringify({
    local: options.store.ticketActivityAt(assignment.ticketId),
    issue: [issue["state"], issue["title"], issue["body"], issue["labels"]],
    pr: [pull["title"], pull["body"], pull["draft"], pull["requested_reviewers"], pull["labels"]],
    comments: comments.filter((c) => typeof c["body"] !== "string" || !c["body"].includes("<!-- ostrom:")).map((c) => [c["id"], c["body"], c["updated_at"]]),
    reviews: evidence.reviews.map((r) => [r.id, r.state, r.submittedAt, r.commitOid]),
    threads: evidence.threads,
  });
  return { evidence, issueOpen: issue["state"] === "open", activity,
    owned: owners.length === 1 && pulls.length === 1 && pulls[0]!["number"] === prNumber && evidence.pr.head.ref === assignment.branch &&
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
  const assignment = options.store.getAssignment(assignmentId);
  const ticket = options.store.getTicket(assignment.ticketId);
  if (!observation.owned) return { eligible: false as const, reason: "pull request ownership or base is ambiguous" };
  if (options.policy.strategy === "human") return { eligible: false as const, reason: "waiting for a human merge" };
  const { evidence } = observation;
  const approvals = { authorLogin: evidence.pr.authorLogin ?? "", identities: await approvalIdentities(options, evidence) };
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
    if (author === null || author.id === null || author.login === null || identities.has(author.id) || review.state !== "APPROVED" || review.commitOid !== evidence.pr.head.sha) continue;
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
  const changed = old === null || old.headSha !== evidence.pr.head.sha || old.baseSha !== evidence.pr.base.sha || old.policy !== policy || old.readySentAt !== readySentAt || old.activityFingerprint !== observation.activity;
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
