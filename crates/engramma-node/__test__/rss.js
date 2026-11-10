// SPDX-License-Identifier: Apache-2.0

// v0.4.4 (P1): the RSS profile of the builtin embedder inside the real
// addon — the measurement of the spec, redone as a script (it is NOT a
// node:test file: `node __test__/rss.js`). Stages: engine open without the
// model, model loaded, after embedPending of 64 notes (half ~600 tokens),
// after 2 s and after 10 s; then vmmap (macOS) / /proc/self/status (Linux)
// for allocated-vs-resident at the end.
//
// Usage: node __test__/rss.js [cellsRoot]   (a fresh temp dir by default)

'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { Engine } = require('..');

const MB = 1024 * 1024;
const rss = () => {
  if (process.platform === 'linux') {
    const status = fs.readFileSync('/proc/self/status', 'utf8');
    return Number(/VmRSS:\s+(\d+) kB/.exec(status)[1]) * 1024;
  }
  return process.memoryUsage().rss;
};
const line = (label) =>
  console.log(`${label.padEnd(34)} ${(rss() / MB).toFixed(0).padStart(6)} MB`);

// ~600 tokens of plausible memory text (≈ 0.75 words/token): distinct per
// note so the tokenizer cache never serves the same text twice.
const longBody = (i) => {
  const words = [
    'ricorda', 'milano', 'trasloco', 'lavoro', 'giulia', 'marco', 'progetto',
    'condominio', 'bici', 'treno', 'ricerca', 'memoria', 'quarantena',
    'istruzioni', 'riunione', 'martedì', 'novembre', 'preventivo', 'cucina',
    'affitto', 'lavori', 'cantina', 'soppalco', 'libreria', 'tramonto',
  ];
  const out = [];
  let s = i * 7919;
  for (let w = 0; w < 470; w++) {
    s = (s * 1103515245 + 12345) >>> 0;
    out.push(words[s % words.length]);
  }
  return `nota lunga ${i}: ${out.join(' ')}.`;
};

const root = process.argv[2] || fs.mkdtempSync(path.join(os.tmpdir(), 'engramma-rss-'));
fs.rmSync(root, { recursive: true, force: true });
fs.mkdirSync(root, { recursive: true });

const main = async () => {
  // 1. engine open, no model
  const bare = await Engine.open({ root, embedder: 'none', actor: 'rss' });
  line('motore aperto senza modello');
  await bare.close();

  // 2. model loaded (Granite 97M via tract)
  const e = await Engine.open({ root, embedder: 'builtin', actor: 'rss' });
  line('modello caricato');

  // 3. 64 notes, half long, then one embedPending
  const cell = await e.cell('rss');
  for (let i = 0; i < 64; i++) {
    await cell.put({
      body: i % 2 === 0 ? longBody(i) : `nota corta ${i}: una cosa da ricordare.`,
      source: 'rss',
      confidence: 'stated',
    });
  }
  await cell.embedPending(64);
  line('dopo embedPending di 64 note');
  const more = await e.cell('rss2');
  for (let i = 0; i < 64; i++) {
    await more.put({
      body: i % 2 === 0 ? longBody(10_000 + i) : `nota corta b${i}: un'altra cosa da ricordare.`,
      source: 'rss',
      confidence: 'stated',
    });
  }
  const t0 = process.hrtime.bigint();
  await more.embedPending(64);
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  console.log(`embedPending di 64 note (2ª cella) ${ms.toFixed(0).padStart(10)} ms`);

  // 4./5. what stays resident when the work is done
  await new Promise((r) => setTimeout(r, 2000));
  line('dopo 2 s');
  await new Promise((r) => setTimeout(r, 8000));
  line('dopo 10 s');

  // 6. allocated vs resident, from the OS
  if (process.platform === 'darwin') {
    const vm = spawnSync('vmmap', ['--summary', String(process.pid)], { encoding: 'utf8' });
    if (!vm.error) {
      for (const l of vm.stdout.split('\n')) {
        if (/footprint|malloc|__DATA|__TEXT|mapped file|shared|wired| Regions|Total/i.test(l)) {
          console.log(l.replace(/^ */, ''));
        }
      }
    }
  } else if (process.platform === 'linux') {
    const status = fs.readFileSync('/proc/self/status', 'utf8');
    for (const l of status.split('\n')) {
      if (/^(VmPeak|VmSize|VmRSS|VmHWM|VmData)/.test(l)) console.log(l);
    }
  }
  await e.close();
};

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
