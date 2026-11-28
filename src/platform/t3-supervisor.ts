import { setTimeout as delay } from "node:timers/promises";

import { getTokenExpiryStatus, loadToken, T3AuthError, T3JsonRpcError, T3McpClient, T3ToolError } from "../connectors/t3/index.ts";
import type {
  T3MilestoneEvent, T3MilestoneName, T3ThreadListItem, T3ThreadPR,
} from "../connectors/t3/index.ts";
import type { PlatformT3Host } from "./config.ts";
import type { PlatformStore, T3HostUpdate, T3PrInput, T3ThreadSnapshot, T3WaitInput } from "./store.ts";
import type { AgentStatus, JsonValue, RunStatus, T3HostState } from "./types.ts";

const milestones = ["thread.run_settled", "thread.needs_input", "thread.input_resolved", "pr.watch_wake"] as const;
const filter = { includeSubagentChildren: true };
const pageSize = 50;
const concurrency = 3;
const runLimit = 10;
const detailPageLimit = 10;
const activeRefreshMs = 5 * 60_000;

interface ReconcileOptions {
  signal?: AbortSignal;
  now?: () => Date;
  tokenExpiresAt?: string;
  anchors?: ReadonlyMap<T3MilestoneName, string | null>;
  resetCursors?: boolean;
  activeOnly?: boolean;
  threadLimit?: number;
  listPageLimit?: number;
}

interface ThreadContext {
  host: PlatformT3Host;
  environmentId: string;
  laneId: string | null;
}

function rpcSignal(signal?: AbortSignal): AbortSignal {
  const timeout = AbortSignal.timeout(15_000);
  return signal === undefined ? timeout : AbortSignal.any([signal, timeout]);
}

function hostUpdate(host: PlatformT3Host, connectionState: T3HostState["connectionState"]): T3HostUpdate {
  return { hostId: host.id, label: host.label, mcpUrl: host.mcpUrl, webUrl: host.webUrl ?? null,
    originNamespace: host.originNamespace ?? null, connectionState };
}

function agentId(hostId: string, environmentId: string, threadId: string): string {
  return JSON.stringify(["t3", hostId, environmentId, threadId]);
}

function runId(agent: string, externalRunId: string): string {
  return JSON.stringify([agent, externalRunId]);
}

function isRunning(status: T3ThreadListItem["status"]): status is "preparing" | "queued" | "starting" | "running" {
  return status === "preparing" || status === "queued" || status === "starting" || status === "running";
}

function projectPr(context: ThreadContext, threadId: string, pr: T3ThreadPR): T3PrInput {
  return {
    hostId: context.host.id, threadId, prHost: pr.host, repository: pr.repository, number: pr.number,
    url: pr.url, source: pr.source, state: pr.state, watching: pr.watching,
    title: pr.title, headBranch: pr.headBranch, baseBranch: pr.baseBranch, isDraft: pr.isDraft,
  };
}

async function readPrs(client: T3McpClient, context: ThreadContext, threadId: string, signal?: AbortSignal) {
  const prs: T3PrInput[] = [];
  let cursor: string | null = null;
  for (let page = 0; page < detailPageLimit; page++) {
    const result = await client.listThreadPullRequests({ threadId, ...(cursor === null ? {} : { cursor }) }, rpcSignal(signal));
    prs.push(...result.pullRequests.map((pr) => projectPr(context, threadId, pr)));
    cursor = result.nextCursor;
    if (cursor === null) return prs;
  }
  throw new Error("T3 pull request page limit exceeded");
}

const approvalPlaceholder = "platform:approval:";

