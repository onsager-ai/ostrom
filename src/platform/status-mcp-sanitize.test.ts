import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { createStatusOAuth, mintStatusApproval } from "./status-mcp-oauth.ts";
import { startStatusMcp } from "./status-mcp.ts";

test("all four MCP results project only the allowed status fields", async () => {
  const root = mkdtempSync(join(tmpdir(), "ostrom-status-privacy-"));
  const socketPath = join(root, "platform.sock");
  const stateDir = join(root, "oauth");
  const publicUrl = "https://status.example.com";
  const host = {
    hostId: "hp-code-1", label: "Personal Workstation", originNamespace: "t3-private-origin",
    environmentId: "private_environment_9342", mcpUrl: "https://tail6262a7.ts.net/mcp",
    connectionState: "connected", tokenExpiresAt: "2030-12-31T00:00:00.000Z", lastError: "OAuth pairing device expired",
  };
  const wait = { threadId: "thread-private-AbCdEf", requestId: "request-private-123", questions: ["Raw question about the outage"] };
  const pr = { threadId: "private-pr-thread", repository: "onsager-ai/stealth-repo", lastWakeSummary: "Raw PR wait status" };
  const agent = { id: "agent-private-a", label: "Private Worker", externalRef: "private-agent-ref" };
  const decision = {
    id: "dec_123456abcdef", laneId: "lane_internal_id", status: "pending",
    authority: "owner", category: "release", urgency: "digest", title: "Approve the release",
    recommendation: "Proceed after review", options: ["Proceed", "Wait", "Show marvin the options", "Bearer alpha-private-token"],
    requestedBy: "marvin", decidedBy: "tikazyq", evidence: { path: "/home/marvin/private/secret",
      bearer: "debug-bearer-secret-184", account: "private-evidence-account" },
    createdAt: "2026-11-27T00:00:00.000Z",
  };
  const lane = {
    id: decision.laneId, slug: "releases", name: "Releases", status: "active", priorityRank: 1, ownerAgentId: agent.id,
  };
  const goal = {
    statement: "Ship three releases", metricName: "Releases", metricCurrent: 1, metricTarget: 3,
    metricUnit: "releases", weekStart: "2026-11-23", approvedDecisionId: "internal-approved-decision",
    weeklyPlan: { path: "/home/tikazyq/private-plan" },
  };
  const alerts = [
    { ruleCode: "pr_green_ci", summary: "Private alert body", detail: decision.evidence },
    { ruleCode: "private-custom-rule", summary: "More private alert text" },
  ];
  const health = {
    hosts: [host], agents: [agent], activeRuns: [{ externalRunRef: wait.threadId }], stalledRuns: [],
    pendingOwnerDecisions: [decision], waits: [wait], policyAlerts: alerts, githubPrs: [pr],
  };
  const digest = {
    markdown: "Raw digest including " + JSON.stringify({ host, wait, pr, agent, decision }),
    sections: { prs: [pr], decisions: [decision], lanes: [{ ...lane, goal }] },
  };
  const routes = new Map<string, unknown>([
    ["/v1/health/summary", health], ["/v1/spend/summary", { status: "available", debug: decision.evidence }],
    ["/v1/lanes", [lane]], ["/v1/decisions?status=pending", [
      decision, { ...decision, title: "Investigate hp-code-1 and /var/lib/private",
        recommendation: "Review https://tail6262a7.ts.net with tikazyq",
        options: ["Open onsager-ai/stealth-repo", "Read thread-private-AbCdEf", "Fetch /tmp/local.log",
          { label: "Unsafe option", metadata: decision.evidence }] },
    ]],
    ["/v1/digest?window=01:00Z", digest], ["/v1/digest?window=weekly", digest],
  ]);
  const calls: string[] = [];
  const upstream = createServer((request, response) => {
    calls.push(`${request.method} ${request.url}`);
    const body = routes.get(request.url ?? "");
    response.writeHead(request.method === "GET" && body ? 200 : 404).end(JSON.stringify(body));
  }).listen(socketPath);
  await once(upstream, "listening");

  const oauth = createStatusOAuth(publicUrl, stateDir);
  const redirectUri = "https://platform.openai.com/apps-manage/oauth";
  const registration = oauth.register({ redirect_uris: [redirectUri] });
  const clientId = registration.body.client_id;
  assert.equal(typeof clientId, "string");
  const verifier = "a".repeat(43);
  const params = {
    client_id: clientId!, redirect_uri: redirectUri, response_type: "code",
    code_challenge: createHash("sha256").update(verifier).digest("base64url"),
    code_challenge_method: "S256", state: "state", resource: `${publicUrl}/mcp`,
    approval_code: mintStatusApproval(stateDir),
  };
  const redirect = oauth.authorize(new URLSearchParams(params));
  assert.ok(redirect);
  const token = oauth.exchange(new URLSearchParams({
    ...params, grant_type: "authorization_code", code_verifier: verifier,
    code: new URL(redirect).searchParams.get("code") ?? "",
  }));
  const bearer = token.body.access_token;
  assert.equal(typeof bearer, "string");
  oauth.close();

  const server = await startStatusMcp({ publicUrl, socketPath, stateDir });
  try {
    const banned = [
      host.hostId, host.label, host.originNamespace, host.environmentId, host.mcpUrl, "tail6262a7.ts.net",
      host.tokenExpiresAt, host.lastError, wait.threadId, wait.requestId, wait.questions[0]!, pr.threadId,
      pr.repository, pr.lastWakeSummary, agent.id, agent.label, agent.externalRef, decision.laneId,
      decision.requestedBy, decision.decidedBy, ...Object.values(decision.evidence),
      goal.weeklyPlan.path, goal.approvedDecisionId, "Raw digest", "Private alert body", "private-custom-rule",
      "alpha-private-token", "/var/lib/private", "/tmp/local.log", bearer!,
      "hostId", "mcpUrl", "originNamespace", "environmentId", "requestedBy", "decidedBy", "weeklyPlan", "evidence",
    ];
    async function call(name: string, args = {}) {
      const response = await fetch(server.url, {
        method: "POST", headers: {
          "Content-Type": "application/json", Authorization: `Bearer ${bearer}`,
        },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: args } }),
      });
      assert.equal(response.status, 200);
      const serialized = await response.text();
      for (const secret of banned) assert.ok(!serialized.includes(secret), `${name} leaked ${secret}`);
      const { result } = JSON.parse(serialized);
      assert.equal(result.isError, undefined);
      return JSON.parse(result.content[0].text);
    }
    const summary = await call("status_summary");
    assert.deepEqual(summary, {
      pendingOwnerDecisions: 1, activeRuns: 1, stalledRuns: 0, openWaits: 1, openPolicyAlerts: 2,
      trackedAgents: 1, connectedHosts: 1, totalHosts: 1, spendStatus: "available", generatedAt: summary.generatedAt,
    });
    assert.ok(Date.parse(summary.generatedAt) <= Date.now());
    assert.deepEqual(await call("list_lanes"), [{
      slug: "releases", name: "Releases", status: "active", priorityRank: 1,
      goal: { statement: goal.statement, metricName: goal.metricName, metricCurrent: 1, metricTarget: 3,
        metricUnit: "releases", weekStart: goal.weekStart, approved: true },
    }]);
    const decisions = await call("list_open_decisions");
    assert.deepEqual(decisions[0], {
      id: decision.id, laneSlug: lane.slug, authority: "owner", category: "release", urgency: "digest",
      title: decision.title, recommendation: decision.recommendation, options: ["Proceed", "Wait", "[redacted]", "[redacted]"],
      createdAt: decision.createdAt,
    });
    assert.equal(decisions[1].options.length, 3);
    const latest = await call("read_latest_digest", { window: "weekly" });
    assert.equal(latest.window, "weekly");
    assert.match(latest.markdown, /Approve the release/);
    assert.match(latest.markdown, /Ship three releases/);
    assert.ok(latest.markdown.includes("pr\\_green\\_ci · 1"));
    assert.equal((await call("read_latest_digest")).window, "01:00Z");
    assert.deepEqual(new Set(calls), new Set([...routes.keys()].map((path) => `GET ${path}`)));
  } finally {
    await server.close();
    await new Promise<void>((resolve) => upstream.close(() => resolve()));
    rmSync(root, { recursive: true, force: true });
  }
});
