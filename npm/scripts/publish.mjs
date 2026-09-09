import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  ROOT,
  argValue,
  cargoVersion,
  config,
  packageDirs,
  registryHasVersion,
} from './lib.mjs';

// The publish/skip decision for a batch of packages, kept pure and separate
// from the execFileSync side effect so it can be unit-tested without
// spawning npm. Never called in --dry-run mode: dry runs stay offline and
// always publish.
export function publishPlan(packages, version, view) {
  return packages.map((pkg) => ({
    name: pkg.name,
    action: registryHasVersion(pkg.name, version, view) ? 'skip' : 'publish',
  }));
}

export function skipMessage(name, version) {
  return `already published: ${name}@${version}`;
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
  const target = argValue(args, '--target', 'platforms');
  const tag = argValue(args, '--tag', 'latest');
  const stagingRoot = resolve(
    ROOT,
    argValue(args, '--staging', config.stagingDir),
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
  const plan = dryRun
    ? packages.map((pkg) => ({ name: pkg.name, action: 'publish' }))
    : publishPlan(packages, version);

  const actions = new Map(plan.map((entry) => [entry.name, entry.action]));

  // Iterating the packages rather than the plan is deliberate: a package the
  // plan somehow omitted gets published, not silently dropped.
  for (const pkg of packages) {
    if (actions.get(pkg.name) === 'skip') {
      console.log(skipMessage(pkg.name, version));
      continue;
    }
    const npmArgs = [
      'publish',
      pkg.dir,
      '--access',
      'public',
      '--tag',
      tag,
      '--provenance',
    ];
    if (dryRun) npmArgs.push('--dry-run');
    console.log(`${dryRun ? '[dry-run] ' : ''}publishing ${pkg.name}`);
    execFileSync('npm', npmArgs, { stdio: 'inherit' });
  }
}
