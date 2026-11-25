// The evidence read for a possible merge. This module only reads GitHub; it never sends a merge.
import { readFileSync } from "node:fs";

import { ghMergeGet, type GhRunner } from "./gh.ts";

export const MERGE_EVIDENCE_QUERY = readFileSync(new URL("./merge-evidence.graphql", import.meta.url), "utf8");

type Json = Record<string, unknown>;
type Page = { hasNextPage: boolean; endCursor: string | null };
type ConnectionState = { cursor: string | undefined; done: boolean };

export interface MergeReview {
  id: string;
  state: string;
  submittedAt: string | null;
  author: { id: string | null; login: string | null; type: string } | null;
  commitOid: string | null;
  body: string;
  inlineComments: number;
}

export interface MergeEvidence {
  pr: {
    number: number;
    state: string;
    draft: boolean;
    mergeable: string;
    mergeStateStatus: string;
    reviewDecision: string | null;
    head: { ref: string; sha: string; repository: string };
    base: { ref: string; sha: string; repository: string; rollupState: string | null };
    authorLogin: string | null;
  };
  rollup: { oid: string; state: string | null; contexts: unknown[]; suites: unknown[]; associatedPrNumbers: number[] } | null;
  threads: { id: string; resolved: boolean }[];
  reviews: MergeReview[];
  rest: { pull: Json; repository: Json; protection: unknown; rules: unknown };
}

/** Reads all pages of each connection. A missing field or changed enum shape is rejected. */
export async function readMergeEvidence(gh: GhRunner, repo: string, prNumber: number): Promise<MergeEvidence> {
  const [owner, name] = repo.split("/");
  if (!owner || !name) throw new Error(`invalid repository ${repo}`);

  const pull = object(await ghMergeGet(gh, `repos/${repo}/pulls/${prNumber}`));
  const base = object(pull, "base");
  const baseRef = string(base, "ref");
  const [repository, protection, rules] = await Promise.all([
    ghMergeGet(gh, `repos/${repo}`),
    optionalGet(gh, `repos/${repo}/branches/${encodeURIComponent(baseRef)}/protection`, true),
    optionalGet(gh, `repos/${repo}/rules/branches/${encodeURIComponent(baseRef)}`),
  ]);

  const threads: MergeEvidence["threads"] = [];
  const reviews: MergeReview[] = [];
  const contexts: unknown[] = [];
  const suites: unknown[] = [];
  const seen = { threads: new Set<string>(), reviews: new Set<string>(), contexts: new Set<string>(), suites: new Set<string>() };
  const associatedPrNumbers: number[] = [];
  const state: Record<"threadsAfter" | "reviewsAfter" | "contextsAfter" | "suitesAfter" | "associatedPrsAfter", ConnectionState> = {
    threadsAfter: { cursor: undefined, done: false }, reviewsAfter: { cursor: undefined, done: false },
    contextsAfter: { cursor: undefined, done: false }, suitesAfter: { cursor: undefined, done: false },
    associatedPrsAfter: { cursor: undefined, done: false },
  };
  let pr: MergeEvidence["pr"] | null = null;
  let rollup: MergeEvidence["rollup"] = null;

  for (;;) {
    const data = await graphql(gh, {
      owner, name, number: prNumber,
      threadsAfter: state.threadsAfter.cursor, reviewsAfter: state.reviewsAfter.cursor,
      contextsAfter: state.contextsAfter.cursor, suitesAfter: state.suitesAfter.cursor,
      associatedPrsAfter: state.associatedPrsAfter.cursor,
    });
    const node = object(object(data, "repository"), "pullRequest");
    pr ??= decodePull(node);
    const threadPage = connection(node, "reviewThreads");
    if (!state.threadsAfter.done) {
      for (const item of threadPage.nodes) {
        const row = object(item);
        if (seen.threads.add(string(row, "id"))) threads.push({ id: string(row, "id"), resolved: boolean(row, "isResolved") });
      }
      advance(state.threadsAfter, threadPage.page);
    }
    const reviewPage = connection(node, "reviews");
    if (!state.reviewsAfter.done) {
      for (const item of reviewPage.nodes) {
        const row = decodeReview(item);
        if (seen.reviews.add(row.id)) reviews.push(row);
      }
      advance(state.reviewsAfter, reviewPage.page);
    }
    const commit = object((array(object(node, "commits"), "nodes")[0] ?? {}), "commit");
    const associatedPage = connection(commit, "associatedPullRequests");
    if (!state.associatedPrsAfter.done) {
      for (const item of associatedPage.nodes) {
        const value = number(object(item), "number");
        if (!associatedPrNumbers.includes(value)) associatedPrNumbers.push(value);
      }
      advance(state.associatedPrsAfter, associatedPage.page);
    }
    const status = commit["statusCheckRollup"] === null ? null : object(commit, "statusCheckRollup");
    if (status === null) state.contextsAfter.done = true;
    else {
      const contextPage = connection(status, "contexts");
      if (!state.contextsAfter.done) {
        for (const item of contextPage.nodes) {
          const row = object(item);
          const type = string(row, "__typename");
          const key = `${type}:${type === "StatusContext" ? string(row, "context") : nullableNumber(row, "databaseId")}`;
          if (seen.contexts.add(key)) contexts.push(item);
        }
        advance(state.contextsAfter, contextPage.page);
      }
      rollup ??= { oid: string(commit, "oid"), state: nullableString(status, "state"), contexts, suites, associatedPrNumbers };
    }
    const suitePage = connection(commit, "checkSuites");
    if (!state.suitesAfter.done) {
      for (const item of suitePage.nodes) {
        const row = object(item);
        const key = String(number(row, "databaseId"));
        if (seen.suites.add(key)) suites.push(item);
      }
      advance(state.suitesAfter, suitePage.page);
    }
    if (rollup !== null) rollup.suites = suites;
    if (Object.values(state).every((connection) => connection.done)) break;
  }

  const decoded = pr!;
  decoded.head.repository = nestedString(pull, "head", "repo", "full_name");
  decoded.base.repository = nestedString(pull, "base", "repo", "full_name");
  reviews.sort((left, right) => (left.submittedAt ?? "").localeCompare(right.submittedAt ?? "") || left.id.localeCompare(right.id));
  return { pr: decoded, rollup, threads, reviews, rest: { pull, repository: object(repository), protection, rules } };
}

