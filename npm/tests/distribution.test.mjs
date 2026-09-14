import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import {
  copyFileSync,
  mkdtempSync,
  mkdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { execFileSync } from 'node:child_process';
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

// publish.mjs runs its side effects behind an entry guard so this file can
// import the decision without spawning npm. That guard is the one place where
// a mistake would be silent: a false comparison makes the release's publish
// step do nothing and still exit 0. Spawning the script proves the guard's
// other branch still fires, offline and without reaching npm — an unknown
// --target throws before any package is touched.
//
// It spawns the script the way release.yml does — `node npm/scripts/publish.mjs`
// from the repository root — so the invocation under test is the one that has
// to work. Node hands back an already-absolute process.argv[1] either way, so
// the guard's resolve() is defensive rather than load-bearing; what this test
// actually pins is that the comparison still selects the script body.
test('publish.mjs runs its script body when invoked the way release.yml does', () => {
  assert.throws(
    () =>
      execFileSync(
        process.execPath,
        ['npm/scripts/publish.mjs', '--target', 'bogus'],
        { cwd: ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] },
      ),
    (error) => {
      assert.notEqual(
        error.status,
        0,
        'publish.mjs exited 0 for an unknown --target; its entry guard did not fire',
      );
      assert.match(error.stderr, /unknown --target bogus/);
      return true;
    },
  );
});
