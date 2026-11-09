import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { FakeGitHub } from "../github/fake-github.ts";
import { Store } from "../store/store.ts";
import { loadConfig, loadRouterConfig, loadWatchdogConfig, loadWorkerConfig } from "./config.ts";
import { main, TICK, type MainDeps } from "./main.ts";

const noT3: MainDeps["t3"] = async () => {
  throw new Error("this test does not run t3");
};
const noSemon: MainDeps["semon"] = async () => {
  throw new Error("this test does not run semon");
};

function deps(
  env: Record<string, string>,
  gh: MainDeps["gh"],
  t3: MainDeps["t3"] = noT3,
  semon: MainDeps["semon"] = noSemon,
) {
  const out: string[] = [];
  const err: string[] = [];
  const d: MainDeps = {
    env, gh, t3, semon,
    stdout: (l: string) => out.push(l),
    stderr: (l: string) => err.push(l),
    sleep: async () => {},
    stop: new AbortController().signal,
  };
  return { out, err, deps: d };
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
      stalls: [],
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

test("Watchdog config needs a positive whole number of minutes", () => {
  assert.deepEqual(loadWatchdogConfig({ OSTROM_STALL_MINUTES: " 20 " }), { idleMinutes: 20 });
  for (const value of [undefined, "", "0", "1.5", "-3", "soon"]) {
    assert.throws(() => loadWatchdogConfig({ OSTROM_STALL_MINUTES: value }), /OSTROM_STALL_MINUTES/);
  }
});

const shownWorker = (fields: Record<string, unknown>) => {
  const value = JSON.parse(readFileSync(new URL("../t3/fixtures/show.json", import.meta.url), "utf8"));
  Object.assign(value.thread, fields);
  return { code: 0, stdout: JSON.stringify(value), stderr: "" };
};
const semonAnswer = (minutes: number, stalls: unknown[]) => async () => ({
  code: 0, stdout: JSON.stringify({ idle_minutes: minutes, stalls }), stderr: "",
});

test(
  "watchdog prints each check, flags a stall once, and a failed Semon read exits non-zero",
  withDb(async (dbPath) => {
    const env = { OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath, OSTROM_STALL_MINUTES: "20" };
    const store = Store.open(dbPath);
    const source = store.ensureSource("github", "o/r");
    const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://github.com/o/r/issues/7" });
    const trigger = store.insertTrigger({
      sourceId: source.id, sourceEventId: "IC_1", threadId: issue, kind: "issue_comment", actor: "alice",
      url: "https://github.com/o/r/issues/7#issuecomment-1", occurredAt: new Date(),
    }).id;
    const { ticketId } = store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "fix it" });
    store.startAssignment(store.createAssignment(ticketId!), "mcp:w");
    store.close();
    const t3 = async () => shownWorker({ threadId: "mcp:w", branch: "ostrom/ticket-1-1", status: "running", activeRunId: "run:1", updatedAt: new Date().toISOString() });
    const stalled = semonAnswer(20, [{ id: "s", branch: "ostrom/ticket-1-1", stall_reason: "waiting-question", open_question: { text: "Which?" } }]);

    const missing = deps({ ...env, OSTROM_STALL_MINUTES: "" }, async () => "[]", t3, stalled);
    assert.equal(await main(["watchdog"], missing.deps), 1);
    assert.match(missing.err.join("\n"), /OSTROM_STALL_MINUTES/);

    const first = deps(env, async () => "[]", t3, stalled);
    assert.equal(await main(["watchdog"], first.deps), 0);
    assert.deepEqual(first.out.map((line) => JSON.parse(line)), [{
      assignmentId: 1, t3ThreadId: "mcp:w", semonReason: "waiting-question", t3Reason: null,
      stallId: 1, newEpisode: true, clearedStallId: null,
    }]);
    const second = deps(env, async () => "[]", t3, stalled);
    assert.equal(await main(["watchdog"], second.deps), 0);
    assert.equal(JSON.parse(second.out[0]!).newEpisode, false);

    const broken = deps(env, async () => "[]", t3, async () => ({ code: 1, stdout: "", stderr: "no logs" }));
    assert.equal(await main(["watchdog"], broken.deps), 1);
    assert.match(broken.err.join("\n"), /semon stalls failed: semon query stalls exited 1: no logs/);
    assert.equal(broken.out.length, 1);
  }),
);

