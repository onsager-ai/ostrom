import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { annotationsPath, appendSessionAnnotation, appendSessionAnnotationIfChanged, buildSessionAnnotation, validateSessionAnnotation } from "./annotations.ts";

const sample = {
  harness: "codex" as const, native_id: "native-1", system: "ostrom", repository: "owner/repo", ticket: "42",
  issue: 42, pr: null, title: "A title", updated_at: "2026-11-19T04:52:01Z",
};

test("builds only the v1 contract fields and truncates title to 200 characters", () => {
  const line = buildSessionAnnotation({ ...sample, title: "x".repeat(201) });
  const parsed = JSON.parse(line) as Record<string, unknown>;
  assert.deepEqual(Object.keys(parsed), ["v", "harness", "native_id", "system", "repository", "ticket", "issue", "pr", "title", "updated_at"]);
  assert.equal(parsed["title"], "x".repeat(200));
  assert.ok(line.endsWith("\n"));
  assert.ok(Buffer.byteLength(line) <= 4096);
  assert.doesNotThrow(() => validateSessionAnnotation(parsed));
});

test("rejects invalid types, nonpositive links, unknown fields and invalid timestamps", () => {
  const good = JSON.parse(buildSessionAnnotation(sample)) as Record<string, unknown>;
  for (const patch of [
    { v: 2 }, { harness: "other" }, { native_id: "" }, { system: 3 }, { repository: "" }, { ticket: null },
    { issue: 0 }, { pr: -1 }, { title: "x".repeat(201) }, { updated_at: "yesterday" }, { extra: true }, { issue: 1.5 },
  ]) {
    assert.throws(() => validateSessionAnnotation({ ...good, ...patch }));
  }
  assert.throws(() => validateSessionAnnotation({ ...good, issue: undefined }));
});

test("enforces the 4 KiB line cap", () => {
  assert.throws(() => buildSessionAnnotation({ ...sample, repository: "r".repeat(5000) }), /4 KiB/);
});

test("uses absolute XDG state home and falls back for unset or relative values", () => {
  assert.equal(annotationsPath({ XDG_STATE_HOME: "/state" }), "/state/ostrom/semon-annotations.jsonl");
  assert.equal(annotationsPath({ HOME: "/home/test", XDG_STATE_HOME: "relative" }), "/home/test/.local/state/ostrom/semon-annotations.jsonl");
});

test("appends one complete newline terminated record", () => {
  const home = mkdtempSync(join(tmpdir(), "annotations-"));
  try {
    const env = { HOME: home };
    appendSessionAnnotation(sample, env);
    appendSessionAnnotation({ ...sample, title: null }, env);
    const lines = readFileSync(annotationsPath(env), "utf8").split("\n");
    assert.equal(lines.length, 3);
    assert.ok(lines.slice(0, 2).every((line) => JSON.parse(line).native_id === "native-1"));
  } finally { rmSync(home, { recursive: true }); }
});

test("appendSessionAnnotationIfChanged writes only when the PR or title changes and counts title characters", () => {
  const home = mkdtempSync(join(tmpdir(), "annotations-changed-"));
  try {
    const env = { HOME: home };
    const input = { ...sample, native_id: "changed-1", pr: null, title: null };
    assert.equal(appendSessionAnnotationIfChanged(input, env), true);
    assert.equal(appendSessionAnnotationIfChanged(input, env), false);
    assert.equal(appendSessionAnnotationIfChanged({ ...input, pr: 12 }, env), true);
    assert.equal(appendSessionAnnotationIfChanged({ ...input, pr: 12 }, env), false);
    const long = "😀".repeat(201);
    assert.equal(appendSessionAnnotationIfChanged({ ...input, pr: 12, title: long }, env), true);
    assert.equal(appendSessionAnnotationIfChanged({ ...input, pr: 12, title: long }, env), false);
    const lines = readFileSync(annotationsPath(env), "utf8").trim().split("\n").map((text) => JSON.parse(text) as { title: string });
    assert.equal(lines.length, 3);
    assert.equal([...lines[2]!.title].length, 200);
    assert.equal(lines[2]!.title, "😀".repeat(200));
  } finally { rmSync(home, { recursive: true }); }
});
