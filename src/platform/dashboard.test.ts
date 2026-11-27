import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { request } from "node:http";
import type { OutgoingHttpHeaders } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { runInNewContext } from "node:vm";

import { startPlatformApiServer } from "./api.ts";
import { startPlatformDashboard } from "./dashboard.ts";
import { PlatformStore } from "./store.ts";

function read(base: URL, path: string, headers: OutgoingHttpHeaders = {}, method = "GET") {
  return new Promise<{ status: number | undefined; headers: OutgoingHttpHeaders; body: string }>((resolve, reject) => {
    const req = request(new URL(path, base), { headers, method }, (response) => {
      let body = "";
      response.setEncoding("utf8").on("data", (chunk: string) => { body += chunk; });
      response.on("error", reject);
      response.on("end", () => resolve({ status: response.statusCode, headers: response.headers, body }));
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
        let firstToggle: (() => void) | undefined;
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
        document: { getElementById: () => ({}), querySelectorAll: () => [] },
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
