#!/usr/bin/env node

const { execFileSync } = require('node:child_process');
const { dirname, join } = require('node:path');

class LauncherError extends Error {
  constructor(message) {
    super(message);
    this.name = 'LauncherError';
  }
}

function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const manifest = require('./package.json');
  const packages = manifest.ostrom?.platformPackages ?? {};
  const platformPackage = packages[key];

  if (!platformPackage) {
    const supported = Object.keys(packages)
      .map((platform) => `  - ${platform}`)
      .join('\n');
    throw new LauncherError(
      `ostrom: unsupported platform "${key}".\n\n` +
        `Prebuilt binaries are published for:\n${supported}`,
    );
  }

  try {
    const manifestPath = require.resolve(`${platformPackage}/package.json`);
    const platformManifest = require(manifestPath);
    return join(dirname(manifestPath), platformManifest.main);
  } catch {
    throw new LauncherError(
      `ostrom: the platform package "${platformPackage}" for ${key} is not installed.\n\n` +
        'Optional dependencies may have been disabled. Reinstall without ' +
        '--no-optional or --omit=optional.',
    );
  }
}

function main() {
  const binary = resolveBinary();
  try {
    execFileSync(binary, process.argv.slice(2), { stdio: 'inherit' });
  } catch (error) {
    if (error && typeof error.status === 'number') {
      process.exit(error.status);
    }
    throw new LauncherError(
      `ostrom: failed to execute ${binary}: ${error?.message ?? error}`,
    );
  }
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    if (error instanceof LauncherError) {
      process.stderr.write(`${error.message}\n`);
      process.exitCode = 1;
    } else {
      throw error;
    }
  }
}

module.exports = { LauncherError, main, resolveBinary };
