import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { FakeGitHub } from "../github/fake-github.ts";
import { Store } from "../store/store.ts";
import { FakeRunner } from "../worker/fake-runner.ts";
import { loadConfig, loadRouterConfig, loadWatchdogConfig, loadWorkerConfig } from "./config.ts";
import { main, TICK, type MainDeps } from "./main.ts";

const noSemon: MainDeps["semon"] = async () => {
  throw new Error("this test does not run semon");
};

function deps(
  env: Record<string, string>,
  gh: MainDeps["gh"],
  semon: MainDeps["semon"] = noSemon,
) {
  const out: string[] = [];
  const err: string[] = [];
  const runner = new FakeRunner();
  const d: MainDeps = {
    env, gh, semon,
    stdout: (l: string) => out.push(l),
    stderr: (l: string) => err.push(l),
    sleep: async () => {},
    stop: new AbortController().signal,
    runners: { claude: runner, codex: runner },
    addWorktree: async (_repo, _path, _base, ticket, assignment) => `ostrom/ticket-${ticket}-${assignment}`,
  };
  return { out, err, deps: d, runner };
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
  assert.equal(await main(["dev", "poll"], d), 1);
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
    assert.equal(await main(["dev", "poll"], ok.deps), 0);
    assert.equal(ok.out.length, 1);
    assert.equal(JSON.parse(ok.out[0]!).body, "hello");

    github.failWhen = () => new Error("HTTP 500");
    const failed = deps(env, github.gh);
    assert.equal(await main(["dev", "poll"], failed.deps), 1);
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
    store.enqueueReply({ id: "ack-1", threadId: thread, body: "On it.", blocking: true });
    store.close();
    github.issue(7);
    github.failWhen = (args) => (args[2] === "POST" ? new Error("HTTP 502") : undefined);

    const env = { OSTROM_REPO: github.repo, OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath };
    const run = deps(env, github.gh);
    assert.equal(await main(["dev", "send"], run.deps), 1);
    assert.match(run.err.join("\n"), /ack-1 failed: HTTP 502/);
  }),
);

const workerEnv = {
  OSTROM_WORKER_REPO: "/work/ostrom",
  OSTROM_WORKER_BASE: "main",
  OSTROM_WORKER_HARNESS: "claude",
  OSTROM_WORKER_MODEL: "claude-haiku-5-5",
};
const routerEnv = { OSTROM_ROUTER_HARNESS: "codex", OSTROM_ROUTER_MODEL: "gpt-6-luna" };

test("Worker config requires a repo and base, and defaults to codex on gpt-6-luna", () => {
  const required = { OSTROM_WORKER_REPO: "/work/ostrom", OSTROM_WORKER_BASE: "main" };
  const { machine, ...config } = loadWorkerConfig(required);
  assert.equal(machine.length > 0, true);
  assert.deepEqual(config, {
    repo: "/work/ostrom", base: "main", worktrees: "/work/ostrom.worktrees", harness: "codex", model: "gpt-6-luna",
  });
  assert.equal(loadWorkerConfig(workerEnv).harness, "claude");
  assert.equal(loadWorkerConfig({ ...workerEnv, OSTROM_WORKER_WORKTREES: "/wt" }).worktrees, "/wt");
  for (const name of Object.keys(required)) {
    assert.throws(() => loadWorkerConfig({ ...required, [name]: " " }), new RegExp(name));
  }
  assert.throws(() => loadWorkerConfig({ ...required, OSTROM_WORKER_HARNESS: "gemini" }), /OSTROM_WORKER_HARNESS/);
});

