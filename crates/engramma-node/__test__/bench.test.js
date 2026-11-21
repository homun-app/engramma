// SPDX-License-Identifier: Apache-2.0

// v0.6.11 (K3): the bench cleans after itself, and every test file works
// in its own NEW folder even inside a reused ENGRAMMA_TEST_TMP — the two
// rules the v0.6.10 verification wrote (147 `engramma-node-*` folders,
// 4.3 GB; and four red tests when `npm test` and `test:electron` shared
// one folder). The judge here is OUTSIDE the bench: these tests spawn a
// real run of a bench file as a child and look at what $TMPDIR is left
// holding.

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

// our own bench folder (the helper's rules apply to this file too)
const TMP = require('./tmpdir.js').testTmp('bench');

const engrammaNodeDirs = () =>
  fs
    .readdirSync(os.tmpdir())
    .filter((n) => n.startsWith('engramma-node-'))
    .map((n) => path.join(os.tmpdir(), n));

/** One run of a bench file, as a child of the current executable (Node
 * under `npm test`, Homun's Electron under `test:electron`). */
function runBench(file, env = {}) {
  return new Promise((resolve, reject) => {
    // a run the judge can see: no inherited bench folder, no KEEP, and
    // none of the runner's own context (a child that inherits it believes
    // it is a recursive run() and skips everything)
    const childEnv = { ...process.env };
    delete childEnv.ENGRAMMA_TEST_TMP;
    delete childEnv.ENGRAMMA_TEST_KEEP;
    delete childEnv.NODE_TEST_CONTEXT;
    const child = spawn(process.execPath, ['--test', path.join(__dirname, file)], {
      cwd: __dirname,
      env: { ...childEnv, ...env },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (out += d));
    child.on('error', reject);
    child.on('close', (code) => resolve({ code, out }));
  });
}

test('a bench run leaves no engramma-node-* behind, failures included', async () => {
  const before = engrammaNodeDirs();
  const run = await runBench('engine.test.js');
  assert.equal(run.code, 0, `the bench run itself must be green:\n${run.out}`);
  const after = engrammaNodeDirs();
  assert.deepEqual(
    after,
    before,
    `engramma-node-* left behind: ${after.filter((d) => !before.includes(d)).join(', ')}`,
  );

  // ENGRAMMA_TEST_KEEP=1 is the declared escape: the folder STAYS and
  // the bench prints where it is
  const kept = await runBench('engine.test.js', { ENGRAMMA_TEST_KEEP: '1' });
  assert.equal(kept.code, 0, `the KEEP run must be green:\n${kept.out}`);
  assert.match(kept.out, /ENGRAMMA_TEST_KEEP=1: .*engramma-node-[^\s]+ resta/);
  const grown = engrammaNodeDirs().filter((d) => !before.includes(d));
  assert.ok(grown.length >= 1, 'KEEP leaves the folder behind');
  // the judge cleans what it asked the bench to keep
  for (const dir of grown) fs.rmSync(dir, { recursive: true, force: true });
});

test('two runs in a row on the same ENGRAMMA_TEST_TMP are both green', async () => {
  const shared = fs.mkdtempSync(path.join(os.tmpdir(), 'engramma-k3-'));
  try {
    for (const run of [1, 2]) {
      const result = await runBench('engine.test.js', {
        ENGRAMMA_TEST_TMP: shared,
      });
      assert.equal(
        result.code,
        0,
        `run ${run} on the same ENGRAMMA_TEST_TMP must be green:\n${result.out}`,
      );
    }
    // and the shared folder holds no bench leftovers either
    const leftovers = fs.readdirSync(shared);
    assert.deepEqual(
      leftovers,
      [],
      `leftovers in the shared ENGRAMMA_TEST_TMP: ${leftovers.join(', ')}`,
    );
  } finally {
    fs.rmSync(shared, { recursive: true, force: true });
  }
});
