// Deterministic GitHub intake for one repository: one pass reads the comment-type events milestone 1
// needs, drops Ostrom's own replies and actors not on the allowlist unless a ticket subscribes to
// their thread, stores the rest as triggers,
// and only then advances the cursor. A crash before the cursor moves replays the pass; trigger
// inserts are idempotent on the event's node id, so a replay stores nothing twice.
//
// Streams, each read since its own cursor:
// - issue comments (issue and PR conversation): GET /repos/{repo}/issues/comments
// - PR review comments (inline on a diff):      GET /repos/{repo}/pulls/comments
// - PR reviews: GitHub has no repository-wide list, so GET /repos/{repo}/issues (PRs included)
//   gives the PRs updated since the cursor, and GET /repos/{repo}/pulls/{n}/reviews their reviews.

import type { Store, ThreadKind } from "../store/store.ts";
import { ghGet, sinceParam, type GhRunner } from "./gh.ts";
import { hasReplyMarker } from "./marker.ts";

export type TriggerKind = "issue_comment" | "pull_request_review_comment" | "pull_request_review" | "issue_state";

/** One GitHub event, normalized. */
export interface GitHubEvent {
  /** GitHub's node id (IC_…, PRRC_…, PRR_…): unique across all three kinds. */
  id: string;
  kind: TriggerKind;
  threadKind: ThreadKind;
  number: number;
  threadUrl: string;
  url: string;
  actor: string;
  body: string;
  occurredAt: Date;
  /** GitHub numeric review id that groups inline comments with their submitted review. */
  reviewId?: string;
  path?: string;
  line?: number;
}

/** A stored trigger the Router has not seen yet. The body is carried, not stored. */
export interface Candidate {
  triggerId: number;
  threadId: number;
  kind: TriggerKind;
  actor: string;
  url: string;
  body: string;
  occurredAt: Date;
}

export interface PollResult {
  candidates: Candidate[];
  /** Events already stored by an earlier (or replayed) pass. */
  duplicates: number;
  droppedOwn: number;
  droppedNotAllowed: number;
}

export interface PollOptions {
  store: Store;
  gh: GhRunner;
  /** owner/name */
  repo: string;
  /** GitHub logins allowed to start work; compared case-insensitively, as GitHub does. */
  allowlist: readonly string[];
  now?: () => Date;
  /** Page size; GitHub's maximum is 100. */
  perPage?: number;
  /**
   * How far each stream re-reads behind its cursor. GitHub's lists are eventually consistent, so an
   * event can surface after a later one; the overlap catches it and dedup absorbs the re-reads.
   */
  overlapMs?: number;
}

/** Per-stream positions: the newest `updated_at` fully read. Stored as JSON in `source.cursor`. */
interface Cursor {
  issueComments: string;
  reviewComments: string;
  issues: string;
}

export type FilterVerdict = "keep" | "own" | "not_allowed";

/** The two milestone 1 filters, in code. Ostrom's own replies go first: they are never candidates. */
export function classify(event: Pick<GitHubEvent, "actor" | "body">, allowlist: ReadonlySet<string>): FilterVerdict {
  if (hasReplyMarker(event.body)) return "own";
  if (!allowlist.has(event.actor.toLowerCase())) return "not_allowed";
  return "keep";
}