async function readWaits(client: T3McpClient, store: PlatformStore, context: ThreadContext, threadId: string,
  occurredAt: string, pendingCount: number, event?: T3MilestoneEvent, signal?: AbortSignal): Promise<T3WaitInput[]> {
  const result = await client.pendingRequest({ threadId, action: "list" }, rpcSignal(signal));
  if (!("requestIds" in result)) throw new Error("T3 pending request list is invalid");
  if (result.requestIds.length > 50) throw new Error("T3 pending request limit exceeded");
  const waits: T3WaitInput[] = [];
  for (const requestId of result.requestIds) {
    const pending = await client.pendingRequest({ threadId, action: "read", requestId }, rpcSignal(signal));
    if (!("requestId" in pending)) throw new Error("T3 pending request detail is invalid");
    const questions = pending.questions as unknown as JsonValue;
    const at = event?.type === "thread.needs_input" && event.requestId === requestId ? event.occurredAt : occurredAt;
    waits.push({ hostId: context.host.id, threadId, requestId, kind: "user_input", questions, occurredAt: at });
  }
  // T3 v0.1.13 exposes approval counts in thread reads and IDs only in milestones.
  const approvalCount = pendingCount - waits.length;
  if (approvalCount <= 0) return waits;
  if (approvalCount > 50) throw new Error("T3 pending approval limit exceeded");
  const known = new Map(store.listOpenT3Waits(context.host.id, threadId)
    .filter((wait) => wait.kind === "approval" && !wait.requestId.startsWith(approvalPlaceholder))
    .map((wait) => [wait.requestId, wait.occurredAt]));
  if (event?.type === "thread.input_resolved") known.delete(event.requestId);
  // If a gap made it ambiguous which requests survived, keep the count and use placeholders.
  if (known.size > approvalCount) known.clear();
  if (event?.type === "thread.needs_input" && event.kind === "approval") {
    if (!known.has(event.requestId) && known.size >= approvalCount) known.clear();
    known.set(event.requestId, event.occurredAt);
  }
  for (let index = known.size; index < approvalCount; index++) known.set(approvalPlaceholder + index, occurredAt);
  for (const [requestId, at] of known) {
    waits.push({ hostId: context.host.id, threadId, requestId, kind: "approval", questions: null, occurredAt: at });
  }
  return waits;
}

function isThreadNotFound(error: unknown): boolean {
  if (error instanceof T3ToolError) {
    if (error.tool !== "t3_thread_read") return false;
    if (error.code !== undefined) return error.code === "thread_not_found";
  } else if (error instanceof T3JsonRpcError) {
    const data = error.data;
    if (data && typeof data === "object" && "code" in data) return data.code === "thread_not_found";
  } else {
    return false;
  }
  return /\bthread_not_found\b|^no such thread\b|^thread\b.*\b(?:not found|no longer available)\b/i.test(error.message);
}

