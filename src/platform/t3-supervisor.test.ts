import assert from "node:assert/strict";
import { once } from "node:events";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import type { ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { loadToken, saveToken, T3McpClient } from "../connectors/t3/index.ts";
import type { T3MilestoneEvent, T3MilestoneName } from "../connectors/t3/index.ts";
import { runPlatformCli } from "./cli.ts";
import { loadPlatformHosts } from "./config.ts";
import { evaluatePolicyAndHealth } from "./policy-observer.ts";
import { PlatformStore } from "./store.ts";
import { reconcileT3HostOnce, T3HostSupervisor } from "./t3-supervisor.ts";

const time = "2026-11-27T01:00:00.000Z";
const later = "2026-11-27T01:01:00.000Z";
const prKey = { host: "forge.example", repository: "org/repo", number: 17 };

type Rpc = { id: number; method: string; params: {
  name: string; cursor?: string; arguments: { action?: string; requestId?: string; cursor?: number };
} };
type FixtureRun = { runId: string; status: string; startedAt: string; completedAt: string; model: string };

function frame(response: ServerResponse, name: string, params: unknown): void {
  const message = { jsonrpc: "2.0", method: "notifications/events/" + name, params };
  response.write("data: " + JSON.stringify(message) + "\n\n");
}

async function mockHost(id: string, options: { deny?: number; polling?: boolean } = {}) {
  let epoch = 1;
  let activity = 0;
  let environmentReads = 0;
  let listPages = 0;
  let requests = 0;
  let threadReads = 0;
  let settledRuns: FixtureRun[] | undefined;
  let threadReadFailure: "structured" | "text" | "transient" | undefined;
  let unboundedList = false;
  let nextEnvironment: Promise<void> | undefined;
  let runStatus = "waiting";
  let watching = true;
  const pending = new Map<string, "user_input" | "approval">([["initial", "user_input"]]);
  const streams = new Map<string, ServerResponse>();
  const retained: { event: T3MilestoneEvent; position: number }[] = [];
  const streamOpens: { cursor: string | undefined; name: string }[] = [];
  const polls: { cursor: string | undefined; at: number; name: string }[] = [];
  const cursor = () => "epoch" + epoch + "." + activity;
  const truncated = (value?: string) => value != null && !value.startsWith("epoch" + epoch + ".");
  const server = createServer(async (request, response) => {
    try {
      assert.equal(request.url, "/mcp");
      assert.equal(request.headers.authorization, "Bearer fixture");
      let input = "";
      for await (const chunk of request.setEncoding("utf8")) input += chunk;
      const rpc = JSON.parse(input) as Rpc;
      const { name, arguments: args } = rpc.params;
      requests++;
      if (options.deny !== undefined) {
        response.writeHead(options.deny).end("Access revoked");
        return;
      }
      if (rpc.method === "events/stream") {
        if (options.polling) {
          response.writeHead(405).end();
          return;
        }
        response.writeHead(200, { "content-type": "text/event-stream" });
        streams.set(name, response);
        streamOpens.push({ cursor: rpc.params.cursor, name });
        response.once("close", () => { if (streams.get(name) === response) streams.delete(name); });
        const lost = truncated(rpc.params.cursor);
        const from = Number(rpc.params.cursor?.split(".")[1]);
        const replay = lost ? [] : retained.filter(({ event, position }) => event.type === name && position > from);
        frame(response, "active", { cursor: replay.length ? rpc.params.cursor : cursor(), truncated: lost, heartbeatMs: 15_000 });
        for (const { event, position } of replay) {
          frame(response, "event", { eventId: event.id, name, timestamp: event.occurredAt,
            cursor: "epoch" + epoch + "." + position, data: event });
        }
        return;
      }
      let result: unknown;
      if (rpc.method === "events/poll") {
        polls.push({ cursor: rpc.params.cursor, at: performance.now(), name });
        result = { events: [], cursor: cursor(), truncated: truncated(rpc.params.cursor), hasMore: false, nextPollMs: 200 };
      } else {
        let data: unknown;
        const thread = { threadId: "thread", title: id, status: settledRuns?.[0]?.status ?? runStatus,
          settled: settledRuns !== undefined, latestRunId: settledRuns?.[0]?.runId ?? "run",
          relationshipToParent: null, updatedAt: later, activeRunId: settledRuns || runStatus === "completed" ? null : "run",
          pendingRequestCount: pending.size };
        switch (name) {
          case "t3_environment_read":
            environmentReads++;
            await nextEnvironment;
            data = { environmentId: "env-" + id, serverVersion: "test" };
            break;
          case "t3_thread_list":
            listPages++;
            data = unboundedList ? { threads: [], nextCursor: (args.cursor ?? 0) + 1 }
              : args.cursor === undefined ? { threads: [], nextCursor: 1 } : { threads: [thread], nextCursor: null };
            break;
          case "t3_thread_read":
            threadReads++;
            if (threadReadFailure !== undefined) {
              if (threadReadFailure === "transient") {
                response.writeHead(503, { "content-type": "application/json" }).end(JSON.stringify({
                  jsonrpc: "2.0", id: rpc.id, error: { code: -32000, data: { code: "thread_not_found" }, message: "Gone" },
                }));
                return;
              }
              const result = threadReadFailure === "text"
                ? { isError: true, content: [{ type: "text", text: "Thread thread is no longer available." }] }
                : { isError: true, content: [], structuredContent: { code: "thread_not_found", message: "Gone" } };
              response.writeHead(200, { "content-type": "application/json" })
                .end(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result }));
              return;
            }
            data = { thread, recentRuns: settledRuns ?? [{ runId: "run", status: runStatus, startedAt: time,
              completedAt: runStatus === "completed" ? later : null, model: "test" }] };
            break;
          case "t3_pending_request":
            data = args.action === "list" ? { requestIds: [...pending].filter(([, kind]) => kind === "user_input").map(([key]) => key) }
              : { requestId: args.requestId, questions: [{ question: "Continue?" }] };
            break;
          case "list_thread_pull_requests":
            data = { pullRequests: [{ ...prKey, url: "https://forge.example/org/repo/pull/17", source: "manual", state: "open",
              watching, title: "Fix", headBranch: "fix", baseBranch: "main", isDraft: false }], nextCursor: null };
            break;
          default:
            throw new Error("unexpected mock tool " + name);
        }
        result = { structuredContent: data, content: [] };
      }
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result }));
    } catch (error) {
      response.writeHead(500).end(String(error));
    }
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert(address && typeof address === "object");

  function stream(name: T3MilestoneName): ServerResponse {
    const response = streams.get(name);
    assert(response, "missing stream " + name);
    return response;
  }

  return {
    host: { id, label: id, mcpUrl: "http://127.0.0.1:" + address.port + "/mcp" },
    get streamCount() { return streams.size; },
    get environmentReads() { return environmentReads; },
    get listPages() { return listPages; },
    get threadReads() { return threadReads; },
    get requests() { return requests; },
    polls,
    streamOpens,
    pending,
    setStatus: (status: string) => { runStatus = status; },
    setSettledRuns: (runs: FixtureRun[]) => {
      settledRuns = runs;
      pending.clear();
      watching = false;
    },
    failThreadRead: (failure: typeof threadReadFailure) => { threadReadFailure = failure; },
    setUnboundedList: (value: boolean) => { unboundedList = value; },
    pauseEnvironment: (promise: Promise<void>) => { nextEnvironment = promise; },
    rotate: () => { epoch++; activity = 0; retained.length = 0; },
    heartbeat: () => { frame(stream("thread.run_settled"), "heartbeat", { cursor: cursor() }); },
    disconnect: () => { for (const response of streams.values()) response.end(); },
    revoke: () => { frame(stream("thread.run_settled"), "error", { error: { code: -32000, message: "Access revoked" } }); },
    gap: () => { frame(stream("thread.run_settled"), "active", { cursor: cursor(), truncated: true, heartbeatMs: 15_000 }); },
    retain(event: T3MilestoneEvent) { retained.push({ event, position: ++activity }); },
    emit(event: T3MilestoneEvent) {
      activity++;
      if (event.type === "pr.watch_wake" && event.watchEnded) watching = false;
      frame(stream(event.type), "event", { eventId: event.id, name: event.type, timestamp: event.occurredAt,
        cursor: cursor(), data: event });
    },
    async close() {
      const closed = once(server, "close");
      server.close();
      server.closeAllConnections();
      await closed;
    },
  };
}

