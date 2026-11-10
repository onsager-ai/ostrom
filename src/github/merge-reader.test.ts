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

test("the committed query contains every paginated evidence connection", () => {
  for (const field of ["reviewThreads", "reviews", "contexts", "checkSuites", "associatedPullRequests"]) assert.match(MERGE_EVIDENCE_QUERY, new RegExp(`${field}\\(first: 100`));
  assert.match(MERGE_EVIDENCE_QUERY, /mergeable mergeStateStatus reviewDecision/);
  assert.match(MERGE_EVIDENCE_QUERY, /workflowRun \{ event runAttempt workflow/);
});
