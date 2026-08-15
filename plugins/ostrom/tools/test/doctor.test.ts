import { execFileSync, spawnSync } from "node:child_process";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { checkPluginCacheDrift } from "../src/checks/plugin-cache-drift.js";
import { checkPlugin } from "../src/checks/plugin.js";
import type { DoctorContext } from "../src/lib/context.js";
import { parseOstromYaml } from "../src/lib/config.js";
import { runDoctor } from "../src/lib/doctor.js";
import { formatResult, type CheckResult } from "../src/lib/result.js";

const pluginRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const pluginVersion = JSON.parse(
  readFileSync(join(pluginRoot, ".claude-plugin", "plugin.json"), "utf8"),
).version as string;
const hook = join(pluginRoot, "hooks", "inject-constitution.sh");
const frozenRules = join(pluginRoot, "rules", "frozen-rules.md");
const roots: string[] = [];

interface Fixture {
  root: string;
  home: string;
  configDir: string;
  cwd: string;
  installPath: string;
}

function command(commandName: string, args: string[], cwd?: string): string {
  return execFileSync(commandName, args, {
    cwd,
    encoding: "utf8",
    env: {
      ...process.env,
      GIT_AUTHOR_NAME: "Ostrom Test",
      GIT_AUTHOR_EMAIL: "ostrom@example.test",
      GIT_COMMITTER_NAME: "Ostrom Test",
      GIT_COMMITTER_EMAIL: "ostrom@example.test",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
}

function initRepo(path: string, filename: string, contents: string): void {
  mkdirSync(path, { recursive: true });
  command("git", ["init", "-b", "main"], path);
  writeFileSync(join(path, filename), contents);
  command("git", ["add", filename], path);
  command("git", ["commit", "-m", "fixture"], path);
}

function wireMarketplace(fixture: Fixture): void {
  const origin = join(fixture.root, "marketplace-origin");
  initRepo(origin, "README.md", "marketplace\n");
  mkdirSync(
    join(origin, "plugins", "ostrom", ".claude-plugin"),
    { recursive: true },
  );
  mkdirSync(join(origin, "plugins", "ostrom", "skills", "fixture"), {
    recursive: true,
  });
  writeFileSync(
    join(origin, "plugins", "ostrom", ".claude-plugin", "plugin.json"),
    JSON.stringify({ name: "ostrom", version: pluginVersion }),
  );
  writeFileSync(
    join(origin, "plugins", "ostrom", "skills", "fixture", "SKILL.md"),
    "fixture protocol\n",
  );
  command("git", ["add", "."], origin);
  command("git", ["commit", "-m", "add plugin fixture"], origin);
  const marketplaceParent = join(
    fixture.configDir,
    "plugins",
    "marketplaces",
  );
  mkdirSync(marketplaceParent, { recursive: true });
  command("git", ["clone", origin, join(marketplaceParent, "ostrom")]);
  writeFileSync(
    join(fixture.configDir, "plugins", "known_marketplaces.json"),
    '{"ostrom":{"source":"onsager-ai/ostrom"}}\n',
  );
}

function baseFixture(): Fixture {
  const root = mkdtempSync(join(tmpdir(), "ostrom-doctor-"));
  roots.push(root);
  const home = join(root, "home");
  const configDir = join(root, "claude");
  const cwd = join(root, "project");
  const installPath = join(root, "installed-ostrom");
  mkdirSync(join(configDir, "plugins"), { recursive: true });
  mkdirSync(cwd, { recursive: true });
  mkdirSync(join(installPath, ".claude-plugin"), { recursive: true });
  mkdirSync(join(installPath, "skills", "fixture"), { recursive: true });
  writeFileSync(
    join(installPath, ".claude-plugin", "plugin.json"),
    JSON.stringify({ name: "ostrom", version: pluginVersion }),
  );
  writeFileSync(
    join(installPath, "skills", "fixture", "SKILL.md"),
    "fixture protocol\n",
  );
  writeFileSync(
    join(configDir, "plugins", "installed_plugins.json"),
    JSON.stringify({
      plugins: {
        "ostrom@ostrom": [
          { installPath, version: pluginVersion },
        ],
      },
    }),
  );
  mkdirSync(home, { recursive: true });
  mkdirSync(join(configDir, "ostrom"), { recursive: true });
  const sourceRoot = join(root, "source-root");
  const sourceRepository = join(
    sourceRoot,
    "example-org",
    "example-repo",
  );
  initRepo(sourceRepository, "README.md", "source checkout\n");
  command(
    "git",
    [
      "remote",
      "add",
      "origin",
      "https://github.com/example-org/example-repo.git",
    ],
    sourceRepository,
  );
  writeFileSync(
    join(configDir, "ostrom", "mandates.yaml"),
    `search_roots:\n  - ${sourceRoot}\n`,
  );
  writeFileSync(
    join(configDir, "ostrom", "sprint.jsonl"),
    '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake1","outcome":"completed"},"narration":{}}\n' +
      '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake1","outcome":"completed"},"narration":{}}\n',
  );
  mkdirSync(join(configDir, "ostrom", "publish"), { recursive: true });
  writeFileSync(
    join(configDir, "ostrom", "publish", "manifest.json"),
    '{"published_at":"2026-08-01T00:00:00Z","expected_sweep_interval_hours":24}\n',
  );
  const fixture = { root, home, configDir, cwd, installPath };
  wireMarketplace(fixture);
  return fixture;
}

function doctorContext(
  fixture: Fixture,
  loadedPluginRoot = pluginRoot,
): DoctorContext {
  return {
    pluginRoot: loadedPluginRoot,
    configDir: fixture.configDir,
    cwd: fixture.cwd,
    home: fixture.home,
    env: process.env,
    resolveConfig: () => ({
      provider: "file",
      path: "~/.claude/ostrom/touch-log.md",
      autoCommit: "False",
    }),
    readTrace: () => ({ exists: false }),
  };
}

function pluginResult(
  fixture: Fixture,
  loadedPluginRoot = pluginRoot,
): CheckResult {
  return checkPlugin(doctorContext(fixture, loadedPluginRoot));
}

function writeRegistry(fixture: Fixture, version?: string): void {
  writeFileSync(
    join(fixture.configDir, "plugins", "installed_plugins.json"),
    JSON.stringify({
      plugins: {
        "ostrom@ostrom": [
          { installPath: fixture.installPath, ...(version ? { version } : {}) },
        ],
      },
    }),
  );
}

function run(fixture: Fixture, env: NodeJS.ProcessEnv = {}): string {
  return runDoctor({
    pluginRoot,
    configDir: fixture.configDir,
    cwd: fixture.cwd,
    home: fixture.home,
    env: { ...process.env, MANDATE_NOW_EPOCH: "1785542400", ...env },
  });
}

function output(results: CheckResult[]): string {
  return `${results.map(formatResult).join("\n")}\n`;
}

function commonExpected(
  rulesDetail: string,
  touch: CheckResult,
  provider: CheckResult,
): string {
  return output([
    {
      status: "OK",
      name: "plugin",
      detail: `installed, loaded version ${pluginVersion}`,
      remedy: "",
    },
    {
      status: "OK",
      name: "marketplace",
      detail: "cached clone can fast-forward to origin/main",
      remedy: "",
    },
    {
      status: "OK",
      name: "plugin-cache-drift",
      detail: `version ${pluginVersion} and shipped files agree with the marketplace checkout`,
      remedy: "",
    },
    { status: "OK", name: "rules-layers", detail: rulesDetail, remedy: "" },
    touch,
    provider,
    {
      status: "OK",
      name: "dispatch-source-roots",
      detail: "1 search root configured for dispatch",
      remedy: "",
    },
    {
      status: "OK",
      name: "trace-lease",
      detail: "trace current, last 2026-08-01T00:00:00Z; lease idle",
      remedy: "",
    },
    {
      status: "OK",
      name: "work-orders",
      detail: "no work orders in flight",
      remedy: "",
    },
    {
      status: "OK",
      name: "builder-pass",
      detail:
        "builder pass current, last 2026-08-01T00:00:00Z (age 0m; 3h cadence)",
      remedy: "",
    },
    {
      status: "OK",
      name: "gatekeeper-pass",
      detail:
        "gatekeeper pass current, last 2026-08-01T00:00:00Z (age 0m; 1h cadence)",
      remedy: "",
    },
    {
      status: "OK",
      name: "publish",
      detail: "publish current, last 2026-08-01T00:00:00Z (24h cadence)",
      remedy: "",
    },
    { status: "OK", name: "environment", detail: "local", remedy: "" },
    {
      status: "OK",
      name: "config-parser",
      detail:
        "used the built-in ostrom-shape parser (top-level scalars, one level of nesting, inline lists, and comments; the values behind touch-durability/provider-reachable are authoritative for this supported config shape; a DEFER line is still resolved by the caller)",
      remedy: "",
    },
  ]);
}

describe("plugin check", () => {
  it("reports OK when the loaded and registry versions agree", () => {
    const fixture = baseFixture();

    expect(pluginResult(fixture)).toEqual({
      status: "OK",
      name: "plugin",
      detail: `installed, loaded version ${pluginVersion}`,
      remedy: "",
    });
  });

  it("warns and names both versions when the loaded and registry versions disagree", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.installPath, ".claude-plugin", "plugin.json"),
      '{"name":"ostrom","version":"1.1.0"}\n',
    );
    writeRegistry(fixture, "1.1.0");

    expect(pluginResult(fixture)).toEqual({
      status: "WARN",
      name: "plugin",
      detail: `installed, loaded version ${pluginVersion}, registry version 1.1.0`,
      remedy:
        "restart the session to reconcile the loaded plugin with the registry",
    });
  });

  it("falls back to the registry when the loaded plugin.json is unreadable", () => {
    const fixture = baseFixture();

    expect(pluginResult(fixture, join(fixture.root, "missing-plugin"))).toEqual({
      status: "OK",
      name: "plugin",
      detail: `installed, version ${pluginVersion} (loaded plugin.json not readable, using registry version)`,
      remedy: "",
    });
  });

  it("fails unchanged when installed_plugins.json is absent", () => {
    const fixture = baseFixture();
    const installedJson = join(
      fixture.configDir,
      "plugins",
      "installed_plugins.json",
    );
    rmSync(installedJson);

    expect(pluginResult(fixture)).toEqual({
      status: "FAIL",
      name: "plugin",
      detail: `no installed_plugins.json at ${installedJson}`,
      remedy: "/plugin install ostrom@ostrom",
    });
  });

  it("fails unchanged when the ostrom registry entry is absent", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "plugins", "installed_plugins.json"),
      '{"plugins":{}}\n',
    );

    expect(pluginResult(fixture)).toEqual({
      status: "FAIL",
      name: "plugin",
      detail: "ostrom@ostrom not present in installed_plugins.json",
      remedy: "/plugin install ostrom@ostrom",
    });
  });

  it("fails unchanged when no version can be determined", () => {
    const fixture = baseFixture();
    rmSync(join(fixture.installPath, ".claude-plugin", "plugin.json"));
    writeRegistry(fixture);

    expect(pluginResult(fixture, join(fixture.root, "missing-plugin"))).toEqual({
      status: "FAIL",
      name: "plugin",
      detail: "ostrom@ostrom entry found but no version could be determined",
      remedy: "/plugin install ostrom@ostrom",
    });
  });
});

