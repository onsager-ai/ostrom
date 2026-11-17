import { strict as assert } from "node:assert";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { rolloutContext, rolloutModel } from "./rollout.ts";

function root(): string {
  return mkdtempSync(join(tmpdir(), "rollout-"));
}

const line = (type: string, payload: unknown) => JSON.stringify({ type, payload });

test("the model is the latest turn_context in the thread's rollout", () => {
  const dir = root();
  try {
    mkdirSync(join(dir, "2026", "11", "09"), { recursive: true });
    writeFileSync(
      join(dir, "2026", "11", "09", "rollout-2026-11-09T01-02-03-thr-1.jsonl"),
      [line("session_meta", { id: "thr-1" }), line("turn_context", { model: "a", other: 1 }), line("turn_context", { model: "gpt-6-luna" })].join("\n") + "\n",
    );
    writeFileSync(join(dir, "2026", "11", "09", "rollout-2026-11-09T01-02-04-thr-2.jsonl"), line("turn_context", { model: "other" }));
    assert.equal(rolloutModel(dir, "thr-1"), "gpt-6-luna");
  } finally {
    rmSync(dir, { recursive: true });
  }
});

test("Codex rollout agreement: date path and latest turn_context expose raw model and effort", () => {
  const dir = root();
  try {
    const day = join(dir, "2026", "11", "17");
    mkdirSync(day, { recursive: true });
    writeFileSync(join(day, "rollout-2026-11-17T01-02-03-thread-id.jsonl"), [
      line("turn_context", { model: "old", effort: "high" }),
      JSON.stringify({ type: "turn_context", payload: { effort: "low", model: "gpt-6-luna" } }, null, 0),
      line("event_msg", { message: '"type":"turn_context","payload":{"model":"wrong"}' }),
    ].join("\n") + "\n");
    assert.deepEqual(rolloutContext(dir, "thread-id"), { model: "gpt-6-luna", effort: "low" });
    writeFileSync(join(day, "rollout-2026-11-17T01-02-03-thread-id.jsonl"), line("turn_context", { model: "gpt-6-luna" }));
    assert.deepEqual(rolloutContext(dir, "thread-id"), { model: "gpt-6-luna", effort: null });
  } finally { rmSync(dir, { recursive: true }); }
});

test("a missing rollout or one with no turn model throws", () => {
  const dir = root();
  try {
    mkdirSync(join(dir, "2026", "11", "09"), { recursive: true });
    assert.throws(() => rolloutModel(dir, "thr-1"), /no rollout record/);
    writeFileSync(join(dir, "2026", "11", "09", "rollout-x-thr-1.jsonl"), line("session_meta", { id: "thr-1" }));
    assert.throws(() => rolloutModel(dir, "thr-1"), /no turn model/);
  } finally {
    rmSync(dir, { recursive: true });
  }
});
