import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { generateKeyPairSync, verify } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { appJwt, GitHubApp, loadAppConfig } from "./app.ts";
import { githubRunner } from "./gh.ts";
import { loadConfig } from "../ostromd/config.ts";
import { workerAuth } from "./worker-auth.ts";

function keypair() {
  return generateKeyPairSync("rsa", { modulusLength: 2048, privateKeyEncoding: { type: "pkcs8", format: "pem" }, publicKeyEncoding: { type: "spki", format: "pem" } });
}

test("JWT uses RS256, a skewed iat, bounded expiry and the configured App ID", () => {
  const { privateKey, publicKey } = keypair();
  const now = Date.parse("2026-11-11T12:00:00Z");
  const jwt = appJwt("4544316", privateKey, now);
  const [header, claims, signature] = jwt.split(".") as [string, string, string];
  assert.deepEqual(JSON.parse(Buffer.from(header, "base64url").toString()), { alg: "RS256", typ: "JWT" });
  assert.deepEqual(JSON.parse(Buffer.from(claims, "base64url").toString()), { iss: "4544316", iat: now / 1000 - 60, exp: now / 1000 + 540 });
  assert.equal(verify("RSA-SHA256", Buffer.from(`${header}.${claims}`), publicKey, Buffer.from(signature, "base64url")), true);
});

test("configuration permits ambient auth and requires both App variables", () => {
  const base = { OSTROM_REPO: "o/r", OSTROM_DB: ":memory:", OSTROM_ALLOWLIST: "operator" };
  assert.equal(loadAppConfig(base), undefined);
  assert.doesNotThrow(() => loadConfig(base));
  for (const partial of [{ OSTROM_GITHUB_APP_ID: "4544316" }, { OSTROM_GITHUB_APP_KEY_FILE: "/key" }]) {
    assert.throws(() => loadConfig({ ...base, ...partial }), /must be set together/);
  }
  assert.deepEqual(loadAppConfig({ OSTROM_GITHUB_APP_ID: "4544316", OSTROM_GITHUB_APP_KEY_FILE: "/key" }), { appId: "4544316", keyFile: "/key" });
  assert.throws(() => loadAppConfig({ OSTROM_GITHUB_APP_ID: "bad", OSTROM_GITHUB_APP_KEY_FILE: "/key" }), /positive integer/);
});