describe("plugin cache drift check", () => {
  it("reports OK when same-version shipped files agree", () => {
    const fixture = baseFixture();

    expect(checkPluginCacheDrift(doctorContext(fixture))).toEqual({
      status: "OK",
      name: "plugin-cache-drift",
      detail: `version ${pluginVersion} and shipped files agree with the marketplace checkout`,
      remedy: "",
    });
  });

  it("fails and names a changed file when same-version contents drift", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.installPath, "skills", "fixture", "SKILL.md"),
      "cached protocol\n",
    );

    expect(checkPluginCacheDrift(doctorContext(fixture))).toEqual({
      status: "FAIL",
      name: "plugin-cache-drift",
      detail: `version ${pluginVersion} agrees but shipped files drift: content differs: skills/fixture/SKILL.md`,
      remedy: "update and reinstall ostrom@ostrom, then restart the session",
    });
  });

  it("warns when the marketplace clone cannot be fetched", () => {
    const fixture = baseFixture();
    const marketplace = join(
      fixture.configDir,
      "plugins",
      "marketplaces",
      "ostrom",
    );
    command(
      "git",
      ["remote", "set-url", "origin", join(fixture.root, "missing-origin")],
      marketplace,
    );

    const result = checkPluginCacheDrift(doctorContext(fixture));
    expect(result.status).toBe("WARN");
    expect(result.name).toBe("plugin-cache-drift");
    expect(result.detail).toContain("git fetch failed (offline?)");
  });

  it("warns when the marketplace clone is missing", () => {
    const fixture = baseFixture();
    rmSync(
      join(fixture.configDir, "plugins", "marketplaces", "ostrom"),
      { recursive: true },
    );

    expect(checkPluginCacheDrift(doctorContext(fixture))).toEqual({
      status: "WARN",
      name: "plugin-cache-drift",
      detail: `cannot compare shipped files: registered, but no cached clone at ${join(fixture.configDir, "plugins", "marketplaces", "ostrom")}`,
      remedy: "/plugin marketplace add onsager-ai/ostrom",
    });
  });
});

