import type { CodexOptions } from "@openai/codex-sdk";

// Credentials that route Claude through a gateway or an API key instead of the Claude Code login.
const CLAUDE_KEY_VARS = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"];

/**
 * Pass the operator's environment through to the SDKs; provider selection is configuration.
 * OSTROM_CLAUDE_AUTH=login drops gateway and API-key variables so Claude uses the Claude Code login.
 * OSTROM_CODEX_PROVIDER=openai with no API key makes Codex use the ChatGPT login.
 */
export function sessionConfig(env: Record<string, string | undefined>): {
  claudeEnv: Record<string, string | undefined>;
  codex: CodexOptions;
} {
  const claudeAuth = env["OSTROM_CLAUDE_AUTH"]?.trim() || "env";
  if (claudeAuth !== "env" && claudeAuth !== "login") throw new Error(`OSTROM_CLAUDE_AUTH must be env or login, not ${claudeAuth}`);
  const claudeEnv = { ...env };
  if (claudeAuth === "login") for (const name of CLAUDE_KEY_VARS) delete claudeEnv[name];
  const codexEnv = Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => entry[1] !== undefined));
  const provider = env["OSTROM_CODEX_PROVIDER"]?.trim();
  const appMode = Boolean(env["OSTROM_GITHUB_APP_ID"] && env["OSTROM_GITHUB_APP_KEY_FILE"]);
  // Login profiles can put the operator's gh ahead of the App helper, including in snapshots.
  const config = {
    ...(provider ? { model_provider: provider } : {}),
    ...(appMode ? { allow_login_shell: false, features: { shell_snapshot: false } } : {}),
  };
  return {
    claudeEnv,
    codex: { env: codexEnv, ...(provider || appMode ? { config } : {}) },
  };
}
