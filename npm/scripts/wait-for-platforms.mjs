import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  ROOT,
  argValue,
  cargoVersion,
  config,
  decidePublishAction,
  defaultSleep,
  platformPackageName,
  readPackResults,
  registryState,
  resolvePublishablePackage,
} from './lib.mjs';

// Bounded by time, not by attempt count. npm's registry propagation delay is
// a real-world duration that varies by package and by moment, not a fixed
// number of tries at a fixed interval — the v0.14.0 release (#585) is the
// case this replaces: a 40-attempt/15s (10 minute) budget gave up at
// 04:51:41Z while the registry went on to serve the missing platform package
// at 04:54:38Z, three minutes later. Exponential backoff also spends fewer
// requests early, when a package that just published is least likely to be
// visible yet.
export const DEFAULT_WAIT_TIMEOUT_MS = 45 * 60 * 1000;
export const DEFAULT_WAIT_INITIAL_DELAY_MS = 5000;
export const DEFAULT_WAIT_MAX_DELAY_MS = 60000;

export function waitTimeoutMessage(missing, { tag } = {}) {
  const tagArg = tag ? `tag=${tag}` : 'tag=<the release tag>';
  return (
    `registry propagation timed out waiting for: ${missing.join(', ')}. ` +
    'Every publish command succeeded; the registry has simply not yet ' +
    'served these packages. Once they are visible on the registry, re-run ' +
    'the failed job (gh run rerun <run-id> --failed) and it will skip what ' +
    'is already there. If the build artifacts have expired (1-day ' +
    `retention), dispatch the Release workflow with ${tagArg} instead.`
  );
}

// Poll the registry for a set of packages until each is present with the
// bytes this build produced — or, under `expected.rebuildOfTag`, from the
// same commit — sharing decidePublishAction's identity rule with publish.mjs
// so a package can never be "satisfied" here by a rule looser than the one
// that decided whether to publish it. A present-but-different package is a
// real conflict (npm versions are immutable) and fails immediately rather
// than waiting out the budget; only "absent" and "unknown" mean "keep
// polling".
export async function waitForPackages({
  packages,
  expected = {},
  state = registryState,
  sleep = defaultSleep,
  now = Date.now,
  timeoutMs = DEFAULT_WAIT_TIMEOUT_MS,
  initialDelayMs = DEFAULT_WAIT_INITIAL_DELAY_MS,
  maxDelayMs = DEFAULT_WAIT_MAX_DELAY_MS,
  tag,
} = {}) {
  const { gitHead, rebuildOfTag = false } = expected;
  const pending = new Map(packages.map((pkg) => [pkg.name, pkg]));
  const start = now();
  let delay = initialDelayMs;
  let lastUnknown;

  for (;;) {
    for (const pkg of [...pending.values()]) {
      let result;
      try {
        result = await state(pkg.name, pkg.version);
      } catch (error) {
        // registryState throws only when it cannot classify the state even
        // after its own retries: an outage, not an answer. Waiting is exactly
        // where that must not end the run, because the registry may already
        // hold the right bytes and simply be unreachable for a few minutes.
        // The package stays pending and the time budget decides; the last
        // such error is reported if the budget runs out.
        //
        // Each such read has already spent registryState's own retries, about
        // 62s by default, before arriving here. During an outage, one pass over
        // five pending packages can therefore take about five minutes, and the
        // budget can overrun by up to that much. That is accepted: overrunning
        // a wait is harmless, and giving up early is the failure this avoids.
        lastUnknown = `${pkg.name}@${pkg.version}: ${error.message}`;
        continue;
      }
      // Lets a REGISTRY_CONFLICT error from decidePublishAction propagate
      // immediately, unwrapped — that is the "do not wait it out" case.
      const decision = decidePublishAction(result, {
        name: pkg.name,
        version: pkg.version,
        integrity: pkg.integrity,
        gitHead,
        rebuildOfTag,
      });
      if (decision.action === 'skip') {
        console.log(decision.message);
        pending.delete(pkg.name);
      }
    }
    if (pending.size === 0) {
      return { satisfied: packages.map((pkg) => pkg.name) };
    }
    if (now() - start >= timeoutMs) {
      const error = new Error(
        waitTimeoutMessage([...pending.keys()], { tag }) +
          (lastUnknown ? ` Last unclassifiable registry read: ${lastUnknown}.` : ''),
      );
      error.code = 'WAIT_TIMEOUT';
      throw error;
    }
    await sleep(Math.min(delay, maxDelayMs));
    delay = Math.min(delay * 2, maxDelayMs);
  }
}

// Run only when invoked as a script, so tests may import waitForPackages and
// waitTimeoutMessage without spawning npm or sleeping for real. The same
// entry-guard test shape as publish.mjs: an unrecognised flag has to fail
// loudly and offline, before this ever reaches the registry.
if (
  process.argv[1] &&
  fileURLToPath(import.meta.url) === resolve(process.argv[1])
) {
  const args = process.argv.slice(2);
  const version = argValue(args, '--version', cargoVersion());
  const rebuildOfTag = args.includes('--rebuild-of-tag');
  const tarballsRoot = resolve(
    ROOT,
    argValue(args, '--tarballs', 'target/npm-tarballs'),
  );
  const timeoutMsRaw = argValue(
    args,
    '--timeout-ms',
    String(DEFAULT_WAIT_TIMEOUT_MS),
  );
  const timeoutMs = Number(timeoutMsRaw);
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
    throw new Error(
      `--timeout-ms must be a positive number, got ${JSON.stringify(timeoutMsRaw)}`,
    );
  }

  const packResults = readPackResults(tarballsRoot);
  const stagingRoot = resolve(ROOT, config.stagingDir);
  const packages = config.platforms.map((platform) => {
    const pkg = {
      dir: join(stagingRoot, platform.dir),
      name: platformPackageName(platform),
    };
    return {
      ...resolvePublishablePackage(pkg, packResults, tarballsRoot),
      version,
    };
  });
  const gitHead = packages[0]?.gitHead;
  const tag = version.startsWith('v') ? version : `v${version}`;

  await waitForPackages({
    packages,
    expected: { gitHead, rebuildOfTag },
    timeoutMs,
    tag,
  });
  console.log(`all platform packages are visible at ${version}`);
}