function notionConfig(fixture: Fixture, extra = ""): void {
  const configRepo = join(fixture.root, "private-config");
  initRepo(
    configRepo,
    "config.yaml",
    `provider: notion\nnotion:\n  data_source: collection://fixture\n${extra}`,
  );
  mkdirSync(join(fixture.configDir, "ostrom"), { recursive: true });
  symlinkSync(
    join(configRepo, "config.yaml"),
    join(fixture.configDir, "ostrom", "config.yaml"),
  );
}

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true });
});

describe("doctor golden output", () => {
  it("matches the fully-wired machine golden", () => {
    const fixture = baseFixture();
    notionConfig(fixture);
    mkdirSync(join(fixture.configDir, "ostrom"), { recursive: true });
    writeFileSync(
      join(fixture.configDir, "ostrom", "rules.md"),
      "<!-- seeded but intentionally empty -->\n",
    );

    expect(run(fixture)).toBe(
      commonExpected(
        "shipped only (user layer present but carries no rules yet (by design))",
        {
          status: "OK",
          name: "touch-durability",
          detail:
            "target: provider notion (target is inherently shared) -- config: config.yaml is a symlink into a git repo (versioned, syncs across machines)",
          remedy: "",
        },
        {
          status: "DEFER",
          name: "provider-reachable",
          detail:
            "notion: MCP availability is a session property, not visible to a shell",
          remedy: "",
        },
      ),
    );
  });

  it("faults when search_roots is empty because dispatch cannot run", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "mandates.yaml"),
      "search_roots: []\n",
    );

    expect(run(fixture)).toContain(
      "FAIL|dispatch-source-roots|search_roots is empty; dispatch cannot resolve source repositories|configure search_roots with a parent directory containing the roster checkouts\n",
    );
  });

  it("treats a comment-only user rules layer as correct by design", () => {
    const fixture = baseFixture();
    mkdirSync(join(fixture.configDir, "ostrom"), { recursive: true });
    writeFileSync(
      join(fixture.configDir, "ostrom", "rules.md"),
      "<!--\nprivate rules will go here\n-->\n",
    );

    expect(run(fixture)).toContain(
      "OK|rules-layers|shipped only (user layer present but carries no rules yet (by design))|\n",
    );
  });

  it("warns for both an unversioned file target and a plain user config", () => {
    const fixture = baseFixture();
    const logDir = join(fixture.root, "unversioned");
    const logPath = join(logDir, "touch-log.md");
    mkdirSync(logDir);
    mkdirSync(join(fixture.configDir, "ostrom"), { recursive: true });
    writeFileSync(
      join(fixture.configDir, "ostrom", "config.yaml"),
      `provider: file\nfile:\n  path: ${logPath}\n  auto_commit: false\n`,
    );

    const line = run(fixture)
      .split("\n")
      .find((candidate) => candidate.includes("|touch-durability|"));
    expect(line).toBe(
      `WARN|touch-durability|target: file provider, ${logPath} is NOT inside a git repo — touches logged here never reach another machine -- config: config.yaml is a plain machine-local file (will not sync across machines)|point file.path into a synced repo and set auto_commit: true, or switch provider; version it: move it into a private config repo and symlink it back to ${join(fixture.configDir, "ostrom", "config.yaml")}`,
    );
  });

  it("preserves PyYAML-style boolean detail for a versioned file target", () => {
    const fixture = baseFixture();
    const logRepo = join(fixture.root, "touch-log-repo");
    initRepo(logRepo, ".gitkeep", "");
    const logPath = join(logRepo, "touch-log.md");
    mkdirSync(join(fixture.configDir, "ostrom"), { recursive: true });
    writeFileSync(
      join(fixture.configDir, "ostrom", "config.yaml"),
      `provider: file\nfile:\n  path: ${logPath}\n  auto_commit: true\n`,
    );

    expect(run(fixture)).toContain(
      `target: file provider, ${logPath} is inside a git repo (auto_commit=True)`,
    );
  });

  it("supports a nonexistent CLAUDE_CONFIG_DIR without creating it", () => {
    const fixture = baseFixture();
    const missing = join(fixture.root, "does-not-exist");
    const report = runDoctor({
      pluginRoot,
      configDir: missing,
      cwd: fixture.cwd,
      home: fixture.home,
      env: {
        HOME: fixture.home,
        CLAUDE_CONFIG_DIR: missing,
      },
    });

    expect(report.split("\n").filter(Boolean)).toHaveLength(13);
    expect(existsSync(missing)).toBe(false);
  });

  it("warns when the sprint trace is absent", () => {
    const fixture = baseFixture();
    rmSync(join(fixture.configDir, "ostrom", "sprint.jsonl"));

    expect(run(fixture)).toContain(
      "WARN|trace-lease|trace absent; lease idle|run /ostrom:gatekeep and confirm it creates sprint.jsonl\n",
    );
  });

  it("reports missing, stale, and current publish state from the manifest cadence", () => {
    const fixture = baseFixture();
    const manifestPath = join(
      fixture.configDir,
      "ostrom",
      "publish",
      "manifest.json",
    );

    rmSync(manifestPath);
    expect(run(fixture)).toContain(
      "WARN|publish|no publish has been recorded|run mandate publish.sh and confirm the state branch is reachable\n",
    );

    writeFileSync(
      manifestPath,
      '{"published_at":"2026-07-31T11:00:00Z","expected_sweep_interval_hours":12}\n',
    );
    expect(run(fixture)).toContain(
      "WARN|publish|publish stale, last 2026-07-31T11:00:00Z (older than 12h cadence)|run mandate publish.sh and confirm the state branch is reachable\n",
    );

    writeFileSync(
      manifestPath,
      '{"published_at":"2026-07-31T13:00:00Z","expected_sweep_interval_hours":12}\n',
    );
    expect(run(fixture)).toContain(
      "OK|publish|publish current, last 2026-07-31T13:00:00Z (12h cadence)|\n",
    );
  });

  it("warns when the sprint trace is stale", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-07-30T00:00:00Z","kind":"pass-ended","fact":{},"narration":{}}\n',
    );

    expect(run(fixture)).toContain(
      "WARN|trace-lease|trace stale, last 2026-07-30T00:00:00Z (older than 24h); lease idle|run /ostrom:gatekeep and confirm the recurring loop is active\n",
    );
  });

  it("reports an active work order in flight", () => {
    const fixture = baseFixture();
    const systemctl = join(fixture.root, "systemctl-active");
    writeFileSync(systemctl, "#!/bin/sh\nprintf 'active\\n'\n");
    command("chmod", ["+x", systemctl]);
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake1","outcome":"completed"},"narration":{}}\n' +
        '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake1","outcome":"completed"},"narration":{}}\n' +
        '{"ts":"2026-08-01T00:01:00Z","kind":"work-dispatched","fact":{"schema_version":1,"item_id":"example-org/example-repo#123","order_id":"order-placeholder","unit_name":"ostrom-implementer-0123456789abcdef","backend":"systemd","cost_ceiling_usd":20,"token_ceiling":500000,"cost_usd":null,"duration_seconds":0},"narration":{}}\n',
    );

    expect(run(fixture, { MANDATE_SYSTEMCTL_BIN: systemctl })).toContain(
      "OK|work-orders|1 in flight: example-org/example-repo#123 (ostrom-implementer-0123456789abcdef)|\n",
    );
  });

  it("faults when a dispatched unit exits without a terminal row", () => {
    const fixture = baseFixture();
    const systemctl = join(fixture.root, "systemctl-exited");
    writeFileSync(systemctl, "#!/bin/sh\nexit 4\n");
    command("chmod", ["+x", systemctl]);
    const tracePath = join(fixture.configDir, "ostrom", "sprint.jsonl");
    const dispatch =
      '{"ts":"2026-08-01T00:01:00Z","kind":"work-dispatched","fact":{"schema_version":1,"item_id":"example-org/example-repo#123","order_id":"order-placeholder","unit_name":"ostrom-implementer-0123456789abcdef","backend":"systemd","cost_ceiling_usd":20,"token_ceiling":500000,"cost_usd":null,"duration_seconds":0},"narration":{}}\n';
    writeFileSync(tracePath, readFileSync(tracePath, "utf8") + dispatch);

    expect(run(fixture, { MANDATE_SYSTEMCTL_BIN: systemctl })).toContain(
      "FAIL|work-orders|1 in flight; unit exited without terminal row: example-org/example-repo#123 (ostrom-implementer-0123456789abcdef)|inspect the transient unit journal and append work-failed before clearing its per-item lease\n",
    );

    writeFileSync(
      tracePath,
      readFileSync(tracePath, "utf8") +
        '{"ts":"2026-08-01T00:02:00Z","kind":"work-failed","fact":{"schema_version":1,"item_id":"example-org/example-repo#123","order_id":"order-placeholder","unit_name":"ostrom-implementer-0123456789abcdef","backend":"systemd","cost_ceiling_usd":20,"token_ceiling":500000,"cost_usd":null,"duration_seconds":60,"pr_url":null,"reason":"signal-TERM"},"narration":{}}\n',
    );
    expect(run(fixture, { MANDATE_SYSTEMCTL_BIN: systemctl })).toContain(
      "OK|work-orders|no work orders in flight|\n",
    );
  });

  it("distinguishes no recorded builder pass from stale builder history", () => {
    const fixture = baseFixture();
    const tracePath = join(fixture.configDir, "ostrom", "sprint.jsonl");
    writeFileSync(
      tracePath,
      '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake1"},"narration":{}}\n',
    );

    expect(run(fixture)).toContain(
      "WARN|builder-pass|no builder pass ever recorded|run /ostrom:work and confirm it records pass-ended\n",
    );

    writeFileSync(
      tracePath,
      '{"ts":"2026-07-30T00:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake1"},"narration":{}}\n' +
        '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake1"},"narration":{}}\n',
    );

    expect(run(fixture)).toContain(
      "WARN|builder-pass|builder pass stale, last 2026-07-30T00:00:00Z (age 48h; older than 3h cadence)|confirm ostrom-builder-pass.timer is active and loop-armed is present\n",
    );
  });

  it("uses the delivery timer cadence rather than mandate sweep cadence", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "mandates.yaml"),
      "cadence_hours: 12\n",
    );
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-07-31T11:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake1"},"narration":{}}\n',
    );

    expect(run(fixture)).toContain(
      "WARN|builder-pass|builder pass stale, last 2026-07-31T11:00:00Z (age 13h; older than 3h cadence)|confirm ostrom-builder-pass.timer is active and loop-armed is present\n",
    );
  });

  it("reports gatekeeper pass age against the hourly timer cadence", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake1"},"narration":{}}\n' +
        '{"ts":"2026-07-31T22:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake1"},"narration":{}}\n',
    );

    expect(run(fixture)).toContain(
      "WARN|gatekeeper-pass|gatekeeper pass stale, last 2026-07-31T22:00:00Z (age 2h; older than 1h cadence)|confirm ostrom-gatekeeper-pass.timer is active and loop-armed is present\n",
    );
  });

  it("stays quiet for a single isolated no-op pass", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake1","outcome":"no-op","reason":"blocked"}}\n' +
        '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake1","outcome":"completed"}}\n',
    );

    // One contended lease reads as nominal: same "current" wording as any
    // other pass-ended, with no mention of the no-op it happened to be.
    expect(run(fixture)).toContain(
      "OK|builder-pass|builder pass current, last 2026-08-01T00:00:00Z (age 0m; 3h cadence)|\n",
    );
  });

  it("reports a fault after three consecutive no-op passes for a role", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-07-31T22:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake1","outcome":"no-op","reason":"blocked"}}\n' +
        '{"ts":"2026-07-31T23:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake2","outcome":"no-op","reason":"blocked"}}\n' +
        '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake3","outcome":"no-op","reason":"blocked"}}\n' +
        '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake1","outcome":"completed"}}\n',
    );

    // Three in a row is a fault even though the last one is well inside the
    // 3h cadence -- the loop is running on schedule and producing nothing,
    // which the age/cadence check alone cannot see.
    expect(run(fixture)).toContain(
      "FAIL|builder-pass|builder loop has produced 3 consecutive no-op passes, last 2026-08-01T00:00:00Z (age 0m)|inspect pass-runs/builder transcripts; the loop is running but the protocol never takes ownership\n",
    );
  });

  it("reports a fault after three consecutive failed passes for a role", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-07-31T22:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake1","outcome":"failed"}}\n' +
        '{"ts":"2026-07-31T23:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake2","outcome":"failed"}}\n' +
        '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"gatekeeper-fixture-wake3","outcome":"failed"}}\n',
    );

    // Fresh timestamps prove the timer is firing, but three protocol-owned
    // failures are no healthier than three passes that never took ownership.
    expect(run(fixture)).toContain(
      "FAIL|gatekeeper-pass|gatekeeper loop has produced 3 consecutive failed passes, last 2026-08-01T00:00:00Z (age 0m)|inspect pass-runs/gatekeeper transcripts; the protocol takes ownership but does not complete\n",
    );
  });

  it("clears the no-op fault the moment a pass completes again", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-07-31T21:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake1","outcome":"no-op","reason":"blocked"}}\n' +
        '{"ts":"2026-07-31T22:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake2","outcome":"no-op","reason":"blocked"}}\n' +
        '{"ts":"2026-07-31T23:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake3","outcome":"no-op","reason":"blocked"}}\n' +
        '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake4","outcome":"completed"}}\n',
    );

    // The streak the fault check counts is the trailing run ending at the
    // most recent pass, not "any three no-ops in history" -- a working pass
    // breaks it immediately.
    expect(run(fixture)).toContain(
      "OK|builder-pass|builder pass current, last 2026-08-01T00:00:00Z (age 0m; 3h cadence)|\n",
    );
  });

  it("uses the last trace record before trailing newlines", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-07-30T00:00:00Z","kind":"pass-ended","fact":{},"narration":{}}\n' +
        '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{},"narration":{}}\n\n',
    );

    expect(run(fixture)).toContain(
      "OK|trace-lease|trace current, last 2026-08-01T00:00:00Z; lease idle|\n",
    );
  });

  it("warns when the sprint trace is empty", () => {
    const fixture = baseFixture();
    writeFileSync(join(fixture.configDir, "ostrom", "sprint.jsonl"), "");

    expect(run(fixture)).toContain(
      "WARN|trace-lease|trace present but empty; lease idle|run /ostrom:gatekeep and confirm it appends a complete pass\n",
    );
  });

  it("preserves the malformed-record warning for a whitespace-only trace", () => {
    const fixture = baseFixture();
    writeFileSync(join(fixture.configDir, "ostrom", "sprint.jsonl"), " \t\n");

    expect(run(fixture)).toContain(
      "WARN|trace-lease|trace last record is unreadable; lease idle|inspect sprint.jsonl and repair or remove its malformed last record\n",
    );
  });

  it("warns when the last sprint trace record is malformed", () => {
    const fixture = baseFixture();
    writeFileSync(
      join(fixture.configDir, "ostrom", "sprint.jsonl"),
      '{"ts":"2026-08-01T00:00:00Z","kind":"pass-ended","fact":{},"narration":{}}\nnot-json\n',
    );

    expect(run(fixture)).toContain(
      "WARN|trace-lease|trace last record is unreadable; lease idle|inspect sprint.jsonl and repair or remove its malformed last record\n",
    );
  });

  it("reports held and stale leases against deterministic time", () => {
    const fixture = baseFixture();
    const leasePath = join(fixture.configDir, "ostrom", "sprint.lease");
    writeFileSync(
      leasePath,
      '{"owner":"gatekeeper-alpha","started_at":1785542400,"expires_at":1785546000}\n',
    );
    expect(run(fixture)).toContain(
      "OK|trace-lease|trace current, last 2026-08-01T00:00:00Z; lease held by gatekeeper-alpha until 2026-08-01T01:00:00.000Z|\n",
    );

    writeFileSync(
      leasePath,
      '{"owner":"gatekeeper-alpha","started_at":1785538800,"expires_at":1785542400}\n',
    );
    expect(run(fixture)).toContain(
      "WARN|trace-lease|trace current, last 2026-08-01T00:00:00Z; lease stale for gatekeeper-alpha, expired 2026-08-01T00:00:00.000Z|allow the next gatekeeper pass to reclaim the expired lease\n",
    );
  });

  it("is deterministic across repeated runs and does not touch config or logs", () => {
    const fixture = baseFixture();
    notionConfig(fixture);
    const configPath = join(fixture.configDir, "ostrom", "config.yaml");
    const tracePath = join(fixture.configDir, "ostrom", "sprint.jsonl");
    const target = readFileSync(configPath, "utf8");
    const trace = readFileSync(tracePath, "utf8");
    const first = run(fixture);
    const second = run(fixture);

    expect(second).toBe(first);
    expect(readFileSync(configPath, "utf8")).toBe(target);
    expect(readFileSync(tracePath, "utf8")).toBe(trace);
    expect(lstatSync(configPath).isSymbolicLink()).toBe(true);
    expect(readdirSync(fixture.cwd)).toEqual([]);
  });

  it("names unrelated marketplace histories and gives the remove/re-add remedy", () => {
    const fixture = baseFixture();
    const marketplace = join(
      fixture.configDir,
      "plugins",
      "marketplaces",
      "ostrom",
    );
    const replacement = join(fixture.root, "unrelated-cache");
    initRepo(replacement, "LOCAL.md", "unrelated\n");
    command("git", ["remote", "add", "origin", join(fixture.root, "marketplace-origin")], replacement);
    command("mv", [marketplace, join(fixture.root, "old-cache")]);
    command("mv", [replacement, marketplace]);

    expect(run(fixture)).toContain(
      "FAIL|marketplace|cached clone and origin/main have unrelated histories (marketplace was republished from a fresh history)|/plugin marketplace remove ostrom && /plugin marketplace add onsager-ai/ostrom\n",
    );
  });

  it("distinguishes a shared-history divergence from unrelated histories", () => {
    const fixture = baseFixture();
    const origin = join(fixture.root, "marketplace-origin");
    const marketplace = join(
      fixture.configDir,
      "plugins",
      "marketplaces",
      "ostrom",
    );
    writeFileSync(join(origin, "REMOTE.md"), "remote\n");
    command("git", ["add", "REMOTE.md"], origin);
    command("git", ["commit", "-m", "remote change"], origin);
    writeFileSync(join(marketplace, "LOCAL.md"), "local\n");
    command("git", ["add", "LOCAL.md"], marketplace);
    command("git", ["commit", "-m", "local change"], marketplace);

    expect(run(fixture)).toContain(
      "WARN|marketplace|cached clone has diverged from origin/main (shared history, not fast-forwardable)|/plugin marketplace update ostrom\n",
    );
  });

  it("never leaks Notion ids or configured bucket vocabulary", () => {
    const fixture = baseFixture();
    const secretId = "collection://768aef53-80c1-4d7a-a39f-6f468bc02c04";
    const buckets = ["可冻结", "需验证", "真我"];
    notionConfig(
      fixture,
      `  data_source: ${secretId}\nbuckets: [${buckets.join(", ")}]\n`,
    );
    const report = run(fixture);

    expect(report).not.toContain(secretId);
    for (const bucket of buckets) expect(report).not.toContain(bucket);
  });
});

describe("supported config shape", () => {
  it("parses scalars, one nested level, inline lists, and comments", () => {
    expect(
      parseOstromYaml(`
provider: file # comment
buckets: [freezable, "needs review", personal]
file:
  path: "~/touch # literal.md" # trailing comment
  auto_commit: true
`),
    ).toEqual({
      provider: "file",
      buckets: ["freezable", "needs review", "personal"],
      file: {
        path: "~/touch # literal.md",
        auto_commit: true,
      },
    });
  });
});

describe("untouched SessionStart hook", () => {
  it("emits exactly the frozen rules with an empty config directory", () => {
    const root = mkdtempSync(join(tmpdir(), "ostrom-hook-"));
    roots.push(root);
    const configDir = join(root, "empty");
    mkdirSync(configDir);
    const result = spawnSync("bash", [hook], {
      cwd: root,
      env: {
        ...process.env,
        CLAUDE_PLUGIN_ROOT: pluginRoot,
        CLAUDE_CONFIG_DIR: configDir,
      },
      encoding: "utf8",
    });

    expect(result.status).toBe(0);
    expect(result.stderr).toBe("");
    expect(result.stdout).toBe(readFileSync(frozenRules, "utf8"));
  });
});
