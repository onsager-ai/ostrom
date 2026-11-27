import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { PlatformApiClient, startPlatformApiServer } from "./api.ts";
import { runPlatformCli } from "./cli.ts";
import { PlatformStore } from "./store.ts";
import type { Decision, DecisionVerification, FrictionEntry, Goal, Lane, PolicyRule } from "./types.ts";

async function cli(argv: string[]) {
  let stdout = "";
  let stderr = "";
  const code = await runPlatformCli(argv, {
    stdout: { write: (text) => { stdout += text; } },
    stderr: { write: (text) => { stderr += text; } },
  });
  return { code, stdout, stderr };
}

for (const transport of ["db", "socket"]) {
  test(`CLI uses --${transport} for the complete decision and registry flow`, async (t) => {
    const directory = mkdtempSync(join(tmpdir(), "ostrom-cli-"));
    const dbPath = join(directory, "platform.sqlite");
    const socketPath = join(directory, "api.sock");
    const store = PlatformStore.open(dbPath);
    const server = transport === "socket" ? await startPlatformApiServer({ store, socketPath }) : null;
    t.after(async () => {
      try {
        await server?.close();
      } finally {
        store.close();
        rmSync(directory, { recursive: true, force: true });
      }
    });
    const global = [`--${transport}`, transport === "db" ? dbPath : socketPath];
    const run = (...args: string[]) => cli([...global, ...args, "--json"]);
    const call = async <T>(...args: string[]): Promise<T> => {
      const result = await run(...args);
      assert.equal(result.code, 0, result.stderr);
      assert.equal(result.stderr, "");
      return JSON.parse(result.stdout) as T;
    };
    const verify = async (id: string, status: DecisionVerification["status"], code: number) => {
      const result = await run("decision", "verify", id);
      assert.equal(result.code, code, result.stderr);
      assert.equal(result.stderr, "");
      const output = JSON.parse(result.stdout) as DecisionVerification;
      assert.equal(output.valid, code === 0);
      assert.equal(output.status, status);
    };

    const lane = await call<Lane>("lane", "create", "--slug", "platform", "--name", "Platform", "--priority", "2");
    assert.equal(lane.priorityRank, 2);
    assert.deepEqual(await call<Lane[]>("lane", "list"), [lane]);
    const create = (title: string, category: Decision["category"] = "weekly_plan") => call<Decision>("decision", "create",
      "--title", title, "--authority", "owner", "--urgency", "blocking_today", "--category", category,
      "--recommendation", "Proceed", "--requested-by", "manager", "--lane", "platform",
      "--evidence", '{"count":2}', "--options", '["Proceed", "Wait"]');
    const pending = await create("Approve the plan");
    assert.equal(pending.laneId, lane.id);
    assert.deepEqual(pending.evidence, { count: 2 });
    assert.deepEqual(pending.options, ["Proceed", "Wait"]);
    assert.deepEqual(await call<Decision>("decision", "show", pending.id), pending);
    assert.deepEqual(await call<Decision[]>("decision", "list", "--status", "pending", "--authority", "owner",
      "--urgency", "blocking_today", "--lane", "platform"), [pending]);
    await verify(pending.id, "pending", 1);
    await verify("missing", "not_found", 1);

    const setGoal = ["goal", "set", "platform", "--statement", "Ship two releases.", "--metric", "Releases",
      "--target", "2", "--current", "0", "--unit", "releases", "--week", "2026-11-23", "--plan", '["Test","Ship"]',
      "--approved-decision", pending.id];
    const denied = await run(...setGoal);
    assert.equal(denied.code, 1);
    assert.match(denied.stderr, /must be approved/);

    const approved = await call<Decision>("decision", "answer", pending.id, "--status", "approved", "--by", "owner",
      "--answer", '{"allowed":true}', "--rationale", "Reviewed");
    assert.deepEqual(approved.answer, { allowed: true });
    await verify(pending.id, "approved", 0);
    const goal = await call<Goal>(...setGoal);
    assert.equal(goal.metricTarget, 2);
    assert.equal(goal.metricCurrent, 0);
    assert.equal(goal.metricUnit, "releases");
    assert.equal(goal.approvedDecisionId, pending.id);
    assert.deepEqual(goal.weeklyPlan, ["Test", "Ship"]);
    assert.deepEqual(await call("lane", "show", "platform"), { ...lane, goal });

    const replacement = await create("Replace the plan");
    const superseded = await call<Decision>("decision", "supersede", pending.id, "--by-decision", replacement.id,
      "--by", "manager", "--rationale", "Changed scope");
    assert.deepEqual(superseded, { ...approved, status: "superseded", supersededById: replacement.id });
    await verify(pending.id, "superseded", 1);
    const rejected = await call<Decision>("decision", "answer", replacement.id, "--status", "rejected", "--by", "owner",
      "--answer", "Please revise");
    assert.equal(rejected.answer, "Please revise");
    await verify(replacement.id, "rejected", 1);
    const expired = await create("Expired approval");
    store.expireDecision(expired.id, { actor: "manager" });
    await verify(expired.id, "expired", 1);
    assert.equal((await run("decision", "answer", replacement.id, "--status", "approved", "--by", "owner", "--answer", "OK")).code, 1);

    const ownerAction = await create("Approve a new campaign", "owner_action");
    assert.equal(ownerAction.category, "owner_action");
    await verify(ownerAction.id, "pending", 1);
    await call<Decision>("decision", "answer", ownerAction.id, "--status", "approved", "--by", "owner", "--answer", "Proceed");
    await verify(ownerAction.id, "approved", 0);

    assert.equal((await call<PolicyRule[]>("policy", "list")).length, 6);
    const budget = await call("budget", "set", "--scope", "lane", "--scope-id", lane.id,
      "--period", "weekly", "--cap-usd", "10", "--warn-pct", "85");
    assert.deepEqual(await call("budget", "list"), [budget]);
    const agentId = JSON.stringify(["t3", "east", "env-1", "thread-1"]);
    store.upsertAgent({ id: agentId, role: "worker", connectorKind: "t3", hostId: "east", externalRef: "thread-1",
      label: "Worker", status: "active" });
    await call("budget", "set", "--scope", "agent", "--scope-id", agentId,
      "--period", "weekly", "--cap-usd", "10", "--warn-pct", "85");
    const spend = await call<{ status: string; billedUsd: number | null }>("spend", "summary",
      "--scope", "lane", "--scope-id", lane.id);
    assert.equal(spend.status, "unavailable");
    assert.equal(spend.billedUsd, null);
    assert.notEqual((await run("spend", "summary", "--scope-id", "../unsafe")).code, 0);
    assert.notEqual((await run("spend", "summary", "--period", "yearly")).code, 0);
    assert.notEqual((await run("budget", "set", "--scope", "team", "--period", "weekly",
      "--cap-usd", "10", "--warn-pct", "101")).code, 0);
    const friction = await call<FrictionEntry>("friction", "add", "--kind", "owner_intervention", "--summary", "Clarified scope",
      "--lane", "platform", "--rule", "authority_routing");
    assert.equal(friction.laneId, lane.id);
    assert.deepEqual(await call("friction", "list", "--status", "open"), [friction]);
    assert.equal((await run("lane", "list", "--status", "open")).code, 2);
    assert.equal((await run("goal", "set", "platform", "--statement", "Goal", "--metric", "Count", "--target", "NaN")).code, 2);
  });
}

