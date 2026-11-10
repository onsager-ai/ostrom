// The evidence read for a possible merge. This module only reads GitHub; it never sends a merge.
import { readFileSync } from "node:fs";

import { ghMergeGet, type GhRunner } from "./gh.ts";

export const MERGE_EVIDENCE_QUERY = readFileSync(new URL("./merge-evidence.graphql", import.meta.url), "utf8");

type Json = Record<string, unknown>;
type Page = { hasNextPage: boolean; endCursor: string | null };

export interface MergeReview {
  id: string;
  state: string;
  submittedAt: string | null;
  author: { id: string | null; login: string | null; type: string } | null;
  commitOid: string | null;
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
    base: { ref: string; sha: string; repository: string };
  };
  rollup: { oid: string; state: string | null; contexts: unknown[]; suites: unknown[]; associatedPrNumbers: number[] } | null;
  threads: { id: string; resolved: boolean }[];
  reviews: MergeReview[];
  rest: { pull: Json; repository: Json; protection: unknown; rules: unknown };
}

/** Reads all pages of each connection. A missing field or changed enum shape is rejected. */
export async function readMergeEvidence(gh: GhRunner, repo: string, number: number): Promise<MergeEvidence> {
  const [owner, name] = repo.split("/");
  if (!owner || !name) throw new Error(`invalid repository ${repo}`);

  const pull = object(await ghMergeGet(gh, `repos/${repo}/pulls/${number}`));
  const base = object(pull, "base");
  const baseRef = string(base, "ref");
  const [repository, protection, rules] = await Promise.all([
    ghMergeGet(gh, `repos/${repo}`),
    optionalGet(gh, `repos/${repo}/branches/${encodeURIComponent(baseRef)}/protection`),
    optionalGet(gh, `repos/${repo}/rules/branches/${encodeURIComponent(baseRef)}`),
  ]);

  const threads: MergeEvidence["threads"] = [];
  const reviews: MergeReview[] = [];
  const contexts: unknown[] = [];
  const suites: unknown[] = [];
  const seen = { threads: new Set<string>(), reviews: new Set<string>(), contexts: new Set<string>(), suites: new Set<string>() };
  const associatedPrNumbers: number[] = [];
  let cursors: Record<"threadsAfter" | "reviewsAfter" | "contextsAfter" | "suitesAfter" | "associatedPrsAfter", string | undefined> = {};
  let pr: MergeEvidence["pr"] | null = null;
  let rollup: MergeEvidence["rollup"] = null;

  for (;;) {
    const data = await graphql(gh, { owner, name, number, ...cursors });
    const node = object(object(data, "repository"), "pullRequest");
    pr ??= decodePull(node);
    const threadPage = connection(node, "reviewThreads");
    for (const item of threadPage.nodes) {
      const row = object(item);
      if (seen.threads.add(string(row, "id"))) threads.push({ id: string(row, "id"), resolved: boolean(row, "isResolved") });
    }
    const reviewPage = connection(node, "reviews");
    for (const item of reviewPage.nodes) {
      const row = decodeReview(item);
      if (seen.reviews.add(row.id)) reviews.push(row);
    }
    const commit = object((array(object(node, "commits"), "nodes")[0] ?? {}), "commit");
    const associatedPage = connection(commit, "associatedPullRequests");
    for (const item of associatedPage.nodes) associatedPrNumbers.push(number(object(item), "number"));
    const status = commit["statusCheckRollup"] === null ? null : object(commit, "statusCheckRollup");
    if (status !== null) {
      const contextPage = connection(status, "contexts");
      for (const item of contextPage.nodes) {
        const row = object(item);
        const key = `${string(row, "__typename")}:${nullableNumber(row, "databaseId") ?? nullableString(row, "context") ?? ""}`;
        if (seen.contexts.add(key)) contexts.push(item);
      }
      rollup ??= { oid: string(commit, "oid"), state: nullableString(status, "state"), contexts, suites, associatedPrNumbers };
      cursors.contextsAfter = next(contextPage.page);
    }
    const suitePage = connection(commit, "checkSuites");
    for (const item of suitePage.nodes) {
      const row = object(item);
      const key = String(number(row, "databaseId"));
      if (seen.suites.add(key)) suites.push(item);
    }
    if (rollup !== null) rollup.suites = suites;
    cursors.threadsAfter = next(threadPage.page);
    cursors.reviewsAfter = next(reviewPage.page);
    cursors.suitesAfter = next(suitePage.page);
    cursors.associatedPrsAfter = next(associatedPage.page);
    if (Object.values(cursors).every((cursor) => cursor === undefined)) break;
  }

  const decoded = pr!;
  decoded.head.repository = nestedString(pull, "head", "repo", "full_name");
  decoded.base.repository = nestedString(pull, "base", "repo", "full_name");
  reviews.sort((left, right) => (left.submittedAt ?? "").localeCompare(right.submittedAt ?? "") || left.id.localeCompare(right.id));
  return { pr: decoded, rollup, threads, reviews, rest: { pull, repository: object(repository), protection, rules } };
}