/** Reviewers to notify after the PR head advances beyond their latest change request. */
export function reviewersToReRequest(evidence: MergeEvidence): string[] {
  const latest = new Map<string, MergeReview>();
  for (const review of evidence.reviews) {
    const login = review.author?.login;
    if (login === null || login === undefined || login === evidence.pr.authorLogin || login.endsWith("[bot]") ||
        review.author?.type !== "User" || review.submittedAt === null) continue;
    const previous = latest.get(login);
    if (previous === undefined || (review.submittedAt > (previous.submittedAt ?? "")) ||
        (review.submittedAt === previous.submittedAt && review.id > previous.id)) latest.set(login, review);
  }
  return [...latest].flatMap(([login, review]) =>
    review.state === "CHANGES_REQUESTED" && review.commitOid !== null && review.commitOid !== evidence.pr.head.sha ? [login] : []);
}

export interface FailedCheck { name: string; url: string | null; }

/** Only explicit failures on the current PR head can wake a Worker; pending checks wait. Any failure
 * counts, because the merge needs the whole rollup to succeed, not only protection-required checks. */
export function failedChecks(evidence: MergeEvidence): FailedCheck[] {
  if (evidence.rollup === null || evidence.rollup.oid !== evidence.pr.head.sha) return [];
  const failed: FailedCheck[] = [];
  for (const value of evidence.rollup.contexts) {
    const check = object(value);
    if (check["__typename"] === "CheckRun" && check["status"] === "COMPLETED" &&
        ["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE"].includes(String(check["conclusion"]))) {
      failed.push({ name: string(check, "name"), url: typeof check["detailsUrl"] === "string" ? check["detailsUrl"] : null });
    } else if (check["__typename"] === "StatusContext" && ["FAILURE", "ERROR"].includes(String(check["state"]))) {
      failed.push({ name: string(check, "context"), url: typeof check["targetUrl"] === "string" ? check["targetUrl"] : null });
    }
  }
  return failed;
}

async function optionalGet(gh: GhRunner, path: string, classicProtection = false): Promise<unknown> {
  try { return await ghMergeGet(gh, path); }
  catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    // execGh retains gh's exact HTTP error message. Only this endpoint's explicit absence of
    // classic protection is safe to normalize; Not Found can hide a permission denial.
    if (classicProtection && /: gh: Branch not protected \(HTTP 404\)$/.test(message)) return {};
    return { unavailable: message };
  }
}

