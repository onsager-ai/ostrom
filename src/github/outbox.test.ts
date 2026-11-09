import assert from "node:assert/strict";
import { test } from "node:test";
import { Store } from "../store/store.ts";
import { FakeGitHub } from "./fake-github.ts";
import { replyMarker } from "./marker.ts";
import { defaultBackoffMs, sendDueReplies, type SendOptions } from "./outbox.ts";

const T0 = Date.parse("2026-11-09T12:00:00.000Z");

function fixture() {
  let now = new Date(T0);
  const clock = () => now;
  const store = Store.open(":memory:", { now: clock });
  const github = new FakeGitHub(clock);
  const source = store.ensureSource("github", github.repo);
  github.issue(7);
  const thread = store.ensureThread({
    sourceId: source.id,
    kind: "issue",
    number: 7,
    url: "https://github.com/onsager-ai/ostrom/issues/7",
  });
  const send = (extra: Partial<SendOptions> = {}) =>
    sendDueReplies({ store, gh: github.gh, repo: github.repo, now: clock, ...extra });
  const posts = () => github.calls.filter((args) => args[2] === "POST").length;
  const reply = (id: string) => store.dueReplies().find((r) => r.id === id);
  return { store, github, thread, send, posts, reply, advance: (ms: number) => (now = new Date(now.getTime() + ms)) };
}

test("a due reply is posted once, with its marker, and marked sent", async () => {
  const { store, github, thread, send, posts } = fixture();
  store.enqueueReply({ id: "ack-1", threadId: thread, body: "On it." });

  assert.deepEqual(await send(), { sent: ["ack-1"], found: [], failed: [] });
  const comments = github.commentsOn(7);
  assert.equal(comments.length, 1);
  assert.equal(comments[0]!["body"], `On it.\n\n${replyMarker("ack-1")}`);
  assert.equal(store.dueReplies().length, 0);

  assert.deepEqual(await send(), { sent: [], found: [], failed: [] });
  assert.equal(posts(), 1);
});

test("a failed send backs off, then the retry posts", async () => {
  const { store, github, thread, send, posts, reply, advance } = fixture();
  store.enqueueReply({ id: "ack-1", threadId: thread, body: "On it." });
  github.failWhen = (args) => (args[2] === "POST" ? new Error("HTTP 502") : undefined);

  const first = await send();
  assert.deepEqual(first.failed, [{ id: "ack-1", error: "HTTP 502" }]);
  assert.equal(store.dueReplies().length, 0, "not due again until the backoff passes");

  advance(defaultBackoffMs(0));
  const due = reply("ack-1")!;
  assert.equal(due.attempts, 1);
  assert.equal(due.lastError, "HTTP 502");
  assert.equal(due.nextAttemptAt, new Date(T0 + 30_000).toISOString());

  await send();
  advance(defaultBackoffMs(1) - 1);
  assert.equal(store.dueReplies().length, 0, "the second failure waits twice as long");
  advance(1);
  assert.equal(reply("ack-1")!.attempts, 2);

  github.failWhen = undefined;
  assert.deepEqual((await send()).sent, ["ack-1"]);
  assert.equal(github.commentsOn(7).length, 1);
  assert.equal(posts(), 3);
});

test("backoff doubles and caps at an hour", () => {
  assert.equal(defaultBackoffMs(0), 30_000);
  assert.equal(defaultBackoffMs(1), 60_000);
  assert.equal(defaultBackoffMs(20), 3_600_000);
});

test("a crash after posting does not double-post on the retry", async () => {
  const { store, github, thread, send, posts, advance } = fixture();
  store.enqueueReply({ id: "ack-1", threadId: thread, body: "On it." });
  // The comment lands on GitHub, but the process never learns it did.
  github.failWhen = (args) => {
    if (args[2] !== "POST") return undefined;
    github.failWhen = undefined;
    github.comment(7, { login: "operator", body: args[5]!.slice("body=".length), at: new Date(T0) });
    return new Error("connection reset");
  };
  assert.equal((await send()).failed.length, 1);

  advance(defaultBackoffMs(0));
  assert.deepEqual(await send(), { sent: [], found: ["ack-1"], failed: [] });
  assert.equal(github.commentsOn(7).length, 1);
  assert.equal(posts(), 1);
  assert.equal(store.dueReplies().length, 0);
});

test("a crash between posting and marking sent, on the first attempt, does not double-post", async () => {
  const { store, github, thread, send, posts } = fixture();
  store.enqueueReply({ id: "ack-1", threadId: thread, body: "On it." });
  // The earlier process posted, then died before markReplySent: the reply is due with 0 attempts.
  github.comment(7, { login: "operator", body: `On it.\n\n${replyMarker("ack-1")}`, at: new Date(T0) });

  assert.deepEqual(await send(), { sent: [], found: ["ack-1"], failed: [] });
  assert.equal(posts(), 0);
});

test("the search matches the exact id and reads every page", async () => {
  const { store, github, thread, send, posts } = fixture();
  store.enqueueReply({ id: "r-1", threadId: thread, body: "first" });
  github.comment(7, { login: "operator", body: `other\n\n${replyMarker("r-10")}`, at: new Date(T0) });
  for (let i = 0; i < 4; i++) github.comment(7, { login: "alice", body: `chatter ${i}`, at: new Date(T0) });
  github.comment(7, { login: "operator", body: `first\n\n${replyMarker("r-1")}`, at: new Date(T0) });

  assert.deepEqual(await send({ perPage: 2 }), { sent: [], found: ["r-1"], failed: [] });
  assert.equal(posts(), 0);
});

test("a reply whose id cannot go in a marker fails instead of posting unmarked", async () => {
  const { store, thread, send, posts } = fixture();
  store.enqueueReply({ id: "bad id -->", threadId: thread, body: "x" });
  const result = await send();
  assert.equal(result.failed.length, 1);
  assert.match(result.failed[0]!.error, /cannot go in a marker/);
  assert.equal(posts(), 0);
});
