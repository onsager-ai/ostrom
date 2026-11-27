import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { mkdtemp, readdir, rm, stat } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  approveWithPairingCode,
  buildAuthorizationRequest,
  buildAuthorizeUrl,
  discoverOAuthMetadata,
  exchangeAuthorizationCode,
  generatePkcePair,
  registerOAuthClient,
  revokeOAuthToken,
  waitForLoopbackCallback,
} from "./oauth.ts";
import type { T3OAuthResourceMetadata, T3OAuthServerMetadata } from "./oauth.ts";
import { deleteToken, getTokenExpiryStatus, loadToken, saveToken } from "./tokens.ts";

test("obtains, stores and revokes a read-only token with PKCE", async (t) => {
  const callback = await waitForLoopbackCallback();
  t.after(() => callback.close());
  const pkce = generatePkcePair();
  assert.match(pkce.verifier, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(pkce.challenge, createHash("sha256").update(pkce.verifier).digest("base64url"));
  assert.match(pkce.challenge, /^[A-Za-z0-9_-]{43}$/);
  assert.match(pkce.state, /^[A-Za-z0-9_-]{43}$/);
  assert.notEqual(pkce.state, pkce.verifier);

  const clientId = "mock-client";
  const authorizationCode = "mock-authorization-code";
  const accessToken = "mock-access-token";
  const pairingCode = "mock-pairing-code";
  const scope = "orchestration:read";
  const requests: string[] = [];
  let origin: string;
  const server = createServer(async (request, response) => {
    try {
      const url = new URL(request.url ?? "/", origin);
      requests.push(url.pathname);
      assert.equal(request.headers.origin, undefined);
      const resource = `${origin}/mcp`;
      const metadata: T3OAuthServerMetadata = {
        issuer: origin,
        authorization_endpoint: `${origin}/oauth/mcp/authorize`,
        registration_endpoint: `${origin}/oauth/mcp/register`,
        token_endpoint: `${origin}/oauth/mcp/token`,
        revocation_endpoint: `${origin}/oauth/mcp/revoke`,
        response_types_supported: ["code"],
        grant_types_supported: ["authorization_code"],
        code_challenge_methods_supported: ["S256"],
        token_endpoint_auth_methods_supported: ["none"],
        scopes_supported: [scope, "orchestration:operate"],
        authorization_response_iss_parameter_supported: true,
      };
      const resourceMetadata: T3OAuthResourceMetadata = {
        resource,
        authorization_servers: [origin],
        scopes_supported: metadata.scopes_supported,
        bearer_methods_supported: ["header"],
        resource_name: "T3 Code",
      };
      const chunks: Buffer[] = [];
      for await (const chunk of request) chunks.push(Buffer.from(chunk));
      const body = Buffer.concat(chunks).toString();
      const json = (status: number, payload: unknown) => {
        response.writeHead(status, { "content-type": "application/json" });
        response.end(JSON.stringify(payload));
      };
      const authorize = {
        response_type: "code",
        client_id: clientId,
        redirect_uri: callback.redirectUri,
        code_challenge: pkce.challenge,
        code_challenge_method: "S256",
        state: pkce.state,
        resource,
      };
      switch (url.pathname) {
        case "/.well-known/oauth-authorization-server":
          assert.equal(request.method, "GET");
          json(200, metadata);
          break;
        case "/.well-known/oauth-protected-resource/mcp":
          assert.equal(request.method, "GET");
          json(200, resourceMetadata);
          break;
        case "/oauth/mcp/register":
          assert.equal(request.method, "POST");
          assert.equal(request.headers["content-type"], "application/json");
          assert.deepEqual(JSON.parse(body), { client_name: "Ostrom test", redirect_uris: [callback.redirectUri] });
          json(201, {
            client_id: clientId,
            client_name: "Ostrom test",
            redirect_uris: [callback.redirectUri],
            token_endpoint_auth_method: "none",
            grant_types: ["authorization_code"],
            response_types: ["code"],
          });
          break;
        case "/oauth/mcp/authorize":
          assert.equal(request.method, "GET");
          assert.deepEqual(Object.fromEntries(url.searchParams), authorize);
          response.writeHead(302, { location: `/connect-agent${url.search}` }).end();
          break;
        case "/oauth/mcp/decision": {
          assert.equal(request.method, "POST");
          assert.equal(request.headers["content-type"], "application/json");
          assert.deepEqual(JSON.parse(body), {
            authorization: authorize,
            decision: { _tag: "pairing-code", access: "read-only", code: pairingCode },
          });
          const redirect = new URL(callback.redirectUri);
          redirect.searchParams.set("code", authorizationCode);
          redirect.searchParams.set("state", pkce.state);
          redirect.searchParams.set("iss", origin);
          json(200, { redirectTo: redirect.href });
          break;
        }
        case "/oauth/mcp/token": {
          assert.equal(request.method, "POST");
          assert.equal(request.headers["content-type"], "application/x-www-form-urlencoded");
          const params = new URLSearchParams(body);
          assert.deepEqual(Object.fromEntries(params), {
            grant_type: "authorization_code",
            code: authorizationCode,
            redirect_uri: callback.redirectUri,
            client_id: clientId,
            code_verifier: pkce.verifier,
            resource,
          });
          json(200, { access_token: accessToken, token_type: "Bearer", expires_in: 2_592_000, scope });
          break;
        }
        case "/oauth/mcp/revoke":
          assert.equal(request.method, "POST");
          assert.equal(request.headers["content-type"], "application/x-www-form-urlencoded");
          assert.deepEqual(Object.fromEntries(new URLSearchParams(body)), {
            token: accessToken, token_type_hint: "access_token",
          });
          response.writeHead(200).end();
          break;
        default:
          response.writeHead(404).end();
      }
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
  origin = `http://127.0.0.1:${address.port}`;
  assert.equal(new URL(callback.redirectUri).hostname, "127.0.0.1");
  assert(Number(new URL(callback.redirectUri).port) > 0);

  const { authorizationServer, protectedResource } = await discoverOAuthMetadata(`${origin}/mcp`);
  assert.equal(authorizationServer.issuer, origin);
  assert.equal(protectedResource.resource, `${origin}/mcp`);
  assert.deepEqual(authorizationServer.code_challenge_methods_supported, ["S256"]);

  const registered = await registerOAuthClient(authorizationServer.registration_endpoint, {
    clientName: "Ostrom test", redirectUris: [callback.redirectUri],
  });
  assert.equal(registered.client_id, clientId);
  const options = {
    clientId: registered.client_id,
    redirectUri: callback.redirectUri,
    challenge: pkce.challenge,
    state: pkce.state,
    resource: protectedResource.resource,
  };
  const authorizeUrl = buildAuthorizeUrl(authorizationServer.authorization_endpoint, options);
  const authorize = await fetch(authorizeUrl, { redirect: "manual" });
  assert.equal(authorize.status, 302);
  const location = authorize.headers.get("location");
  assert(location);
  const pending = callback.waitForResult({ state: pkce.state, issuer: authorizationServer.issuer });
  const { redirectTo } = await approveWithPairingCode(new URL(location, origin), {
    authorization: buildAuthorizationRequest(options),
    code: pairingCode,
  });
  for (const [key, value] of [["state", "wrong-state"], ["iss", `${origin}/wrong-issuer`]] as const) {
    const invalid = new URL(redirectTo);
    invalid.searchParams.set(key, value);
    const rejected = await fetch(invalid);
    assert.equal(rejected.status, 400);
    await rejected.body?.cancel();
  }
  const browser = await fetch(redirectTo);
  assert.equal(browser.status, 200);
  await browser.body?.cancel();
  const { code } = await pending;
  assert.equal(code, authorizationCode);
  const before = Date.now();
  const token = await exchangeAuthorizationCode(authorizationServer.token_endpoint, {
    clientId, redirectUri: callback.redirectUri, code, verifier: pkce.verifier, resource: options.resource,
  });
  const lifetimeMs = 2_592_000_000;
  assert.equal(token.accessToken, accessToken);
  assert(token.expiresAt >= before + lifetimeMs && token.expiresAt <= Date.now() + lifetimeMs);

  const stateRoot = await mkdtemp(join(tmpdir(), "ostrom-t3-test-"));
  t.after(() => rm(stateRoot, { force: true, recursive: true }));
  const dir = join(stateRoot, "t3-tokens");
  const hostId = "test-host";
  assert.equal(await loadToken(hostId, dir), null);
  await saveToken(hostId, token, dir);
  assert.deepEqual(await readdir(dir), ["test-host.json"]);
  assert.equal((await stat(dir)).mode & 0o777, 0o700);
  assert.equal((await stat(join(dir, "test-host.json"))).mode & 0o777, 0o600);
  assert.deepEqual(await loadToken(hostId, dir), token);
  await saveToken("other-host", { accessToken: "other-mock-token", expiresAt: token.expiresAt }, dir);
  assert.equal((await loadToken(hostId, dir))?.accessToken, accessToken);
  await assert.rejects(saveToken("../test-host", token, dir), /Invalid T3 token host ID/);

  const sevenDays = 7 * 86_400_000;
  assert.deepEqual(getTokenExpiryStatus(token, token.expiresAt - sevenDays), {
    expired: false, warning: true, remainingMs: sevenDays, expiresAt: token.expiresAt,
  });
  assert.equal(getTokenExpiryStatus(token, token.expiresAt - sevenDays - 1).warning, false);
  assert.deepEqual(getTokenExpiryStatus(token, token.expiresAt), {
    expired: true, warning: false, remainingMs: 0, expiresAt: token.expiresAt,
  });
  await revokeOAuthToken(authorizationServer.revocation_endpoint, token.accessToken);
  await deleteToken(hostId, dir);
  assert.equal(await loadToken(hostId, dir), null);
  assert(await loadToken("other-host", dir));
  assert.deepEqual(requests.slice(0, 2).sort(), [
    "/.well-known/oauth-authorization-server", "/.well-known/oauth-protected-resource/mcp",
  ]);
  assert.deepEqual(requests.slice(2), [
    "/oauth/mcp/register", "/oauth/mcp/authorize", "/oauth/mcp/decision", "/oauth/mcp/token", "/oauth/mcp/revoke",
  ]);
});

test("loopback callback stops waiting at the timeout", async (t) => {
  const callback = await waitForLoopbackCallback({ timeoutMs: 20 });
  t.after(() => callback.close());
  await assert.rejects(callback.waitForResult({ state: "test-state", issuer: "http://127.0.0.1" }), /timed out/);
});
