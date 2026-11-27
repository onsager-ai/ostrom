import { createPublicKey, randomBytes, timingSafeEqual, verify } from "node:crypto";
import type { KeyObject } from "node:crypto";
import { closeSync, constants, fstatSync, lstatSync, openSync, readFileSync } from "node:fs";
import type { Stats } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer, request } from "node:http";
import type { ServerResponse } from "node:http";

interface CloudflareAccessOptions {
  teamDomain: string;
  aud: string;
  fetchJwks?: (url: string) => Promise<unknown>;
  now?: () => number;
}

export async function startPlatformDashboard({ socketPath, port = 0, publicUrl, tokenFile, cfAccess }: {
  socketPath: string;
  port?: number;
  publicUrl?: string;
  tokenFile?: string;
  cfAccess?: CloudflareAccessOptions;
}) {
  const loopback = "127.0.0.1";
  if (tokenFile !== undefined && publicUrl === undefined) {
    throw new Error("Dashboard token file requires a public URL");
  }
  const publicOrigin = publicUrl === undefined ? undefined : httpsOrigin(publicUrl, "Dashboard public URL");
  if (cfAccess !== undefined && publicOrigin === undefined) {
    throw new Error("Dashboard Cloudflare Access requires a public URL");
  }
  if (publicOrigin !== undefined && tokenFile === undefined && cfAccess === undefined) {
    throw new Error("Dashboard public URL requires a token file or Cloudflare Access");
  }
  const verifyAccess = cfAccess === undefined ? undefined : cloudflareAccessVerifier(cfAccess);
  const token = tokenFile === undefined && publicOrigin === undefined ? randomBytes(32).toString("hex")
    : tokenFile === undefined ? undefined : readTokenFile(tokenFile);
  const expectedToken = token === undefined ? undefined : Buffer.from(token);
  const authorized = (authorization: string | undefined) => {
    const supplied = /^Bearer ([0-9a-f]{64})$/.exec(authorization ?? "")?.[1];
    return supplied !== undefined && expectedToken !== undefined && timingSafeEqual(Buffer.from(supplied), expectedToken);
  };
  let page = await readFile(new URL("./dashboard.html", import.meta.url));
  if (publicOrigin !== undefined) {
    const html = page.toString("utf8");
    const root = '<html lang="en">';
    if (!html.includes(root)) throw new Error("Dashboard HTML is missing its expected root element");
    page = Buffer.from(html.replace(root, '<html lang="en" data-auth="proxy">'));
  }
  let authority = "";
  const trusted = (host: string | undefined, origin: string | undefined) =>
    host === (publicOrigin?.host ?? authority) && (origin === undefined || origin === (publicOrigin?.origin ?? `http://${authority}`));
  const server = createServer(async (incoming, outgoing) => {
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
    if (publicOrigin !== undefined && tokenFile !== undefined && !authorized(authorization)) return send(outgoing, 401, "Unauthorized");
    if (verifyAccess !== undefined && !await verifyAccess(incoming.headersDistinct["cf-access-jwt-assertion"])) {
      return send(outgoing, 403, "Forbidden");
    }
    const path = incoming.url ?? "/";
    if (path === "/" || path === "/dashboard.html") {
      outgoing.writeHead(200, { "Content-Type": "text/html; charset=utf-8" }).end(page);
      return;
    }
    if (!path.startsWith("/v1/")) return send(outgoing, 404, "Not found");
    if (publicOrigin === undefined && !authorized(authorization)) {
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
    url: publicOrigin === undefined ? `http://${authority}/#token=${token}` : `http://${authority}/`,
    close: () => new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}

function httpsOrigin(raw: string, setting: string): URL {
  const url = URL.parse(raw);
  if (!url || url.protocol !== "https:" || url.href !== `${url.origin}/` ||
      !/^https:\/\/[^/?#@\\\s\p{Cc}]+\/?$/u.test(raw)) {
    throw new Error(`${setting} must be an HTTPS origin`);
  }
  return url;
}

function readTokenFile(path: string): string {
  const check = (stat: Stats) => {
    if (!stat.isFile() || stat.uid !== process.getuid?.() || (stat.mode & 0o777) !== 0o600) {
      throw new Error("Dashboard token file must be a regular file owned by the current user with mode 0600");
    }
  };
  const stat = lstatSync(path, { throwIfNoEntry: false });
  if (stat === undefined) throw new Error("Dashboard token file does not exist");
  check(stat);
  // Recheck the opened file so a replacement between lstat and open cannot bypass validation.
  const fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    check(fstatSync(fd));
    const token = readFileSync(fd, "utf8").trim();
    if (!/^[0-9a-f]{64}$/.test(token)) throw new Error("Dashboard token file must contain 64 lowercase hexadecimal characters");
    return token;
  } finally {
    closeSync(fd);
  }
}

async function fetchAccessJwks(url: string): Promise<unknown> {
  const response = await fetch(url, { signal: AbortSignal.timeout(5_000), cache: "no-store", redirect: "error" });
  if (!response.ok) throw new Error("Could not fetch Cloudflare Access keys");
  return response.json();
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function decodeBase64url(segment: string): Buffer {
  const decoded = Buffer.from(segment, "base64url");
  if (decoded.toString("base64url") !== segment) throw new Error("Invalid JWT encoding");
  return decoded;
}

function cloudflareAccessVerifier({ teamDomain, aud, fetchJwks = fetchAccessJwks, now = Date.now }: CloudflareAccessOptions) {
  const origin = /^(https:\/\/[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.cloudflareaccess\.com)\/?$/.exec(teamDomain)?.[1];
  if (origin === undefined) {
    throw new Error("Cloudflare Access team domain must be an HTTPS origin for a team on cloudflareaccess.com");
  }
  if (typeof aud !== "string" || !aud || aud !== aud.trim()) {
    throw new Error("Cloudflare Access audience must be a non-empty trimmed string");
  }

  let keys = new Map<string, KeyObject>();
  let expiresAt = -Infinity;
  let nextRefreshAt = -Infinity;
  let refreshing: Promise<void> | undefined;
  async function refreshKeys(): Promise<void> {
    const jwks = await fetchJwks(`${origin}/cdn-cgi/access/certs`);
    if (!isRecord(jwks) || !Array.isArray(jwks.keys)) throw new Error("Invalid Cloudflare Access key set");
    const updated = new Map<string, KeyObject>();
    for (const jwk of jwks.keys as unknown[]) {
      if (!isRecord(jwk) || jwk.kty !== "RSA" || typeof jwk.kid !== "string" || !jwk.kid ||
          typeof jwk.n !== "string" || !jwk.n || typeof jwk.e !== "string" || !jwk.e) continue;
      try {
        decodeBase64url(jwk.n);
        decodeBase64url(jwk.e);
        updated.set(jwk.kid, createPublicKey({ key: { kty: "RSA", n: jwk.n, e: jwk.e }, format: "jwk" }));
      } catch {
        // A malformed entry does not invalidate other keys in the set.
      }
    }
    keys = updated;
    expiresAt = now() + 600_000;
  }
  async function findKey(kid: string): Promise<KeyObject | undefined> {
    const time = now();
    const cached = time < expiresAt ? keys.get(kid) : undefined;
    if (cached !== undefined) return cached;
    if (refreshing === undefined && time >= nextRefreshAt) {
      // Count attempts, including cold starts and failures, toward the refresh limit.
      nextRefreshAt = time + 60_000;
      refreshing = refreshKeys().finally(() => { refreshing = undefined; });
    }
    await refreshing;
    return now() < expiresAt ? keys.get(kid) : undefined;
  }

  const decoder = new TextDecoder("utf-8", { fatal: true });
  return async (assertions: string[] | undefined): Promise<boolean> => {
    if (assertions?.length !== 1 || typeof assertions[0] !== "string") return false;
    const [encodedHeader, encodedPayload, encodedSignature, extra] = assertions[0].split(".");
    if (!encodedHeader || !encodedPayload || !encodedSignature || extra !== undefined) return false;
    try {
      const header: unknown = JSON.parse(decoder.decode(decodeBase64url(encodedHeader)));
      if (!isRecord(header) || header.alg !== "RS256" || typeof header.kid !== "string" || !header.kid) return false;
      const payload: unknown = JSON.parse(decoder.decode(decodeBase64url(encodedPayload)));
      if (!isRecord(payload) || payload.iss !== origin) return false;
      const { aud: audience, exp, nbf } = payload;
      if (audience !== aud && !(Array.isArray(audience) && audience.every((entry) => typeof entry === "string") &&
          audience.includes(aud))) return false;
      const nowSec = now() / 1000;
      if (typeof exp !== "number" || !Number.isFinite(exp) || !(nowSec < exp)) return false;
      if (nbf !== undefined && (typeof nbf !== "number" || !Number.isFinite(nbf) || !(nowSec + 60 >= nbf))) return false;
      const signature = decodeBase64url(encodedSignature);
      const key = await findKey(header.kid);
      if (key === undefined || now() / 1000 >= exp) return false;
      return verify("RSA-SHA256", Buffer.from(`${encodedHeader}.${encodedPayload}`), key, signature);
    } catch {
      return false;
    }
  };
}

function send(response: ServerResponse, status: number, text: string): void {
  response.writeHead(status, { "Content-Type": "text/plain; charset=utf-8" }).end(text);
}
