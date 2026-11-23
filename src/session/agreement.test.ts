import { strict as assert } from "node:assert";
import { lstatSync, readFileSync, realpathSync } from "node:fs";
import { execFile } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import { test } from "node:test";
import type { Options, SDKMessage, SDKResultSuccess, SDKSystemMessage } from "@anthropic-ai/claude-agent-sdk";
import { Codex, type AgentMessageItem, type ItemCompletedEvent, type ThreadOptions, type ThreadStartedEvent, type TurnCompletedEvent, type TurnOptions } from "@openai/codex-sdk";
import { sessionConfig } from "./config.ts";
import { claudeRunner, codexRunner } from "./runner.ts";

// The fixture projections are the fields the runner reads. Referencing SDK types
// here makes removal or type changes in those fields a compile-time failure.
type ClaudeInit = Pick<SDKSystemMessage, "type" | "subtype" | "session_id" | "model">;
type ClaudeResult = Pick<SDKResultSuccess, "type" | "subtype" | "session_id" | "result" | "is_error"> & {
  usage: Pick<SDKResultSuccess["usage"], "input_tokens" | "output_tokens" | "cache_read_input_tokens" | "cache_creation_input_tokens">;
  modelUsage: SDKResultSuccess["modelUsage"];
};
type CodexMessage = Pick<ItemCompletedEvent, "type"> & { item: Pick<AgentMessageItem, "id" | "type" | "text"> };

