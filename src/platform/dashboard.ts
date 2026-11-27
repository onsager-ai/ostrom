import { randomBytes, timingSafeEqual } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer, request } from "node:http";
import type { ServerResponse } from "node:http";

export async function startPlatformDashboard({ socketPath, port = 0 }: { socketPath: string; port?: number }) {
  const loopback = "127.0.0.1";
  const token = randomBytes(32).toString("hex");
  const expectedToken = Buffer.from(token);
  const page = await readFile(new URL("./dashboard.html", import.meta.url));
  let authority = "";
  const trusted = (host: string | undefined, origin: string | undefined) =>
    host === authority && (origin === undefined || origin === `http://${authority}`);
  const server = createServer((incoming, outgoing) => {
    outgoing.setHeader("Cache-Control", "no-store");
    outgoing.setHeader("Referrer-Policy", "no-referrer");
    outgoing.setHeader("X-Content-Type-Options", "nosniff");
    outgoing.setHeader("Content-Security-Policy",
      "default-src 'none'; connect-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'");
    const { host, origin, authorization } = incoming.headers;
    if (!trusted(host, origin)) return send(outgoing, 403, "Forbidden");
    if (incoming.method !== "GET") {
      outgoing.setHeader("Allow", "GET");
      return send(outgoing, 405, "Method not allowed");
    }
    const path = incoming.url ?? "/";
    if (path === "/" || path === "/dashboard.html") {
      outgoing.writeHead(200, { "Content-Type": "text/html; charset=utf-8" }).end(page);
      return;
    }
    if (!path.startsWith("/v1/")) return send(outgoing, 404, "Not found");
    const supplied = /^Bearer ([0-9a-f]{64})$/.exec(authorization ?? "")?.[1];
    if (supplied === undefined || !timingSafeEqual(Buffer.from(supplied), expectedToken)) {
      return send(outgoing, 401, "Unauthorized");
    }

    const upstream = request({ socketPath, method: "GET", path }, (response) => {
      outgoing.writeHead(response.statusCode ?? 502, { "Content-Type": "application/json; charset=utf-8" });
      response.once("error", () => outgoing.destroy());
      response.pipe(outgoing);
    });
    upstream.setTimeout(5_000, () => upstream.destroy(new Error("Platform API timed out")));
    upstream.once("error", () => {
      if (outgoing.headersSent) outgoing.destroy();
      else send(outgoing, 502, "Platform API unavailable");
    });
    outgoing.once("close", () => upstream.destroy());
    upstream.end();
  });
  // Node delivers CONNECT outside the ordinary request handler.
  server.on("connect", (incoming, socket) => {
    const allowedHost = trusted(incoming.headers.host, incoming.headers.origin);
    const status = allowedHost ? "405 Method Not Allowed\r\nAllow: GET" : "403 Forbidden";
    socket.end(`HTTP/1.1 ${status}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, loopback, () => {
      server.off("error", reject);
      const address = server.address();
      if (typeof address !== "object" || address === null) return reject(new Error("Dashboard listener did not bind TCP"));
      authority = `${loopback}:${address.port}`;
      resolve();
    });
  });
  return {
    url: `http://${authority}/#token=${token}`,
    close: () => new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}

function send(response: ServerResponse, status: number, text: string): void {
  response.writeHead(status, { "Content-Type": "text/plain; charset=utf-8" }).end(text);
}
