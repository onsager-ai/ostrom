import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";
import { Store } from "../store/store.ts";
import { FakeGitHub } from "./fake-github.ts";
import { replyMarker } from "./marker.ts";
import { classify, pollOnce, type PollOptions } from "./poller.ts";

const T0 = Date.parse("2026-11-09T12:00:00.000Z");
const at = (minutes: number) => new Date(T0 + minutes * 60_000);

/** A store and a fake GitHub sharing a clock; the first pass has already set the baseline at T0. */
async function fixture() {
  let now = at(0);
  const clock = () => now;
  const db = new DatabaseSync(":memory:", { enableForeignKeyConstraints: true });
  const store = new Store(db, { now: clock });
  const github = new FakeGitHub(clock);
  const poll = (extra: Partial<PollOptions> = {}) =>
    pollOnce({ store, gh: github.gh, repo: github.repo, allowlist: ["Alice", "bob"], now: clock, ...extra });
  const baseline = await poll();
  assert.equal(baseline.candidates.length, 0);
  const cursor = () => JSON.parse(store.ensureSource("github", github.repo).cursor!) as Record<string, string>;
  const count = (table: string) => Number(db.prepare(`SELECT COUNT(*) AS n FROM "${table}"`).get()!["n"]);
  return { db, store, github, poll, cursor, count, setNow: (minutes: number) => (now = at(minutes)) };
}

test("the first pass sets a baseline instead of replaying history", async () => {
  let now = at(0);
  const store = Store.open(":memory:", { now: () => now });
  const github = new FakeGitHub(() => now);
  github.issue(1);
  github.comment(1, { login: "alice", body: "old", at: at(-60) });
  const result = await pollOnce({ store, gh: github.gh, repo: github.repo, allowlist: ["alice"], now: () => now });
  assert.equal(result.candidates.length, 0);
  assert.deepEqual(JSON.parse(store.ensureSource("github", github.repo).cursor!), {
    issueComments: at(0).toISOString(),
    reviewComments: at(0).toISOString(),
    issues: at(0).toISOString(),
  });
});

test("a pass stores all three comment kinds as triggers on their threads, in time order", async () => {
  const { db, github, poll, setNow } = await fixture();
  github.issue(7);
  github.issue(8, true);
  github.review(8, { login: "bob", body: "please rename", state: "CHANGES_REQUESTED", at: at(3) });
  github.comment(7, { login: "alice", body: "can you fix this?", at: at(1) });
  github.reviewComment(8, { login: "alice", body: "nit", at: at(2) });
  github.comment(8, { login: "alice", body: "on the PR", at: at(4) });
  setNow(10);

  const result = await poll();
  assert.deepEqual(
    result.candidates.map((c) => [c.kind, c.actor, c.body]),
    [
      ["issue_comment", "alice", "can you fix this?"],
      ["pull_request_review_comment", "alice", "nit"],
      ["pull_request_review", "bob", "please rename"],
      ["issue_comment", "alice", "on the PR"],
    ],
  );
  const threads = db.prepare("SELECT id, kind, number, url FROM thread ORDER BY number").all();
  assert.deepEqual(
    threads.map((t) => [t["kind"], t["number"], t["url"]]),
    [
      ["issue", 7, "https://github.com/onsager-ai/ostrom/issues/7"],
      ["pull_request", 8, "https://github.com/onsager-ai/ostrom/pull/8"],
    ],
  );
  const triggers = db.prepare(`SELECT source_event_id, thread_id, url FROM "trigger" ORDER BY id`).all();
  assert.equal(triggers[0]!["thread_id"], threads[0]!["id"]);
  assert.match(String(triggers[0]!["source_event_id"]), /^IC_/);
  assert.match(String(triggers[1]!["source_event_id"]), /^PRRC_/);
  assert.match(String(triggers[2]!["source_event_id"]), /^PRR_/);
  assert.deepEqual(
    result.candidates.map((c) => c.triggerId),
    db.prepare(`SELECT id FROM "trigger" ORDER BY id`).all().map((r) => r["id"]),
  );
});

test("Ostrom's own replies and actors off the allowlist are dropped and not stored", async () => {
  const { github, poll, count, setNow } = await fixture();
  github.issue(7);
  github.issue(9);
  github.comment(7, { login: "alice", body: `On it.\n\n${replyMarker("ack-1")}`, at: at(1) });
  github.comment(9, { login: "mallory", body: "do something", at: at(2) });
  github.comment(9, { login: null, body: "ghost", at: at(3) });
  github.comment(7, { login: "BOB", body: "case does not matter", at: at(4) });
  setNow(10);

  const result = await poll();
  assert.deepEqual(result.candidates.map((c) => c.body), ["case does not matter"]);
  assert.equal(result.droppedOwn, 1);
  assert.equal(result.droppedNotAllowed, 2);
  assert.equal(count("trigger"), 1);
  assert.equal(count("thread"), 1, "a thread with only dropped events is not created");
});

test("classify checks the marker before the allowlist", () => {
  const allow = new Set(["alice"]);
  assert.equal(classify({ actor: "alice", body: replyMarker("x") }, allow), "own");
  assert.equal(classify({ actor: "mallory", body: replyMarker("x") }, allow), "own");
  assert.equal(classify({ actor: "mallory", body: "hi" }, allow), "not_allowed");
  assert.equal(classify({ actor: "alice", body: "hi" }, allow), "keep");
});