async function eventually(condition: () => boolean | undefined) {
  const deadline = performance.now() + 6_000;
  while (!condition()) {
    if (performance.now() >= deadline) assert.fail("mock T3 state did not converge");
    await delay(10);
  }
}

function envelope(id: string) {
  return { id, occurredAt: later, projectId: null, threadId: "thread", parentThreadId: null,
    actor: { type: "agent" as const }, isSubagentChild: false };
}

test("reconciles separate hosts, projects live milestones, and rebuilds after both gap signals", async () => {
  const [east, west] = await Promise.all([mockHost("east"), mockHost("west")]);
  const directory = await mkdtemp(join(tmpdir(), "ostrom-t3-"));
  const store = PlatformStore.open(":memory:");
  let supervisor: T3HostSupervisor | undefined;
  const hold = Promise.withResolvers<void>();
  try {
    for (const mock of [east, west]) await saveToken(mock.host.id, { accessToken: "fixture", expiresAt: Date.now() + 60_000 }, directory);
    west.pending.set("approval-startup", "approval");
    supervisor = new T3HostSupervisor(store, [{ ...east.host, webUrl: "https://t3-east.example" }, west.host],
      { tokensDir: directory, retryMs: 10 });
    await eventually(() => east.streamCount === 4 && west.streamCount === 4
      && store.listT3HostStates().every((host) => host.connectionState === "connected"));
    assert.deepEqual(store.listAgents().map((agent) => agent.externalRef), ["env-east/thread", "env-west/thread"]);
    assert.deepEqual(store.listT3HostStates().map((host) => host.webUrl), ["https://t3-east.example", null]);
    assert.deepEqual(store.listRuns().map((run) => run.status), ["waiting_input", "waiting_approval"]);
    assert.equal(store.listOpenT3Waits().length, 3);
    assert.equal(store.listT3ThreadPrs().length, 2);
    assert.equal(east.listPages, 2);
    for (const item of [store.listT3HostStates()[0], store.listOpenT3Waits()[0], store.listT3ThreadPrs()[0]]) {
      assert(item);
      assert.equal(Object.getPrototypeOf(item), Object.prototype);
    }
    const run = store.listRuns().find((item) => item.hostId === "east");
    assert(run);
    store.linkRunSemonSession(run.id, "semon-exact");

    east.pending.set("input", "user_input");
    east.emit({ ...envelope("needs"), type: "thread.needs_input", kind: "user_input", requestId: "input" });
    await eventually(() => store.listOpenT3Waits("east").some((wait) => wait.requestId === "input"));
    assert.deepEqual(store.listOpenT3Waits("east").find((wait) => wait.requestId === "input")?.questions, [{ question: "Continue?" }]);
    east.pending.set("approval", "approval");
    east.emit({ ...envelope("approval"), type: "thread.needs_input", kind: "approval", requestId: "approval" });
    await eventually(() => store.listRuns().find((item) => item.id === run.id)?.status === "waiting_approval");
    assert.equal(store.listOpenT3Waits("east").find((wait) => wait.requestId === "approval")?.kind, "approval");

    east.pending.delete("input");
    east.emit({ ...envelope("resolved"), type: "thread.input_resolved", kind: "user_input", requestId: "input", resolution: "resolved" });
    await eventually(() => !store.listOpenT3Waits("east").some((wait) => wait.requestId === "input"));
    east.pending.clear();
    east.setStatus("completed");
    east.emit({ ...envelope("settled"), type: "thread.run_settled", runId: "run", outcome: "completed" });
    await eventually(() => store.listRuns().find((item) => item.id === run.id)?.status === "completed");
    east.emit({ ...envelope("woke"), type: "pr.watch_wake", pullRequests: [prKey],
      outcome: "completed", summary: "Checks passed", watchEnded: true });
    await eventually(() => store.listT3ThreadPrs("east").some((pr) => pr.lastWakeSummary === "Checks passed"));
    assert.equal(store.listT3ThreadPrs("east")[0]?.watching, false);
    east.heartbeat();
    await eventually(() => store.getT3Cursor("east", "thread.run_settled") === "epoch1.5");

    // A restarted host rejects the old cursor after streaming falls back to polling.
    east.rotate();
    east.pending.set("recovered", "user_input");
    east.setStatus("waiting");
    east.disconnect();
    await eventually(() => east.polls.some((poll) => poll.cursor?.startsWith("epoch1."))
      && (["thread.run_settled", "thread.needs_input", "thread.input_resolved", "pr.watch_wake"] as const)
        .every((name) => store.getT3Cursor("east", name) === "epoch2.0"));
    assert(east.environmentReads >= 3);
    assert.deepEqual(store.listOpenT3Waits("east").map((wait) => wait.requestId), ["recovered"]);
    assert.equal(store.listRuns().find((item) => item.id === run.id)?.semonSessionUid, "semon-exact");
    assert.equal(store.listT3ThreadPrs("east")[0]?.lastWakeSummary, "Checks passed");

    west.pauseEnvironment(hold.promise);
    const unaffectedCursor = store.getT3Cursor("west", "pr.watch_wake");
    assert.equal(unaffectedCursor, "epoch1.0");
    west.retain({ ...envelope("retained-wake"), type: "pr.watch_wake", pullRequests: [prKey],
      outcome: "completed", summary: "Retained after the gap", watchEnded: false });
    west.gap();
    await eventually(() => west.environmentReads === 2
      && store.listT3HostStates().find((host) => host.hostId === "west")?.connectionState === "stale");
    hold.resolve();
    await eventually(() => west.streamCount === 4 && store.getT3Cursor("west", "thread.run_settled") === "epoch1.1"
      && store.listT3ThreadPrs("west")[0]?.lastWakeSummary === "Retained after the gap");
    assert.deepEqual(west.streamOpens.filter(({ name }) => name === "pr.watch_wake").map(({ cursor }) => cursor),
      [unaffectedCursor, unaffectedCursor]);
    assert(store.listT3HostStates().every((host) => host.connectionState === "connected" && host.lastError === null));
  } finally {
    hold.resolve();
    await supervisor?.close();
    store.close();
    await Promise.all([east.close(), west.close(), rm(directory, { recursive: true, force: true })]);
  }
});