test("the CLI entry point serves the requested database and shuts down on SIGTERM", async (t) => {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-serve-"));
  const dbPath = join(directory, "platform.sqlite");
  const socketPath = join(directory, "api.sock");
  const store = PlatformStore.open(dbPath);
  store.upsertT3HostState({ hostId: "east", label: "East", mcpUrl: "https://t3.example/mcp",
    originNamespace: null, connectionState: "connected" });
  store.recordT3PrWake({ hostId: "east", threadId: "thread", prHost: "github.com", repository: "org/repo", number: 1,
    outcome: "watch", summary: "Watching", watchEnded: false, occurredAt: "2026-11-27T13:00:00Z" });
  store.close();
  const script = fileURLToPath(new URL("./cli.ts", import.meta.url));
  const child = spawn(process.execPath, [script, "serve", "--db", dbPath, "--socket", socketPath, "--json"],
    { stdio: ["ignore", "pipe", "pipe"] });
  const exited = once(child, "exit");
  t.after(async () => {
    try {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      await exited;
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk: string) => { stderr += chunk; });
  child.stdout.setEncoding("utf8");
  const ready: unknown = await Promise.race([
    once(child.stdout, "data").then(([chunk]) => chunk),
    exited.then(() => { throw new Error(`serve exited before listening: ${stderr}`); }),
  ]);
  assert.deepEqual(JSON.parse(String(ready)), { socketPath });
  const client = new PlatformApiClient(socketPath);
  const lane = await client.request<Lane>("POST", "/v1/lanes", { slug: "platform", name: "Platform" });
  assert.deepEqual(await client.request("GET", "/v1/github/prs"), []);
  assert.equal((await client.request<{ status: string }>("GET", "/v1/spend/summary")).status, "unavailable");
  child.kill("SIGTERM");
  assert.deepEqual(await exited, [0, null]);
  assert.equal(existsSync(socketPath), false);
  const persisted = await cli(["--db", dbPath, "lane", "list", "--json"]);
  assert.equal(persisted.code, 0, persisted.stderr);
  assert.deepEqual(JSON.parse(persisted.stdout), [lane]);
});