async function readSnapshot(client: T3McpClient, store: PlatformStore, context: ThreadContext, threadId: string,
  { details, signal, event }: { details: boolean; signal?: AbortSignal | undefined; event?: T3MilestoneEvent }): Promise<T3ThreadSnapshot | null> {
  const result = await client.readThread({ threadId, view: "messages", limit: 5, runLimit, maxCharsPerItem: 256 }, rpcSignal(signal))
    .catch((error: unknown) => {
      if (isThreadNotFound(error)) return null;
      throw error;
    });
  if (result === null) return null;
  const { thread, recentRuns } = result;
  const relevant = !thread.settled || thread.pendingRequestCount > 0 || thread.activeRunId !== null;
  const waits = (details && relevant) || thread.pendingRequestCount > 0
    ? await readWaits(client, store, context, threadId, thread.updatedAt, thread.pendingRequestCount, event, signal) : [];
  const prs = details || event?.type === "pr.watch_wake"
    ? await readPrs(client, context, threadId, signal) : undefined;

  const waitingForApproval = waits.some((wait) => wait.kind === "approval");
  const waitingStatus = waitingForApproval ? "waiting_approval" : "waiting_input";
  const status: AgentStatus = thread.archived ? "offline"
    : thread.status === "waiting" || waits.length > 0 ? "waiting"
    : isRunning(thread.status) ? "active" : "idle";
  const id = agentId(context.host.id, context.environmentId, threadId);
  const runs: T3ThreadSnapshot["runs"] = recentRuns.map((run) => {
    const runStatus: RunStatus = run.status === "waiting" ? waitingStatus
      : isRunning(run.status) ? "running"
      : run.status === "rolled_back" ? "interrupted" : run.status;
    return {
      id: runId(id, run.runId), agentId: id, laneId: context.laneId, connectorKind: "t3", hostId: context.host.id,
      externalRunRef: run.runId, model: run.executedModel ?? run.model,
      status: runStatus, startedAt: run.startedAt ?? run.requestedAt, completedAt: run.completedAt,
      failureCount: runStatus === "failed" ? 1 : 0,
    };
  });

  if (event?.type === "thread.run_settled") {
    const matched = runs.find((run) => run.externalRunRef === event.runId);
    if (matched === undefined) {
      runs.push({
        id: runId(id, event.runId), agentId: id, laneId: context.laneId, connectorKind: "t3", hostId: context.host.id,
        externalRunRef: event.runId, status: event.outcome, completedAt: event.occurredAt,
        failureCount: event.outcome === "failed" ? 1 : 0,
      });
    } else if (matched.completedAt == null) {
      matched.status = event.outcome;
      matched.completedAt = event.occurredAt;
      matched.failureCount = event.outcome === "failed" ? 1 : 0;
    }
  }

  return {
    threadId,
    agent: { id, role: thread.relationshipToParent === "subagent" || thread.delegatedFrom ? "worker" : "manager",
      laneId: context.laneId, connectorKind: "t3", hostId: context.host.id,
      externalRef: `${context.environmentId}/${threadId}`, label: thread.title.trim() || threadId,
      status, lastSeenAt: thread.updatedAt },
    runs,
    waits,
    ...(prs === undefined ? {} : { prs }),
  };
}

function contextFor(store: PlatformStore, host: PlatformT3Host, environmentId: string): ThreadContext {
  const laneId = host.laneSlug === undefined ? null : store.getLaneBySlug(host.laneSlug)?.id ?? null;
  return { host, environmentId, laneId };
}