export async function pollOnce(options: PollOptions): Promise<PollResult> {
  const { store, gh, repo } = options;
  const now = options.now ?? (() => new Date());
  const perPage = options.perPage ?? 100;
  const overlapMs = options.overlapMs ?? 5 * 60_000;
  const allowlist = new Set(options.allowlist.map((login) => login.toLowerCase()));

  const source = store.ensureSource("github", repo);
  const cursor = parseCursor(source.cursor, now());
  const from = (at: string) => new Date(Date.parse(at) - overlapMs);

  const issueComments = await listUpdatedSince(gh, `repos/${repo}/issues/comments`, from(cursor.issueComments), perPage);
  const reviewComments = await listUpdatedSince(gh, `repos/${repo}/pulls/comments`, from(cursor.reviewComments), perPage);
  const issues = await listUpdatedSince(gh, `repos/${repo}/issues`, from(cursor.issues), perPage, "state=all");

  const events: GitHubEvent[] = [
    ...issueComments.map(issueCommentEvent),
    ...reviewComments.map(reviewCommentEvent),
    ...issues.map((issue) => issueStateEvent(issue, store, source.id)).filter((event): event is GitHubEvent => event !== null),
  ];
  const reviewsSince = from(cursor.issues).getTime();
  for (const issue of issues) {
    if (issue["pull_request"] == null) continue;
    const pr = field(issue, "number", "number");
    for (const review of await listPages(gh, `repos/${repo}/pulls/${pr}/reviews`, perPage)) {
      const event = reviewEvent(review, pr);
      if (event !== null && event.occurredAt.getTime() >= reviewsSince) events.push(event);
    }
  }
  events.sort((a, b) => a.occurredAt.getTime() - b.occurredAt.getTime());

  const result: PollResult = { candidates: [], duplicates: 0, droppedOwn: 0, droppedNotAllowed: 0 };
  for (const event of events) {
    const verdict = classify(event, allowlist);
    if (verdict === "own") {
      result.droppedOwn++;
      continue;
    }
    const existing = store.findThread(source.id, event.number);
    if (verdict === "not_allowed" && event.kind !== "issue_state" && (existing === null || !store.ticketsForThread(existing.id).some((ticket) => ticket.state === "open" || ticket.state === "paused"))) {
      result.droppedNotAllowed++;
      continue;
    }
    const threadId = existing?.id ?? store.ensureThread({
      sourceId: source.id,
      kind: event.threadKind,
      number: event.number,
      url: event.threadUrl,
    });
    const inserted = store.insertTrigger({
      sourceId: source.id,
      sourceEventId: event.id,
      threadId,
      kind: event.kind,
      actor: event.actor,
      url: event.url,
      occurredAt: event.occurredAt,
      ...(event.reviewId === undefined ? {} : { reviewId: event.reviewId }),
      body: event.body,
      ...(event.path === undefined ? {} : { path: event.path }),
      ...(event.line === undefined ? {} : { line: event.line }),
    });
    if (inserted.duplicate) {
      result.duplicates++;
      continue;
    }
    result.candidates.push({
      triggerId: inserted.id,
      threadId,
      kind: event.kind,
      actor: event.actor,
      url: event.url,
      body: event.body,
      occurredAt: event.occurredAt,
    });
  }

  // Only now, with every event of the pass stored, may the cursor move.
  const next: Cursor = {
    issueComments: newest(cursor.issueComments, issueComments),
    reviewComments: newest(cursor.reviewComments, reviewComments),
    issues: newest(cursor.issues, issues),
  };
  store.advanceCursor(source.id, JSON.stringify(next));
  return result;
}

// --- cursor ----------------------------------------------------------------------------------

/**
 * A source with no cursor starts at `now`: the first pass sets a baseline and reads back only the
 * overlap, rather than replaying the repository's history into the Router.
 */
function parseCursor(raw: string | null, now: Date): Cursor {
  if (raw === null) {
    const at = now.toISOString();
    return { issueComments: at, reviewComments: at, issues: at };
  }
  const parsed = JSON.parse(raw) as Partial<Cursor>;
  for (const key of ["issueComments", "reviewComments", "issues"] as const) {
    const value = parsed[key];
    if (typeof value !== "string" || Number.isNaN(Date.parse(value))) {
      throw new Error(`source cursor has no valid ${key}: ${raw}`);
    }
  }
  return parsed as Cursor;
}

function newest(current: string, items: readonly Item[]): string {
  let max = Date.parse(current);
  for (const item of items) max = Math.max(max, Date.parse(field(item, "updated_at", "string")));
  return new Date(max).toISOString();
}

// --- pagination ------------------------------------------------------------------------------

type Item = Record<string, unknown>;

/**
 * Reads every item updated at or after `since`, oldest update first. Pages are keyset, not numbered:
 * each next request restarts `since` at the last item's `updated_at`. With numbered pages an item
 * updated mid-read moves to the end of the list and shifts the rest back, so one can slip between
 * pages unseen; `since` is inclusive, so a keyset restart cannot skip one. When a full page shares a
 * single timestamp, the page number advances instead, so the loop always makes progress.
 */
async function listUpdatedSince(
  gh: GhRunner,
  path: string,
  since: Date,
  perPage: number,
  extraQuery = "",
): Promise<Item[]> {
  const byId = new Map<string, Item>();
  let from = sinceParam(since);
  let page = 1;
  for (;;) {
    const query = `sort=updated&direction=asc&per_page=${perPage}&page=${page}&since=${from}`;
    const items = asItems(await ghGet(gh, `${path}?${extraQuery ? `${extraQuery}&` : ""}${query}`), path);
    for (const item of items) byId.set(field(item, "node_id", "string"), item);
    if (items.length < perPage) break;
    const last = sinceParam(new Date(field(items.at(-1)!, "updated_at", "string")));
    if (last === from) {
      page++;
    } else {
      from = last;
      page = 1;
    }
  }
  return [...byId.values()];
}

/** Reads a numbered list to its end. Used for a PR's reviews, which only ever append. */
async function listPages(gh: GhRunner, path: string, perPage: number): Promise<Item[]> {
  const all: Item[] = [];
  for (let page = 1; ; page++) {
    const items = asItems(await ghGet(gh, `${path}?per_page=${perPage}&page=${page}`), path);
    all.push(...items);
    if (items.length < perPage) return all;
  }
}

