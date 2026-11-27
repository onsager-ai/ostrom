import { once } from "node:events";
import { createServer } from "node:http";
import type { IncomingMessage, ServerResponse } from "node:http";

import { digestWindows, isDigestWindow } from "./digest.ts";
import { createStatusOAuth, statusPublicOrigin } from "./status-mcp-oauth.ts";
import { createStatusReader } from "./status-mcp-sanitize.ts";

const bodyLimit = 64 * 1024;
const rateWindow = 60_000;
const resourceMetadataPath = "/.well-known/oauth-protected-resource/mcp";
const defaultVersion = "2025-06-18";
const versions = [defaultVersion, "2026-07-28"];
const tools = [
  { name: "status_summary", description: "Read aggregate platform health and spend availability." },
  { name: "list_lanes", description: "List lanes and their current goals." },
  { name: "list_open_decisions", description: "List pending decisions and their options." },
  { name: "read_latest_digest", description: "Read a sanitized digest for the requested window." },
].map((tool) => ({
  ...tool,
  annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false },
  inputSchema: {
    type: "object",
    properties: tool.name === "read_latest_digest" ? { window: { type: "string", enum: digestWindows } } : {},
    additionalProperties: false,
  },
}));

class RequestError extends Error {
  readonly status: number;
  constructor(status: number) {
    super();
    this.status = status;
  }
}

function object(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : undefined;
}

function json(response: ServerResponse, status: number, value: unknown): void {
  response.writeHead(status, { "Content-Type": "application/json; charset=utf-8" }).end(JSON.stringify(value));
}

async function readBody(request: IncomingMessage, contentType: string): Promise<string> {
  if (Number(request.headers["content-length"]) > bodyLimit) throw new RequestError(413);
  if (request.headers["content-type"]?.split(";")[0]?.trim().toLowerCase() !== contentType) {
    throw new RequestError(415);
  }
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of request.iterator({ destroyOnReturn: false }) as AsyncIterable<Buffer>) {
    size += chunk.length;
    if (size > bodyLimit) throw new RequestError(413);
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}

function limiter() {
  const clients = new Map<string, number[]>();
  let sweepAt = 0;
  return (key: string, maximum: number): number => {
    const now = Date.now();
    if (now >= sweepAt) {
      for (const [client, times] of clients) if ((times.at(-1) ?? 0) <= now - rateWindow) clients.delete(client);
      sweepAt = now + rateWindow;
    }
    const times = (clients.get(key) ?? []).filter((time) => time > now - rateWindow);
    if (times.length >= maximum) return Math.ceil(((times[0] ?? now) + rateWindow - now) / 1000);
    clients.set(key, [...times, now]);
    return 0;
  };
}

async function handleRpc(value: unknown, response: ServerResponse, reader: ReturnType<typeof createStatusReader>) {
  const rpc = object(value);
  const id = rpc?.["id"];
  const validId = typeof id === "string" || typeof id === "number" || id === null;
  const fail = (code: number, message: string, status = 200) =>
    json(response, status, { jsonrpc: "2.0", id: validId ? id : null, error: { code, message } });
  const result = (data: unknown) => json(response, 200, { jsonrpc: "2.0", id, result: data });
  if (rpc?.["jsonrpc"] !== "2.0" || typeof rpc["method"] !== "string" || (id !== undefined && !validId)) {
    return fail(-32600, "Invalid Request", 400);
  }
  if (id === undefined) {
    response.writeHead(202).end();
    return;
  }
  const params = object(rpc["params"]);
  switch (rpc["method"]) {
    case "initialize": {
      const requested = params?.["protocolVersion"];
      if (typeof requested !== "string") return fail(-32602, "Invalid params");
      return result({
        protocolVersion: versions.includes(requested) ? requested : defaultVersion,
        capabilities: { tools: {} },
        serverInfo: { name: "ostrom-status", version: "1.0.0" },
      });
    }
    case "ping":
      return result({});
    case "tools/list":
      return result({ tools });
    case "tools/call": {
      const name = params?.["name"];
      if (!tools.some((tool) => tool.name === name)) return fail(-32602, "Unknown tool");
      const args = params?.["arguments"] === undefined ? {} : object(params["arguments"]);
      if (!args || Object.keys(args).some((key) => name !== "read_latest_digest" || key !== "window")) {
        return fail(-32602, "Invalid tool arguments");
      }
      const window = args["window"] === undefined ? "01:00Z" : args["window"];
      if (typeof window !== "string" || !isDigestWindow(window)) return fail(-32602, "Invalid digest window");
      try {
        let data: unknown;
        switch (name) {
          case "status_summary": data = await reader.statusSummary(); break;
          case "list_lanes": data = await reader.listLanes(); break;
          case "list_open_decisions": data = await reader.listOpenDecisions(); break;
          case "read_latest_digest": data = await reader.readLatestDigest(window); break;
        }
        return result({ content: [{ type: "text", text: JSON.stringify(data) }] });
      } catch {
        return result({ isError: true, content: [{ type: "text", text: "Platform status unavailable." }] });
      }
    }
    default:
      return fail(-32601, "Method not found");
  }
}

