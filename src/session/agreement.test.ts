import { strict as assert } from "node:assert";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import type { SDKResultSuccess, SDKSystemMessage } from "@anthropic-ai/claude-agent-sdk";
import type { AgentMessageItem, ItemCompletedEvent, ThreadStartedEvent, TurnCompletedEvent } from "@openai/codex-sdk";

// The fixture projections are the fields the runner reads. Referencing SDK types
// here makes removal or type changes in those fields a compile-time failure.
type ClaudeInit = Pick<SDKSystemMessage, "type" | "subtype" | "session_id" | "model">;
type ClaudeResult = Pick<SDKResultSuccess, "type" | "subtype" | "session_id" | "result" | "is_error"> & {
  usage: Pick<SDKResultSuccess["usage"], "input_tokens" | "output_tokens">;
  modelUsage: Record<string, object>;
};
type CodexMessage = Pick<ItemCompletedEvent, "type"> & { item: Pick<AgentMessageItem, "id" | "type" | "text"> };

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
  assert.ok(Object.keys(result.modelUsage).length > 0);
});

test("recorded Codex events retain the fields consumed by the runner", () => {
  const [started, message, completed] = fixture("codex") as [ThreadStartedEvent, CodexMessage, TurnCompletedEvent];
  assert.equal(started.type, "thread.started"); assert.equal(typeof started.thread_id, "string");
  assert.equal(message.type, "item.completed"); assert.equal(message.item.type, "agent_message");
  assert.equal(typeof message.item.text, "string");
  assert.equal(completed.type, "turn.completed");
  assert.equal(typeof completed.usage.input_tokens, "number"); assert.equal(typeof completed.usage.output_tokens, "number");
});