function asItems(value: unknown, path: string): Item[] {
  if (!Array.isArray(value)) throw new Error(`GET ${path}: expected a JSON array`);
  return value as Item[];
}

// --- normalizing -----------------------------------------------------------------------------

function issueCommentEvent(item: Item): GitHubEvent {
  const url = field(item, "html_url", "string");
  return {
    id: field(item, "node_id", "string"),
    kind: "issue_comment",
    threadKind: url.includes("/pull/") ? "pull_request" : "issue",
    number: lastSegment(field(item, "issue_url", "string")),
    threadUrl: threadUrl(url),
    url,
    actor: login(item),
    body: bodyOf(item),
    occurredAt: new Date(field(item, "created_at", "string")),
  };
}

function issueStateEvent(item: Item, store: Store, sourceId: number): GitHubEvent | null {
  const state = item["state"];
  if (state !== "closed" && state !== "open") return null;
  const kind: ThreadKind = item["pull_request"] == null ? "issue" : "pull_request";
  const number = field(item, "number", "number");
  const existing = store.findThread(sourceId, number);
  if (state === "open" && (existing === null || !store.ticketsForThread(existing.id).some((ticket) => ticket.state === "ended"))) return null;
  const thread = existing ?? store.getThread(store.ensureThread({ sourceId, kind, number, url: field(item, "html_url", "string") }));
  if (state === "closed" && !store.ticketsForThread(thread.id).some((ticket) => ticket.state === "open" || ticket.state === "paused")) return null;
  const url = field(item, "html_url", "string");
  return {
    id: `${field(item, "node_id", "string")}:${state}:${field(item, "updated_at", "string")}`,
    kind: "issue_state",
    threadKind: kind,
    number,
    threadUrl: url,
    url,
    actor: login(item),
    body: state,
    occurredAt: new Date(field(item, "updated_at", "string")),
  };
}

function reviewCommentEvent(item: Item): GitHubEvent {
  const url = field(item, "html_url", "string");
  const reviewId = item["pull_request_review_id"];
  if (typeof reviewId !== "number") throw new Error("GitHub review comment is missing pull_request_review_id");
  return {
    id: field(item, "node_id", "string"),
    kind: "pull_request_review_comment",
    threadKind: "pull_request",
    number: lastSegment(field(item, "pull_request_url", "string")),
    threadUrl: threadUrl(url),
    url,
    actor: login(item),
    body: bodyOf(item),
    occurredAt: new Date(field(item, "created_at", "string")),
    reviewId: String(reviewId),
    ...(typeof item["path"] === "string" ? { path: item["path"] } : {}),
    ...(typeof item["line"] === "number" ? { line: item["line"] } : {}),
  };
}

/**
 * A submitted review, or null. Pending reviews are drafts nobody else can see. A bare COMMENTED
 * review with no body is the wrapper GitHub creates around inline comments, which arrive on their
 * own as review comments; keeping it would hand the Router an empty duplicate.
 */
function reviewEvent(item: Item, pr: number): GitHubEvent | null {
  const state = field(item, "state", "string");
  const body = bodyOf(item);
  if (state === "PENDING" || item["submitted_at"] == null) return null;
  if (state === "COMMENTED" && body.trim() === "") return null;
  const url = field(item, "html_url", "string");
  return {
    id: field(item, "node_id", "string"),
    kind: "pull_request_review",
    threadKind: "pull_request",
    number: pr,
    threadUrl: threadUrl(url),
    url,
    actor: login(item),
    body,
    occurredAt: new Date(field(item, "submitted_at", "string")),
    reviewId: String(field(item, "id", "number")),
  };
}

/** A deleted account comes back as `user: null`; an empty login matches no allowlist entry. */
function login(item: Item): string {
  const user = item["user"] as Item | null | undefined;
  return typeof user?.["login"] === "string" ? user["login"] : "";
}

function bodyOf(item: Item): string {
  return typeof item["body"] === "string" ? item["body"] : "";
}

function threadUrl(htmlUrl: string): string {
  return htmlUrl.split("#")[0]!;
}

function lastSegment(url: string): number {
  const number = Number(url.slice(url.lastIndexOf("/") + 1));
  if (!Number.isInteger(number) || number <= 0) throw new Error(`no issue number in ${url}`);
  return number;
}

function field<T extends "string" | "number">(
  item: Item,
  key: string,
  type: T,
): T extends "string" ? string : number {
  const value = item[key];
  if (typeof value !== type) throw new Error(`GitHub response is missing ${type} field ${key}`);
  return value as T extends "string" ? string : number;
}
