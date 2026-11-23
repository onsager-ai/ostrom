# Operator dashboard

Status: proposed, 2026-11-16. Milestone 3.

Build `ostrom dashboard`: one read-only local page, served by a short-lived CLI process that
proxies the daemon's Unix-socket API. Show attention items first, then active tickets, recent
failures and intake. Keep pause, resume, stop and extend in the existing CLI for this milestone.
Worker placement across machines is deferred; this page describes the one local daemon.

## The failure this fixes

During [the milestone-2 run](e2e-m2-hp2.md), an issue without an `@ostrom` mention went unseen for
20 minutes. The operator had to ask the PM whether work was done. Later, a Codex resume failed;
the PM had to read logs and query SQLite to explain the failed ticket. An approval the operator
thought they had submitted was still a draft. The page must make silence, failure and merge waits
visible without a PM interpreting the store. It cannot see an unsubmitted GitHub review.

## Browser access and trust

- `ostrom dashboard` binds an OS-assigned port on **127.0.0.1 only**, prints a local URL, and stays
  attached until interrupted. It uses the same socket selection as the CLI. No daemon listener,
  system service, remote binding or port-forwarding feature is added.
- A browser cannot speak HTTP over a Unix socket. The CLI serves the page and proxies only
  `GET /v1/status`, `/v1/tickets`, `/v1/tickets/:id` and the intake route below. It does not expose
  an arbitrary proxy, `/v1/preview` (which can perform GitHub reads), or any POST route.
- Prefer this to a loopback listener in `ostromd`: the daemon keeps its socket-only boundary and
  the browser bridge exists only when the operator asks. A daemon listener would still need the
  same browser protection and would expose a port for the daemon's entire lifetime.
- Loopback is reachable by other local users and processes, not just the operator. The socket's
  mode 0600 does not protect the bridge. Mint a random 256-bit token per CLI run; print it in the
  URL fragment, read it into page memory, and remove the fragment with `history.replaceState`.
  Require it in an `Authorization: Bearer` header on every proxied request. Do not persist it,
  put it in query strings, or log request headers. The static shell contains no operator data.
- Accept only the exact bound `Host`; reject a supplied `Origin` unless it matches the page's
  origin. Do not enable CORS. This blocks hostile web pages and DNS rebinding from using the
  bridge; the token protects reads from other local users who can discover the port.
- Return `Cache-Control: no-store` and `Referrer-Policy: no-referrer`; use no external assets.
  Render GitHub text and errors as text, and allow only HTTPS GitHub links. The token is a local
  capability, not protection from malicious code running as the operator or an administrator.
- Browser pause/resume/stop/extend requests always fail without reaching the socket. The page
  names the corresponding CLI command. Adding browser controls later needs a separate decision
  about authenticated POSTs and cross-origin requests; M3 introduces no mutation surface.

## What the operator sees

Use one card per active ticket: issue/PR number, the latest decision's summary, a plain-language
phase, numbered PR links, last completed turn, and small time/turn/token budget meters. Keep raw
state, branch, native session ID, worktree and timelines in a collapsed Details disclosure per
card. Show actionable attention items above the cards; keep Watchdog/machine status in a quiet
footer. Failures use a compact table; intake and recently finished tickets start collapsed.
Temperature needs attention at 75 °C or above.

| View | Existing source and smallest addition |
|---|---|
| Attention | `/v1/status.stuckTickets` with cause and detection time; paused tickets with `pauseCause`; active tickets at 80% of a budget; failed Watchdog reads; local heat at or above 75 °C. Show counts and links to the relevant ticket. |
| Active tickets | `/v1/tickets`: issue URL, state, harness/model/machine, `turnStartedAt`, `lastActivityAt`, session ID and budgets. Label last activity as **last completed turn**, with absolute time and age; null means no completed turn. Do not call it live session activity. |
| PR and merge wait | Add subscribed PR thread URLs and the latest assignment's stored `MergeCandidate` summary to each ticket: policy, `blockingReason`, `quietDeadline`. Show the stored reason verbatim, including `policy approvals are not satisfied` and `quiet period has not elapsed`; null means no recorded reason, not permission to merge. |
| Recent failures | Ticket detail already has all assignments: show `outcome=failed` with `lastError`/`outcomeAt`, and dispatch failures with `endReason`/`endedAt`. Add a compact latest-failure summary across assignments to list rows so failures remain visible after a ticket ends or another assignment starts. Keep the newest 20 failures visible. |
| Budget | Existing `TicketBudget`: wall time, idle time, turns, fresh input plus output tokens, and escalations. Add server-computed usage and warning fields using `budgetUsage`/`budgetWarning`, so the page uses the same clock and rules as the daemon. Warn at 80% of any positive limit; zero escalation allowance is disabled. Cached input is shown separately. |
| Watchdog and machine | `/v1/status` already has local temperature and slot counts. Add last Semon-read attempt time, success/failure, last success time and a concise error; also expose the configured tick interval. Keep this in daemon memory, reset to unknown on restart. No stall does not mean Semon is healthy. |
| Intake | Add `GET /v1/intake?limit=50`, newest first: event URL, observed time, disposition and reason. Join existing Router decisions (including `ignore`) to their triggers; these exist in SQLite but have no standalone operator API. Show undecided stored triggers as awaiting a decision. |

