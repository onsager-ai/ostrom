import { createHash, randomBytes } from "node:crypto";
import { once } from "node:events";
import { createServer } from "node:http";
import type { T3Token } from "./tokens.ts";

export interface T3OAuthServerMetadata {
  issuer: string;
  authorization_endpoint: string;
  registration_endpoint: string;
  token_endpoint: string;
  revocation_endpoint: string;
  response_types_supported: string[];
  grant_types_supported: string[];
  code_challenge_methods_supported: string[];
  token_endpoint_auth_methods_supported: string[];
  scopes_supported: string[];
  authorization_response_iss_parameter_supported: boolean;
}

export interface T3OAuthResourceMetadata {
  resource: string;
  authorization_servers: string[];
  scopes_supported: string[];
  bearer_methods_supported: string[];
  resource_name: string;
}

export interface T3OAuthClientRegistration {
  client_id: string;
  client_name: string;
  redirect_uris: string[];
  token_endpoint_auth_method: "none";
  grant_types: "authorization_code"[];
  response_types: "code"[];
}

export interface T3PkcePair {
  verifier: string;
  challenge: string;
  state: string;
}

export interface T3AuthorizeOptions {
  clientId: string;
  redirectUri: string;
  challenge: string;
  state: string;
  resource?: string;
}

export interface T3OAuthAuthorization {
  response_type: "code";
  client_id: string;
  redirect_uri: string;
  code_challenge: string;
  code_challenge_method: "S256";
  state: string;
  resource?: string;
}

interface T3CodeExchangeOptions {
  clientId: string;
  redirectUri: string;
  code: string;
  verifier: string;
  resource?: string;
}

async function oauthRequest(url: string | URL, operation: string, body?: URLSearchParams | string): Promise<Response> {
  const response = await fetch(url, body === undefined ? { redirect: "error" } : {
    method: "POST",
    headers: { "content-type": typeof body === "string" ? "application/json" : "application/x-www-form-urlencoded" },
    body,
    redirect: "error",
  });
  if (!response.ok) {
    await response.body?.cancel();
    throw new Error(`T3 OAuth ${operation} failed (HTTP ${response.status})`);
  }
  return response;
}

async function oauthJson<Result>(url: string | URL, operation: string, body?: URLSearchParams | string): Promise<Result> {
  const response = await oauthRequest(url, operation, body);
  try {
    return await response.json() as Result;
  } catch (error) {
    if (!(error instanceof SyntaxError)) throw error;
    throw new Error(`T3 OAuth ${operation} returned invalid JSON`);
  }
}

export async function discoverOAuthMetadata(mcpUrl: string | URL) {
  const origin = new URL(mcpUrl).origin;
  const [authorizationServer, protectedResource] = await Promise.all([
    oauthJson<T3OAuthServerMetadata>(new URL("/.well-known/oauth-authorization-server", origin), "server discovery"),
    oauthJson<T3OAuthResourceMetadata>(new URL("/.well-known/oauth-protected-resource/mcp", origin), "resource discovery"),
  ]);
  if (authorizationServer.issuer !== origin ||
      !protectedResource.authorization_servers.includes(origin) ||
      protectedResource.resource !== new URL("/mcp", origin).href) {
    throw new Error("T3 OAuth discovery returned a different issuer or resource");
  }
  for (const endpoint of [
    authorizationServer.authorization_endpoint,
    authorizationServer.registration_endpoint,
    authorizationServer.token_endpoint,
    authorizationServer.revocation_endpoint,
  ]) {
    if (new URL(endpoint).origin !== origin) throw new Error("T3 OAuth endpoint is on a different origin");
  }
  return { authorizationServer, protectedResource };
}

export function registerOAuthClient(
  registrationEndpoint: string | URL,
  { clientName, redirectUris }: { clientName: string; redirectUris: string[] },
): Promise<T3OAuthClientRegistration> {
  return oauthJson(registrationEndpoint, "registration", JSON.stringify({
    client_name: clientName,
    redirect_uris: redirectUris,
  }));
}

export function generatePkcePair(): T3PkcePair {
  const verifier = randomBytes(32).toString("base64url");
  return {
    verifier,
    challenge: createHash("sha256").update(verifier).digest("base64url"),
    state: randomBytes(32).toString("base64url"),
  };
}

export function buildAuthorizationRequest(options: T3AuthorizeOptions): T3OAuthAuthorization {
  return {
    response_type: "code",
    client_id: options.clientId,
    redirect_uri: options.redirectUri,
    code_challenge: options.challenge,
    code_challenge_method: "S256",
    state: options.state,
    ...(options.resource === undefined ? {} : { resource: options.resource }),
  };
}

