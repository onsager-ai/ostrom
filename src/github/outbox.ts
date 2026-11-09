// The outbox sender: posts due replies as issue/PR comments, each carrying the hidden marker with the
// reply's stable id. Before every post it looks for a comment that already carries that marker,
// because a crash between posting and `markReplySent` leaves the reply due with no record that it
// went out. Looking first on every attempt, not only on retries, covers a crash on the first try.

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
}

export interface SendResult {
  /** Posted by this pass. */
  sent: string[];
  /** Found already on GitHub (an earlier pass posted it, then crashed) and marked sent. */
  found: string[];
  failed: { id: string; error: string }[];
}

/** 30 s, doubling per attempt, capped at an hour. */
export function defaultBackoffMs(attempts: number): number {
  return Math.min(30_000 * 2 ** attempts, 60 * 60_000);
}

export async function sendDueReplies(options: SendOptions): Promise<SendResult> {
  const { store, gh, repo } = options;
  const now = options.now ?? (() => new Date());
  const backoffMs = options.backoffMs ?? defaultBackoffMs;
  const perPage = options.perPage ?? 100;
  const result: SendResult = { sent: [], found: [], failed: [] };

  for (const reply of store.dueReplies()) {
    try {
      const { number } = store.getThread(reply.threadId);
      const marker = replyMarker(reply.id);
      if (await alreadyPosted(gh, repo, number, reply, marker, perPage)) {
        store.markReplySent(reply.id);
        result.found.push(reply.id);
        continue;
      }
      await gh([
        "api",
        "--method",
        "POST",
        `repos/${repo}/issues/${number}/comments`,
        "-f",
        `body=${reply.body}\n\n${marker}`,
      ]);
      store.markReplySent(reply.id);
      result.sent.push(reply.id);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      store.recordReplyFailure(reply.id, message, new Date(now().getTime() + backoffMs(reply.attempts)));
      result.failed.push({ id: reply.id, error: message });
    }
  }
  return result;
}

/** Whether a comment on the thread already carries the reply's marker. It cannot predate the enqueue. */
async function alreadyPosted(
  gh: GhRunner,
  repo: string,
  number: number,
  reply: Reply,
  marker: string,
  perPage: number,
): Promise<boolean> {
  const since = sinceParam(new Date(reply.enqueuedAt));
  for (let page = 1; ; page++) {
    const path = `repos/${repo}/issues/${number}/comments?since=${since}&per_page=${perPage}&page=${page}`;
    const comments = await ghGet(gh, path);
    if (!Array.isArray(comments)) throw new Error(`GET ${path}: expected a JSON array`);
    if (comments.some((c: { body?: unknown }) => typeof c.body === "string" && c.body.includes(marker))) {
      return true;
    }
    if (comments.length < perPage) return false;
  }
}
