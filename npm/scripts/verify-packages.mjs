import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { ROOT, argValue, config, packageDirs } from './lib.mjs';
import { assertVersion } from './assert-version.mjs';

const args = process.argv.slice(2);
const stagingRoot = resolve(
  ROOT,
  argValue(args, '--staging', config.stagingDir),
);
const packages = packageDirs(stagingRoot);
const manifests = packages.map((pkg) => ({
  ...pkg,
  manifest: JSON.parse(readFileSync(join(pkg.dir, 'package.json'), 'utf8')),
}));
const versions = new Set(manifests.map(({ manifest }) => manifest.version));

if (versions.size !== 1) {
  throw new Error(`npm package version mismatch: ${[...versions].join(', ')}`);
}
for (const { name, manifest } of manifests) {
  if (manifest.scripts?.postinstall) {
    throw new Error(`${name} contains a forbidden postinstall script`);
  }
}

const versionOutput = argValue(args, '--version-output', undefined);
if (versionOutput) {
  assertVersion(
    readFileSync(resolve(ROOT, versionOutput), 'utf8'),
    manifests.find(({ kind }) => kind === 'main').manifest.version,
  );
  console.log(`binary and package version match: ${[...versions][0]}`);
}

console.log(
  `${packages.length} package manifests verified at version ${[...versions][0]}; no postinstall scripts`,
);
