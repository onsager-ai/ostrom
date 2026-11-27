# T3 Code v0.1.13 External Integration Reference

Technical reference for building external integrations (specifically Ostrom) against T3 Code without reading the upstream source.

- **Pinned upstream tags (`pingdotgg/t3code`):**
  - Target: **`v0.1.13`** (`bc0f04da94f64b5e487c598761a43b4cf06b2ac9`, released 2026-11-26T19:32Z)
  - Baseline comparison: **`v0.1.10`** (`7d32075e3757ded707914e2450c3e392a9665765`)
- **Code pointer convention:** All `path:line` pointers refer to `pingdotgg/t3code` at `bc0f04da94f64b5e487c598761a43b4cf06b2ac9` unless prefixed with `v0.1.10`. Paths under `packages/contracts/src/` or `apps/server/src/` are written in full on first mention and abbreviated within tables where noted.
- **Host state (`hp-code-2`, read-only check 2026-11-27T01:52Z):** The running T3 server reports `serverVersion: "0.1.13"`, while the nvm-installed `t3` binary on `PATH` is `v0.1.10`.
- **Scope & constraints:** Read-only research; no T3 settings, tokens, or hosts were modified. Design A (Managers stay T3 threads) holds until the Ostrom pilot ends, and Ostrom Rule 2 prohibits Ostrom from creating T3 threads. Items not directly verified in pinned source are marked `[Guess]`.

---

## 1. MCP (`/mcp`)

### 1.1 Transport, Protocol, and Schema Decoding

- **Endpoint:** `/mcp` (`apps/server/src/httpCors.ts:1–3`, `apps/server/src/mcp/McpHttpServer.ts:1536–1544`). Streamable HTTP MCP; `POST /mcp` is verified by integration tests (`McpHttpServer.test.ts:1034`), while `GET`/`DELETE` transport dispatch lives inside the patched `effect` dependency `[Guess]`.
- **CORS / Origin policy:** Any request carrying an `Origin` header is rejected with HTTP `403`; no browser CORS middleware is applied (`apps/server/src/http.ts:331–342`, `McpHttpServer.ts:350–408`).
- **Protocol versions:** Advertises `2025-06-18` (fallback for unknown versions) and `2026-07-28` (`McpHttpServer.ts:207`).
- **Compacted `tools/list` vs full `tools/call` decoding:** `tools/list` strips optional `null` branches and JSON-string fallbacks and can flatten tagged unions; `tools/call` checks top-level key names against the advertised schema and then decodes via the full Effect Schema JSON codec (`apps/server/src/mcp/advertisedJsonSchema.ts:35–71,74–111`, `McpHttpServer.ts:615–649,1350–1376`). Optional fields accept `null` as absent; preserve explicit `null` only where the contract field itself is nullable (e.g. `beforeRunId: null` on `t3_queue` reorder).

### 1.2 Outside-Client Access Levels & Enforcement Helpers

Outside MCP clients (`subject: "mcp-client"`) are granted an access level rather than raw scopes, ranked from lowest to highest (`packages/contracts/src/auth.ts:270–277`, `providerPolicy.ts:25–47`, `apps/server/src/mcp/McpInvocationContext.ts:96–136`, `McpHttpServer.ts:377–381`):

`read-only` < `approval-required` < `auto-accept-edits` < `auto` < `full-access`

Interaction mode ranks `plan` < `default`. Effective caller ceilings are narrowed by any `T3-Mode-Limit` header and managed policy `maxRuntimeMode`. Mapping `read-only` to `approval-required` inside ceiling math never grants write access (`McpToolAccess.ts:122–130`).

