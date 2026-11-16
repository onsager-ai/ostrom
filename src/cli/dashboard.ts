import { randomBytes, timingSafeEqual } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer, request, type Server } from "node:http";
import { defaultSocketPath } from "../api/server.ts";

const PAGE = new URL("./dashboard.html", import.meta.url);
const ALLOWED = new Set(["/v1/status", "/v1/tickets", "/v1/intake?limit=50"]);

export interface DashboardServer {
  url: string;
  close(): Promise<void>;
}

export async function startDashboard(env: Record<string, string | undefined> = process.env): Promise<DashboardServer> {
  const token = randomBytes(32).toString("hex");
  const socketPath = defaultSocketPath(env);
  let boundHost = "";
  const server = createServer(async (incoming, outgoing) => {
    outgoing.setHeader("Cache-Control", "no-store");
    outgoing.setHeader("Referrer-Policy", "no-referrer");
    const host = incoming.headers.host;
    const origin = incoming.headers.origin;
    if (host !== boundHost || (origin !== undefined && origin !== `http://${boundHost}`)) return send(outgoing, 403, "Forbidden");
    const pathname = incoming.url ?? "/";
    if (incoming.method === "GET" && (pathname === "/" || pathname === "/dashboard.html")) {
      try {
        outgoing.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        outgoing.end(await readFile(PAGE));
      } catch {
        send(outgoing, 500, "Dashboard page unavailable");
      }
      return;
    }
    if (incoming.method !== "GET" || !(ALLOWED.has(pathname) || /^\/v1\/tickets\/[1-9]\d*$/.test(pathname))) return send(outgoing, 404, "Not found");
    const authorization = incoming.headers.authorization ?? "";
    const supplied = /^Bearer ([0-9a-f]{64})$/.exec(authorization)?.[1] ?? "";
    if (supplied.length !== token.length || !timingSafeEqual(Buffer.from(supplied), Buffer.from(token))) return send(outgoing, 401, "Unauthorized");
    const upstream = request({ socketPath, path: pathname, method: "GET", headers: { host: "ostromd", authorization: `Bearer ${token}` } }, (response) => {
      outgoing.writeHead(response.statusCode ?? 502, { "content-type": response.headers["content-type"] ?? "application/json" });
      response.pipe(outgoing);
    });
    upstream.once("error", () => send(outgoing, 502, JSON.stringify({ error: "ostromd is unavailable" })));
    upstream.end();
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", reject);
      const address = server.address();
      if (address === null || typeof address === "string") return reject(new Error("dashboard listener did not bind TCP"));
      boundHost = `127.0.0.1:${address.port}`;
      resolve();
    });
  });
  return {
    url: `http://${boundHost}/#${token}`,
    close: () => new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}

function send(response: import("node:http").ServerResponse, status: number, body: string): void {
  response.writeHead(status, { "content-type": "text/plain; charset=utf-8" });
  response.end(body);
}