const tickEnv = (dbPath: string) => ({
  OSTROM_REPO: "onsager-ai/ostrom", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath, ...workerEnv,
  OSTROM_ROUTER_PROVIDER: "claudeAgent", OSTROM_ROUTER_MODEL: "claude-haiku-5-5", OSTROM_STALL_MINUTES: "20",
});
/** The pass each summary line on stderr came from, in order. */
const passes = (err: string[]) => err.map((line) => /^ostromd ([\w-]+):/.exec(line)?.[1]);

test("tick runs every pass once, in order", () => {
  assert.deepEqual([...TICK], ["poll", "observe", "route", "watchdog", "route-candidates", "send"]);
});

test(
  "tick runs the passes in order, reports a failed one, runs the rest and exits non-zero",
  withDb(async (dbPath) => {
    const github = new FakeGitHub(() => new Date());
    const ok = deps(tickEnv(dbPath), github.gh, noT3, semonAnswer(20, []));
    assert.equal(await main(["tick"], ok.deps), 0);
    assert.deepEqual(passes(ok.err), [...TICK, "tick"]);
    assert.match(ok.err.at(-1)!, /all 6 passes ran/);

    // Intake and Semon both fail; every other pass still runs, and the tick names the failures.
    github.failWhen = () => new Error("HTTP 500");
    const failed = deps(tickEnv(dbPath), github.gh, noT3, async () => ({ code: 127, stdout: "", stderr: "semon: not found" }));
    assert.equal(await main(["tick"], failed.deps), 1);
    assert.deepEqual(
      passes(failed.err).filter((name, i, all) => name !== all[i - 1]),
      [...TICK, "tick"],
    );
    assert.match(failed.err.at(-1)!, /failed: poll \(exit 1\), watchdog \(exit 1\)$/);
  }),
);

test("tick and run check every variable before any pass", async () => {
  for (const argv of [["tick"], ["run", "--interval", "60"]]) {
    const run = deps({ ...tickEnv("/nonexistent/ostrom.sqlite"), OSTROM_ROUTER_MODEL: "" }, async () => "[]");
    assert.equal(await main(argv, run.deps), 1);
    assert.deepEqual(run.err.length, 1);
    assert.match(run.err[0]!, /OSTROM_ROUTER_MODEL/);
  }
});

test("run needs --interval with whole seconds", async () => {
  for (const argv of [["run"], ["run", "60"], ["run", "--interval"], ["run", "--interval", "0"], ["run", "--interval", "1.5"], ["tick", "x"], ["watchdog", "1"]]) {
    const run = deps({}, async () => "[]");
    assert.equal(await main(argv, run.deps), 2, argv.join(" "));
  }
});

test(
  "run repeats tick with the interval between, until stopped, and exits with the last tick's code",
  withDb(async (dbPath) => {
    const github = new FakeGitHub(() => new Date());
    const stop = new AbortController();
    const waits: number[] = [];
    const run = deps(tickEnv(dbPath), github.gh, noT3, semonAnswer(20, []));
    run.deps.stop = stop.signal;
    run.deps.sleep = async (ms) => {
      waits.push(ms);
      // The second tick fails; the operator stops the loop during the following wait.
      if (waits.length === 1) github.failWhen = () => new Error("HTTP 500");
      if (waits.length === 2) stop.abort();
    };
    assert.equal(await main(["run", "--interval", "30"], run.deps), 1);
    assert.deepEqual(waits, [30_000, 30_000]);
    assert.equal(run.err.filter((line) => line.startsWith("ostromd tick:")).length, 2);
  }),
);