test("pinned Codex CLI recognizes App shell controls as booleans", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-codex-shell-agreement-"));
  try {
    // Resolve the CLI dependency from the SDK, not an operator-installed codex on PATH.
    const require = createRequire(import.meta.resolve("@openai/codex-sdk"));
    const cli = join(dirname(require.resolve("@openai/codex/package.json")), "bin", "codex.js");
    const exec = promisify(execFile);
    const run = (...overrides: string[]) => exec(process.execPath,
      [cli, ...overrides.flatMap((value) => ["-c", value]), "features", "list"],
      { cwd: root, env: { PATH: process.env["PATH"], HOME: root, CODEX_HOME: root }, timeout: 15_000 });
    const result = await run("allow_login_shell=false", "features.shell_snapshot=false");
    assert.match(result.stdout, /^shell_snapshot[^\S\n]+[^\n]+[^\S\n]+false[^\S\n]*$/m);
    // Unknown keys may be ignored; invalid-type rejection proves these keys are recognized.
    await assert.rejects(run('allow_login_shell="invalid"'), /allow_login_shell|boolean/);
    await assert.rejects(run('features.shell_snapshot="invalid"'), /shell_snapshot|boolean/);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("pinned Codex SDK applies provider overrides to start and resume before session creation", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-codex-provider-agreement-"));
  try {
    const workingDirectory = realpathSync(root);
    // Refuse to launch under a checkout, where the valid provider could pass the CLI's Git check.
    for (let dir = workingDirectory; ; dir = dirname(dir)) {
      const gitEntry = join(dir, ".git");
      assert.equal(lstatSync(gitEntry, { throwIfNoEntry: false }), undefined, `Unexpected Git entry: ${gitEntry}`);
      if (dir === dirname(dir)) break;
    }
    await writeFile(join(root, "config.toml"), [
      'model_provider = "file_provider_missing"',
      'analytics.enabled = false',
      'otel.metrics_exporter = "none"',
      '',
    ].join("\n"));
    const env = { PATH: process.env["PATH"], HOME: root, CODEX_HOME: root };
    const config = (provider?: string) => sessionConfig({ ...env, OSTROM_CODEX_PROVIDER: provider }).codex;
    const sessionId = "00000000-0000-0000-0000-000000000000";
    const run = async (codex: Codex, resume: boolean) => {
      const options = { workingDirectory, model: "offline-probe" };
      const thread = resume ? codex.resumeThread(sessionId, options) : codex.startThread(options);
      const { events } = await thread.runStreamed("offline provider probe", { signal: AbortSignal.timeout(15_000) });
      for await (const event of events) assert.fail(`unexpected session event: ${event.type}`);
    };
    for (const resume of [false, true]) {
      await assert.rejects(run(new Codex(config()), resume), /Model provider `file_provider_missing` not found/);
      await assert.rejects(run(new Codex(config("override_provider_missing")), resume), /Model provider `override_provider_missing` not found/);
      await assert.rejects(run(new Codex(config("openai")), resume), /Not inside a trusted directory/);
    }

    // Native preflight errors occur before session lookup, so capture the SDK's resume arguments separately.
    const fakeCli = join(root, "record-codex-argv");
    await writeFile(fakeCli, '#!/bin/sh\ncat >/dev/null\nprintf "%s\\n" "$@" > "$CODEX_HOME/resume-argv"\n', { mode: 0o700 });
    await run(new Codex({ ...config("openai"), codexPathOverride: fakeCli }), true);
    const argv = readFileSync(join(root, "resume-argv"), "utf8").trimEnd().split("\n");
    assert.deepEqual(argv.slice(-2), ["resume", sessionId]);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("effort options retain the pinned SDK names and values across same-session resumes", async () => {
  const claudeCalls: Options[] = [];
  const run = async function* (input: Parameters<typeof import("@anthropic-ai/claude-agent-sdk").query>[0]): AsyncGenerator<SDKMessage> {
    claudeCalls.push(input.options!);
    yield* fixture("claude") as SDKMessage[];
  };
  const claude = claudeRunner({}, run).resume({ cwd: "/tmp", model: "haiku", sessionId: "claude-id", effort: "low" });
  await claude.send("first");
  await claude.send("next", undefined, "max");
  await claude.send("default", undefined, null);
  assert.deepEqual(claudeCalls.map((options) => options.effort), ["low", "max", undefined]);
  assert.equal("effort" in claudeCalls[2]!, false);

  const calls: { id: string | null; options: ThreadOptions }[] = [];
  const thread = { runStreamed: async () => ({ events: (async function* () { yield* fixture("codex") as import("@openai/codex-sdk").ThreadEvent[]; })() }) };
  const client = {
    startThread: (options: ThreadOptions) => { calls.push({ id: null, options }); return thread; },
    resumeThread: (id: string, options: ThreadOptions) => { calls.push({ id, options }); return thread; },
  } as unknown as Pick<Codex, "startThread" | "resumeThread">;
  const codex = codexRunner({}, () => client).resume({ cwd: "/tmp", model: "luna", sessionId: "codex-id", effort: "minimal" });
  await codex.send("first");
  await codex.send("next", undefined, "xhigh");
  await codex.send("default", undefined, null);
  assert.deepEqual(calls.map(({ options }) => options.modelReasoningEffort), ["minimal", "xhigh", undefined]);
  assert.equal(calls[0]!.id, "codex-id");
  assert.ok(calls.every(({ id }) => id !== null));
  assert.equal("modelReasoningEffort" in calls[2]!.options, false);
});

test("pinned SDK cancellation uses Claude abortController and Codex turn signal", async () => {
  const stop = new AbortController();
  let claudeSignal: AbortSignal | undefined;
  let activity = 0;
  const run = async function* (input: Parameters<typeof import("@anthropic-ai/claude-agent-sdk").query>[0]): AsyncGenerator<SDKMessage> {
    const options: Pick<Options, "abortController"> = input.options!;
    claudeSignal = options.abortController!.signal;
    yield (fixture("claude") as SDKMessage[])[0]!;
    await new Promise<void>((resolve) => {
      claudeSignal!.addEventListener("abort", () => resolve(), { once: true });
      stop.abort();
    });
    throw new Error("SDK aborted");
  };
  const claude = claudeRunner({}, run).start({ cwd: "/tmp", model: "haiku", onActivity: () => { activity++; } });
  assert.equal((await claude.send("work", stop.signal)).status, "cancelled");
  assert.equal(claudeSignal!.aborted, true);
  assert.equal(activity, 1);

  const codexStop = new AbortController();
  let codexSignal: TurnOptions["signal"];
  const thread = { runStreamed: async (_prompt: string, options?: TurnOptions) => {
    codexSignal = options?.signal;
    return { events: (async function* (): AsyncGenerator<import("@openai/codex-sdk").ThreadEvent> {
      yield { type: "thread.started", thread_id: "id" };
      await new Promise<void>((resolve) => {
        codexSignal!.addEventListener("abort", () => resolve(), { once: true });
        codexStop.abort();
      });
      throw new Error("SDK aborted");
    })() };
  } };
  const client = { startThread: () => thread, resumeThread: () => thread } as unknown as Pick<Codex, "startThread" | "resumeThread">;
  const codex = codexRunner({}, () => client).start({ cwd: "/tmp", model: "luna", onActivity: () => { activity++; } });
  assert.equal((await codex.send("work", codexStop.signal)).status, "cancelled");
  assert.equal(codexSignal, codexStop.signal);
  assert.equal(codexSignal!.aborted, true);
  assert.equal(activity, 2);
});

function fixture(name: string): unknown {
  return JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), "utf8"));
}

test("recorded Claude events retain the fields consumed by the runner", () => {
  const [init, result] = fixture("claude") as [ClaudeInit, ClaudeResult];
  assert.equal(init.type, "system"); assert.equal(init.subtype, "init");
  assert.equal(typeof init.session_id, "string"); assert.equal(typeof init.model, "string");
  assert.equal(result.type, "result"); assert.equal(result.subtype, "success");
  assert.equal(typeof result.session_id, "string"); assert.equal(typeof result.result, "string");
  assert.equal(typeof result.is_error, "boolean");
  assert.equal(typeof result.usage.input_tokens, "number"); assert.equal(typeof result.usage.output_tokens, "number");
  assert.equal(typeof result.usage.cache_read_input_tokens, "number"); assert.equal(typeof result.usage.cache_creation_input_tokens, "number");
  assert.ok(Object.keys(result.modelUsage).length > 0);
});

test("recorded Codex events retain the fields consumed by the runner", () => {
  const [started, message, completed] = fixture("codex") as [ThreadStartedEvent, CodexMessage, TurnCompletedEvent];
  assert.equal(started.type, "thread.started"); assert.equal(typeof started.thread_id, "string");
  assert.equal(message.type, "item.completed"); assert.equal(message.item.type, "agent_message");
  assert.equal(typeof message.item.text, "string");
  assert.equal(completed.type, "turn.completed");
  assert.equal(typeof completed.usage.input_tokens, "number"); assert.equal(typeof completed.usage.cached_input_tokens, "number");
  assert.equal(typeof completed.usage.output_tokens, "number");
});
