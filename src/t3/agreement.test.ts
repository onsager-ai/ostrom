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

import {
  parseLaunchResult,
  parseRunResult,
  parseThreadShow,
  T3_PULL_REQUEST_SOURCES,
  T3_THREAD_STATUSES,
} from "./cli-json.ts";
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
const PullRequestSource = contracts.OrchestratorMcpThreadPullRequest.fields["source"];
const ThreadId = base.ThreadId;
const RunId = base.RunId;
const ProjectId = base.ProjectId;

// What `t3 thread start --json` prints without --wait: the CLI's own ThreadLaunchResult, decoded
// from the t3_thread_launch reply and printed as is (apps/server/src/cli/thread.ts).
const LaunchResult = Schema.Struct({
  threadId: ThreadId,
  projectId: ProjectId,
  runId: Schema.NullOr(RunId),
});

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

// The upstream schemas arrive untyped (see the computed import above), which leaves their
// decoding services as `any`; upstream's schemas need none, so decode them as plain codecs.
const decode = (schema: Schema.Constraint, value: unknown) =>
  Schema.decodeUnknownSync(schema as unknown as Schema.Codec<unknown>)(value);

const SAMPLES = 300;
// Values as the CLI prints them: generated from the upstream schema, then through JSON.
const printed = async (schema: Schema.Constraint): Promise<any[]> => {
  const values = await Effect.runPromise(
    Arbitrary.sampleEffect(Arbitrary.schema(schema), { count: SAMPLES, seed: 7 }),
  );
  return values.map((value) => JSON.parse(JSON.stringify(value)));
};

// The vendor script resolves the release tag to its commit against upstream and records both.
// Upstream's package.json files are not evidence of the release version: at the v0.1.7 tag they
// still read 0.1.6, because the release build stamps the version.
test("the vendored upstream tree is the pinned release tag and its commit", () => {
  const source = vendored("SOURCE.md");
  assert.equal(T3_PIN.releaseTag, `v${T3_PIN.cliVersion}`);
  assert.match(source, new RegExp(`^- Repository: https://github.com/${T3_PIN.repository}$`, "m"));
  assert.match(source, new RegExp(`^- Tag: ${T3_PIN.releaseTag.replaceAll(".", "\\.")}$`, "m"));
  assert.match(source, new RegExp(`^- Commit: ${T3_PIN.commit}$`, "m"));
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
    // start without --wait: the launch result, decoded with the CLI's own struct.
    `const ThreadLaunchResult = Schema.Struct({\n  threadId: ThreadId,\n  projectId: ProjectId,\n  runId: Schema.NullOr(RunId),\n});`,
    `yield* flags.json ? printJson(launched) : Console.log(launched.threadId);`,
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

const literalsOf = (schema: unknown): string[] =>
  (schema as { members?: ReadonlyArray<unknown>; literal?: string; literals?: readonly string[] })
    .members?.flatMap(literalsOf) ??
  (schema as { literals?: readonly string[] }).literals?.slice() ??
  ((schema as { literal?: string }).literal === undefined ? [] : [(schema as { literal: string }).literal]);

test("every pull request source upstream can report is one the parser accepts", () => {
  const upstream = literalsOf(PullRequestSource);
  assert.ok(upstream.length > 0, "could not read the upstream pull request sources");
  for (const source of upstream) {
    assert.ok((T3_PULL_REQUEST_SOURCES as readonly string[]).includes(source), `unknown upstream source ${source}`);
  }
});

test("the real captures are valid under the upstream schemas", () => {
  for (const name of ["start.json", "send.json"]) {
    decode(RunResult, JSON.parse(fixture(name)));
  }
  decode(ThreadShow, JSON.parse(fixture("show.json")));
  decode(LaunchResult, JSON.parse(fixture("launch.json")));
});

test("the launch-result parser reads every value upstream can print", async () => {
  for (const value of await printed(LaunchResult)) {
    assert.deepEqual(parseLaunchResult(JSON.stringify(value)), value);
  }
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
          source: pr.source,
          state: pr.state,
        })),
      },
      messages: value.messages,
    });
  }
});
