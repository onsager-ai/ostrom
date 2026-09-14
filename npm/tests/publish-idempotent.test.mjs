// Tests for #585: a release run, or a rerun of it at any time, must end
// green whenever the registry actually holds the right bytes, and fail
// loudly whenever it does not. Everything here is offline — `view`, `sleep`,
// `now`, and the publisher are injected fakes; the only real subprocesses
// spawned are stage-packages.mjs and wait-for-platforms.mjs themselves,
// exercised through their own offline entry guards.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from 'node:fs';
import { join } from 'node:path';
import { after, test } from 'node:test';
import {
  ROOT,
  decidePublishAction,
  packageDirs,
  registryState,
  viewArgs,
} from '../scripts/lib.mjs';
import { publishPackage } from '../scripts/publish.mjs';
import { waitForPackages } from '../scripts/wait-for-platforms.mjs';

const testRoot = join(ROOT, 'target');
mkdirSync(testRoot, { recursive: true });
const fixture = mkdtempSync(join(testRoot, 'ostrom-npm-idempotent-test-'));
after(() => rmSync(fixture, { recursive: true, force: true }));

const SHA_A = 'a'.repeat(40);
const SHA_B = 'b'.repeat(40);
const noopSleep = async () => {};

// --- lib.mjs: viewArgs -------------------------------------------------

test('the default registry view bypasses npm\'s local packument cache', () => {
  // npm serves a still-fresh cached packument for a few minutes (its own
  // `max-age`); without --prefer-online a wait loop or a post-failure
  // re-read would keep seeing "No match found" for a version that is
  // already live. An offline test cannot demonstrate that staleness
  // directly, so it pins the flag instead.
  assert.ok(viewArgs('@ostrom/cli', '1.2.3').includes('--prefer-online'));
});

// --- lib.mjs: registryState ---------------------------------------------

test('registryState reports absent only for an E404 view result', async () => {
  const result = await registryState('@ostrom/cli', '9.9.9', {
    view: () => ({
      error: { code: 'E404', summary: 'No match found for version 9.9.9' },
    }),
    sleep: noopSleep,
  });
  assert.deepEqual(result, { state: 'absent' });
});

test('registryState reports present with the registry\'s integrity and gitHead', async () => {
  const result = await registryState('@ostrom/cli-linux-x64', '0.14.0', {
    view: () => ({
      version: '0.14.0',
      'dist.integrity': 'sha512-abc',
      gitHead: SHA_A,
    }),
    sleep: noopSleep,
  });
  assert.deepEqual(result, {
    state: 'present',
    integrity: 'sha512-abc',
    gitHead: SHA_A,
  });
});

test('registryState retries a transient failure and returns present once it clears', async () => {
  let calls = 0;
  const sleeps = [];
  const result = await registryState('@ostrom/cli', '1.0.0', {
    view: () => {
      calls += 1;
      if (calls === 1) throw new Error('ECONNRESET');
      return { version: '1.0.0', 'dist.integrity': 'sha512-xyz', gitHead: null };
    },
    sleep: async (ms) => sleeps.push(ms),
  });
  assert.equal(result.state, 'present');
  assert.equal(calls, 2, 'the transient failure should have been retried exactly once');
  assert.equal(sleeps.length, 1, 'a retry happened, so the backoff sleep must have been observed');
});

test('registryState throws — and never returns absent — after persistent non-404 failures', async () => {
  let calls = 0;
  await assert.rejects(
    () =>
      registryState('@ostrom/cli', '1.0.0', {
        view: () => {
          calls += 1;
          throw new Error('registry unavailable');
        },
        sleep: noopSleep,
        retries: 2,
      }),
    (error) => {
      assert.match(error.message, /@ostrom\/cli@1\.0\.0/);
      assert.match(error.message, /registry unavailable/);
      return true;
    },
  );
  assert.equal(calls, 3, 'one initial attempt plus 2 retries');
});

test('registryState throws — and never returns absent — when npm reports a non-404 error as JSON', async () => {
  // In production, defaultView returns npm's error JSON from `error.stdout` as
  // a value, not a thrown error. A real E401, E403, E500 or ETIMEDOUT arrives
  // here as data, so this is the path an auth or registry failure actually
  // takes. Only E404 may mean absent.
  let calls = 0;
  await assert.rejects(
    () =>
      registryState('@ostrom/cli', '1.0.0', {
        view: () => {
          calls += 1;
          return { error: { code: 'E403', summary: 'Forbidden' } };
        },
        sleep: noopSleep,
        retries: 2,
      }),
    (error) => {
      assert.match(error.message, /@ostrom\/cli@1\.0\.0/);
      assert.match(error.message, /E403/);
      return true;
    },
  );
  assert.equal(calls, 3, 'one initial attempt plus 2 retries, then a throw');
});

