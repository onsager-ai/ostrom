import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { FakeGitHub } from "../github/fake-github.ts";
import { Store } from "../store/store.ts";
import { loadConfig, loadRouterConfig, loadWorkerConfig } from "./config.ts";
import { main, type MainDeps } from "./main.ts";

const noT3: MainDeps["t3"] = async () => {
  throw new Error("this test does not run t3");
};

function deps(env: Record<string, string>, gh: MainDeps["gh"], t3: MainDeps["t3"] = noT3) {
  const out: string[] = [];
  const err: string[] = [];
  return { out, err, deps: { env, gh, t3, stdout: (l: string) => out.push(l), stderr: (l: string) => err.push(l) } };
}

function withDb(body: (dbPath: string) => Promise<void>) {
  return async () => {
    const dir = mkdtempSync(join(tmpdir(), "ostromd-"));
    try {
      await body(join(dir, "ostrom.sqlite"));
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  };
}

test("ostromd without a command prints usage and exits non-zero", () => {
  const entry = fileURLToPath(new URL("./main.ts", import.meta.url));
  const run = spawnSync(process.execPath, [entry], { encoding: "utf8" });
  assert.equal(run.status, 2);
  assert.match(run.stderr, /usage: ostromd/);
});

test("config comes from three required environment variables", () => {
  assert.deepEqual(
    loadConfig({ OSTROM_REPO: "onsager-ai/ostrom", OSTROM_ALLOWLIST: " alice, bob ,", OSTROM_DB: "/tmp/o.sqlite" }),
    { repo: "onsager-ai/ostrom", allowlist: ["alice", "bob"], dbPath: "/tmp/o.sqlite" },
  );
  assert.throws(() => loadConfig({ OSTROM_ALLOWLIST: "a", OSTROM_DB: "x" }), /OSTROM_REPO/);
  assert.throws(() => loadConfig({ OSTROM_REPO: "nope", OSTROM_ALLOWLIST: "a", OSTROM_DB: "x" }), /OSTROM_REPO/);
  assert.throws(() => loadConfig({ OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: " , ", OSTROM_DB: "x" }), /OSTROM_ALLOWLIST/);
  assert.throws(() => loadConfig({ OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: "a" }), /OSTROM_DB/);
});

test("poll with bad config exits non-zero", async () => {
  const { err, deps: d } = deps({}, async () => "[]");
  assert.equal(await main(["poll"], d), 1);
  assert.match(err.join("\n"), /OSTROM_REPO/);
});

test(
  "poll prints candidates as JSON lines, and a GitHub failure exits non-zero",
  withDb(async (dbPath) => {
    const github = new FakeGitHub(() => new Date());
    const env = { OSTROM_REPO: github.repo, OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath };
    github.issue(7);
    github.comment(7, { login: "alice", body: "hello", at: new Date() });

    const ok = deps(env, github.gh);
    assert.equal(await main(["poll"], ok.deps), 0);
    assert.equal(ok.out.length, 1);
    assert.equal(JSON.parse(ok.out[0]!).body, "hello");

    github.failWhen = () => new Error("HTTP 500");
    const failed = deps(env, github.gh);
    assert.equal(await main(["poll"], failed.deps), 1);
    assert.match(failed.err.join("\n"), /HTTP 500/);
  }),
);

test(
  "send exits non-zero when a reply failed",
  withDb(async (dbPath) => {
    const github = new FakeGitHub(() => new Date());
    const store = Store.open(dbPath);
    const source = store.ensureSource("github", github.repo);
    const thread = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "u" });
    store.enqueueReply({ id: "ack-1", threadId: thread, body: "On it." });
    store.close();
    github.issue(7);
    github.failWhen = (args) => (args[2] === "POST" ? new Error("HTTP 502") : undefined);

    const env = { OSTROM_REPO: github.repo, OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath };
    const run = deps(env, github.gh);
    assert.equal(await main(["send"], run.deps), 1);
    assert.match(run.err.join("\n"), /ack-1 failed: HTTP 502/);
  }),
);

