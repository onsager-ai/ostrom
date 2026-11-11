import { sign } from "node:crypto";
import { readFile } from "node:fs/promises";

export interface AppConfig { appId: string; keyFile: string; }
export function loadAppConfig(env: Record<string, string | undefined>): AppConfig | undefined {
  const appId = env["OSTROM_GITHUB_APP_ID"]?.trim() || "";
  const keyFile = env["OSTROM_GITHUB_APP_KEY_FILE"]?.trim() || "";
  if (!appId && !keyFile) return undefined;
  if (!appId || !keyFile) throw new Error("OSTROM_GITHUB_APP_ID and OSTROM_GITHUB_APP_KEY_FILE must be set together");
  if (!/^[1-9]\d*$/.test(appId)) throw new Error("OSTROM_GITHUB_APP_ID must be a positive integer");
  return { appId, keyFile };
}

export function appJwt(appId: string, key: string, now: number): string {
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const seconds = Math.floor(now / 1000);
  const payload = `${encode({ alg: "RS256", typ: "JWT" })}.${encode({ iat: seconds - 60, exp: seconds + 540, iss: appId })}`;
  return `${payload}.${sign("RSA-SHA256", Buffer.from(payload), key).toString("base64url")}`;
}

export class GitHubApp {
  private cached?: { token: string; expires: number };
  private pending: Promise<string> | undefined;
  private config: AppConfig;
  private repo: string;
  private request: typeof fetch;
  private now: () => number;
  constructor(config: AppConfig, repo: string, request: typeof fetch = fetch, now = Date.now) {
    this.config = config;
    this.repo = repo;
    this.request = request;
    this.now = now;
  }

  private async api(path: string, authorization: string, method = "GET"): Promise<Record<string, unknown>> {
    // Never include response bodies or underlying errors: credentials must not reach logs.
    let response: Response;
    try {
      response = await this.request(`https://api.github.com/${path}`, { method, headers: {
        Authorization: `Bearer ${authorization}`, Accept: "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "ostrom",
      } });
    } catch { throw new Error("GitHub App request failed"); }
    if (!response.ok) throw new Error(`GitHub App request failed (HTTP ${response.status})`);
    try { return await response.json() as Record<string, unknown>; }
    catch { throw new Error("Invalid GitHub App response"); }
  }

  private async jwt(): Promise<string> {
    try { return appJwt(this.config.appId, await readFile(this.config.keyFile, "utf8"), this.now()); }
    catch { throw new Error("Cannot sign GitHub App JWT; check the private key file"); }
  }

  async token(): Promise<string> {
    if (this.cached && this.cached.expires - 180_000 > this.now()) return this.cached.token;
    if (this.pending) return this.pending;
    this.pending = this.mint();
    try { return await this.pending; } finally { this.pending = undefined; }
  }

  private async mint(): Promise<string> {
    const jwt = await this.jwt();
    const installation = await this.api(`repos/${this.repo}/installation`, jwt);
    if (!Number.isSafeInteger(installation["id"]) || Number(installation["id"]) < 1) throw new Error("Invalid GitHub App installation");
    const result = await this.api(`app/installations/${installation["id"]}/access_tokens`, jwt, "POST");
    const expires = Date.parse(String(result["expires_at"]));
    if (typeof result["token"] !== "string" || !result["token"] || !Number.isFinite(expires) || expires <= this.now() + 180_000) throw new Error("Invalid GitHub App token response");
    this.cached = { token: result["token"], expires };
    return this.cached.token;
  }

  async identity(): Promise<{ id: number; name: string; email: string }> {
    const app = await this.api("app", await this.jwt());
    if (String(app["id"]) !== this.config.appId || typeof app["slug"] !== "string" || !/^[\w-]+$/.test(app["slug"])) throw new Error("Invalid GitHub App identity");
    const name = `${app["slug"]}[bot]`;
    const bot = await this.api(`users/${encodeURIComponent(name)}`, await this.token());
    if (!Number.isSafeInteger(bot["id"]) || Number(bot["id"]) < 1 || bot["login"] !== name) throw new Error("Invalid GitHub App bot identity");
    const id = Number(bot["id"]);
    return { id, name, email: `${id}+${name}@users.noreply.github.com` };
  }
}
