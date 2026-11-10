// The outbox sender. Comments use hidden markers for crash recovery; status-card edits are
// idempotent PATCHes; reactions use GitHub's reaction endpoints. New ordinary comments are batched
// per thread so routine progress is quiet, while blocking questions go out immediately.

import type { Reply, Store } from "../store/store.ts";
import { ghGet, sinceParam, type GhRunner } from "./gh.ts";
import { replyMarker } from "./marker.ts";

export interface SendOptions {
  store: Store;
  gh: GhRunner;
  /** owner/name */
  repo: string;
  now?: () => Date;
  /** Delay before the next attempt, given the attempts made before this failure. */
  backoffMs?: (attempts: number) => number;
  perPage?: number;
  batchWindowMs?: number;
  commentGapMs?: number;
}

export interface SendResult {
  /** Delivered by this pass. */
  sent: string[];
  /** Found already on GitHub (a prior pass posted then crashed) and marked sent. */
  found: string[];
  failed: { id: string; error: string }[];
}

/** 30 s, doubling per attempt, capped at an hour. */
export function defaultBackoffMs(attempts: number): number {
  return Math.min(30_000 * 2 ** attempts, 60 * 60_000);
}

export async function sendDueReplies(options: SendOptions): Promise<SendResult> {
  const now = options.now ?? (() => new Date());
  const batchWindowMs = options.batchWindowMs ?? minutesFromEnv("OSTROM_BATCH_WINDOW_MIN", 10);
  const commentGapMs = options.commentGapMs ?? minutesFromEnv("OSTROM_COMMENT_GAP_MIN", 30);
  const result: SendResult = { sent: [], found: [], failed: [] };
  const due = options.store.dueReplies();
  const comments = new Map<number, Reply[]>();

  for (const reply of due) {
    if (reply.kind !== "comment") {
      await sendOne(reply, options, result);
      continue;
    }
    if (reply.id === `card-thread-${reply.threadId}` || reply.id.startsWith("review-")) {
      await postCommentBatch([reply], options, result);
      continue;
    }
    const grouped = comments.get(reply.threadId) ?? [];
    grouped.push(reply);
    comments.set(reply.threadId, grouped);
  }
  for (const replies of comments.values()) {
    await sendComments(replies, options, result, now(), batchWindowMs, commentGapMs);
  }
  return result;
}

async function sendComments(
  replies: readonly Reply[],
  options: SendOptions,
  result: SendResult,
  now: Date,
  batchWindowMs: number,
  commentGapMs: number,
): Promise<void> {
  const blocking = replies.filter((reply) => reply.blocking);
  const ordinary = replies.filter((reply) => !reply.blocking);
  if (blocking.length > 0) await postCommentBatch(blocking, options, result);
  if (ordinary.length === 0) return;

  const oldest = Math.min(...ordinary.map((reply) => Date.parse(reply.enqueuedAt)));
  const readyAt = new Date(oldest + batchWindowMs);
  const lastSentAt = options.store.lastCommentSentAt(ordinary[0]!.threadId);
  const gapAt = lastSentAt === null ? null : new Date(Date.parse(lastSentAt) + commentGapMs);
  const next = gapAt !== null && gapAt > readyAt ? gapAt : readyAt;
  if (next > now) {
    for (const reply of ordinary) options.store.deferReply(reply.id, next);
    return;
  }
  await postCommentBatch(ordinary, options, result);
}

async function postCommentBatch(replies: readonly Reply[], options: SendOptions, result: SendResult): Promise<void> {
  const { store, gh, repo } = options;
  const number = store.getThread(replies[0]!.threadId).number;
  let remaining: Reply[] = [];
  try {
    for (const reply of replies) {
      // Add before deriving the marker so a malformed id is recorded as a retryable failure too.
      remaining.push(reply);
      const found = await alreadyPosted(gh, repo, number, reply, replyMarker(reply.id), options.perPage ?? 100);
      if (found === null) continue;
      remaining.pop();
      store.markReplySent(reply.id);
      store.markReadyReplySent(reply.id);
      if (isStatusCard(reply)) store.setStatusCardCommentId(reply.threadId, found);
      result.found.push(reply.id);
    }
    if (remaining.length === 0) return;
    // A rendered status card already carries its marker, so later edits keep it; add one only when missing.
    const body = remaining.map((reply) => {
      const marker = replyMarker(reply.id);
      return reply.body.includes(marker) ? reply.body : `${reply.body}\n\n${marker}`;
    }).join("\n\n---\n\n");
    const posted = JSON.parse(await gh(["api", "--method", "POST", `repos/${repo}/issues/${number}/comments`, "-f", `body=${body}`])) as { id?: unknown };
    for (const reply of remaining) {
      store.markReplySent(reply.id);
      if (isStatusCard(reply)) {
        if (typeof posted.id !== "number") throw new Error("GitHub did not return the status card comment id");
        store.setStatusCardCommentId(reply.threadId, posted.id);
      }
      result.sent.push(reply.id);
    }
  } catch (error) {
    for (const reply of remaining) fail(reply, error, options, result);
  }
}