export async function startStatusMcp({ publicUrl, socketPath, stateDir, port = 0, redirectAllowlist }: {
  publicUrl: string; socketPath: string; stateDir: string; port?: number; redirectAllowlist?: readonly string[];
}) {
  const origin = statusPublicOrigin(publicUrl);
  const oauth = createStatusOAuth(origin, stateDir, redirectAllowlist);
  const reader = createStatusReader(socketPath);
  const limit = limiter();
  const routes = new Map([
    ["/mcp", "POST"],
    ["/.well-known/oauth-protected-resource", "GET"],
    [resourceMetadataPath, "GET"],
    ["/.well-known/oauth-authorization-server", "GET"],
    ["/oauth/register", "POST"],
    ["/oauth/authorize", "GET, POST"],
    ["/oauth/token", "POST"],
    ["/oauth/revoke", "POST"],
  ]);

  async function handle(request: IncomingMessage, response: ServerResponse) {
    response.setHeader("Cache-Control", "no-store");
    response.setHeader("Referrer-Policy", "no-referrer");
    response.setHeader("X-Content-Type-Options", "nosniff");
    response.setHeader("Content-Security-Policy", "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'");
    if (!request.url?.startsWith("/") || request.url.startsWith("//") || request.url.includes("\\")) {
      throw new RequestError(400);
    }
    const url = new URL(`${origin}${request.url}`);
    const methods = routes.get(url.pathname);
    if (!methods) throw new RequestError(404);
    if (!methods.split(", ").includes(request.method ?? "")) {
      response.setHeader("Allow", methods);
      throw new RequestError(405);
    }
    if (request.headers.origin !== undefined && request.headers.origin !== origin) throw new RequestError(403);
    const token = url.pathname === "/mcp" ? oauth.authenticate(request.headers.authorization) : undefined;
    // A reverse proxy shares one anonymous IP budget. Authenticated reads have their own budget.
    const retry = token === undefined
      ? limit(`ip:${request.socket.remoteAddress ?? ""}`, 120)
      : limit(`token:${token}`, 60);
    if (retry) {
      response.setHeader("Retry-After", retry);
      throw new RequestError(429);
    }
    switch (url.pathname) {
      case "/.well-known/oauth-protected-resource":
      case resourceMetadataPath:
        return json(response, 200, oauth.resourceMetadata);
      case "/.well-known/oauth-authorization-server":
        return json(response, 200, oauth.serverMetadata);
      case "/mcp": {
        if (!token) {
          response.setHeader("WWW-Authenticate", `Bearer resource_metadata="${origin}${resourceMetadataPath}"`);
          throw new RequestError(401);
        }
        const version = request.headers["mcp-protocol-version"];
        if (version !== undefined && (typeof version !== "string" || !versions.includes(version))) {
          return json(response, 400, { error: "unsupported_protocol_version" });
        }
        const raw = await readBody(request, "application/json");
        let value: unknown;
        try { value = JSON.parse(raw); } catch {
          return json(response, 400, { jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } });
        }
        return handleRpc(value, response, reader);
      }
      case "/oauth/register": {
        const raw = await readBody(request, "application/json");
        let value: unknown;
        try { value = JSON.parse(raw); } catch { value = undefined; }
        const result = oauth.register(object(value) ?? {});
        return json(response, result.status, result.body);
      }
      case "/oauth/authorize": {
        if (request.method === "GET") {
          const page = oauth.approvalPage(url.searchParams);
          if (!page) return json(response, 400, { error: "invalid_request" });
          response.setHeader("Content-Security-Policy",
            `default-src 'none'; form-action 'self' ${new URL(page.redirect).origin}; frame-ancestors 'none'; base-uri 'none'`);
          response.writeHead(200, { "Content-Type": "text/html; charset=utf-8" }).end(page.html);
          return;
        }
        const redirect = oauth.authorize(new URLSearchParams(await readBody(request, "application/x-www-form-urlencoded")));
        if (!redirect) return json(response, 400, { error: "access_denied" });
        response.writeHead(303, { Location: redirect }).end();
        return;
      }
      case "/oauth/token": {
        const result = oauth.exchange(new URLSearchParams(await readBody(request, "application/x-www-form-urlencoded")));
        return json(response, result.status, result.body);
      }
      case "/oauth/revoke":
        oauth.revoke(new URLSearchParams(await readBody(request, "application/x-www-form-urlencoded")));
        response.writeHead(200).end();
    }
  }

  const server = createServer({ requestTimeout: 15_000 }, (request, response) => {
    void handle(request, response).catch((error: unknown) => {
      if (response.headersSent || response.destroyed) return;
      const status = error instanceof RequestError ? error.status : 500;
      if (!request.complete) response.setHeader("Connection", "close");
      json(response, status, { error: status === 500 ? "internal_error" : "request_rejected" });
    });
  });
  server.on("connect", (request, socket) => {
    socket.end(request.url === "/mcp"
      ? "HTTP/1.1 405 Method Not Allowed\r\nAllow: POST\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
      : "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
  });
  try {
    server.listen(port, "127.0.0.1");
    await once(server, "listening");
  } catch (error) {
    oauth.close();
    throw error;
  }
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("Status MCP listener did not bind TCP");
  return {
    url: `http://127.0.0.1:${address.port}/mcp`,
    close: () => new Promise<void>((resolve, reject) => server.close((error) => {
      oauth.close();
      if (error) reject(error);
      else resolve();
    })),
  };
}