// --- lib.mjs: decidePublishAction ---------------------------------------

test('decidePublishAction publishes an absent package', () => {
  assert.deepEqual(
    decidePublishAction(
      { state: 'absent' },
      { name: '@ostrom/cli', version: '1.0.0', integrity: 'sha512-a' },
    ),
    { action: 'publish' },
  );
});

test('decidePublishAction skips a package already published with identical bytes', () => {
  assert.deepEqual(
    decidePublishAction(
      { state: 'present', integrity: 'sha512-a', gitHead: SHA_A },
      { name: '@ostrom/cli', version: '1.0.0', integrity: 'sha512-a', gitHead: SHA_A },
    ),
    {
      action: 'skip',
      message: 'already published with identical bytes: @ostrom/cli@1.0.0',
    },
  );
});

test('decidePublishAction throws on differing integrity without --rebuild-of-tag, naming both integrities', () => {
  assert.throws(
    () =>
      decidePublishAction(
        { state: 'present', integrity: 'sha512-registry', gitHead: SHA_A },
        { name: '@ostrom/cli', version: '1.0.0', integrity: 'sha512-local', gitHead: SHA_A },
      ),
    (error) => {
      assert.equal(error.code, 'REGISTRY_CONFLICT');
      assert.match(error.message, /sha512-registry/);
      assert.match(error.message, /sha512-local/);
      return true;
    },
  );
});

test('decidePublishAction skips a rebuild-of-tag with matching gitHead despite differing bytes', () => {
  assert.deepEqual(
    decidePublishAction(
      { state: 'present', integrity: 'sha512-registry', gitHead: SHA_A },
      {
        name: '@ostrom/cli',
        version: '1.0.0',
        integrity: 'sha512-local',
        gitHead: SHA_A,
        rebuildOfTag: true,
      },
    ),
    {
      action: 'skip',
      message: `already published from ${SHA_A} by an earlier build: @ostrom/cli@1.0.0`,
    },
  );
});

test('decidePublishAction throws a rebuild-of-tag with a different gitHead too', () => {
  assert.throws(
    () =>
      decidePublishAction(
        { state: 'present', integrity: 'sha512-registry', gitHead: SHA_A },
        {
          name: '@ostrom/cli',
          version: '1.0.0',
          integrity: 'sha512-local',
          gitHead: SHA_B,
          rebuildOfTag: true,
        },
      ),
    (error) => {
      assert.equal(error.code, 'REGISTRY_CONFLICT');
      return true;
    },
  );
});

// --- publish.mjs: publishPackage recovery --------------------------------

test('publishPackage recovers when npm reports failure but the registry already holds the right bytes', async () => {
  let stateCalls = 0;
  const result = await publishPackage({
    name: '@ostrom/cli-linux-x64',
    version: '0.14.0',
    tarballPath: '/tarballs/cli-linux-x64.tgz',
    integrity: 'sha512-local',
    gitHead: SHA_A,
    tag: 'latest',
    publish: () => {
      throw new Error('network hiccup talking to the registry');
    },
    state: async () => {
      stateCalls += 1;
      // Call 1 is the pre-publish check (absent, so publishPackage attempts
      // to publish). Call 2 is the post-failure recovery re-read.
      if (stateCalls === 1) return { state: 'absent' };
      return { state: 'present', integrity: 'sha512-local', gitHead: SHA_A };
    },
    sleep: noopSleep,
    now: () => 0,
    recoveryTimeoutMs: 60000,
  });
  assert.equal(result.action, 'published-after-recovery');
});

test('publishPackage throws the registry conflict when npm fails and the registry holds different bytes', async () => {
  let stateCalls = 0;
  await assert.rejects(
    () =>
      publishPackage({
        name: '@ostrom/cli-linux-x64',
        version: '0.14.0',
        tarballPath: '/tarballs/cli-linux-x64.tgz',
        integrity: 'sha512-local',
        gitHead: SHA_A,
        tag: 'latest',
        publish: () => {
          throw new Error('network hiccup talking to the registry');
        },
        state: async () => {
          stateCalls += 1;
          if (stateCalls === 1) return { state: 'absent' };
          return { state: 'present', integrity: 'sha512-registry', gitHead: SHA_B };
        },
        sleep: noopSleep,
        now: () => 0,
        recoveryTimeoutMs: 60000,
      }),
    (error) => {
      assert.equal(error.code, 'REGISTRY_CONFLICT');
      return true;
    },
  );
});