test("installation lookup, token cache, expiry, concurrency, and renamed bot identity", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-app-test-"));
  try {
    const { privateKey, publicKey } = keypair();
    const keyFile = join(root, "key.pem");
    await writeFile(keyFile, privateKey, { mode: 0o600 });
    let now = Date.parse("2026-11-11T12:00:00Z");
    let minted = 0;
    const calls: string[] = [];
    const request: typeof fetch = async (input, init) => {
      const path = String(input).replace("https://api.github.com/", "");
      calls.push(path);
      const authorization = (init!.headers as Record<string, string>)["Authorization"]!;
      if (!path.startsWith("users/")) {
        const [header, claims, signature] = authorization.slice(7).split(".") as [string, string, string];
        assert.equal(verify("RSA-SHA256", Buffer.from(`${header}.${claims}`), publicKey, Buffer.from(signature, "base64url")), true);
      }
      if (path === "repos/o/r/installation") return Response.json({ id: 7 });
      if (path === "app/installations/7/access_tokens") {
        assert.equal(init!.method, "POST");
        return Response.json({ token: `test-token-${++minted}`, expires_at: new Date(now + 3600_000).toISOString() });
      }
      if (path === "app") return Response.json({ id: 4544316, slug: "renamed-app" });
      assert.equal(path, "users/renamed-app%5Bbot%5D");
      assert.equal(authorization, `Bearer test-token-${minted}`);
      return Response.json({ id: 123, login: "renamed-app[bot]" });
    };
    const app = new GitHubApp({ appId: "4544316", keyFile }, "o/r", request, () => now);
    assert.deepEqual(await Promise.all([app.token(), app.token()]), ["test-token-1", "test-token-1"]);
    now += 56 * 60_000;
    assert.equal(await app.token(), "test-token-1");
    now += 60_000;
    assert.equal(await app.token(), "test-token-2");
    assert.equal(calls.filter((path) => path === "repos/o/r/installation").length, 2);
    assert.deepEqual(await app.identity(), { id: 123, name: "renamed-app[bot]", email: "123+renamed-app[bot]@users.noreply.github.com" });
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("GH_TOKEN reaches the child environment without appearing in argv or errors", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-gh-env-"));
  try {
    await writeFile(join(root, "gh"), `#!${process.execPath}\nif (process.argv.includes('fail')) { process.stderr.write('gh: '+process.env.GH_TOKEN+' (HTTP 403)'); process.exitCode=1; } else process.stdout.write(JSON.stringify({token:process.env.GH_TOKEN,args:process.argv.slice(2)}));\n`, { mode: 0o700 });
    const gh = githubRunner(async () => "test-secret", { PATH: root });
    assert.deepEqual(JSON.parse(await gh(["api", "user"])), { token: "test-secret", args: ["api", "user"] });
    await assert.rejects(gh(["api", "fail"]), { message: "gh api fail failed: gh: [redacted] (HTTP 403)" });
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("Worker environment installs on-demand helpers and bot commit identity without a saved token", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-worker-auth-"));
  try {
    await writeFile(join(root, "gh"), "#!/bin/sh\nexit 0\n", { mode: 0o700 });
    const app = { identity: async () => ({ id: 123, name: "renamed[bot]", email: "123+renamed[bot]@users.noreply.github.com" }) } as GitHubApp;
    const auth = await workerAuth(app, { PATH: root, OSTROM_REPO: "o/r", GH_TOKEN: "ambient", GITHUB_TOKEN: "ambient" });
    try {
      assert.equal(auth.env["GH_TOKEN"], undefined);
      assert.equal(auth.env["GITHUB_TOKEN"], undefined);
      assert.equal(auth.env["GIT_AUTHOR_NAME"], "renamed[bot]");
      assert.equal(auth.env["GIT_COMMITTER_EMAIL"], "123+renamed[bot]@users.noreply.github.com");
      assert.equal(auth.env["GIT_CONFIG_VALUE_0"], "");
      assert.equal(auth.env["GIT_CONFIG_KEY_1"], "credential.https://github.com.helper");
      assert.match(auth.env["GIT_CONFIG_VALUE_1"]!, /worker-credential\.ts/);
      assert.equal(auth.env["GIT_CONFIG_VALUE_3"], "git@github.com:o/r");
      assert.equal(Object.values(auth.env).some((value) => value?.includes("ambient")), false);
      const exec = promisify(execFile);
      const gitEnv = { ...auth.env, PATH: `${auth.env["PATH"]}:${process.env["PATH"] ?? ""}` };
      const rewritten = await exec("git", ["ls-remote", "--get-url", "git@github.com:o/r.git"], { env: gitEnv });
      assert.equal(rewritten.stdout.trim(), "https://github.com/o/r.git");
      const helpers = await exec("git", ["config", "--get-all", "credential.https://github.com.helper"], { env: gitEnv });
      assert.match(helpers.stdout, /^!/);
      // store/erase must never mint or persist a credential, even without a key file.
      const wrapper = join(auth.env["PATH"]!.split(":")[0]!, "git-credential-ostrom");
      assert.equal((await exec(wrapper, ["store"], { env: gitEnv })).stdout, "");
    } finally { await auth.close(); }
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("App errors do not include response bodies, keys or transport credentials", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-app-error-"));
  try {
    const keyFile = join(root, "key");
    await writeFile(keyFile, keypair().privateKey);
    const app = new GitHubApp({ appId: "1", keyFile }, "o/r", async () => new Response("sensitive body", { status: 403 }));
    await assert.rejects(app.token(), { message: "GitHub App request failed (HTTP 403)" });
    const broken = new GitHubApp({ appId: "1", keyFile }, "o/r", async () => { throw new Error("sensitive transport"); });
    await assert.rejects(broken.token(), { message: "GitHub App request failed" });
  } finally { await rm(root, { recursive: true, force: true }); }
});
