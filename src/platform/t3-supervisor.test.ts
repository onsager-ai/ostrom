import assert from "node:assert/strict";
import { once } from "node:events";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import type { ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { loadToken, saveToken } from "../connectors/t3/index.ts";
import type { T3MilestoneEvent, T3MilestoneName } from "../connectors/t3/index.ts";
import { runPlatformCli } from "./cli.ts";
import { loadPlatformHosts } from "./config.ts";
import { PlatformStore } from "./store.ts";
import { T3HostSupervisor } from "./t3-supervisor.ts";

const time = "2026-11-27T01:00:00.000Z";
const later = "2026-11-27T01:01:00.000Z";
const prKey = { host: "forge.example", repository: "org/repo", number: 17 };

type Rpc = { id: number; method: string; params: {
  name: string; cursor?: string; arguments: { action?: string; requestId?: string; cursor?: number };
} };

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
  let nextEnvironment: Promise<void> | undefined;
  let runStatus = "waiting";
  let watching = true;
  const pending = new Map<string, "user_input" | "approval">([["initial", "user_input"]]);
  const streams = new Map<string, ServerResponse>();
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
        response.once("close", () => { if (streams.get(name) === response) streams.delete(name); });
        frame(response, "active", { cursor: cursor(), truncated: truncated(rpc.params.cursor), heartbeatMs: 15_000 });
        return;
      }
      let result: unknown;
      if (rpc.method === "events/poll") {
        polls.push({ cursor: rpc.params.cursor, at: performance.now(), name });
        result = { events: [], cursor: cursor(), truncated: truncated(rpc.params.cursor), hasMore: false, nextPollMs: 200 };
      } else {
        let data: unknown;
        const thread = { threadId: "thread", title: id, status: runStatus, settled: false,
          relationshipToParent: null, updatedAt: later, activeRunId: runStatus === "completed" ? null : "run",
          pendingRequestCount: pending.size };
        switch (name) {
          case "t3_environment_read":
            environmentReads++;
            await nextEnvironment;
            data = { environmentId: "env-" + id, serverVersion: "test" };
            break;
          case "t3_thread_list":
            listPages++;
            data = args.cursor === undefined ? { threads: [], nextCursor: 1 } : { threads: [thread], nextCursor: null };
            break;
          case "t3_thread_read":
            data = { thread, recentRuns: [{ runId: "run", status: runStatus, startedAt: time,
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
    get requests() { return requests; },
    polls,
    pending,
    setStatus: (status: string) => { runStatus = status; },
    pauseEnvironment: (promise: Promise<void>) => { nextEnvironment = promise; },
    rotate: () => { epoch++; activity = 0; },
    heartbeat: () => { frame(stream("thread.run_settled"), "heartbeat", { cursor: cursor() }); },
    disconnect: () => { for (const response of streams.values()) response.end(); },
    revoke: () => { frame(stream("thread.run_settled"), "error", { error: { code: -32000, message: "Access revoked" } }); },
    gap: () => { frame(stream("thread.run_settled"), "active", { cursor: cursor(), truncated: true, heartbeatMs: 15_000 }); },
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
    supervisor = new T3HostSupervisor(store, [east.host, west.host], { tokensDir: directory, retryMs: 10 });
    await eventually(() => east.streamCount === 4 && west.streamCount === 4
      && store.listT3HostStates().every((host) => host.connectionState === "connected"));
    assert.deepEqual(store.listAgents().map((agent) => agent.externalRef), ["env-east/thread", "env-west/thread"]);
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
      && store.getT3Cursor("east", "thread.run_settled") === "epoch2.0");
    assert(east.environmentReads >= 3);
    assert.deepEqual(store.listOpenT3Waits("east").map((wait) => wait.requestId), ["recovered"]);
    assert.equal(store.listRuns().find((item) => item.id === run.id)?.semonSessionUid, "semon-exact");
    assert.equal(store.listT3ThreadPrs("east")[0]?.lastWakeSummary, "Checks passed");

    west.pauseEnvironment(hold.promise);
    west.rotate();
    west.gap();
    await eventually(() => west.environmentReads === 2
      && store.listT3HostStates().find((host) => host.hostId === "west")?.connectionState === "stale");
    hold.resolve();
    await eventually(() => west.streamCount === 4 && store.getT3Cursor("west", "thread.run_settled") === "epoch2.0");
    assert(store.listT3HostStates().every((host) => host.connectionState === "connected" && host.lastError === null));
  } finally {
    hold.resolve();
    await supervisor?.close();
    store.close();
    await Promise.all([east.close(), west.close(), rm(directory, { recursive: true, force: true })]);
  }
});

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
  let revoked = false;
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
        case "/revoke":
          assert.equal(new URLSearchParams(input).get("token"), widerToken);
          revoked = true;
          response.writeHead(200).end();
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
    const authorize = async () => {
      let output = "";
      let errors = "";
      const code = await runPlatformCli(["auth", "t3", "local", "--hosts-config", config, "--tokens-dir", tokensDir,
        "--pairing-code", fixturePairing, "--json"], {
        stdout: { write: (text) => { output += text; } },
        stderr: { write: (text) => { errors += text; } },
      });
      assert(!(output + errors).includes(fixtureToken));
      assert(!(output + errors).includes(widerToken));
      assert(!(output + errors).includes(fixturePairing));
      return { code, output, errors };
    };
    const good = await authorize();
    assert.equal(good.code, 0, good.errors);
    const saved = await loadToken("local", tokensDir);
    assert.equal(saved?.accessToken, fixtureToken);
    assert.deepEqual(JSON.parse(good.output), {
      hostId: "local", access: "read-only", expiresAt: new Date(saved!.expiresAt).toISOString(),
    });
    scope += " orchestration:operate";
    const denied = await authorize();
    assert.equal(denied.code, 1);
    assert.match(denied.errors, /read-only bearer token/);
    assert(revoked);
    assert.deepEqual(await loadToken("local", tokensDir), saved);
  } finally {
    server.close();
    server.closeAllConnections();
    await rm(directory, { recursive: true, force: true });
  }
});