test('publishPackage rethrows the original npm error when the registry never confirms the package', async () => {
  let clock = 0;
  await assert.rejects(
    () =>
      publishPackage({
        name: '@ostrom/cli-linux-x64',
        version: '0.14.0',
        tarballPath: '/tarballs/cli-linux-x64.tgz',
        integrity: 'sha512-local',
        gitHead: SHA_A,
        tag: 'latest',
        publish: () => {
          throw new Error('ECONNRESET talking to registry.npmjs.org');
        },
        state: async () => ({ state: 'absent' }),
        sleep: async (ms) => {
          clock += ms;
        },
        now: () => clock,
        recoveryTimeoutMs: 20000,
      }),
    (error) => {
      assert.match(error.message, /ECONNRESET talking to registry\.npmjs\.org/);
      assert.match(error.message, /@ostrom\/cli-linux-x64@0\.14\.0/);
      return true;
    },
  );
});

// --- wait-for-platforms.mjs: waitForPackages -----------------------------

test('waitForPackages resolves once a package appears after N polls', async () => {
  let calls = 0;
  const sleeps = [];
  const result = await waitForPackages({
    packages: [{ name: '@ostrom/cli-linux-arm64', version: '0.14.0', integrity: 'sha512-a' }],
    expected: {},
    state: async () => {
      calls += 1;
      if (calls < 3) return { state: 'absent' };
      return { state: 'present', integrity: 'sha512-a', gitHead: null };
    },
    sleep: async (ms) => sleeps.push(ms),
    now: () => 0,
  });
  assert.deepEqual(result.satisfied, ['@ostrom/cli-linux-arm64']);
  assert.equal(calls, 3);
  assert.equal(sleeps.length, 2);
});

test('waitForPackages throws after the time budget, naming the package and the recovery commands', async () => {
  let clock = 0;
  await assert.rejects(
    () =>
      waitForPackages({
        packages: [{ name: '@ostrom/cli-linux-arm64', version: '0.14.0', integrity: 'sha512-a' }],
        expected: {},
        state: async () => ({ state: 'absent' }),
        sleep: async (ms) => {
          clock += ms;
        },
        now: () => clock,
        timeoutMs: 20000,
        initialDelayMs: 5000,
        maxDelayMs: 60000,
        tag: 'v0.14.0',
      }),
    (error) => {
      assert.equal(error.code, 'WAIT_TIMEOUT');
      assert.match(error.message, /@ostrom\/cli-linux-arm64/);
      assert.match(error.message, /--failed/);
      assert.match(error.message, /tag=v0\.14\.0/);
      return true;
    },
  );
});

test('waitForPackages fails immediately on a present-but-different package, without sleeping to the budget', async () => {
  let sleepCalls = 0;
  await assert.rejects(
    () =>
      waitForPackages({
        packages: [{ name: '@ostrom/cli-linux-arm64', version: '0.14.0', integrity: 'sha512-local' }],
        expected: {},
        state: async () => ({ state: 'present', integrity: 'sha512-registry', gitHead: null }),
        sleep: async () => {
          sleepCalls += 1;
        },
        now: () => 0,
        timeoutMs: 45 * 60 * 1000,
      }),
    (error) => {
      assert.equal(error.code, 'REGISTRY_CONFLICT');
      return true;
    },
  );
  assert.equal(sleepCalls, 0, 'a real conflict must not wait out any part of the budget');
});

test('waitForPackages backoff delays grow and cap', async () => {
  const delays = [];
  let clock = 0;
  await assert.rejects(() =>
    waitForPackages({
      packages: [{ name: '@ostrom/cli-linux-arm64', version: '0.14.0', integrity: 'sha512-a' }],
      expected: {},
      state: async () => ({ state: 'absent' }),
      sleep: async (ms) => {
        delays.push(ms);
        clock += ms;
      },
      now: () => clock,
      timeoutMs: 200000,
      initialDelayMs: 1000,
      maxDelayMs: 4000,
    }),
  );
  assert.deepEqual(delays.slice(0, 4), [1000, 2000, 4000, 4000]);
});

