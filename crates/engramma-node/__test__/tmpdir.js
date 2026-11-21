// SPDX-License-Identifier: Apache-2.0

// v0.6.11 (K3): the bench cleans after itself. The v0.6.10 verification
// found 147 `engramma-node-*` folders (4.3 GB) in $TMPDIR: only one of
// the three test files removed its own, and none did when
// ENGRAMMA_TEST_TMP was set. The rules now:
// - every test file works in ITS OWN NEW subfolder — inside a reused
//   ENGRAMMA_TEST_TMP too (the verification met four red tests reusing
//   one folder between `npm test` and `test:electron`);
// - the run removes its folder at the end, test failures included (the
//   `exit` hook fires on every exit path but a kill);
// - ENGRAMMA_TEST_KEEP=1 keeps the folders AND prints where they are,
//   for the debug.

'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

/** The bench folder of one test file. */
function testTmp(label) {
  const envBase = process.env.ENGRAMMA_TEST_TMP;
  // the base is ours only when we minted it: a reused ENGRAMMA_TEST_TMP
  // belongs to whoever set it
  const base = envBase
    ? envBase
    : fs.mkdtempSync(path.join(os.tmpdir(), 'engramma-node-'));
  const dir = fs.mkdtempSync(path.join(base, `${label}-`));
  const keep = process.env.ENGRAMMA_TEST_KEEP === '1';
  const clean = () => {
    if (keep) {
      console.error(`[engramma-bench] ENGRAMMA_TEST_KEEP=1: ${dir} resta`);
      if (!envBase) {
        console.error(`[engramma-bench] ENGRAMMA_TEST_KEEP=1: ${base} resta`);
      }
      return;
    }
    fs.rmSync(dir, { recursive: true, force: true });
    if (!envBase) {
      // the base we minted holds only our subfolder: it goes too
      fs.rmSync(base, { recursive: true, force: true });
    }
  };
  process.on('exit', clean);
  return dir;
}

module.exports = { testTmp };