async function graphql(gh: GhRunner, variables: Record<string, string | number | undefined>): Promise<Json> {
  const args: string[] = ["api", "graphql", "-f", `query=${MERGE_EVIDENCE_QUERY}`];
  for (const [key, value] of Object.entries(variables)) {
    if (value !== undefined) args.push(typeof value === "number" ? "-F" : "-f", `${key}=${value}`);
  }
  return object(JSON.parse(await gh(args)), "data");
}

function decodePull(row: Json): MergeEvidence["pr"] {
  const author = row["author"] === null ? null : object(row, "author");
  const baseSha = string(row, "baseRefOid");
  const baseRef = row["baseRef"] === null ? null : object(row, "baseRef");
  const baseTarget = baseRef === null || baseRef["target"] === null ? null : object(baseRef, "target");
  // A closed PR can retain its old base SHA after the branch has advanced.
  const baseRollup = baseTarget !== null && string(baseTarget, "oid") === baseSha && baseTarget["statusCheckRollup"] !== null
    ? object(baseTarget, "statusCheckRollup") : null;
  return {
    number: number(row, "number"), state: string(row, "state"), draft: boolean(row, "isDraft"),
    mergeable: string(row, "mergeable"), mergeStateStatus: string(row, "mergeStateStatus"), reviewDecision: nullableString(row, "reviewDecision"),
    head: { ref: string(row, "headRefName"), sha: string(row, "headRefOid"), repository: "" },
    base: { ref: string(row, "baseRefName"), sha: baseSha, repository: "", rollupState: baseRollup === null ? null : string(baseRollup, "state") },
    authorLogin: author === null ? null : nullableString(author, "login"),
  };
}

function decodeReview(value: unknown): MergeReview {
  const row = object(value);
  const author = row["author"] === null ? null : object(row, "author");
  const commit = row["commit"] === null ? null : object(row, "commit");
  return {
    id: string(row, "id"), state: string(row, "state"), submittedAt: nullableString(row, "submittedAt"),
    author: author === null ? null : { id: nullableString(author, "id"), login: nullableString(author, "login"), type: string(author, "__typename") },
    commitOid: commit === null ? null : nullableString(commit, "oid"),
    body: string(row, "body"), inlineComments: number(object(row, "comments"), "totalCount"),
  };
}

function connection(row: Json, key: string): { nodes: unknown[]; page: Page } {
  const value = object(row, key);
  const page = object(value, "pageInfo");
  return { nodes: array(value, "nodes"), page: { hasNextPage: boolean(page, "hasNextPage"), endCursor: nullableString(page, "endCursor") } };
}
function advance(state: ConnectionState, page: Page): void {
  if (page.hasNextPage) {
    if (page.endCursor === null) throw new Error("GitHub page has no end cursor");
    state.cursor = page.endCursor;
  } else {
    state.cursor = page.endCursor ?? state.cursor;
    state.done = true;
  }
}
function nestedString(value: Json, ...keys: string[]): string { let current: unknown = value; for (const key of keys) current = object(current)[key]; if (typeof current !== "string") throw new Error(`GitHub response is missing string ${keys.join(".")}`); return current; }
function object(value: unknown, key?: string): Json { const result = key === undefined ? value : (value as Json)[key]; if (result === null || typeof result !== "object" || Array.isArray(result)) throw new Error(`GitHub response is missing object ${key ?? "value"}`); return result as Json; }
function array(value: Json, key: string): unknown[] { if (!Array.isArray(value[key])) throw new Error(`GitHub response is missing array ${key}`); return value[key] as unknown[]; }
function string(value: Json, key: string): string { if (typeof value[key] !== "string") throw new Error(`GitHub response is missing string ${key}`); return value[key] as string; }
function nullableString(value: Json, key: string): string | null { if (value[key] !== null && typeof value[key] !== "string") throw new Error(`GitHub response has invalid string ${key}`); return value[key] as string | null; }
function number(value: Json, key: string): number { if (typeof value[key] !== "number") throw new Error(`GitHub response is missing number ${key}`); return value[key] as number; }
function nullableNumber(value: Json, key: string): number | null { if (value[key] !== null && typeof value[key] !== "number") throw new Error(`GitHub response has invalid number ${key}`); return value[key] as number | null; }
function boolean(value: Json, key: string): boolean { if (typeof value[key] !== "boolean") throw new Error(`GitHub response is missing boolean ${key}`); return value[key] as boolean; }
