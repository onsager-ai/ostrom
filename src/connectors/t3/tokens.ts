import { randomUUID } from "node:crypto";
import { constants } from "node:fs";
import { chmod, mkdir, open, rename, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";

export interface T3Token {
  accessToken: string;
  expiresAt: number; // Unix time in milliseconds.
}

export function defaultT3TokenDirectory(): string {
  const stateHome = process.env.XDG_STATE_HOME;
  const stateDir = stateHome && isAbsolute(stateHome) ? stateHome : join(homedir(), ".local", "state");
  return join(stateDir, "ostrom", "t3-tokens");
}

function tokenPath(dir: string, hostId: string): string {
  if (!/^[a-zA-Z0-9][a-zA-Z0-9_.-]*$/.test(hostId)) {
    throw new Error("Invalid T3 token host ID");
  }
  return join(dir, `${hostId}.json`);
}

export async function loadToken(hostId: string, dir = defaultT3TokenDirectory()): Promise<T3Token | null> {
  const path = tokenPath(dir, hostId);
  try {
    const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    try {
      const stat = await file.stat();
      if (!stat.isFile() || (stat.mode & 0o777) !== 0o600) {
        throw new Error("T3 token must be a regular file with mode 0600");
      }
      let token: Partial<T3Token> | null;
      try {
        token = JSON.parse(await file.readFile("utf8")) as Partial<T3Token> | null;
      } catch (error) {
        if (!(error instanceof SyntaxError)) throw error;
        throw new Error("Invalid T3 token file");
      }
      if (typeof token?.accessToken !== "string" || !token.accessToken ||
          typeof token.expiresAt !== "number" || !Number.isFinite(token.expiresAt)) {
        throw new Error("Invalid T3 token file");
      }
      return { accessToken: token.accessToken, expiresAt: token.expiresAt };
    } finally {
      await file.close();
    }
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw error;
  }
}

export async function saveToken(hostId: string, token: T3Token, dir = defaultT3TokenDirectory()): Promise<void> {
  const path = tokenPath(dir, hostId);
  await mkdir(dir, { recursive: true, mode: 0o700 });
  await chmod(dir, 0o700);
  const tempPath = join(dir, `.${randomUUID()}.tmp`);
  const temp = await open(tempPath, "wx", 0o600);
  try {
    try {
      await temp.chmod(0o600);
      await temp.writeFile(JSON.stringify(token));
    } finally {
      await temp.close();
    }
    await rename(tempPath, path);
  } finally {
    await rm(tempPath, { force: true });
  }
}

export async function deleteToken(hostId: string, dir = defaultT3TokenDirectory()): Promise<void> {
  await rm(tokenPath(dir, hostId), { force: true });
}

export function getTokenExpiryStatus(token: T3Token, now: number, warnDays = 7) {
  const { expiresAt } = token;
  const remainingMs = expiresAt - now;
  const expired = remainingMs <= 0;
  return { expired, warning: !expired && remainingMs <= warnDays * 86_400_000, remainingMs, expiresAt };
}
