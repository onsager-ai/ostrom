import { strict as assert } from "node:assert";
import { test } from "node:test";
import type { SDKMessage } from "@anthropic-ai/claude-agent-sdk";
import type { Codex, ThreadEvent } from "@openai/codex-sdk";
import { claudeRunner, codexRunner } from "./runner.ts";
import { sessionConfig } from "./config.ts";

test("Claude resumes the same session with the explicit model", async () => {
  const calls: { model?: string; resume?: string; permissionMode: string | undefined; allowDangerouslySkipPermissions: boolean | undefined }[] = [];
  const run = async function* (input: Parameters<typeof import("@anthropic-ai/claude-agent-sdk").query>[0]): AsyncGenerator<SDKMessage> {
    calls.push({ ...(input.options?.model ? { model: input.options.model } : {}), ...(input.options?.resume ? { resume: input.options.resume } : {}), permissionMode: input.options?.permissionMode, allowDangerouslySkipPermissions: input.options?.allowDangerouslySkipPermissions });
    yield { type: "system", subtype: "init", session_id: "session", model: "haiku" } as SDKMessage;
    yield { type: "result", subtype: "success", session_id: "session", result: "ok", is_error: false, modelUsage: { haiku: {}, auxiliary: {} }, usage: { input_tokens: 4, cache_read_input_tokens: 3, cache_creation_input_tokens: 1, output_tokens: 2 } } as unknown as SDKMessage;
  };
  const session = claudeRunner({}, run).start({ cwd: "/tmp", model: "haiku" });
  assert.deepEqual(await session.send("hello"), { sessionId: "session", text: "ok", status: "completed", model: "haiku", modelUsage: { haiku: {}, auxiliary: {} }, usage: { inputTokens: 5, cachedInputTokens: 3, outputTokens: 2 } });
  await session.send("again");
  assert.deepEqual(calls, [{ model: "haiku", permissionMode: "bypassPermissions", allowDangerouslySkipPermissions: true }, { model: "haiku", resume: "session", permissionMode: "bypassPermissions", allowDangerouslySkipPermissions: true }]);
});

test("Codex returns an unverified model and forwards resume options", async () => {
  const events: ThreadEvent[] = [
    { type: "thread.started", thread_id: "id" },
    { type: "item.completed", item: { type: "agent_message", id: "item", text: "ok" } },
    { type: "turn.completed", usage: { input_tokens: 4, cached_input_tokens: 3, cache_write_input_tokens: 0, output_tokens: 2, reasoning_output_tokens: 0 } },
  ];
  let seen: unknown;
  const thread = { runStreamed: async () => ({ events: (async function* () { yield* events; })() }) };
  const client = { startThread: () => thread, resumeThread: (id: string, options: unknown) => { seen = { id, options }; return thread; } } as unknown as Pick<Codex, "startThread" | "resumeThread">;
  const session = codexRunner({}, client).resume({ cwd: "/tmp", model: "luna", sessionId: "id" });
  const result = await session.send("hello");
  assert.equal(result.model, null);
  assert.deepEqual(result.usage, { inputTokens: 1, cachedInputTokens: 3, outputTokens: 2 });
  assert.deepEqual(seen, { id: "id", options: { workingDirectory: "/tmp", model: "luna", sandboxMode: "workspace-write", networkAccessEnabled: true, approvalPolicy: "never" } });
});

test("Codex cancellation is reported rather than treated as success", async () => {
  const thread = { runStreamed: async (_prompt: string, options: { signal: AbortSignal }) => {
    assert.fail(`cancelled turn unexpectedly started: ${options.signal.aborted}`);
  } };
  const client = { startThread: () => thread } as unknown as Pick<Codex, "startThread" | "resumeThread">;
  const controller = new AbortController(); controller.abort();
  const result = await codexRunner({}, client).start({ cwd: "/tmp", model: "luna" }).send("hello", controller.signal);
  assert.equal(result.status, "cancelled");
});

test("provider selection comes from the environment", () => {
  assert.deepEqual(sessionConfig({ OSTROM_CODEX_PROVIDER: "openai" }).codex.config, { model_provider: "openai" });
});

test("Claude login mode drops gateway and API-key variables", () => {
  const env = { ANTHROPIC_AUTH_TOKEN: "t", ANTHROPIC_BASE_URL: "u", ANTHROPIC_API_KEY: "k", HOME: "/h" };
  assert.deepEqual(sessionConfig({ ...env, OSTROM_CLAUDE_AUTH: "login" }).claudeEnv, { HOME: "/h", OSTROM_CLAUDE_AUTH: "login" });
  assert.deepEqual(sessionConfig(env).claudeEnv, env);
  assert.throws(() => sessionConfig({ OSTROM_CLAUDE_AUTH: "key" }), /env or login/);
});
