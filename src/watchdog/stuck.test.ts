import assert from "node:assert/strict";
import { test } from "node:test";

import { parseStuckReply, renderStuckReply } from "./stuck.ts";

test("stuck reply snapshot", () => {
  assert.equal(
    renderStuckReply({
      tryingTo: "update the parser", blockedBy: "the expected format is unclear", tried: "read the fixtures",
      choices: "1. use format A\n2. use format B", workSoFar: "The branch has the parser change.",
    }),
    [
      "**I'm stuck on this and need a decision.**",
      "**Trying to:** update the parser",
      "**Blocked by:** the expected format is unclear",
      "**Tried:** read the fixtures",
      "**I need you to choose:** 1. use format A\n2. use format B",
      "**Work so far:** The branch has the parser change. Reply with a number, or anything else to redirect.",
    ].join("\n"),
  );
});

test("malformed Worker reply gets a complete, honest fallback", () => {
  assert.deepEqual(parseStuckReply('{"tryingTo":"only one field"}', "budget reached: tokens"), {
    tryingTo: "complete the requested work", blockedBy: "budget reached: tokens",
    tried: "two automatic recovery attempts on the same Worker session",
    choices: "1. provide the missing decision\n2. stop this ticket",
    workSoFar: "The Worker was asked to push its work in progress before this message.",
  });
});
