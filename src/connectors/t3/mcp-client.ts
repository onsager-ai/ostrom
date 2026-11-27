import { T3_MCP_PROTOCOL_VERSION } from "./types.ts";
import type {
  T3EventWindow,
  T3EventsPollRequest,
  T3EventsPollRpcResult,
  T3EventsStreamNotification,
  T3EventsStreamRequest,
  T3McpDiscoverRequest,
  T3McpDiscoverResult,
  T3McpRequestMetadata,
  T3McpToolCallRequest,
  T3McpToolResult,
  T3MilestoneFilter,
  T3MilestoneName,
  T3PollResult,
  T3ReadToolArgs,
  T3ReadToolName,
  T3ReadToolResults,
  T3RpcError,
  T3RpcResponse,
  T3StreamItem,
  T3TargetConfig,
} from "./types.ts";

const readTools = {
  t3_environment_read: true,
  t3_project_list: true,
  t3_thread_list: true,
  t3_thread_read: true,
  t3_pending_request: true,
  list_thread_pull_requests: true,
} satisfies Record<T3ReadToolName, true>;

export class T3ReadOnlyPolicyError extends Error {
  override name = "T3ReadOnlyPolicyError";
}

export class T3AuthError extends Error {
  override name = "T3AuthError";
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.status = status;
  }
}

export class T3JsonRpcError extends Error {
  override name = "T3JsonRpcError";
  readonly code: number;
  readonly data: unknown;

  constructor(error: T3RpcError) {
    super(error.message);
    this.code = error.code;
    this.data = error.data;
  }
}

export class T3ToolError extends Error {
  override name = "T3ToolError";
  readonly tool: T3ReadToolName;

  constructor(tool: T3ReadToolName, message: string) {
    super(message);
    this.tool = tool;
  }
}

type Request = T3McpDiscoverRequest | T3McpToolCallRequest | T3EventsPollRequest | T3EventsStreamRequest;

type PollOptions = T3EventWindow & { maxEvents?: number; signal?: AbortSignal };
type StreamOptions = T3EventWindow & { heartbeatMs?: number; signal?: AbortSignal };

function raiseRpcError(error: T3RpcError): never {
  if (error.message === "Access revoked" || error.message === "Forbidden: Access revoked") {
    throw new T3AuthError("Access revoked");
  }
  throw new T3JsonRpcError(error);
}

function rpcResult<Result>(message: T3RpcResponse<Result>, request: Request): Result {
  if (message?.jsonrpc !== "2.0" || (message.id !== request.id && !("error" in message && message.id === null))) {
    throw new Error(`Invalid JSON-RPC response for ${request.method}`);
  }
  if ("error" in message) raiseRpcError(message.error);
  if (!("result" in message)) throw new Error(`Missing JSON-RPC result for ${request.method}`);
  return message.result;
}

function isEventStream(response: Response): boolean {
  return response.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase() === "text/event-stream";
}

async function* sseData(response: Response): AsyncGenerator<string> {
  if (!response.body) throw new Error("Missing T3 event stream body");
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let data: string[] = [];
  try {
    while (true) {
      const { value, done } = await reader.read();
      buffer += decoder.decode(value, { stream: !done });
      while (true) {
        const end = buffer.search(/[\r\n]/);
        if (end < 0 || (!done && buffer[end] === "\r" && end === buffer.length - 1)) break;
        const line = buffer.slice(0, end);
        const newlineLength = buffer.startsWith("\r\n", end) ? 2 : 1;
        buffer = buffer.slice(end + newlineLength);
        if (line === "" && data.length > 0) {
          yield data.join("\n");
          data = [];
        } else if (line === "data") {
          data.push("");
        } else if (line.startsWith("data:")) {
          data.push(line.slice(line[5] === " " ? 6 : 5));
        }
      }
      if (done) return;
    }
  } finally {
    try {
      await reader.cancel();
    } catch {
      // A failed or aborted body is already closed.
    } finally {
      reader.releaseLock();
    }
  }
}

