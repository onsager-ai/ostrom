import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ROOT } from './lib.mjs';

const packageName = '@ostrom/cli';
// The marketplace plugin and npm CLI ship independently. Querying the exact
// declared floor prevents a green source build from approving a plugin whose
// first command cannot run on any version an operator can actually install.
// This is an operational-correctness floor, not merely the first release with
// the subcommands: 0.2.0 resolved most commands through the wrong XDG fallback,
// and 0.2.1 made an empty CLAUDE_CONFIG_DIR relative. 0.2.2 is the first release
// that reliably resolves the state root those commands must read.
const defaultManifest = join(
  ROOT,
  'plugins',
  'ostrom',
  '.claude-plugin',
  'plugin.json',
);

export class RegistryInconclusiveError extends Error {
  constructor(message) {
    super(message);
    this.name = 'RegistryInconclusiveError';
  }
}

function parseSemVer(value) {
  const match = value.match(
    /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/,
  );
  if (!match) return undefined;
  return {
    core: [Number(match[1]), Number(match[2]), Number(match[3])],
    prerelease: match[4]?.split('.') ?? [],
  };
}

function compareIdentifiers(left, right) {
  const leftNumeric = /^\d+$/.test(left);
  const rightNumeric = /^\d+$/.test(right);
  if (leftNumeric && rightNumeric) return Number(left) - Number(right);
  if (leftNumeric) return -1;
  if (rightNumeric) return 1;
  return left.localeCompare(right);
}

function compareSemVer(left, right) {
  for (let index = 0; index < left.core.length; index += 1) {
    const difference = left.core[index] - right.core[index];
    if (difference !== 0) return difference;
  }
  if (left.prerelease.length === 0 && right.prerelease.length === 0) return 0;
  if (left.prerelease.length === 0) return 1;
  if (right.prerelease.length === 0) return -1;
  const length = Math.max(left.prerelease.length, right.prerelease.length);
  for (let index = 0; index < length; index += 1) {
    const leftIdentifier = left.prerelease[index];
    const rightIdentifier = right.prerelease[index];
    if (leftIdentifier === undefined) return -1;
    if (rightIdentifier === undefined) return 1;
    const difference = compareIdentifiers(leftIdentifier, rightIdentifier);
    if (difference !== 0) return difference;
  }
  return 0;
}

export function pluginCliFloor(manifestPath = defaultManifest) {
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  const floor = manifest.minimumCliVersion;
  if (
    typeof floor !== 'string' ||
    !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?$/.test(floor)
  ) {
    throw new Error(
      `plugin manifest must declare a semantic minimumCliVersion, got ${JSON.stringify(floor)}`,
    );
  }
  return floor;
}

export function publishedCliVersion(floor, npmCommand = 'npm') {
  const result = spawnSync(
    npmCommand,
    ['view', `${packageName}@${floor}`, 'version', '--json'],
    {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 30_000,
    },
  );
  if (result.status !== 0) {
    const detail = [result.stderr, result.stdout, result.error?.message]
      .filter(Boolean)
      .join('\n')
      .trim();
    const definitelyAbsent =
      /\b(?:E404|ETARGET)\b|\b404 Not Found\b|No match(?:ing)? version found/i.test(
        detail,
      );
    if (definitelyAbsent) {
      throw new Error(
        `${packageName}@${floor} is not published${detail ? `: ${detail}` : ''}`,
      );
    }
    throw new RegistryInconclusiveError(
      `registry query for ${packageName}@${floor} was inconclusive${detail ? `: ${detail}` : ''}`,
    );
  }
  try {
    const published = JSON.parse(result.stdout);
    if (published !== floor) {
      throw new Error(`registry returned ${JSON.stringify(published)}`);
    }
    return published;
  } catch (error) {
    throw new Error(
      `could not verify ${packageName}@${floor}: ${error.message}`,
    );
  }
}

export function assertPluginCliFloor(
  manifestPath = defaultManifest,
  npmCommand = 'npm',
  releaseVersion,
) {
  const floor = pluginCliFloor(manifestPath);
  if (releaseVersion !== undefined) {
    const parsedRelease = parseSemVer(releaseVersion);
    if (!parsedRelease) {
      throw new Error(
        `release version must be semantic, got ${JSON.stringify(releaseVersion)}`,
      );
    }
    const parsedFloor = parseSemVer(floor);
    if (compareSemVer(parsedRelease, parsedFloor) >= 0) return floor;
  }
  publishedCliVersion(floor, npmCommand);
  return floor;
}

function optionValue(name) {
  const index = process.argv.indexOf(name);
  if (index === -1) return undefined;
  const value = process.argv[index + 1];
  if (!value || value.startsWith('--')) {
    throw new Error(`${name} requires a value`);
  }
  return value;
}

if (
  process.argv[1] &&
  fileURLToPath(import.meta.url) === resolve(process.argv[1])
) {
  try {
    const releaseVersion = optionValue('--release-version');
    const floor = assertPluginCliFloor(
      defaultManifest,
      'npm',
      releaseVersion,
    );
    console.log(`plugin CLI floor is satisfiable: ${packageName}@${floor}`);
  } catch (error) {
    if (
      process.argv.includes('--allow-inconclusive') &&
      error instanceof RegistryInconclusiveError
    ) {
      console.warn(`plugin CLI floor assertion inconclusive: ${error.message}`);
    } else {
      console.error(`plugin CLI floor assertion failed: ${error.message}`);
      process.exitCode = 1;
    }
  }
}
