import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import {
  chmodSync,
  copyFileSync,
  existsSync,
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
import {
  assertPluginCliFloor,
  pluginCliFloor,
} from '../scripts/assert-plugin-cli-floor.mjs';

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

test('the plugin CLI floor assertion accepts an exact published version', () => {
  const manifest = join(fixture, 'plugin.json');
  const npm = join(fixture, 'npm-published');
  writeFileSync(manifest, '{"minimumCliVersion":"0.2.2"}\n');
  writeFileSync(npm, "#!/bin/sh\nprintf '\"0.2.2\"\\n'\n");
  chmodSync(npm, 0o755);

  assert.equal(pluginCliFloor(manifest), '0.2.2');
  assert.equal(assertPluginCliFloor(manifest, npm), '0.2.2');
});

test('the release assertion accepts a floor equal to the version being released', () => {
  const manifest = join(fixture, 'plugin-release.json');
  const npm = join(fixture, 'npm-release');
  const marker = join(fixture, 'release-npm-was-run');
  writeFileSync(manifest, '{"minimumCliVersion":"0.3.0"}\n');
  writeFileSync(npm, `#!/bin/sh\ntouch '${marker}'\nexit 1\n`);
  chmodSync(npm, 0o755);

  assert.equal(assertPluginCliFloor(manifest, npm, '0.3.0'), '0.3.0');
  assert.equal(existsSync(marker), false);
});

test('the plugin CLI floor assertion rejects a definitely absent version', () => {
  const manifest = join(fixture, 'plugin-unpublished.json');
  const npm = join(fixture, 'npm-unpublished');
  writeFileSync(manifest, '{"minimumCliVersion":"9.9.9"}\n');
  writeFileSync(
    npm,
    "#!/bin/sh\nprintf 'npm error code E404\\nnpm error 404 Not Found\\n' >&2\nexit 1\n",
  );
  chmodSync(npm, 0o755);

  assert.throws(
    () => assertPluginCliFloor(manifest, npm),
    (error) => {
      assert.match(error.message, /@ostrom\/cli@9\.9\.9 is not published/);
      assert.doesNotMatch(error.message, /inconclusive/i);
      return true;
    },
  );
});

test('a registry network error is inconclusive, not an absent version', () => {
  const manifest = join(fixture, 'plugin-network.json');
  const npm = join(fixture, 'npm-network');
  writeFileSync(manifest, '{"minimumCliVersion":"9.9.8"}\n');
  writeFileSync(
    npm,
    "#!/bin/sh\nprintf 'npm error code ENOTFOUND\\nnpm error network registry unavailable\\n' >&2\nexit 1\n",
  );
  chmodSync(npm, 0o755);

  assert.throws(
    () => assertPluginCliFloor(manifest, npm),
    (error) => {
      assert.match(error.message, /inconclusive/i);
      assert.doesNotMatch(error.message, /is not published/);
      return true;
    },
  );
});