const workerEnv = {
  OSTROM_T3_PROJECT: "/work/ostrom",
  OSTROM_WORKER_BASE: "main",
  OSTROM_WORKER_PROVIDER: "claudeAgent",
  OSTROM_WORKER_MODEL: "claude-haiku-5-5",
};

test("Worker config has four required variables and no default model", () => {
  assert.deepEqual(loadWorkerConfig(workerEnv), {
    project: "/work/ostrom",
    base: "main",
    provider: "claudeAgent",
    model: "claude-haiku-5-5",
  });
  for (const name of Object.keys(workerEnv)) {
    assert.throws(() => loadWorkerConfig({ ...workerEnv, [name]: " " }), new RegExp(name));
  }
});

test("dispatch needs one ticket id", async () => {
  for (const argv of [["dispatch"], ["dispatch", "x"], ["dispatch", "0"], ["dispatch", "1", "2"], ["observe", "1"], ["route", "1"], ["route-candidates", "1"]]) {
    const run = deps({}, async () => "[]");
    assert.equal(await main(argv, run.deps), 2, argv.join(" "));
  }
});

test(
  "dispatch and observe run through t3, and a t3 failure exits non-zero",
  withDb(async (dbPath) => {
    const env = { OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath };
    const store = Store.open(dbPath);
    const source = store.ensureSource("github", "o/r");
    const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://github.com/o/r/issues/7" });
    const trigger = store.insertTrigger({
      sourceId: source.id, sourceEventId: "IC_1", threadId: issue, kind: "issue_comment", actor: "alice",
      url: "https://github.com/o/r/issues/7#issuecomment-1", occurredAt: new Date(),
    }).id;
    store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "fix it" });
    store.close();

    const missing = deps(env, async () => "[]");
    assert.equal(await main(["dispatch", "1"], missing.deps), 1);
    assert.match(missing.err.join("\n"), /OSTROM_T3_PROJECT/);

    const launched = { threadId: "mcp:w", projectId: "mcp:p", runId: "run:1" };
    const started = deps({ ...env, ...workerEnv }, async () => "[]", async () => ({
      code: 0, stdout: JSON.stringify(launched), stderr: "",
    }));
    assert.equal(await main(["dispatch", "1"], started.deps), 0);
    assert.deepEqual(JSON.parse(started.out[0]!), {
      ticketId: 1, assignmentId: 1, t3ThreadId: "mcp:w", branch: "ostrom/ticket-1-1",
    });

    const unreadable = deps(env, async () => "[]", async () => ({ code: 1, stdout: "", stderr: "server down" }));
    assert.equal(await main(["observe"], unreadable.deps), 1);
    assert.match(unreadable.err.join("\n"), /assignment 1 failed: t3 thread show exited 1: server down/);
  }),
);

