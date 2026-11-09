// Parsers for the `t3` CLI's `--json` output, narrowed to the fields milestone 1 reads.
// The pinned upstream version is in ./pin.ts; ./agreement.test.ts checks these parsers against
// the upstream schemas vendored at that commit. Unknown extra fields are ignored; a missing or
// mistyped field we read throws, so a changed shape fails loudly instead of being guessed at.

export const T3_THREAD_STATUSES = [
  "idle",
  "preparing",
  "queued",
  "starting",
  "running",
  "waiting",
  "completed",
  "interrupted",
  "failed",
  "cancelled",
  "rolled_back",
] as const;
export type T3ThreadStatus = (typeof T3_THREAD_STATUSES)[number];

const PULL_REQUEST_STATES = ["open", "closed", "merged"] as const;
export type T3PullRequestState = (typeof PULL_REQUEST_STATES)[number];

export const T3_PULL_REQUEST_SOURCES = ["manual", "created", "agent", "stack", "stack-dismissed"] as const;
/** How the PR came to be on the thread; `stack-dismissed` is one the user hid from it. */
export type T3PullRequestSource = (typeof T3_PULL_REQUEST_SOURCES)[number];

const MESSAGE_ROLES = ["user", "assistant"] as const;
export type T3MessageRole = (typeof MESSAGE_ROLES)[number];

/** `t3 thread start --wait --json` and `t3 thread send --wait --json`. */
export interface T3RunResult {
  readonly threadId: string;
  readonly runId: string;
  readonly status: T3ThreadStatus;
  /** The run's last assistant message; null unless the run completed. */
  readonly reply: string | null;
  /** The run's last error; null when it completed or is waiting for an answer. */
  readonly error: string | null;
}

/** `t3 thread start --json` without `--wait`: the thread was created and its first run queued. */
export interface T3LaunchResult {
  readonly threadId: string;
  readonly projectId: string;
  /** null when the thread was launched without a first message. */
  readonly runId: string | null;
}

export const T3_SEND_DELIVERIES = ["started", "queued", "steered", "restarted"] as const;
/** How T3 took a message: it started a run on an idle thread, or queued it behind the busy one. */
export type T3SendDelivery = (typeof T3_SEND_DELIVERIES)[number];

/** `t3 thread send --json` without `--wait`: the message was accepted as run `runId`. */
export interface T3SendResult {
  readonly threadId: string;
  readonly runId: string;
  readonly delivery: T3SendDelivery;
}

export interface T3LinkedPullRequest {
  readonly repository: string;
  readonly number: number;
  readonly url: string;
}

export interface T3PullRequest {
  readonly host: string;
  readonly repository: string;
  readonly number: number;
  readonly url: string;
  readonly source: T3PullRequestSource;
  /** null until T3 has read the PR from its host. */
  readonly state: T3PullRequestState | null;
}

export interface T3Thread {
  readonly threadId: string;
  readonly status: T3ThreadStatus;
  readonly activeRunId: string | null;
  readonly settled: boolean;
  readonly pendingRequestCount: number;
  readonly branch: string | null;
  readonly worktreePath: string | null;
  readonly linkedPullRequest: T3LinkedPullRequest | null;
  /** Upstream may omit the field; an omitted list reads as empty. */
  readonly pullRequests: readonly T3PullRequest[];
  /** When T3 last changed the thread, as printed; the Watchdog reads it as an ISO timestamp. */
  readonly updatedAt: string;
}

/**
 * Whether the thread is blocked on a question or approval until someone answers it. Status does
 * not say so: the scripted stall's Worker, blocked on its question, read `running` with
 * `pendingRequestCount` 1 at the pin.
 */
export function hasPendingRequest(thread: T3Thread): boolean {
  return thread.pendingRequestCount > 0;
}

export interface T3Message {
  readonly role: T3MessageRole;
  readonly runId: string | null;
  readonly text: string;
}

/** `t3 thread show <thread> --json`. Messages are the latest ones, oldest first. */
export interface T3ThreadShow {
  readonly thread: T3Thread;
  readonly messages: readonly T3Message[];
}

export class T3OutputError extends Error {
  override readonly name = "T3OutputError";
}

export function parseRunResult(stdout: string): T3RunResult {
  return readRunResult(parseJson(stdout), "$");
}

export function parseLaunchResult(stdout: string): T3LaunchResult {
  const value = record(parseJson(stdout), "$");
  return {
    threadId: string(value["threadId"], "$.threadId"),
    projectId: string(value["projectId"], "$.projectId"),
    runId: nullable(string, value["runId"], "$.runId"),
  };
}