export function buildAuthorizeUrl(authorizationEndpoint: string | URL, options: T3AuthorizeOptions): URL {
  const url = new URL(authorizationEndpoint);
  for (const [key, value] of Object.entries(buildAuthorizationRequest(options))) {
    url.searchParams.set(key, value);
  }
  return url;
}

export function approveWithPairingCode(
  approvalUrlOrOrigin: string | URL,
  { authorization, code }: { authorization: T3OAuthAuthorization; code: string },
): Promise<{ redirectTo: string }> {
  const endpoint = new URL("/oauth/mcp/decision", approvalUrlOrOrigin);
  return oauthJson(endpoint, "pairing-code approval", JSON.stringify({
    authorization,
    decision: { _tag: "pairing-code", access: "read-only", code },
  }));
}

export async function exchangeAuthorizationCode(
  tokenEndpoint: string | URL,
  options: T3CodeExchangeOptions,
): Promise<T3Token> {
  const body = new URLSearchParams({
    grant_type: "authorization_code",
    code: options.code,
    client_id: options.clientId,
    redirect_uri: options.redirectUri,
    code_verifier: options.verifier,
  });
  if (options.resource !== undefined) body.set("resource", options.resource);
  const now = Date.now();
  const token = await oauthJson<{ access_token: string; token_type: string; expires_in: number; scope: string }>(
    tokenEndpoint, "token exchange", body,
  );
  if (token.token_type !== "Bearer" || token.scope !== "orchestration:read" ||
      typeof token.access_token !== "string" || !token.access_token ||
      !Number.isFinite(token.expires_in) || token.expires_in < 0) {
    throw new Error("T3 OAuth token exchange did not return a read-only bearer token");
  }
  return { accessToken: token.access_token, expiresAt: now + token.expires_in * 1000 };
}

export async function revokeOAuthToken(revocationEndpoint: string | URL, token: string): Promise<void> {
  const response = await oauthRequest(
    revocationEndpoint, "revocation", new URLSearchParams({ token, token_type_hint: "access_token" }),
  );
  await response.body?.cancel();
}

interface T3CallbackOptions {
  state: string;
  issuer: string;
}

type T3CallbackResult = { code: string } | { error: Error };

export async function waitForLoopbackCallback(
  { host = "127.0.0.1", timeoutMs = 120_000 }: { host?: "127.0.0.1"; timeoutMs?: number } = {},
) {
  if (host !== "127.0.0.1") throw new Error("T3 OAuth callbacks must use IPv4 loopback");
  const result = Promise.withResolvers<T3CallbackResult>();
  let expected: T3CallbackOptions | undefined;
  let timer: ReturnType<typeof setTimeout>;
  let finished = false;

  function finish(value: T3CallbackResult) {
    if (finished) return;
    finished = true;
    result.resolve(value);
    clearTimeout(timer);
    server.close();
  }

  const server = createServer((request, response) => {
    response.setHeader("connection", "close");
    response.setHeader("cache-control", "no-store");
    response.setHeader("content-type", "text/plain; charset=utf-8");
    // Resolve the callback using the bound address, ignoring the request's Host header.
    if (request.method !== "GET" || !request.url?.startsWith("/")) {
      response.writeHead(404).end("Callback path not found.");
      return;
    }
    const target = new URL(`http://${host}${request.url}`);
    if (target.pathname !== "/callback") {
      response.writeHead(404).end("Callback path not found.");
      return;
    }
    if (finished || !expected || target.searchParams.get("state") !== expected.state ||
        target.searchParams.get("iss") !== expected.issuer) {
      response.writeHead(400).end("OAuth state or issuer did not match.");
      return;
    }
    const code = target.searchParams.get("code");
    if (target.searchParams.has("error") || !code) {
      response.writeHead(400).end("T3 authorization failed.");
      finish({ error: new Error("T3 OAuth authorization failed") });
      return;
    }
    response.end("T3 authorization complete. You can close this tab.");
    finish({ code });
  });

  server.listen(0, host);
  await once(server, "listening");
  const address = server.address();
  if (address === null || typeof address === "string") {
    server.close();
    throw new Error("T3 loopback callback failed to bind a TCP address");
  }
  const redirectUri = `http://${host}:${address.port}/callback`;
  timer = setTimeout(() => finish({ error: new Error("T3 OAuth callback timed out") }), timeoutMs);

  return {
    redirectUri,
    async waitForResult(options: T3CallbackOptions): Promise<{ code: string }> {
      if (expected) throw new Error("T3 OAuth callback already has a waiter");
      expected = { ...options };
      const callback = await result.promise;
      if ("error" in callback) throw callback.error;
      return callback;
    },
    close() {
      finish({ error: new Error("T3 OAuth callback closed") });
    },
  };
}
