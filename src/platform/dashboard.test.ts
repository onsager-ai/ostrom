import assert from "node:assert/strict";
import { generateKeyPairSync, sign } from "node:crypto";
import { chmod, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { request } from "node:http";
import type { OutgoingHttpHeaders } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { TestContext } from "node:test";
import { runInNewContext } from "node:vm";

import { startPlatformApiServer } from "./api.ts";
import { startPlatformDashboard } from "./dashboard.ts";
import { PlatformStore } from "./store.ts";

function read(base: URL, path: string, headers: OutgoingHttpHeaders = {}, method = "GET") {
  return new Promise<{ status: number | undefined; headers: OutgoingHttpHeaders; body: string }>((resolve, reject) => {
    const req = request(base, { path, headers, method }, (response) => {
      let body = "";
      response.setEncoding("utf8").on("data", (chunk: string) => { body += chunk; });
      response.on("error", reject);
      response.on("end", () => resolve({ status: response.statusCode, headers: response.headers, body }));
    });
    req.on("connect", (response, socket) => {
      socket.destroy();
      resolve({ status: response.statusCode, headers: response.headers, body: "" });
    });
    req.on("error", reject);
    req.end();
  });
}

test("serves the loopback page and proxies authenticated GETs only", async () => {
  const directory = await mkdtemp(join(tmpdir(), "ostrom-platform-dashboard-"));
  const socketPath = join(directory, "platform.sock");
  const store = PlatformStore.open(":memory:");
  const api = await startPlatformApiServer({ store, socketPath });
  try {
    const lane = store.createLane({ slug: "platform", name: "Platform" });
    store.upsertT3HostState({ hostId: "east", label: "<script>invalid()</script>", mcpUrl: "https://t3.example/mcp",
      originNamespace: null, connectionState: "connected" });
    const dashboard = await startPlatformDashboard({ socketPath, port: 0 });
    try {
      const url = new URL(dashboard.url);
      assert.equal(url.hostname, "127.0.0.1");
      const token = new URLSearchParams(url.hash.slice(1)).get("token");
      assert.match(token ?? "", /^[0-9a-f]{64}$/);
      const headers = { authorization: "Bearer " + token };
      const page = await read(url, "/");
      assert.equal(page.status, 200);
      assert.equal(page.body, await readFile(new URL("./dashboard.html", import.meta.url), "utf8"));
      assert.equal((await read(url, "/dashboard.html")).body, page.body);
      assert.equal(page.headers["cache-control"], "no-store");
      assert.equal(page.headers["referrer-policy"], "no-referrer");
      assert.equal(page.headers["content-security-policy"],
        "default-src 'none'; connect-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'");
      assert.doesNotMatch(page.body, /<(?:link|img)\b|<script\b[^>]*\bsrc\s*=/i);
      assert.doesNotMatch(page.body, /\binnerHTML\b/);
      assert.doesNotMatch(page.body, /https?:\/\/[\w.-]+\.[a-z]{2,}|\/home\//i);
      const fontSizes = [...page.body.matchAll(/\bfont-size:\s*([^;}]*)/g)];
      assert(fontSizes.length > 0);
      for (const [, size] of fontSizes) {
        assert.match(size ?? "", /^\d+px\s*$/);
        assert(Number.parseInt(size!, 10) >= 15, `text size ${size} is too small`);
      }
      assert.match(page.body, /aria-label="Theme"/);
      for (const theme of ["auto", "light", "dark"]) assert(page.body.includes(`data-theme="${theme}"`));
      assert(!page.body.includes("<script>invalid()</script>"));

      const previewHtml = await readFile(new URL("./dashboard-v2.html", import.meta.url), "utf8");
      for (const path of ["/preview", "/preview/", "/preview?style=a", "/preview?style=b"]) {
        const preview = await read(url, path);
        assert.equal(preview.status, 200);
        assert.equal(preview.body, previewHtml);
        for (const header of ["content-type", "content-security-policy", "cache-control", "referrer-policy", "x-content-type-options"]) {
          assert.equal(preview.headers[header], page.headers[header]);
        }
        assert.equal((await read(url, path, { origin: "https://foreign.example" })).status, 403);
        assert.equal((await read(url, path, {}, "POST")).status, 405);
      }

      assert.equal((await read(url, "/?style=b")).status, 404);
      assert.equal((await read(url, "/previews?style=b")).status, 404);
      assert.equal((await read(url, "/v1/hosts")).status, 401);
      assert.equal((await read(url, "/v1/unknown")).status, 401);
      assert.equal((await read(url, "/v1/hosts", { authorization: "Bearer " + "0".repeat(64) })).status, 401);
      assert.equal((await read(url, "/v1/hosts", { ...headers, host: "localhost:" + url.port })).status, 403);
      assert.equal((await read(url, "/v1/hosts", { ...headers, origin: "https://foreign.example" })).status, 403);
      assert.equal((await read(url, "/v1/decisions", headers, "POST")).status, 405);
      assert.equal((await read(url, "/v1/spend/rollups", headers, "POST")).status, 405);
      assert.equal((await read(url, "/v1/lanes/platform/goal/self", headers, "POST")).status, 405);
      const detail = await read(url, "/v1/lanes/platform/detail", headers);
      assert.equal(detail.status, 200);
      assert.deepEqual(JSON.parse(detail.body).lane, lane);
      assert.equal((await read(url, "/v1/lanes/platform/detail")).status, 401);
      assert.equal((await read(url, "/v1/lanes/platform/detail", headers, "PUT")).status, 405);
      assert.equal((await read(url, "/v1/goals/audit?lane=platform", headers)).status, 404);
      assert.equal((await read(url, "/", {}, "HEAD")).status, 405);
      assert.equal((await read(url, "/v1/budgets", headers, "PUT")).status, 405);
      assert.equal((await read(url, "/v1/lanes/platform", headers, "PUT")).status, 405);
      for (const path of ["/v1/budgets", "/v1/github/prs", "/v1/spend/summary", "/v1/goals"]) {
        assert.equal((await read(url, path, headers)).status, 200);
      }
      const hosts = await read(url, "/v1/hosts", { ...headers, origin: url.origin });
      assert.equal(hosts.status, 200);
      assert.equal(hosts.headers["cache-control"], "no-store");
      assert.deepEqual(JSON.parse(hosts.body), store.listT3HostStates());
      const digest = await read(url, "/v1/digest?window=13:00Z", headers);
      assert.equal(digest.status, 200);
      assert.equal(JSON.parse(digest.body).window, "13:00Z");
      assert.match(JSON.parse(digest.body).markdown, /Spend: unavailable/);
      assert.equal((await read(url, "/v1/digest?window=bad", headers)).status, 400);
    } finally {
      await dashboard.close();
    }
  } finally {
    await api.close();
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

const publicUrl = "https://dashboard.example";
const teamDomain = "https://team.cloudflareaccess.com";
const aud = "dashboard-audience";
const dashboardToken = "a1".repeat(32);
const proxyHeaders = { host: "dashboard.example", authorization: `Bearer ${dashboardToken}` };

async function publicOptions(t: TestContext) {
  const directory = await mkdtemp(join(tmpdir(), "ostrom-public-dashboard-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const tokenFile = join(directory, "dashboard.token");
  await writeFile(tokenFile, `${dashboardToken}\n`, { mode: 0o600 });
  return { publicUrl, tokenFile, socketPath: join(directory, "api.sock") };
}

test("rejects invalid public dashboard configuration and token files", async (t) => {
  const options = await publicOptions(t);
  const { socketPath, tokenFile } = options;
  await assert.rejects(startPlatformDashboard({ socketPath, publicUrl }), /requires a token file or Cloudflare Access/);
  await assert.rejects(startPlatformDashboard({ socketPath, tokenFile }), /requires a public URL/);
  await assert.rejects(startPlatformDashboard({ socketPath, cfAccess: { teamDomain, aud } }), /requires a public URL/);
  for (const invalid of ["http://dashboard.example", `${publicUrl}/path`, `${publicUrl}/path/..`,
    `${publicUrl}?`, `${publicUrl}#`, "https://@dashboard.example"]) {
    await assert.rejects(startPlatformDashboard({ ...options, publicUrl: invalid }), /public URL must be an HTTPS origin/);
  }
  for (const invalid of [publicUrl, `${teamDomain}/path`, `${teamDomain}:443`, "https://TEAM.cloudflareaccess.com"]) {
    await assert.rejects(startPlatformDashboard({ ...options, cfAccess: { teamDomain: invalid, aud } }), /team domain/);
  }
  for (const invalid of ["", " ", ` ${aud}`]) {
    await assert.rejects(startPlatformDashboard({ ...options, cfAccess: { teamDomain, aud: invalid } }), /audience/);
  }
  const link = `${tokenFile}.link`;
  await symlink(tokenFile, link);
  await assert.rejects(startPlatformDashboard({ ...options, tokenFile: link }), /regular file.*0600/);
  await chmod(tokenFile, 0o644);
  await assert.rejects(startPlatformDashboard(options), /regular file.*0600/);
  await chmod(tokenFile, 0o600);
  for (const invalid of ["a".repeat(63), "A".repeat(64), "g".repeat(64)]) {
    await writeFile(tokenFile, invalid);
    await assert.rejects(startPlatformDashboard(options), { message: "Dashboard token file must contain 64 lowercase hexadecimal characters" });
  }
});

test("public dashboard accepts Cloudflare Access alone without a bearer token", async (t) => {
  const keyPair = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const jwk = { ...keyPair.publicKey.export({ format: "jwk" }), kid: "dashboard" };
  const nowSec = Date.now() / 1000;
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const input = `${encode({ alg: "RS256", kid: "dashboard" })}.${encode({ iss: teamDomain, aud, exp: nowSec + 3600 })}`;
  const assertion = `${input}.${sign("RSA-SHA256", Buffer.from(input), keyPair.privateKey).toString("base64url")}`;
  const options = await publicOptions(t);
  const store = PlatformStore.open(":memory:");
  t.after(() => store.close());
  const api = await startPlatformApiServer({ store, socketPath: options.socketPath });
  t.after(() => api.close());
  const dashboard = await startPlatformDashboard({
    socketPath: options.socketPath,
    publicUrl: options.publicUrl,
    cfAccess: { teamDomain, aud, async fetchJwks() { return { keys: [jwk] }; } },
  });
  t.after(() => dashboard.close());
  assert.match(dashboard.url, /^http:\/\/127\.0\.0\.1:\d+\/$/);
  const url = new URL(dashboard.url);
  const headers = { host: "dashboard.example", "cf-access-jwt-assertion": assertion };
  for (const path of ["/", "/preview", "/preview/", "/preview?style=a", "/preview?style=b", "/v1/hosts"]) {
    assert.equal((await read(url, path, { host: headers.host })).status, 403);
    assert.equal((await read(url, path, { ...headers, "cf-access-jwt-assertion": "invalid" })).status, 403);
  }
  const page = await read(url, "/", headers);
  assert.equal(page.status, 200);
  assert.match(page.body, /<html lang="en" data-auth="proxy">/);
  for (const path of ["/preview", "/preview/", "/preview?style=a", "/preview?style=b"]) {
    const preview = await read(url, path, headers);
    assert.equal(preview.status, 200);
    assert.match(preview.body, /<html lang="en" data-auth="proxy">/);
    assert.match(preview.body, /<title>Ostrom Preview<\/title>/);
    assert.equal((await read(url, path, { ...headers, host: url.host })).status, 403);
    assert.equal((await read(url, path, headers, "POST")).status, 405);
  }
  const hosts = await read(url, "/v1/hosts", headers);
  assert.equal(hosts.status, 200);
  assert.deepEqual(JSON.parse(hosts.body), []);
  assert.equal((await read(url, "/", { ...headers, host: url.host })).status, 403);
  assert.equal((await read(url, "/", { ...headers, origin: `${publicUrl}/` })).status, 403);
  assert.equal((await read(url, "/v1/hosts", headers, "POST")).status, 405);
});

test("public dashboard enforces proxy headers and keeps the bearer token out of the browser", async (t) => {
  const options = await publicOptions(t);
  const store = PlatformStore.open(":memory:");
  t.after(() => store.close());
  const api = await startPlatformApiServer({ store, socketPath: options.socketPath });
  t.after(() => api.close());
  const dashboard = await startPlatformDashboard(options);
  t.after(() => dashboard.close());
  const url = new URL(dashboard.url);
  assert.match(dashboard.url, /^http:\/\/127\.0\.0\.1:\d+\/$/);
  assert.equal((await read(url, "/", { ...proxyHeaders, host: url.host })).status, 403);
  assert.equal((await read(url, "/", { ...proxyHeaders, host: "dashboard.example:443" })).status, 403);
  assert.equal((await read(url, "/", { ...proxyHeaders, origin: `${publicUrl}/` })).status, 403);
  const post = await read(url, "/v1/hosts", { host: proxyHeaders.host }, "POST");
  assert.equal(post.status, 405);
  assert.equal(post.headers.allow, "GET");
  const tunnel = await read(url, "dashboard.example:443", proxyHeaders, "CONNECT");
  assert.equal(tunnel.status, 405);
  assert.equal(tunnel.headers.allow, "GET");
  assert.equal((await read(url, "dashboard.example:443", { host: url.host }, "CONNECT")).status, 403);
  for (const path of ["/", "/dashboard.html", "/preview", "/preview/", "/preview?style=a", "/preview?style=b", "/v1/hosts", "/unknown"]) {
    assert.equal((await read(url, path, { host: proxyHeaders.host })).status, 401);
    assert.equal((await read(url, path, { ...proxyHeaders, authorization: `Bearer ${"0".repeat(64)}` })).status, 401);
  }
  const hosts = await read(url, "/v1/hosts", { ...proxyHeaders, origin: publicUrl });
  assert.equal(hosts.status, 200);
  assert.deepEqual(JSON.parse(hosts.body), []);
  const page = await read(url, "/", proxyHeaders);
  assert.equal(page.status, 200);
  assert.match(page.body, /<html lang="en" data-auth="proxy">/);
  assert(!page.body.includes(dashboardToken));
  assert.equal((await read(url, "/dashboard.html", proxyHeaders)).body, page.body);
  for (const path of ["/preview", "/preview/", "/preview?style=a", "/preview?style=b"]) {
    const preview = await read(url, path, proxyHeaders);
    assert.equal(preview.status, 200);
    assert.match(preview.body, /<html lang="en" data-auth="proxy">/);
    assert.match(preview.body, /<title>Ostrom Preview<\/title>/);
    assert(!preview.body.includes(dashboardToken));
  }

  const script = /<script type="module">([\s\S]*?)<\/script>/.exec(page.body)?.[1];
  assert(script);
  const windowSelect: { value: string; onchange?: () => void } = { value: "13:00Z" };
  const calls: string[] = [];
  const authorizationError = runInNewContext(`${script}\nread("/v1/hosts");`, {
    location: { hash: "#system", pathname: "/", search: "" },
    history: { replaceState() { assert.fail("proxy navigation hash must survive startup"); } },
    localStorage: { getItem: () => null },
    document: {
      documentElement: { dataset: { auth: "proxy" } },
      getElementById: (id: string) => id === "window" ? windowSelect : {},
      querySelectorAll: () => [],
    },
    window: { addEventListener() {} },
    AbortSignal, clearTimeout() {}, setTimeout() {}, setInterval() {},
    async fetch(path: string, options: RequestInit) {
      assert.equal(new Headers(options.headers).has("authorization"), false);
      calls.push(path);
      return { ok: false, status: path === "/v1/hosts" ? 401 : 403 };
    },
  });
  await assert.rejects(authorizationError, { message: "Dashboard authorization failed." });
  assert.deepEqual(calls, ["/v1/digest?window=13%3A00Z", "/v1/health/summary", "/v1/friction?status=open", "/v1/prs", "/v1/hosts"]);
  assert(windowSelect.onchange);
  windowSelect.value = "weekly";
  windowSelect.onchange();
  assert.deepEqual(calls.slice(5), ["/v1/digest?window=weekly", "/v1/health/summary", "/v1/friction?status=open", "/v1/prs"]);
});

test("public dashboard verifies Access JWTs and refreshes its key cache at a bounded rate", async (t) => {
  const first = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const second = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const firstJwk = { ...first.publicKey.export({ format: "jwk" }), kid: "first" };
  const secondJwk = { ...second.publicKey.export({ format: "jwk" }), kid: "second" };
  let now = Date.parse("2026-11-27T13:00:00Z");
  const nowSec = now / 1000;
  let jwks: unknown = { keys: [null, { kid: "bad", kty: "RSA" }, firstJwk] };
  let fetchCount = 0;
  let failFetch = false;
  const dashboard = await startPlatformDashboard({
    ...await publicOptions(t),
    cfAccess: {
      teamDomain, aud, now: () => now,
      async fetchJwks(url) {
        fetchCount++;
        assert.equal(url, `${teamDomain}/cdn-cgi/access/certs`);
        if (failFetch) throw new Error("Network unavailable");
        return jwks;
      },
    },
  });
  t.after(() => dashboard.close());
  const url = new URL(dashboard.url);
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const jwt = (claims: Record<string, unknown> = {}, header: Record<string, unknown> = {}, key = first.privateKey) => {
    const input = `${encode({ alg: "RS256", kid: "first", ...header })}.${encode({ iss: teamDomain, aud, exp: nowSec + 3600, ...claims })}`;
    return `${input}.${sign("RSA-SHA256", Buffer.from(input), key).toString("base64url")}`;
  };
  const withJwt = (assertion: string | string[], path = "/") =>
    read(url, path, { ...proxyHeaders, "cf-access-jwt-assertion": assertion });

  for (const path of ["/", "/dashboard.html", "/v1/hosts", "/unknown"]) {
    assert.equal((await read(url, path, proxyHeaders)).status, 403);
  }
  const valid = jwt();
  const invalid = [
    valid + "=", "broken.jwt", "eA.eA.eA", jwt({}, { kid: "" }),
    ...["none", "HS256", "RS512"].map((alg) => jwt({}, { alg })),
    jwt({}, {}, second.privateKey), jwt({ iss: `${teamDomain}/` }), jwt({ aud: "other" }),
    jwt({ aud: [aud, 1] }), jwt({ exp: nowSec }), jwt({ exp: String(nowSec + 3600) }), jwt({ nbf: nowSec + 61 }),
  ];
  for (const assertion of invalid) assert.equal((await withJwt(assertion)).status, 403);
  assert.equal((await withJwt([valid, valid])).status, 403);
  assert.equal((await withJwt(valid)).status, 200);
  assert.equal((await withJwt(jwt({ aud: ["other", aud], nbf: nowSec + 60 }), "/dashboard.html")).status, 200);
  assert.equal((await withJwt(valid, "/unknown")).status, 404);
  assert.equal(fetchCount, 1);

  const rotated = jwt({}, { kid: "second" }, second.privateKey);
  jwks = { keys: [secondJwk] };
  assert.equal((await withJwt(rotated)).status, 403);
  assert.equal(fetchCount, 1);
  now += 60_000;
  const responses = await Promise.all([withJwt(rotated), withJwt(rotated)]);
  assert.deepEqual(responses.map(({ status }) => status), [200, 200]);
  assert.equal((await withJwt(valid)).status, 403);
  assert.equal(fetchCount, 2);
  now += 599_999;
  assert.equal((await withJwt(rotated)).status, 200);
  assert.equal(fetchCount, 2);
  now += 1;
  failFetch = true;
  assert.equal((await withJwt(rotated)).status, 403);
  assert.equal((await withJwt(rotated)).status, 403);
  assert.equal(fetchCount, 3);
  now += 60_000;
  failFetch = false;
  jwks = { keys: null };
  assert.equal((await withJwt(rotated)).status, 403);
  assert.equal(fetchCount, 4);
});

async function dashboardScript(file = "./dashboard.html"): Promise<string> {
  const html = await readFile(new URL(file, import.meta.url), "utf8");
  const script = /<script type="module">([\s\S]*?)<\/script>/.exec(html)?.[1];
  assert(script);
  return script;
}

test("Lane Board caps exceptions, keeps lane summaries current, and preserves drill-down state", async () => {
  const script = await dashboardScript();
  runInNewContext(String.raw`
    function mockElement(tag) {
      return {
        tag, dataset: {}, children: [], open: false,
        append(...nodes) { this.children.push(...nodes); },
        replaceChildren(...nodes) { this.children = nodes; },
        querySelector() { return descendants(this).find((node) => node.tag === "details"); },
        get firstChild() { return this.children[0]; },
        setAttribute(name, value) { this[name] = value; },
      };
    }
    function descendants(node) {
      return node.children.flatMap((child) => [child, ...descendants(child)]);
    }
    function textIn(node) {
      return [node, ...descendants(node)].map((child) => child.textContent).filter(Boolean).join(" ");
    }
    const ids = new Map();
    const document = {
      documentElement: { dataset: {} }, createElement: mockElement, querySelectorAll: () => [],
      getElementById(id) {
        if (!ids.has(id)) ids.set(id, mockElement("div"));
        return ids.get(id);
      },
    };
    let navigate;
    const window = { addEventListener: (event, handler) => { navigate = handler; }, scrollTo() {} };
    ${script}
    Date.now = () => Date.parse("2026-11-27T13:00:00.000Z");
    assert.equal(relativeTime("2026-11-27T12:59:42.000Z"), "18s ago");
    assert.equal(relativeTime("2026-11-27T12:57:00.000Z"), "3 min ago");
    assert.equal(relativeExpiry("2026-12-27T13:00:00.000Z"), "expires in 30d");
    assert.equal(relativeExpiry("2026-11-25T13:00:00.000Z"), "expired 2d ago");
    assert.equal(relativeTime("invalid date"), "unknown time");
    assert.equal(relativeExpiry("invalid date"), "expiry unknown");

    const goal = { statement: "Ship the release", weekStart: "2026-11-23", metricName: "Releases",
      metricTarget: 1, approvedDecisionId: "approval", completedAt: "2026-11-27T12:00:00.000Z", weeklyPlan: {
        nextStep: "Review patch", next: "Alternative", blockers: ["Get test data"], items: ["Review patch", "Ship patch"],
      } };
    const lane = { id: "release", slug: "crawlab", name: "Release", status: "active", ownerAgentId: "release-manager", priorityRank: 1, goal };
    const staleLane = { id: "stale", slug: "stale", name: "Stale Lane", status: "active", ownerAgentId: "stale-manager", priorityRank: 2 };
    const idleLane = { id: "idle", slug: "idle", name: "Quiet Lane", status: "active", ownerAgentId: "idle-manager", priorityRank: 5 };
    const pausedLane = { id: "paused", slug: "paused", name: "Paused Lane", status: "paused",
      ownerAgentId: "paused-manager", priorityRank: 6 };
    const decision = (title, urgency, day, laneId) => ({
      title, urgency, createdAt: "2026-11-" + day + "T00:00:00Z", laneId, recommendation: "Proceed",
    });
    const alert = (summary, ruleCode, day, detail = {}) => ({
      summary, ruleCode, createdAt: "2026-11-" + day + "T00:00:00Z", detail,
    });
    const decisions = [
      decision("Digest request", "digest", 28), decision("Incident older", "incident", 23, lane.id),
      decision("Block today", "blocking_today", 26), decision("Incident newest", "incident", 24),
    ];
    const laneAlert = alert("CI failed twice", "two_failure_stop", 24, { repository: "org/crawlab-pro" });
    const alerts = [
      alert("CI warning", "pr_green_ci", 29), laneAlert,
      { ...alert("Stale Lane goal stale (no progress for 24h+)", "goal_stale", 28), laneId: staleLane.id },
      alert("Routing exception", "authority_routing", 25), alert("Budget exceeded", "budget_exceeded", 23),
      alert("Budget warning", "budget_cap", 30, { level: "warning" }),
      alert("Draft CI failed", "pr_green_ci", 29, { repository: "Org/Idle", number: 6, isDraft: false }),
    ];
    const pr = (number, rollupState) => ({ repository: "org/crawlab-pro", number, rollupState });
    const prChecks = [pr(1, "success"), pr(2, "pending"), pr(3, "error"), pr(4, "closed"),
      { repository: "org/idle", number: 6, isDraft: true, rollupState: "failure" },
      { ...pr(7, "failure"), isDraft: true }];
    const linkedPrs = [
      { repository: "org/crawlab-pro", number: 5, prHost: "github.com", state: "merged", title: "Merged work" },
      { repository: "org/idle", number: 6, prHost: "github.com", state: "open", isDraft: false, title: "New work" },
    ];
    const host = { hostId: "east", webUrl: "https://t3.example", environmentId: "env" };
    const manager = { id: lane.ownerAgentId, label: "Release Manager", hostId: host.hostId, externalRef: "env/thread", status: "active" };
    const staleManager = { ...manager, id: staleLane.ownerAgentId, status: "idle" };
    const idleManager = { ...manager, id: idleLane.ownerAgentId, status: "idle" };
    const health = { pendingOwnerDecisions: decisions, policyAlerts: alerts, hosts: [host], agents: [manager, staleManager, idleManager],
      activeRuns: [], stalledRuns: [], waits: [], githubPrs: prChecks, budgets: [], spend: { status: "unavailable" } };
    const boardData = { digest: { sections: { lanes: [idleLane, lane, staleLane, pausedLane] } }, health, friction: [], linkedPrs };
    currentContext = context(boardData);
    const view = byId("view");
    renderView();
    const [waiting, flagged, lanes, spend] = view.children;
    const titles = (panel) => panel.children[1].children.filter((node) => node.className?.split(" ").includes("row"))
      .map((node) => node.firstChild.firstChild.firstChild.textContent);
    const laneRows = (panel) => panel.children[1].children.filter((node) => node.className === "lane-row");
    const laneNames = (panel) => laneRows(panel).map((node) => node.firstChild.firstChild.firstChild.firstChild.textContent);
    assert.deepEqual(titles(waiting), ["Incident newest", "Incident older", "Block today"]);
    assert.equal(waiting.children[1].children[3].textContent, "View all 4 waiting on you →");
    assert.equal(waiting.children[1].children[3].href, "#system");
    assert.deepEqual(titles(flagged), ["Routing exception", "CI failed twice", "Budget exceeded"]);
    assert.equal(flagged.children[1].children[3].textContent, "View all 6 alerts →");
    assert.deepEqual(laneNames(lanes), ["Release", "Stale Lane"]);
    assert.match(textIn(lanes), /Release blocked Ship the release Next: Review patch · Blocker: Get test data/);
    assert.match(textIn(lanes), /Stale Lane blocked Blocker: Stale Lane goal stale \(no progress for 24h\+\)/);
    assert.match(textIn(lanes), /Show all lanes \(2 hidden\)/);
    assert.doesNotMatch(textIn(view), /Open in T3|Draft CI failed/);
    for (const link of [waiting.children[1].children[1], flagged.children[1].children[1], laneRows(lanes)[0].firstChild]) {
      assert.equal(link.tag, "a");
      assert.match(link.className, /\brow-link\b/);
      assert.equal(link.href, "https://t3.example/threads/env/thread");
      assert.equal(link.target, "_blank");
      assert.equal(link.rel, "noopener noreferrer");
      assert.equal(link.children[1].textContent, "↗");
    }
    location.hash = "#system";
    renderView();
    const systemAlertRows = view.children[3].children[1].children;
    const goalAlertRow = systemAlertRows.find((node) => textIn(node).includes("Stale Lane goal stale"));
    assert.ok(goalAlertRow);
    const goalAlertLink = goalAlertRow.children[1].children[0];
    assert.equal(goalAlertLink.tag, "a");
    assert.equal(goalAlertLink.href, "https://t3.example/threads/env/thread");
    assert.equal(goalAlertRow.children[0].children[0].children[1].textContent, "Warning");
    location.hash = "";
    renderView();
    const toggle = (panel) => panel.children[1].children.at(-1).firstChild;
    toggle(lanes).onclick();
    currentContext = context(boardData);
    renderView();
    const expanded = view.children[2];
    assert.deepEqual(laneNames(expanded), ["Release", "Stale Lane", "Quiet Lane", "Paused Lane"]);
    assert.equal(toggle(expanded).textContent, "Hide idle lanes");
    assert.equal(toggle(expanded)["aria-expanded"], "true");
    assert.match(textIn(expanded), /Quiet Lane idle.*Paused Lane paused/);
    assert.equal(laneRows(expanded)[2].firstChild.firstChild.children.length, 1);
    toggle(expanded).onclick();
    assert.deepEqual(laneNames(view.children[2]), ["Release", "Stale Lane"]);
    assert.match(textIn(spend), /Unavailable · Semon not enabled on this host/);
    const estimate = { status: "available", preferredUsd: 4496.166422454997, sessionCount: 1672, coverage: "partial",
      source: "semon", observedAt: "2026-11-27T13:00:00Z", unpricedCount: 1700 };
    assert.match(textIn(spendMeter({ spend: estimate, budgets: [] })), /USD 4,496[.]17 est[.] monthly · 1,672 sessions · partial/);
    assert.match(textIn(spendRow("Team monthly", estimate)), /1700 unpriced steps across 1672 sessions/);
    assert.match(textIn(spendMeter({ spend: { ...estimate, preferredUsd: null }, budgets: [] })), /No priced steps/);
    const capped = spendMeter({ spend: health.spend, budgets: [{ enabled: true, scopeKind: "team",
      period: "monthly", capUsd: 12, spend: { ...estimate, preferredUsd: 4.5 } }] });
    assert(textIn(capped).includes("USD 4.50 / 12 monthly · est. · partial"));
    assert.equal(byId("owner-digest").hidden, true);
    assert.equal(descendants(view).some((node) => node.textContent === "Hosts"), false);
    const withoutPlan = { ...lane, goal: null };
    assert.equal(summarizeLaneLine2(laneSummary(withoutPlan, currentContext)), "Blocker: Incident older");
    assert.equal(summarizeLaneLine2(laneSummary(idleLane, currentContext)), "");
    assert.equal(summarizeLaneLine2(laneSummary(pausedLane, currentContext)), "");
    const cleanContext = { ...currentContext, decisionsByLane: new Map(), alertsByLane: new Map() };
    const planSummary = (weeklyPlan) => summarizeLaneLine2(laneSummary({ ...lane, goal: { weeklyPlan } }, cleanContext));
    assert.equal(planSummary([{ next: "Test", blocker: "Fixtures" }]), "Next: Test · Blocker: Fixtures");
    assert.equal(planSummary(["Test"]), "Next: Test");
    assert.equal(planSummary(null), "");

    const waitingLane = { ...idleLane, id: "waiting", slug: "waiting", ownerAgentId: "waiting-manager" };
    const stalledLane = { ...idleLane, id: "stalled", slug: "stalled", ownerAgentId: null };
    const unownedLane = { id: "unowned", slug: "unowned", status: "active", priorityRank: 7 };
    const blockedLane = { ...idleLane, id: "blocked", slug: "blocked", ownerAgentId: "blocked-manager" };
    const planLane = { ...pausedLane, id: "plan", slug: "plan", goal: { weeklyPlan: { blocker: "Need a fixture" } } };
    const listing = [withoutPlan, idleLane, pausedLane, waitingLane, stalledLane, blockedLane, planLane, unownedLane];
    const partitionContext = context({ ...boardData, digest: { sections: { lanes: listing } }, health: {
      ...health, pendingOwnerDecisions: [],
      policyAlerts: [alert("Manager failed twice", "two_failure_stop", 29, { agentId: "blocked-manager" })],
      agents: [...health.agents, { ...manager, id: "waiting-manager", status: "waiting" },
        { ...idleManager, id: "blocked-manager" }, { ...manager, id: pausedLane.ownerAgentId }],
      activeRuns: [{ id: "paused-run", agentId: pausedLane.ownerAgentId, laneId: pausedLane.id }],
      stalledRuns: [{ id: "stalled-run", laneId: stalledLane.id }],
    } });
    const partitioned = partitionBoardLanes(listing, partitionContext);
    assert.deepEqual(partitioned.visible.map((item) => item.id), [lane.id, waitingLane.id, stalledLane.id, blockedLane.id, planLane.id]);
    assert.deepEqual(partitioned.hidden.map((item) => item.id), [idleLane.id, pausedLane.id, unownedLane.id]);
    assert.equal(laneSummary(waitingLane, partitionContext).status, "waiting");
    assert.equal(laneSummary(blockedLane, partitionContext).status, "blocked");
    assert.equal(summarizeLaneLine2(laneSummary(blockedLane, partitionContext)), "Blocker: Manager failed twice");

    const laneButton = laneRows(view.children[2])[0].children[1];
    assert.equal(laneButton.textContent, "Details");
    assert.equal(laneButton["aria-label"], "Details for Release");
    let prevented = false;
    let stopped = false;
    laneButton.onclick({ preventDefault() { prevented = true; }, stopPropagation() { stopped = true; } });
    assert(prevented && stopped);
    assert.equal(location.hash, "#lane/crawlab");
    navigate();
    assert.match(textIn(view), /Goal & Weekly Plan Ship the release Releases · unknown\/1 Week of 2026-11-23 · Approved · Completed/);
    assert.match(textIn(view), /Ship patch/);
    assert.match(textIn(view), /CI failed twice/);
    assert.doesNotMatch(textIn(view), /org\/idle/);
    assert.match(textIn(view), /#3.*#2.*#1.*Closed and draft PRs \(3\).*#7 draft failure/);
    const laneDisclosure = view.querySelector();
    laneDisclosure.open = true;
    renderView();
    assert.equal(view.querySelector().open, true);
    location.hash = "#system";
    navigate();
    assert.equal(view.querySelector().open, false);
    assert.equal(byId("owner-digest").hidden, false);
    assert.match(textIn(view), /Incident newest.*Incident older.*Block today.*Digest request/);
    assert.match(textIn(view), /CI warning/);
    assert.match(textIn(view), /Closed and draft PRs \(4\).*org\/idle #6 · New work draft failure/);
    location.hash = "#lane/crawlab";
    navigate();
    assert.equal(view.querySelector().open, true);
    location.hash = "";
    navigate();
    assert.equal(byId("owner-digest").hidden, true);
    currentContext = context({ ...boardData, digest: { sections: { lanes: [idleLane, pausedLane] } } });
    renderView();
    assert.match(textIn(view.children[2]), /All lanes are idle with no blockers Show all lanes \(2\)/);
    currentContext = context({ digest: { sections: { lanes: [] } }, health: {
      ...health, pendingOwnerDecisions: [], policyAlerts: [],
    }, friction: [], linkedPrs });
    renderView();
    assert.match(textIn(view), /Nothing waiting on you Alerts No active alerts/);
  `, {
    assert, URL,
    location: {
      fragment: "",
      get hash() { return this.fragment; },
      set hash(value: string) { this.fragment = value && !value.startsWith("#") ? "#" + value : value; },
    },
    localStorage: { getItem: () => null },
  });
});

test("T3 deep links encode both segments and follow evidence, lane, requester, or repository", async () => {
  const script = await dashboardScript();
  runInNewContext(`${script}
    const east = { hostId: "east", webUrl: "https://t3-east.example/", mcpUrl: "http://127.0.0.1:8000/mcp", environmentId: "env-east" };
    const west = { hostId: "west", webUrl: "https://t3-west.example/threads", mcpUrl: "https://t3-west.example/mcp", environmentId: "env-west" };
    const eastAgent = { id: "east-manager", hostId: "east", externalRef: "env-east/mcp:launch:one/two", label: "Manager" };
    const westAgent = { id: "chief-pm", hostId: "west", externalRef: "env-west/mcp:launch:three", label: "Chief PM" };
    const lane = { id: "lane1", slug: "awb", ownerAgentId: eastAgent.id };
    const ctx = {
      agentsById: new Map([[eastAgent.id, eastAgent], [westAgent.id, westAgent]]),
      hostsById: new Map([[east.hostId, east], [west.hostId, west]]),
      lanesById: new Map([[lane.id, lane]]), lanesBySlug: new Map([[lane.slug, lane]]),
    };
    const eastLink = "https://t3-east.example/threads/env-east/mcp%3Alaunch%3Aone%2Ftwo";
    const westLink = "https://t3-west.example/threads/env-west/mcp%3Alaunch%3Athree";
    assert.equal(encodeT3Segment(".!~*'() /:"), "%2E%21%7E%2A%27%28%29%20%2F%3A");
    assert.equal(t3ThreadUrl(east, "env.one", "thread!"), "https://t3-east.example/threads/env%2Eone/thread%21");
    assert.equal(hostWebOrigin({ ...east, mcpUrl: west.mcpUrl }), "https://t3-east.example");
    assert.equal(hostWebOrigin(west), "https://t3-west.example");
    for (const webUrl of [null, "http://t3-east.example", "https://t3-east.example/?", "https://t3-east.example/.", "https://@t3-east.example"]) {
      assert.equal(hostWebOrigin({ ...east, webUrl }), null);
    }
    assert.equal(t3ThreadUrl(east, "", "thread"), null);
    assert.equal(t3ThreadUrl(east, "env", ""), null);
    assert.equal(laneThreadUrl(lane, ctx.agentsById, ctx.hostsById), eastLink);
    assert.equal(agentThreadUrl({ ...eastAgent, externalRef: "thread-only" }, ctx.hostsById), null);
    assert.equal(decisionThreadUrl({ evidence: { hostId: "west", environmentId: "env-west", threadId: "mcp:launch:three" }, laneId: lane.id }, ctx), westLink);
    assert.equal(decisionThreadUrl({ evidence: [{ nested: { agentId: westAgent.id } }] }, ctx), westLink);
    assert.equal(decisionThreadUrl({ evidence: ["t3-thread://v2/env-west/mcp:launch:three"] }, ctx), westLink);
    assert.equal(decisionThreadUrl({ laneId: lane.id, requestedBy: westAgent.id }, ctx), eastLink);
    assert.equal(decisionThreadUrl({ requestedBy: "chief pm" }, ctx), westLink);
    assert.equal(decisionThreadUrl({ requestedBy: westAgent.id }, ctx), westLink);
    assert.equal(decisionThreadUrl({}, ctx), null);
    assert.equal(alertThreadUrl({ detail: { agentId: westAgent.id }, laneId: lane.id }, ctx), westLink);
    assert.equal(alertThreadUrl({ detail: { environmentId: "env-west", threadId: "mcp:launch:three" }, laneId: lane.id }, ctx), westLink);
    assert.equal(alertThreadUrl({ laneId: lane.id }, ctx), eastLink);
    assert.equal(alertThreadUrl({ detail: { repository: "org/agent-workbench" } }, ctx), eastLink);
    assert.equal(alertThreadUrl({ detail: { repository: "org/other" } }, ctx), null);
    const link = t3Link(eastLink);
    assert.equal(link.href, eastLink);
    assert.equal(link.target, "_blank");
    assert.equal(link.rel, "noopener noreferrer");
    let stopped = false;
    link.onclick({ stopPropagation() { stopped = true; } });
    assert(stopped);
    assert.equal(t3Link("http://127.0.0.1:8000"), null);
  `, {
    assert, URL, location: { hash: "" }, window: { addEventListener() {} },
    localStorage: { getItem: () => null },
    document: {
      documentElement: { dataset: {} }, getElementById: () => ({}),
      querySelectorAll: () => [], createElement: () => ({}),
    },
  });
});

test("the loopback page removes the token fragment and retains authorization during hash navigation", async () => {
  const script = await dashboardScript();
  const token = "a".repeat(64);
  const location = { hash: `#token=${token}`, pathname: "/", search: "" };
  const browser: { navigate?: () => void } = {};
  const calls: string[] = [];
  const authorization = runInNewContext(`${script}\nread("/v1/hosts");`, {
    location, URLSearchParams,
    history: { replaceState(_state: null, _title: string, path: string) {
      assert.equal(path, "/");
      location.hash = "";
    } },
    window: { addEventListener(_event: string, callback: () => void) { browser.navigate = callback; }, scrollTo() {} },
    localStorage: { getItem: () => null },
    document: { documentElement: { dataset: {} }, querySelectorAll: () => [], getElementById: () => ({ value: "01:00Z" }) },
    AbortSignal, clearTimeout() {}, setTimeout() {}, setInterval() {},
    async fetch(path: string, options: RequestInit) {
      assert.equal(new Headers(options.headers).get("authorization"), `Bearer ${token}`);
      calls.push(path);
      return { ok: false, status: 401 };
    },
  });
  await assert.rejects(authorization, { message: "Open the dashboard URL printed by the CLI." });
  assert.equal(location.hash, "");
  assert(calls.includes("/v1/hosts"));
  assert(browser.navigate);
  location.hash = "#lane/crawlab";
  browser.navigate();
  assert.equal(location.hash, "#lane/crawlab");
});

test("the preview follows the system theme and contains no write controls or outbound T3 links", async () => {
  const html = await readFile(new URL("./dashboard-v2.html", import.meta.url), "utf8");
  assert.match(html, /color-scheme:\s*light dark/);
  assert.match(html, /:root \{[^}]*--accent: light-dark\(#3e5bd8, #7b93ff\)/);
  assert.doesNotMatch(html, /data-style|\?style=/);
  assert.doesNotMatch(html, /t3-thread:\/\/|\.ts\.net\/threads\/|Open in T3/i);
  assert.doesNotMatch(html, /theme-toggle|data-theme|<form\b|type=["']submit|\binnerHTML\b/i);
  assert.doesNotMatch(html, /\bStalled\b/);
  assert.match(html, /Stall: unavailable \(Semon #788\)/);
  assert.doesNotMatch(html, /\.tile p\b/);
  assert.match(html, /\.tile strong \{[^}]*white-space: nowrap; text-overflow: ellipsis/);
  assert.match(html, /\.tabbar\s*\{[^}]*position:\s*fixed;[^}]*inset:\s*auto 0 0/);
  assert.match(html, /main\s*\{[^}]*calc\(72px \+ env\(safe-area-inset-bottom\)\)/);
  assert.match(html, /\.card-head \+ \.settled-prs \{ border-top: 0 \}/);
});

const previewDom = `
  function mockElement(tag) {
    return {
      tag, children: [], dataset: {}, attrs: {}, style: { setProperty() {} },
      append(...children) { this.children.push(...children); },
      replaceChildren(...children) { this.children = children; },
      get firstChild() { return this.children[0]; },
      get textContent() { return (this.text ?? "") + this.children.map((child) => child.textContent).join(""); },
      set textContent(value) { this.text = String(value); this.children = []; },
      setAttribute(key, value) { this.attrs[key] = String(value); },
      removeAttribute(key) { delete this.attrs[key]; },
      querySelector(name) { return descendants(this).slice(1).find((node) => node.tag === name) ?? null; },
      focus() {},
    };
  }
  function descendants(node) {
    return [node, ...node.children.flatMap(descendants)];
  }
  const nodes = new Map();
  const document = {
    documentElement: { dataset: {} },
    createElement: mockElement,
    createElementNS: (_, name) => mockElement(name),
    getElementById(id) {
      if (!nodes.has(id)) nodes.set(id, mockElement("div"));
      return nodes.get(id);
    },
    querySelectorAll() { return []; },
  };
  const location = { hash: "#overview" };
  const window = { addEventListener(name, handler) { this[name] = handler; }, scrollTo() {} };
  const testingNow = Date.parse("2026-11-28T13:00:00Z");
`;

test("preview overview summarizes UTC activity, prioritizes goals, and renders Semon rollups and recent fleet health", async () => {
  const script = await dashboardScript("./dashboard-v2.html");
  runInNewContext(String.raw`${previewDom}
    ${script}
    const lanes = [
      { id: "ongoing", slug: "ongoing", name: "Ongoing", status: "active", priorityRank: 1 },
      { id: "waiting", slug: "waiting", name: "Waiting lane", status: "active", priorityRank: 2 },
      { id: "risk", slug: "risk", name: "Risk lane", status: "active", priorityRank: 3 },
      { id: "old", slug: "old", name: "Prior week", status: "active", priorityRank: 4 },
    ];
    const weekStart = "2026-11-23";
    const goals = [
      { laneId: "ongoing", weekStart, metricCurrent: 3, metricTarget: 8, metricName: "Items", weeklyPlan: { targetDate: "2026-11-29" } },
      { laneId: "waiting", weekStart, metricCurrent: 1, metricTarget: 4, metricName: "Checks" },
      { laneId: "risk", weekStart, metricCurrent: 0, metricTarget: 1, metricName: "Releases" },
      { laneId: "old", weekStart: "2026-11-16", metricCurrent: 99, metricTarget: 99, statement: "Reviewed release", completedAt: "2026-11-28T13:00:20Z" },
      { laneId: "old", weekStart: "2026-11-16", statement: "Beyond clock grace", completedAt: "2026-11-28T13:00:31Z" },
      { laneId: "old", weekStart: "2026-11-16", statement: "Other goal", completedAt: "2026-11-23T03:00:00Z" },
    ];
    function pr(number, lastWakeAt, state = "merged") {
      return { prHost: "github.com", repository: "org/project", number, lastWakeAt, state, title: "PR " + number };
    }
    const linkedPrs = [
      pr(7, "2026-11-28T08:00:00Z"), pr(7, "2026-11-28T13:00:25Z"), pr(3, "2026-11-22T00:00:00Z"),
      pr(1, "2026-11-21T23:59:59Z"), pr(2, "not a date"), pr(4, "2026-11-28T13:00:31Z"),
      pr(9, "2026-11-28T11:00:00Z", "open"),
    ];
    const risk = { laneId: "risk", summary: "Checks failing", detail: { repository: "org/project", number: 10 } };
    const health = {
      pendingOwnerDecisions: [{ laneId: "waiting" }], policyAlerts: [risk], budgets: [],
      agents: [
        { id: "active", status: "active", label: "Running agent" },
        { id: "waiting", status: "waiting", label: "Waiting agent", lastSeenAt: "2026-11-20T09:00:00Z" },
        { id: "recent", status: "idle", lastSeenAt: "2026-11-27T12:59:30Z" },
        { id: "old", status: "idle", lastSeenAt: "2026-11-27T12:59:29Z" },
        { id: "offline", status: "offline", lastSeenAt: "2026-11-28T13:00:20Z" },
        { id: "future", status: "offline", lastSeenAt: "2026-11-28T13:00:31Z" },
      ],
      activeRuns: [
        { id: "0", agentId: "active", model: "openai/gpt-superseded-run", status: "waiting_input", startedAt: "2026-11-28T12:54:00Z" },
        { id: "1", agentId: "active", model: "openai/gpt-6.2-sol", status: "running", startedAt: "2026-11-28T12:55:00Z" },
        { id: "2", agentId: "waiting", model: "google/gemini-4-neon", status: "waiting_approval", updatedAt: "2026-11-28T12:57:00Z" },
        { id: "2-a", agentId: "waiting", model: "google/gemini-newest", status: "waiting_approval", updatedAt: "2026-11-28T12:57:00Z" },
        { id: "3", agentId: "input", externalRunRef: "Input run", status: "waiting_input" },
        { id: "4", agentId: "legacy", externalRunRef: "Legacy run", status: "stalled" },
      ],
      stalledRuns: [{ id: "1" }, { id: "4" }],
    };
    const rollup = (preferredUsd, coverage = "complete") =>
      ({ status: "available", preferredUsd, coverage, observedAt: "2026-11-28T12:00:00Z" });
    const spend = { daily: rollup(17.5), weekly: rollup(51, "partial"), monthly: rollup(173.21) };
    ctx = context({ digest: { generatedAt: "2026-11-28T12:59:30Z", sections: { lanes } }, health, goals, linkedPrs, spend,
      friction: [{ summary: "Repeated approval request" }] }, testingNow);

    const summary = summaryTiles(ctx);
    assert.equal(summary.textContent, "Shipped today1 PR · 1 goalAt risk1 alertNeeds you1 ask");
    assert.deepEqual(summary.children.map(({ title }) => title), ["PR 7 · Reviewed release", "Risk lane #10", "Waiting lane"]);
    assert.equal(summary.children[2].href, "#decisions");
    const emptySummary = summaryTiles({ ...ctx, activity: activityByDay([], [], testingNow), alerts: [], decisions: [] });
    assert.match(emptySummary.textContent, /0 alerts/);
    assert.deepEqual(emptySummary.children.map(({ title }) => title), ["None recorded yet today", "All clear", "No pending decisions"]);

    assert.equal(ctx.lanes.map(({ lane }) => lane.id).join(","), "waiting,risk,ongoing,old");
    const laneText = goalsCard(ctx, true).textContent;
    assert.match(laneText, /Waiting laneWaiting on owner.*Risk laneAt risk.*OngoingActive3\/811-29.*Prior weekNo goal/);
    assert.doesNotMatch(laneText, /99\/99/);
    assert.equal(overview({ ...ctx, decisions: [] }).flatMap(descendants).filter((node) => node.href === "#lane/ongoing").length, 2);

    const cost = spendCard(ctx);
    assert.match(cost.textContent, /PartialToday\$17\.50This week\$51\.00Month est\.\$173\.21/);
    assert.match(cost.textContent, /No team budget configured/);
    assert.match(cost.textContent, /By-model & by-lane breakdown: No data yet/);
    assert(!descendants(cost).some((node) => node.className?.includes("meter")));
    health.budgets.push({ enabled: true, scopeKind: "team", period: "weekly", capUsd: 90, warnPct: 80, spendSource: "semon" });
    assert(descendants(spendCard(ctx)).some((node) => node.className?.includes("meter")));
    const missingCost = spendCard({ health, spend: {
      daily: { status: "available", preferredUsd: 30, coverage: "complete" },
      weekly: rollup(null), monthly: { status: "unavailable", preferredUsd: 17 },
    } });
    assert.match(missingCost.textContent, /UnknownTodayUnavailableThis weekUnavailableMonth est\.Unavailable/);

    const fleet = fleetCard(ctx);
    assert.match(fleet.textContent, /1 active1 waiting1 idle1 offline/);
    assert.match(fleet.textContent, /Codex · gpt-6.2-solRunning5m/);
    assert.match(fleet.textContent, /Gemini · gemini-newestNeeds approval/);
    assert.match(fleet.textContent, /Input runNeeds input/);
    assert.match(fleet.textContent, /Legacy runUnknown/);
    assert.match(fleet.textContent, /Stall: unavailable \(Semon #788\)/);
    assert.doesNotMatch(fleet.textContent, /stalled|gpt-superseded-run|gemini-4-neon/i);
    assert.equal(descendants(fleet).filter((node) => node.className === "run-row").length, 4);

    const throughputPanel = throughputCard(ctx);
    const chart = descendants(throughputPanel).find((node) => node.tag === "svg" && node.attrs.role === "img");
    assert(chart);
    assert.equal(descendants(chart).filter((node) => node.tag === "rect").length, 2);
    assert.equal(descendants(chart).filter((node) => node.tag === "circle").length, 2);
    assert.notEqual(chart.attrs.preserveAspectRatio, "none");
    assert.match(chart.attrs["aria-label"], /Nov 22: 1 PR merged, 0 goals completed/);
    assert.match(chart.attrs["aria-label"], /Nov 28: 1 PR merged, 1 goal completed/);
    assert.match(throughputPanel.textContent, /2 PRs merged2 goals completed/);
    assert.match(throughputPanel.textContent, /1 open frictionRepeated approval request/);
    assert.equal(throughput(activityByDay([], [], testingNow)).textContent, "No data yet");

    const extraRuns = Array.from({ length: 4 }, (_, i) => ({ id: "extra-" + i, agentId: "extra-" + i,
      externalRunRef: "Extra run " + i, status: "running" }));
    ctx = { ...ctx, decisions: [], health: { ...health, activeRuns: [...health.activeRuns, ...extraRuns] } };
    render();
    const visible = () => descendants(byId("view")).filter((node) => node.className === "run-row");
    const toggle = () => descendants(byId("view")).find((node) => node.dataset.focus === "overview-run-toggle");
    assert.equal(visible().length, 5);
    assert.equal(toggle().textContent, "Show all 8 active runs");
    assert.equal(toggle().attrs["aria-expanded"], "false");
    toggle().onclick();
    assert.equal(visible().length, 8);
    assert.equal(toggle().attrs["aria-expanded"], "true");
    toggle().onclick();
    assert.equal(visible().length, 5);
  `, { assert });
});

test("preview decisions filter by real due dates and blockers and render read-only GitHub PR links", async () => {
  const script = await dashboardScript("./dashboard-v2.html");
  runInNewContext(String.raw`${previewDom}
    ${script}
    const decisions = [
      { id: "approval", laneId: "product", title: "Approve <review>?", urgency: "blocking_today",
        recommendation: "Approve", options: ["Approve", { label: "Request changes" }, "Defer"], evidence: {
          due: "2026-11-28T17:00:00Z", blocks: ["Release", "Docs"],
          prs: ["https://github.com/org/project/pull/31", "http://github.com/org/project/pull/32",
            "https://github.com@other.example/org/project/pull/33", "https://github.com/org/project/issues/34",
            "javascript:alert(1)", "https://user@github.com/org/project/pull/35"],
        } },
      { id: "later", laneId: null, title: "Discuss later", urgency: "digest", recommendation: "Review next week",
        evidence: { due: "2026-12-02", blocks: [] }, options: null },
      { id: "overdue", laneId: "product", title: "Renew account", urgency: "digest", recommendation: "Renew",
        evidence: { due: "2026-11-27" }, options: [] },
    ];
    ctx = { now: testingNow, decisions, laneNames: new Map([["product", "Product"]]), lanes: [] };
    location.hash = "#decisions";
    render();
    const view = byId("view");
    const filter = (label) => descendants(view).find((node) => node.tag === "button" && node.textContent === label);
    const articles = () => descendants(view).filter((node) => node.tag === "article");
    assert(filter("All (3)"));
    assert.equal(articles().length, 3);
    const [approval, later, overdue] = articles();
    assert.match(approval.textContent, /ProductDue.*Blocking todayApprove <review>\?/);
    assert.match(approval.textContent, /RecommendedApproveOptionsRequest changesDeferBlocks Release · Docs/);
    assert.match(approval.textContent, /Read-only · Answer in chat/);
    const links = descendants(approval).filter((node) => node.tag === "a");
    assert.equal(links.length, 1);
    assert.equal(links[0].href, "https://github.com/org/project/pull/31");
    assert.equal(links[0].rel, "noopener noreferrer");
    assert(descendants(approval).some((node) => node.className === "tag warn" && node.textContent.startsWith("Due")));
    assert(descendants(overdue).some((node) => node.className === "tag bad" && node.textContent.startsWith("Overdue")));
    assert(!articles().flatMap(descendants).some((node) => ["form", "button", "input"].includes(node.tag)));
    assert.match(later.textContent, /TeamDue Dec 2DigestDiscuss later/);

    filter("Due soon").onclick();
    assert.equal(articles().map((node) => descendants(node).find((item) => item.tag === "h2").textContent).join(","),
      "Approve <review>?,Renew account");
    filter("Blocking").onclick();
    assert.equal(articles().length, 1);
    assert.match(articles()[0].textContent, /Approve <review>\?/);
  `, { assert, URL });
});

test("preview lane directory filters stored goals and links to the in-Ostrom lane view", async () => {
  const script = await dashboardScript("./dashboard-v2.html");
  runInNewContext(String.raw`${previewDom}
    ${script}
    const lanes = [
      { lane: { id: "prod", slug: "prod", name: "Product", status: "active", goal: {
        statement: "Ship the release", metricName: "PRs", metricCurrent: 2, metricTarget: 3,
        weeklyPlan: { due: "2026-11-30" },
      } } },
      { lane: { id: "ops", slug: "ops", name: "Operations", status: "active", goal: {
        statement: "Review access", metricName: "Checks", metricCurrent: 0, metricTarget: 1,
      } } },
      { lane: { id: "docs", slug: "docs", name: "Docs", status: "active", ownerAgentId: "manager" } },
      { lane: { id: "paused", slug: "paused", name: "Paused lane", status: "paused" } },
      { lane: { id: "archive", slug: "archive", name: "Archived lane", status: "archived" } },
    ];
    ctx = { lanes, decisions: [], alerts: [{ laneId: "ops" }], health: { agents: [{ id: "manager", status: "waiting" }] } };
    location.hash = "#lanes";
    render();
    const view = byId("view");
    const visibleLanes = () => descendants(view).filter((node) => node.className === "lane-row");
    const button = (label) => descendants(view).find((node) => node.tag === "button" && node.textContent === label);
    assert.equal(visibleLanes().map((row) => row.href).join(","), "#lane/prod,#lane/ops,#lane/docs,#lane/paused,#lane/archive");
    assert.match(visibleLanes()[0].textContent, /ProductActiveShip the releaseDone 2\/3Due 11-30›/);
    assert(descendants(visibleLanes()[0]).some((node) => node.attrs["aria-label"] === "PRs: 2/3"));
    for (const [name, slug] of [["At risk", "ops"], ["Waiting", "docs"], ["Active", "prod"], ["Paused", "paused"]]) {
      button(name).onclick();
      assert.equal(button(name).attrs["aria-pressed"], "true");
      assert.equal(visibleLanes().map((row) => row.href).join(","), "#lane/" + slug);
    }
    button("All").onclick();
    const search = descendants(view).find((node) => node.tag === "input");
    search.value = " ACCESS ";
    search.oninput();
    assert.equal(visibleLanes().map((row) => row.href).join(","), "#lane/ops");
    button("Paused").onclick();
    assert.match(view.textContent, /No lanes match these filters\./);
  `, { assert });
});

test("preview lane detail renders the six sections and uses only safe GitHub PR links", async () => {
  const script = await dashboardScript("./dashboard-v2.html");
  runInNewContext(String.raw`${previewDom}
    ${script}
    location.hash = "#lane/%70roduct";
    assert.equal(routeEndpoint(), "/v1/lanes/product/detail");
    const lane = { id: "product", name: "Product", status: "active" };
    const goal = { id: "current", statement: "Publish an update", metricName: "Releases", metricCurrent: 1, metricTarget: 2,
      weeklyPlan: { nextStep: "Review the draft", blockers: ["Release notes"], due: "2026-11-30" } };
    const prior = { id: "prior", statement: "Cut the previous release", metricName: "Releases", metricCurrent: 1, metricTarget: 1,
      weekStart: "2026-11-16", completedAt: "2026-11-22T11:30:00Z" };
    const pr = (number, info = {}) => ({ number, repository: "org/product", title: "Update " + number,
      url: "https://github.com/org/product/pull/" + number, state: "open", ...info });
    const detail = { lane, goal, goalHistory: [goal, prior], prs: [pr(4, { rollupState: "success" }),
      pr(7, { rollupState: "failure", failedChecks: 1, totalChecks: 25, consecutiveFailures: 2 }),
      pr(5, { rollupState: "pending", consecutiveFailures: 2 }),
      pr(8, { url: "https://github.com/org/product/pull/8?redirect=invalid" }),
      pr(9, { url: "http://github.com/org/product/pull/9" }),
      pr(10, { url: "https://github.com@other.example/org/product/pull/10" }),
      pr(11, { url: "https://github.com/org/../pull/11" }),
      ...Array.from({ length: 6 }, (_, index) => pr(20 + index, { state: index ? "closed" : "merged" }))],
      activeRuns: [], recentAgents: [], spend: { status: "unavailable" },
      decisions: [{ id: "review", laneId: lane.id, authority: "owner", status: "pending", urgency: "blocking_today",
        title: "Approve the release?", recommendation: "Approve", options: ["Approve", "Defer"],
        evidence: { due: "2026-11-28T14:00:00Z", blocks: ["Rollout"] } },
      { id: "resolved", laneId: lane.id, authority: "owner", status: "approved", title: "Old decision" }],
      alerts: [{ summary: "Two check failures" }], friction: [{ summary: "Clarify scope" }], timeline: [
        { kind: "goal_completed", at: "2026-11-22T11:30:00Z", summary: "Goal completed · Cut the previous release" },
      ] };
    routeResult = { endpoint: routeEndpoint(), value: detail };
    render();
    const view = byId("view");
    assert.equal(descendants(view).find((node) => node.className === "lineup lane-back").href, "#lanes");
    assert(navItems.filter(({ key }) => key === "lanes").every(({ link }) => link.attrs["aria-current"] === "page"));
    assert.equal(descendants(view).filter((node) => node.tag === "h2").map((node) => node.textContent).join("|"),
      "Current Goal & Next Step|Recent PRs & Merges|Running Workers|Lane Spend|Decisions & Blockers|Activity Timeline");
    assert.match(view.textContent, /At riskProduct.*Publish an updateDone 1\/2Due 11-30Next: Review the draft/);
    assert.match(view.textContent, /Goal History · 1Cut the previous releaseWeek of 2026-11-16 · Releases 1\/1Completed/);
    assert.match(view.textContent, /Recent PRs & Merges#7 Update 7org\/product · 1\/25 checks failed · 2 consecutive headsFailing›#5 Update 5org\/product · re-runningPending›/);
    assert.match(view.textContent, /#4 Update 4org\/product · Checks passedPassing›/);
    assert.match(view.textContent, /Merged & closed · 6.*mergedMerged/);
    assert.match(view.textContent, /No active runs on ProductStall: unavailable \(Semon #788\)/);
    assert.match(view.textContent, /No data yet · awaiting per-lane Semon rollup/);
    assert.match(view.textContent, /Approve the release\?RecommendedApproveOptionsDeferBlocks RolloutRead-only · Answer in chat/);
    assert.match(view.textContent, /Goal completed · Cut the previous release2026-11-22 11:30:00 UTC/);
    for (const label of ["Two check failures", "Release notes", "Clarify scope"]) assert(view.textContent.includes(label));
    assert.doesNotMatch(view.textContent, /Old decision|Stalled/);
    assert(!descendants(view).some((node) => ["button", "form", "input"].includes(node.tag)));
    const external = descendants(view).filter((node) => node.tag === "a" && node.href !== "#lanes");
    assert.equal(external.length, 9);
    assert(external.every((node) => /^https:\/\/github\.com\/org\/product\/pull\/\d+$/.test(node.href)
      && node.target === "_blank" && node.rel === "noopener noreferrer"));
    const settledOnly = lanePrCard(lane, detail.prs.filter(isSettledPr));
    assert.equal(settledOnly.children.length, 2);
    assert.equal(settledOnly.children[1].className, "settled-prs");

    const history = descendants(view).find((node) => node.className === "goal-history");
    history.open = true;
    history.ontoggle();
    detail.activeRuns = [
      { agentLabel: "Manager", model: "gpt-6.2-sol", status: "running", isLaneOwner: true,
        startedAt: new Date(Date.now() - 300_000).toISOString() },
      { agentLabel: "Worker", model: "google/gemini-4-neon", status: "waiting_input" },
    ];
    detail.spend = { status: "available", coverage: "partial", preferredUsd: 12.5 };
    render();
    assert(descendants(view).find((node) => node.className === "goal-history").open);
    assert.match(view.textContent, /LanesAt riskCodex · gpt-6\.2-solProduct/);
    assert.match(view.textContent, /ManagerCodex · gpt-6\.2-solRunning5mWorkerGemini · gemini-4-neonNeeds input/);
    assert.match(view.textContent, /Lane Spend\$12\.50Coverage: partialThis week · Semon · USD/);
    detail.activeRuns = [...detail.activeRuns, ...detail.activeRuns, ...detail.activeRuns];
    render();
    assert.equal(descendants(view).filter((node) => node.tag === "strong" && node.textContent === "Worker").length, 3);
    assert.doesNotMatch(view.textContent, /Show all.*active runs/);

    routeResult.value = { lane, goal: null, goalHistory: [], prs: [], activeRuns: [], recentAgents: [],
      spend: { status: "unavailable" }, decisions: [], alerts: [], friction: [], timeline: [] };
    render();
    for (const label of ["No prior goals recorded yet", "No linked PRs yet", "No open blockers or decisions", "No recent activity recorded yet"]) {
      assert(view.textContent.includes(label));
    }
    location.hash = "#lane/%ZZ";
    assert.equal(routeEndpoint(), null);
    render();
    assert.match(view.textContent, /Lane not found\./);
  `, { assert, URL });
});

test("preview fetches lane detail immediately on navigation and refreshes the active lane", async () => {
  const script = await dashboardScript("./dashboard-v2.html");
  const calls: string[] = [];
  let poll: (() => Promise<void>) | undefined;
  const generatedAt = "2026-11-28T13:00:00Z";
  const sections = { lanes: [], decisions: [], policyAlerts: [], interventionsByDay: [], openFrictionCount: 0 };
  const dailyDigest = { generatedAt, sections };
  const health = { hosts: [], pendingOwnerDecisions: [], policyAlerts: [], budgets: [], agents: [], activeRuns: [] };
  const detail = { lane: { name: "Product", status: "active" }, goal: null, goalHistory: [], prs: [], activeRuns: [],
    recentAgents: [], spend: { status: "unavailable" }, decisions: [], alerts: [], friction: [], timeline: [] };
  const readFixture = (path: string) => {
    calls.push(path);
    if (path === "/v1/digest?window=13:00Z") return dailyDigest;
    if (path === "/v1/health/summary") return health;
    if (path === "/v1/lanes/product/detail") return detail;
    if (path.startsWith("/v1/spend/summary")) return { status: "unavailable" };
    if (path === "/v1/friction?status=open" || path === "/v1/prs" || path === "/v1/goals") return [];
    throw new Error(`Unexpected dashboard request ${path}`);
  };
  const browser = runInNewContext(String.raw`${previewDom}
    document.documentElement.dataset.auth = "proxy";
    ${script}
    ({ navigate(hash) { location.hash = hash; window.hashchange(); }, get text() { return byId("view").textContent; } })
  `, { AbortSignal, setInterval() {}, setTimeout(callback: () => Promise<void>) { poll = callback; },
    async fetch(path: string) {
      if (path === "/v1/lanes/missing/detail") { calls.push(path); return { ok: false, status: 404 }; }
      const value = readFixture(path);
      return { ok: true, async json() { return value; } };
    } }) as { navigate: (hash: string) => void; text: string };
  const flushed = () => new Promise<void>((resolve) => { setImmediate(resolve); });
  await flushed();
  assert(poll);
  assert.equal(calls.length, 8);
  browser.navigate("#lane/product");
  assert.equal(calls.at(-1), "/v1/lanes/product/detail");
  await flushed();
  assert.match(browser.text, /Product.*No active runs on Product/);
  await poll();
  assert.equal(calls.filter((path) => path === "/v1/lanes/product/detail").length, 2);

  browser.navigate("#lane/missing");
  assert.equal(calls.at(-1), "/v1/lanes/missing/detail");
  await flushed();
  assert.match(browser.text, /Lane not found\./);
  assert.doesNotMatch(browser.text, /Product/);
});

test("preview report fetches the selected digest immediately and ignores a superseded response", async () => {
  const script = await dashboardScript("./dashboard-v2.html");
  const calls: string[] = [];
  const weeklyResponse = Promise.withResolvers<void>();
  const digest = (window: string, generatedAt: string) =>
    ({ window, generatedAt, markdown: "# Digest " + window, sections: { lanes: [] } });
  const digests = {
    "/v1/digest?window=13:00Z": digest("13:00Z", "2026-11-28T13:00:00Z"),
    "/v1/digest?window=01:00Z": digest("01:00Z", "2026-11-28T01:00:00Z"),
    "/v1/digest?window=weekly": digest("weekly", "2026-11-28T13:00:00Z"),
  };
  const linkedPrs = [
    { number: 1, lastWakeAt: "2026-11-28T12:00:00Z" },
    { number: 2, lastWakeAt: "2026-11-27T04:00:00Z" },
    { number: 3, lastWakeAt: "2026-11-24T12:00:00Z" },
  ].map((pr) => ({ ...pr, state: "merged", prHost: "github.com", repository: "org/repo" }));
  const health = { hosts: [], pendingOwnerDecisions: [], policyAlerts: [], budgets: [], agents: [], activeRuns: [] };
  const browser = runInNewContext(String.raw`${previewDom}
    document.documentElement.dataset.auth = "proxy";
    location.hash = "#report";
    ${script}
    const controls = () => descendants(byId("view")).find((node) => node.className === "filterbar report-switch").children;
    ({
      choose(label) { controls().find((node) => node.textContent === label).onclick(); },
      get labels() { return controls().map((node) => node.textContent).join("|"); },
      get selected() { return controls().find((node) => node.attrs["aria-pressed"] === "true").textContent; },
      get text() { return byId("view").textContent; },
    })
  `, {
    AbortSignal, URL, setInterval() {}, setTimeout() {},
    async fetch(path: string, options: RequestInit) {
      assert.equal(options.method ?? "GET", "GET");
      calls.push(path);
      if (path === "/v1/digest?window=weekly") await weeklyResponse.promise;
      const value = digests[path as keyof typeof digests] ?? (path === "/v1/prs" ? linkedPrs
        : path === "/v1/health/summary" ? health : path.startsWith("/v1/spend/summary") ? { status: "unavailable" } : []);
      return { ok: true, async json() { return value; } };
    },
  }) as { choose(label: string): void; labels: string; selected: string; text: string };
  const flushed = () => new Promise<void>((resolve) => { setImmediate(resolve); });
  await flushed();
  assert.equal(calls.filter((path) => path.includes("/v1/digest")).length, 1);
  assert.equal(calls[0], "/v1/digest?window=13:00Z");
  assert.equal(browser.labels, "Daily · 01:00Z|Daily · 13:00Z|Weekly");
  assert.equal(browser.selected, "Daily · 13:00Z");
  assert.match(browser.text, /Shipped · 24h.*org\/repo #1/);
  assert.doesNotMatch(browser.text, /org\/repo #[23]/);

  browser.choose("Weekly");
  assert.equal(calls.at(-1), "/v1/digest?window=weekly");
  assert.match(browser.text, /Loading Weekly report/);
  assert.doesNotMatch(browser.text, /org\/repo #1/);
  browser.choose("Daily · 01:00Z");
  assert.equal(calls.at(-1), "/v1/digest?window=01:00Z");
  await flushed();
  assert.match(browser.text, /Shipped · 24h.*org\/repo #2/);
  assert.doesNotMatch(browser.text, /org\/repo #[13]/);
  weeklyResponse.resolve();
  await flushed();
  assert.equal(browser.selected, "Daily · 01:00Z");
  assert.doesNotMatch(browser.text, /# Digest weekly|7 days|org\/repo #3/);

  browser.choose("Weekly");
  await flushed();
  assert.match(browser.text, /Shipped · 7 days.*org\/repo #1.*org\/repo #2.*org\/repo #3.*# Digest weekly/);
  browser.choose("Daily · 13:00Z");
  assert.equal(calls.at(-1), "/v1/digest?window=13:00Z");
  await flushed();
  assert.match(browser.text, /Shipped · 24h.*org\/repo #1/);
  assert.doesNotMatch(browser.text, /org\/repo #[23]/);
});

test("preview executive report displays platform records, rollup coverage and the raw Markdown", async () => {
  const script = await dashboardScript("./dashboard-v2.html");
  await runInNewContext(String.raw`${previewDom}
    let copied, selected, clipboardError;
    const navigator = { clipboard: { async writeText(markdown) {
      if (clipboardError) throw Error("Clipboard unavailable");
      copied = markdown;
    } } };
    window.getSelection = () => ({ selectAllChildren(node) { selected = node; } });
    ${script}
    location.hash = "#report";
    const lanes = Array.from({ length: 12 }, (_, i) => ({ id: "lane-" + i, slug: "lane-" + i, name: "Lane " + i,
      status: i === 11 ? "paused" : "active", priorityRank: i }));
    const decisions = [
      { id: "approval", laneId: "lane-9", title: "Review <release>?", recommendation: "Read the release draft before responding.",
        evidence: { due: "2026-11-29" } },
      { id: "scope", laneId: "lane-9", title: "Choose release scope", recommendation: "Keep current scope." },
      { id: "plan", laneId: "lane-8", title: "Approve the plan", recommendation: "Approve.", evidence: { due: "2026-11-27" } },
    ];
    const health = { hosts: [], pendingOwnerDecisions: decisions, agents: [], activeRuns: [],
      policyAlerts: [{ laneId: "lane-9" }], budgets: [
        { enabled: true, scopeKind: "team", period: "weekly", capUsd: 50, warnPct: 80, spendSource: "semon" },
        { enabled: true, scopeKind: "lane", period: "weekly", capUsd: 2, warnPct: 80, spendSource: "semon" },
      ] };
    const goal = (id, laneId, info) => ({ id, laneId, weekStart: "2026-11-23", statement: "Plan " + laneId,
      metricName: "Releases", metricCurrent: 2, metricTarget: 4, completedAt: null, ...info });
    const goals = [
      goal("risk", "lane-9", { statement: "Ship two updates", weeklyPlan: { due: "2026-11-30" } }),
      goal("waiting", "lane-8", { metricCurrent: null, weeklyPlan: { targetDate: "2026-12-01" } }),
      goal("boundary", "lane-4", { weekStart: "2026-11-16", statement: "Cut the release", completedAt: "2026-11-27T13:00:00Z" }),
      goal("outside", "lane-4", { weekStart: "2026-11-16", statement: "Old release", completedAt: "2026-11-27T12:59:59Z" }),
    ];
    const pr = (number, lastWakeAt, info = {}) => ({ number, lastWakeAt, prHost: "github.com", repository: "org/repo", state: "merged",
      title: "Change " + number, url: "https://github.com/org/repo/pull/" + number, ...info });
    const linkedPrs = [
      pr(7, "2026-11-28T12:30:00Z"),
      pr(7, "2026-11-28T12:20:00Z", { title: "Stale duplicate" }),
      pr(8, "2026-11-28T13:01:00Z"),
      pr(9, "2026-11-28T13:01:01Z"),
      pr(10, "2026-11-27T12:59:59Z"),
      pr(11, "2026-11-28T12:31:00Z", { url: "https://github.com@foreign.example/org/repo/pull/11" }),
      pr(12, "2026-11-28T12:00:00Z", { state: "closed" }),
      pr(13, "2026-11-28T12:00:00Z", { state: "open" }),
      pr(14, null),
      ...Array.from({ length: 6 }, (_, i) => pr(20 + i, "2026-11-28T11:00:00Z")),
    ];
    const friction = Array.from({ length: 6 }, (_, i) => ({ kind: i ? "repeated_ask" : "policy_break",
      summary: "Friction " + i, createdAt: "2026-11-28T12:00:00Z" }));
    const rollup = (preferredUsd, coverage) => ({ status: "available", preferredUsd, coverage, observedAt: "2026-11-28T12:00:00Z" });
    const spend = { daily: rollup(17.5, "complete"), weekly: rollup(51, "partial"), monthly: rollup(173.21, "complete") };
    const markdown = "# Report\n\n<em>Literal digest Markdown</em>";
    const reportDigest = { generatedAt: "2026-11-28T13:00:00Z", markdown, sections: { lanes } };
    ctx = context({ digest: reportDigest, health, friction, linkedPrs, goals, spend }, testingNow);
    routeResult = { endpoint: routeEndpoint(), value: reportDigest };
    render();

    const view = byId("view");
    const get = (id) => descendants(view).find((node) => node.id === id);
    const button = (label) => descendants(view).find((node) => node.tag === "button" && node.textContent === label);
    const panel = (className) => descendants(view).find((node) => node.className === className);
    assert.match(panel("card report-verdict").textContent, /Executive Verdict.*Shipped1024hDecisions3blocking 2 lanesSpend\$17\.50TodayComplete/);
    const decisionsRows = descendants(view).filter((node) => node.href === "#decisions");
    assert.equal(decisionsRows.length, 3);
    assert.match(decisionsRows[0].textContent, /Lane 9Due Nov 29Review <release>\?Read the release draft/);
    assert.match(decisionsRows[1].textContent, /No due date/);
    assert.match(decisionsRows[2].textContent, /Overdue Nov 27/);
    assert.equal(descendants(decisionsRows[0]).find((node) => node.className === "report-clamp muted").tag, "span");

    assert.equal(get("report-shipped").children.length, 8);
    assert.match(get("report-shipped").textContent, /org\/repo #8Change 8Merged.*org\/repo #7Change 7Merged/);
    assert.doesNotMatch(get("report-shipped").textContent, /#9|#10|#12|#13|#14|Stale duplicate|Cut the release/);
    const showShipped = button("Show all 10 shipped items");
    assert.equal(showShipped.attrs["aria-expanded"], "false");
    showShipped.onclick();
    assert.equal(get("report-shipped").children.length, 10);
    assert.equal(button("Show fewer shipped items").attrs["aria-controls"], "report-shipped");
    assert.match(get("report-shipped").children.at(-1).textContent, /Lane 4Cut the releaseCompleted/);
    assert.equal(get("report-shipped").children.at(-1).href, "#lane/lane-4");
    assert.equal(get("report-shipped").children.find((node) => node.textContent.includes("#11")).href, undefined);
    const external = descendants(view).filter((node) => node.tag === "a" && !node.href.startsWith("#"));
    assert.equal(external.length, 8);
    assert(external.every((node) => /^https:\/\/github\.com\/org\/repo\/pull\/\d+$/.test(node.href)
      && node.target === "_blank" && node.rel === "noopener noreferrer"));

    const score = descendants(view).filter((node) => node.className === "matrix-row report-score");
    assert.equal(score.length, 12);
    assert.equal(score[0].href, "#lane/lane-8");
    assert.match(score[0].textContent, /Lane 8WaitingPlan lane-8—12-01/);
    assert.equal(score[1].href, "#lane/lane-9");
    assert.match(score[1].textContent, /Lane 9At riskShip two updates2\/411-30/);
    assert(descendants(score[1]).some((node) => node.attrs["aria-label"] === "Releases: 2/4"));
    assert.match(score.at(-1).textContent, /Lane 11Paused/);
    assert.match(view.textContent, /Weekly ScorecardWeek of Nov 23LaneDoneDue/);

    assert.match(panel("card report-spend").textContent, /Spend & Harness FrictionPartialToday\$17\.50This week\$51\.00Month est\.\$173\.21/);
    assert.match(view.textContent, /\$51\.00 \/ \$50\.00 · weekly capCap reached/);
    assert.match(view.textContent, /By-model & by-lane breakdown: No data yet \(awaiting Semon rollup\)/);
    assert.match(view.textContent, /Stall: unavailable \(Semon #788\)/);
    assert.equal(get("report-friction").children.length, 5);
    assert.match(get("report-friction").textContent, /policy break1h 0m agoFriction 0/);
    button("Show all 6 open friction items").onclick();
    assert.equal(get("report-friction").children.length, 6);
    assert.match(get("report-friction").textContent, /Friction 5/);
    assert.doesNotMatch(view.textContent, /Stalled|Open in T3/);
    assert(descendants(view).every((node) => node.tag !== "form" && (node.tag !== "button" || node.type === "button" && !node.disabled)));

    const raw = view.children.at(-1);
    assert.equal(raw.tag, "details");
    assert.equal(raw.children[0].textContent, "Raw Markdown Digest");
    assert.equal(raw.open, false);
    assert.equal(descendants(raw).find((node) => node.tag === "pre").textContent, markdown);
    raw.open = true;
    raw.ontoggle();
    (async () => {
      await button("Copy Markdown").onclick();
      assert.equal(copied, markdown);
      assert.match(raw.textContent, /Markdown copied\./);
      clipboardError = true;
      await button("Copy Markdown").onclick();
      assert.equal(selected.tag, "pre");
      assert.equal(selected.textContent, markdown);
      assert.match(raw.textContent, /Markdown selected\./);

      ctx = { ...ctx, linkedPrs: [], goals: [], decisions: [], friction: [],
        health: { ...health, budgets: [] }, spend: {} };
      render();
      assert.equal(view.children.at(-1).open, true);
      assert.match(view.textContent, /Shipped024hDecisions0none waitingSpend—TodayUnavailable/);
      assert.match(reportVerdict(reportWindows.weekly, reportDigest.generatedAt, [], [{ laneId: null }], {}).textContent,
        /Decisions1team-level/);
      for (const empty of ["No owner decisions waiting", "No merged PRs or completed goals recorded in this window yet",
        "No team budget configured", "No open friction items"]) assert(view.textContent.includes(empty));
      ctx.spend.daily = rollup(null, "unknown");
      render();
      assert.match(panel("card report-verdict").textContent, /Spend—TodayUnknown/);
      ctx.spend.weekly = spend.weekly;
      reportWindow = "weekly";
      routeResult = { endpoint: routeEndpoint(), value: reportDigest };
      render();
      assert.match(panel("card report-verdict").textContent, /Spend\$51\.00This weekPartial/);
    })()
  `, { assert, URL });
});