test('waitForPackages keeps polling through an unclassifiable registry read instead of failing the run', async () => {
  let calls = 0;
  const result = await waitForPackages({
    packages: [{ name: '@ostrom/cli-linux-arm64', version: '0.14.0', integrity: 'sha512-a' }],
    expected: {},
    state: async () => {
      calls += 1;
      if (calls === 1) throw new Error('could not determine registry state: ETIMEDOUT');
      if (calls === 2) return { state: 'absent' };
      return { state: 'present', integrity: 'sha512-a', gitHead: null };
    },
    sleep: noopSleep,
    now: () => 0,
  });
  assert.deepEqual(result.satisfied, ['@ostrom/cli-linux-arm64']);
  assert.equal(calls, 3, 'an outage is a reason to keep waiting, not to stop');
});

test('waitForPackages reports the last unclassifiable read when the budget runs out', async () => {
  let clock = 0;
  await assert.rejects(
    () =>
      waitForPackages({
        packages: [{ name: '@ostrom/cli-linux-arm64', version: '0.14.0', integrity: 'sha512-a' }],
        expected: {},
        state: async () => {
          throw new Error('ETIMEDOUT reaching the registry');
        },
        sleep: async (ms) => {
          clock += ms;
        },
        now: () => clock,
        timeoutMs: 20000,
        tag: 'v0.14.0',
      }),
    (error) => {
      assert.equal(error.code, 'WAIT_TIMEOUT');
      assert.match(error.message, /@ostrom\/cli-linux-arm64/);
      assert.match(error.message, /ETIMEDOUT reaching the registry/);
      return true;
    },
  );
});

// --- stage-packages.mjs: gitHead identity ---------------------------------

test('stage-packages.mjs writes the given --git-head into every staged manifest', () => {
  const artifacts = join(fixture, 'git-head-artifacts');
  const output = join(fixture, 'git-head-packages');
  execFileSync(
    process.execPath,
    ['npm/scripts/create-test-artifacts.mjs', '--output', artifacts],
    { cwd: ROOT, stdio: 'ignore' },
  );
  execFileSync(
    process.execPath,
    [
      'npm/scripts/stage-packages.mjs',
      '--artifacts',
      artifacts,
      '--output',
      output,
      '--git-head',
      SHA_A,
    ],
    { cwd: ROOT, stdio: 'ignore' },
  );
  const manifests = packageDirs(output).map(
    (pkg) => JSON.parse(readFileSync(join(pkg.dir, 'package.json'), 'utf8')),
  );
  assert.ok(manifests.length > 0);
  for (const manifest of manifests) {
    assert.equal(manifest.gitHead, SHA_A);
  }
});

test('stage-packages.mjs refuses an invalid --git-head', () => {
  const artifacts = join(fixture, 'bad-git-head-artifacts');
  execFileSync(
    process.execPath,
    ['npm/scripts/create-test-artifacts.mjs', '--output', artifacts],
    { cwd: ROOT, stdio: 'ignore' },
  );
  assert.throws(
    () =>
      execFileSync(
        process.execPath,
        [
          'npm/scripts/stage-packages.mjs',
          '--artifacts',
          artifacts,
          '--output',
          join(fixture, 'bad-git-head-packages'),
          '--git-head',
          'not-a-commit-sha',
        ],
        { cwd: ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] },
      ),
    (error) => {
      assert.notEqual(error.status, 0);
      assert.match(error.stderr, /invalid git head/);
      return true;
    },
  );
});

// --- wait-for-platforms.mjs: entry guard ----------------------------------

// The equivalent of distribution.test.mjs's publish.mjs entry-guard test: an
// unrecognised/invalid flag has to fail loudly and offline, before this ever
// reaches the registry — proven by spawning the real script rather than
// only its imported functions.
test('wait-for-platforms.mjs refuses an invalid --timeout-ms offline', () => {
  assert.throws(
    () =>
      execFileSync(
        process.execPath,
        ['npm/scripts/wait-for-platforms.mjs', '--timeout-ms', 'not-a-number'],
        { cwd: ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] },
      ),
    (error) => {
      assert.notEqual(
        error.status,
        0,
        'wait-for-platforms.mjs exited 0 for an invalid --timeout-ms; its entry guard did not fire',
      );
      assert.match(error.stderr, /--timeout-ms must be a positive number/);
      return true;
    },
  );
});
