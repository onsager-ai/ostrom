// Agreement test (AGENTS.md rule 5): the fields Ostrom reads from `t3 ... --json` must exist in
// the upstream T3 Code schemas at the pinned commit, with types our parsers accept. The relation
// is "everything upstream may print, we read correctly", not "we mirror upstream": upstream may
// carry fields we ignore. Upstream is vendored verbatim under vendor/t3code by
// scripts/vendor-t3code.sh; this file is the only reader of it.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import * as Arbitrary from "effect/Arbitrary";
import * as Effect from "effect/Effect";
import * as Schema from "effect/Schema";

import { parseRunResult, parseThreadShow, T3_THREAD_STATUSES } from "./cli-json.ts";
import { T3_PIN } from "./pin.ts";

const vendor = new URL("../../vendor/t3code/", import.meta.url);
const vendored = (path: string) => readFileSync(new URL(path, vendor), "utf8");
const fixture = (name: string) => readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8");

// A computed specifier keeps tsc out of the vendored tree: it is upstream's code, checked by
// upstream's compiler settings, and is only executed here.
const contractsPath = new URL("packages/contracts/src/orchestratorMcp.ts", vendor).href;
const baseSchemasPath = new URL("packages/contracts/src/baseSchemas.ts", vendor).href;
const contracts = await import(contractsPath);
const base = await import(baseSchemasPath);

const ThreadDetail = contracts.OrchestratorMcpThreadDetail;
const TimelineItem = contracts.OrchestratorMcpThreadTimelineItem;
const ThreadStatus = contracts.OrchestratorMcpThreadStatus;
const ThreadId = base.ThreadId;
const RunId = base.RunId;

// What `t3 thread start|send --wait --json` prints. Upstream has no named schema for it: the CLI
// builds it inline in finishRun (apps/server/src/cli/thread.ts). The structure below is read from
// that code, and the source anchors in the next test fail when that code changes.
const RunResult = Schema.Struct({
  threadId: ThreadId,
  runId: RunId,
  status: ThreadStatus,
  reply: Schema.NullOr(Schema.String),
  error: Schema.NullOr(Schema.String),
});

// What `t3 thread show --json` prints: the thread detail as read, plus one entry per message,
// also built inline in the CLI's show command.
const ThreadShow = Schema.Struct({
  thread: ThreadDetail,
  messages: Schema.Array(
    Schema.Struct({
      role: Schema.Literals(["user", "assistant"]),
      runId: TimelineItem.fields["runId"],
      text: Schema.String,
    }),
  ),
});

const SAMPLES = 300;
// Values as the CLI prints them: generated from the upstream schema, then through JSON.
const printed = async (schema: Schema.Constraint): Promise<any[]> => {
  const values = await Effect.runPromise(
    Arbitrary.sampleEffect(Arbitrary.schema(schema), { count: SAMPLES, seed: 7 }),
  );
  return values.map((value) => JSON.parse(JSON.stringify(value)));
};

test("the vendored upstream tree is the pinned commit and CLI version", () => {
  assert.match(vendored("SOURCE.md"), new RegExp(`^- Commit: ${T3_PIN.commit}$`, "m"));
  assert.match(vendored("SOURCE.md"), new RegExp(`^- Repository: https://github.com/${T3_PIN.repository}$`, "m"));
  assert.equal(JSON.parse(vendored("packages/contracts/package.json")).version, T3_PIN.cliVersion);
});

test("the CLI still builds its --json output the way the schemas above assume", () => {
  const cli = vendored("apps/server/src/cli/thread.ts");
  const anchors = [
    // start/send --wait: the run result, where status is the wait's thread status or "waiting".
    `const status = outcome.type === "ended" ? outcome.status : "waiting";`,
    `yield* printJson({ threadId: input.threadId, runId, status, reply, error });`,
    `type: "assistant_message" | "error",`,
    // show: the thread from t3_thread_read, and one entry per message.
    `OrchestratorMcpThreadReadResult,`,
    `role: item.type === "user_message" ? "user" : "assistant",`,
    `runId: item.runId,`,
    `return yield* printJson({ thread, messages });`,
  ];
  for (const anchor of anchors) {
    assert.ok(cli.includes(anchor), `upstream thread.ts changed; re-derive the shape around: ${anchor}`);
  }
});

test("every thread status upstream can report is one the parser accepts", () => {
  const upstream = (ThreadStatus as unknown as { members: ReadonlyArray<{ literal?: string; literals?: readonly string[] }> })
    .members.flatMap((member) => member.literals ?? (member.literal === undefined ? [] : [member.literal]));
  assert.ok(upstream.length > 0, "could not read the upstream status literals");
  for (const status of upstream) {
    assert.ok((T3_THREAD_STATUSES as readonly string[]).includes(status), `unknown upstream status ${status}`);
  }
});

test("the real captures are valid under the upstream schemas", () => {
  for (const name of ["start.json", "send.json"]) {
    Schema.decodeUnknownSync(RunResult)(JSON.parse(fixture(name)));
  }
  Schema.decodeUnknownSync(ThreadShow)(JSON.parse(fixture("show.json")));
});

test("the run-result parser reads every value upstream can print", async () => {
  for (const value of await printed(RunResult)) {
    assert.deepEqual(parseRunResult(JSON.stringify(value)), {
      threadId: value.threadId,
      runId: value.runId,
      status: value.status,
      reply: value.reply,
      error: value.error,
    });
  }
});

test("the thread-show parser reads every value upstream can print", async () => {
  for (const value of await printed(ThreadShow)) {
    const thread = value.thread;
    assert.deepEqual(parseThreadShow(JSON.stringify(value)), {
      thread: {
        threadId: thread.threadId,
        status: thread.status,
        activeRunId: thread.activeRunId,
        settled: thread.settled,
        pendingRequestCount: thread.pendingRequestCount,
        branch: thread.branch,
        worktreePath: thread.worktreePath,
        linkedPullRequest:
          thread.linkedPullRequest === null
            ? null
            : {
                repository: thread.linkedPullRequest.repository,
                number: thread.linkedPullRequest.number,
                url: thread.linkedPullRequest.url,
              },
        pullRequests: (thread.pullRequests ?? []).map((pr: any) => ({
          host: pr.host,
          repository: pr.repository,
          number: pr.number,
          url: pr.url,
          state: pr.state,
        })),
      },
      messages: value.messages,
    });
  }
});