test(
  "route prints deliveries as JSON lines, and a failed send exits non-zero until it is retried",
  withDb(async (dbPath) => {
    const env = { OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath };
    const store = Store.open(dbPath);
    const source = store.ensureSource("github", "o/r");
    const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://github.com/o/r/issues/7" });
    const comment = (id: string) =>
      store.insertTrigger({
        sourceId: source.id, sourceEventId: id, threadId: issue, kind: "issue_comment", actor: "alice",
        url: `https://github.com/o/r/issues/7#${id}`, occurredAt: new Date(),
      }).id;
    const { ticketId } = store.recordDecision({ threadId: issue, triggerIds: [comment("IC_1")], verdict: "accept", reason: "fix it" });
    const assignmentId = store.createAssignment(ticketId!);
    store.startAssignment(assignmentId, "mcp:w");
    store.subscribe(ticketId!, issue);
    const followUp = comment("IC_2");
    store.close();

    const down = deps(env, async () => "[]", async () => ({ code: 1, stdout: "", stderr: "server down" }));
    assert.equal(await main(["route"], down.deps), 1);
    assert.match(down.err.join("\n"), /trigger 2 to ticket 1 failed: t3 thread send exited 1: server down/);

    const sent = { threadId: "mcp:w", messageId: "message:m", runId: "run:2", status: "queued", delivery: "queued" };
    const up = deps(env, async () => "[]", async () => ({ code: 0, stdout: JSON.stringify(sent), stderr: "" }));
    assert.equal(await main(["route"], up.deps), 0);
    assert.deepEqual(up.out.map((line) => JSON.parse(line)), [
      { triggerId: followUp, ticketId: 1, assignmentId, t3ThreadId: "mcp:w", t3RunId: "run:2", delivery: "queued" },
    ]);

    const replay = deps(env, async () => "[]");
    assert.equal(await main(["route"], replay.deps), 0);
    assert.deepEqual(replay.out, []);
  }),
);

test("Router config needs a provider and model, with no default", () => {
  const env = { OSTROM_T3_PROJECT: "/work/ostrom", OSTROM_ROUTER_PROVIDER: "claudeAgent", OSTROM_ROUTER_MODEL: "claude-haiku-5-5" };
  assert.deepEqual(loadRouterConfig(env), { project: "/work/ostrom", provider: "claudeAgent", model: "claude-haiku-5-5" });
  for (const name of Object.keys(env)) {
    assert.throws(() => loadRouterConfig({ ...env, [name]: "" }), new RegExp(name));
  }
});

test(
  "route-candidates prints the pass as JSON lines, and invalid Router output exits non-zero",
  withDb(async (dbPath) => {
    const env = {
      OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath, ...workerEnv,
      OSTROM_ROUTER_PROVIDER: "claudeAgent", OSTROM_ROUTER_MODEL: "claude-haiku-5-5",
    };
    const store = Store.open(dbPath);
    const source = store.ensureSource("github", "o/r");
    const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://github.com/o/r/issues/7" });
    const trigger = store.insertTrigger({
      sourceId: source.id, sourceEventId: "IC_1", threadId: issue, kind: "issue_comment", actor: "alice",
      url: "https://github.com/o/r/issues/7#issuecomment-1", occurredAt: new Date(),
    }).id;
    store.close();
    const gh: MainDeps["gh"] = async () => JSON.stringify({ body: "please fix the typo" });

    const missing = deps({ ...env, OSTROM_ROUTER_MODEL: "" }, gh);
    assert.equal(await main(["route-candidates"], missing.deps), 1);
    assert.match(missing.err.join("\n"), /OSTROM_ROUTER_MODEL/);

    const run = (reply: string) => ({
      code: 0, stdout: JSON.stringify({ threadId: "mcp:router", runId: "run:1", status: "completed", reply, error: null }), stderr: "",
    });
    const garbled = deps(env, gh, async () => run("I accept."));
    assert.equal(await main(["route-candidates"], garbled.deps), 1);
    assert.match(garbled.err.join("\n"), /Router reply is not JSON/);

    const answer = {
      decisions: [{ thread: issue, triggers: [trigger], verdict: "ignore", reason: "nothing asked", reply: null }],
      outcomes: [],
    };
    const shown = JSON.parse(readFileSync(new URL("../t3/fixtures/show.json", import.meta.url), "utf8"));
    const judged = deps(env, gh, async ({ args }) =>
      args[1] === "show" ? { code: 0, stdout: JSON.stringify(shown), stderr: "" } : run(JSON.stringify(answer)),
    );
    assert.equal(await main(["route-candidates"], judged.deps), 0);
    assert.deepEqual(judged.out.map((line) => JSON.parse(line).type), ["router", "decision"]);
    assert.equal(JSON.parse(judged.out[1]!).verdict, "ignore");
  }),
);
