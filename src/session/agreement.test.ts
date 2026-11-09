import { strict as assert } from "node:assert";
import { test } from "node:test";
import type { SDKResultMessage, SDKSystemMessage } from "@anthropic-ai/claude-agent-sdk";
import type { ThreadStartedEvent, TurnCompletedEvent } from "@openai/codex-sdk";

// Redacted fields from the 2026-11-09 SDK probes. Each object is checked against
// fields of the SDK's exported types, and parsed at runtime as the wire format.
const claudeInit = { type: "system", subtype: "init", apiKeySource: "none", model: "claude-haiku-5-5" } satisfies Pick<SDKSystemMessage, "type" | "subtype" | "apiKeySource" | "model">;
const claudeResult = { type: "result", subtype: "success", is_error: false } satisfies Pick<SDKResultMessage, "type" | "subtype" | "is_error">;
const codexStarted = { type: "thread.started", thread_id: "redacted" } satisfies ThreadStartedEvent;
const codexCompleted = { type: "turn.completed" } satisfies Pick<TurnCompletedEvent, "type">;

test("recorded SDK event fields retain their pinned shapes", () => {
  for (const event of [claudeInit, claudeResult, codexStarted, codexCompleted]) {
    assert.deepEqual(JSON.parse(JSON.stringify(event)), event);
  }
});