export async function reconcileT3HostOnce(client: T3McpClient, store: PlatformStore,
  host: PlatformT3Host, opts: ReconcileOptions = {}): Promise<T3HostState> {
  const { signal, threadLimit = 500, listPageLimit = 40 } = opts;
  const environment = await client.readEnvironment({}, rpcSignal(signal));
  const context = contextFor(store, host, environment.environmentId);
  const listed = new Map<string, T3ThreadListItem>();
  let cursor: number | null = null;
  for (let page = 0; page < listPageLimit; page++) {
    const result = await client.listThreads({
      allProjects: true, includeSubagents: true, limit: pageSize, ...(cursor === null ? {} : { cursor }),
      ...(opts.activeOnly ? { statuses: ["preparing", "queued", "starting", "running", "waiting"] } : {}),
    }, rpcSignal(signal));
    for (const thread of result.threads) listed.set(thread.threadId, thread);
    cursor = result.nextCursor;
    if (cursor === null) break;
  }

  const knownAgents = new Map(store.listAgents().filter((agent) => agent.hostId === host.id).map((agent) => [agent.id, agent]));
  const previousWaits = new Set(store.listOpenT3Waits(host.id).map((wait) => wait.threadId));
  const previousActive = new Set([...knownAgents.values()].filter((agent) =>
    agent.status === "active" || agent.status === "waiting").map((agent) => agent.id));
  const watched = new Set(store.listT3ThreadPrs(host.id).filter((pr) => pr.watching).map((pr) => pr.threadId));
  const previousLatest = store.listT3LatestRunRefs(host.id);
  const idFor = (threadId: string) => agentId(host.id, environment.environmentId, threadId);
  const active = (thread: T3ThreadListItem) => isRunning(thread.status) || thread.status === "waiting";
  const changed = (thread: T3ThreadListItem) => thread.latestRunId != null
    && previousLatest.get(idFor(thread.threadId)) !== thread.latestRunId;
  const needsDetail = (thread: T3ThreadListItem) => {
    const id = idFor(thread.threadId);
    if (active(thread) || previousActive.has(id) || previousWaits.has(thread.threadId) || watched.has(thread.threadId)
      || changed(thread) || ("pendingRequestCount" in thread && typeof thread.pendingRequestCount === "number"
        && thread.pendingRequestCount > 0)) return true;
    const known = knownAgents.get(id);
    if (known === undefined || known.status === "offline") return !thread.settled || thread.latestRunId != null;
    return known.lastSeenAt !== thread.updatedAt;
  };
  const candidates = [...listed.values()].filter(needsDetail).sort((a, b) =>
    Number(active(b)) - Number(active(a)) || Number(changed(b)) - Number(changed(a))
    || b.updatedAt.localeCompare(a.updatedAt) || a.threadId.localeCompare(b.threadId));
  const selected = candidates.slice(0, threadLimit);
  const partialCoverage = cursor !== null || selected.length < candidates.length;
  const snapshots: T3ThreadSnapshot[] = [];
  // Small batches keep in-flight calls bounded and wait for outstanding requests on failure.
  for (let index = 0; index < selected.length; index += concurrency) {
    const batch = await Promise.allSettled(selected.slice(index, index + concurrency).map(async ({ threadId }) => ({
      threadId, snapshot: await readSnapshot(client, store, context, threadId, { details: true, signal }),
    })));
    const failure = batch.find((result) => result.status === "rejected" && result.reason instanceof T3AuthError)
      ?? batch.find((result) => result.status === "rejected");
    if (failure?.status === "rejected") throw failure.reason;
    for (const result of batch) {
      if (result.status !== "fulfilled") continue;
      const { threadId, snapshot } = result.value;
      if (snapshot === null) listed.delete(threadId);
      else snapshots.push(snapshot);
    }
  }
  signal?.throwIfAborted();
  return store.reconcileT3Host({
    ...hostUpdate(host, "connected"), environmentId: environment.environmentId, serverVersion: environment.serverVersion,
    ...(opts.tokenExpiresAt === undefined ? {} : { tokenExpiresAt: opts.tokenExpiresAt }),
    ...(!opts.activeOnly || partialCoverage ? { partialCoverage } : {}),
  }, snapshots, {
    observedAt: (opts.now?.() ?? new Date()).toISOString(),
    ...(!opts.activeOnly && cursor === null ? {
      listedAgentIds: [...listed.keys()].map(idFor),
      listedThreadIds: [...listed.keys()],
    } : {}),
    ...(opts.anchors === undefined ? {} : { anchors: opts.anchors }),
    ...(opts.resetCursors === undefined ? {} : { resetCursors: opts.resetCursors }),
  });
}

class CursorGap extends Error {
  readonly eventType: T3MilestoneName;
  readonly cursor: string | null;

  constructor(eventType: T3MilestoneName, cursor: string | null) {
    super("T3 event cursor gap");
    this.eventType = eventType;
    this.cursor = cursor;
  }
}
class StreamUnavailable extends Error {}

interface SupervisorOptions {
  tokensDir?: string;
  retryMs?: number;
}

export class T3HostSupervisor {
  private readonly abort = new AbortController();
  private readonly running: Promise<void[]>;

  constructor(store: PlatformStore, hosts: readonly PlatformT3Host[], options: SupervisorOptions = {}) {
    for (const registered of store.listT3HostStates()) {
      if (!hosts.some((host) => host.id === registered.hostId)) {
        store.upsertT3HostState({ ...registered, connectionState: "disconnected" });
      }
    }
    this.running = Promise.all(hosts.map((host) => this.runHost(store, host, options)));
  }

  async close(): Promise<void> {
    this.abort.abort();
    await this.running;
  }

