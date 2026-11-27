import { createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { closeSync, constants, lstatSync, mkdirSync, openSync } from "node:fs";
import { dirname, join } from "node:path";
import { DatabaseSync } from "node:sqlite";

import { defaultPlatformDbPath } from "./api.ts";

const scope = "status:read";
const tokenTtl = 7 * 86_400;
const secret = () => randomBytes(32).toString("base64url");
const hash = (value: string) => createHash("sha256").update(value).digest("base64url");
const secretPattern = /^[A-Za-z0-9_-]{43}$/;
const redirects = [
  "https://chatgpt.com/connector_platform_oauth_redirect",
  "https://platform.openai.com/apps-manage/oauth",
];

export function defaultStatusStateDir(env: NodeJS.ProcessEnv = process.env): string {
  return join(dirname(defaultPlatformDbPath(env)), "status-mcp");
}

export function statusPublicOrigin(raw: string): string {
  const url = URL.parse(raw);
  if (!url || url.href !== `${url.origin}/` || url.username || url.password ||
      !(url.protocol === "https:" || (url.protocol === "http:" && /^http:\/\/(?:127\.0\.0\.1|localhost)(?::\d+)?\/?$/.test(raw))) ||
      /[?#@]/.test(raw) || !/^https?:\/\/[^/]+\/?$/.test(raw)) {
    throw new Error("--public-url must be an HTTPS origin or an HTTP loopback origin");
  }
  return url.origin;
}

function openState(stateDir: string): DatabaseSync {
  mkdirSync(stateDir, { mode: 0o700, recursive: true });
  const directory = lstatSync(stateDir);
  if (!directory.isDirectory() || directory.uid !== process.getuid?.() || (directory.mode & 0o777) !== 0o700) {
    throw new Error("Status MCP state directory must be owned by the current user with mode 0700");
  }
  const path = join(stateDir, "oauth.sqlite");
  if (lstatSync(path, { throwIfNoEntry: false }) === undefined) {
    const { O_CREAT, O_EXCL, O_NOFOLLOW, O_RDWR } = constants;
    try {
      closeSync(openSync(path, O_CREAT | O_EXCL | O_NOFOLLOW | O_RDWR, 0o600));
    } catch (error) {
      // The CLI and server can initialize the same directory concurrently.
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
    }
  }
  const file = lstatSync(path);
  if (!file.isFile() || file.uid !== directory.uid || (file.mode & 0o777) !== 0o600) {
    throw new Error("Status MCP state file must be owned by the current user with mode 0600");
  }
  const db = new DatabaseSync(path);
  try {
    db.exec(`
      PRAGMA busy_timeout = 5000;
      CREATE TABLE IF NOT EXISTS signing_key (id INTEGER PRIMARY KEY CHECK (id = 1), key TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS approval (id INTEGER PRIMARY KEY CHECK (id = 1), hash TEXT NOT NULL, expires INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS token (hash TEXT PRIMARY KEY, resource TEXT NOT NULL, scope TEXT NOT NULL, expires INTEGER NOT NULL);
    `);
    return db;
  } catch (error) {
    db.close();
    throw error;
  }
}

export function mintStatusApproval(stateDir: string, ttlSec = 600): string {
  if (!Number.isInteger(ttlSec) || ttlSec < 1 || ttlSec > 600) {
    throw new Error("--ttl-sec must be an integer from 1 to 600");
  }
  const code = secret();
  const db = openState(stateDir);
  try {
    db.prepare("INSERT OR REPLACE INTO approval VALUES (1, ?, ?)").run(hash(code), Date.now() + ttlSec * 1000);
  } finally {
    db.close();
  }
  return code;
}

interface Authorization {
  client_id: string;
  redirect_uri: string;
  response_type: string;
  code_challenge: string;
  code_challenge_method: string;
  state: string;
  resource: string;
  scope: string;
}

const invalid = (error: string) => ({ status: 400, body: { error } });

function htmlAttribute(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

export function createStatusOAuth(origin: string, stateDir: string, redirectAllowlist: readonly string[] = redirects) {
  const db = openState(stateDir);
  db.prepare("INSERT OR IGNORE INTO signing_key VALUES (1, ?)").run(secret());
  const key = db.prepare("SELECT key FROM signing_key WHERE id = 1").get()?.["key"];
  if (typeof key !== "string" || !secretPattern.test(key) ||
      Buffer.from(key, "base64url").toString("base64url") !== key) {
    db.close();
    throw new Error("Invalid Status MCP signing key");
  }
  const resource = `${origin}/mcp`;
  const codes = new Map<string, { request: Authorization; expires: number }>();
  const signature = (payload: string) => createHmac("sha256", key).update(payload).digest("base64url");
  const allowedRedirect = (raw: string) => {
    const url = URL.parse(raw);
    return url?.protocol === "https:" && !url.username && !url.password && !raw.includes("#") &&
      redirectAllowlist.includes(raw);
  };
  function registered(clientId: string, uri: string): boolean {
    const [payload, mac, extra] = clientId.split(".");
    if (!payload || !mac || extra !== undefined || !secretPattern.test(mac) ||
        !timingSafeEqual(Buffer.from(mac), Buffer.from(signature(payload))) || !allowedRedirect(uri)) return false;
    const { uris } = JSON.parse(Buffer.from(payload, "base64url").toString("utf8")) as { uris: string[] };
    return Array.isArray(uris) && uris.includes(uri);
  }
  function checkAuthorization(params: URLSearchParams): Authorization | undefined {
    const request = {
      client_id: params.get("client_id") ?? "",
      redirect_uri: params.get("redirect_uri") ?? "",
      response_type: params.get("response_type") ?? "",
      code_challenge: params.get("code_challenge") ?? "",
      code_challenge_method: params.get("code_challenge_method") ?? "",
      state: params.get("state") ?? "",
      resource: params.get("resource") ?? "",
      scope: params.get("scope") ?? scope,
    };
    if (Object.keys(request).some((name) => params.getAll(name).length > 1) ||
        request.response_type !== "code" || request.code_challenge_method !== "S256" ||
        !/^[A-Za-z0-9_-]{43,128}$/.test(request.code_challenge) ||
        !request.state || request.state.length > 1024 || /[\x00-\x1f\x7f]/.test(request.state) ||
        request.resource !== resource || request.scope !== scope ||
        !registered(request.client_id, request.redirect_uri)) return undefined;
    return request;
  }
  return {
    resourceMetadata: {
      resource, authorization_servers: [origin], scopes_supported: [scope], bearer_methods_supported: ["header"],
    },
    serverMetadata: {
      issuer: origin, authorization_endpoint: `${origin}/oauth/authorize`, token_endpoint: `${origin}/oauth/token`,
      registration_endpoint: `${origin}/oauth/register`, revocation_endpoint: `${origin}/oauth/revoke`,
      response_types_supported: ["code"], grant_types_supported: ["authorization_code"],
      code_challenge_methods_supported: ["S256"], token_endpoint_auth_methods_supported: ["none"],
      revocation_endpoint_auth_methods_supported: ["none"], scopes_supported: [scope],
      authorization_response_iss_parameter_supported: true,
    },
    register(body: Record<string, unknown>) {
      const uris = body["redirect_uris"];
      if (!Array.isArray(uris) || uris.length === 0 || uris.length > redirectAllowlist.length ||
          !uris.every((uri): uri is string => typeof uri === "string" && allowedRedirect(uri))) {
        return invalid("invalid_redirect_uri");
      }
      if ((body["token_endpoint_auth_method"] !== undefined && body["token_endpoint_auth_method"] !== "none") ||
          (body["grant_types"] !== undefined && JSON.stringify(body["grant_types"]) !== '["authorization_code"]') ||
          (body["response_types"] !== undefined && JSON.stringify(body["response_types"]) !== '["code"]') ||
          (body["scope"] !== undefined && body["scope"] !== scope)) return invalid("invalid_client_metadata");
      const payload = Buffer.from(JSON.stringify({ nonce: randomBytes(12).toString("base64url"), uris })).toString("base64url");
      return { status: 201, body: {
        client_id: `${payload}.${signature(payload)}`, redirect_uris: uris, token_endpoint_auth_method: "none",
        grant_types: ["authorization_code"], response_types: ["code"], scope,
      } };
    },
    approvalPage(params: URLSearchParams) {
      const request = checkAuthorization(params);
      if (!request) return undefined;
      const fields = Object.entries(request).map(([name, value]) =>
        `<input type="hidden" name="${name}" value="${htmlAttribute(value)}">`).join("");
      return { redirect: request.redirect_uri, html: '<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width">' +
        '<title>Approve Ostrom Status access</title><h1>Approve Ostrom Status access</h1>' +
        '<p>This grants read-only access to platform status, lanes, decisions and digests.</p>' +
        `<form method="post" action="/oauth/authorize">${fields}<label>Operator approval code ` +
        '<input name="approval_code" type="password" required autocomplete="off"></label> <button>Approve</button></form></html>' };
    },
    authorize(params: URLSearchParams) {
      const request = checkAuthorization(params);
      const approval = params.get("approval_code");
      if (!request || !approval || !secretPattern.test(approval) || params.getAll("approval_code").length !== 1) return undefined;
      const now = Date.now();
      const expected = db.prepare("SELECT hash FROM approval WHERE id = 1 AND expires > ?").get(now)?.["hash"];
      const supplied = hash(approval);
      if (typeof expected !== "string" || !timingSafeEqual(Buffer.from(expected), Buffer.from(supplied)) ||
          db.prepare("DELETE FROM approval WHERE id = 1 AND hash = ? AND expires > ?").run(supplied, now).changes !== 1) {
        return undefined;
      }
      for (const [code, entry] of codes) if (entry.expires <= now) codes.delete(code);
      const code = secret();
      codes.set(code, { request, expires: now + 60_000 });
      const redirect = new URL(request.redirect_uri);
      redirect.searchParams.set("code", code);
      redirect.searchParams.set("state", request.state);
      redirect.searchParams.set("iss", origin);
      return redirect.href;
    },
    exchange(params: URLSearchParams) {
      if (params.get("grant_type") !== "authorization_code") return invalid("unsupported_grant_type");
      if ([...params.keys()].some((key) => params.getAll(key).length !== 1)) return invalid("invalid_request");
      if (params.get("resource") !== resource) return invalid("invalid_target");
      const code = params.get("code") ?? "";
      const entry = codes.get(code);
      const now = Date.now();
      const verifier = params.get("code_verifier") ?? "";
      if (!entry || entry.expires <= now || entry.request.client_id !== params.get("client_id") ||
          entry.request.redirect_uri !== params.get("redirect_uri") ||
          !/^[A-Za-z0-9._~-]{43,128}$/.test(verifier) || hash(verifier) !== entry.request.code_challenge) {
        return invalid("invalid_grant");
      }
      const token = secret();
      db.prepare("DELETE FROM token WHERE expires <= ?").run(now);
      db.prepare("INSERT INTO token VALUES (?, ?, ?, ?)").run(hash(token), resource, scope, now + tokenTtl * 1000);
      codes.delete(code);
      return { status: 200, body: { access_token: token, token_type: "Bearer", expires_in: tokenTtl, scope } };
    },
    authenticate(header: string | undefined): string | undefined {
      const token = /^Bearer ([A-Za-z0-9_-]{43})$/i.exec(header ?? "")?.[1];
      if (!token) return undefined;
      const digest = hash(token);
      return db.prepare("SELECT 1 FROM token WHERE hash = ? AND resource = ? AND scope = ? AND expires > ?")
        .get(digest, resource, scope, Date.now()) ? digest : undefined;
    },
    revoke(params: URLSearchParams) {
      const token = params.get("token");
      if (token && secretPattern.test(token)) {
        db.prepare("DELETE FROM token WHERE hash = ? AND resource = ?").run(hash(token), resource);
      }
    },
    close: () => db.close(),
  };
}
