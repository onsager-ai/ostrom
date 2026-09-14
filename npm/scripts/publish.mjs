import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  ROOT,
  argValue,
  cargoVersion,
  config,
  decidePublishAction,
  defaultSleep,
  packageDirs,
  readPackResults,
  registryState,
  resolvePublishablePackage,
} from './lib.mjs';
import { waitForPackages } from './wait-for-platforms.mjs';

// How long to keep re-reading the registry after `npm publish` itself
// reports failure, before believing that failure. npm's own exit code is not
// trustworthy at the boundary this task exists to fix — a publish call can
// fail its HTTP round trip while the registry still accepts and later serves
// the bytes (see lib.mjs's registryState comment). This budget is shorter
// than wait-for-platforms.mjs's because it only covers a registry catching
// up with a publish call this process just made, not a cold wait for a
// sibling job's packages.
export const PUBLISH_RECOVERY_TIMEOUT_MS = 10 * 60 * 1000;

function runNpmPublish({ tarballPath, tag, dryRun }) {
  const npmArgs = [
    'publish',
    tarballPath,
    '--access',
    'public',
    '--tag',
    tag,
    '--provenance',
  ];
  if (dryRun) npmArgs.push('--dry-run');
  execFileSync('npm', npmArgs, { stdio: 'inherit' });
}

// Publish (or skip, or recover) a single package, kept pure of process.exit
// and separate from execFileSync so it can be unit-tested without spawning
// npm or the real registry. `publish` and `state` are injected for tests;
// production code gets `runNpmPublish` and `registryState`.
export async function publishPackage({
  name,
  version,
  tarballPath,
  integrity,
  gitHead,
  tag,
  rebuildOfTag = false,
  dryRun = false,
  publish = runNpmPublish,
  state = registryState,
  sleep = defaultSleep,
  now = Date.now,
  recoveryTimeoutMs = PUBLISH_RECOVERY_TIMEOUT_MS,
}) {
  if (dryRun) {
    console.log(`[dry-run] publishing ${name}@${version}`);
    publish({ tarballPath, tag, dryRun: true });
    return { action: 'published' };
  }

  const current = await state(name, version, { sleep });
  const decision = decidePublishAction(current, {
    name,
    version,
    integrity,
    gitHead,
    rebuildOfTag,
  });
  if (decision.action === 'skip') {
    console.log(decision.message);
    return decision;
  }

  console.log(`publishing ${name}@${version} from ${tarballPath}`);
  try {
    publish({ tarballPath, tag, dryRun: false });
    return { action: 'published' };
  } catch (publishError) {
    console.error(
      `npm publish reported a failure for ${name}@${version}; re-reading ` +
        `the registry before giving up: ${publishError.message}`,
    );
    try {
      await waitForPackages({
        packages: [{ name, version, integrity }],
        expected: { gitHead, rebuildOfTag },
        state,
        sleep,
        now,
        timeoutMs: recoveryTimeoutMs,
        tag,
      });
    } catch (recoveryError) {
      // A REGISTRY_CONFLICT is the one genuine failure — the registry holds
      // different bytes than this build produced — and is thrown as-is,
      // naming both integrities and both commits. Anything else (the
      // recovery wait timed out, or registryState itself could never
      // classify the state) means the registry never confirmed this
      // package, so the original `npm publish` error is not forgotten.
      if (recoveryError.code === 'REGISTRY_CONFLICT') {
        throw recoveryError;
      }
      throw new Error(
        `npm publish failed for ${name}@${version} and the registry did ` +
          `not confirm matching bytes within the recovery window: ` +
          `${recoveryError.message}. Original npm publish error: ${publishError.message}`,
      );
    }
    console.log(
      `npm reported a publish failure for ${name}@${version}, but the ` +
        'registry now holds the right bytes; treating this as success',
    );
    return { action: 'published-after-recovery' };
  }
}

// Run only when invoked as a script, so tests may import the decision above
// without spawning npm. A broken guard here would make the whole publish step
// a no-op that still exits 0, so `publish.mjs refuses an unknown --target`
// in npm/tests/distribution.test.mjs spawns this file to trip it.
if (
  process.argv[1] &&
  fileURLToPath(import.meta.url) === resolve(process.argv[1])
) {
  const args = process.argv.slice(2);
  const dryRun = args.includes('--dry-run');
  const rebuildOfTag = args.includes('--rebuild-of-tag');
  const target = argValue(args, '--target', 'platforms');
  const tag = argValue(args, '--tag', 'latest');
  const stagingRoot = resolve(
    ROOT,
    argValue(args, '--staging', config.stagingDir),
  );
  const tarballsRoot = resolve(
    ROOT,
    argValue(args, '--tarballs', 'target/npm-tarballs'),
  );

  if (!dryRun && config.scope.includes('placeholder')) {
    throw new Error(
      'refusing to publish with the placeholder npm scope; update npm/publish.config.json first',
    );
  }

  const packages = packageDirs(stagingRoot).filter((pkg) => {
    if (target === 'platforms') return pkg.kind === 'platform';
    if (target === 'main') return pkg.kind === 'main';
    throw new Error(`unknown --target ${target}; expected platforms or main`);
  });

  const version = cargoVersion();
  const packResults = readPackResults(tarballsRoot);

  // Iterating the staged packages rather than any precomputed plan is
  // deliberate: a package pack-results.json somehow omitted throws (inside
  // resolvePublishablePackage) rather than being silently skipped.
  for (const pkg of packages) {
    const resolved = resolvePublishablePackage(pkg, packResults, tarballsRoot);
    await publishPackage({
      name: pkg.name,
      version,
      tarballPath: resolved.tarballPath,
      integrity: resolved.integrity,
      gitHead: resolved.gitHead,
      tag,
      rebuildOfTag,
      dryRun,
    });
  }
}