async function optionalGet(gh: GhRunner, path: string): Promise<unknown> {
  try { return await ghMergeGet(gh, path); }
  catch (error) { return { unavailable: error instanceof Error ? error.message : String(error) }; }
}

async function graphql(gh: GhRunner, variables: Record<string, string | number | undefined>): Promise<Json> {
  const args: string[] = ["api", "graphql", "-f", `query=${MERGE_EVIDENCE_QUERY}`];
  for (const [key, value] of Object.entries(variables)) {
    if (value !== undefined) args.push(typeof value === "number" ? "-F" : "-f", `${key}=${value}`);
  }
  return object(JSON.parse(await gh(args)), "data");
}

function decodePull(row: Json): MergeEvidence["pr"] {
  return {
    number: number(row, "number"), state: string(row, "state"), draft: boolean(row, "isDraft"),
    mergeable: string(row, "mergeable"), mergeStateStatus: string(row, "mergeStateStatus"), reviewDecision: nullableString(row, "reviewDecision"),
    head: { ref: string(row, "headRefName"), sha: string(row, "headRefOid"), repository: "" },
    base: { ref: string(row, "baseRefName"), sha: string(row, "baseRefOid"), repository: "" },
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
  };
}

function connection(row: Json, key: string): { nodes: unknown[]; page: Page } {
  const value = object(row, key);
  const page = object(value, "pageInfo");
  return { nodes: array(value, "nodes"), page: { hasNextPage: boolean(page, "hasNextPage"), endCursor: nullableString(page, "endCursor") } };
}
function next(page: Page): string | undefined { if (!page.hasNextPage) return undefined; if (page.endCursor === null) throw new Error("GitHub page has no end cursor"); return page.endCursor; }
function nestedString(value: Json, ...keys: string[]): string { let current: unknown = value; for (const key of keys) current = object(current)[key]; if (typeof current !== "string") throw new Error(`GitHub response is missing string ${keys.join(".")}`); return current; }
function object(value: unknown, key?: string): Json { const result = key === undefined ? value : (value as Json)[key]; if (result === null || typeof result !== "object" || Array.isArray(result)) throw new Error(`GitHub response is missing object ${key ?? "value"}`); return result as Json; }
function array(value: Json, key: string): unknown[] { if (!Array.isArray(value[key])) throw new Error(`GitHub response is missing array ${key}`); return value[key] as unknown[]; }
function string(value: Json, key: string): string { if (typeof value[key] !== "string") throw new Error(`GitHub response is missing string ${key}`); return value[key] as string; }
function nullableString(value: Json, key: string): string | null { if (value[key] !== null && typeof value[key] !== "string") throw new Error(`GitHub response has invalid string ${key}`); return value[key] as string | null; }
function number(value: Json, key: string): number { if (typeof value[key] !== "number") throw new Error(`GitHub response is missing number ${key}`); return value[key] as number; }
function nullableNumber(value: Json, key: string): number | null { if (value[key] !== null && typeof value[key] !== "number") throw new Error(`GitHub response has invalid number ${key}`); return value[key] as number | null; }
function boolean(value: Json, key: string): boolean { if (typeof value[key] !== "boolean") throw new Error(`GitHub response is missing boolean ${key}`); return value[key] as boolean; }
