import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import { FakeGitHub } from "../github/fake-github.ts";
import { Store } from "../store/store.ts";
import { observe, pullRequestsForBranch, workerBrief } from "./dispatch.ts";

const REPO = "onsager-ai/ostrom";
const stores: Store[] = [];

afterEach(() => {
  while (stores.length > 0) stores.pop()!.close();
});

function fixture() {
  const store = Store.open(":memory:", { now: () => new Date("2026-11-09T12:00:00.000Z") });
  stores.push(store);
  const github = new FakeGitHub(() => new Date("2026-11-09T12:00:00.000Z"));
  const source = store.ensureSource("github", REPO);
  const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: `https://github.com/${REPO}/issues/7` });
  const trigger = store.insertTrigger({
    sourceId: source.id,
    sourceEventId: "IC_1",
    threadId: issue,
    kind: "issue_comment",
    actor: "alice",
    url: `https://github.com/${REPO}/issues/7#issuecomment-1`,
    occurredAt: new Date("2026-11-09T11:59:00.000Z"),
  }).id;
  const { ticketId } = store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "fix the README typo" });
  const worker = (sessionId: string) => {
    const id = store.createAssignment(ticketId!);
    store.configureNativeAssignment(id, { harness: "codex", model: "m", worktree: `/wt/${id}`, branch: `ostrom/ticket-${ticketId}-${id}`, machine: "h" });
    store.startNativeAssignment(id, sessionId);
    return id;
  };
  return { store, github, ticketId: ticketId!, worker };
}

test("the brief is a summary plus pointers and asks for a PR that references the issue", () => {
  const { store, ticketId } = fixture();
  const brief = workerBrief(store.ticketContext(ticketId), "ostrom/ticket-1-1", "main");
  assert.match(brief, /fix the README typo/);
  assert.match(brief, new RegExp(`issue #7: https://github.com/${REPO}/issues/7\n`));
  assert.match(brief, new RegExp(`- https://github.com/${REPO}/issues/7#issuecomment-1\n`));
  assert.match(brief, /branch ostrom\/ticket-1-1/);
  assert.match(brief, new RegExp(`pull request\non ${REPO} against main whose description says "Refs #7"`));
});

test("the brief tells an unattended Worker never to block on a question or work around a failed commit", () => {
  const { store, ticketId } = fixture();
  const brief = workerBrief(store.ticketContext(ticketId), "b", "main");
  assert.match(brief, /never stop to ask an interactive question/);
  assert.match(brief, /do not create or push commit objects with Git plumbing/);
});

test("the machine's house rules close the brief when configured, and are absent otherwise", () => {
  const { store, ticketId } = fixture();
  const context = store.ticketContext(ticketId);
  assert.match(workerBrief(context, "b", "main", "Run tests in CI only."), /House rules for this machine, which you must follow:\nRun tests in CI only\.$/);
  assert.doesNotMatch(workerBrief(context, "b", "main"), /House rules/);
});

test("a branch's pull requests are read from GitHub with their state", async () => {
  const { github } = fixture();
  github.pulls.push({ branch: "ostrom/ticket-1-1", number: 12, state: "open" }, { branch: "ostrom/ticket-1-1", number: 11, state: "merged" },
    { branch: "ostrom/ticket-1-1", number: 13, state: "closed" }, { branch: "other", number: 14, state: "open" });
  assert.deepEqual(
    (await pullRequestsForBranch(github.gh, REPO, "ostrom/ticket-1-1")).map((p) => [p.number, p.state]),
    [[11, "merged"], [12, "open"], [13, "closed"]],
  );
});

test("observe subscribes the ticket to the Worker's PRs, once across passes", async () => {
  const { store, github, ticketId, worker } = fixture();
  const id = worker("sess-1");
  const branch = `ostrom/ticket-${ticketId}-${id}`;

  assert.deepEqual((await observe(store, github.gh, REPO)).observed[0]?.pullRequests, []);

  github.pulls.push({ branch, number: 12, state: "open" });
  const first = await observe(store, github.gh, REPO);
  assert.deepEqual(first, {
    observed: [{ assignmentId: id, sessionId: "sess-1", pullRequests: [12], newlySubscribed: [12], outcome: null }],
    failed: [],
  });
  const prThread = store.ensureThread({ sourceId: store.ensureSource("github", REPO).id, kind: "pull_request", number: 12, url: `https://github.com/${REPO}/pull/12` });
  assert.deepEqual(store.ticketsForThread(prThread).map((t) => t.id), [ticketId]);

  const second = await observe(store, github.gh, REPO);
  assert.deepEqual(second.observed[0]?.newlySubscribed, []);
});

test("a newly observed PR refreshes the status card with its link", async () => {
  const { store, github, ticketId, worker } = fixture();
  const id = worker("sess-1");
  github.pulls.push({ branch: `ostrom/ticket-${ticketId}-${id}`, number: 12, state: "open" });
  await observe(store, github.gh, REPO);
  const cards = store.operatorTicket(ticketId).outbox.filter((reply) => reply.id.startsWith("card-thread-"));
  assert.equal(cards.length, 1);
  assert.match(cards[0]!.body, /^PR: https:\/\/github\.com\/onsager-ai\/ostrom\/pull\/12$/m);
});

test("observe reports the outcome the Worker's turns recorded and leaves it alone", async () => {
  const { store, github, worker } = fixture();
  const id = worker("sess-1");
  store.recordOutcome(id, "failed");
  const pass = await observe(store, github.gh, REPO);
  assert.equal(pass.observed[0]?.outcome, "failed");
  assert.equal(store.getAssignment(id).outcome, "failed");
});

test("a branch that cannot be read fails that assignment and the pass goes on", async () => {
  const { store, github, worker } = fixture();
  const lost = worker("sess-1");
  const kept = worker("sess-2");
  github.failWhen = (args) => (args.join(" ").includes(`ticket-1-${lost}&`) ? new Error("gh api failed: boom") : undefined);
  const pass = await observe(store, github.gh, REPO);
  assert.deepEqual(pass.failed, [{ assignmentId: lost, error: "gh api failed: boom" }]);
  assert.deepEqual(pass.observed.map((o) => o.assignmentId), [kept]);
});