  private async runHost(store: PlatformStore, host: PlatformT3Host, options: SupervisorOptions): Promise<void> {
    const signal = this.abort.signal;
    let consecutiveFailures = 0;
    let transport: "stream" | "poll" = "stream";
    const previous = store.listT3HostStates().find((state) => state.hostId === host.id);
    let reset = previous?.mcpUrl !== host.mcpUrl;
    let gap: CursorGap | undefined;
    store.upsertT3HostState({
      ...hostUpdate(host, "connecting"), lastError: null,
      ...(reset ? { environmentId: null, serverVersion: null, lastReconciledAt: null, lastEventAt: null } : {}),
    });
    // After auth fails, stop requesting the host until the service is restarted with fresh credentials.
    try {
      while (!signal.aborted) {
        let tokenExpiresAt: string | null = null;
        try {
          const token = await loadToken(host.id, options.tokensDir).catch(() => {
            throw new T3AuthError("Cannot read T3 authorization");
          });
          signal.throwIfAborted();
          if (token !== null) tokenExpiresAt = new Date(token.expiresAt).toISOString();
          if (token === null || getTokenExpiryStatus(token, Date.now()).expired) {
            throw new T3AuthError(token === null ? "T3 authorization is missing" : "T3 authorization expired");
          }
          store.upsertT3HostState({ ...hostUpdate(host, consecutiveFailures === 0 ? "connecting" : "stale"), tokenExpiresAt });
          const client = new T3McpClient(host, token.accessToken);
          const anchors = new Map<T3MilestoneName, string | null>();
          // Capture fresh cursors before taking a snapshot, so events during it can be replayed.
          for (const name of milestones) {
            if (gap?.eventType === name && gap.cursor !== null) {
              anchors.set(name, gap.cursor);
              continue;
            }
            if (reset || gap?.eventType === name || store.getT3Cursor(host.id, name) === null) {
              const head = await client.pollEvents(name, filter, { signal: rpcSignal(signal), maxEvents: 1 });
              anchors.set(name, head.cursor);
            }
          }
          const state = await reconcileT3HostOnce(client, store, host, {
            signal, ...(tokenExpiresAt === null ? {} : { tokenExpiresAt }), anchors, resetCursors: reset,
          });
          reset = false;
          gap = undefined;
          consecutiveFailures = 0;
          const context = contextFor(store, host, state.environmentId!);
          await this.consume(client, store, context, transport, signal);
        } catch (error) {
          if (signal.aborted) break;
          consecutiveFailures++;
          if (error instanceof T3AuthError) {
            store.upsertT3HostState({ ...hostUpdate(host, "auth_expired"), tokenExpiresAt,
              lastError: error.message, consecutiveFailures });
            return;
          }
          let lastError = "T3 connection or reconciliation failed";
          if (error instanceof CursorGap) {
            gap = error;
            lastError = "T3 event cursor gap";
          } else if (error instanceof StreamUnavailable) {
            transport = "poll";
            lastError = "T3 event stream disconnected";
          }
          store.upsertT3HostState({ ...hostUpdate(host, "stale"), lastError, consecutiveFailures });
          // Bound reconnection traffic even if a busy host keeps overflowing its buffer.
          const retryMs = options.retryMs ?? Math.min(30_000, 1_000 * 2 ** Math.min(consecutiveFailures - 1, 5));
          await delay(retryMs, undefined, { signal });
        }
      }
    } catch (error) {
      if (!signal.aborted) throw error;
    }
    store.upsertT3HostState(hostUpdate(host, "disconnected"));
  }

