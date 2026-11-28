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

      assert.equal((await read(url, "/v1/hosts")).status, 401);
      assert.equal((await read(url, "/v1/unknown")).status, 401);
      assert.equal((await read(url, "/v1/hosts", { authorization: "Bearer " + "0".repeat(64) })).status, 401);
      assert.equal((await read(url, "/v1/hosts", { ...headers, host: "localhost:" + url.port })).status, 403);
      assert.equal((await read(url, "/v1/hosts", { ...headers, origin: "https://foreign.example" })).status, 403);
      assert.equal((await read(url, "/v1/decisions", headers, "POST")).status, 405);
      assert.equal((await read(url, "/", {}, "HEAD")).status, 405);
      assert.equal((await read(url, "/v1/budgets", headers, "PUT")).status, 405);
      assert.equal((await read(url, "/v1/lanes/platform", headers, "PUT")).status, 405);
      for (const path of ["/v1/budgets", "/v1/github/prs", "/v1/spend/summary"]) {
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
  for (const path of ["/", "/v1/hosts"]) {
    assert.equal((await read(url, path, { host: headers.host })).status, 403);
    assert.equal((await read(url, path, { ...headers, "cf-access-jwt-assertion": "invalid" })).status, 403);
  }
  const page = await read(url, "/", headers);
  assert.equal(page.status, 200);
  assert.match(page.body, /<html lang="en" data-auth="proxy">/);
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
  for (const path of ["/", "/dashboard.html", "/v1/hosts", "/unknown"]) {
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

async function dashboardScript(): Promise<string> {
  const html = await readFile(new URL("./dashboard.html", import.meta.url), "utf8");
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
      metricTarget: 1, approvedDecisionId: "approval", weeklyPlan: {
        nextStep: "Review patch", next: "Alternative", blockers: ["Get test data"], items: ["Review patch", "Ship patch"],
      } };
    const lane = { id: "release", slug: "crawlab", name: "Release", status: "active", ownerAgentId: "release-manager", priorityRank: 1, goal };
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
      { ...alert("Quiet Lane goal stale (no progress for 24h+)", "goal_stale", 28), laneId: idleLane.id },
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
    const idleManager = { ...manager, id: idleLane.ownerAgentId, status: "idle" };
    const health = { pendingOwnerDecisions: decisions, policyAlerts: alerts, hosts: [host], agents: [manager, idleManager],
      activeRuns: [], stalledRuns: [], waits: [], githubPrs: prChecks, budgets: [], spend: { status: "unavailable" } };
    const boardData = { digest: { sections: { lanes: [idleLane, lane, pausedLane] } }, health, friction: [], linkedPrs };
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
    assert.deepEqual(laneNames(lanes), ["Release", "Quiet Lane"]);
    assert.match(textIn(lanes), /Release blocked Ship the release Next: Review patch · Blocker: Get test data/);
    assert.match(textIn(lanes), /Quiet Lane goal stale \(no progress for 24h\+\)/);
    assert.match(textIn(lanes), /Show all lanes \(1 hidden\)/);
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
    const goalAlertRow = systemAlertRows.find((node) => textIn(node).includes("Quiet Lane goal stale"));
    assert.ok(goalAlertRow);
    assert.equal(goalAlertRow.href, "https://t3.example/threads/env/thread");
    assert.equal(goalAlertRow.children[0].children[0].children[1].textContent, "Warning");
    location.hash = "";
    renderView();
    const toggle = (panel) => panel.children[1].children.at(-1).firstChild;
    toggle(lanes).onclick();
    currentContext = context(boardData);
    renderView();
    const expanded = view.children[2];
    assert.deepEqual(laneNames(expanded), ["Release", "Quiet Lane", "Paused Lane"]);
    assert.equal(toggle(expanded).textContent, "Hide idle lanes");
    assert.equal(toggle(expanded)["aria-expanded"], "true");
    assert.match(textIn(expanded), /Quiet Lane idle.*Paused Lane paused/);
    assert.equal(laneRows(expanded)[1].firstChild.firstChild.children.length, 1);
    toggle(expanded).onclick();
    assert.deepEqual(laneNames(view.children[2]), ["Release"]);
    assert.match(textIn(spend), /Unavailable · Semon not enabled on this host/);
    assert.match(textIn(spendMeter({ spend: { status: "available", preferredUsd: 4.5 }, budgets: [] })), /USD 4.5 estimated/);
    assert.match(textIn(spendMeter({ spend: health.spend, budgets: [{ enabled: true, scopeKind: "team",
      period: "monthly", capUsd: 12, spend: { status: "available", preferredUsd: 4.5 } }] })), /USD 4.5 \/ 12 monthly/);
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
    assert.match(textIn(view), /Goal & Weekly Plan Ship the release Releases · unknown\/1 Week of 2026-11-23 · Approved/);
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
