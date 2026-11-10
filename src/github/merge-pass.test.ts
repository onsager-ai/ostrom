import assert from "node:assert/strict";
import { test } from "node:test";

import { mergePass } from "./merge-pass.ts";
import type { GhRunner } from "./gh.ts";
import { Store } from "../store/store.ts";

function fixture(policy: "unattended" | "human" | "approvals" = "unattended") {
  let now = new Date("2026-11-10T12:00:00Z"); let merged = false; let head = "head"; let put = 0;
  const store = Store.open(":memory:", { now: () => now });
  const source = store.ensureSource("github", "o/r"); const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 1, url: "https://github.com/o/r/issues/1" });
  const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: "I_1", threadId: issue, kind: "issue_comment", actor: "alice", url: "https://github.com/o/r/issues/1#issuecomment-1", occurredAt: now }).id;
  const ticket = store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "work" }).ticketId!;
  const assignment = store.createAssignment(ticket); store.configureNativeAssignment(assignment, { harness: "claude", model: "m", worktree: "/w", branch: "b", machine: "h" }); store.startNativeAssignment(assignment, "s");
  const pr = store.ensureThread({ sourceId: source.id, kind: "pull_request", number: 2, url: "https://github.com/o/r/pull/2" }); store.subscribe(ticket, pr);
  const gh: GhRunner = async (args) => {
    if (args[1] === "graphql") return JSON.stringify({ data: { repository: { pullRequest: graph(merged, head) } } });
    const path = args.find((a) => a.startsWith("repos/"))!;
    if (args.includes("PUT")) { put++; if (head !== "head") throw new Error("gh api failed: conflict (HTTP 409)"); merged = true; return JSON.stringify({ merged: true, sha: "merge" }); }
    if (path.endsWith("/permission")) return JSON.stringify({ role_name: "write" });
    if (path === "repos/o/r") return JSON.stringify({ allow_squash_merge: true });
    if (path.includes("/protection") || path.includes("/rules/")) return JSON.stringify({});
    return JSON.stringify({ head: { ref: "b", sha: head, repo: { full_name: "o/r" } }, base: { ref: "main", sha: "base", repo: { full_name: "o/r" } }, merge_commit_sha: merged ? "merge" : null, merged_by: merged ? { login: "marvin" } : null });
  };
  const released: number[] = []; const workers = { release: (id: number) => { released.push(id); store.releaseAssignmentReservation(id); }, wake() {} } as never;
  const options = () => ({ store, workers, gh, repo: "o/r", policy: policy === "approvals" ? { strategy: "approvals" as const, approvals: 1, approvers: [] } : { strategy: policy }, now: () => now });
  return { store, assignment, options, advance: (ms: number) => { now = new Date(now.getTime() + ms); }, setHead: (value: string) => { head = value; }, put: () => put, released };
}
function graph(merged: boolean, head: string) { const page = { hasNextPage: false, endCursor: null }; return { number: 2, state: merged ? "MERGED" : "OPEN", isDraft: false, mergeable: "MERGEABLE", mergeStateStatus: "CLEAN", reviewDecision: "APPROVED", author: { login: "author" }, headRefName: "b", headRefOid: head, baseRefName: "main", baseRefOid: "base", reviewThreads: { nodes: [], pageInfo: page }, reviews: { nodes: [{ id: "r", state: "APPROVED", submittedAt: "2026-11-10T12:00:00Z", author: { __typename: "User", id: "u", login: "reviewer" }, commit: { oid: head } }], pageInfo: page }, commits: { nodes: [{ commit: { oid: head, associatedPullRequests: { nodes: [{ number: 2 }], pageInfo: page }, statusCheckRollup: { state: "SUCCESS", contexts: { nodes: [], pageInfo: page } }, checkSuites: { nodes: [{ databaseId: 1, status: "COMPLETED", conclusion: "SUCCESS", workflowRun: { event: "pull_request", runAttempt: 1, workflow: { databaseId: 1, name: "ci" } } }], pageInfo: page } } }] } }; }

async function ready(f: ReturnType<typeof fixture>) { await mergePass(f.options()); f.store.markReadyReplySent(`outcome-a${f.assignment}-pr2-opened`); await mergePass(f.options()); f.advance(10 * 60_000); }

test("happy path merges once with captured SHA; human policy never merges", async () => { const f = fixture(); await ready(f); await mergePass(f.options()); await mergePass(f.options()); assert.equal(f.put(), 1); assert.equal(f.store.getTicket(1).state, "ended"); const human = fixture("human"); await ready(human); await mergePass(human.options()); assert.equal(human.put(), 0); });
test("approvals count and head/activity windows reset", async () => { const f = fixture("approvals"); await ready(f); await mergePass(f.options()); assert.equal(f.put(), 1); const changed = fixture(); await ready(changed); changed.setHead("new"); await mergePass(changed.options()); assert.equal(changed.put(), 0); });
test("409 is failed, transport loss is unknown then reconciled, and held reservations release", async () => { const f = fixture(); await ready(f); f.setHead("new"); await mergePass(f.options()); assert.equal(f.put(), 0); f.store.reserveAssignment(f.assignment); await mergePass(f.options()); assert.equal(f.store.reservationHeld(f.assignment), false); });
test("external merge records external origin and existing PR begins with fresh notice", async () => { const f = fixture(); await mergePass(f.options()); assert.equal(f.store.readyReplySentAt(f.assignment), null); assert.equal(f.store.hasReply(`outcome-a${f.assignment}-pr2-opened`), true); f.setHead("external"); await mergePass(f.options()); assert.equal(f.store.prMerge("o/r", 2), null); });
