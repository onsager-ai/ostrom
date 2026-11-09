import type { CodexOptions } from "@openai/codex-sdk";

/** Pass the operator's environment through to the SDK; provider selection remains configuration. */
export function sessionConfig(env: Record<string, string | undefined>): {
  claudeEnv: Record<string, string | undefined>;
  codex: CodexOptions;
} {
  const codexEnv = Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => entry[1] !== undefined));
  const provider = env["OSTROM_CODEX_PROVIDER"]?.trim();
  return {
    claudeEnv: { ...env },
    codex: { env: codexEnv, ...(provider ? { config: { model_provider: provider } } : {}) },
  };
}
