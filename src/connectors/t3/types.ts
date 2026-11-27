// Wire contracts from T3 Code v0.1.13 (bc0f04da).
export const T3_MCP_PROTOCOL_VERSION = "2026-07-28";

export interface T3TargetConfig {
  id: string;
  label: string;
  mcpUrl: string;
  originNamespace?: string;
}

export interface T3RpcRequest<Method extends string, Params> {
  jsonrpc: "2.0";
  id: number;
  method: Method;
  params: Params;
}

export interface T3RpcError {
  code: number;
  message: string;
  data?: unknown;
}

export type T3RpcResponse<Result> =
  | { jsonrpc: "2.0"; id: number; result: Result }
  | { jsonrpc: "2.0"; id: number | null; error: T3RpcError };

export interface T3McpClientInfo {
  name: string;
  version: string;
}

export interface T3McpRequestMetadata {
  "io.modelcontextprotocol/protocolVersion": typeof T3_MCP_PROTOCOL_VERSION;
  "io.modelcontextprotocol/clientCapabilities": Record<string, never>;
  "io.modelcontextprotocol/clientInfo": T3McpClientInfo;
}

// The 2026-07-28 protocol uses stateless discovery for initialization.
export type T3McpDiscoverRequest = T3RpcRequest<"server/discover", {
  _meta: T3McpRequestMetadata;
}>;

export interface T3McpDiscoverResult {
  supportedVersions: string[];
  capabilities: Record<string, unknown>;
  instructions?: string;
  _meta?: { "io.modelcontextprotocol/serverInfo"?: T3McpClientInfo };
}

interface T3EnvironmentArgs {
  environmentId?: string;
}

interface T3ThreadArgs extends T3EnvironmentArgs {
  threadId: string;
}

export interface T3EnvironmentRead {
  environmentId: string;
  label: string;
  serverVersion: string;
  platform: {
    os: "darwin" | "linux" | "windows" | "unknown";
    arch: "arm64" | "x64" | "other";
    machine?: "server" | "cloud" | "linux" | "desktop" | "laptop" | "mac-mini" | "mac-studio";
  };
  preferences: {
    defaultThreadEnvMode?: "local" | "worktree";
    newWorktreesStartFromOrigin: boolean;
    enableProviderUpdateChecks: boolean;
    backgroundActivity: { profile: "balanced" | "performance" | "battery-saver" | "custom" };
    sourceControlWritingStyle: {
      mode: string;
      followChangeRequestTemplates: boolean;
      customInstructions: string;
      truncated: boolean;
    };
  };
}

interface T3RepositoryIdentity {
  canonicalKey: string;
  locator: { source: "git-remote"; remoteName: string; remoteUrl: string };
  webUrl?: string;
  rootPath?: string;
  displayName?: string;
  provider?: string;
  owner?: string;
  name?: string;
  origin?: { canonicalKey: string; remoteUrl?: string; displayName?: string };
}

interface T3ProjectScript {
  id: string;
  name: string;
  command: string;
  icon: "play" | "test" | "lint" | "configure" | "build" | "debug";
  runOnWorktreeCreate: boolean;
  runOnSettle?: boolean;
  stopOnSettle?: boolean;
  async?: boolean;
  previewUrl?: string;
  autoOpenPreview?: boolean;
}

type T3ProjectIconColor =
  | "gray" | "red" | "orange" | "amber" | "yellow" | "lime" | "green" | "emerald"
  | "teal" | "cyan" | "sky" | "blue" | "indigo" | "violet" | "purple" | "fuchsia" | "pink" | "rose";

type T3ProjectIcon =
  | { kind: "lucide"; name: string; color: T3ProjectIconColor; monogramText?: string; monogram?: string }
  | { kind: "emoji"; name: string; color: T3ProjectIconColor }
  | { kind: "monogram"; text: string; color: T3ProjectIconColor };

export interface T3Project {
  id: string;
  title: string;
  workspaceRoot: string;
  repositoryIdentity?: T3RepositoryIdentity | null;
  faviconPath?: string | null;
  projectIcon?: T3ProjectIcon | null;
  defaultModelSelection: {
    instanceId: string;
    model: string;
    options?: { id: string; value: string | boolean }[];
  } | null;
  defaultThreadEnvMode?: "local" | "worktree" | null;
  autoPull?: boolean;
  scripts: T3ProjectScript[];
  trackedFolders?: { path: string; name: string; discovered?: boolean }[];
  pinnedAt?: string | null;
  archivedAt?: string | null;
  createdAt: string;
  updatedAt: string;
  deletedAt: string | null;
}

export type T3RunStatus =
  | "preparing" | "queued" | "starting" | "running" | "waiting"
  | "completed" | "interrupted" | "failed" | "cancelled" | "rolled_back";
