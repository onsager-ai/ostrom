import { execFileSync, spawnSync } from "node:child_process";
import {
  cpSync,
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
import { parseOstromYaml } from "../src/lib/config.js";
import { runDoctor } from "../src/lib/doctor.js";
import { formatResult, type CheckResult } from "../src/lib/result.js";

const sourcePluginRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const hook = join(sourcePluginRoot, "hooks", "inject-constitution.sh");
const frozenRules = join(sourcePluginRoot, "rules", "frozen-rules.md");
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
  const installPath = join(root, "installed-constitution");
  mkdirSync(join(configDir, "plugins"), { recursive: true });
  mkdirSync(cwd, { recursive: true });
  mkdirSync(installPath, { recursive: true });
  for (const directory of [".claude-plugin", "config", "hooks", "rules"]) {
    cpSync(join(sourcePluginRoot, directory), join(installPath, directory), {
      recursive: true,
    });
  }
  const installedVersion = JSON.parse(
    readFileSync(join(installPath, ".claude-plugin", "plugin.json"), "utf8"),
  ).version as string;
  writeFileSync(
    join(configDir, "plugins", "installed_plugins.json"),
    JSON.stringify({
      plugins: {
        "constitution@ostrom": [
          { installPath, version: installedVersion },
        ],
      },
    }),
  );
  mkdirSync(home, { recursive: true });
  const fixture = { root, home, configDir, cwd, installPath };
  wireMarketplace(fixture);
  return fixture;
}