test("reconciles newly settled runs after downtime and reports capped coverage", async () => {
  const mock = await mockHost("outage");
  const store = PlatformStore.open(":memory:");
  try {
    const client = new T3McpClient(mock.host, "fixture");
    const run = (runId: string, status: string, minute: string) => ({
      runId, status, startedAt: "2026-11-27T01:" + minute + ":00.000Z",
      completedAt: "2026-11-27T01:" + minute + ":01.000Z", model: "test",
    });
    const first = run("initial-success", "completed", "01");
    mock.setSettledRuns([first]);
    const initial = await reconcileT3HostOnce(client, store, mock.host);
    assert.equal(initial.partialCoverage, false);
    assert.deepEqual(store.listRuns().map((item) => item.externalRunRef), [first.runId]);
    const [failedFirst, failedSecond] = [run("failed-first", "failed", "05"), run("failed-second", "failed", "07")];
    mock.setSettledRuns([failedSecond, failedFirst, first]);
    await reconcileT3HostOnce(client, store, mock.host);
    assert.equal(mock.threadReads, 2);
    assert.equal(store.listRuns().filter((item) => item.status === "failed").length, 2);
    const { events } = evaluatePolicyAndHealth(store, { now: later });
    assert.deepEqual(events.map((event) => event.ruleCode), ["two_failure_stop"]);
    await reconcileT3HostOnce(client, store, mock.host);
    assert.equal(mock.threadReads, 2, "unchanged settled threads do not need another detail read");

    mock.setUnboundedList(true);
    assert.equal((await reconcileT3HostOnce(client, store, mock.host)).partialCoverage, true);
    assert.equal(store.listT3HostStates()[0]?.partialCoverage, true);
    assert.equal(store.listAgents()[0]?.status, "idle", "an incomplete listing cannot prove removal");
    mock.setUnboundedList(false);
    assert.equal((await reconcileT3HostOnce(client, store, mock.host)).partialCoverage, false);
  } finally {
    store.close();
    await mock.close();
  }
});

