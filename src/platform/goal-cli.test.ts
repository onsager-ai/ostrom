import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import type { TestContext } from "node:test";

import { defaultPlatformSocketPath, startPlatformApiServer } from "./api.ts";
import { runGoalWrapper } from "./goal-cli.ts";
import { PlatformStore } from "./store.ts";

function configFile(t: TestContext, config: object = {}): string {
  const directory = mkdtempSync(join(tmpdir(), "ostrom-goal-"));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const path = join(directory, "goal-remote.json");
  writeFileSync(path, JSON.stringify(config));
  return path;
}

async function wrapper(argv: string[], io: NonNullable<Parameters<typeof runGoalWrapper>[1]>) {
  let stdout = "";
  let stderr = "";
  const code = await runGoalWrapper(argv, { ...io,
    stdout: { write: (text) => { stdout += text; } },
    stderr: { write: (text) => { stderr += text; } },
  });
  return { code, stdout, stderr };
}

test("ostrom-goal uses T3CODE_THREAD_ID and calls the local Unix socket", async (t) => {
  const path = configFile(t);
  const env = { T3CODE_THREAD_ID: " thread-1 ", OSTROM_GOAL_CONFIG: path, XDG_RUNTIME_DIR: dirname(path) };
  const store = PlatformStore.open(":memory:");
  t.after(() => store.close());
  const owner = store.upsertAgent({ id: "manager", role: "manager", connectorKind: "t3", hostId: "east",
    externalRef: "environment/thread-1", label: "Manager", status: "active" });
  const lane = store.createLane({ slug: "platform", name: "Platform", ownerAgentId: owner.id });
  const server = await startPlatformApiServer({ store, socketPath: defaultPlatformSocketPath(env) });
  t.after(() => server.close());
  const ssh = () => assert.fail("the local wrapper should use the Unix socket");
  const set = await wrapper(["set", lane.slug, "--statement", "Ship a release", "--metric", "Releases", "--target", "1"], { env, ssh });
  assert.equal(set.code, 0, set.stderr);
  assert.deepEqual(JSON.parse(set.stdout), { goal: store.getLatestGoal(lane.id), audit: store.listGoalAuditEvents()[0] });
  const complete = await wrapper(["complete", lane.slug, "--thread", "thread-1", "--current", "1"], { env, ssh });
  assert.equal(complete.code, 0, complete.stderr);
  assert.deepEqual(store.listGoalAuditEvents().map(({ callerThread, action }) => [callerThread, action]),
    [["thread-1", "complete"], ["thread-1", "set"]]);
});

test("ostrom-goal refuses mismatched threads, missing threads, and other commands before transport", async () => {
  const ssh = () => assert.fail("invalid arguments must stop before transport");
  const env = { T3CODE_THREAD_ID: "thread-1", OSTROM_GOAL_SSH_TARGET: "test-target" };
  for (const [argv, diagnostic] of [
    [["complete", "platform", "--thread", "thread-2"], /must match T3CODE_THREAD_ID/],
    [["lane", "list"], /expected set or complete/],
    [["decision", "list"], /expected set or complete/],
    [["complete", "platform", "--db", "file"], /unknown option --db/],
    [["set", "platform", "--statement", "Ship", "--metric", "Count", "--target", "NaN"], /requires a number/],
  ] as const) {
    const result = await wrapper([...argv], { env, ssh });
    assert.equal(result.code, 2);
    assert.match(result.stderr, diagnostic);
    assert.match(result.stderr, /usage: ostrom-goal/);
  }
  const missing = await wrapper(["complete", "platform"], { env: {}, ssh });
  assert.equal(missing.code, 2);
  assert.match(missing.stderr, /--thread is required/);
});

test("ostrom-goal quotes only the configured remote command and sends goal fields over SSH stdin", async (t) => {
  const path = configFile(t, { sshTarget: "config-target", remoteNode: "/test path/node", remoteCli: "/test/it's/cli.ts" });
  const statement = "Ship $release; check `version`\nowner's plan";
  const argv = ["set", "platform", "--thread", "thread-1", "--statement", statement, "--metric", "Releases",
    "--target", "2", "--current", "0", "--unit", "releases", "--week", "2026-11-23", "--plan", '["Test","Ship"]',
    "--approved-decision", "approved-plan"];
  for (const target of ["config-target", "env-target"]) {
    const env = { OSTROM_GOAL_CONFIG: path, ...(target === "env-target" ? { OSTROM_GOAL_SSH_TARGET: target } : {}) };
    const ssh = (args: string[], input: string, childEnv: NodeJS.ProcessEnv) => {
      assert.deepEqual(args, ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=accept-new", "--", target,
        "'/test path/node' '--' '/test/it'\\''s/cli.ts' '--json' 'goal-self' '--stdin-json'"]);
      assert.deepEqual(childEnv, env);
      assert.deepEqual(JSON.parse(input), { action: "set", slug: "platform", callerThread: "thread-1", statement, metricName: "Releases",
        metricTarget: 2, metricCurrent: 0, metricUnit: "releases", weekStart: "2026-11-23", weeklyPlan: ["Test", "Ship"],
        approvedDecisionId: "approved-plan" });
      return { pid: 0, signal: null, output: [], status: 1, stdout: "", stderr: '{"error":"refused"}\n' };
    };
    const result = await wrapper(argv, { env, ssh });
    assert.deepEqual(result, { code: 1, stdout: "", stderr: '{"error":"refused"}\n' });
  }
});
