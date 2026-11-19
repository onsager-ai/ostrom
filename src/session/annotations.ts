import { appendFileSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";

export interface SessionAnnotation {
  v: 1;
  harness: "claude" | "codex";
  native_id: string;
  system: string;
  repository: string;
  ticket: string;
  issue: number | null;
  pr: number | null;
  title: string | null;
  updated_at: string;
}

const KEYS = ["v", "harness", "native_id", "system", "repository", "ticket", "issue", "pr", "title", "updated_at"];

/** Counts characters as code points, the same way validation does, so a title never splits a surrogate pair. */
export function truncateTitle(title: string | null): string | null {
  return title === null ? null : [...title].slice(0, 200).join("");
}

export function buildSessionAnnotation(input: Omit<SessionAnnotation, "v" | "title"> & { title: string | null }): string {
  const record: SessionAnnotation = { v: 1, ...input, title: truncateTitle(input.title) };
  validateSessionAnnotation(record);
  const line = `${JSON.stringify(record)}\n`;
  if (Buffer.byteLength(line, "utf8") > 4096) throw new Error("session annotation exceeds 4 KiB");
  return line;
}

export function validateSessionAnnotation(value: unknown): asserts value is SessionAnnotation {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("annotation must be an object");
  const record = value as Record<string, unknown>;
  if (Object.keys(record).length !== KEYS.length || KEYS.some((key) => !(key in record))) throw new Error("annotation fields do not match v1");
  if (record["v"] !== 1) throw new Error("unsupported annotation version");
  if (record["harness"] !== "claude" && record["harness"] !== "codex") throw new Error("invalid harness");
  for (const key of ["native_id", "system", "repository", "ticket"] as const) {
    if (typeof record[key] !== "string" || record[key].length === 0) throw new Error(`invalid ${key}`);
  }
  for (const key of ["issue", "pr"] as const) {
    if (record[key] !== null && (!Number.isSafeInteger(record[key]) || (record[key] as number) <= 0)) throw new Error(`invalid ${key}`);
  }
  if (record["title"] !== null && (typeof record["title"] !== "string" || [...record["title"]].length > 200)) throw new Error("invalid title");
  if (typeof record["updated_at"] !== "string" || !/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)$/.test(record["updated_at"]) || Number.isNaN(Date.parse(record["updated_at"]))) {
    throw new Error("invalid updated_at");
  }
}

export function annotationsPath(env: Record<string, string | undefined>): string {
  const xdg = env["XDG_STATE_HOME"];
  const stateHome = xdg && isAbsolute(xdg) ? xdg : join(env["HOME"] && isAbsolute(env["HOME"]) ? env["HOME"] : homedir(), ".local", "state");
  return join(stateHome, "ostrom", "semon-annotations.jsonl");
}

const lastAnnotationBySession = new Map<string, { pr: number | null; title: string | null }>();

/**
 * Appends a line only when the PR number or title differs from the last line this process wrote
 * for the session. The memory is per process, so the first pass after a restart writes one line
 * that repeats the last one; Semon keeps the latest line, so that is harmless.
 */
export function appendSessionAnnotationIfChanged(input: Parameters<typeof buildSessionAnnotation>[0], env = process.env): boolean {
  const key = `${annotationsPath(env)}:${input.harness}:${input.native_id}`;
  const next = { pr: input.pr, title: truncateTitle(input.title) };
  const previous = lastAnnotationBySession.get(key);
  if (previous && previous.pr === next.pr && previous.title === next.title) return false;
  appendSessionAnnotation(input, env);
  lastAnnotationBySession.set(key, next);
  return true;
}

export function appendSessionAnnotation(input: Parameters<typeof buildSessionAnnotation>[0], env = process.env): void {
  const path = annotationsPath(env);
  mkdirSync(dirname(path), { recursive: true });
  appendFileSync(path, buildSessionAnnotation(input), { encoding: "utf8", flag: "a" });
}
