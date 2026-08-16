import { readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import {
  ROOT,
  argValue,
  cargoVersion,
  config,
  mainPackageName,
  platformPackageName,
} from './lib.mjs';

const args = process.argv.slice(2);
const stagingRoot = resolve(
  ROOT,
  argValue(args, '--staging', config.stagingDir),
);
const outputRoot = resolve(
  ROOT,
  argValue(args, '--output', 'target/npm-tarballs'),
);
const files = readdirSync(outputRoot);
const archives = files.filter((file) => file.endsWith('.tgz'));
const resultFiles = files.filter((file) => file.startsWith('.pack-result-'));
const results = resultFiles.flatMap((file) =>
  JSON.parse(readFileSync(join(outputRoot, file), 'utf8')),
);

if (archives.length !== config.platforms.length + 1) {
  throw new Error(
    `expected ${config.platforms.length + 1} tarballs, found ${archives.length}`,
  );
}
if (results.length !== archives.length) {
  throw new Error(
    `expected ${archives.length} npm pack results, found ${results.length}`,
  );
}
for (const result of results) {
  if (result.files.some(({ path }) => /postinstall(?:\.js)?$/.test(path))) {
    throw new Error(
      `${result.name} tarball contains a forbidden postinstall file`,
    );
  }
}
for (const platform of config.platforms.filter(({ os }) => os !== 'win32')) {
  const result = results.find(
    ({ name }) => name === platformPackageName(platform),
  );
  for (const binary of config.binaryNames) {
    const packed = result?.files.find(({ path }) => path === binary);
    if (!packed || (packed.mode & 0o111) === 0) {
      throw new Error(
        `${platform.platform} binary is not executable in its tarball`,
      );
    }
  }
}

const manifestPaths = [
  ...config.platforms.map((platform) =>
    join(stagingRoot, platform.dir, 'package.json'),
  ),
  join(stagingRoot, config.mainPackage.name, 'package.json'),
];
const manifests = manifestPaths.map((path) =>
  JSON.parse(readFileSync(path, 'utf8')),
);
const versions = new Set(manifests.map(({ version }) => version));
const names = new Set(manifests.map(({ name }) => name));
const expectedNames = new Set([
  ...config.platforms.map(platformPackageName),
  mainPackageName(),
]);
if (versions.size !== 1 || !versions.has(cargoVersion())) {
  throw new Error(`packed package versions do not match Cargo ${cargoVersion()}`);
}
if (
  names.size !== expectedNames.size ||
  [...names].some((name) => !expectedNames.has(name))
) {
  throw new Error('packed package names do not match publish configuration');
}
for (const manifest of manifests) {
  if (manifest.scripts?.postinstall) {
    throw new Error(`${manifest.name} contains a forbidden postinstall script`);
  }
}

writeFileSync(
  join(outputRoot, 'pack-results.json'),
  `${JSON.stringify(results, null, 2)}\n`,
);
for (const file of resultFiles) rmSync(join(outputRoot, file));
console.log(
  `verified ${archives.length} npm tarballs at version ${cargoVersion()}; no postinstall scripts`,
);
