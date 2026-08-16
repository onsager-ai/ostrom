import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';
import { ROOT, argValue, config, packageDirs } from './lib.mjs';

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

for (const pkg of packages) {
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
