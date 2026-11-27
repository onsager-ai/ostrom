import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, statSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";

import { runPlatformCli } from "./cli.ts";
import { startStatusMcp } from "./status-mcp.ts";
import { mintStatusApproval, statusPublicOrigin } from "./status-mcp-oauth.ts";

const publicUrl = "https://status.example.com";
const redirectUri = "https://chatgpt.com/connector_platform_oauth_redirect";
const verifier = "a".repeat(43);
async function json<T>(response: Response): Promise<T> {
  return await response.json() as T;
}

const challenge = createHash("sha256").update(verifier).digest("base64url");

test("status MCP OAuth, transport and tool discovery", async () => {
  const root = mkdtempSync(join(tmpdir(), "ostrom-status-test-"));
  const stateDir = join(root, "oauth");
  const config = { publicUrl, stateDir, socketPath: join(root, "missing.sock") };
  let server = await startStatusMcp(config);
  const request = (path: string, init?: RequestInit) => fetch(new URL(path, server.url), { ...init, redirect: "manual" });
  const postJson = (path: string, data: unknown, token?: string) => request(path, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...(token ? { Authorization: `Bearer ${token}` } : {}) },
    body: JSON.stringify(data),
  });
  const form = (path: string, data: Record<string, string>) =>
    request(path, { method: "POST", body: new URLSearchParams(data) });
  const rpc = (token: string, method: string, params?: unknown) =>
    postJson("/mcp", { jsonrpc: "2.0", id: 1, method, params }, token);
  try {
    const denied = await postJson("/mcp", {});
    assert.equal(denied.status, 401);
    assert.equal(denied.headers.get("www-authenticate"),
      `Bearer resource_metadata="${publicUrl}/.well-known/oauth-protected-resource/mcp"`);
    for (const path of ["/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp"]) {
      assert.deepEqual(await (await request(path)).json(), {
        resource: `${publicUrl}/mcp`, authorization_servers: [publicUrl],
        scopes_supported: ["status:read"], bearer_methods_supported: ["header"],
      });
    }
    const metadata = await json<{
      issuer: string; authorization_endpoint: string; token_endpoint: string;
      registration_endpoint: string; revocation_endpoint: string;
      code_challenge_methods_supported: string[]; token_endpoint_auth_methods_supported: string[];
    }>(await request("/.well-known/oauth-authorization-server"));
    assert.equal(metadata.issuer, publicUrl);
    assert.equal(metadata.authorization_endpoint, `${publicUrl}/oauth/authorize`);
    assert.equal(metadata.token_endpoint, `${publicUrl}/oauth/token`);
    assert.equal(metadata.registration_endpoint, `${publicUrl}/oauth/register`);
    assert.equal(metadata.revocation_endpoint, `${publicUrl}/oauth/revoke`);
    assert.deepEqual(metadata.code_challenge_methods_supported, ["S256"]);
    assert.deepEqual(metadata.token_endpoint_auth_methods_supported, ["none"]);

    for (const uri of ["https://evil.example/redirect", redirectUri.replace("https:", "http:"),
      redirectUri.replace("chatgpt.com", "user@chatgpt.com"), redirectUri + "#", redirectUri + "/"]) {
      assert.equal((await postJson("/oauth/register", { redirect_uris: [uri] })).status, 400);
    }
    const registration = await postJson("/oauth/register", { redirect_uris: [redirectUri] });
    assert.equal(registration.status, 201);
    const { client_id } = await json<{ client_id: string }>(registration);
    assert.equal(typeof client_id, "string");
    const secondRegistration = await postJson("/oauth/register", { redirect_uris: [redirectUri] });
    assert.equal(secondRegistration.status, 201);
    const secondClient = await json<{ client_id: string }>(secondRegistration);
    assert.equal(typeof secondClient.client_id, "string");
    assert.notEqual(secondClient.client_id, client_id);
    const auth = {
      client_id, redirect_uri: redirectUri, response_type: "code", code_challenge: challenge,
      code_challenge_method: "S256", state: 'abc"><script>', resource: `${publicUrl}/mcp`,
    };
    const approvePath = (params: Record<string, string>) => `/oauth/authorize?${new URLSearchParams(params)}`;
    for (const overrides of [{ code_challenge_method: "plain" }, { resource: "https://other.example/mcp" },
      { state: "" }, { client_id: client_id + "a" }, { redirect_uri: "https://platform.openai.com/apps-manage/oauth" }]) {
      assert.equal((await request(approvePath({ ...auth, ...overrides }))).status, 400);
    }
    const page = await request(approvePath(auth));
    assert.equal(page.status, 200);
    assert.match(page.headers.get("content-security-policy") ?? "", /default-src 'none'/);
    assert.match(await page.text(), /abc&quot;&gt;&lt;script&gt;/);

    const expired = mintStatusApproval(stateDir);
    const statePath = join(stateDir, "oauth.sqlite");
    const db = new DatabaseSync(statePath);
    db.exec("UPDATE approval SET expires = 0");
    db.close();
    assert.equal((await form("/oauth/authorize", { ...auth, approval_code: expired })).status, 400);
    const approval = mintStatusApproval(stateDir);
    assert.equal(statSync(stateDir).mode & 0o777, 0o700);
    assert.equal(statSync(statePath).mode & 0o777, 0o600);
    assert.ok(!readFileSync(statePath).includes(approval));
    assert.equal((await form("/oauth/authorize", { ...auth, approval_code: "x".repeat(43) })).status, 400);
    const approvals = await Promise.all([
      form("/oauth/authorize", { ...auth, approval_code: approval }),
      form("/oauth/authorize", { ...auth, approval_code: approval }),
    ]);
    assert.deepEqual(approvals.map(({ status }) => status).sort(), [303, 400]);
    const callback = new URL(approvals.find(({ status }) => status === 303)?.headers.get("location") ?? "");
    assert.equal(callback.origin + callback.pathname, redirectUri);
    assert.equal(callback.searchParams.get("state"), auth.state);
    assert.equal(callback.searchParams.get("iss"), publicUrl);
    const tokenRequest = {
      grant_type: "authorization_code", code: callback.searchParams.get("code") ?? "",
      client_id, redirect_uri: redirectUri, code_verifier: verifier, resource: auth.resource,
    };
    assert.equal((await form("/oauth/token", { ...tokenRequest, code_verifier: "b".repeat(43) })).status, 400);
    assert.equal((await form("/oauth/token", { ...tokenRequest, resource: `${publicUrl}/other` })).status, 400);
    const exchange = await form("/oauth/token", tokenRequest);
    assert.equal(exchange.status, 200);
    const { access_token: token, token_type, scope, expires_in } = await json<{ access_token: string; token_type: string; scope: string; expires_in: number }>(exchange);
    assert.equal(token_type, "Bearer");
    assert.equal(scope, "status:read");
    assert.equal(expires_in, 604800);
    assert.ok(!readFileSync(statePath).includes(token));
    assert.equal((await form("/oauth/token", tokenRequest)).status, 400);

    await server.close();
    server = await startStatusMcp({ ...config, publicUrl: "https://other.example" });
    assert.equal((await rpc(token, "ping")).status, 401);
    await server.close();
    server = await startStatusMcp(config);

    const method = await request("/mcp");
    assert.equal(method.status, 405);
    assert.equal(method.headers.get("allow"), "POST");
    assert.equal(method.headers.get("cache-control"), "no-store");
    assert.equal(method.headers.get("referrer-policy"), "no-referrer");
    assert.equal(method.headers.get("x-content-type-options"), "nosniff");
    assert.equal((await postJson("/mcp", "a".repeat(64 * 1024), token)).status, 413);
    const unsupported = await request("/mcp", {
      method: "POST", headers: {
        Authorization: `Bearer ${token}`, "Content-Type": "application/json", "MCP-Protocol-Version": "1900-01-01",
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "ping" }),
    });
    assert.equal(unsupported.status, 400);
    assert.deepEqual(await unsupported.json(), { error: "unsupported_protocol_version" });
    for (const protocolVersion of ["2025-06-18", "2026-07-28"]) {
      assert.equal((await json<{ result: { protocolVersion: string } }>(await rpc(token, "initialize", { protocolVersion }))).result.protocolVersion, protocolVersion);
    }
    assert.equal((await postJson("/mcp", { jsonrpc: "2.0", method: "notifications/initialized" }, token)).status, 202);
    const rpcError = async (method: string, params?: unknown) =>
      (await json<{ error: { code: number } }>(await rpc(token, method, params))).error.code;
    assert.equal(await rpcError("unknown"), -32601);
    assert.equal(await rpcError("tools/call", { name: "answer_decision" }), -32602);
    assert.equal(await rpcError("tools/call", { name: "read_latest_digest", arguments: { window: "other" } }), -32602);
    const unavailable = await json<{ result: unknown }>(await rpc(token, "tools/call", { name: "status_summary" }));
    assert.deepEqual(unavailable.result, { isError: true, content: [{ type: "text", text: "Platform status unavailable." }] });
    const { result } = await json<{ result: { tools: { name: string; annotations: unknown }[] } }>(
      await rpc(token, "tools/list"));
    assert.deepEqual(result.tools.map(({ name }: { name: string }) => name),
      ["status_summary", "list_lanes", "list_open_decisions", "read_latest_digest"]);
    for (const tool of result.tools) {
      assert.deepEqual(tool.annotations, {
        readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false,
      });
    }
    let response: Response;
    do {
      response = await rpc(token, "ping");
      await response.text();
    } while (response.status === 200);
    assert.equal(response.status, 429);
    assert.ok(Number(response.headers.get("retry-after")) > 0);
    await server.close();
    server = await startStatusMcp(config);
    response = await request("/.well-known/oauth-authorization-server");
    await response.text();
    assert.equal(response.status, 200);
    // The IP budget includes discovery and rejects requests even without an access token.
    for (let i = 0; i < 121 && response.status === 200; i++) {
      response = await request("/.well-known/oauth-authorization-server");
      await response.text();
    }
    assert.equal(response.status, 429);
    assert.ok(Number(response.headers.get("retry-after")) > 0);
    assert.equal((await rpc(token, "ping")).status, 200);
    assert.equal((await rpc("x".repeat(43), "ping")).status, 429);
    await server.close();
    server = await startStatusMcp(config);
    assert.equal((await form("/oauth/revoke", { token })).status, 200);
    assert.equal((await form("/oauth/revoke", { token })).status, 200);
    assert.equal((await rpc(token, "ping")).status, 401);
  } finally {
    await server.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test("public origins and approval CLI", async () => {
  assert.equal(statusPublicOrigin("http://127.0.0.1:3000"), "http://127.0.0.1:3000");
  assert.equal(statusPublicOrigin("http://localhost:3000/"), "http://localhost:3000");
  for (const url of ["http://example.com", "https://user@example.com", "https://example.com?",
    "https://example.com#", "https://example.com/mcp", "http://127.1", "http://2130706433"]) {
    assert.throws(() => statusPublicOrigin(url));
  }
  const root = mkdtempSync(join(tmpdir(), "ostrom-status-cli-"));
  let printed = "";
  try {
    assert.equal(await runPlatformCli(["status-mcp-approve", "--state-dir", join(root, "state"), "--ttl-sec", "10"], {
      stdout: { write: (text) => { printed += text; } },
    }), 0);
    assert.match(printed, /^[A-Za-z0-9_-]{43}\n$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});


test("OAuth state rejects symlinked database files and corrupt signing keys", async () => {
  const root = mkdtempSync(join(tmpdir(), "ostrom-status-state-"));
  const stateDir = join(root, "oauth");
  const statePath = join(stateDir, "oauth.sqlite");
  const target = join(root, "target");
  const config = { publicUrl, stateDir, socketPath: join(root, "missing.sock") };
  try {
    mkdirSync(stateDir, { mode: 0o700 });
    writeFileSync(target, "untouched", { mode: 0o600 });
    symlinkSync(target, statePath);
    assert.throws(() => mintStatusApproval(stateDir), /state file/);
    assert.equal(readFileSync(target, "utf8"), "untouched");
    unlinkSync(statePath);
    const server = await startStatusMcp(config);
    await server.close();
    const db = new DatabaseSync(statePath);
    try {
      db.prepare("UPDATE signing_key SET key = ?").run("x".repeat(43));
    } finally {
      db.close();
    }
    await assert.rejects(startStatusMcp(config), /Invalid Status MCP signing key/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
