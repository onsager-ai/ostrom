import assert from "node:assert/strict";
import { generateKeyPairSync, sign } from "node:crypto";
import { chmod, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
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
      assert.doesNotMatch(page.body, /<(?:link|script|img)\b[^>]*https?:\/\//i);
      assert.doesNotMatch(page.body, /\binnerHTML\b/);
      assert.match(page.body, /GitHub PR CI/);
      assert.match(page.body, /Closed PRs \(\$\{closedGithubPrs\.length\}\)/);
      assert.match(page.body, /const disclosure = element\("details"\)/);
      assert.match(page.body, /restoreClosedPrsDisclosure\(disclosure\)/);
      assert.match(page.body, /Budgets &amp; Spend/);
      assert.match(page.body, /Semon connector not enabled on this host/);
      assert.match(page.body, /No open watched PRs checked yet/);
      assert.match(page.body, /No lanes or decisions seeded yet on this host\./);
      assert.match(page.body, /No pending decisions\./);
      assert.match(page.body, /No lanes seeded yet on this host\./);
      assert.match(page.body, /No active runs, open waits, or PR wakes\./);
      assert.match(page.body, /aria-label="Theme"/);
      for (const theme of ["auto", "light", "dark"]) assert(page.body.includes(`data-theme="${theme}"`));
      assert.match(page.body, /document\.documentElement\.dataset\.theme/);
      assert.match(page.body, /Estimated USD.*Reported USD/);
      assert.match(page.body, /Billed USD.*Coverage/);
      assert.match(page.body, /history\.replaceState/);
      assert(!page.body.includes("<script>invalid()</script>"));

      const script = /<script type="module">([\s\S]*?)<\/script>/.exec(page.body)?.[1];
      assert(script);
      runInNewContext(`${script}
        Date.now = () => Date.parse("2026-11-27T13:00:00.000Z");
        assert.equal(relativeTime("2026-11-27T12:59:42.000Z"), "18s ago");
        assert.equal(relativeTime("2026-11-27T12:57:00.000Z"), "3 min ago");
        assert.equal(relativeExpiry("2026-12-27T13:00:00.000Z"), "expires in 30d");
        assert.equal(relativeExpiry("2026-11-25T13:00:00.000Z"), "expired 2d ago");
        assert.equal(relativeTime("invalid date"), "unknown time");
        assert.equal(relativeExpiry("invalid date"), "expiry unknown");
        const validExpiry = "2026-12-27T13:00:00.000Z";
        const hosts = ["invalid date", null, validExpiry].map((tokenExpiresAt) => ({
          connectionState: "connected", tokenExpiresAt,
        }));
        const hostSummary = summarizeHosts(hosts);
        assert.equal(hostSummary.degraded, 2);
        assert.equal(hostSummary.unknownExpiry, 2);
        assert.equal(hostSummary.hostError, false);
        assert.equal(hostSummary.soonestExpiry, validExpiry);
        const alerts = [{ summary: "Repeated run failure", ruleCode: "two_failure_stop" }];
        const entry = { ...alerts[0], kind: "policy_break" };
        assert.equal(isEchoedPolicyBreak(entry, alerts), true);
        assert.equal(isEchoedPolicyBreak({ ...entry, ruleCode: null }, alerts), true);
        assert.equal(isEchoedPolicyBreak({ ...entry, ruleCode: "authority_routing" }, alerts), false);
        assert.equal(isEchoedPolicyBreak({ ...entry, summary: "Other failure" }, alerts), false);
        assert.equal(isEchoedPolicyBreak({ ...entry, kind: "correction" }, alerts), false);
        assert.equal(goalHasMetric({ metricName: " Unknown " }), false);
        assert.equal(goalHasMetric({ metricName: "Releases" }), true);
        assert.equal(goalHasWeeklyPlan({ weeklyPlan: null }), false);
        assert.equal(goalHasWeeklyPlan({ weeklyPlan: [] }), false);
        assert.equal(goalHasWeeklyPlan({ weeklyPlan: ["Ship"] }), true);
        assert.equal(policySeverity("pr_green_ci"), "warn");
        assert.equal(policySeverity("budget_warning"), "warn");
        assert.equal(policySeverity("two_failure_stop"), "err");
        assert.equal(policySeverity("unknown_critical_rule"), "err");
        const groupedPrs = partitionGithubPrs([
          { repository: "z/repo", number: 2, rollupState: "success" },
          { repository: "a/repo", number: 3, rollupState: "pending" },
          { repository: "a/repo", number: 2, rollupState: "error" },
          { repository: "a/repo", number: 1, rollupState: "closed" },
        ]);
        assert.equal(groupedPrs.open.map((pr) => pr.number).join(","), "2,3,2");
        assert.equal(groupedPrs.closed.map((pr) => pr.number).join(","), "1");
        let firstToggle;
        const firstDisclosure = { open: false, addEventListener: (name, callback) => { assert.equal(name, "toggle"); firstToggle = callback; } };
        restoreClosedPrsDisclosure(firstDisclosure);
        firstDisclosure.open = true;
        assert(firstToggle);
        firstToggle();
        const nextDisclosure = { open: false, addEventListener() {} };
        restoreClosedPrsDisclosure(nextDisclosure);
        assert.equal(nextDisclosure.open, true);
      `, {
        assert, URLSearchParams, location: { hash: "", pathname: "/", search: "" },
        history: { replaceState() {} },
        localStorage: { getItem: () => null },
        document: { documentElement: { dataset: {} }, getElementById: () => ({}), querySelectorAll: () => [] },
      });

      assert.equal((await read(url, "/v1/hosts")).status, 401);
      assert.equal((await read(url, "/v1/unknown")).status, 401);
      assert.equal((await read(url, "/v1/hosts", { authorization: "Bearer " + "0".repeat(64) })).status, 401);
      assert.equal((await read(url, "/v1/hosts", { ...headers, host: "localhost:" + url.port })).status, 403);
      assert.equal((await read(url, "/v1/hosts", { ...headers, origin: "https://foreign.example" })).status, 403);
      assert.equal((await read(url, "/v1/decisions", headers, "POST")).status, 405);
      assert.equal((await read(url, "/", {}, "HEAD")).status, 405);
      assert.equal((await read(url, "/v1/budgets", headers, "PUT")).status, 405);
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
  await assert.rejects(startPlatformDashboard({ socketPath, publicUrl }), /must be given together/);
  await assert.rejects(startPlatformDashboard({ socketPath, tokenFile }), /must be given together/);
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
    location: { hash: "", pathname: "/", search: "" },
    history: { replaceState() {} },
    localStorage: { getItem: () => null },
    document: {
      documentElement: { dataset: { auth: "proxy" } },
      getElementById: (id: string) => id === "window" ? windowSelect : {},
      querySelectorAll: () => [],
    },
    AbortSignal, clearTimeout() {}, setTimeout() {}, setInterval() {},
    async fetch(path: string, options: RequestInit) {
      assert.equal(new Headers(options.headers).has("authorization"), false);
      calls.push(path);
      return { ok: false, status: path === "/v1/hosts" ? 401 : 403 };
    },
  });
  await assert.rejects(authorizationError, { message: "Dashboard authorization failed." });
  assert.deepEqual(calls, ["/v1/digest?window=13%3A00Z", "/v1/health/summary", "/v1/friction?status=open", "/v1/hosts"]);
  assert(windowSelect.onchange);
  windowSelect.value = "weekly";
  windowSelect.onchange();
  assert.deepEqual(calls.slice(4), ["/v1/digest?window=weekly", "/v1/health/summary", "/v1/friction?status=open"]);
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
