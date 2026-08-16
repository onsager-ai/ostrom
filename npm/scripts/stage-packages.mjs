import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { join, resolve } from 'node:path';
import {
  ROOT,
  argValue,
  binaryNames,
  cargoVersion,
  config,
  mainPackageName,
  platformPackageName,
  requireSafeOutputPath,
  sourcePlatformDir,
} from './lib.mjs';

const args = process.argv.slice(2);
const artifactsRoot = resolve(
  ROOT,
  argValue(args, '--artifacts', 'artifacts'),
);
const outputRoot = requireSafeOutputPath(
  resolve(ROOT, argValue(args, '--output', config.stagingDir)),
);
const version = argValue(args, '--version', cargoVersion());

const headers = {
  linux: [[0x7f, 0x45, 0x4c, 0x46]],
  darwin: [
    [0xcf, 0xfa, 0xed, 0xfe],
    [0xfe, 0xed, 0xfa, 0xcf],
  ],
  win32: [[0x4d, 0x5a]],
};

function headerMatches(filePath, os) {
  const signatures = headers[os] ?? [];
  const bytes = readFileSync(filePath);
  return signatures.some((signature) =>
    signature.every((byte, index) => bytes[index] === byte),
  );
}

function writeJson(filePath, value) {
  writeFileSync(filePath, `${JSON.stringify(value, null, 2)}\n`);
}

rmSync(outputRoot, { recursive: true, force: true });
mkdirSync(outputRoot, { recursive: true });

const optionalDependencies = {};
const platformPackages = {};

for (const platform of config.platforms) {
  if (!existsSync(sourcePlatformDir(platform))) {
    throw new Error(
      `missing platform package directory: ${sourcePlatformDir(platform)}`,
    );
  }
  const packageName = platformPackageName(platform);
  const packageRoot = join(outputRoot, platform.dir);
  mkdirSync(packageRoot, { recursive: true });

  for (const binary of binaryNames(platform)) {
    const source = join(
      artifactsRoot,
      `binary-${platform.platform}`,
      binary,
    );
    const destination = join(packageRoot, binary);
    if (!existsSync(source)) {
      throw new Error(`missing build artifact: ${source}`);
    }
    if (statSync(source).size === 0) {
      throw new Error(`empty build artifact: ${source}`);
    }
    if (!headerMatches(source, platform.os)) {
      throw new Error(`invalid ${platform.os} binary header: ${source}`);
    }
    copyFileSync(source, destination);
    if (platform.os !== 'win32') chmodSync(destination, 0o755);
  }

  writeJson(join(packageRoot, 'package.json'), {
    name: packageName,
    version,
    description: `Prebuilt Ostrom CLI for ${platform.platform}.`,
    license: 'MIT',
    os: [platform.os],
    cpu: [platform.cpu],
    main: binaryNames(platform)[0],
    files: binaryNames(platform),
    repository: { type: 'git', url: config.repositoryUrl },
    publishConfig: { access: 'public' },
  });

  optionalDependencies[packageName] = version;
  platformPackages[platform.nodeKey] = packageName;
  console.log(`staged ${packageName}@${version}`);
}

const mainRoot = join(outputRoot, config.mainPackage.name);
mkdirSync(mainRoot, { recursive: true });
copyFileSync(
  join(ROOT, config.mainPackage.dir, 'bin.js'),
  join(mainRoot, 'bin.js'),
);
copyFileSync(
  join(ROOT, config.mainPackage.dir, 'README.md'),
  join(mainRoot, 'README.md'),
);
chmodSync(join(mainRoot, 'bin.js'), 0o755);
writeJson(join(mainRoot, 'package.json'), {
  name: mainPackageName(),
  version,
  description: 'Ostrom workflow commons command-line interface.',
  license: 'MIT',
  bin: { [config.binaryNames[0]]: 'bin.js' },
  files: ['bin.js', 'README.md'],
  engines: { node: '>=18' },
  repository: { type: 'git', url: config.repositoryUrl },
  publishConfig: { access: 'public' },
  optionalDependencies,
  ostrom: { platformPackages },
});
console.log(`staged ${mainPackageName()}@${version}`);
console.log(`staged ${config.platforms.length + 1} npm packages in ${outputRoot}`);