for (const failure of ["structured", "text", "transient"] as const) {
  test(`handles ${failure} thread reads on an earlier event`, async () => {
    const mock = await mockHost("deleted");
    const directory = await mkdtemp(join(tmpdir(), "ostrom-t3-deleted-"));
    const store = PlatformStore.open(":memory:");
    let supervisor: T3HostSupervisor | undefined;
    const hold = Promise.withResolvers<void>();
    try {
      await saveToken(mock.host.id, { accessToken: "fixture", expiresAt: Date.now() + 60_000 }, directory);
      supervisor = new T3HostSupervisor(store, [mock.host], { tokensDir: directory, retryMs: 10 });
      await eventually(() => mock.streamCount === 4);
      assert.equal(store.getT3Cursor(mock.host.id, "thread.run_settled"), "epoch1.0");
      mock.failThreadRead(failure);
      mock.emit({ ...envelope("retained-settlement"), type: "thread.run_settled", runId: "run", outcome: "completed" });
      if (failure === "transient") {
        mock.pauseEnvironment(hold.promise);
        await eventually(() => store.listT3HostStates()[0]?.connectionState === "stale" && mock.environmentReads >= 2);
        assert.equal(store.getT3Cursor(mock.host.id, "thread.run_settled"), "epoch1.0");
        assert.equal(store.listAgents()[0]?.status, "waiting");
      } else {
        await eventually(() => store.getT3Cursor(mock.host.id, "thread.run_settled") === "epoch1.1");
        assert.equal(store.listAgents()[0]?.status, "offline");
        assert.equal(store.listRuns()[0]?.status, "interrupted");
        assert.deepEqual(store.listOpenT3Waits(), []);
        assert.equal(store.listT3ThreadPrs()[0]?.watching, false);
        assert.equal(store.listT3HostStates()[0]?.connectionState, "connected");
        assert.equal(mock.environmentReads, 1, "a missing thread does not reconnect the host");
      }
    } finally {
      await supervisor?.close();
      hold.resolve();
      store.close();
      await Promise.all([mock.close(), rm(directory, { recursive: true, force: true })]);
    }
  });
}

