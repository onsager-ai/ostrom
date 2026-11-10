import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { MERGE_EVIDENCE_QUERY, readMergeEvidence } from "./merge-reader.ts";
import type { GhRunner } from "./gh.ts";

const fixture = (name: string) => JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8")) as unknown;

test("reader decodes the recorded sandbox GraphQL and pinned REST shapes", async () => {
  const graphql = fixture("merge-reader-graphql.json");
  const pull = fixture("merge-reader-pr.json");
  const repository = fixture("merge-reader-repository.json");
  const protection = fixture("merge-reader-protection.json");
  const rules = fixture("merge-reader-rules.json");
  const calls: string[][] = [];
  const gh: GhRunner = async (args) => {
    calls.push([...args]);
    if (args[1] === "graphql") return JSON.stringify(graphql);
    const path = args.at(-1)!;
    if (path.endsWith("/pulls/8")) return JSON.stringify(pull);
    if (path === "repos/onsager-ai/ostrom-sandbox") return JSON.stringify(repository);
    if (path.includes("/protection")) return JSON.stringify(protection);
    return JSON.stringify(rules);
  };
  const evidence = await readMergeEvidence(gh, "onsager-ai/ostrom-sandbox", 8);
  assert.equal(evidence.pr.head.sha, "fd2d929802c33741c934f5c2acd7c9fc7008e235");
  assert.equal(evidence.rollup?.state, "SUCCESS");
  assert.equal(evidence.rollup?.contexts.length, 1);
  assert.equal(evidence.rollup?.suites.length, 3);
  assert.equal(calls.filter((call) => call[1] === "graphql").length, 1);
  assert.equal(calls.filter((call) => call[1] !== "graphql").every((call) => call.includes("X-GitHub-Api-Version: 2022-11-28")), true);
});

test("reader finishes connections with different page counts without refetching page one", async () => {
  const pull = { head: { ref: "branch", sha: "head", repo: { full_name: "o/r" } }, base: { ref: "main", sha: "base", repo: { full_name: "o/r" } } };
  const rest = { allow_squash_merge: true };
  const page = (after: string | undefined, items: readonly unknown[], prefix: string) => {
    const index = after === undefined ? 0 : Number(after.slice(prefix.length));
    return { nodes: items[index] === undefined ? [] : [items[index]], pageInfo: { hasNextPage: index + 1 < items.length, endCursor: `${prefix}${index + 1}` } };
  };
  const calls: string[][] = [];
  const gh: GhRunner = async (args) => {
    calls.push([...args]);
    if (args[1] !== "graphql") return JSON.stringify(args.at(-1) === "repos/o/r" ? rest : pull);
    const value = (key: string) => args.find((arg) => arg.startsWith(`${key}=`))?.slice(key.length + 1);
    const threads = page(value("threadsAfter"), [{ id: "t1", isResolved: true }, { id: "t2", isResolved: true }], "t");
    const reviews = page(value("reviewsAfter"), [
      { id: "r1", state: "COMMENTED", submittedAt: "2026-01-01T00:00:00Z", author: { __typename: "User", id: "u", login: "a" }, commit: { oid: "head" } },
      { id: "r2", state: "COMMENTED", submittedAt: "2026-01-02T00:00:00Z", author: { __typename: "User", id: "u", login: "a" }, commit: { oid: "head" } },
      { id: "r3", state: "COMMENTED", submittedAt: "2026-01-03T00:00:00Z", author: { __typename: "User", id: "u", login: "a" }, commit: { oid: "head" } },
    ], "r");
    const contexts = page(value("contextsAfter"), [], "c");
    const suites = page(value("suitesAfter"), [], "s");
    const associated = page(value("associatedPrsAfter"), [{ number: 1 }], "p");
    return JSON.stringify({ data: { repository: { pullRequest: {
      number: 1, state: "OPEN", isDraft: false, mergeable: "MERGEABLE", mergeStateStatus: "CLEAN", reviewDecision: null,
      headRefName: "branch", headRefOid: "head", baseRefName: "main", baseRefOid: "base",
      reviewThreads: threads, reviews, commits: { nodes: [{ commit: { oid: "head", associatedPullRequests: associated,
        statusCheckRollup: { state: "SUCCESS", contexts }, checkSuites: suites } }] },
    } } } });
  };
  const evidence = await readMergeEvidence(gh, "o/r", 1);
  assert.equal(calls.filter((call) => call[1] === "graphql").length, 3);
  assert.deepEqual(evidence.threads.map((thread) => thread.id), ["t1", "t2"]);
  assert.deepEqual(evidence.reviews.map((review) => review.id), ["r1", "r2", "r3"]);
  assert.deepEqual(evidence.rollup?.associatedPrNumbers, [1]);
});

test("the committed query contains every paginated evidence connection", () => {
  for (const field of ["reviewThreads", "reviews", "contexts", "checkSuites", "associatedPullRequests"]) assert.match(MERGE_EVIDENCE_QUERY, new RegExp(`${field}\\(first: 100`));
  assert.match(MERGE_EVIDENCE_QUERY, /mergeable mergeStateStatus reviewDecision/);
  assert.match(MERGE_EVIDENCE_QUERY, /workflowRun \{ event runAttempt workflow/);
});