export type T3ThreadStatus = "idle" | T3RunStatus;
type T3Actor = "user" | "agent" | "system";
type T3CreationSource = "web" | "mobile" | "mcp" | "provider" | "server";
export type T3PRState = "open" | "closed" | "merged";
type T3PRChainKind = "native" | "derived";

interface T3PRKey {
  host: string;
  repository: string;
  number: number;
}

interface T3PRLink extends T3PRKey {
  url: string;
  source: "manual" | "created" | "agent" | "stack" | "stack-dismissed";
  state: T3PRState | null;
  watching: boolean;
}

export interface T3ThreadPR extends T3PRLink {
  title: string | null;
  headBranch: string | null;
  baseBranch: string | null;
  isDraft: boolean | null;
  stack: { kind: T3PRChainKind; position: number; size: number } | null;
}

export interface T3ThreadListItem {
  threadId: string;
  projectId: string;
  title: string;
  link?: string;
  createdBy: T3Actor;
  creationSource: T3CreationSource;
  status: T3ThreadStatus;
  latestRunId: string | null;
  providerInstanceId: string;
  model: string;
  runtimeMode: "approval-required" | "auto-accept-edits" | "auto" | "full-access";
  interactionMode: "default" | "plan";
  linkedPullRequest: { projectId: string; repository: string; number: number; url: string } | null;
  pullRequests?: (T3PRLink & { watchStartedAt: string | null })[];
  pullRequestCount?: number;
  openPullRequestCount?: number;
  settled: boolean;
  settledAt: string | null;
  snoozed: boolean;
  snoozedUntil: string | null;
  pinned?: boolean;
  blockedOnThreadWait?: boolean;
  pendingBackgroundTasks?: {
    taskId: string;
    kind: "subagent" | "command" | "monitor" | "background_task";
    description?: string;
    awaited?: true;
    childThreadId?: string;
  }[];
  parentThreadId: string | null;
  relationshipToParent: "fork" | "subagent" | null;
  delegatedFrom?: { environmentId: string; threadId: string; title: string };
  itemCount: number;
  createdAt: string;
  updatedAt: string;
}

export interface T3ThreadDetail extends T3ThreadListItem {
  activeRunId: string | null;
  titleRegeneration: { requestId: string; startedAt: string } | null;
  lastTitleRegeneration?: {
    status: "renamed" | "unchanged" | "failed";
    message?: string;
    requestId: string;
    completedAt: string;
  } | null;
  branch: string | null;
  worktreePath: string | null;
  runCount: number;
  pendingRequestCount: number;
  archived: boolean;
}

export interface T3Run {
  // T3 run identity. The MCP projection has no native provider session ID.
  runId: string;
  ordinal: number;
  status: T3RunStatus;
  providerInstanceId: string;
  model: string;
  executedModel?: string | null;
  requestedAt: string;
  startedAt: string | null;
  completedAt: string | null;
}

export interface T3TimelineItem {
  position: number;
  visibility: "local" | "inherited" | "synthetic";
  sourceThreadId: string;
  itemId: string;
  subagentId?: string;
  runId: string | null;
  messageId: string | null;
  createdBy: T3Actor | null;
  creationSource: T3CreationSource | null;
  senderThreadId?: string;
  type: string;
  status: "idle" | "pending" | "running" | "waiting" | "completed" | "failed" | "cancelled" | "interrupted";
  title: string | null;
  text: string | null;
  textTruncated: boolean;
  nextTextOffset?: number | null;
  updatedAt: string;
}

export interface T3PendingQuestion {
  id: string;
  header: string;
  question: string;
  options: { label: string; description: string; value?: string; preview?: string }[];
  multiSelect?: boolean;
  allowCustomAnswer?: boolean;
  initialAnswer?: string;
  required?: boolean;
}

export type T3PendingRequestArgs = T3ThreadArgs & (
  | { action: "list" }
  | { action: "read"; requestId: string }
);

export interface T3ReadToolArgs {
  t3_environment_read: T3EnvironmentArgs;
  t3_project_list: T3EnvironmentArgs & { cursor?: number; limit?: number };
  t3_thread_list: T3EnvironmentArgs & {
    projectId?: string;
    allProjects?: boolean;
    statuses?: T3ThreadStatus[];
    titleContains?: string;
    settled?: boolean;
    snoozed?: boolean;
    includeSubagents?: boolean;
    cursor?: number;
    limit?: number;
  };
  t3_thread_read: T3ThreadArgs & {
    itemId?: string;
    textOffset?: number;
    view?: "messages" | "activity";
    afterPosition?: number;
    beforePosition?: number;
    limit?: number;
    runLimit?: number;
    maxCharsPerItem?: number;
  };
  t3_pending_request: T3PendingRequestArgs;
  list_thread_pull_requests: T3ThreadArgs & { states?: T3PRState[]; cursor?: string };
}