test("stops requests on 401, 403, and revocation, and honors the server polling interval", async () => {
  const mocks = await Promise.all([
    mockHost("unauthorized", { deny: 401 }), mockHost("forbidden", { deny: 403 }),
    mockHost("revoked"), mockHost("polling", { polling: true }),
  ] as const);
  const [unauthorized, forbidden, revoked, polling] = mocks;
  const directory = await mkdtemp(join(tmpdir(), "ostrom-t3-"));
  const store = PlatformStore.open(":memory:");
  let supervisor: T3HostSupervisor | undefined;
  try {
    for (const mock of mocks) await saveToken(mock.host.id, { accessToken: "fixture", expiresAt: Date.now() + 60_000 }, directory);
    await saveToken("expired", { accessToken: "fixture", expiresAt: Date.now() - 1 }, directory);
    supervisor = new T3HostSupervisor(store, [...mocks.map((mock) => mock.host),
      { ...polling.host, id: "missing" }, { ...polling.host, id: "expired" }],
      { tokensDir: directory, retryMs: 10 });
    await eventually(() => revoked.streamCount === 4
      && store.listT3HostStates().filter((host) => host.connectionState === "auth_expired").length === 4);
    revoked.revoke();
    await eventually(() => store.listT3HostStates().filter((host) => host.connectionState === "auth_expired").length === 5);
    assert.equal(unauthorized.requests, 1);
    assert.equal(forbidden.requests, 1);
    const count = revoked.requests;
    const realPolls = () => polling.polls.filter(({ cursor, name }) => cursor !== undefined && name === "thread.run_settled");
    await eventually(() => realPolls().length >= 2);
    assert(realPolls()[1]!.at - realPolls()[0]!.at >= 180);
    assert.equal(revoked.requests, count);
    assert.equal(store.listT3HostStates().find((host) => host.hostId === "polling")?.connectionState, "connected");
  } finally {
    await supervisor?.close();
    store.close();
    await Promise.all([...mocks.map((mock) => mock.close()), rm(directory, { recursive: true, force: true })]);
  }
});

