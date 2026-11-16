// SPDX-License-Identifier: Apache-2.0

// v0.6.3 (C3): the caller side of `npm run test:electron-main`. It spawns
// the Electron main-process test, MEASURES the whole run and fails beyond
// 10 s: `app.quit()` must end the process, and the bound is the proof —
// the child cannot grade its own exit.

const { spawn } = require('node:child_process');

const electron =
  process.env.HOMUN_ELECTRON ||
  'node_modules/electron/dist/Electron.app/Contents/MacOS/Electron';
const BOUND_MS = 10_000;

const started = Date.now();
const child = spawn(electron, [require('node:path').join(__dirname, '..', '__test__', 'electron-main.js')], {
  stdio: 'inherit',
});

const killer = setTimeout(() => {
  console.error(`electron-main: oltre ${BOUND_MS / 1000} s — ucciso`);
  child.kill('SIGKILL');
}, BOUND_MS);

child.on('exit', (code, signal) => {
  clearTimeout(killer);
  const elapsed = Date.now() - started;
  console.log(`electron-main: uscita ${signal ?? code} in ${elapsed} ms`);
  if (signal) {
    process.exit(124);
  }
  process.exit(code ?? 1);
});

child.on('error', (err) => {
  clearTimeout(killer);
  console.error('electron-main: Electron non parte —', err.message);
  process.exit(1);
});