export interface T3ReadToolResults {
  t3_environment_read: T3EnvironmentRead;
  t3_project_list: { projects: T3Project[]; nextCursor: number | null };
  t3_thread_list: {
    projectId: string | null;
    currentThreadId: string | null;
    threads: T3ThreadListItem[];
    nextCursor: number | null;
    total: number;
  };
  t3_thread_read: {
    thread: T3ThreadDetail;
    recentRuns: T3Run[];
    items: T3TimelineItem[];
    nextPosition: number | null;
    hasMore: boolean;
    earlierPosition?: number | null;
  };
  t3_pending_request: { requestIds: string[] } | { requestId: string; questions: T3PendingQuestion[] };
  list_thread_pull_requests: {
    pullRequests: T3ThreadPR[];
    chains: { kind: T3PRChainKind; numbers: number[] }[];
    nextCursor: string | null;
  };
}

export type T3ReadToolName = keyof T3ReadToolArgs;

export type T3McpToolCallRequest = T3RpcRequest<"tools/call", {
  name: T3ReadToolName;
  arguments: T3ReadToolArgs[T3ReadToolName];
  _meta: T3McpRequestMetadata;
}>;

export interface T3McpToolResult {
  content: { type: string; text?: string }[];
  structuredContent?: Record<string, unknown>;
  isError?: boolean;
  resultType?: "complete" | "input_required";
}

interface T3MilestoneEnvelope {
  id: string;
  occurredAt: string;
  projectId: string | null;
  threadId: string | null;
  parentThreadId: string | null;
  actor: { type: T3Actor | "guest"; id?: string; label?: string };
  isSubagentChild: boolean;
  workflowRunId?: string;
}

type T3InputKind = "user_input" | "approval";

export type T3MilestoneEvent = T3MilestoneEnvelope & (
  | { type: "thread.run_settled"; runId: string; outcome: "completed" | "failed" | "interrupted" | "cancelled" }
  | { type: "thread.needs_input"; requestId: string; kind: T3InputKind }
  | { type: "thread.input_resolved"; requestId: string; kind: T3InputKind; resolution: "resolved" | "expired" | "cancelled" }
  | {
      type: "pr.watch_wake";
      pullRequests: T3PRKey[];
      outcome: "completed" | "failed" | "cancelled" | "updated" | "unknown";
      summary: string;
      watchEnded: boolean;
    }
);
export type T3MilestoneName = T3MilestoneEvent["type"];

export interface T3MilestoneFilter {
  projectIds?: string[];
  threadIds?: string[];
  workflowRunIds?: string[];
  includeSubagentChildren?: boolean;
}

export interface T3EventWindow {
  cursor?: string | null;
  maxAgeMs?: number;
}

interface T3EventRequestParams extends T3EventWindow {
  name: T3MilestoneName;
  arguments?: T3MilestoneFilter;
  _meta: T3McpRequestMetadata;
}

export type T3EventsPollRequest = T3RpcRequest<"events/poll", T3EventRequestParams & { maxEvents?: number }>;
export type T3EventsStreamRequest = T3RpcRequest<"events/stream", T3EventRequestParams & { heartbeatMs?: number }>;

export interface T3EventOccurrence<Name extends T3MilestoneName = T3MilestoneName> {
  eventId: string;
  name: Name;
  timestamp: string;
  data: Extract<T3MilestoneEvent, { type: Name }>;
  cursor?: string | null;
  _meta?: Record<string, unknown>;
}

export interface T3PollResult<Name extends T3MilestoneName = T3MilestoneName> {
  events: T3EventOccurrence<Name>[];
  cursor: string | null;
  truncated: boolean;
  hasMore: boolean;
  nextPollMs?: number;
}

export type T3EventsPollRpcResult<Name extends T3MilestoneName = T3MilestoneName> =
  T3PollResult<Name> & { nextPollMs: number };

export type T3EventsStreamNotification<Name extends T3MilestoneName = T3MilestoneName> = { jsonrpc: "2.0" } & (
  | { method: "notifications/events/active"; params: { cursor: string | null; truncated: boolean; heartbeatMs: number } }
  | { method: "notifications/events/heartbeat"; params: { cursor: string | null } }
  | { method: "notifications/events/event"; params: T3EventOccurrence<Name> & { cursor: string | null } }
  | { method: "notifications/events/error" | "notifications/events/terminated"; params: { error: T3RpcError } }
);

export type T3StreamItem<Name extends T3MilestoneName = T3MilestoneName> =
  | { kind: "event"; cursor: string | null; eventId: string; timestamp: string; data: T3EventOccurrence<Name>["data"] }
  | { kind: "activated"; cursor: string | null; truncated: boolean }
  | { kind: "heartbeat"; cursor: string | null }
  | { kind: "gap"; cursor: string | null };