async function sendOne(reply: Reply, options: SendOptions, result: SendResult): Promise<void> {
  try {
    if (reply.kind === "edit") {
      await options.gh(["api", "--method", "PATCH", cardPath(options.repo, reply), "-f", `body=${reply.body}`]);
    } else if (reply.kind === "reaction") {
      await options.gh(["api", "--method", "POST", reactionPath(options.repo, reply), "-f", `content=${reply.reaction!}`]);
    } else {
      throw new Error(`outbox kind ${reply.kind} is not implemented`);
    }
    options.store.markReplySent(reply.id);
    options.store.markReadyReplySent(reply.id);
    result.sent.push(reply.id);
  } catch (error) {
    fail(reply, error, options, result);
  }
}

function cardPath(repo: string, reply: Reply): string {
  const id = Number(reply.target);
  if (!Number.isSafeInteger(id) || id <= 0) throw new Error(`invalid status card comment id ${String(reply.target)}`);
  return `repos/${repo}/issues/comments/${id}`;
}

/** Review-comment reactions have their own endpoint; issue and review summaries use issue comments. */
function reactionPath(repo: string, reply: Reply): string {
  if (reply.target === null) throw new Error("reaction has no target");
  const url = new URL(reply.target);
  const review = url.hash.match(/^#discussion_r(\d+)$/);
  const issue = url.hash.match(/^#issuecomment-(\d+)$/);
  if (review) return `repos/${repo}/pulls/comments/${review[1]}/reactions`;
  if (issue) return `repos/${repo}/issues/comments/${issue[1]}/reactions`;
  throw new Error(`reaction target ${reply.target} is not a GitHub comment URL`);
}

function fail(reply: Reply, error: unknown, options: SendOptions, result: SendResult): void {
  const message = error instanceof Error ? error.message : String(error);
  const now = options.now ?? (() => new Date());
  const backoffMs = options.backoffMs ?? defaultBackoffMs;
  options.store.recordReplyFailure(reply.id, message, new Date(now().getTime() + backoffMs(reply.attempts)));
  result.failed.push({ id: reply.id, error: message });
}

/** Whether a comment on the thread already carries the reply's marker. */
async function alreadyPosted(
  gh: GhRunner,
  repo: string,
  number: number,
  reply: Reply,
  marker: string,
  perPage: number,
): Promise<number | null> {
  const since = sinceParam(new Date(reply.enqueuedAt));
  for (let page = 1; ; page++) {
    const path = `repos/${repo}/issues/${number}/comments?since=${since}&per_page=${perPage}&page=${page}`;
    const comments = await ghGet(gh, path);
    if (!Array.isArray(comments)) throw new Error(`GET ${path}: expected a JSON array`);
    const found = comments.find((c: { body?: unknown }) => typeof c.body === "string" && c.body.includes(marker)) as { id?: unknown } | undefined;
    if (found !== undefined) {
      if (typeof found.id !== "number") throw new Error(`GET ${path}: marker comment has no numeric id`);
      return found.id;
    }
    if (comments.length < perPage) return null;
  }
}

function isStatusCard(reply: Reply): boolean {
  return reply.id === `card-thread-${reply.threadId}` || reply.id.startsWith(`card-thread-${reply.threadId}-`);
}

function minutesFromEnv(name: string, fallback: number): number {
  const value = process.env[name]?.trim() ?? "";
  return /^[1-9]\d*$/.test(value) ? Number(value) * 60_000 : fallback * 60_000;
}
