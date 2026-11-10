import { query, type SDKMessage } from "@anthropic-ai/claude-agent-sdk";
import { Codex, type CodexOptions, type ThreadEvent } from "@openai/codex-sdk";

export interface TurnResult {
  sessionId: string | null;
  text: string;
  status: "completed" | "cancelled";
  usage: { inputTokens: number; cachedInputTokens: number; outputTokens: number } | null;
  // Null means unverified, never the requested model.
  model: string | null;
  modelUsage?: Record<string, unknown>;
}

export interface Session {
  send(prompt: string, signal?: AbortSignal): Promise<TurnResult>;
}

export interface SessionOptions {
  cwd: string;
  model: string;
  sessionId?: string;
  onSessionId?: (id: string) => void;
  /** A Router uses a one-shot, read-only session; Workers retain their writable worktree. */
  sandbox?: "read-only" | "workspace-write";
  /** Git metadata paths a Codex Worker needs outside its worktree. */
  writableDirs?: string[];
}

export interface Runner {
  start(options: SessionOptions): Session;
  resume(options: SessionOptions & { sessionId: string }): Session;
}

type ClaudeQuery = (options: Parameters<typeof query>[0]) => AsyncIterable<SDKMessage>;
type CodexThread = { runStreamed: (prompt: string, options?: { signal?: AbortSignal }) => Promise<{ events: AsyncIterable<ThreadEvent> }> };
type CodexClient = Pick<Codex, "startThread" | "resumeThread">;

const CODEX_WORKER_PROFILE = "ostrom_worker";

/**
 * Codex protects `.git` under workspace-write roots, including a linked worktree's gitdir.
 * An explicit profile grants only the worktree and metadata paths Git changes for a commit.
 */
export function codexWorkerConfigOverrides(cwd: string, writableDirs: string[]): string[] {
  const filesystem = [`${JSON.stringify("/")}=\"read\"`, `${JSON.stringify(cwd)}=\"write\"`, ...writableDirs.map((dir) => `${JSON.stringify(dir)}=\"write\"`)].join(",");
  return [
    `default_permissions=${JSON.stringify(CODEX_WORKER_PROFILE)}`,
    `permissions.${CODEX_WORKER_PROFILE}={filesystem={${filesystem}},network={enabled=true}}`,
  ];
}

function requireOptions(options: SessionOptions): void {
  if (!options.cwd || !options.model) throw new Error("cwd and model are required");
}

export function claudeRunner(env: Record<string, string | undefined>, run: ClaudeQuery = query): Runner {
  function create(options: SessionOptions): Session {
    requireOptions(options);
    let id = options.sessionId ?? null;
    let busy = false;
    return { async send(prompt, signal) {
      if (busy) throw new Error("session already has a running turn");
      busy = true;
      const abortController = new AbortController();
      const abort = () => abortController.abort();
      signal?.addEventListener("abort", abort, { once: true });
      if (signal?.aborted) abort();
      let model: string | null = null;
      try {
        if (signal?.aborted) return { sessionId: id, text: "", status: "cancelled", usage: null, model };
        for await (const message of run({ prompt, options: {
          cwd: options.cwd, model: options.model, env, abortController,
          ...(options.sandbox === "read-only"
            ? { permissionMode: "plan" as const, disallowedTools: ["Bash", "Edit", "Write"] }
            : { permissionMode: "bypassPermissions" as const, allowDangerouslySkipPermissions: true }),
          ...(id === null ? {} : { resume: id }),
        } })) {
          if (message.type === "system" && message.subtype === "init") {
            if (id === null) options.onSessionId?.(message.session_id);
            id = message.session_id;
            model = message.model;
          }
          if (message.type === "result") {
            id = message.session_id;
            if (message.subtype !== "success" || message.is_error) throw new Error("Claude turn failed");
            return { sessionId: id, text: message.result, status: "completed", model, modelUsage: message.modelUsage,
              usage: { inputTokens: message.usage.input_tokens + message.usage.cache_creation_input_tokens, cachedInputTokens: message.usage.cache_read_input_tokens, outputTokens: message.usage.output_tokens } };
          }
        }
        throw new Error("Claude ended without a result");
      } catch (error) {
        if (signal?.aborted) return { sessionId: id, text: "", status: "cancelled", usage: null, model };
        throw error;
      } finally {
        signal?.removeEventListener("abort", abort);
        busy = false;
      }
    } };
  }
  return { start: create, resume: create };
}

export function codexRunner(config: CodexOptions, client?: CodexClient): Runner {
  function create(options: SessionOptions): Session {
    requireOptions(options);
    const workerConfig: CodexOptions = options.sandbox === "read-only" || options.writableDirs === undefined
      ? config
      : { ...config, configOverrides: [...(config.configOverrides ?? []), ...codexWorkerConfigOverrides(options.cwd, options.writableDirs)] };
    const codex = client ?? new Codex(workerConfig);
    let id = options.sessionId ?? null;
    let busy = false;
    const threadOptions = {
      workingDirectory: options.cwd,
      model: options.model,
      sandboxMode: options.sandbox ?? "workspace-write",
      networkAccessEnabled: options.sandbox !== "read-only",
      approvalPolicy: "never" as const,
      // A read-only session (the Router) runs in a scratch directory that is not a Git repository.
      ...(options.sandbox === "read-only" ? { skipGitRepoCheck: true } : {}),
    };
    const thread: CodexThread = id === null ? codex.startThread(threadOptions) : codex.resumeThread(id, threadOptions);
    return { async send(prompt, signal) {
      if (busy) throw new Error("session already has a running turn");
      busy = true;
      let text = "";
      try {
        if (signal?.aborted) return { sessionId: id, text, status: "cancelled", usage: null, model: null };
        const { events } = await thread.runStreamed(prompt, signal ? { signal } : {});
        for await (const event of events) {
          if (event.type === "thread.started") {
            if (id === null) options.onSessionId?.(event.thread_id);
            id = event.thread_id;
          }
          if (event.type === "item.completed" && event.item.type === "agent_message") text = event.item.text;
          if (event.type === "error") throw new Error(event.message);
          if (event.type === "turn.failed") throw new Error(event.error.message);
          if (event.type === "turn.completed") return { sessionId: id, text, status: "completed", model: null,
            usage: { inputTokens: event.usage.input_tokens - event.usage.cached_input_tokens, cachedInputTokens: event.usage.cached_input_tokens, outputTokens: event.usage.output_tokens } };
        }
        throw new Error("Codex ended without a result");
      } catch (error) {
        if (signal?.aborted) return { sessionId: id, text, status: "cancelled", usage: null, model: null };
        throw error;
      } finally { busy = false; }
    } };
  }
  return { start: create, resume: create };
}