function run(fixture: Fixture, env: NodeJS.ProcessEnv = {}): string {
  return runDoctor({
    pluginRoot: fixture.installPath,
    configDir: fixture.configDir,
    cwd: fixture.cwd,
    home: fixture.home,
    env: { ...process.env, ...env },
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
      detail: "installed, version 0.7.0",
      remedy: "",
    },
    {
      status: "OK",
      name: "marketplace",
      detail: "cached clone can fast-forward to origin/main",
      remedy: "",
    },
    { status: "OK", name: "rules-layers", detail: rulesDetail, remedy: "" },
    {
      status: "OK",
      name: "rule-distribution",
      detail:
        "running payload has 4 frozen rules; no constitution marketplace cache found; repo checkout not found",
      remedy: "",
    },
    touch,
    provider,
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

function fixtureRules(count: number, label = "rule"): string {
  const headings = Array.from(
    { length: count },
    (_, index) => `## ${label} ${index + 1}\n\nbody ${index + 1}`,
  );
  return `# Frozen working conventions\n\n${headings.join("\n\n")}\n`;
}

function wireCachedPayload(
  fixture: Fixture,
  marketplace: string,
  cacheVersion: string,
  version: string,
  rules: string,
): void {
  const payload = join(
    fixture.configDir,
    "plugins",
    "cache",
    marketplace,
    "constitution",
    cacheVersion,
  );
  mkdirSync(join(payload, ".claude-plugin"), { recursive: true });
  mkdirSync(join(payload, "rules"), { recursive: true });
  writeFileSync(
    join(payload, ".claude-plugin", "plugin.json"),
    JSON.stringify({ name: "constitution", version }),
  );
  writeFileSync(join(payload, "rules", "frozen-rules.md"), rules);
}

function wireRepoPayload(
  fixture: Fixture,
  version: string,
  rules: string,
): void {
  initRepo(fixture.cwd, "README.md", "ostrom\n");
  const constitution = join(fixture.cwd, "plugins", "constitution");
  mkdirSync(join(constitution, ".claude-plugin"), { recursive: true });
  mkdirSync(join(constitution, "rules"), { recursive: true });
  writeFileSync(
    join(constitution, ".claude-plugin", "plugin.json"),
    JSON.stringify({ name: "constitution", version }),
  );
  writeFileSync(join(constitution, "rules", "frozen-rules.md"), rules);
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
      pluginRoot: fixture.installPath,
      configDir: missing,
      cwd: fixture.cwd,
      home: fixture.home,
      env: {
        HOME: fixture.home,
        CLAUDE_CONFIG_DIR: missing,
      },
    });

    expect(report.split("\n").filter(Boolean)).toHaveLength(8);
    expect(existsSync(missing)).toBe(false);
  });

  it("is deterministic across repeated runs and does not touch config or logs", () => {
    const fixture = baseFixture();
    notionConfig(fixture);
    const configPath = join(fixture.configDir, "ostrom", "config.yaml");
    const target = readFileSync(configPath, "utf8");
    const first = run(fixture);
    const second = run(fixture);

    expect(second).toBe(first);
    expect(readFileSync(configPath, "utf8")).toBe(target);
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

describe("rule distribution", () => {
  it("stays OK with running-payload facts when no cache or checkout is present", () => {
    const fixture = baseFixture();

    expect(run(fixture)).toContain(
      "OK|rule-distribution|running payload has 4 frozen rules; no constitution marketplace cache found; repo checkout not found|\n",
    );
  });

  it("stays OK for a marketplace-only user while reporting the cache", () => {
    const fixture = baseFixture();
    wireCachedPayload(fixture, "company", "0.6.0", "0.6.0", fixtureRules(3));

    expect(run(fixture)).toContain(
      "OK|rule-distribution|running payload has 4 frozen rules; marketplace company cache 0.6.0 has 3 frozen rules and declares version 0.6.0; repo checkout not found|\n",
    );
  });

  it("stays OK when a checkout exists but no marketplace cache does", () => {
    const fixture = baseFixture();
    wireRepoPayload(fixture, "0.7.0", fixtureRules(4));

    expect(run(fixture)).toContain(
      "OK|rule-distribution|running payload has 4 frozen rules; no constitution marketplace cache found; repo has 4 frozen rules and declares version 0.7.0|\n",
    );
  });

  it("detects a stale marketplace cache even when the running bundle matches the repo", () => {
    const fixture = baseFixture();
    wireCachedPayload(fixture, "ostrom", "0.6.0", "0.6.0", fixtureRules(3));
    wireRepoPayload(
      fixture,
      "0.7.0",
      readFileSync(join(fixture.installPath, "rules", "frozen-rules.md"), "utf8"),
    );

    expect(run(fixture)).toContain(
      "FAIL|rule-distribution|running payload has 4 frozen rules; marketplace ostrom cache 0.6.0 has 3 frozen rules and declares version 0.6.0; repo has 4 frozen rules and declares version 0.7.0; cache differs from repo in marketplace ostrom|refresh the ostrom marketplace cache; if one stays stale, remove and re-add that marketplace\n",
    );
  });

  it("calls out equal-version content drift as a missed version bump", () => {
    const fixture = baseFixture();
    wireCachedPayload(
      fixture,
      "private",
      "0.7.0",
      "0.7.0",
      fixtureRules(4, "cached"),
    );
    wireRepoPayload(fixture, "0.7.0", fixtureRules(4, "repo"));

    expect(run(fixture)).toContain(
      "missed-version-bump signature in marketplace private: equal version 0.7.0 with differing rule content|bump the constitution plugin version in plugins/constitution/.claude-plugin/plugin.json; then refresh the private marketplace cache",
    );
  });

  it("selects the highest semantic version in each marketplace cache", () => {
    const fixture = baseFixture();
    const currentRules = fixtureRules(4, "current");
    wireCachedPayload(fixture, "ostrom", "0.9.0", "0.9.0", fixtureRules(3));
    wireCachedPayload(fixture, "ostrom", "0.10.0", "0.10.0", currentRules);
    wireRepoPayload(fixture, "0.10.0", currentRules);

    const report = run(fixture);
    expect(report).toContain(
      "OK|rule-distribution|running payload has 4 frozen rules; marketplace ostrom cache 0.10.0 has 4 frozen rules and declares version 0.10.0; repo has 4 frozen rules and declares version 0.10.0|\n",
    );
    expect(report).not.toContain("marketplace ostrom cache 0.9.0");
  });

  it("reports every marketplace that carries the constitution plugin", () => {
    const fixture = baseFixture();
    const rules = fixtureRules(4);
    wireCachedPayload(fixture, "alpha", "0.7.0", "0.7.0", rules);
    wireCachedPayload(fixture, "beta", "0.6.0", "0.6.0", fixtureRules(3));
    wireRepoPayload(fixture, "0.7.0", rules);

    const report = run(fixture);
    expect(report).toContain("marketplace alpha cache 0.7.0");
    expect(report).toContain("marketplace beta cache 0.6.0");
    expect(report).toContain("cache differs from repo in marketplace beta");
  });

  it("passes when the highest cached payload and repo declaration match", () => {
    const fixture = baseFixture();
    const rules = fixtureRules(4);
    wireCachedPayload(fixture, "renamed-marketplace", "0.7.0", "0.7.0", rules);
    wireRepoPayload(fixture, "0.7.0", rules);

    expect(run(fixture)).toContain(
      "OK|rule-distribution|running payload has 4 frozen rules; marketplace renamed-marketplace cache 0.7.0 has 4 frozen rules and declares version 0.7.0; repo has 4 frozen rules and declares version 0.7.0|\n",
    );
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
        CLAUDE_PLUGIN_ROOT: sourcePluginRoot,
        CLAUDE_CONFIG_DIR: configDir,
      },
      encoding: "utf8",
    });

    expect(result.status).toBe(0);
    expect(result.stderr).toBe("");
    expect(result.stdout).toBe(readFileSync(frozenRules, "utf8"));
  });
});