test("validates platform host identities and transport", async () => {
  const directory = await mkdtemp(join(tmpdir(), "ostrom-t3-config-"));
  try {
    const path = join(directory, "hosts.json");
    const host = { id: "east", label: "East", mcpUrl: "https://t3.example/mcp", originNamespace: "test", laneSlug: "release" };
    const load = async (...hosts: unknown[]) => {
      await writeFile(path, JSON.stringify({ hosts }));
      return loadPlatformHosts(path);
    };
    assert.deepEqual(await load(host), [host]);
    for (const id of [".", "..", "foo/bar", "x".repeat(65)]) await assert.rejects(load({ ...host, id }), /IDs/);
    await assert.rejects(load(host, host), /IDs/);
    for (const mcpUrl of ["http://remote.example/mcp", "ftp://localhost/mcp", "https://user:pass@t3.example/mcp"]) {
      await assert.rejects(load({ ...host, mcpUrl }), /mcpUrl/);
    }
    assert.equal((await load({ ...host, mcpUrl: "http://localhost:8000/mcp" }))[0]?.id, host.id);
    const webHost = { ...host, mcpUrl: "http://127.0.0.1:8000/mcp", webUrl: "https://t3-east.example/" };
    assert.deepEqual(await load(webHost), [webHost]);
    assert.equal((await load({ ...webHost, webUrl: "https://t3-east.example" }))[0]?.webUrl, "https://t3-east.example");
    for (const webUrl of [null, "http://t3-east.example", "https://t3-east.example/mcp",
      "https://t3-east.example/.", "https://t3-east.example/?", "https://t3-east.example/#", "https://@t3-east.example",
      " https://t3-east.example", "https:t3-east.example"]) {
      await assert.rejects(load({ ...webHost, webUrl }), /webUrl/);
    }
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("auth CLI stores only a read-only OAuth grant and reports its expiry without credentials", async () => {
  let origin = "";
  const fixtureToken = "fixture-oauth-bearer";
  const widerToken = "fixture-extra-scopes";
  const fixturePairing = "fixture-pairing";
  let scope = "orchestration:read";
  const server = createServer(async (request, response) => {
    try {
      let input = "";
      for await (const chunk of request.setEncoding("utf8")) input += chunk;
      const json = (value: unknown) => response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify(value));
      switch (request.url) {
        case "/.well-known/oauth-authorization-server":
          json({ issuer: origin, authorization_endpoint: origin + "/authorize", registration_endpoint: origin + "/register",
            token_endpoint: origin + "/token", revocation_endpoint: origin + "/revoke" });
          break;
        case "/.well-known/oauth-protected-resource/mcp":
          json({ resource: origin + "/mcp", authorization_servers: [origin] });
          break;
        case "/register": {
          const data = JSON.parse(input) as { redirect_uris: string[] };
          assert.equal(new URL(data.redirect_uris[0]!).hostname, "127.0.0.1");
          json({ client_id: "client" });
          break;
        }
        case "/oauth/mcp/decision": {
          const { decision, authorization } = JSON.parse(input) as {
            decision: unknown; authorization: { redirect_uri: string; state: string; code_challenge_method: string };
          };
          assert.deepEqual(decision, { _tag: "pairing-code", access: "read-only", code: fixturePairing });
          assert.equal(authorization.code_challenge_method, "S256");
          const redirect = new URL(authorization.redirect_uri);
          redirect.search = new URLSearchParams({ state: authorization.state, iss: origin, code: "fixture-code" }).toString();
          json({ redirectTo: redirect.href });
          break;
        }
        case "/token":
          assert.equal(new URLSearchParams(input).get("code"), "fixture-code");
          json({ access_token: scope === "orchestration:read" ? fixtureToken : widerToken, token_type: "Bearer", expires_in: 60, scope });
          break;
        default:
          response.writeHead(404).end();
      }
    } catch (error) {
      response.writeHead(500).end(String(error));
    }
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert(address && typeof address === "object");
  origin = "http://127.0.0.1:" + address.port;
  const directory = await mkdtemp(join(tmpdir(), "ostrom-platform-auth-"));
  try {
    const config = join(directory, "hosts.json");
    const tokensDir = join(directory, "tokens");
    await writeFile(config, JSON.stringify({ hosts: [{ id: "local", label: "Local", mcpUrl: origin + "/mcp" }] }));
    const authorize = async (...options: string[]) => {
      let output = "";
      let errors = "";
      const code = await runPlatformCli(["auth", "t3", "local", "--hosts-config", config, "--tokens-dir", tokensDir,
        ...options, "--json"], {
        stdout: { write: (text) => { output += text; } },
        stderr: { write: (text) => { errors += text; } },
      });
      assert(!(output + errors).includes(fixtureToken));
      assert(!(output + errors).includes(widerToken));
      assert(!(output + errors).includes(fixturePairing));
      return { code, output, errors };
    };
    for (const options of [[], ["--browser"], ["--browser", "--pairing-code", fixturePairing]]) {
      const refused = await authorize(...options);
      assert.equal(refused.code, 2);
      assert.equal(refused.output, "");
      assert.match(refused.errors, /T3 v0\.1\.13 requires --pairing-code <code> to enforce read-only access/);
    }
    const good = await authorize("--pairing-code", fixturePairing);
    assert.equal(good.code, 0, good.errors);
    const saved = await loadToken("local", tokensDir);
    assert.equal(saved?.accessToken, fixtureToken);
    assert.deepEqual(JSON.parse(good.output), {
      hostId: "local", access: "read-only", expiresAt: new Date(saved!.expiresAt).toISOString(),
    });
    scope += " orchestration:operate";
    const denied = await authorize("--pairing-code", fixturePairing);
    assert.equal(denied.code, 1);
    assert.match(denied.errors, /read-only bearer token/);
    assert.deepEqual(await loadToken("local", tokensDir), saved);
  } finally {
    server.close();
    server.closeAllConnections();
    await rm(directory, { recursive: true, force: true });
  }
});
