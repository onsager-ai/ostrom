import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { T3OutputError } from "./cli-json.ts";
import { processRunner, T3Cli, T3CommandError, type T3Exit, type T3Invocation } from "./runner.ts";

const fixture = (name: string) => readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8");

function recording(exit: T3Exit) {
  const calls: T3Invocation[] = [];
  return { calls, cli: new T3Cli(async (invocation) => (calls.push(invocation), exit)) };
}

const start = {
  project: "/work/ostrom",
  title: "Ostrom ticket 1: o/r#7",
  brief: "--not-a-flag; it goes on stdin",
  worktreeBase: "main",
  branch: "ostrom/ticket-1-1",
  provider: "claudeAgent",
  model: "claude-haiku-5-5",
};

test("start passes flags as argv, the brief on stdin, and reads the launch result", async () => {
  const { calls, cli } = recording({ code: 0, stdout: fixture("launch.json"), stderr: "" });
  const launched = await cli.startThread(start);
  assert.equal(launched.threadId, "mcp:c400630d-714d-4262-9616-09205df51320");
  assert.deepEqual(calls, [
    {
      args: [
        "thread", "start",
        "--title", "Ostrom ticket 1: o/r#7",
        "--worktree", "main",
        "--branch", "ostrom/ticket-1-1",
        "--provider", "claudeAgent",
        "--model", "claude-haiku-5-5",
        "--json",
        "/work/ostrom",
        "-",
      ],
      stdin: "--not-a-flag; it goes on stdin",
    },
  ]);
});

test("show asks for the thread state without messages", async () => {
  const { calls, cli } = recording({ code: 0, stdout: fixture("show.json"), stderr: "" });
  assert.equal((await cli.showThread("mcp:x")).thread.status, "completed");
  assert.deepEqual(calls[0]?.args, ["thread", "show", "--messages", "0", "--json", "mcp:x"]);
});

test("send passes the message on stdin without --wait and reads the send result", async () => {
  const { calls, cli } = recording({ code: 0, stdout: fixture("send-nowait.json"), stderr: "" });
  const sent = await cli.sendMessage("mcp:f6e981f5-2da9-4a00-8d24-42f1dc982c93", "--not-a-flag");
  assert.deepEqual(sent, {
    threadId: "mcp:f6e981f5-2da9-4a00-8d24-42f1dc982c93",
    runId: "run:thread:mcp%3Af6e981f5-2da9-4a00-8d24-42f1dc982c93:ordinal:2",
    delivery: "started",
  });
  assert.deepEqual(calls, [
    { args: ["thread", "send", "--json", "mcp:f6e981f5-2da9-4a00-8d24-42f1dc982c93", "-"], stdin: "--not-a-flag" },
  ]);
  const missing = recording({ code: 1, stdout: "", stderr: "Thread mcp:gone was not found.\n" }).cli;
  await assert.rejects(missing.sendMessage("mcp:gone", "x"), /t3 thread send exited 1: Thread mcp:gone was not found\./);
});

test("a non-zero exit is an error carrying t3's message, even with JSON on stdout", async () => {
  const { cli } = recording({ code: 1, stdout: fixture("launch.json"), stderr: "Project not found.\n" });
  await assert.rejects(cli.startThread(start), {
    name: "T3CommandError",
    message: "t3 thread start exited 1: Project not found.",
  });
  const killed = recording({ code: null, stdout: "", stderr: "" }).cli;
  await assert.rejects(killed.showThread("mcp:x"), /exited by signal: no output/);
});

test("output the parser rejects is an error, not a success", async () => {
  const { cli } = recording({ code: 0, stdout: "mcp:x\n", stderr: "" });
  await assert.rejects(cli.startThread(start), T3OutputError);
});

test("a launch that started no run is an error", async () => {
  const stdout = JSON.stringify({ threadId: "mcp:x", projectId: "mcp:p", runId: null });
  const { cli } = recording({ code: 0, stdout, stderr: "" });
  await assert.rejects(cli.startThread(start), T3CommandError);
});

test("the process runner uses no shell and feeds stdin", async () => {
  // `node -e` echoes argv and stdin back; a shell would have expanded the $HOME argument.
  const run = processRunner(process.execPath);
  const script = "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>console.log(JSON.stringify([process.argv.slice(1),s])))";
  const exit = await run({ args: ["-e", script, "$HOME", "a b"], stdin: "brief" });
  assert.equal(exit.code, 0);
  assert.deepEqual(JSON.parse(exit.stdout), [["$HOME", "a b"], "brief"]);
  const failed = await run({ args: ["-e", "process.stderr.write('boom');process.exit(3)"] });
  assert.deepEqual(failed, { code: 3, stdout: "", stderr: "boom" });
  await assert.rejects(processRunner("/nonexistent/t3")({ args: [] }), /ENOENT/);
});
