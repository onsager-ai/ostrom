import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { request } from "node:http";
import type { OutgoingHttpHeaders } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

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
      assert.match(page.body, /GitHub PR CI/);
      assert.match(page.body, /Budgets &amp; Spend/);
      assert.match(page.body, /Spend unavailable/);
      assert.match(page.body, /Estimated USD.*Reported USD/);
      assert.match(page.body, /Billed USD.*Coverage/);
      assert.match(page.body, /history\.replaceState/);
      assert(!page.body.includes("<script>invalid()</script>"));
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
