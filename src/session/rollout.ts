import { closeSync, openSync, readdirSync, readSync, fstatSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

/** How much of the end of a rollout file is read; its records can be large, so this is bounded. */
const TAIL_BYTES = 2 * 1024 * 1024;

export const codexSessionsDir = (): string => join(homedir(), ".codex", "sessions");

/**
 * The model Codex recorded for the latest turn of a thread, read from its rollout file
 * (`<root>/YYYY/MM/DD/rollout-<time>-<threadId>.jsonl`). Throws when the record is missing or
 * names no model: an unreadable record is unverified, never assumed correct.
 */
export function rolloutModel(root: string, threadId: string): string {
  const context = rolloutContext(root, threadId);
  if (context.model === null) throw new Error("rollout records no turn model");
  return context.model;
}

export function rolloutContext(root: string, threadId: string): { model: string | null; effort: string | null } {
  const path = findRollout(root, threadId);
  const fd = openSync(path, "r");
  let text: string;
  try {
    const size = fstatSync(fd).size;
    const length = Math.min(size, TAIL_BYTES);
    const buffer = Buffer.alloc(length);
    readSync(fd, buffer, 0, length, size - length);
    text = buffer.toString("utf8");
  } finally {
    closeSync(fd);
  }
  let context: { model: string | null; effort: string | null } | undefined;
  for (const line of text.split("\n")) {
    let record: unknown;
    try { record = JSON.parse(line); }
    catch { continue; } // The bounded tail may start in the middle of a JSONL record.
    if (typeof record !== "object" || record === null || !("type" in record) || record.type !== "turn_context" || !("payload" in record)) continue;
    const payload = record.payload;
    if (typeof payload !== "object" || payload === null) continue;
    context = {
      model: "model" in payload && typeof payload.model === "string" ? payload.model : null,
      effort: "effort" in payload && typeof payload.effort === "string" ? payload.effort : null,
    };
  }
  if (context === undefined) throw new Error(`rollout ${path} records no turn model`);
  return context;
}

function findRollout(root: string, threadId: string): string {
  const suffix = `-${threadId}.jsonl`;
  const down = (dir: string) => readdirSync(dir).sort().reverse();
  for (const year of down(root)) {
    for (const month of down(join(root, year))) {
      for (const day of down(join(root, year, month))) {
        const dir = join(root, year, month, day);
        const name = readdirSync(dir).find((file) => file.startsWith("rollout-") && file.endsWith(suffix));
        if (name !== undefined) return join(dir, name);
      }
    }
  }
  throw new Error(`no rollout record for thread ${threadId} under ${root}`);
}