export function parseSendResult(stdout: string): T3SendResult {
  const value = record(parseJson(stdout), "$");
  return {
    threadId: string(value["threadId"], "$.threadId"),
    runId: string(value["runId"], "$.runId"),
    delivery: oneOf(T3_SEND_DELIVERIES, value["delivery"], "$.delivery"),
  };
}

export function parseThreadShow(stdout: string): T3ThreadShow {
  const value = record(parseJson(stdout), "$");
  return {
    thread: readThread(value["thread"], "$.thread"),
    messages: array(value["messages"], "$.messages").map((item, index) =>
      readMessage(item, `$.messages[${index}]`),
    ),
  };
}

function readRunResult(input: unknown, path: string): T3RunResult {
  const value = record(input, path);
  return {
    threadId: string(value["threadId"], `${path}.threadId`),
    runId: string(value["runId"], `${path}.runId`),
    status: oneOf(T3_THREAD_STATUSES, value["status"], `${path}.status`),
    reply: nullable(string, value["reply"], `${path}.reply`),
    error: nullable(string, value["error"], `${path}.error`),
  };
}

function readThread(input: unknown, path: string): T3Thread {
  const value = record(input, path);
  const pullRequests = value["pullRequests"];
  return {
    threadId: string(value["threadId"], `${path}.threadId`),
    status: oneOf(T3_THREAD_STATUSES, value["status"], `${path}.status`),
    activeRunId: nullable(string, value["activeRunId"], `${path}.activeRunId`),
    settled: boolean(value["settled"], `${path}.settled`),
    pendingRequestCount: count(value["pendingRequestCount"], `${path}.pendingRequestCount`),
    branch: nullable(string, value["branch"], `${path}.branch`),
    worktreePath: nullable(string, value["worktreePath"], `${path}.worktreePath`),
    linkedPullRequest: nullable(
      readLinkedPullRequest,
      value["linkedPullRequest"],
      `${path}.linkedPullRequest`,
    ),
    pullRequests:
      pullRequests === undefined
        ? []
        : array(pullRequests, `${path}.pullRequests`).map((item, index) =>
            readPullRequest(item, `${path}.pullRequests[${index}]`),
          ),
    updatedAt: string(value["updatedAt"], `${path}.updatedAt`),
  };
}

function readLinkedPullRequest(input: unknown, path: string): T3LinkedPullRequest {
  const value = record(input, path);
  return {
    repository: string(value["repository"], `${path}.repository`),
    number: count(value["number"], `${path}.number`),
    url: string(value["url"], `${path}.url`),
  };
}

function readPullRequest(input: unknown, path: string): T3PullRequest {
  const value = record(input, path);
  return {
    host: string(value["host"], `${path}.host`),
    repository: string(value["repository"], `${path}.repository`),
    number: count(value["number"], `${path}.number`),
    url: string(value["url"], `${path}.url`),
    source: oneOf(T3_PULL_REQUEST_SOURCES, value["source"], `${path}.source`),
    state: nullable(
      (state, statePath) => oneOf(PULL_REQUEST_STATES, state, statePath),
      value["state"],
      `${path}.state`,
    ),
  };
}

function readMessage(input: unknown, path: string): T3Message {
  const value = record(input, path);
  return {
    role: oneOf(MESSAGE_ROLES, value["role"], `${path}.role`),
    runId: nullable(string, value["runId"], `${path}.runId`),
    text: string(value["text"], `${path}.text`),
  };
}

function parseJson(stdout: string): unknown {
  try {
    return JSON.parse(stdout);
  } catch (cause) {
    throw new T3OutputError("t3 printed output that is not JSON", { cause });
  }
}

function fail(path: string, expected: string, value: unknown): never {
  throw new T3OutputError(`${path}: expected ${expected}, got ${JSON.stringify(value) ?? "nothing"}`);
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return fail(path, "an object", value);
  }
  return value as Record<string, unknown>;
}

function array(value: unknown, path: string): readonly unknown[] {
  return Array.isArray(value) ? value : fail(path, "an array", value);
}

function string(value: unknown, path: string): string {
  return typeof value === "string" ? value : fail(path, "a string", value);
}

function boolean(value: unknown, path: string): boolean {
  return typeof value === "boolean" ? value : fail(path, "a boolean", value);
}

function count(value: unknown, path: string): number {
  return Number.isSafeInteger(value) && (value as number) >= 0
    ? (value as number)
    : fail(path, "a non-negative integer", value);
}

function oneOf<const T extends string>(options: readonly T[], value: unknown, path: string): T {
  return options.includes(value as T) ? (value as T) : fail(path, `one of ${options.join(", ")}`, value);
}

function nullable<T>(
  read: (value: unknown, path: string) => T,
  value: unknown,
  path: string,
): T | null {
  return value === null ? null : read(value, path);
}
