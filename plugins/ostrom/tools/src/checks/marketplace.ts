import { readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import { git } from "../lib/process.js";
import type { CheckResult } from "../lib/result.js";

export function checkMarketplace(context: DoctorContext): CheckResult {
  const knownJson = join(context.configDir, "plugins", "known_marketplaces.json");
  const marketplaceDir = join(
    context.configDir,
    "plugins",
    "marketplaces",
    "ostrom",
  );
  let knownSource = "";
  try {
    knownSource = readFileSync(knownJson, "utf8");
  } catch {
    // Missing/unreadable is reported as not registered, as before.
  }
  let knownIsFile = false;
  let cloneIsDirectory = false;
  try {
    knownIsFile = statSync(knownJson).isFile();
  } catch {
    // Report missing/unreadable metadata through the normal diagnosis.
  }
  try {
    cloneIsDirectory = statSync(join(marketplaceDir, ".git")).isDirectory();
  } catch {
    // Report a missing clone through the normal diagnosis.
  }
  if (!knownIsFile || !/"ostrom"\s*:/.test(knownSource)) {
    return {
      status: "FAIL",
      name: "marketplace",
      detail: "ostrom not registered in known_marketplaces.json",
      remedy: "/plugin marketplace add onsager-ai/ostrom",
    };
  }
  if (!cloneIsDirectory) {
    return {
      status: "FAIL",
      name: "marketplace",
      detail: `registered, but no cached clone at ${marketplaceDir}`,
      remedy: "/plugin marketplace add onsager-ai/ostrom",
    };
  }

  const fetch = git(marketplaceDir, ["fetch", "origin", "main"]);
  if (fetch.status !== 0) {
    const firstLine = `${fetch.stdout}${fetch.stderr}`.split(/\r?\n/, 1)[0] ?? "";
    return {
      status: "WARN",
      name: "marketplace",
      detail: `cannot verify freshness, git fetch failed (offline?): ${firstLine}`,
      remedy: "",
    };
  }
  if (
    git(marketplaceDir, ["rev-parse", "--verify", "origin/main"]).status !== 0
  ) {
    return {
      status: "WARN",
      name: "marketplace",
      detail: "fetched, but origin/main not found (default branch may differ)",
      remedy: "",
    };
  }
  if (
    git(marketplaceDir, [
      "merge-base",
      "--is-ancestor",
      "HEAD",
      "origin/main",
    ]).status === 0
  ) {
    return {
      status: "OK",
      name: "marketplace",
      detail: "cached clone can fast-forward to origin/main",
      remedy: "",
    };
  }
  if (
    git(marketplaceDir, ["merge-base", "HEAD", "origin/main"]).status === 0
  ) {
    return {
      status: "WARN",
      name: "marketplace",
      detail:
        "cached clone has diverged from origin/main (shared history, not fast-forwardable)",
      remedy: "/plugin marketplace update ostrom",
    };
  }
  return {
    status: "FAIL",
    name: "marketplace",
    detail:
      "cached clone and origin/main have unrelated histories (marketplace was republished from a fresh history)",
    remedy:
      "/plugin marketplace remove ostrom && /plugin marketplace add onsager-ai/ostrom",
  };
}