export class T3McpClient {
  readonly #mcpUrl: string;
  readonly #accessToken: string;
  #nextId = 1;
  readonly #metadata: T3McpRequestMetadata = {
    "io.modelcontextprotocol/protocolVersion": T3_MCP_PROTOCOL_VERSION,
    "io.modelcontextprotocol/clientCapabilities": {},
    "io.modelcontextprotocol/clientInfo": { name: "ostrom", version: "0.1.0" },
  };

  constructor(target: T3TargetConfig, accessToken: string) {
    this.#mcpUrl = target.mcpUrl;
    this.#accessToken = accessToken;
  }

  async initialize(signal?: AbortSignal): Promise<T3McpDiscoverResult> {
    // T3's initialize RPC selects a stateful version (2025-06-18). SEP-3415
    // uses the stateless 2026-07-28 protocol and negotiates with server/discover.
    const result = await this.#request<T3McpDiscoverResult>({
      jsonrpc: "2.0",
      id: this.#nextId++,
      method: "server/discover",
      params: { _meta: this.#metadata },
    }, signal);
    if (!result.supportedVersions.includes(T3_MCP_PROTOCOL_VERSION)) {
      throw new Error(`T3 does not support MCP ${T3_MCP_PROTOCOL_VERSION}`);
    }
    return result;
  }

  async callTool<Name extends T3ReadToolName>(
    name: Name,
    args: T3ReadToolArgs[NoInfer<Name>],
    signal?: AbortSignal,
  ): Promise<T3ReadToolResults[Name]> {
    if (typeof name !== "string" || !Object.hasOwn(readTools, name)) {
      throw new T3ReadOnlyPolicyError("Tool is outside the T3 read-only allowlist");
    }

    let wireArgs: T3ReadToolArgs[T3ReadToolName] = args;
    if (name === "t3_pending_request") {
      const pending = args as Partial<T3ReadToolArgs["t3_pending_request"]> | null | undefined;
      const action = pending?.action;
      if (action !== "list" && action !== "read") {
        throw new T3ReadOnlyPolicyError("t3_pending_request only permits list and read");
      }
      // Use the checked action in the outgoing payload.
      wireArgs = {
        action,
        environmentId: pending?.environmentId,
        threadId: pending?.threadId,
        requestId: pending && "requestId" in pending ? pending.requestId : undefined,
      } as T3ReadToolArgs["t3_pending_request"];
    }

    const result = await this.#request<T3McpToolResult>({
      jsonrpc: "2.0",
      id: this.#nextId++,
      method: "tools/call",
      params: { name, arguments: wireArgs, _meta: this.#metadata },
    }, signal);
    const text = result.content?.find((item) => item.type === "text")?.text;
    if (result.isError) throw new T3ToolError(name, text ?? `T3 tool ${name} failed`);
    if (result.resultType === "input_required") {
      throw new T3ToolError(name, `T3 tool ${name} requested client input`);
    }
    if (result.structuredContent) return result.structuredContent as unknown as T3ReadToolResults[Name];
    if (text !== undefined) return JSON.parse(text) as T3ReadToolResults[Name];
    throw new T3ToolError(name, `T3 tool ${name} returned no content`);
  }

  readEnvironment(args: T3ReadToolArgs["t3_environment_read"] = {}, signal?: AbortSignal) {
    return this.callTool("t3_environment_read", args, signal);
  }

  listProjects(args: T3ReadToolArgs["t3_project_list"] = {}, signal?: AbortSignal) {
    return this.callTool("t3_project_list", args, signal);
  }

  listThreads(args: T3ReadToolArgs["t3_thread_list"] = {}, signal?: AbortSignal) {
    return this.callTool("t3_thread_list", args, signal);
  }

  readThread(args: T3ReadToolArgs["t3_thread_read"], signal?: AbortSignal) {
    return this.callTool("t3_thread_read", args, signal);
  }

  pendingRequest(args: T3ReadToolArgs["t3_pending_request"], signal?: AbortSignal) {
    return this.callTool("t3_pending_request", args, signal);
  }

  listThreadPullRequests(args: T3ReadToolArgs["list_thread_pull_requests"], signal?: AbortSignal) {
    return this.callTool("list_thread_pull_requests", args, signal);
  }

  async pollEvents<Name extends T3MilestoneName>(
    name: Name,
    args: T3MilestoneFilter = {},
    { signal, ...window }: PollOptions = {},
  ): Promise<T3PollResult<Name>> {
    const { events, cursor, truncated, hasMore, nextPollMs } = await this.#request<T3EventsPollRpcResult<Name>>({
      jsonrpc: "2.0",
      id: this.#nextId++,
      method: "events/poll",
      params: { ...window, name, arguments: args, _meta: this.#metadata },
    }, signal);
    return { events, cursor, truncated, hasMore, ...(nextPollMs === undefined ? {} : { nextPollMs }) };
  }

  async *streamEvents<Name extends T3MilestoneName>(
    name: Name,
    args: T3MilestoneFilter = {},
    { signal, ...window }: StreamOptions = {},
  ): AsyncGenerator<T3StreamItem<Name>> {
    const request: T3EventsStreamRequest = {
      jsonrpc: "2.0",
      id: this.#nextId++,
      method: "events/stream",
      params: { ...window, name, arguments: args, _meta: this.#metadata },
    };
    const response = await this.#post(request, signal);
    if (!isEventStream(response)) {
      await this.#readResponse(response, request);
      throw new Error("T3 events/stream did not return an event stream");
    }

    let activated = false;
    for await (const data of sseData(response)) {
      signal?.throwIfAborted();
      const message = JSON.parse(data) as T3EventsStreamNotification<Name> | T3RpcResponse<unknown>;
      if (!("method" in message)) {
        rpcResult(message, request);
        if (!response.ok) throw new Error(`T3 events/stream failed with HTTP ${response.status}`);
        return;
      }
      switch (message.method) {
        case "notifications/events/active": {
          const { cursor, truncated } = message.params;
          // T3 also uses active frames to report gaps after opening the stream.
          if (!activated || !truncated) yield { kind: "activated", cursor, truncated };
          activated = true;
          if (truncated) yield { kind: "gap", cursor };
          break;
        }
        case "notifications/events/heartbeat":
          yield { kind: "heartbeat", cursor: message.params.cursor };
          break;
        case "notifications/events/event": {
          const { cursor, eventId, timestamp, data: milestone } = message.params;
          yield { kind: "event", cursor, eventId, timestamp, data: milestone };
          break;
        }
        case "notifications/events/error":
        case "notifications/events/terminated":
          raiseRpcError(message.params.error);
      }
    }
    signal?.throwIfAborted();
    if (!response.ok) throw new Error(`T3 events/stream failed with HTTP ${response.status}`);
  }

  async #post(request: Request, signal?: AbortSignal): Promise<Response> {
    const headers: Record<string, string> = {
      authorization: `Bearer ${this.#accessToken}`,
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      "mcp-protocol-version": T3_MCP_PROTOCOL_VERSION,
      "mcp-method": request.method,
    };
    if ("name" in request.params) headers["mcp-name"] = request.params.name;
    const response = await fetch(this.#mcpUrl, {
      method: "POST",
      headers,
      body: JSON.stringify(request),
      signal: signal ?? null,
      redirect: "error",
    });
    if (response.status === 401 || response.status === 403) {
      await response.body?.cancel();
      throw new T3AuthError(`T3 rejected MCP access (HTTP ${response.status})`, response.status);
    }
    return response;
  }

  async #readResponse<Result>(response: Response, request: Request): Promise<Result> {
    let message: T3RpcResponse<Result> | undefined;
    if (isEventStream(response)) {
      for await (const data of sseData(response)) {
        message = JSON.parse(data) as T3RpcResponse<Result>;
        break;
      }
    } else if (response.headers.get("content-type")?.startsWith("application/json")) {
      message = await response.json() as T3RpcResponse<Result>;
    } else {
      await response.body?.cancel();
    }
    if (message === undefined) throw new Error(`T3 ${request.method} returned no JSON-RPC response (HTTP ${response.status})`);
    const result = rpcResult(message, request);
    if (!response.ok) throw new Error(`T3 ${request.method} failed with HTTP ${response.status}`);
    return result;
  }

  async #request<Result>(request: Request, signal?: AbortSignal): Promise<Result> {
    return this.#readResponse<Result>(await this.#post(request, signal), request);
  }
}
