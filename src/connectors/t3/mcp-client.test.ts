import assert from "node:assert/strict";
import { once } from "node:events";
import { createServer } from "node:http";
import type { ServerResponse } from "node:http";
import { test } from "node:test";
import type { TestContext } from "node:test";
import { T3AuthError, T3JsonRpcError, T3McpClient, T3ReadOnlyPolicyError, T3ToolError } from "./mcp-client.ts";
import type {
  T3EnvironmentRead,
  T3EventsStreamNotification,
  T3MilestoneEvent,
  T3ReadToolResults,
  T3StreamItem,
  T3TargetConfig,
} from "./types.ts";

interface MockRequest {
  jsonrpc: string;
  id: number;
  method: string;
  params: { name?: string; arguments?: unknown; _meta?: unknown; [key: string]: unknown };
}

async function mockMcp(
  t: TestContext,
  send: (rpc: MockRequest, response: ServerResponse) => void,
  accessToken = "mock-token",
) {
  const requests: MockRequest[] = [];
  const server = createServer(async (request, response) => {
    try {
      const chunks: Buffer[] = [];
      for await (const chunk of request) chunks.push(Buffer.from(chunk));
      const rpc = JSON.parse(Buffer.concat(chunks).toString()) as MockRequest;
      assert.equal(request.url, "/mcp");
      assert.equal(request.method, "POST");
      assert.equal(request.headers.authorization, `Bearer ${accessToken}`);
      assert.equal(request.headers["content-type"], "application/json");
      assert.equal(request.headers.accept, "application/json, text/event-stream");
      assert.equal(request.headers.origin, undefined);
      assert.equal(request.headers["mcp-session-id"], undefined);
      assert.equal(request.headers["mcp-protocol-version"], "2026-07-28");
      assert.equal(request.headers["mcp-method"], rpc.method);
      assert.equal(request.headers["mcp-name"], rpc.params.name);
      assert.equal(rpc.jsonrpc, "2.0");
      assert.deepEqual(rpc.params._meta, {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { name: "ostrom", version: "0.1.0" },
      });
      requests.push(rpc);
      send(rpc, response);
    } catch (error) {
      t.diagnostic(String(error));
      response.destroy();
    }
  });
  t.after(() => {
    server.close();
    server.closeAllConnections();
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert(address && typeof address === "object");
  const target: T3TargetConfig = {
    id: "test-host",
    label: "Test",
    mcpUrl: `http://127.0.0.1:${address.port}/mcp`,
    originNamespace: "test-origin",
  };
  return { client: new T3McpClient(target, accessToken), requests };
}

function jsonReply(response: ServerResponse, id: number, result: unknown) {
  response.writeHead(200, { "content-type": "application/json" });
  response.end(JSON.stringify({ jsonrpc: "2.0", id, result }));
}

function sseFrame(message: unknown): string {
  return `event: message\r\ndata: ${JSON.stringify(message)}\r\n\r\n`;
}

const environment = {
  environmentId: "environment-1",
  label: "Mock T3",
  serverVersion: "0.1.13",
  platform: { os: "linux", arch: "x64" },
  preferences: {
    newWorktreesStartFromOrigin: true,
    enableProviderUpdateChecks: true,
    backgroundActivity: { profile: "balanced" },
    sourceControlWritingStyle: {
      mode: "repo_conventions", followChangeRequestTemplates: false, customInstructions: "", truncated: false,
    },
  },
} satisfies T3EnvironmentRead;

const time = "2026-11-27T00:00:00.000Z";

const threadDetail = {
  threadId: "thread-1",
  projectId: "project-1",
  title: "Test thread",
  createdBy: "user",
  creationSource: "web",
  status: "completed",
  latestRunId: "run-1",
  providerInstanceId: "provider-1",
  model: "test-model",
  runtimeMode: "approval-required",
  interactionMode: "default",
  linkedPullRequest: null,
  settled: true,
  settledAt: time,
  snoozed: false,
  snoozedUntil: null,
  parentThreadId: null,
  relationshipToParent: null,
  itemCount: 0,
  createdAt: time,
  updatedAt: time,
  activeRunId: null,
  titleRegeneration: null,
  branch: "feat/example",
  worktreePath: null,
  runCount: 1,
  pendingRequestCount: 1,
  archived: false,
} satisfies T3ReadToolResults["t3_thread_read"]["thread"];

const readResults = {
  t3_environment_read: environment,
  t3_project_list: {
    projects: [{
      id: "project-1", title: "Test project", workspaceRoot: "/mock", defaultModelSelection: null,
      scripts: [], createdAt: time, updatedAt: time, deletedAt: null,
    }],
    nextCursor: null,
  },
  t3_thread_list: {
    projectId: null, currentThreadId: null, threads: [], nextCursor: null, total: 0,
  },
  t3_thread_read: {
    thread: threadDetail,
    recentRuns: [{
      runId: "run-1", ordinal: 1, status: "completed", providerInstanceId: "provider-1",
      model: "test-model", requestedAt: time, startedAt: time, completedAt: time,
    }],
    items: [],
    nextPosition: null,
    hasMore: false,
  },
  t3_pending_request: { requestIds: ["request-1"] },
  list_thread_pull_requests: {
    pullRequests: [{
      host: "forge.example", repository: "org/repo", number: 1, url: "https://forge.example/org/repo/pull/1",
      source: "manual", state: "open", watching: true, title: "Example PR", headBranch: "feat/example",
      baseBranch: "main", isDraft: false, stack: null,
    }],
    chains: [],
    nextCursor: null,
  },
} satisfies T3ReadToolResults;

test("negotiates stateless 2026-07-28 and calls the six read-only tools over JSON and SSE", async (t) => {
  const question = {
    requestId: "request-1",
    questions: [{ id: "question-1", header: "Question", question: "Continue?", options: [] }],
  };
  const { client, requests } = await mockMcp(t, (rpc, response) => {
    if (rpc.method === "server/discover") {
      jsonReply(response, rpc.id, { supportedVersions: ["2025-06-18", "2026-07-28"], capabilities: {} });
      return;
    }
    assert.equal(rpc.method, "tools/call");
    assert(rpc.params.name && Object.hasOwn(readResults, rpc.params.name));
    const name = rpc.params.name as keyof typeof readResults;
    const args = rpc.params.arguments;
    if (name === "t3_environment_read") {
      response.writeHead(200, { "content-type": "text/event-stream; charset=utf-8" });
      response.write(": comment\r\n\r\n");
      response.end(sseFrame({ jsonrpc: "2.0", id: rpc.id, result: {
        content: [{ type: "text", text: "ignored in favor of structured content" }],
        structuredContent: environment,
      } }));
      return;
    }
    const result = name === "t3_pending_request" && (args as { action?: string }).action === "read"
      ? question : readResults[name];
    jsonReply(response, rpc.id, { content: [{ type: "text", text: JSON.stringify(result) }], resultType: "complete" });
  });

  assert((await client.initialize()).supportedVersions.includes("2026-07-28"));
  assert.deepEqual(await client.readEnvironment(), environment);
  assert.deepEqual(await client.listProjects({ limit: 10 }), readResults.t3_project_list);
  assert.deepEqual(await client.listThreads({ allProjects: true }), readResults.t3_thread_list);
  assert.deepEqual(await client.readThread({ threadId: "thread-1", view: "messages" }), readResults.t3_thread_read);
  assert.deepEqual(await client.pendingRequest({ threadId: "thread-1", action: "list" }), readResults.t3_pending_request);
  assert.deepEqual(await client.pendingRequest({ threadId: "thread-1", action: "read", requestId: "request-1" }), question);
  assert.deepEqual(await client.listThreadPullRequests({ threadId: "thread-1" }), readResults.list_thread_pull_requests);
  assert.deepEqual(requests.slice(1).map(({ params }) => [params.name, params.arguments]), [
    ["t3_environment_read", {}],
    ["t3_project_list", { limit: 10 }],
    ["t3_thread_list", { allProjects: true }],
    ["t3_thread_read", { threadId: "thread-1", view: "messages" }],
    ["t3_pending_request", { threadId: "thread-1", action: "list" }],
    ["t3_pending_request", { threadId: "thread-1", action: "read", requestId: "request-1" }],
    ["list_thread_pull_requests", { threadId: "thread-1" }],
  ]);
});

test("rejects disallowed tools and pending request actions before HTTP", async (t) => {
  const { client, requests } = await mockMcp(t, (rpc, response) => jsonReply(response, rpc.id, {}));
  // @ts-expect-error Mutating tools are outside the typed allowlist.
  await assert.rejects(client.callTool("t3_thread_send", {}), T3ReadOnlyPolicyError);
  // @ts-expect-error Forge PR reads are outside the typed allowlist.
  await assert.rejects(client.callTool("pull_request_read", {}), T3ReadOnlyPolicyError);
  // @ts-expect-error Responding to pending questions is outside the typed allowlist.
  await assert.rejects(client.callTool("t3_pending_request", { threadId: "thread-1", action: "respond" }), T3ReadOnlyPolicyError);
  // @ts-expect-error Inherited object properties are outside the typed allowlist.
  await assert.rejects(client.callTool("toString", {}), T3ReadOnlyPolicyError);
  assert.deepEqual(requests, []);
});

test("preserves JSON-RPC errors and rejects MCP tool errors", async (t) => {
  const { client } = await mockMcp(t, (rpc, response) => {
    if (rpc.params.name === "t3_project_list") {
      response.writeHead(400, { "content-type": "application/json" });
      response.end(JSON.stringify({
        jsonrpc: "2.0",
        id: rpc.id,
        error: { code: -32602, message: "Invalid params", data: { parameter: "limit" } },
      }));
    } else {
      jsonReply(response, rpc.id, {
        content: [{ type: "text", text: "No such thread" }],
        structuredContent: { reason: "missing" },
        isError: true,
      });
    }
  });
  await assert.rejects(client.listProjects(), (error: unknown) => {
    assert(error instanceof T3JsonRpcError);
    assert.equal(error.code, -32602);
    assert.deepEqual(error.data, { parameter: "limit" });
    return true;
  });
  await assert.rejects(client.readThread({ threadId: "missing" }), {
    name: T3ToolError.name, message: "No such thread", tool: "t3_thread_read",
  });
});

const milestone = {
  type: "thread.run_settled",
  id: "milestone-1",
  occurredAt: time,
  projectId: "project-1",
  threadId: "thread-1",
  parentThreadId: null,
  actor: { type: "system" },
  isSubagentChild: false,
  runId: "run-1",
  outcome: "completed",
} satisfies T3MilestoneEvent;

const occurrence = { name: milestone.type, eventId: "evt_mock", timestamp: time, data: milestone };

test("polls a milestone with pagination and truncation information", async (t) => {
  const result = { events: [occurrence], cursor: "server.50", truncated: true, hasMore: true };
  const { client, requests } = await mockMcp(t, (rpc, response) => {
    assert.equal(rpc.method, "events/poll");
    jsonReply(response, rpc.id, rpc.params.cursor === "old.1" ? { ...result, nextPollMs: 1_000 } : result);
  });
  const args = { threadIds: ["thread-1"] };
  const options = { cursor: "old.1", maxEvents: 1, maxAgeMs: 5_000 };
  const actual = await client.pollEvents(milestone.type, args, options);
  assert.deepEqual(actual, { ...result, nextPollMs: 1_000 });
  assert.deepEqual(await client.pollEvents(milestone.type), result);
  assert.equal(actual.events[0]?.data.runId, "run-1");
  assert.deepEqual(requests[0]?.params.arguments, args);
  assert.equal(requests[0]?.params.cursor, options.cursor);
  assert.equal(requests[0]?.params.maxEvents, options.maxEvents);
  assert.equal(requests[0]?.params.maxAgeMs, options.maxAgeMs);
});

function active(cursor: string | null, truncated: boolean): T3EventsStreamNotification {
  return { jsonrpc: "2.0", method: "notifications/events/active", params: { cursor, truncated, heartbeatMs: 1_000 } };
}

function heartbeat(cursor: string | null): T3EventsStreamNotification {
  return { jsonrpc: "2.0", method: "notifications/events/heartbeat", params: { cursor } };
}

test("streams SEP-3415 occurrences and reports a gap from truncated activation", async (t) => {
  const notification: T3EventsStreamNotification = {
    jsonrpc: "2.0", method: "notifications/events/event", params: { ...occurrence, cursor: "server.50" },
  };
  const { client, requests } = await mockMcp(t, (rpc, response) => {
    response.writeHead(200, { "content-type": "text/event-stream" });
    response.write(sseFrame(active("server.49", false)));
    const event = sseFrame(notification);
    response.write(event.slice(0, -1));
    response.write(event.slice(-1));
    response.write(sseFrame(active("server.51", true)));
    response.write(sseFrame(heartbeat("server.52")));
    response.end(sseFrame({ jsonrpc: "2.0", id: rpc.id, result: { resultType: "complete" } }));
  });

  const items: T3StreamItem<typeof milestone.type>[] = [];
  for await (const item of client.streamEvents(milestone.type, {}, { cursor: "server.48", heartbeatMs: 1_000 })) {
    items.push(item);
  }
  assert.deepEqual(items, [
    { kind: "activated", cursor: "server.49", truncated: false },
    { kind: "event", cursor: "server.50", eventId: occurrence.eventId, timestamp: occurrence.timestamp, data: milestone },
    { kind: "gap", cursor: "server.51" },
    { kind: "heartbeat", cursor: "server.52" },
  ]);
  assert.equal(requests[0]?.method, "events/stream");
  assert.equal(requests[0]?.params.cursor, "server.48");
  assert.equal(requests[0]?.params.heartbeatMs, 1_000);
});

test("reconnects from the last gap or heartbeat cursor even without a matching event", async (t) => {
  const { client, requests } = await mockMcp(t, (rpc, response) => {
    response.writeHead(200, { "content-type": "text/event-stream" });
    switch (rpc.params.cursor) {
      case null:
        response.write(sseFrame(active("server.1", false)));
        response.write(sseFrame(active("server.2", true)));
        break;
      case "server.2":
        response.write(sseFrame(active("server.2", false)));
        response.write(sseFrame(heartbeat("server.3")));
        break;
      case "server.3":
        response.write(sseFrame(active("server.4", false)));
        break;
      default:
        assert.fail("Reconnect used a stale cursor");
    }
    response.end();
  });

  let cursor: string | null = null;
  for (const expectedCursor of ["server.2", "server.3", "server.4"]) {
    for await (const item of client.streamEvents(milestone.type, { threadIds: ["thread-1"] }, { cursor })) {
      assert.notEqual(item.kind, "event");
      cursor = item.cursor;
    }
    assert.equal(cursor, expectedCursor);
  }
  assert.deepEqual(requests.map(({ params }) => params.cursor), [null, "server.2", "server.3"]);
});

test("stream rejects denied access before and after HTTP headers arrive", async (t) => {
  for (const status of [401, 403]) {
    const { client } = await mockMcp(t, (_rpc, response) => response.writeHead(status).end());
    await assert.rejects(client.streamEvents(milestone.type).next(), { name: T3AuthError.name, status });
  }
  const { client } = await mockMcp(t, (_rpc, response) => {
    response.writeHead(200, { "content-type": "text/event-stream" });
    response.end(sseFrame({
      jsonrpc: "2.0",
      method: "notifications/events/terminated",
      params: { error: { code: -32003, message: "Access revoked", data: { reason: "Access revoked" } } },
    } satisfies T3EventsStreamNotification));
  });
  await assert.rejects(client.streamEvents(milestone.type).next(), { name: T3AuthError.name, message: "Access revoked" });
});

test("aborting a stream closes the response while awaiting an event", async (t) => {
  const closed = Promise.withResolvers<void>();
  const { client } = await mockMcp(t, (_rpc, response) => {
    response.on("close", () => closed.resolve());
    response.writeHead(200, { "content-type": "text/event-stream" });
    response.write(sseFrame({
      jsonrpc: "2.0", method: "notifications/events/active",
      params: { cursor: "server.1", truncated: true, heartbeatMs: 30_000 },
    } satisfies T3EventsStreamNotification));
  });
  const controller = new AbortController();
  const stream = client.streamEvents(milestone.type, {}, { signal: controller.signal });
  assert.deepEqual(await stream.next(), {
    value: { kind: "activated", cursor: "server.1", truncated: true }, done: false,
  });
  assert.deepEqual(await stream.next(), { value: { kind: "gap", cursor: "server.1" }, done: false });
  const pending = stream.next();
  controller.abort();
  await assert.rejects(pending, { name: "AbortError" });
  await closed.promise;
});
