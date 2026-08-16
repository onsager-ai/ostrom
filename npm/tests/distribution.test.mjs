import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import {
  copyFileSync,
  mkdtempSync,
  mkdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import {
  ROOT,
  cargoVersion,
  config,
  platformPackageName,
} from '../scripts/lib.mjs';
import { assertVersion } from '../scripts/assert-version.mjs';

const testRoot = join(ROOT, 'target');
mkdirSync(testRoot, { recursive: true });
const fixture = mkdtempSync(join(testRoot, 'ostrom-npm-test-'));
const staging = join(fixture, 'packages');

before(() => {
  const mainRoot = join(staging, config.mainPackage.name);
  mkdirSync(mainRoot, { recursive: true });
  copyFileSync(
    join(ROOT, config.mainPackage.dir, 'bin.js'),
    join(mainRoot, 'bin.js'),
  );
  writeFileSync(
    join(mainRoot, 'package.json'),
    JSON.stringify({
      ostrom: {
        platformPackages: Object.fromEntries(
          config.platforms.map((platform) => [
            platform.nodeKey,
            platformPackageName(platform),
          ]),
        ),
      },
    }),
  );
});

after(() => rmSync(fixture, { recursive: true, force: true }));

test('config models one binary and all five release platforms', () => {
  assert.deepEqual(config.binaryNames, ['ostrom']);
  assert.deepEqual(
    config.platforms.map(({ platform }) => platform),
    [
      'linux-x64',
      'linux-arm64',
      'darwin-x64',
      'darwin-arm64',
      'windows-x64',
    ],
  );
});

test('unsupported platform reports the platform key before module resolution', () => {
  const launcher = join(staging, config.mainPackage.name, 'bin.js');
  const require = createRequire(import.meta.url);
  const { resolveBinary } = require(launcher);
  const platform = Object.getOwnPropertyDescriptor(process, 'platform');
  const arch = Object.getOwnPropertyDescriptor(process, 'arch');
  try {
    Object.defineProperty(process, 'platform', {
      configurable: true,
      value: 'freebsd',
    });
    Object.defineProperty(process, 'arch', {
      configurable: true,
      value: 'riscv64',
    });
    assert.throws(
      () => resolveBinary(),
      (error) => {
        assert.match(error.message, /unsupported platform "freebsd-riscv64"/);
        assert.doesNotMatch(error.message, /Cannot find module|MODULE_NOT_FOUND/);
        return true;
      },
    );
  } finally {
    Object.defineProperty(process, 'platform', platform);
    Object.defineProperty(process, 'arch', arch);
  }
});

test('a binary/package version mismatch fails the release assertion', () => {
  assert.throws(
    () => assertVersion('ostrom 9.9.9\n', cargoVersion()),
    /version mismatch: binary reports 9\.9\.9/,
  );
});