  private async consume(client: T3McpClient, store: PlatformStore, context: ThreadContext,
    transport: "stream" | "poll", hostSignal: AbortSignal): Promise<void> {
    const controller = new AbortController();
    const signal = AbortSignal.any([hostSignal, controller.signal]);
    const hostId = context.host.id;
    // Streams have separate cursors. Serialize thread reads and commits so two event types
    // cannot apply snapshots of the same thread out of order.
    let queue = Promise.resolve();
    function serialize(operation: () => Promise<void>): Promise<void> {
      const next = queue.then(async () => {
        signal.throwIfAborted();
        await operation();
      });
      queue = next;
      return next;
    }

    let failure: unknown;
    function stop(error: unknown): void {
      if (signal.aborted) return;
      failure = error;
      controller.abort();
    }

    const subscriptions = milestones.map(async (name) => {
      try {
        if (transport === "stream") {
          const idle = new AbortController();
          const streamSignal = AbortSignal.any([signal, idle.signal]);
          const stream = client.streamEvents(name, filter, {
            cursor: store.getT3Cursor(hostId, name), signal: streamSignal, heartbeatMs: 15_000,
          });
          try {
            while (!signal.aborted) {
              const timer = setTimeout(() => idle.abort(), 45_000);
              const frame = await stream.next().catch((error: unknown) => {
                if (error instanceof T3AuthError) throw error;
                throw new StreamUnavailable();
              }).finally(() => clearTimeout(timer));
              signal.throwIfAborted();
              if (frame.done) throw new StreamUnavailable();
              const item = frame.value;
              if (item.kind === "gap" || (item.kind === "activated" && item.truncated)) {
                throw new CursorGap(name, item.cursor);
              }
              if (item.kind === "event") {
                await serialize(() => applyEvent(client, store, context, name, item.cursor, item.data, signal));
              }
              else store.setT3Cursor(hostId, name, item.cursor);
            }
          } finally {
            idle.abort();
            await stream.return(undefined).catch(() => undefined);
          }
        } else {
          while (!signal.aborted) {
            const result = await client.pollEvents(name, filter, {
              cursor: store.getT3Cursor(hostId, name), signal: rpcSignal(signal),
            });
            signal.throwIfAborted();
            if (result.truncated) throw new CursorGap(name, null);
            for (const occurrence of result.events) {
              await serialize(() => applyEvent(client, store, context, name, occurrence.cursor ?? null, occurrence.data, signal));
            }
            store.setT3Cursor(hostId, name, result.cursor);
            if (!result.hasMore) await delay(Math.max(50, result.nextPollMs ?? 1_000), undefined, { signal });
          }
        }
      } catch (error) {
        stop(error);
      }
    });
    // The subscribed milestones have no run-start event. Refresh active threads for otherwise silent runs.
    const refresh = (async () => {
      try {
        while (!signal.aborted) {
          await delay(activeRefreshMs, undefined, { signal, ref: false });
          await serialize(async () => {
            await reconcileT3HostOnce(client, store, context.host, { signal, activeOnly: true });
          });
        }
      } catch (error) {
        stop(error);
      }
    })();
    await Promise.all([...subscriptions, refresh]);
    if (!hostSignal.aborted) throw failure ?? new StreamUnavailable();
  }
}

async function applyEvent(client: T3McpClient, store: PlatformStore, context: ThreadContext,
  name: T3MilestoneName, cursor: string | null, event: T3MilestoneEvent, signal: AbortSignal): Promise<void> {
  const { threadId, occurredAt } = event;
  const hostId = context.host.id;
  const snapshot = threadId === null ? undefined : await readSnapshot(client, store, context, threadId, { details: false, event, signal });
  signal.throwIfAborted();
  store.applyT3Event(hostId, name, cursor, occurredAt, snapshot ?? undefined, () => {
    if (threadId === null) return;
    if (snapshot === null) {
      store.markT3ThreadRemoved(hostId, threadId, agentId(hostId, context.environmentId, threadId));
      return;
    }
    switch (event.type) {
      case "thread.input_resolved":
        store.resolveT3Wait({ hostId, threadId, requestId: event.requestId, kind: event.kind, occurredAt }, event.resolution);
        break;
      case "pr.watch_wake":
        for (const pr of event.pullRequests) store.recordT3PrWake({ hostId, threadId, prHost: pr.host,
          repository: pr.repository, number: pr.number, outcome: event.outcome, summary: event.summary,
          occurredAt, watchEnded: event.watchEnded });
        break;
    }
  });
}
