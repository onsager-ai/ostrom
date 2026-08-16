import { readFileSync } from 'node:fs';
import { dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

export const ROOT = resolve(here, '..', '..');
export const config = JSON.parse(
  readFileSync(join(ROOT, 'npm', 'publish.config.json'), 'utf8'),
);

export function cargoVersion() {
  const toml = readFileSync(join(ROOT, config.cargoWorkspace), 'utf8');
  const section = toml
    .split(/^\[/m)
    .find((candidate) => candidate.startsWith('workspace.package'));
  const match = section?.match(/^\s*version\s*=\s*"([^"]+)"/m);
  if (!match) {
    throw new Error('could not find [workspace.package] version in Cargo.toml');
  }
  return match[1];
}

// The launcher is deliberately unscoped: `npm i -g ostrom` is the name people
// type, and it is the package a human installs. The scope exists to keep the
// five platform packages unsquattable as a set, not to namespace the entry
// point. Do not "fix" this back to `${scope}/${name}` — 0.1.0 shipped that way
// by accident and had to be reissued.
export function mainPackageName() {
  return config.mainPackage.name;
}

export function platformPackageName(platform) {
  return `${config.scope}/${platform.dir}`;
}

export function sourcePlatformDir(platform) {
  return join(ROOT, config.platformDir, platform.dir);
}

export function binaryNames(platform) {
  return config.binaryNames.map((name) => `${name}${platform.ext}`);
}

export function packageDirs(stagingRoot) {
  return [
    ...config.platforms.map((platform) => ({
      dir: join(stagingRoot, platform.dir),
      name: platformPackageName(platform),
      kind: 'platform',
      platform,
    })),
    {
      dir: join(stagingRoot, config.mainPackage.name),
      name: mainPackageName(),
      kind: 'main',
    },
  ];
}

export function requireSafeOutputPath(output) {
  const absolute = resolve(output);
  const targetRoot = join(ROOT, 'target');
  if (!absolute.startsWith(`${targetRoot}${sep}`)) {
    throw new Error(`refusing unsafe output path: ${absolute}`);
  }
  return absolute;
}

export function argValue(args, flag, fallback) {
  const index = args.indexOf(flag);
  if (index < 0) return fallback;
  const value = args[index + 1];
  if (!value || value.startsWith('--')) {
    throw new Error(`${flag} requires a value`);
  }
  return value;
}