Ticket details retain the existing trigger, decision, assignment and outbox timeline. Link to the
issue and PR on GitHub. Show a copyable native session ID and `semon query read_transcript <id>`;
there is no assumed Semon web URL. Transcripts remain in Semon and daemon logs in journald.

Do not label the existing `tokensToday` counter as daily spend: it sums whole assignments whose
last activity is today. M3 shows per-ticket token use; cost estimates are unavailable. Slot totals
currently mean CPU parallelism, not configured Worker capacity; label them accordingly. Unknown
temperature is **unavailable**, never healthy. Remote machine health waits for placement work.

### Intake that is missing today

`issueOpenedEvent` discards unmentioned issues before creating a trigger. Own replies and
unauthorized new work are filtered with per-pass counts only. There is no stored per-event reason
for those exclusions, and no history to recover for the milestone-2 incident.

To prevent the same silent wait, add a small intake-receipt table keyed by source and source event
ID for newly opened issues excluded for a missing mention or an unauthorized author. Store only
the URL, first observation time and fixed reason; no body or transcript. Re-reading an event must
not duplicate or reset it. The new intake route combines these receipts with stored triggers and
decisions. Do not add receipts for Ostrom's own replies or every filtered comment. The page states
that coverage explicitly and says intake is observed through polling, not a complete GitHub audit.

For an approvals wait, link directly to GitHub reviews and say: **Only submitted reviews count;
check GitHub for an unsubmitted draft.** Pending reviews are invisible to the daemon. Never claim
that a particular person has a draft or that an approval has been submitted without evidence.

## Refresh and build

Serve one static HTML page with plain JavaScript and CSS from the CLI; no frontend framework,
asset CDN or frontend build step. Reuse the CLI socket client and the operator API types. Add only
the projections and intake receipts above; the browser never opens SQLite or queries GitHub/Semon.

Fetch status, ticket summaries and intake immediately, then every **5 seconds**; fetch every active
ticket's detail on the same cycle. Do not overlap cycles. Show last successful refresh and daemon
observation times: a fresh page fetch does not imply a fresh GitHub or Semon poll. After a failed
cycle, keep the last snapshot marked **stale** with the error until a full refresh succeeds. If the
daemon is down, show that fact; the dashboard does not use the CLI's direct-store fallback.

CI runs socket/bridge integration tests for route allowlisting, loopback binding, token failures,
Host/Origin rejection, no forwarded POST and daemon-down errors. API/store fixtures cover merge
reasons, failure history, intake deduplication, Watchdog failure/restart and existing budget rules.
CI browser tests cover refresh/stale recovery, links and text escaping, and the attention list.
No tests, builds or typechecks run locally.

## Live acceptance

Record `docs/e2e-m3.md` with revision, host, daemon interval, UTC times, GitHub URLs, ticket and
session IDs, and screenshots. Run against a fresh sandbox store with the operator's authorization.

1. Open the dashboard while the daemon runs. An unmentioned issue appears as excluded with the
   missing-mention reason within one successful daemon poll plus 5 seconds; a mentioned issue
   becomes a ticket with its issue/PR links, session ID and last completed turn. No SQL is needed.
2. Exercise an approvals wait and the quiet window. The page shows each stored reason and the
   review-submission reminder; a submitted approval then permits the normal merge and ended state.
3. Cause a controlled dispatch or resume failure. Its actual stored reason remains visible after
   the ticket ends. Pause/resume through the CLI and verify the page follows the same session.
4. Use a small ticket budget to show the 80% warning and budget pause. Extend through the CLI;
   the next refresh shows the new limits and resumed state. Exercise a stall attention item too.
5. Cause a controlled Semon-read failure and recovery. Show last attempt, error and last success;
   restarting the daemon resets health to unknown until a read completes. Stop the daemon and
   verify the page marks its snapshot stale, then recovers after restart.
6. Verify the bridge binds only loopback, a client without the token cannot read data, and browser
   controls cannot mutate a ticket. Closing the CLI removes the listener. Record these results.

## Operator questions

None blocks this proposal. The proposed M3 is read-only; browser controls, cost estimates and
multi-machine status remain outside its acceptance criteria.