test("re-reading the overlap is absorbed by dedup, and the cursor moves per stream", async () => {
  const { github, poll, cursor, setNow } = await fixture();
  github.issue(7);
  github.comment(7, { login: "alice", body: "one", at: at(1) });
  setNow(2);
  assert.equal((await poll()).candidates.length, 1);
  assert.equal(cursor()["issueComments"], at(1).toISOString());
  assert.equal(cursor()["reviewComments"], at(0).toISOString(), "an empty stream keeps its cursor");

  github.comment(7, { login: "alice", body: "two", at: at(3) });
  setNow(4);
  const second = await poll();
  assert.deepEqual(second.candidates.map((c) => c.body), ["two"]);
  assert.equal(second.duplicates, 1, "the first comment is inside the overlap and deduplicated");
  assert.equal(cursor()["issueComments"], at(3).toISOString());

  setNow(30);
  const third = await poll();
  assert.equal(third.candidates.length, 0);
});

test("a crash before the cursor advances replays the pass without storing anything twice", async () => {
  const { store, github, poll, cursor, count, setNow } = await fixture();
  github.issue(7);
  github.comment(7, { login: "alice", body: "one", at: at(1) });
  github.comment(7, { login: "bob", body: "two", at: at(2) });
  setNow(10);
  const before = cursor();

  const advance = store.advanceCursor.bind(store);
  store.advanceCursor = () => {
    throw new Error("crash");
  };
  await assert.rejects(poll(), /crash/);
  store.advanceCursor = advance;
  assert.deepEqual(cursor(), before, "the cursor did not move");
  assert.equal(count("trigger"), 2, "the events were stored before the crash");

  github.comment(7, { login: "alice", body: "three", at: at(11) });
  setNow(12);
  const replay = await poll();
  assert.deepEqual(replay.candidates.map((c) => c.body), ["three"]);
  assert.equal(replay.duplicates, 2);
  assert.equal(count("trigger"), 3);
  assert.equal(cursor()["issueComments"], at(11).toISOString());
});

test("a GitHub failure mid-pass leaves the cursor alone, so the next pass drops nothing", async () => {
  const { github, poll, cursor, setNow } = await fixture();
  github.issue(7);
  github.issue(8, true);
  github.comment(7, { login: "alice", body: "one", at: at(1) });
  github.review(8, { login: "bob", body: "looks good", state: "APPROVED", at: at(2) });
  setNow(10);
  const before = cursor();

  github.failWhen = (args) => (args.some((a) => a.includes("/reviews")) ? new Error("HTTP 502") : undefined);
  await assert.rejects(poll(), /HTTP 502/);
  assert.deepEqual(cursor(), before);

  github.failWhen = undefined;
  const retry = await poll();
  assert.deepEqual(retry.candidates.map((c) => c.body), ["one", "looks good"]);
});

test("pagination reads every page, including a full page sharing one timestamp", async () => {
  const { github, poll, setNow } = await fixture();
  github.issue(7);
  for (let i = 1; i <= 5; i++) github.comment(7, { login: "alice", body: `c${i}`, at: at(i) });
  for (let i = 1; i <= 3; i++) github.comment(7, { login: "alice", body: `same${i}`, at: at(6) });
  setNow(10);
  const result = await poll({ perPage: 2 });
  assert.deepEqual(
    result.candidates.map((c) => c.body),
    ["c1", "c2", "c3", "c4", "c5", "same1", "same2", "same3"],
  );
});

test("a comment edited mid-pagination does not make another slip between pages", async () => {
  const { github, poll, setNow } = await fixture();
  github.issue(7);
  const first = github.comment(7, { login: "alice", body: "a", at: at(1) });
  for (const [i, body] of ["b", "c", "d"].entries()) github.comment(7, { login: "alice", body, at: at(i + 2) });
  setNow(10);
  let edited = false;
  github.afterGet = (path) => {
    if (!edited && path.includes("issues/comments")) {
      edited = true;
      github.edit(first, "a, edited", at(9));
    }
  };
  const result = await poll({ perPage: 2 });
  assert.deepEqual(result.candidates.map((c) => c.body).sort(), ["a, edited", "b", "c", "d"]);
});

test("reviews: drafts and empty comment wrappers are skipped, old reviews are not re-read", async () => {
  const { github, poll, setNow } = await fixture();
  github.issue(8, true);
  github.review(8, { login: "bob", body: "ancient", state: "COMMENTED", at: at(-60) });
  github.review(8, { login: "bob", body: "draft", state: "PENDING", at: at(1) });
  github.review(8, { login: "bob", body: "", state: "COMMENTED", at: at(2) });
  github.review(8, { login: "bob", body: "", state: "APPROVED", at: at(3) });
  setNow(10);
  const result = await poll({ perPage: 2 });
  assert.deepEqual(result.candidates.map((c) => [c.kind, c.body]), [["pull_request_review", ""]]);
});

test("a cursor that is not ours is refused, not reset", async () => {
  const { store, github, poll } = await fixture();
  store.advanceCursor(store.ensureSource("github", github.repo).id, "garbage");
  await assert.rejects(poll());
  store.advanceCursor(store.ensureSource("github", github.repo).id, JSON.stringify({ issues: "x" }));
  await assert.rejects(poll(), /no valid issueComments/);
});