| `McpToolAccess` helper | Min outside-client access | Additional checks & source |
|---|---|---|
| `reads` | `read-only` | No write check at wrapper; handler may check capability (`McpToolAccess.ts:150–154`). |
| `writes` | `approval-required` | Rejects `read-only`, requires orchestration capability, refuses linked callers by default (`McpToolAccess.ts:122–144,174–192`, `threadAccess.ts:137–146`). |
| `writesThreads` | `approval-required` (or target thread's mode) | Target threads must fit caller's runtime & interaction ceilings and pass `linkOrigin`; targeting an `auto-accept-edits`/`auto`/`full-access` thread raises the required level (`McpToolAccess.ts:223–255`, `threadAccess.ts:176–191`). |
| `writesWithCapability` | `approval-required` | Refuses linked callers; handler checks domain capability (`McpToolAccess.ts:122–139,212–221`). |
| `startsThreads` | `approval-required` | Requested runtime/interaction modes must fit caller ceilings; stamps `linkOrigin` or refuses linked callers per declaration (`McpToolAccess.ts:278–306`). |
| `writesEnvironment` | `full-access` + `default` interaction | Requires orchestration capability; refuses linked callers except `t3_project_create` / `t3_project_clone` (`McpToolAccess.ts:345–381`, `threadAccess.ts:219–222`, `toolkits/project/handlers.ts:563–618,646–664`). |
| `writesOwnerWork` | `full-access` + `default` (or managed ceiling, > `read-only`) | Refuses linked callers; used by `t3_workflow` mutating actions (`McpToolAccess.ts:194–210`, `threadAccess.ts:224–239`). |
| `either` | Branch-dependent | Picks one of two access declarations from the call arguments (`McpToolAccess.ts:308–329`). |

> **Critical outside-client trap (9 forge PR/CI tools):** `pull_request_read`, `pull_request_list`, `pull_request_create`, `pull_request_update`, `pull_request_respond`, `pull_request_review`, `pull_request_merge`, `ci_logs`, and `ci_rerun` are **listed** in `tools/list` for outside clients (their discovery guard accepts `forge === undefined`, which HTTP sets for outside clients), **but all nine fail at call time with `PullRequestThreadRequiredError`** because their schemas accept no `threadId` and their handlers call `requireThread(..., undefined)` directly or via `locate()` (`McpHttpServer.ts:377–394`, `McpInvocationContext.ts:253–258`, `apps/server/src/mcp/toolkits/pullRequests/handlers.ts:359–379,598–599,1090–1093,1131–1134`). Outside clients **can** call the thread-link, watch, and review-guide PR tools (`link_pull_request*`, `unlink_pull_request*`, `list_thread_pull_requests`, `watch_pull_request`, `unwatch_pull_request`, `write_pull_request_guides`, `read_pull_request_guide`).

### 1.3 Tools Visible to Outside Clients (64 tools across 11 toolkits)

**Shorthand notation:** `s` = string, `b` = boolean, `i` = integer, `n` = number, `nn` = nonnegative int, `pos` = positive int, `J` = any JSON value, `Obj` = `Record<string, unknown>`, `field?` = optional, `| null` = nullable. `E` = `{environmentId?: s}` (remote forwarding selector), `T` = `{threadId?: s}` (always pass `threadId` explicitly from outside clients since there is no calling thread). Declaration lines refer to `apps/server/src/mcp/toolkits/<toolkit>/tools.ts`.

#### `orchestrator` (`apps/server/src/mcp/toolkits/orchestrator/tools.ts`, `handlers.ts:16–243`)

| Tool (line) | Access helper (min) | Input | Return shape |
|---|---|---|---|
| `orchestrator_capabilities` (75) | `reads` (`read-only`) | `{...E, search?: s}` (`search` <= 200) | `Capabilities` |
| `t3_workflow_stop` (131) | `writesThreads` (`approval-required`+) | `{...E, threadId: s, subagentId: s}` | `{}` |
| `schedule_task` (170) | `either`: create=`startsThreads`, update=`writes` (`approval-required`+) | `{...E, projectId?: s, scheduledTaskId?: s, prompt?: s, schedule?: ScheduleWrite\|s, title?: s, enabled?: b, bindToCurrentThread?: b, freshSession?: b, clientRequestId?: s}` (create requires `prompt` & `schedule`; string `schedule` is JSON-encoded) | `Scheduled` |
| `list_scheduled_tasks` (183) | `reads` (`read-only`) | `{...E, projectId?: s, scheduledTaskId?: s}` | `{tasks: Scheduled[]}` |
| `delete_scheduled_task` (197) | `writes` (`approval-required`) | `{...E, scheduledTaskId: s}` | `{scheduledTaskId: s, deleted: b}` |
| `t3_thread_list` (238) | `reads` (`read-only`) | `{...E, projectId?: s, allProjects?: b, statuses?: ThreadStatus[], titleContains?: s, settled?: b, snoozed?: b, includeSubagents?: b, cursor?: nn, limit?: pos}` (`statuses` <= 10, `titleContains` <= 256, `limit` <= 100) | `{projectId: s\|null, currentThreadId: s\|null, threads: ThreadListItem[], nextCursor: nn\|null, total: nn}` |
| `t3_thread_read` (252) | `reads` (`read-only`) | `{...E, threadId: s, itemId?: s, textOffset?: nn, view?: "messages"\|"activity", afterPosition?: i, beforePosition?: nn, limit?: pos, runLimit?: pos, maxCharsPerItem?: pos}` (`afterPosition` >= -1; caps 100 / 50 / 20,000) | `{thread: ThreadDetail, recentRuns: Run[], items: TimelineItem[], nextPosition: nn\|null, hasMore: b, earlierPosition?: nn\|null}` |
| `t3_thread_update` (267) | `writesThreads` (`approval-required`+) | `{...E, ...T, action: "rename"\|"regenerate_title", title?: s, clientRequestId?: s}` (`title` required for `rename`, forbidden for `regenerate_title`) | `{threadId: s, action: "rename"\|"regenerate_title", commandId: s, sequence: nn, title: s, titleRegeneration: TitleRegeneration\|null, linkedPullRequest: LinkedPR\|null, updatedAt: s}` |
| `t3_thread_send` (284) | `writesThreads` (`approval-required`+) | `{...E, threadId: s, message: s, mode?: "auto"\|"queue"\|"steer"\|"restart", freshContext?: b, outputSchema?: Obj, clientRequestId?: s}` (`message` <= 120,000) | `{threadId: s, link?: s, messageId: s, runId: s, taskId?: s, status: RunStatus, delivery: "started"\|"queued"\|"steered"\|"restarted"}` |
| `t3_thread_wait` (297) | `reads` (`read-only`) | `{...E, threadId: s, runId?: s, timeoutMs?: n, untilDelegatedWorkSettled?: b, untilOutput?: b}` | `{threadId: s, runId: s\|null, status: ThreadStatus, timedOut: b, next?: s, resultRun?: {runId: s, status: RunStatus}, output?: J}` |
| `t3_thread_interrupt` (311) | `writesThreads` (`approval-required`+) | `{...E, threadId: s, runId?: s, reason?: s, stop?: b, clientRequestId?: s}` (`reason` <= 2,000) | `{threadId: s, runId: s\|null, status: "interrupt_requested"\|"no_active_run"\|"completed"\|"failed"\|"cancelled"\|"interrupted"\|"rolled_back"}` |
| `t3_environment_links` (323) | `reads` (`read-only`, hidden if Labs off) | `{}` | `{environmentId: s, links: {environmentId: s, label: s, status: "reachable"\|"unreachable"\|"expired", access: Access, expiresAt: s, lastError: s\|null}[], notListed: s}` |
| `t3_environment_link` (337) | `writesEnvironment` (`full-access`, hidden if Labs off) | `{environmentId?: s, url?: s, hint?: s, reason?: s, requestedAccess?: Access, clientRequestId?: s}` | `{status: "pending", turnItemId: s} \| {status: "linked", environmentId: s, label: s, access: Access} \| {status: "declined"\|"cancelled"} \| {status: "failed", message: s}` |
| `t3_environment_unlink` (351) | `writes` (`approval-required`, hidden if Labs off) | `{environmentId: s}` | `{environmentId: s, removed: b, endedThere: b}` |

Shared `orchestrator` types (`packages/contracts/src/orchestratorMcp.ts:48–74,212–215,322–813,904–984`, `scheduledTask.ts:19–217`):

```ts
type RunStatus = "preparing"|"queued"|"starting"|"running"|"waiting"|"completed"|"interrupted"|"failed"|"cancelled"|"rolled_back";
type ThreadStatus = "idle" | RunStatus;
type Actor = "user"|"agent"|"system";
type CreationSource = "web"|"mobile"|"mcp"|"provider"|"server";
type OptionSelection = { id: string; value: string | boolean };
type LinkedPR = { projectId: string; repository: string; number: number; url: string };
type ThreadPR = { host: string; repository: string; number: number; url: string; source: "manual"|"created"|"agent"|"stack"|"stack-dismissed"; state: "open"|"closed"|"merged"|null; watching: boolean; watchStartedAt: string|null };

type Capabilities = {
  parentThreadId: string|null; inheritedProviderInstanceId: string|null; inheritedModel: string|null;
  inheritedOptions?: OptionSelection[]; runtimeMode: Runtime; interactionMode: "default"|"plan";
  providers: { providerInstanceId: string; driverKind: string; displayName: string|null;
    models: { id: string; label?: string; options?: { id: string; values: (string|boolean)[]; default?: string|boolean }[] }[];
    omittedModels?: number; runtimeModes?: Runtime[]; canRunChildTask: boolean;
    canRunCrossProviderChildTask: boolean; constraints: string[] }[];
  features: { appOwnedSubagents: boolean; asyncPolling: boolean; cancellation: boolean;
    batchThreadCreation: boolean; threadManagement: boolean; incrementalThreadRead: boolean;
    scheduledTasks: boolean; maxBatchThreads: number };
};

type ThreadListItem = {
  threadId: string; projectId: string; title: string; link?: string;
  createdBy: Actor; creationSource: CreationSource; status: ThreadStatus; latestRunId: string|null;
  providerInstanceId: string; model: string; runtimeMode: Runtime; interactionMode: "default"|"plan";
  linkedPullRequest: LinkedPR|null; pullRequests?: ThreadPR[]; pullRequestCount?: number; openPullRequestCount?: number;
  settled: boolean; settledAt: string|null; snoozed: boolean; snoozedUntil: string|null; pinned?: boolean;
  blockedOnThreadWait?: boolean;
  pendingBackgroundTasks?: { taskId: string; kind: "subagent"|"command"|"monitor"|"background_task"; description?: string; awaited?: true; childThreadId?: string }[];
  parentThreadId: string|null; relationshipToParent: "fork"|"subagent"|null;
  delegatedFrom?: { environmentId: string; threadId: string; title: string };
  itemCount: number; createdAt: string; updatedAt: string;
};

type ThreadDetail = ThreadListItem & {
  activeRunId: string|null; titleRegeneration: { requestId: string; startedAt: string }|null;
  lastTitleRegeneration?: { status: "renamed"|"unchanged"|"failed"; message?: string; requestId: string; completedAt: string }|null;
  branch: string|null; worktreePath: string|null; runCount: number; pendingRequestCount: number; archived: boolean;
};

type Run = {
  runId: string; ordinal: number; status: RunStatus; providerInstanceId: string; model: string;
  executedModel?: string|null; requestedAt: string; startedAt: string|null; completedAt: string|null;
};

type TimelineItem = {
  position: number; visibility: "local"|"inherited"|"synthetic"; sourceThreadId: string; itemId: string;
  subagentId?: string; runId: string|null; messageId: string|null; createdBy: Actor|null;
  creationSource: CreationSource|null; senderThreadId?: string; type: string; title: string|null;
  text: string|null; status: "idle"|"pending"|"running"|"waiting"|"completed"|"failed"|"cancelled"|"interrupted";
  textTruncated: boolean; nextTextOffset?: number|null; updatedAt: string;
};

type Scheduled = {
  scheduledTaskId: string; title: string; prompt: string; enabled: boolean; projectId: string;
  boundThreadId: string|null; freshSession: boolean; schedule: ScheduleRead; nextRunAt: string|null;
  lastRunStatus: "never"|"running"|"succeeded"|"failed"; webhookUrl?: string;
  webhookSignature?: "none"|"set"; blockedByPolicy?: string;
};
```

#### `thread` (`apps/server/src/mcp/toolkits/thread/tools.ts`, `handlers.ts:113–433`)

| Tool (line) | Access helper (min) | Input | Return shape |
|---|---|---|---|
| `t3_thread_organize` (34) | `writesThreads` (`approval-required`+) | `{...E, ...T, action: "pin"\|"unpin"\|"snooze"\|"unsnooze"\|"settle"\|"unsettle"\|"archive"\|"unarchive"\|"mark_unread", snoozedUntil?: s}` (`snooze` requires ISO date-time) | `{sequence: nn} \| {settlesWhenTurnEnds: true}` |
| `t3_queue` (85) | `either`: `list`/`read`=`reads`, others=`writesThreads` | `{...E, ...T, action: "list"\|"read"\|"edit"\|"cancel"\|"reorder"\|"promote_to_steer", queuedRunId?: s, text?: s, beforeRunId?: s\|null, targetRunId?: s, cursor?: nn, limit?: pos}` (`edit` text <= 100,000; `limit` <= 100) | `{items: QueueEntry[], nextCursor: nn\|null} \| QueueEntry \| {sequence: nn}` (`QueueEntry = {queuedRunId: s, text: s, truncated: b}`) |
| `t3_pending_request` (130) | `either`: `list`/`read`=`reads`, `respond`=`writesThreads` | `{...E, ...T, action: "list"\|"read"\|"respond", requestId?: s, answers?: Record<s, J>}` (user questions only; cannot approve permission prompts) | `{requestIds: s[]} \| {requestId: s, questions: Question[]} \| {sequence: nn}` |
| `t3_thread_configure` (156) | `either`: omit `modelSelection`=`reads`, set=`writesThreads` | `{...E, ...T, modelSelection?: ModelSelection}` | `{threadId: s, modelSelection: ModelSelection, runtimeMode: Runtime, interactionMode: Interaction} \| {sequence: nn}` |
| `t3_thread_transfer` (169) | `either`: `status`=`reads`, `fork`/`merge_back`=`writesThreads` | `{...E, ...T, action: "fork"\|"merge_back"\|"status", sourcePoint?: SourcePoint, targetThreadId?: s, title?: s}` | `{sequence: nn, targetThreadId: s} \| {transfers: {id: s, sourceThreadId: s, targetThreadId: s, status: "pending"\|"resolved_native"\|"resolved_portable"\|"failed"\|"consumed"\|"superseded"}[]}` |
| `t3_thread_search` (196) | `reads` (`read-only`) | `{...E, query: s, limit?: i, projectId?: s}` (`query` 2–200 chars, `limit` 1–50) | `{currentThreadId?: s\|null, matches: {threadId: s, projectId: s, title: s, link?: s, source: "title"\|"user"\|"assistant", snippet?: s, messageCreatedAt?: s\|null}[]}` |
| `run_scheduled_task_now` (224) | `writesEnvironment` (`full-access`) | `{...E, taskId: s}` | `{taskId: s, threadId: s\|null, link?: s, lastRunStatus: TaskRunStatus, lastRunError: s\|null, runCount: nn, nextRunAt: s\|null}` |

`Question = {id: s, header: s, question: s, options: {label: s, description: s, value?: s, preview?: s}[], multiSelect?: b, allowCustomAnswer?: b, initialAnswer?: s, required?: b}`. `SourcePoint = {type: "latest_stable"} | {type: "run", runId: s} | {type: "checkpoint", checkpointId: s}` (`orchestrationV2.ts:230–236,277–300`).

#### `project` (`apps/server/src/mcp/toolkits/project/tools.ts`, `handlers.ts:224–664`)

| Tool (line) | Access helper (min) | Input | Return shape |
|---|---|---|---|
| `t3_project_list` (55) | `reads` (`read-only`) | `{...E, cursor?: nn, limit?: pos}` (`limit` <= 100) | `{projects: Project[], nextCursor: nn\|null}` |
| `t3_project_read` (70) | `reads` (`read-only`) | `{...E, projectId: s}` | `Project` |
| `t3_project_create` (80) | `writesEnvironment` (`full-access`, opts into linked) | `{...E, title: s, workspaceRoot?: s, createWorkspaceRootIfMissing?: b, defaultModelSelection?: ModelSelection\|null, scripts?: ProjectScript[], trackedFolders?: {path: s, name: s}[]}` | `Project & {commitError?: s}` |
| `t3_project_update` (92) | `writesEnvironment` (`full-access`) | `{...E, projectId: s, title?: s, workspaceRoot?: s, defaultModelSelection?: ModelSelection\|null, autoPull?: b, projectIcon?: ProjectIcon\|null, faviconPath?: s\|null, defaultThreadEnvMode?: "local"\|"worktree"\|null, scripts?: ProjectScript[], trackedFolders?: {path: s, name: s}[], pinned?: b, archived?: b}` | `Project` |
| `t3_project_delete` (101) | `writesEnvironment` (`full-access`) | `{...E, projectId: s, force?: b}` | `Project` |
| `t3_project_clone` (115) | `writesEnvironment` (`full-access`, opts into linked) | `{...E, provider?: s, repository?: s, remoteUrl?: s, destinationPath: s, protocol?: "auto"\|"ssh"\|"https"}` | `{cwd: s, remoteUrl: s, repository: {provider: s, nameWithOwner: s, url: s, sshUrl: s}\|null}` |
| `t3_thread_launch` (131) | `startsThreads` (`approval-required`+) | `{...E, projectId?: s, scratch?: b, title: s, generateTitle?: b, modelSelection?: ModelSelection, runtimeMode?: Runtime, interactionMode?: Interaction, workspaceStrategy?: Workspace, message?: s, attachments?: Attachment[], outputSchema?: Obj, clientRequestId?: s, delegatedFrom?: {environmentId: s, threadId: s, title: s}, workflowRun?: {environmentId: s, runId: s, workflow: s, step: s}}` (outside clients must set `projectId` or `scratch`; `message` <= 120,000; `attachments` <= 8) | `{threadId: s, link?: s, projectId: s, modelSelection: ModelSelection, runId: s\|null, status: RunStatus\|null}` |

`ModelSelection` encodes canonically as `{instanceId: s, model: s, options?: OptionSelection[]}` and accepts legacy `provider` as `instanceId` and record-shaped `options` on decode (`packages/contracts/src/modelSelection.ts:9–50`). `Workspace = {type: "root", branch?: s} | {type: "existing_worktree", worktreePath: s, branch?: s} | {type: "worktree", baseRef: s, branch?: s, startFromOrigin?: b, repositories?: {path: s}[]}` (`repositories` and `trackedFolders` cap at 128; `orchestrationV2.ts:788–813`, `project.ts:26–239`).

#### `pullRequests` (`apps/server/src/mcp/toolkits/pullRequests/tools.ts`, `handlers.ts:359–1425`)

`PRTarget = {...E, ...T, url?: s, repository?: s, number?: pos, host?: s}`. `GuideTarget = {url?: s, repository?: s, number?: pos, host?: s}`. `BranchPR = {...E, number?: pos, repository?: s}`. `PRID = {host: s, repository: s, number: i, url: s}`. `PRState = "open" | "closed" | "merged"`.

| Tool (line) | Access helper (min) | Outside-client status | Input | Return shape |
|---|---|---|---|---|
| `link_pull_request` (313) | `writesThreads` (`approval-required`+) | **Works** (pass `threadId`) | `PRTarget` | `PRID & {alreadyLinked: b, next?: s}` |
| `link_pull_requests` (327) | `writesThreads` (`approval-required`+) | **Works** (pass `threadId`) | `{...E, ...T, urls: s[]}` (>= 1) | `{linked: PRNumbers[], alreadyLinked: PRNumbers[], linkedCount: i, invalidUrls?: s[], next?: s}` |
| `unlink_pull_request` (345) | `writesThreads` (`approval-required`+) | **Works** (pass `threadId`) | `PRTarget` | `{host: s, repository: s, number: i, wasLinked: b}` |
| `unlink_pull_requests` (359) | `writesThreads` (`approval-required`+) | **Works** (pass `threadId`) | `{...E, ...T, states: PRState[]}` (>= 1) | `{unlinked: (PRNumbers & {state: PRState})[], unlinkedCount: i, remaining: i}` |
| `list_thread_pull_requests` (377) | `reads` (`read-only`) | **Works** (pass `threadId`) | `{...E, ...T, states?: PRState[], cursor?: s}` | `{pullRequests: ThreadPREntry[], chains: {kind: "native"\|"derived", numbers: i[]}[], nextCursor: s\|null}` |
| `watch_pull_request` (406) | `writesThreads` (`approval-required`+) | **Works** (pass `threadId`) | `PRTarget & {until?: "checks"\|"merged"}` | `WatchResult` |
| `unwatch_pull_request` (421) | `writesThreads` (`approval-required`+) | **Works** (pass `threadId`) | `PRTarget` | `WatchResult` |
| `write_pull_request_guides` (466) | `writesWithCapability` (`approval-required`) | **Works** | `{...E, pullRequests: GuideTarget[]}` (1–20) | `{guides: (PRID & {outcome: "started"\|"running"\|"failed", detail: s\|null})[]}` |
| `read_pull_request_guide` (483) | `reads` (`read-only`) | **Works** | `{...E, ...GuideTarget}` | `PRID & {guide: Guide\|null, run: GuideRun\|null, currentHeadSha: s\|null, stale: b}` |
| `pull_request_read` (637) | `reads` | **Fails (`PullRequestThreadRequiredError`)** | `BranchPR & {include?: ("body"\|"checks"\|"threads"\|"comments"\|"diff")[], includeResolved?: b, cursor?: s}` | `ReadPR` |
| `pull_request_list` (660) | `reads` | **Fails (`PullRequestThreadRequiredError`)** | `{...E, state?: "all"\|PRState, involvement?: "all"\|"reviewing"\|"authored", query?: s, limit?: i, cursor?: s}` | `{pullRequests: PRListEntry[], truncated: b, nextCursor: s\|null}` |
| `pull_request_create` (681) | `writesWithCapability` | **Fails (`PullRequestThreadRequiredError`)** | `{...E, title: s, body: s, base?: s, draft?: b, private?: b}` | `PRID & {created: b, note?: s, next?: s}` |
| `pull_request_update` (710) | `writesWithCapability` | **Fails (`PullRequestThreadRequiredError`)** | `BranchPR & {title?: s, body?: s, ready?: b, state?: "open"\|"closed", addLabels?: s[], removeLabels?: s[], requestReviewers?: s[]}` (arrays 1–25) | `PRID` |
| `pull_request_respond` (736) | `writesWithCapability` | **Fails (`PullRequestThreadRequiredError`)** | `BranchPR & {reviewThreadId?: s, body?: s, resolved?: b}` | `PRID & {replied: b, resolved: b\|null}` |
| `pull_request_review` (769) | `writesWithCapability` | **Fails (`PullRequestThreadRequiredError`)** | `BranchPR & {verdict: "comment"\|"approve"\|"request-changes", expectedHeadSha?: s, body?: s, comments?: {path: s, line: pos, startLine?: pos, side?: "new"\|"old", body: s}[]}` (<= 50; approve requires `expectedHeadSha`) | `PRID` |
| `pull_request_merge` (790) | `writesWithCapability` | **Fails (`PullRequestThreadRequiredError`)** | `BranchPR & {expectedHeadSha: s, method?: "merge"\|"squash"\|"rebase"\|"fast-forward", auto?: b, stack?: b}` | `PRID & {state: PRState}` |
| `ci_logs` (845) | `reads` | **Fails (`PullRequestThreadRequiredError`)** | `BranchPR & {check?: s, tailLines?: i}` (1–2,000) | `PRID & {checks: {name: s, url: s\|null, log: s\|null, truncated: b, detail: s\|null}[]}` |
| `ci_rerun` (867) | `writesWithCapability` | **Fails (`PullRequestThreadRequiredError`)** | `BranchPR & {check?: s}` | `PRID & {checks: {name: s, rerun: b, detail: s\|null}[]}` |

Key PR return shapes (`pullRequests/tools.ts:202–310,442–463,516–635,819–842`):
- `WatchResult = PRID & {watching: b, wasWatching: b, state?: "merged"|"closed", checksState?: "passing"|"failing"|"pending", next?: s}`
- `ThreadPREntry = PRID & {source: PRSource, watching: b, state: PRState|null, title: s|null, headBranch: s|null, baseBranch: s|null, isDraft: b|null, stack: {kind: "native"|"derived", position: i, size: i}|null}`
- `ReadPR = PRID & {title: s, state: PRState, isDraft: b, visibility?: "public"|"private", author: s|null, headBranch: s, baseBranch: s, headSha: s|null, mergeability: "mergeable"|"conflicting"|"unknown", behindBy: i|null, autoMergeEnabled: b|null, checks: {passed: i, pending: i, failed: s[], incomplete?: true}, unresolvedThreads: i, unresolvedThreadsTruncated?: true, body?: s, checkRuns?: PRCheck[], conversation?: {...}, diff?: {patch: s, truncated: b, nextCursor: s|null}}`

#### `issues` (`apps/server/src/mcp/toolkits/issues/tools.ts`, `handlers.ts:91–310`)

No `environmentId` forwarding. `IssueTarget = {url?: s, key?: s, projectId?: s}` (pass `url`, or `key` + `projectId` outside a thread). `IssueKey = {projectId: s, tracker: s, host: s, scope: s, key: s}`. `IssueID = IssueKey & {url: s}`. `IssueWrite = IssueKey & {url?: s}`.

| Tool (line) | Access helper (min) | Input | Return shape |
|---|---|---|---|
| `list_issues` (241) | `reads` (`read-only`) | `{projectId?: s, state?: "all"\|"open"\|"closed", query?: s, limit?: i, cursors?: Record<s, s>}` (`projectId` needed outside; `query` <= 200; `limit` 1–100) | `{issues: IssueEntry[], nextCursors: Record<s, s>, errors: {projectId: s, message: s}[]}` |
| `read_issue` (267) | `reads` (`read-only`) | `IssueTarget` | `ReadIssue` (`IssueID` + `displayKey`, `title`, `state`, `writable`, `capabilities`, `states`, `body`, `comments`, `linkedPullRequests`) |
| `create_issue` (281) | `writesWithCapability` (`approval-required`) | `{projectId?: s, scope?: s, title: s, body?: s, labels?: s[], assignees?: s[]}` (`title` <= 1,024; `body` <= 65,536; `labels` <= 50; `assignees` <= 25) | `IssueID & {displayKey: s}` |
| `comment_on_issue` (307) | `writesWithCapability` (`approval-required`) | `IssueTarget & {body: s}` (`body` 1–65,536) | `IssueWrite` |
| `set_issue_state` (323) | `writesWithCapability` (`approval-required`) | `IssueTarget & {state: s}` | `IssueWrite` |
| `link_issue` (341) | `writesThreads` (`approval-required`+) | `IssueTarget & T` | `IssueID & {alreadyLinked: b}` |
| `unlink_issue` (355) | `writesThreads` (`approval-required`+) | `IssueTarget & T` | `IssueWrite & {wasLinked: b}` |
| `list_thread_issues` (368) | `reads` (`read-only`) | `T` | `{issues: (IssueID & {source: "manual"\|"agent", displayKey: s, title: s})[]}` |

#### `workflow` (`apps/server/src/mcp/toolkits/workflow/tools.ts:18–79`, `handlers.ts:17–129`)

- **`t3_workflow`** — annotated `hiddenFromThreads` (outside-client-only; no `environmentId`).
- **Access:** `either`: `list`, `read`, `questions`, `store` use `reads` (`read-only`); `hooks`, `start`, `signal`, `resume`, `cancel`, `withdraw`, `store_set`, `store_delete` use `writesOwnerWork` (`full-access` / managed ceiling).
- **Input:** `{action: "list"|"read"|"questions"|"store"|"hooks"|"start"|"signal"|"resume"|"cancel"|"withdraw"|"store_set"|"store_delete", workflow?: s, input?: J, idempotencyKey?: s, runId?: s, status?: WorkflowStatus, tag?: s, subject?: s, kind?: s, wait?: s, key?: s, channel?: s, value?: J, signalId?: s, reason?: s}`. Required pairings: `read`/`resume`/`cancel` -> `runId`; `start` -> `workflow`, `idempotencyKey`; `signal` -> `value` + either `(runId, wait)` or `(workflow, channel)`; `withdraw` -> `runId`, `wait`; `store_set` -> `key`, `value`; `store_delete` -> `key`.
- **Return:** `{run?: WorkflowRun, steps?: WorkflowStep[], runs?: WorkflowRun[], questions?: WorkflowQuestion[], hooks?: {workflow: s, name: s, path: s}[], entries?: {key: s, value: J}[], delivery?: "delivered"|"buffered", withdrawal?: "withdrawn"|"not_open"}` (`packages/contracts/src/workflow.ts:6–105`).

#### `environment`, `theme`, `attachment`, `worktree`, `feedback`

| Tool (declaration) | Access helper (min) | Input | Return shape |
|---|---|---|---|
| `t3_environment_read` (`environment/tools.ts:41`) | `reads` (`read-only`) | `E` | `{environmentId: s, label: s, serverVersion: s, platform: Platform, preferences: Preferences}` |
| `t3_environment_preferences_update` (`environment/tools.ts:56`) | `writesEnvironment` (`full-access`) | `{...E, defaultThreadEnvMode?: "local"\|"worktree"\|null, newWorktreesStartFromOrigin?: b, enableProviderUpdateChecks?: b, backgroundActivity?: {profile: "balanced"\|"performance"\|"battery-saver"}, sourceControlWritingStyle?: {mode?: "repo_conventions"\|"conventional_commits"\|"custom", customInstructions?: s, followChangeRequestTemplates?: b}}` | `Preferences` |
| `t3_theme_read` (`theme/tools.ts:27`) | `reads` (`read-only`) | `{themeId?: s}` | `{current: s, builtIn: s[], published: {id: s, name: s, hasCss: b, hasBackground: b}[], theme?: ThemeFile, themesDir: s, guide: s}` |
| `t3_theme_set` (`theme/tools.ts:55`) | `writesEnvironment` (`full-access`) | `{themeId?: s, theme?: ThemeFile, appearance?: "light"\|"dark"\|"system"}` | `{themeId?: s, appearance?: "light"\|"dark"\|"system", published: b}` |
| `t3_attachment_prepare_upload` (`attachment/tools.ts:34`) | `writes` (`approval-required`) | `{upload: {type?: "image", name: s, mimeType: "image/gif"\|"image/jpeg"\|"image/png"\|"image/webp", sizeBytes: pos} \| {type: "file", name: s, mimeType: s, sizeBytes: pos}}` (image <= 10 MiB, file <= 50 MiB) | `{attachmentId: s, relativeUrl: s, expiresAt: n, uploadUrl?: s}` (`expiresAt` = epoch ms) |
| `t3_attachment_discard` (`attachment/tools.ts:44`) | `writes` (`approval-required`) | `{attachmentId: s}` | `{}` |
| `t3_thread_send_attachments` (`attachment/tools.ts:50`) | `writesThreads` (`approval-required`+) | `{...T, message?: s, attachments: Attachment[]}` (`message` <= 120,000; 1–8 attachments) | `{threadId: s, messageId: s, runId: s, status: RunStatus}` |
| `t3_worktree_list` (`worktree/tools.ts:63`) | `reads` (`read-only`) | `{...E, ...T, query?: s, cursor?: nn, limit?: pos, refKind?: "all"\|"local"\|"remote", includeMatchingRemoteRefs?: b}` (`query` <= 256, `limit` <= 200) | `{isAtRepositoryRoot?: b, refs: {name: s, isRemote?: b, remoteName?: s, upstreamRef?: s, current: b, isDefault: b, worktreePath: s\|null}[], isRepo: b, hasPrimaryRemote: b, nextCursor: nn\|null, totalCount: nn}` |
| `t3_feedback` (`feedback/tools.ts:25`) | `writes` (`approval-required`; hidden when feedback off) | `{kind: "bug"\|"confusing"\|"missing_capability"\|"wrong_docs"\|"other", subject: s, expected: s, actual: s, workaround: s, userRequested?: b, turns?: pos}` | `FeedbackReport \| {status: "awaiting_approval", note: s}` |

### 1.4 Tools Hidden From Outside Clients

Discovery filtering uses `McpSchema.EnabledWhen` (`McpHttpServer.ts:1566–1581`, `McpInvocationContext.ts:205–214`), backed by handler-level `requireThreadCaller` enforcement (`McpToolAccess.ts:133–145`) and startup assertions in `McpToolAccess.toLayer` that reject any `actsAsCaller` tool lacking a thread-only predicate (`McpToolAccess.ts:420–482`):

| Toolkit | Hidden tools (27 total) | Predicate & source |
|---|---|---|
| `device` (4) | `device_list`, `device_open`, `device_screenshot`, `device_close` | `hiddenFromOutsideClients` (`device/tools.ts:30–101`) |
| `html` (2) | `html_preview`, `html_render` | `hiddenFromOutsideClients` (`html/tools.ts:30–114`) |
| `message` (1) | `send_to_user` | `hiddenFromOutsideClients` (`message/tools.ts:11–36`) |
| `orchestrator` (6) | `delegate_task`, `task_status`, `task_cancel`, `request_secret`, `create_threads`; `t3_background_wait` | `hiddenFromOutsideClients` (`orchestrator/tools.ts:89–129,209–236`); `hiddenWithoutBackgroundWait` (`orchestrator/tools.ts:146–168`) |
| `preview` (12) | `preview_status`, `preview_open`, `preview_dialog`, `preview_navigate`, `preview_resize`, `preview_set_appearance`, `preview_snapshot`, `preview_act`, `preview_evaluate`, `preview_wait_for`, `preview_recording_start`, `preview_recording_stop` | `hiddenFromOutsideClients` (`preview/tools.ts:39–55,60–end`) |
| `previewControls` (2) | `t3_preview_list`, `t3_preview_close` | `hiddenFromOutsideClients` (`previewControls/tools.ts:26–50`) |
| `thread` (1) | `t3_submit_output` | `hiddenFromOutsideClients` (`thread/tools.ts:246–257`) |
| `toolSearch` (1) | `tool_search` | `hiddenFromOutsideClients` (`toolSearch/tools.ts:8–23`) |
| `worktree` (2) | `t3_worktree_handoff`, `t3_worktree_status` | `hiddenFromOutsideClients` (`worktree/tools.ts:28–61`) |

*(Note: 26 use `hiddenFromOutsideClients` + 1 `t3_background_wait` uses `hiddenWithoutBackgroundWait` = 27 tools hidden from outside clients; conversely, `t3_workflow` uses `hiddenFromThreads` so only outside clients see it.)*

### 1.5 Differences From `v0.1.10`

- **New toolkits in `v0.1.13`:** `feedback`, `issues` (8 tools), `workflow` (`t3_workflow`). All three return 404 at `v0.1.10`.
- **New PR/CI tools in `v0.1.13`:** All 9 forge tools (`pull_request_{read,list,create,update,respond,review,merge}`, `ci_{logs,rerun}`). `v0.1.10` had link/watch/guide tools only.
- **New thread tool:** `t3_submit_output` (`thread/tools.ts:246–257`).
- **Schema additions:** `t3_thread_launch` adds `generateTitle`, `outputSchema`, `workflowRun`; `watch_pull_request` adds `until: "checks"|"merged"` plus `state`/`checksState`/`next` in results; `t3_environment_unlink` now ends the remote session when the peer answers; `t3_background_wait` switches from `hiddenFromOutsideClients` to `hiddenWithoutBackgroundWait`.
- **MCP elicitation (new in `v0.1.13`):** `apps/server/src/mcp/McpClientInput.ts:12–32,94–169` adds form elicitation + sessionless `InputRequired` callbacks (used by `pull_request_create` push-hook approvals at `pullRequests/handlers.ts:1169–1192`).

---

## 2. MCP Events (`io.modelcontextprotocol/events`, SEP-3415)

**Difference from `v0.1.10`:** Absent in `v0.1.10` (`apps/server/src/milestones/` and `McpWebhookTransport.ts` do not exist at `7d32075e`). Added post-`v0.1.10` and live in `v0.1.13`.

Events are registered protocol-level on `/mcp` for outside clients only (`McpInvocationContext.hiddenFromThreads`; workflow events also require `Settings -> Labs -> Workflows` enabled; `McpHttpServer.ts:1645–1672`).

### 2.1 Envelope, All 12 Milestone Types, and Filter Parameters

Shared envelope (`packages/contracts/src/milestones.ts:21–43`):

```ts
type MilestoneActor = { type: "user" | "agent" | "system" | "guest"; id?: string; label?: string };
type MilestoneEnvelope = {
  id: string;                 // Stable per state transition (used for dedup & eventId hash)
  occurredAt: string;         // IsoDateTime
  projectId: string | null;
  threadId: string | null;
  parentThreadId: string | null;
  actor: MilestoneActor;
  isSubagentChild: boolean;   // Excluded by default unless includeSubagentChildren: true
  workflowRunId?: string;
};
```

**Thread/PR milestones (6)** — filter parameters (`MilestoneEvents.ts:26–44`): `{ projectIds?: string[], threadIds?: string[], workflowRunIds?: string[], includeSubagentChildren?: boolean }` (AND across supplied fields; subagent children excluded unless `true`; `milestones.ts:46–101,163–187`):

| Event `type` | Additional payload fields |
|---|---|
| `thread.run_settled` | `runId: string`, `outcome: "completed" \| "failed" \| "interrupted" \| "cancelled"` |
| `thread.needs_input` | `requestId: string`, `kind: "user_input" \| "approval"` |
| `thread.input_resolved` | `requestId: string`, `kind: "user_input" \| "approval"`, `resolution: "resolved" \| "expired" \| "cancelled"` |
| `thread.guest_request` | `shareId: string`, `guestMessageId: string` |
| `thread.guest_request_resolved` | `shareId: string`, `guestMessageId: string`, `resolution: "sent" \| "dismissed" \| "closed"` |
| `pr.watch_wake` | `pullRequests: { host: string, repository: string, number: number }[]`, `outcome: "completed" \| "failed" \| "cancelled" \| "updated" \| "unknown"`, `summary: string`, `watchEnded: boolean` |

**Workflow milestones (6)** — also carry required `workflowRunId: string`, `workflow: string`, `runTitle: string`, `parentWorkflowRunId: string | null`; filter parameters (`MilestoneEvents.ts:46–50`): `{ workflows?: string[], workflowRunIds?: string[], includeSubagentChildren?: boolean }` (`milestones.ts:50–57,102–143`):

| Event `type` | Additional payload fields |
|---|---|
| `workflow.run_started` | *(none)* |
| `workflow.needs_input` | `step: string`, `question: string` |
| `workflow.input_resolved` | `step: string`, `resolution: "answered" \| "expired" \| "withdrawn" \| "cancelled"` |
| `workflow.run_paused` | `reason: string` |
| `workflow.run_resumed` | *(none)* |
| `workflow.run_ended` | `outcome: "completed" \| "stopped" \| "skipped" \| "cancelled"`, `detail: string \| null` |

### 2.2 Poll, Stream, and Webhook Delivery

- **Event ID:** In every delivery mode, `eventId` is `evt_` + first 22 chars of Base64URL(SHA-256(`event.id`)) (`apps/server/src/milestones/MilestoneEvents.ts:96–105`).
- **Ring buffer & cursors (`apps/server/src/milestones/MilestoneHub.ts:16–38,98–138`):**
  - Retains **1,000** milestones in memory and **4,000** seen milestone IDs for duplicate suppression.
  - Cursor format: `<base36(serverStartMs)>.<sequence>`.
  - `cursor: null` starts from "now". An invalid or pre-restart cursor returns from the oldest retained event with `truncated: true`. Events dropped by `maxAgeMs` also set `truncated: true`; hitting `maxEvents` sets `hasMore: true`.
- **Poll (`MilestoneEvents.ts:22–24,144–159`):** `maxEvents` defaults to `100`, capped at `1,000`. Supports `cursor` and `maxAgeMs`.
- **Stream (`MilestoneHub.ts:195–227`, `MilestoneEvents.ts:77–94,160–186`):** Subscribes to live events *before* reading the retained replay slice, then deduplicates overlaps. Each live subscriber has a **256-item** sliding backlog; a slow consumer receives a `Gap` marker. Streams interrupt with `Forbidden: Access revoked` when the client's `AuthSessionId` is invalidated.
- **Webhooks (`patches/effect@4.1.5.patch:2404–2465,2935–3589`, `McpHttpServer.ts:1614–1672`, `McpWebhookTransport.ts:9–127`):**
  - **Principal & quota:** Scoped to the outside client's `sessionId` (requires outside sign-in). Max **100 subscriptions per session principal** (including pending setups). Identity is `(principal, normalizedUrl, eventName, canonicalArgs)`; re-subscribing refreshes without consuming another slot.
  - **Leases (`events/subscribe`):** Accepts `{name, arguments?, cursor?, maxAgeMs?, delivery: {mode?: "webhook", url, secret}, ttlMs?}`. Omitted `ttlMs` = **900,000 ms (15 min)**; `null` = **3,600,000 ms (1 h)**; numeric clamped to **60,000–3,600,000 ms (1–60 min)**. Returns `{id, refreshBefore, cursor, truncated}` (plus `deliveryStatus: {active: true, lastDeliveryAt, lastError}` on refresh). Secret rotation on refresh retains the previous secret for a **5-minute overlap** (two space-separated `v1,` signatures). `events/unsubscribe` matches by `(principal, url, name, arguments)`.
  - **Verification & signing (Standard Webhooks):** Secret must match `whsec_<base64 of 24–64 bytes>`. Every `POST` sends `Content-Type: application/json`, `webhook-id`, `webhook-timestamp` (Unix seconds), `webhook-signature` (`v1,<base64 HMAC-SHA256>` over UTF-8 `<webhook-id>.<webhook-timestamp>.<raw body>`), and `x-mcp-subscription-id`. On subscribe, callback must echo `{challenge}` to a signed `{type: "verification", challenge}` POST. Event body is `{eventId, name, timestamp, data, cursor, _meta?}` with `eventId === webhook-id`.
  - **Retries:** **5 total attempts** (backoff `30s`, `1m`, `2m`, `4m`), **10 s** per-attempt timeout. Retries reuse `webhook-id` with a fresh timestamp and signature.
  - **SSRF guard & env var (`McpWebhookTransport.ts:9–127`):** Public HTTPS only by default; blocks IANA non-global IPv4 ranges (`0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`, `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.168.0.0/16`, multicast/reserved, etc.) and requires IPv6 in `2000::/3` outside exclusions. DNS resolution times out at **5 s**, custom Undici `lookup` re-verifies resolved IPs at connect time (defeating DNS rebinding), and redirects are never followed. Override for local testing: `T3CODE_MCP_WEBHOOK_ALLOWED_HOSTS=localhost,127.0.0.1` (comma-separated hostnames/IPs allowed to use private IPs and plain `http:`).

### 2.3 Restart Behavior

All milestone records, sequence numbers, dedup IDs, and webhook leases live **only in memory** (`milestones.ts:15–19`, `MilestoneHub.ts:31–33,98–120`). A T3 server restart wipes them and rotates the base-36 cursor epoch:
- Webhook leases disappear and must be re-registered via `events/subscribe`.
- Presenting a pre-restart cursor returns whatever new milestones exist since startup with `truncated: true`.
- **Required client pattern:** Whenever `truncated: true` or a `Gap` marker arrives (or on reconnect), reconcile authoritative state via `t3_thread_read` / `t3_thread_list` (or HTTP snapshots), then continue from the new cursor.

---

## 3. Auth Flows (Step by Step)

### 3.1 MCP OAuth (`/oauth/mcp/*`)

1. **Discovery (`McpOAuth.ts:54–106`, `mcpOAuthHttp.ts:103–111`):**
   - `GET /.well-known/oauth-protected-resource` and `GET /.well-known/oauth-protected-resource/mcp` return `{resource: "<origin>/mcp", authorization_servers: ["<origin>"], scopes_supported: ["orchestration:read", "orchestration:operate"], bearer_methods_supported: ["header"], resource_name: "T3 Code"}`. A `401` on `/mcp` can advertise `resource_metadata` pointing here (`McpHttpServer.ts:117–149,350–375`).
   - `GET /.well-known/oauth-authorization-server` advertises `/oauth/mcp/{authorize,token,register,revoke}`, `response_types_supported: ["code"]`, `grant_types_supported: ["authorization_code"]`, `code_challenge_methods_supported: ["S256"]`, `token_endpoint_auth_methods_supported: ["none"]`. *(v0.1.10 lacked `revocation_endpoint`.)*
2. **Stateless dynamic client registration (`POST /oauth/mcp/register`, `McpOAuth.ts:54–66,109–134,274–316,329–375`, `PayloadSigner.ts:8,37`):**
   - Public, unauthenticated; returns HTTP `201` with a stateless HMAC-SHA256-signed `client_id = base64url(JSON {v:1, n, r, p?}) + "." + sig`.
   - `client_name` defaults to `"MCP client"` (<= 100 chars); `redirect_uris` accepts 1–10 URIs (<= 512 chars each), `https:` or `http:` on `localhost`/`127.0.0.1`/`[::1]`, no userinfo and no `#` fragment (even empty). For > 5 URIs, `client_id` is capped at 10,240 chars. `software_id: "t3code-peer-link"` marks a peer-link client (requires Linked environments enabled).
3. **Authorization (`GET /oauth/mcp/authorize`, `McpOAuth.ts:136–145,378–425`, `mcpOAuthHttp.ts:83–101,135–165`):**
   - Query params: `client_id`, `redirect_uri`, `response_type=code`, `code_challenge_method=S256`, `code_challenge` (43-char base64url), optional `state` and `resource` (must match `<origin>/mcp`).
   - Redirects to `/connect-agent` locally (or hosted `/approve-agent?environment=...` when T3 Connect matches the issuer; peer links stay local).
4. **Approval & Decision (`POST /oauth/mcp/approval`, `POST /oauth/mcp/decision`, `McpOAuth.ts:318–327,428–529`, `mcpOAuthHttp.ts:168–204`):**
   - `/approval` validates the auth request and returns metadata + CSRF token (bound to session ID or DPoP key) if the current browser session holds `access:write` + `orchestration:read` + the grant's scopes.
   - `/decision` accepts `{authorization, decision}` where `decision` is `deny`, `{type: "browser-session", access, csrfToken}`, or `{type: "pairing-code", access, code}`. Returns `{redirectTo}` with `?code=...&state=...&iss=...` (or `error=access_denied`).
   - *v0.1.13 addition:* Purpose isolation between app-approval and peer-link codes, plus `otherSessionRevocationCount` race protection against `revoke-others` (`EnvironmentAuth.ts:1737–1759`, `AuthPairingLinks.ts:229–270`).
5. **Token exchange (`POST /oauth/mcp/token`, `application/x-www-form-urlencoded`, `McpOAuth.ts:55,531–594`, `EnvironmentAuth.ts:87–96,1638–1689,1711–1734`, `SessionStore.ts:491–510,709–756`):**
   - Params: `grant_type=authorization_code`, `code`, `client_id`, `redirect_uri` (exact string match), `code_verifier` (43–128 RFC 7636 chars), optional `resource`.
   - In-memory auth code TTL is **60 s** and atomically deleted on first presentation.
   - Returns `{access_token, token_type: "Bearer", expires_in, scope}`. **No refresh token.**
   - Token format: `base64url(JSON claims) + "." + unpadded_base64url(HMAC-SHA256)` validated against a live `SessionStore` row. Ordinary TTL is **30 days**. Peer-link tokens in `v0.1.13` keep the same token string with a sliding 30-day DB deadline extended on use when >= 1 day short *(v0.1.10 peer tokens had fixed expiry)*.
6. **Revocation (`POST /oauth/mcp/revoke`, new in `v0.1.13`, `mcpOAuthHttp.ts:212–220`, `EnvironmentAuth.ts:1693–1709`):**
   - Form body `{token, token_type_hint?}`. Always returns empty HTTP `200` (whether revoked, unknown, omitted, or already revoked).
7. **Access enforcement (`packages/contracts/src/auth.ts:270–290`, `EnvironmentAuth.ts:723–730,1584–1599,1662–1685`):**
   - Scope mapping: `read-only` -> `orchestration:read`; `approval-required`/`auto-accept-edits`/`auto` -> +`orchestration:operate`; `full-access` -> +`settings:write`, +`source-control:write`.
   - All non-`/mcp` HTTP and `/ws` routes reject `subject: "mcp-client"` (except the two peer-link-only exceptions: `GET /api/links/thread/*` and `POST /api/peer-link/client-pairing`).

### 3.2 Pairing Codes & Token Exchange

- **Minting:**
  - CLI: `t3 auth pairing create [--scope S ...] [--ttl DUR] [--label L] [--base-url U] [--json]` or `t3 pair` (QR/URL); default TTL **5 min**, no CLI upper cap (`apps/server/src/cli/auth.ts:63–132`, `cli/pair.ts:481–541`, `PairingGrantStore.ts:326–345`).
  - HTTP: `POST /api/auth/pairing-token` requires `access:write` + every delegated scope (5-min TTL; `v0.1.13` adds `purpose: "peer-link"` with `access`; `auth/http.ts:498–525`, `EnvironmentAuth.ts:1429–1446`).
  - Code format: 12 chars from `23456789ABCDEFGHJKLMNPQRSTUVWXYZ`; atomically consumed once. Exchanging can narrow scopes (intersection), never widen; empty intersection fails without consuming an ordinary grant (`PairingGrantStore.ts:642–675`).
- **Browser cookie exchange:** `POST /api/auth/browser-session` JSON `{credential}` -> sets `HttpOnly; SameSite=Lax; Path=/` (`Secure` on HTTPS) **30-day** session cookie (`auth/http.ts:215–221,327–367`).
- **OAuth token exchange (`POST /oauth/token`, form-encoded, `auth/http.ts:382–464`, `EnvironmentAuth.ts:884–927,1143–1206`, `dpop.ts:56–138`):**
  - `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`, `subject_token=<code>`, `subject_token_type=urn:t3:params:oauth:token-type:environment-bootstrap`, `requested_token_type=urn:t3:params:oauth:token-type:access_token`, optional `scope`, `client_label`, `client_device_type`, `client_os`.
  - Without `DPoP` header: issues a **30-day** `Bearer` token.
  - With `DPoP` header (ES256 proof, normalized URL/method, `±300 s` window, replay-tracked `(thumbprint, jti)`): issues a **1-hour** DPoP-bound token (`Authorization: DPoP <token>` + per-request `DPoP` proof with `ath`).
  - **In-place DPoP renewal (new in `v0.1.13`):** Ordinary paired DPoP sessions carry `rnw=1` and a 30-day rolling DB row. Re-POST `/oauth/token` with `subject_token_type=urn:t3:params:oauth:token-type:access_token`, `subject_token=<old_token>` (accepted even if expired while row is live), no `scope`, and a fresh `DPoP` proof -> renews under the same `sessionId` and extends the row 30 days (`auth/http.ts:393–424`, `SessionStore.ts:968–1045`). CLI, desktop, T3 Connect, and plain Bearer sessions never self-renew.

### 3.3 Scoped Sessions (`t3 auth session issue`) & Complete Scope List

- **CLI:** `t3 auth session issue [--base-dir DIR] [--dev-url URL] [--scope S ...] [--ttl DUR] [--label L] [--subject SUBJ] [--token-only] [--json]` (`cli/auth.ts:293–325`, `EnvironmentAuth.ts:1381–1408`).
- Default subject: `cli-issued-session`; default TTL: **30 days** (no explicit max); default scopes: all 16 standard scopes (all except `review:write` and `relay:configure`); plain Bearer token, no DPoP, no renewal.

All **18** `AuthEnvironmentScope` strings (`packages/contracts/src/auth.ts:31–79,156–178`; `v0.1.10` had 17 — `relay:configure` is new in `v0.1.13`):

| Scope | Standard? | Governs (primary pointers) |
|---|---|---|
| `orchestration:read` | Yes | Shell/thread snapshots, history, streams, project/PR/issue/env reads (`RpcAuthorization.ts:54–74,146–181`) |
| `orchestration:operate` | Yes | Thread launch/dispatch/stop, attachments, voice transcribe, saving host project scripts (`RpcAuthorization.ts:65,188–204`, `speech/http.ts:86–91`) |
| `settings:write` | Yes | Settings/keybindings, voice model management, issue-tracker connections (`RpcAuthorization.ts:96–108,162–163`) |
| `providers:manage` | Yes | Provider config/install/auth, **any secret reference** in settings/voice/requests, `/logout` prompts (`RpcAuthorization.ts:75–92,280–286,337–362`) |
| `environment:maintain` | Yes | Server/desktop updates, process signals, telemetry retry (+`diagnostics:read`) (`RpcAuthorization.ts:93–95,131,323–325`) |
| `preview:operate` | Yes | Preview mutations/interactions, browser file upload (`RpcAuthorization.ts:229–236`, `ServerBrowserStream.ts:74–77`) |
| `diagnostics:read` | Yes | Traces, process/resource telemetry, usage summaries (`RpcAuthorization.ts:122–130,214`) |
| `terminal:read` | Yes | Terminal observe and metadata (`RpcAuthorization.ts:223,228`) |
| `terminal:operate` | Yes | Terminal open/attach/write/close, preview tunnel, layout/workflow file writes (`RpcAuthorization.ts:221–227,237`, `PreviewTunnel.ts:169`) |
| `source-control:write` | Yes | PR/issue mutations, git/VCS write operations, cloning (`clientRpcPermissions.ts:32–75,119–130`) |
| `filesystem:read` | Yes | Project file reads/search, local diffs, workspace assets, voice model linking (`RpcAuthorization.ts:182–197,218–220,326–335`) |
| `filesystem:write` | Yes | `projects.writeFile` (`RpcAuthorization.ts:187`) |
| `access:read` | Yes | Client/pairing access listings & streams, `peerLinks.list` (`RpcAuthorization.ts:101,253`) |
| `access:write` | Yes | Minting/revoking pairings/sessions/Connect devices, guest shares, peer link CRUD, preview-tunnel setting (`auth/http.ts:503–598`, `settings.ts:2079–2080`) |
| `relay:read` | Yes | Relay status, Connect link state and reachability (`RpcAuthorization.ts:144`, `cloud/http.ts:132–145`) |
| `relay:write` | Yes | Relay client install, unlink, Connect preferences (`RpcAuthorization.ts:145`, `cloud/http.ts:146–161`) |
| `relay:configure` | **No (admin only, new in v0.1.13)** | `/api/connect/link-proof`, `/api/connect/relay-config` (`cloud/http.ts:118–140`, `auth.ts:49–50,175–178`) |
| `review:write` | **Retired** | Accepted on old token decode only; cannot be newly granted (`auth.ts:44–45,75–79`) |

Compatibility: auth responses emit both legacy `scopes` and exact `permissions`; `requiredPermission` accompanies legacy `requiredScope` on `403` (mapping `relay:configure` -> `relay:write` for old decoders; `auth.ts:82–151`).

### 3.4 WebSocket Tickets

- **Endpoints:** `POST /api/auth/websocket-ticket/:use` (`use` = `"rpc"` or `"media"`) or legacy `POST /api/auth/websocket-ticket` (no audience restriction). Requires any authenticated environment session (no extra scope; rejects `mcp-client`). Returns `{ticket, expiresAt}` (`auth/http.ts:470–491`, `EnvironmentAuth.ts:1553–1635`, `auth.ts:253–268`).
- **Lifetime & reuse:** **5-minute** signed ticket, **reusable** across reconnects until expiry as long as the parent session remains unrevoked (`SessionStore.ts:491–521,1049–1155`).
- **Usage on upgrade:** Pass `?wsTicket=<ticket>` (with `?orchestrationProtocol=2` on `/ws`). For DPoP-bound sessions, `rpc` tickets embed `jkt` and require a fresh single-use ES256 DPoP proof (with `ath` over the ticket) via `DPoP` header or `?dpop=<proof>` on every upgrade (`dpop.ts:91–138`, `EnvironmentAuth.ts:1584–1635`). `media` and bare tickets are not DPoP-bound. Unchanged from `v0.1.10`.

---

## 4. HTTP API (All 59 `EnvironmentHttpApi` Endpoints + Raw Routes)

### 4.1 `EnvironmentHttpApi` (59 endpoints, `packages/contracts/src/environmentHttp.ts`)

"Session" = cookie, `Authorization: Bearer`, or `Authorization: DPoP` + `DPoP` proof (`apps/server/src/auth/http.ts:243–279`). All five `orchestration` routes also require header `x-t3-orchestration-protocol: 2` (`environmentHttp.ts:106–110`). `+` marks the 3 endpoints added since `v0.1.10` (56 -> 59).

| Group | Method & Path | Required Auth / Scope | Purpose | Contract (`environmentHttp.ts`) / Handler |
|---|---|---|---|---|
| `metadata` (1) | `GET /.well-known/t3/environment` | Public | Environment ID, capabilities, auth policy | `:590` / `http.ts:392` |
| `auth` (11) | `GET /api/auth/session` | Optional session | Auth state, descriptor, `sessionId` *(v0.1.13)* | `:597` / `auth/http.ts:294` |
| | `POST /api/auth/browser-session` | Bootstrap credential | Exchange pairing credential for browser cookie | `:604` / `auth/http.ts:327` |
| | `POST /oauth/token` | Bootstrap credential or renewable DPoP token | Form token exchange or DPoP renewal *(v0.1.13)* | `:611` / `auth/http.ts:380` |
| | `POST /api/auth/websocket-ticket` | Session | Mint legacy untyped WS ticket (5 min) | `:619` / `auth/http.ts:470` |
| | `POST /api/auth/websocket-ticket/:use` | Session | Mint typed `rpc` or `media` WS ticket (5 min) | `:626` / `auth/http.ts:484` |
| | `POST /api/auth/pairing-token` | `access:write` + all delegated scopes | Mint client, MCP, or `peer-link` *(v0.1.13)* code | `:634` / `auth/http.ts:498` |
| | `POST /api/auth/pairing-links/revoke` | `access:write` | Revoke pairing grant by ID | `:642` / `auth/http.ts:529` |
| | `POST /api/auth/clients/revoke` | `access:write` | Revoke another client session | `:650` / `auth/http.ts:543` |
| | `POST /api/auth/connect-devices/approve` | `access:write` | Approve pending T3 Connect device | `:658` / `auth/http.ts:564` |
| | `POST /api/auth/connect-devices/revoke` | `access:write` | Revoke Connect device key & sessions | `:666` / `auth/http.ts:578` |
| | `POST /api/auth/clients/revoke-others` | `access:write` | Revoke all other clients & unspent grants | `:674` / `auth/http.ts:592` |
| `mcpOAuth` (9) | `GET /.well-known/oauth-protected-resource` | Public | OAuth protected resource metadata | `:683` / `mcpOAuthHttp.ts:103` |
| | `GET /.well-known/oauth-protected-resource/mcp` | Public | Same metadata at `/mcp`-suffixed path | `:688` / `mcpOAuthHttp.ts:103` |
| | `GET /.well-known/oauth-authorization-server` | Public | OAuth AS metadata | `:693` / `mcpOAuthHttp.ts:109` |
| | `POST /oauth/mcp/register` | Public | Stateless signed `client_id` registration | `:698` / `mcpOAuthHttp.ts:112` |
| | `GET /oauth/mcp/authorize` | Registered client + PKCE S256 | Redirect to approval UI | `:704` / `mcpOAuthHttp.ts:135` |
| | `POST /oauth/mcp/approval` | Optional environment session | Validate auth request & mint approval CSRF | `:706` / `mcpOAuthHttp.ts:168` |
| | `POST /oauth/mcp/decision` | Session+CSRF or pairing code (approve); public (deny) | Approve or deny MCP sign-in -> `{redirectTo}` | `:715` / `mcpOAuthHttp.ts:193` |
| | `POST /oauth/mcp/token` | Auth code + PKCE verifier | Exchange code for 30d MCP bearer token | `:724` / `mcpOAuthHttp.ts:206` |
| | `POST /oauth/mcp/revoke` **+** | Token possession (idempotent 200) | Revoke live MCP bearer token | `:731` / `mcpOAuthHttp.ts:212` |
| `orchestration` (5) | `GET /api/orchestration/shell` | `orchestration:read` + protocol `2` | Shell snapshot (or `{resumable: true}` via `resumeFrom`) | `:775` / `orchestration-v2/http.ts:188` |
| | `GET /api/orchestration/threads/:threadId` | `orchestration:read` + protocol `2` | Full thread projection (streamed JSON in v0.1.13) | `:783` / `orchestration-v2/http.ts:201` |
| | `GET /api/orchestration/threads/:threadId/transcript` | `orchestration:read` + protocol `2` | Thread transcript | `:791` / `orchestration-v2/http.ts:226` |
| | `GET /api/orchestration/threads/:threadId/bounded` | `orchestration:read` + protocol `2` | Bounded recent thread snapshot (optional `idTable`) | `:799` / `orchestration-v2/http.ts:251` |
| | `GET /api/orchestration/threads/:threadId/history` | `orchestration:read` + protocol `2` | Paginated thread history by `cursor` | `:808` / `orchestration-v2/http.ts:277` |
| `projects` (2) | `GET /api/projects` | `orchestration:read` | Project catalog snapshot | `:819` / `project/http.ts:44` |
| | `POST /api/projects/mutate` | `orchestration:operate` (+`settings:write` if scripts) | Create/update/delete/clone project | `:826` / `project/http.ts:55` |
| `pullRequests` (1) | `POST /api/pull-requests/diff` | `orchestration:read` | Load pull request diff | `:835` / `pullRequest/http.ts:15` |
| `voice` (16) | `GET /api/voice/cloud/keys` | `orchestration:operate` | Cloud speech key status | `:856` / `speech/http.ts:94` |
| | `POST /api/voice/cloud/:provider/key` | `settings:write` (+`providers:manage` for secret ref) | Set/clear cloud speech key | `:863` / `speech/http.ts:103` |
| | `GET /api/voice/cloud/openrouter/models` | `orchestration:operate` | List OpenRouter speech models | `:877` / `speech/http.ts:120` |
| | `POST /api/voice/cloud/:provider/transcribe` | `orchestration:operate` | Cloud audio transcription | `:884` / `speech/http.ts:127` |
| | `GET /api/voice/status` | `orchestration:operate` | Local speech engine status | `:900` / `speech/http.ts:149` |
| | `POST /api/voice/models/prepare` | `orchestration:operate` | Prepare local speech model | `:907` / `speech/http.ts:156` |
| | `GET /api/voice/models` | `orchestration:operate` (+`filesystem:read` for linked paths) | Local speech model catalog | `:914` / `speech/http.ts:163` |
| | `POST /api/voice/models/download` | `settings:write` | Download local speech model | `:922` / `speech/http.ts:172` |
| | `POST /api/voice/models/select` | `settings:write` | Select active speech model | `:930` / `speech/http.ts:181` |
| | `POST /api/voice/models/cancel` | `settings:write` | Cancel model download | `:938` / `speech/http.ts:190` |
| | `POST /api/voice/models/remove` | `settings:write` | Remove downloaded model | `:946` / `speech/http.ts:199` |
| | `POST /api/voice/models/link` | `settings:write` + `filesystem:read` | Link speech models from host path | `:954` / `speech/http.ts:208` |
| | `POST /api/voice/acceleration` | `settings:write` | Configure speech hardware acceleration | `:962` / `speech/http.ts:217` |
| | `POST /api/voice/model-unload-timeout` | `settings:write` | Configure model unload timeout | `:970` / `speech/http.ts:224` |
| | `POST /api/voice/transcribe` | `orchestration:operate` | Local audio transcription | `:978` / `speech/http.ts:231` |
| | `POST /api/voice/post-process` | `orchestration:operate` | Post-process transcript text | `:988` / `speech/http.ts:243` |
| `connect` (9) | `POST /api/connect/link-proof` | `relay:configure` *(was `relay:write` in v0.1.10)* | Issue signed environment link proof | `:998` / `cloud/http.ts:118` |
| | `POST /api/connect/relay-config` | `relay:configure` *(was `relay:write` in v0.1.10)* | Apply relay runtime config | `:1006` / `cloud/http.ts:126` |
| | `GET /api/connect/link-state` | `relay:read` | Read Connect state (`linkableHere` = configure+write) | `:1014` / `cloud/http.ts:132` |
| | `POST /api/connect/reachability` | `relay:read` | Probe Connect reachability | `:1021` / `cloud/http.ts:143` |
| | `POST /api/connect/unlink` | `relay:write` | Unlink T3 Connect | `:1028` / `cloud/http.ts:146` |
| | `POST /api/connect/preferences` | `relay:write` | Update relay preferences | `:1035` / `cloud/http.ts:151` |
| | `POST /api/connect/live-activity-preference` **+** | `relay:write` | Toggle `liveActivitiesEnabled` | `:1043` / `cloud/http.ts:156` |
| | `POST /api/t3-connect/health` | Signed relay proof (`environment:status`) | Cloud relay health/descriptor check | `:1051` / `cloud/http.ts:163` |
| | `POST /api/t3-connect/mint-credential` | Signed relay proof (`environment:connect`, `cnf.jkt`) | Mint proof-bound Connect bootstrap credential | `:1058` / `cloud/http.ts:168` |
| `peerLink` (1) | `POST /api/peer-link/client-pairing` **+** | Full-access peer-link MCP bearer + Labs on + no `Origin` | Mint 3-min client or peer-link pairing code | `:1070` / `peer/http.ts:22` |
| `webhooks` (4) | `POST /api/hooks/:hookId/:token` | URL token (+ optional HMAC-SHA256) | Deliver scheduled-task or workflow (`wf-`) webhook | `:1097` / `webhookRoute.ts:46` |
| | `PUT /api/hooks/:hookId/:token` | URL token (+ optional HMAC-SHA256) | Same | `:1098` / `webhookRoute.ts:46` |
| | `PATCH /api/hooks/:hookId/:token` | URL token (+ optional HMAC-SHA256) | Same | `:1099` / `webhookRoute.ts:46` |
| | `GET /api/hooks/:hookId/:token` | URL token (+ optional HMAC-SHA256) | Same (`HEAD` explicitly returns `405`) | `:1100` / `webhookRoute.ts:46` |

### 4.2 Raw `HttpRouter` Endpoints Mounted Alongside `EnvironmentHttpApi`

Mounted in `apps/server/src/server.ts:918–934`:
- `POST /mcp` (`GET`/`DELETE` `[Guess]`): Streamable HTTP MCP; provider token or `mcp-client` Bearer; rejects `Origin` (`McpHttpServer.ts:1536`).
- `GET /ws`: Effect RPC WebSocket (`?orchestrationProtocol=2`, normal auth or `wsTicket`; `ws.ts:3828`).
- `GET /.well-known/t3/desktop-readiness?challenge=...` *(new in v0.1.13)*: Desktop launch proof via launch secret (`http.ts:408`).
- `POST /api/observability/v1/traces`: Session + `orchestration:operate`; renderer OTLP trace ingest (`http.ts:435`).
- `GET /api/assets/<signed-token>/<filename>` & `POST /api/attachments/upload/<signed-token>`: Expiring signed capability URLs; no session required on redemption (`http.ts:516–613`).
- `POST /api/guest/{redeem,messages}` & `GET /api/guest/{stream,session,render,item,history,command-output,attachment,media}`: Guest share HMAC cookie (`guest/http.ts:79–349`).
- `GET /api/links/thread/{stream,history,item}` *(new in v0.1.13)*: Peer-link MCP bearer, no `Origin`, Labs on (`peer/linkThreadHttp.ts:41–153`).
- `* /api/device-hub/*`: Media auth + `orchestration:read` / `orchestration:operate` (`DeviceHubProxy.ts:200–270`).
- `GET /api/preview-stream/*`, `POST /api/preview-stream/upload`: Media auth + `orchestration:read` / `preview:operate` (`ServerBrowserStream.ts:56–312`).
- `GET /api/preview/tunnel?port=...` *(new in v0.1.13)*: WS upgrade, media auth + `terminal:operate` (`PreviewTunnel.ts:84–189`).
- `GET /ws/voice`: Speech WS, media auth + `orchestration:operate` (v0.1.13 adds idle timeout; `speech/stream.ts:20–133`).
- `GET *`: Static SPA / dev proxy fallback (`http.ts:685–719`).

---

## 5. WebSocket RPC (`/ws`)

> **Stability warning:** `/ws` is T3 Code's **internal first-party protocol** (`packages/contracts`). Method names and coarse protocol `2` held across `v0.1.10`–`v0.1.13`, but payload/record schemas evolve continuously across minor releases with no public backwards-compatibility contract.

### 5.1 Connection, Protocol Negotiation, and Framing

- **Handshake:** `GET /ws?orchestrationProtocol=2` (`packages/contracts/src/environment.ts:13–16`, `apps/server/src/ws.ts:814–819,3828–3847`). Missing/wrong version returns HTTP `426`:
  ```json
  {"code":"orchestration_protocol_incompatible","message":"Update this client to one that supports orchestration protocol 2.","orchestrationProtocolVersion":2}
  ```
- **Auth:** `authenticateWebSocketUpgrade(request, "rpc")` (`EnvironmentAuth.ts:1584–1636`): `?wsTicket=<ticket>` takes precedence (with `DPoP` header or `?dpop=<proof>` if proof-bound); otherwise falls back to session cookie / `Bearer` / `DPoP`. Rejects `mcp-client`.
- **Optional query params (`ws.ts:805–900`):**
  - `clientCapabilities`: comma-separated tokens (`settingsSecretReferences`, and in `v0.1.13` `httpUsageLimitSources`).
  - `frameParts`: presence enables splitting text messages > 48 KiB UTF-8 into consecutive frames with `\u001e` prefix on every part before the last (`packages/shared/src/socketFrameParts.ts:5–72`). Omit to receive whole JSON frames.
  - Analytics metadata: `clientSurface`, `clientAppVersion`, `clientDeviceType`, `clientOs`, `clientWebDeployment`, `clientBrowser`, `connectionMethod`, `clientOsMajorVersion`, `clientDeviceModel`, `clientOwner`, `clientDevice`.
- **Wire envelope (Effect RPC `4.1.5` JSON serialization, `ws.ts:3806–3810,3867–3903`):**
  - Request: `{"_tag":"Request","id":"1","tag":"<rpcTag>","payload":{...},"headers":[]}`
  - Unary exit: `{"_tag":"Exit","requestId":"1","exit":{"_tag":"Success","value":...}}` or `exit.cause[i] = {"_tag":"Fail","error":...}`
  - Stream chunk: `{"_tag":"Chunk","requestId":"1","values":[...]}` -> client sends `{"_tag":"Ack","requestId":"1"}` per chunk (shell/thread/config streams use an **8-chunk window**; terminal adds a 64 KiB window; `terminal/OutputProtocol.ts:16–34,73–139`), or `{"_tag":"Interrupt","requestId":"1"}` to cancel.
- **Heartbeat (new in `v0.1.13`):** Server sends `{"_tag":"Pong"}` every **20 s** on all `/ws` sockets (`apps/server/src/socketHeartbeat.ts:5–25`, `ws.ts:3951–3964`). Handle independently of request IDs.

### 5.2 Key Orchestration RPC Schemas

#### `orchestration.subscribeShell` (`orchestration:read`, `stream: true`, `rpc.ts:2275`, `orchestrationV2.ts:2391–2636,4224–4248`)

- **Request:** `{ afterSequence?: nn, httpSnapshotFallback?: true, knownProjectIdentities?: s, requestCompletionMarker?: b, omitIdleSubagents?: b, omitOlderSettled?: b }`
- **Stream item (`kind` union):**
  - `snapshot`: `{ kind: "snapshot", snapshot: { schemaVersion, snapshotSequence, projects, threads, archivedThreads, olderSettled? }, resolvedRepositoryIdentityRoots?: s[] }`
  - `synchronized`: `{ kind: "synchronized" }` (after initial snapshot/replay if `requestCompletionMarker: true`)
  - `snapshot-required`: `{ kind: "snapshot-required" }` (on unreplayable resume when `httpSnapshotFallback: true` -> re-fetch `GET /api/orchestration/shell` and resubscribe)
  - `project.updated` (`{sequence, project}`), `project.removed` (`{sequence, projectId}`), `thread.updated` (`{sequence, location: "active"|"archive", thread}`), `thread.removed` (`{sequence, location: "active"|"archive", threadId}`).
- *v0.1.10 diff:* Envelope unchanged; v0.1.13 adds optional `issues`, `workflowRun`, workflow lineage on thread rows and `trackedFolders` on project rows.

#### `orchestration.subscribeThread` (`orchestration:read`, `stream: true`, `rpc.ts:2282`, `orchestrationV2.ts:4345–4482`, `ThreadStream.ts:18–116`)

- **Request:** `{ threadId: s, afterSequence?: nn, requestCompletionMarker?: b, acceptBoundedSnapshot?: b, acceptDeferredWork?: b, acceptCompactTurnItems?: b, acceptDedupedBodies?: b, acceptCompactIds?: b }`
- **Stream item (`kind` union):**
  - `snapshot`: `{ kind: "snapshot", snapshotSequence: nn, projection: ThreadProjection, historyCursor?: s|null, hasMoreHistory?: b, latestLocalTurnOrdinal?: nn|null, turnItemsOmitLocalVisible?: true, ids?: (s|n)[] }`
  - `event`: `{ kind: "event", sequence: nn, event: { id, threadId, occurredAt, type, payload, runId?, nodeId?, driver?, providerInstanceId?, rawEventId? } }` (unknown event/turn-item tags decode to `{kind: "unknown-event", sequence, eventType}`)
  - `synchronized`: `{ kind: "synchronized" }`
- **Replay limits:** Max **128 events** and **1 MiB** raw / projected encoding; exceeding limits or pruned cursors triggers a replacement snapshot. Bounded snapshot page policy: 10 user turns / 75 items / 500 turn items / 1 MiB (`threadHistoryPaging.ts:12–32`).
- *v0.1.10 diff:* v0.1.13 registers a thread viewer, trims replay at last `thread.deleted`, and adds domain events `thread.issue-synced` and `run.output-submitted`.

#### `orchestration.dispatchCommand` (`orchestration:operate`, unary, `rpc.ts:2133`, `orchestrationV2.ts:3316–3850,4213–4216`)

- **Request:** `{ type: <variant>, commandId: s, ...fields }` (idempotent by `commandId`).
- **Response:** `{ sequence: nn }` | `OrchestrationV2DispatchCommandError { commandId, commandType, message, detail?, cause? }` | `EnvironmentAuthorizationError`.
- **Key variants:**
  - `thread.create` (`:3329`): `threadId`, `projectId`, `title`, `createdBy`, `creationSource`, `modelSelection`, `runtimeMode`, `interactionMode`, `branch: s|null`, `worktreePath: s|null`, `titleRenamed?`, `importedNativeThread?`. *(Use `launchThread` instead when launching with an initial message.)*
  - `message.dispatch` (`:3615`): `threadId`, `messageId`, `text`, `attachments`, `dispatchMode: {type:"start_immediately"} | {type:"queue_after_active"} | {type:"steer_active", targetRunId} | {type:"restart_active", targetRunId} | {type:"defer_start", workspaceStrategy?}`, optional `deliveryIntent?: "auto"|"steer"|"restart"` (server resolves mode against live run state), `context?`, `modelSelection?`, `freshContext?`, `outputSchema?` *(v0.1.13; never steers)*, `explicitQueue?: true` *(v0.1.13)*, `senderThreadId?`, `linkedSender?`, `scheduledTaskId?`, `guestMessageId?`.
  - **All other 56 public command `type`s (`orchestrationV2.ts:3318–3849`; `+` = added in `v0.1.13`, 56 -> 58 total):** `thread.stop` (`reason?`, `stopDelegatedTasks?`), `thread.archive`, `thread.unarchive`, `thread.delete` (`onlyIfUnused?`), `thread.guest-share.close`, `thread.guest-message.dismiss`, `thread.settle`, `thread.auto-settle`, `thread.unsettle`, `thread.snooze`, `thread.unsnooze`, `thread.auto-settle.set`, `thread.pin`, `thread.unpin`, `thread.tag.set`, `thread.pin.reorder`, `thread.active.reorder`, `thread.visit`, `thread.mark-unread`, `thread.metadata.update`, `thread.pull-request.link`, `thread.pull-request.unlink`, `thread.pull-request-link.sync`, `thread.pull-request.watch` *(v0.1.13 adds `until?`, `checksPending?`)*, `thread.pull-request.sync`, `thread.issue.link` **+**, `thread.issue.unlink` **+**, `thread.title.regeneration.complete`, `thread.runtime-mode.set`, `thread.interaction-mode.set`, `thread.model-selection.set`, `provider-session.detach`, `prepared-run.{release,progress,fail,retry}`, `notification.delivery.accept`, `subagent.stop`, `run.interrupt`, `queued-message.promote-to-steer`, `queue.resume`, `queued-run.{start,reorder,cancel,edit}` *(v0.1.13 `edit` adds `modelSelection?`)*, `runtime-request.respond`, `thread.user-input.dismiss`, `checkpoint.rollback`, `thread.fork`, `thread.merge_back`, `delegated_task.{request,wake-policy,completion-delivery.acknowledge,completion-delivery.dispose}`, `thread.created.record`, `provider.switch`.

#### `orchestration.launchThread` (`orchestration:operate`, unary, `rpc.ts:2269`, `orchestrationV2.ts:4181–4211`)

- **Request:** `{ commandId: s, creationSource?: CreationSource, threadId?: s, reuseExistingThread?: b, projectId: s, title: s, generateTitle?: b, modelSelection: ModelSelection, runtimeMode: Runtime, interactionMode: "default"|"plan", workspaceStrategy: Workspace, initialMessage?: { messageId?: s, text: s, context?: OrchestrationMessageContext, attachments: ChatAttachment[] } }`.
- **Response:** `{ threadId: s, projection: ThreadProjection, resumed: b, sequence?: nn }` | `OrchestrationV2ThreadLaunchError { commandId, projectId, message, cause? }`.
- *v0.1.10 diff:* `worktree` strategy adds `repositories?: {path: s}[]` (<= 128); `/logout` in `initialMessage` now requires `providers:manage`.

#### `orchestration.getThreadProjection` & `idTable` Compaction (`orchestration:read`, unary, `rpc.ts:2224`, `orchestrationV2.ts:2355–2380`, `orchestrationV2IdTable.ts:4–124`)

- **Request:** `{ threadId: s }`.
- **Response:** `{ thread, runs, attempts, nodes, subagents, providerSessions, providerThreads, providerTurns, runtimeRequests, messages, plans, turnItems, checkpointScopes, checkpoints, contextHandoffs, contextTransfers, visibleTurnItems, updatedAt }`. Returns a bounded window (77-row limit) with no cursor and no `idTable` opt-in.
- **`idTable` compaction (opt-in on `subscribeThread` bounded snapshots and HTTP `/bounded`/`/history`):** Wire encoder removes `idTable: true`, adds a sibling `ids: (string|number)[]` table, and interns values of keys named `id`, `*Id`, and array elements of `*Ids` as zero-based integer indices within immediate record collections and `record.item`. Unchanged from `v0.1.10`.

### 5.3 Complete `WsRpcGroup` Catalog (261 tags, 33 streaming; +40 since `v0.1.10`)

`*` = `stream: true`, `+` = added in `v0.1.13` (0 removed/renamed; `packages/contracts/src/rpc.ts:2499–2761`, `apps/server/src/auth/RpcAuthorization.ts:45–387`, `packages/contracts/src/clientRpcPermissions.ts:21–139`):

- **Orchestration V2 (23):**
  - `orchestration:operate`: `orchestration.dispatchCommand` (`:2133`), `stopWorkflow` (`:2233`), `forceStopThread` (`:2239`), `launchThread` (`:2269`).
  - `orchestration:read`: `getWorkflowScript` (`:2245`), `getNativeChildWork` (`:2254`), `getTurnItem` (`:2263`), `getTurnDiff` (`:2139`), `getFullThreadDiff` (`:2145`), `searchThreads` (`:2170`), `searchThread` (`:2154`), `searchThreadStream`* (`:2160`), `getArchivedShellSnapshot` (`:2176`), `getThreadFamily` (`:2185`), `getThreadTitles` (`:2191`), `getThreadLinkMetadata` (`:2197`), `getWorktreeThreadIds` (`:2206`), `getOlderSettledThreads` (`:2215`), `getThreadProjection` (`:2224`), `subscribeShell`* (`:2275`), `subscribeThread`* (`:2282`), `subscribeCommandOutput`* (`:2289`), `subscribePromptSuggestion`* (`:2299`).
- **Pull requests (35):**
  - `orchestration:read`: `pullRequests.{list,listStats,filterOptions,summary,routing,routingIdentity,stack,linkedThreads,detail,preview,checks,activity,reviewGuide,threadComments,diffFileContents,filesViewed,changeBreakdown,invalidate,reportState,reviewerCandidates,labelCandidates}` (`:1236–1443`), `pullRequests.subscribeRefreshes`* (`:1412`), `pullRequests.subscribeLinkedThreadRefreshes`* (`:1420`).
  - `orchestration:operate`: `pullRequests.generateReviewGuide` (`:1315`).
  - `source-control:write`: `pullRequests.{setFilesViewed,runAction,update,comment,updateComment,submitReview,replyToThread,setThreadResolution,setReaction,requestReviewers,setLabels}` (`:1345–1449`).
- **Issues & trackers (14, all `+`):**
  - `orchestration:read`: `issues.{list,detail,comments,templates,candidates,linkedThreads}` (`:1461–1493`), `issueTrackers.list` (`:1535`).
  - `source-control:write`: `issues.{create,comment,update,setState,react}` (`:1499–1523`).
  - `settings:write`: `issueTrackers.{connect,bindProject}` (`:1545–1551`).
- **Projects & workspaces (22):**
  - `source-control:write`: `projectClone.{start,cancel,retry}` (`:1611–1627`), `workspace.addTaskFolderRepository` `+` (`:1741`).
  - `orchestration:read`: `subscribeProjectClones`* (`:1633`).
  - `filesystem:read`: `projects.{listEntries,listIgnoredFiles,readFile,searchContents,searchEntries}` (`:1646–1670`), `filesystem.{browse,getMetadata}` (`:1711–1717`), `workspace.{listFolders,scanFolders,taskFolder,layoutCommands}` `+` (`:1723–1750`).
  - `orchestration:operate`: `projects.{ensureScratch,createNew,createFromThread,mutate}` (`:1682–1700`), `shell.openInEditor` (`:1706`).
  - `filesystem:write`: `projects.writeFile` (`:1676`).
  - `terminal:operate`: `workspace.approveLayoutCommands` `+` (`:1756`).
- **VCS / Git / Source control (28):**
  - `orchestration:read`: `sourceControl.{lookupRepository,appAccess,starred}` (`:1557–1597`), `subscribeVcsStatus`* (`:1856`), `subscribeVcsBranch`* (`:1843`), `vcs.{refreshBranch,refreshStatus,listRefs}` (`:1850–1910`), `subscribeWorktreeSetup`* (`:1878`), `git.resolvePullRequest` (`:1898`).
  - `source-control:write`: `sourceControl.{cloneRepository,publishRepository,star}` (`:1563–1640`), `vcs.{pull,createWorktree,removeWorktree,createRef,switchRef,init}` (`:1863–1940`), `git.runStackedAction`* (`:1891`), `git.preparePullRequestThread` (`:1904`).
  - `settings:write`: `sourceControl.{signInStart,signInWait,signInCancel}` (`:1570–1584`).
  - `orchestration:operate`: `worktreeSetup.cancel` (`:1885`).
  - `filesystem:read`: `review.{getBranchCommits,getDiffPreview,getDiffFileContents}` (`:1945–1957`).
- **Scheduled tasks (8):** `orchestration:read` -> `scheduledTasks.subscribe`* (`:2406`, strips webhook secrets unless caller also has `orchestration:operate`); `orchestration:operate` -> `scheduledTasks.{upsert,setEnabled,delete,runNow,rotateWebhookToken,listWebhookDeliveries,getWebhookDelivery}` (`:2413–2471`).
- **Peer links (10):** `access:read` -> `peerLinks.list` (`:975`); `orchestration:read` -> `peerLinks.names` `+` (`:982`), `peerLinks.subscribeThread`* `+` (`:1001`), `peerLinks.{threadHistoryPage,threadItem}` `+` (`:1008–1014`); `access:write` -> `peerLinks.{link,unlink,answerRequest}` (`:988–1032`), `peerLinks.{pairClient,pairLink}` `+` (`:1020–1026`).
- **Terminal (8):** `terminal:operate` -> `terminal.{open,write,resize,clear,close}` + `terminal.attach`* (`:1963–1998`); `terminal:read` -> `terminal.observe`* (`:1976`), `subscribeTerminalMetadata`* (`:2309`).
- **Preview (11):** `preview:operate` -> `preview.{open,resize,adjust,close,clearProfile,reportProfiles,reportStatus}` (`:2003–2042`); `orchestration:read` -> `preview.list` (`:2026`), `subscribePreviewEvents`* (`:2047`), `subscribeDiscoveredLocalServers`* (`:2059`); `terminal:operate` -> `preview.stopLocalServer` (`:2054`).
- **Device (11):** `settings:write` -> `device.{configure,testHost,easAccount,simlockIdentity}` (`:2068–2092`); `orchestration:read` -> `device.{list,detail}` + `subscribeDeviceState`* (`:2086–2126`); `orchestration:operate` -> `device.{open,close,shutdown,action}` (`:2098–2120`).
- **Server, providers, workflows, diagnostics, auth, other (91):**
  - `orchestration:read`: `server.{probe,getConfig,refreshProviders,getNpmRelease,getSettings,discoverSourceControl,searchAcpRegistry,listAcpRegistrySessions,listAcpRegistryProviders,getHostResources,reportClientActivity}`, `server.getProviderRuntimeModeLimits` `+`, `server.getStorageCleanupReport`*, `server.getDatabaseStorage`*, `provider.install.subscribe`*, `workflows.subscribe`* `+`, `workflows.counts`* `+`, `workflows.{get,about}` `+`, `agentSessions.scan`, `assets.createUrl`, `mcpApps.{toolInfo,readResource}`, `subscribeServerConfig`*, `subscribeServerLifecycle`* (`:765–2367`).
  - `providers:manage`: `server.updateProvider`, `provider.consumeResetCredit`, `provider.auth.{start,complete,respond,cancel,removeMethod,logout}` + `provider.auth.subscribe`*, `provider.chatgpt.{reconnect-profile,import-profile}` + `handoff.subscribe`*, `provider.install.{start,cancel,remove}`, `server.{prepareAcpRegistryAgent,uninstallAcpRegistryManagedBinary,acceptAcpRegistryUrlAuth,setAcpRegistryProvider,disableAcpRegistryProvider,logoutAcpRegistry}`, `provider.mcpSignIn.{start,complete}` `+` (`:827–1837`).
  - `environment:maintain`: `server.{updateServer,commitDesktopUpdate,reclaimDatabaseSpace,signalProcess}`, `server.updateServerWithProgress`* (`:932–1206`).
  - `settings:write`: `server.{upsertKeybinding,removeKeybinding,runStorageCleanup,updateSettings,checkSecretSource,checkOtlpEndpoint,listTextGeneratorModels}` (`:753–1182`).
  - `orchestration:operate`: `server.{importAcpRegistrySession,deleteAcpRegistrySession,prepareTriage}`, `workflows.{answer,resume,cancel}` `+`, `secrets.answerRequest`, `agentFeedback.answerRequest` `+`, `agentSessions.import`, `assets.persistChatAttachments`, `attachments.{createUploadUrl,delete}`, `mcpApps.{callTool,updateModelContext}`, `provider.uploadFeedback` (`:1098–2457`).
  - `diagnostics:read`: `server.{getTraceDiagnostics,getProcessDiagnostics,getProcessResourceHistory,getResourceTelemetryHistory,retryResourceTelemetry,getUsageSummary,refreshUsageRates}`, `subscribeResourceTelemetry`* (`:1137–2487`).
  - `filesystem:read`: `workflows.getDefinition` `+` (`:2388`).
  - `terminal:operate`: `workflows.{saveDefinition,deleteDefinition}` `+` (`:2394–2400`).
  - `relay:read` / `relay:write`: `cloud.getRelayClientStatus` (`:1212`), `cloud.installRelayClient`* (`:1218`).
  - `access:read` / `access:write`: `subscribeAuthAccess`* (`:2480`), `guestShare.create` (`:2446`).

**Payload-dependent extra scopes (`RpcAuthorization.ts:273–387`):** `/logout` in dispatch/launch/queued-edit adds `providers:manage` *(new in v0.1.13)*; secret references in `device.{easAccount,simlockIdentity}`, `server.listTextGeneratorModels`, `secrets.answerRequest`, `server.updateSettings` add `providers:manage` *(new in v0.1.13)*; `peerLinks.pairLink` adds the requested access scopes; `guestShare.create` adds `orchestration:read`; `git.preparePullRequestThread` (`worktree`+`threadId`) adds `orchestration:operate`; `git.runStackedAction` (`create_pr`/`commit_push_pr`+`threadId`) adds `orchestration:read`; `server.retryResourceTelemetry` adds `environment:maintain`; `assets.createUrl` adds `filesystem:read` for workspace/media/app-icon paths; `device.list` adds `orchestration:operate` for `retryHostId`/`updateTool`.

---

## 6. Inbound Webhooks & Scheduled Tasks

### 6.1 Endpoint, Token, HMAC Signature, and Relay Proof

- **URL & methods:** `/api/hooks/:hookId/:token` accepts `GET`, `POST`, `PUT`, `PATCH`; `HEAD` returns HTTP `405` with `Allow: GET, POST, PUT, PATCH` (`ScheduledTaskService.ts:85,394–407`, `webhookRoute.ts:57–61,188–192`). Workflow webhooks use `wf-`-prefixed hook IDs on the same route (`webhookRoute.ts:106–140`).
- **URL token:** 32 random bytes as unpadded Base64URL (**43 chars**). Verified with timing-safe comparison; an invalid token returns `404` indistinguishable from an unknown hook. Rotatable via `scheduledTasks.rotateWebhookToken` (`ScheduledTaskService.ts:635–638,1789–1814,2208–2219`, `webhookVerification.ts:24–47`).
- **Optional HMAC-SHA256 signature (`packages/contracts/src/scheduledTask.ts:76–89`, `webhookVerification.ts:24–47`):**
  - Config: `{ header: string, encoding: "hex" | "base64", prefix: string }` (e.g. `header: "x-hub-signature-256"`, `encoding: "hex"`, `prefix: "sha256="`).
  - Verification: lowercase header lookup, trim value, case-insensitive prefix match, compute **HMAC-SHA256 over the exact raw request body bytes**, decode hex case-insensitively (or base64), verify MAC. Failure -> `401 rejected_signature`.
- **Relay delivery proof (`RelayDeliveryProof.ts:19–20,41–75`, `scheduledTask.ts:91–98`):** Optional T3 Connect JWT proof supplies trusted `deliveryId` and `receivedAt` (proof ceiling **25 h**) for `maxDeliveryAgeMinutes` (`1–1,440` min; `null`/omitted disables age cutoff).

### 6.2 Deduplication, Filters, and Prompt Rendering

- **`replayKey` (`scheduledTask.ts:100–117`, `webhookDedupe.ts:9–43`):** Template string (must contain >= 1 `{{body.path}}` / `{{headers.name}}` / `{{query.name}}` / `{{request}}` placeholder). Rendered in raw mode, SHA-256 hashed, and suppresses duplicates for **7 days** (`202 duplicate`). Empty/missing render disables dedup for that request.
- **`coalesceKey` & `coalesceWindowSeconds` (`webhookDedupe.ts:55–91`):** Matching requests within the window (**default 120 s**, range **10–3,600 s**) overwrite the pending request payload while retaining delivery IDs (`202 coalesced`).
- **Path filters (`scheduledTask.ts:119–156`, `webhookFilter.ts:10–49`):** Up to **16** filters, combined with **AND**. Paths begin with `body`, `headers`, or `query`. A `[]` segment fans out across array items. Operators: `equals`, `notEquals`, `startsWith`, `in` (non-empty list, no commas or outer whitespace), `exists`. Wildcard semantics: `equals`/`in`/`startsWith` pass if *any* scalar matches; `notEquals` passes only if *none* match; `exists` passes if *any* element is non-null. Non-object scalars coerce to string; objects pass only `exists`.
- **Prompt rendering & redaction (`webhookTemplate.ts:17–45,119–177`):** Placeholders: `{{body.path}}`, `{{headers.name}}`, `{{query.name}}`, `{{body}}`, `{{headers}}`, `{{query}}`, `{{request}}`. `Cookie` is stripped at the route (`webhookRoute.ts:86–90`). Whole `{{headers}}`, `{{query}}`, and `{{request}}` redact credential-like names; **named `{{headers.<name>}}` and `{{query.<name>}}` return raw unredacted values**, so never reference secret headers in prompt templates. Non-raw substitutions are framed as untrusted data with mention/command markers broken.

### 6.3 Limits, Status Codes, and Example `curl`

- **Limits (`webhookRoute.ts:20–21`, `ScheduledTaskService.ts:93–109,1887–2057`):** Max body **1 MiB** (checked via `Content-Length` and streaming reader); **60 req/min** per task; max **20** queued/pending/running deliveries per task; delivery logs retain **50** main + **20** filtered + **20** dedupe/rate-limit rows (filtered/dedupe logs capped at 10/min); logged body and rendered prompt truncate at **64 KiB**.
- **HTTP responses (all include header `x-t3-hook-outcome`, `webhookRoute.ts:26–35,57–81,124–185`):**
  - `202`: `accepted` (`{"deliveryId": "..."}`), `duplicate`, `coalesced`, `filtered`
  - `400`: `invalid` (workflow hooks)
  - `401`: `rejected_signature`
  - `403`: `blocked_by_policy`
  - `404`: `not_found` (unknown hook or bad URL token)
  - `405`: `method_not_allowed` (`HEAD`)
  - `409`: `disabled`
  - `410`: `expired` (older than `maxDeliveryAgeMinutes`)
  - `413`: `body_too_large`
  - `429`: `rate_limited` or `queue_full`
  - `500`: `error`
- **Schedule union (`scheduledTask.ts:21–74,193–215`):** `interval` (`everyMs >= 60000` on upsert), `fixed_time` (`timeOfDay` 24h, `weekdays` 0=Sun..6=Sat), `once` (ISO date-time with offset), `webhook`.
- **Difference from `v0.1.10`:** Core webhook route, HMAC, dedup, filters, and limits existed in `v0.1.10`. `v0.1.13` adds `wf-` workflow hook dispatch, `403 blocked_by_policy`, explicit `405` for `HEAD`, `Cookie` stripping, `webhookUrlToken.ts` redaction helper, and null-wildcard template handling.

```sh
# Placeholder-only example (configure task signature: header=x-ostrom-signature-256, prefix=sha256=, encoding=hex)
HOOK_URL="https://t3.example.invalid/api/hooks/HOOK_ID_PLACEHOLDER/URL_TOKEN_43_CHAR_PLACEHOLDER"
HMAC_SECRET="HMAC_SECRET_PLACEHOLDER"
BODY='{"event":"ticket.stuck","ticketId":42,"repo":"onsager-ai/example","summary":"Worker hit turn budget"}'
SIG="sha256=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$HMAC_SECRET" -hex | awk '{print $NF}')"

curl -i -X POST "$HOOK_URL" \
  -H "Content-Type: application/json" \
  -H "X-Ostrom-Signature-256: $SIG" \
  --data-binary "$BODY"
```

---

## 7. CLI (`t3`)

### 7.1 Command Tree & Key Flags (`apps/server/src/binCli.ts:13–121`, `docs/user/cli.md`)

- **Server & access:**
  - `t3 [cwd]` / `t3 start` (unlisted) / `t3 serve`: `--mode <web|desktop>`, `--port`, `--host`, `--base-dir`, `--dev-url`, `--no-browser`, `--bootstrap-fd`, `--auto-bootstrap-project-from-cwd` (root only), `--log-websocket-events` (`--log-ws-events`), `--tailscale-serve`, `--tailscale-serve-port`, `--lan` (`cli/config.ts:34–96`, `cli/server.ts:75–90`).
  - `t3 app [path]`; `t3 pair [--base-dir] [--scope ...] [--ttl] [--label] [--tailscale] [--tailscale-serve-port]` (`cli/pair.ts:505–538`).
  - `t3 up [--connect|--no-connect] [--lan|--no-lan] [--yes] [--headless]`; `t3 status [--json]`; `t3 connect [login|link [--publish-only]|status [--json]|publish [--disable]|unlink|logout] [--headless]` (`cli/up.ts:458–557`, `cli/connect.ts:615–818`).
- **Threads (`cli/thread.ts:838–1121`):**
  - `t3 thread list [--project <id|path>] [--json]` (`--project` defaults to `.`)
  - `t3 thread start <project> <message|-> [--title] [--generate-title] [--provider] [--model] [--option id=val ...] [--worktree <base>] [--branch <name>] [--output-schema <json>] [--wait] [--json]` (`--branch` needs `--worktree`; `--provider`+`--model` paired; `--option` needs both)
  - `t3 thread send <thread> <message|-> [--output-schema <json>] [--wait] [--json]` (queues next turn; never interrupts)
  - `t3 thread show <thread> [--messages N] [--json]` (`--messages` default 10)
- **Auth (`cli/auth.ts:63–385`):**
  - `t3 auth pairing create [--scope ...] [--ttl] [--label] [--base-url] [--json]` | `list [--json]` | `revoke <id>`
  - `t3 auth session issue [--scope ...] [--ttl] [--label] [--subject] [--token-only] [--json]` | `list [--json]` | `revoke <session-id>`
  - `t3 auth connect-device list [--json]` | `approve <code>` | `revoke <code>`
- **Environments & projects:**
  - `t3 environment link [url] [--url ...] [--pairing-code] [--from <file>] [--access <level>]` (default `approval-required`) | `list [--json]` | `unlink <environment-id>` (`cli/environment.ts:208–327`).
  - `t3 project add <path> [--title] [--track ...]` | `remove <project> [--force]` | `rename <project> <title>` (`cli/project.ts:370–566`).
- **Workflows (new in `v0.1.13`, `cli/workflow.ts:59–492`):** `t3 workflow start <wf> --input <json> --key <key>` | `list [--status] [--tag] [--subject]` | `show <run>` | `resume <run>` | `cancel <run> [--reason]` | `signal <val> [--run|--wait|--workflow|--channel|--id]` | `withdraw --run --wait [--reason]` | `questions [--kind]` | `hooks` | `store list|set|delete` (all accept `--json`).
- **Service, updates, feedback, maintenance:** `t3 service install|status|restart|uninstall|protect [--undo]`; `t3 update [version] [--channel nightly] [--allow-downgrade] [--yes]`; `t3 install` / `uninstall`; `t3 theme set|clear|show`; `t3 trace summary`; `t3 doctor [--dry-run] [--fix]`; `t3 policy check|show|settings|templates`; `t3 triage`; `t3 browser setup`; `t3 feedback [--kind --subject --expected --actual --workaround]` / `feedback list [--json]` *(new in v0.1.13)*. Internal helpers: `acp-mcp-bridge`, `acp-mcp-call`, `claude-history`, `speech-worker`, `ssh-helper`, `service-preflight`, `__service-launcher`.

### 7.2 Exit Codes & `--json` Output Shapes

- **Exit codes (`cli/thread.ts:120–140,721–768`, `binCli.ts:124–148`):**
  - `t3 thread start|send --wait`: **`0`** = completed; **`1`** = failed/cancelled/interrupted/rolled_back/error; **`2`** = thread waiting for user input (`ThreadNeedsInputError`). With `--json`, the structured JSON is printed to stdout *before* exiting non-zero.
  - Other commands (`workflow`, `policy`, etc.) return `0` on success and non-zero (`1`) on error.
- **Verified `--json` shapes:**
  - `t3 thread list --json`: `ThreadListItem[]` (`cli/thread.ts:908–910`).
  - `t3 thread start --json` (no `--wait`): `{threadId, projectId, runId}` (`cli/thread.ts:990–1000`).
  - `t3 thread start|send --wait --json`: `{threadId, runId, status, reply, error, output?}` (`cli/thread.ts:721–768`).
  - `t3 thread show --json`: `{thread, messages: [{role, runId, text}], error}` (`cli/thread.ts:1040–1088`).
  - `t3 status --json`: `{service: {supported, installed, installedVersion, otherBaseDir}, connect: null | {enabled, revoked}, lan: {enabled, inheritedDefault, hostOverride}}` (`cli/up.ts:510–529`).
  - `t3 auth pairing create --json`: `{id, credential, label?, scopes, expiresAt, pairUrl?}` (`pairUrl` uses `/pair#token=...`; `cliAuthFormat.ts:33–62`).
  - `t3 auth pairing list --json`: `[{id, label?, scopes, createdAt, expiresAt}]` (`cliAuthFormat.ts:75–92`).
  - `t3 auth session issue --json`: `{sessionId, token, method: "bearer-access-token", scopes, subject, client, expiresAt}` (`--token-only` prints raw token instead; `cliAuthFormat.ts:113–137`).
  - `t3 auth session list --json`: `[{sessionId, method, scopes, subject, client, connected, issuedAt, expiresAt, lastConnectedAt}]` (`cliAuthFormat.ts:152–173`).
  - `t3 environment list --json`: `PeerLinkSummary[]` (`cli/environment.ts:279–290`).
  - `t3 workflow ... --json`: `start`/`show` -> `{run, steps}`; `list` -> `WorkflowRun[]`; `questions` -> `{questions}`; `hooks` -> `{hooks}`; `signal` -> `{delivery}`; `withdraw` -> `{run, steps, withdrawal}` (`cli/workflow.ts:18–33,132–139`).

### 7.3 Local Authentication Mechanics

- **`t3 thread` (`cli/thread.ts:249–299`):** Reads the live server origin from `serverRuntimeStatePath` inside `T3CODE_HOME` (`~/.t3/userdata`), mints an ephemeral `full-access` `mcp-client` session (`label: "t3 thread cli"`, `peerLink: false`, `deviceType: "bot"`) directly via local `EnvironmentAuth` SQLite/signing access, calls `/mcp`, and revokes the session on exit. Fails if no server is running.
- **State-changing CLI commands (`cli/runningServer.ts:46–129`, new in `v0.1.13`):** `RunningServer.serverClient` mints a **2-minute** bearer session (`subject: "cli"`, requested scopes) against the running server and revokes on exit. If no server is running (or connection refused with lock free), `changeState` acquires `ServerOwnership` lock and mutates SQLite locally; unanswered timeouts fail without retrying locally.
- **Differences from `v0.1.10`:** `v0.1.13` adds `workflow`, `feedback`, `runningServer.ts`, `--headless` on `up`/`connect`, `connect publish`, and missing-tool handling in `thread.ts`. *(Full flag-by-flag diff on unchanged maintenance commands not exhaustively audited `[Guess]`.)*

---

## 8. Linked Environments (Peer Links)

Requires `Settings -> Labs -> Linked environments` (`linkedEnvironmentsEnabled`) on both peers; when disabled, peer endpoints return HTTP `403` with `{"error": "linked_environments_off"}` (`PeerLinks.ts:254–257`, `McpHttpServer.ts:272–278`, `linkThreadHttp.ts:42–60`).

### 8.1 Headless OAuth Pairing & Sliding Renewal

1. Linking host probes peer `GET /.well-known/t3/environment`, verifies identity & link-fencing capability (`PeerLinks.ts:503–559`).
2. Registers headless OAuth client on peer `/oauth/mcp/register` with `software_id: "t3code-peer-link"` and dummy redirect `http://127.0.0.1/t3code-peer-link` (never bound locally), generates PKCE S256 + state, posts pairing code to `/oauth/mcp/decision`, extracts `code` from the returned `{redirectTo}`, and exchanges at `/oauth/mcp/token` (`PeerLinks.ts:49–50,330–421`).
3. Stores access token in `ServerSecretStore` under a per-link secret name (DB row stores only metadata and secret reference; `PeerLinks.ts:259–263,560–609`).
4. **Sliding 30-day renewal (`v0.1.13`):** Peer extends session deadline back to 30 days on use when >= 1 day short (`PEER_LINK_RENEWAL_STEP = 1 day`), returning the updated timestamp in response header `T3-Link-Expires-At`, which `PeerMcpClient` records (`EnvironmentAuth.ts:86–96,1638–1659`, `PeerMcpClient.ts:129–145`).

### 8.2 Forwarding Headers, Idempotency, and `linkOrigin` Fencing

- **Protocol:** `PeerMcpClient` speaks MCP `2025-06-18` to the peer's `/mcp` (`PeerMcpClient.ts:24`).
- **Headers (`PeerMcpClient.ts:24–29,153–213`, `McpHttpServer.ts:216–269`):**
  - `T3-Mode-Limit: <runtimeMode>/<interactionMode>` — peer intersects downward with the link's approved access and managed ceiling (`McpInvocationContext.ts:61–118`). Malformed header returns HTTP `400 invalid_mode_limit`.
  - `T3-Sender-Thread: <environmentId>/<threadId>/<urlEncodedTitle>` — title truncated to **200 Unicode code points** before URL-encoding.
- **Idempotency namespacing (`PeerForwarding.ts:134–143`):** Forwarded `clientRequestId` values are hashed as `fwd:<sha256(namespace + "\n" + clientRequestId)>` so different local agents sharing one peer link cannot collide.
- **Fencing (`apps/server/src/mcp/linkOrigin.ts:12–76`, `PeerForwarding.ts:127–131`):**
  - Non-`full-access` links stamp every launched/derived thread with immutable `linkOrigin`. Such links can **read** normally, can **mutate only threads carrying their own `linkOrigin`**, cannot mutate projects/settings/scheduled tasks, skip project setup scripts and layout commands, and cannot chain through the peer's own links.
  - `full-access` links are unfenced (layout commands still require human approval text check in `TaskFolders.ts`).

### 8.3 Read-Only Thread Relay, Peer Client Pairing, and Limits

- **Read-only thread relay (`v0.1.13`, `packages/contracts/src/peerLink.ts:107–137`, `apps/server/src/peer/linkThreadHttp.ts:28–162`):**
  - `GET /api/links/thread/stream` (bounded snapshot + SSE), `GET /api/links/thread/history`, `GET /api/links/thread/item`.
  - Auth: peer-link MCP bearer (any access level), rejects `Origin`, requires Labs on. Streams re-verify revocation and Labs toggle every **30 s**.
- **Peer client pairing (`v0.1.13`, `environmentHttp.ts:1070`, `peer/http.ts:22–75`, `PeerClientPairing.ts:90–249`):**
  - `POST /api/peer-link/client-pairing` — requires a **`full-access`** peer-link MCP bearer, Labs on, managed ceiling allowing `full-access`, no `Origin`. Mints a 3-min client pairing credential or purpose-bound peer-link code.
- **Timeouts & limits (`PeerLinks.ts:49–58,276–290`):** Descriptor/handshake `probeTimeout` = **3 s** loopback, **10 s** remote (1 retry after 1 s on non-TLS errors); `LIST_PROBE_TIMEOUT` = **3 s**. No cross-peer MCP event forwarding (`MilestoneHub` is local only).
- **Differences from `v0.1.10`:** `v0.1.13` adds `PeerThreadRelay`, `PeerClientPairing`, `linkThreadHttp.ts`, `T3-Link-Expires-At` sliding expiry, remote revocation on `unlink`, `linkUnlessLinked`, stored link names, and cursor-fallback retry for older peers.

---

## 9. Ostrom Integration Playbooks (Exact Calls in Order)

All playbooks assume T3 Code `v0.1.13` and honor Design A (Managers stay T3 threads; Ostrom does not create foreground T3 threads).

### 9.1 Watch Runs

**Path A — Preferred (public contract, least privilege: read-only MCP + MCP Events):**
1. *(One-time owner setup)* Complete `/oauth/mcp/register` -> `/oauth/mcp/authorize` -> approve **`read-only`** -> `/oauth/mcp/token` to obtain a 30-day `mcp-client` Bearer token.
2. `POST /mcp` (`Authorization: Bearer <token>`, no `Origin`): `initialize` with `protocolVersion: "2026-07-28"` (for SEP-3415 events).
3. Initial state reconciliation: `tools/call` `t3_thread_list` `{ allProjects: true }` (or `{ projectId }`) and `t3_thread_read` `{ threadId, view: "messages", limit: 10 }` for each watched Manager thread.
4. Subscribe or poll `thread.run_settled` (`arguments: { threadIds: ["..."], includeSubagentChildren: false }`, `cursor: null`):
   - **Stream / Poll:** avoid exposing a callback server or touching `T3CODE_MCP_WEBHOOK_ALLOWED_HOSTS`.
   - **Webhook (optional):** call `events/subscribe` with `delivery: { mode: "webhook", url, secret: "whsec_..." }`, `ttlMs: 3600000`, echo `{ challenge }` on verification POST, verify `v1,` HMAC on every delivery, and refresh before `refreshBefore`.
5. On every `thread.run_settled` event (or whenever `truncated: true` / `Gap` occurs after a T3 restart), call `t3_thread_read` `{ threadId, view: "messages", limit: 5 }` to inspect authoritative `recentRuns` and latest assistant message.

**Path B — Internal alternative (authoritative live projection over `/ws`, higher churn risk):**
1. *(Owner item)* Mint an `orchestration:read` bearer token via `t3 auth session issue --scope orchestration:read --token-only`.
2. `GET /api/orchestration/shell` (`Authorization: Bearer <token>`, `x-t3-orchestration-protocol: 2`) -> save `snapshotSequence` and `projectIdentities`.
3. `POST /api/auth/websocket-ticket/rpc` (`Authorization: Bearer <token>`) -> `{ ticket }`.
4. Upgrade `GET /ws?orchestrationProtocol=2&wsTicket=<ticket>`.
5. Send `{"_tag":"Request","id":"1","tag":"orchestration.subscribeShell","payload":{"afterSequence":<snapshotSequence>,"knownProjectIdentities":"...","httpSnapshotFallback":true,"requestCompletionMarker":true},"headers":[]}`; reply `{"_tag":"Ack","requestId":"1"}` per chunk; ignore `{"_tag":"Pong"}` heartbeats; on `snapshot-required` repeat step 2.

### 9.2 React to `needs-input`

1. Over the same read-only MCP connection from §9.1, follow `thread.needs_input` and `thread.input_resolved` (filtered by `threadIds`).
2. When `thread.needs_input` arrives with `{ threadId, requestId, kind }`:
   - If `kind === "user_input"`: call `t3_pending_request` `{ threadId, action: "read", requestId }` to retrieve `{ requestId, questions }`.
   - If `kind === "approval"`: note that `t3_pending_request` **cannot** list or approve permission prompts (`t3_pending_request` excludes approvals). Surface the alert to the operator (or escalate via §9.3); answering an approval programmatically would require WS `orchestration.dispatchCommand` `type: "runtime-request.respond"` with `orchestration:operate`, which Ostrom should not hold by default.
3. If Ostrom itself is authorized to answer a `user_input` prompt (requires an `approval-required`+ MCP client matching the target thread's ceiling, not `read-only`): call `t3_pending_request` `{ threadId, action: "respond", requestId, answers: { "<questionId>": "<answer>" } }`.
4. Clear local waiting state upon `thread.input_resolved` (`resolution: "resolved"|"expired"|"cancelled"`).
5. *(Local CLI note)* If driving a host-local thread via `t3 thread send <id> <msg> --wait --json`, exit code `2` signals `needs-input` with `{ threadId, runId, status, ... }` on stdout.

### 9.3 Send an Alert to a Manager Thread

**Path A — Preferred (narrowest blast radius: HMAC-signed inbound webhook):**
1. *(One-time owner setup in T3)* Create a webhook-triggered scheduled task bound to the target Manager thread with:
   - `signature: { header: "x-ostrom-signature-256", encoding: "hex", prefix: "sha256=", secret: "<placeholder>" }`
   - `dedupe: { replayKey: "{{body.alertId}}", coalesceKey: "{{body.ticketKey}}", coalesceWindowSeconds: 120 }`
   - Prompt template using `{{body.summary}}`, `{{body.url}}`, etc. (never `{{headers.<secret>}}`).
2. `ostromd` computes `HMAC-SHA256(secret, rawBodyBytes)` and sends `POST /api/hooks/<hookId>/<urlToken>` with `X-Ostrom-Signature-256: sha256=<hex>`.
3. Check HTTP `202` and `x-t3-hook-outcome` (`accepted` / `coalesced` / `duplicate`); retry with backoff on `429` (`rate_limited` / `queue_full`) or `500`, and alert locally on `401`/`403`/`404`/`409`.

**Path B — Alternative (MCP `t3_thread_send`):**
1. Requires an MCP OAuth client approved at >= the target Manager thread's `runtimeMode` ceiling (e.g. `full-access` for a `full-access` Manager).
2. `POST /mcp` -> `tools/call` `t3_thread_send` `{ threadId, message, mode: "queue", clientRequestId: "ostrom-alert-<uuid>" }` -> `{ threadId, messageId, runId, status, delivery }`.

**Path C — Same-host CLI fallback (no OAuth, uses host OS user):**
1. `t3 thread send <threadId> - --json <<< "$ALERT_TEXT"` (always queues; never interrupts; requires `t3` CLI updated to match the `v0.1.13` server).

### 9.4 Read PR State

> **Important:** Do **not** call `pull_request_read`, `pull_request_list`, or `ci_logs` from an outside MCP client on `v0.1.13` — they are advertised in `tools/list` but always fail with `PullRequestThreadRequiredError` (§1.2).

- **Option 1 — Ostrom's own PR/CI loop (Recommended):** Keep reading GitHub directly via Ostrom's GitHub App installation token (`docs/running.md`). Zero dependency on T3's forge cache.
- **Option 2 — T3 thread-linked PR metadata & watch wakes over read-only MCP:**
  1. `POST /mcp` -> `tools/call` `list_thread_pull_requests` `{ threadId }` -> linked PR keys, `state`, `watching`, `headBranch`, `baseBranch`, `isDraft`, `stack`.
  2. Optionally call `read_pull_request_guide` `{ repository, number, host }` (or `{ url }`) to check T3's stored review guide and `currentHeadSha` / `stale`.
  3. Subscribe to MCP Event `pr.watch_wake` (`arguments: { threadIds: ["..."] }`) to receive `{ pullRequests, outcome, summary, watchEnded }` whenever T3's PR watcher wakes the thread.
- **Option 3 — Full T3-cached PR detail, checks, activity, and diff over WS/HTTP (`orchestration:read`):**
  1. Mint WS ticket (`POST /api/auth/websocket-ticket/rpc`) and open `/ws?orchestrationProtocol=2&wsTicket=...`.
  2. Call `pullRequests.detail`, `pullRequests.checks`, `pullRequests.activity`, or stream `pullRequests.subscribeRefreshes` (all `orchestration:read`, `rpc.ts:1285–1412`).
  3. For raw diff, call HTTP `POST /api/pull-requests/diff` (`Authorization: Bearer <token>`, `orchestration:read`, `environmentHttp.ts:835`).

### 9.5 Owner Items / Credentials Summary (If Built Post-Pilot)

| Integration path | Owner action / credential needed | Rotation / maintenance |
|---|---|---|
| Read-only `/mcp` + MCP Events (§9.1A, §9.2, §9.4 Opt 2) | Approve one read-only MCP OAuth client in Settings -> Sharing & access | Re-approve every **30 days** (no refresh token); refresh webhook leases <= 1 h (or use stream/poll) |
| Inbound webhook alert (§9.3A) | Create T3 webhook scheduled task; provision URL token + HMAC secret to `ostromd` | Rotate URL token or HMAC secret on demand; no 30-day expiry |
| Direct `/ws` + HTTP (`orchestration:read`, §9.1B, §9.4 Opt 3) | Mint `orchestration:read` token via `t3 auth session issue` | Rotate before custom TTL (no self-renewal); pin & test against `packages/contracts` on every `t3 update` |
| Host `t3` CLI (§7, §9.3C) | Upgrade the nvm-installed `t3` binary on `PATH` from `v0.1.10` to `v0.1.13` | No extra token on-host, but runs at `full-access` ceiling |
