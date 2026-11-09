import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

import { parseComment } from "./parser.ts";
import { controlsLine, renderCommandTable } from "./table.ts";

test("commands are case-insensitive and must start the comment", () => {
  assert.deepEqual(parseComment("@ostrom pause"), { kind: "command", command: "pause" });
  assert.deepEqual(parseComment("  @OSTROM ReSuMe until tomorrow"), { kind: "command", command: "resume" });
  assert.deepEqual(parseComment("@ostrom stop\nplease save progress"), { kind: "command", command: "stop" });
  assert.deepEqual(parseComment("please @ostrom pause"), { kind: "redirect" });
  assert.deepEqual(parseComment("@ostrom paused"), { kind: "redirect" });
  assert.deepEqual(parseComment("@ostrom help"), { kind: "redirect" });
});

test("Journey 1 command table is generated from the command contract", async () => {
  const experience = await readFile(new URL("../../docs/experience.md", import.meta.url), "utf8");
  const actual = experience.match(/\| Say or do \| Effect \| Who may \| Acknowledged by \|\n(?:\|.*\n?){8}/)?.[0].trimEnd();
  assert.equal(actual, renderCommandTable());
});

test("the status-card controls come from the command contract", () => {
  assert.equal(controlsLine(), "Controls: `@ostrom pause`, `@ostrom resume`, `@ostrom stop`, or reply to redirect.");
});
