import assert from "node:assert/strict";
import { createServer, request } from "node:http";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { startDashboard } from "./dashboard.ts";

function call(url: string, path: string, headers: Record<string, string> = {}, method = "GET"): Promise<{ status: number; headers: import("node:http").IncomingHttpHeaders; body: string }> {
  const parsed = new URL(url);
  return new Promise((resolve, reject) => {
    const client = request({ hostname: parsed.hostname, port: Number(parsed.port), method, path, headers: { host: parsed.host, ...headers } }, (response) => {
      let body = ""; response.setEncoding("utf8"); response.on("data", (part: string) => { body += part; });
      response.on("end", () => resolve({ status: response.statusCode!, headers: response.headers, body }));
    });
    client.once("error", reject); client.end();
  });
}

test("dashboard bridge restricts loopback, credentials, host, origin, routes and methods", async () => {
  const directory = await mkdtemp(join(tmpdir(), "ostrom-dashboard-"));
  const socketDirectory = join(directory, "ostrom");
  await mkdir(socketDirectory);
  const socketPath = join(socketDirectory, "ostromd.sock");
  const forwarded: string[] = [];
  const fake = createServer((req, res) => { forwarded.push(`${req.method} ${req.url}`); res.writeHead(200, { "content-type": "application/json" }); res.end("{}") });
  await new Promise<void>((resolve, reject) => { fake.once("error", reject); fake.listen(socketPath, resolve); });
  const dash = await startDashboard({ XDG_RUNTIME_DIR: directory });
  try {
    const token = new URL(dash.url).hash.slice(1);
    assert.equal((await call(dash.url, "/v1/status")).status, 401);
    assert.equal((await call(dash.url, "/v1/status", { authorization: `Bearer ${"f".repeat(64)}` })).status, 401);
    assert.equal((await call(dash.url, "/v1/status", { host: "evil.test", authorization: `Bearer ${token}` })).status, 403);
    assert.equal((await call(dash.url, "/v1/status", { origin: "http://evil.test", authorization: `Bearer ${token}` })).status, 403);
    assert.equal((await call(dash.url, "/v1/preview", { authorization: `Bearer ${token}` })).status, 404);
    assert.equal((await call(dash.url, "/v1/status", { authorization: `Bearer ${token}` }, "POST")).status, 404);
    assert.deepEqual(forwarded, []);
    const proxied = await call(dash.url, "/v1/status", { authorization: `Bearer ${token}` });
    assert.equal(proxied.status, 200);
    assert.deepEqual(forwarded, ["GET /v1/status"]);
    assert.equal(proxied.headers["cache-control"], "no-store");
    assert.equal(proxied.headers["referrer-policy"], "no-referrer");
  } finally {
    await dash.close();
    await new Promise<void>((resolve, reject) => fake.close((error) => error ? reject(error) : resolve()));
  }
  await rm(socketDirectory, { recursive: true, force: true });
  const down = await startDashboard({ XDG_RUNTIME_DIR: directory });
  try {
    const token = new URL(down.url).hash.slice(1);
    const response = await call(down.url, "/v1/status", { authorization: `Bearer ${token}` });
    assert.equal(response.status, 502);
    assert.match(response.body, /unavailable/);
  } finally { await down.close(); await rm(directory, { recursive: true, force: true }); }
});