test("Worker house rules come from a file, and an empty one is refused", () => {
  const required = { OSTROM_WORKER_REPO: "/work/ostrom", OSTROM_WORKER_BASE: "main" };
  const dir = mkdtempSync(join(tmpdir(), "ostrom-rules-"));
  try {
    writeFileSync(join(dir, "rules.md"), "Run tests in CI only.\n");
    writeFileSync(join(dir, "empty.md"), "  \n");
    assert.equal(loadWorkerConfig({ ...required, OSTROM_WORKER_RULES_FILE: join(dir, "rules.md") }).rules, "Run tests in CI only.");
    assert.throws(() => loadWorkerConfig({ ...required, OSTROM_WORKER_RULES_FILE: join(dir, "empty.md") }), /is empty/);
    assert.equal(loadWorkerConfig(required).rules, undefined);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("dispatch needs one ticket id", async () => {
  for (const argv of [["dispatch"], ["dispatch", "x"], ["dispatch", "0"], ["dispatch", "1", "2"], ["observe", "1"], ["route", "1"], ["route-candidates", "1"]]) {
    const run = deps({}, async () => "[]");
    assert.equal(await main(["dev", ...argv], run.deps), 2, argv.join(" "));
  }
});

test(
  "dispatch starts a session and observe reads GitHub, and a gh failure exits non-zero",
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
    assert.equal(await main(["dev", "dispatch", "1"], missing.deps), 1);
    assert.match(missing.err.join("\n"), /OSTROM_WORKER_REPO/);

    const started = deps({ ...env, ...workerEnv }, async () => "[]");
    assert.equal(await main(["dev", "dispatch", "1"], started.deps), 0);
    assert.deepEqual(JSON.parse(started.out[0]!), {
      ticketId: 1, assignmentId: 1, sessionId: "sess-1", branch: "ostrom/ticket-1-1",
    });
    assert.equal(started.runner.sessions[0]?.prompts.length, 1, "the command waited for the turn");

    const unreadable = deps(env, async () => {
      throw new Error("gh down");
    });
    assert.equal(await main(["dev", "observe"], unreadable.deps), 1);
    assert.match(unreadable.err.join("\n"), /assignment 1 failed: gh down/);
  }),
);

test(
  "route prints queued follow-ups as JSON lines, exits non-zero while a ticket has no Worker, and replays nothing",
  withDb(async (dbPath) => {
    const env = { OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath, ...workerEnv };
    const store = Store.open(dbPath);
    const source = store.ensureSource("github", "o/r");
    const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://github.com/o/r/issues/7" });
    const comment = (id: string) =>
      store.insertTrigger({
        sourceId: source.id, sourceEventId: id, threadId: issue, kind: "issue_comment", actor: "alice",
        url: `https://github.com/o/r/issues/7#${id}`, occurredAt: new Date(),
      }).id;
    const { ticketId } = store.recordDecision({ threadId: issue, triggerIds: [comment("IC_1")], verdict: "accept", reason: "fix it" });
    store.subscribe(ticketId!, issue);
    const followUp = comment("IC_2");
    store.close();

    const none = deps(env, async () => "[]");
    assert.equal(await main(["dev", "route"], none.deps), 1);
    assert.match(none.err.join("\n"), /trigger 2 to ticket 1 failed: ticket 1 has no running Worker/);

    const reopened = Store.open(dbPath);
    const assignmentId = reopened.createAssignment(ticketId!);
    reopened.configureNativeAssignment(assignmentId, { harness: "claude", model: "m", worktree: "/w", branch: "b", machine: "h" });
    reopened.startNativeAssignment(assignmentId, "sess-9");
    reopened.close();

    const up = deps(env, async () => "[]");
    assert.equal(await main(["dev", "route"], up.deps), 0);
    assert.deepEqual(up.out.map((line) => JSON.parse(line)), [{ triggerId: followUp, ticketId: 1, assignmentId }]);
    assert.equal(up.runner.sessions[0]?.id, "sess-9");
    assert.equal(up.runner.sessions[0]?.prompts.length, 1, "the command waited for the turn");

    const replay = deps(env, async () => "[]");
    assert.equal(await main(["dev", "route"], replay.deps), 0);
    assert.deepEqual(replay.out, []);
  }),
);

test("Router config defaults to the small Codex tier and validates its harness", () => {
  assert.deepEqual(loadRouterConfig({}), { harness: "codex", model: "gpt-6-luna" });
  assert.deepEqual(
    loadRouterConfig({ OSTROM_ROUTER_HARNESS: "claude", OSTROM_ROUTER_MODEL: "claude-haiku-5-5" }),
    { harness: "claude", model: "claude-haiku-5-5" },
  );
  assert.throws(() => loadRouterConfig({ OSTROM_ROUTER_HARNESS: "t3" }), /OSTROM_ROUTER_HARNESS/);
});

test(
  "route-candidates prints the pass as JSON lines, and invalid Router output exits non-zero",
  withDb(async (dbPath) => {
    const env = {
      OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath, ...workerEnv, ...routerEnv,
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

    const badHarness = deps({ ...env, OSTROM_ROUTER_HARNESS: "t3" }, gh);
    assert.equal(await main(["dev", "route-candidates"], badHarness.deps), 1);
    assert.match(badHarness.err.join("\n"), /OSTROM_ROUTER_HARNESS/);

    const garbled = deps(env, gh);
    garbled.runner.start = ((options) => {
      const session = new FakeRunner().start(options);
      session.reply = () => "I accept.";
      return session;
    }) as typeof garbled.runner.start;
    assert.equal(await main(["dev", "route-candidates"], garbled.deps), 1);
    assert.match(garbled.err.join("\n"), /Router reply is not JSON/);

    const answer = {
      decisions: [{ thread: issue, triggers: [trigger], verdict: "ignore", reason: "nothing asked", reply: null }],
      outcomes: [],
      stalls: [],
    };
    const judged = deps(env, gh);
    judged.runner.start = ((options) => {
      const session = new FakeRunner().start(options);
      session.reply = () => JSON.stringify(answer);
      return session;
    }) as typeof judged.runner.start;
    assert.equal(await main(["dev", "route-candidates"], judged.deps), 0);
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

const semonAnswer = (minutes: number, stalls: unknown[]) => async () => ({
  code: 0, stdout: JSON.stringify({ idle_minutes: minutes, stalls }), stderr: "",
});

test(
  "watchdog prints each check, flags a stall once, and a failed Semon read exits non-zero",
  withDb(async (dbPath) => {
    const env = { OSTROM_REPO: "o/r", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath, OSTROM_STALL_MINUTES: "20", ...workerEnv };
    const store = Store.open(dbPath);
    const source = store.ensureSource("github", "o/r");
    const issue = store.ensureThread({ sourceId: source.id, kind: "issue", number: 7, url: "https://github.com/o/r/issues/7" });
    const trigger = store.insertTrigger({
      sourceId: source.id, sourceEventId: "IC_1", threadId: issue, kind: "issue_comment", actor: "alice",
      url: "https://github.com/o/r/issues/7#issuecomment-1", occurredAt: new Date(),
    }).id;
    const { ticketId } = store.recordDecision({ threadId: issue, triggerIds: [trigger], verdict: "accept", reason: "fix it" });
    const assignmentId = store.createAssignment(ticketId!);
    store.configureNativeAssignment(assignmentId, { harness: "claude", model: "claude-haiku-5-5", worktree: "/w", branch: "ostrom/ticket-1-1", machine: "host" });
    store.startNativeAssignment(assignmentId, "session-1");
    store.close();
    const stalled = semonAnswer(20, [{ id: "s", branch: "ostrom/ticket-1-1", stall_reason: "waiting-question", open_question: { text: "Which?" } }]);

    const missing = deps({ ...env, OSTROM_STALL_MINUTES: "" }, async () => "[]", stalled);
    assert.equal(await main(["dev", "watchdog"], missing.deps), 1);
    assert.match(missing.err.join("\n"), /OSTROM_STALL_MINUTES/);

    const first = deps(env, async () => "[]", stalled);
    assert.equal(await main(["dev", "watchdog"], first.deps), 0);
    assert.deepEqual(first.out.map((line) => JSON.parse(line)), [{
      assignmentId: 1, semonReason: "waiting-question", stallId: 1, newEpisode: true, clearedStallId: null,
    }]);
    const second = deps(env, async () => "[]", stalled);
    assert.equal(await main(["dev", "watchdog"], second.deps), 0);
    assert.equal(JSON.parse(second.out[0]!).newEpisode, false);

    const broken = deps(env, async () => "[]", async () => ({ code: 1, stdout: "", stderr: "no logs" }));
    assert.equal(await main(["dev", "watchdog"], broken.deps), 1);
    assert.match(broken.err.join("\n"), /semon stalls failed: semon query stalls exited 1: no logs/);
    assert.equal(broken.out.length, 1);
  }),
);

const tickEnv = (dbPath: string) => ({
  OSTROM_REPO: "onsager-ai/ostrom", OSTROM_ALLOWLIST: "alice", OSTROM_DB: dbPath, ...workerEnv, ...routerEnv,
  OSTROM_STALL_MINUTES: "20",
});
/** The pass each summary line on stderr came from, in order. */
const passes = (err: string[]) => err.map((line) => /^ostromd ([\w-]+):/.exec(line)?.[1]);

test("tick runs every pass once, in order", () => {
  assert.deepEqual([...TICK], ["poll", "observe", "commands", "route", "watchdog", "route-candidates", "send"]);
});

test(
  "tick runs the passes in order, reports a failed one, runs the rest and exits non-zero",
  withDb(async (dbPath) => {
    const github = new FakeGitHub(() => new Date());
    const ok = deps(tickEnv(dbPath), github.gh, semonAnswer(20, []));
    assert.equal(await main(["dev", "tick"], ok.deps), 0);
    assert.deepEqual(passes(ok.err), [...TICK, "tick"]);
    assert.match(ok.err.at(-1)!, /all 7 passes ran/);

    // Intake and Semon both fail; every other pass still runs, and the tick names the failures.
    github.failWhen = () => new Error("HTTP 500");
    const failed = deps(tickEnv(dbPath), github.gh, async () => ({ code: 127, stdout: "", stderr: "semon: not found" }));
    assert.equal(await main(["dev", "tick"], failed.deps), 1);
    assert.deepEqual(
      passes(failed.err).filter((name, i, all) => name !== all[i - 1]),
      [...TICK, "tick"],
    );
    assert.match(failed.err.at(-1)!, /failed: poll \(exit 1\), watchdog \(exit 1\)$/);
  }),
);

test(
  "a pass given the run's store leaves it open for Worker turns that finish later",
  withDb(async (dbPath) => {
    const github = new FakeGitHub(() => new Date());
    const store = Store.open(dbPath);
    try {
      const pass = deps(tickEnv(dbPath), github.gh, semonAnswer(20, []));
      pass.deps.store = store;
      for (const step of TICK) await main(["dev", step], pass.deps);
      assert.deepEqual(store.pendingDeliveries(), []);
    } finally {
      store.close();
    }
  }),
);

test("tick and run check every variable before any pass", async () => {
  for (const argv of [["dev", "tick"], ["run", "--interval", "60"]]) {
    const run = deps({ ...tickEnv("/nonexistent/ostrom.sqlite"), OSTROM_ROUTER_HARNESS: "t3" }, async () => "[]");
    assert.equal(await main(argv, run.deps), 1);
    assert.deepEqual(run.err.length, 1);
    assert.match(run.err[0]!, /OSTROM_ROUTER_HARNESS/);
  }
});

test("run needs --interval with whole seconds", async () => {
  for (const argv of [["run"], ["run", "60"], ["run", "--interval"], ["run", "--interval", "0"], ["run", "--interval", "1.5"], ["tick", "x"], ["watchdog", "1"]]) {
    const run = deps({}, async () => "[]");
    assert.equal(await main(["dev", ...argv], run.deps), 2, argv.join(" "));
  }
});

test(
  "run repeats tick with the interval between, until stopped, and exits with the last tick's code",
  withDb(async (dbPath) => {
    const github = new FakeGitHub(() => new Date());
    const stop = new AbortController();
    const waits: number[] = [];
    const run = deps(tickEnv(dbPath), github.gh, semonAnswer(20, []));
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
