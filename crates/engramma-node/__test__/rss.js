// SPDX-License-Identifier: Apache-2.0

// v0.4.4 (P1) → v0.4.5 (P2): the RSS profile of the builtin embedder
// inside the real addon. Stages: engine open without the model, model
// loaded, after embedPending of 64 notes (half ~600 tokens), after 2 s
// and after 10 s; then vmmap (macOS) / /proc/self/status (Linux) for
// allocated-vs-resident at the end. It is NOT a node:test file:
// `node __test__/rss.js [cellsRoot]` (a fresh temp dir by default).
//
// v0.4.5 (P2): a single run no longer says anything — the RSS after 10 s
// swings by more than 1 GB from run to run on darwin 27. `--matrix N`
// spawns N SINGLE-RUN child Node processes (sequentially: parallel runs
// would read each other's memory pressure) and prints median/min/max of
// every stage plus the PEAK (`process.resourceUsage().maxRSS`).
//
// Usage: node __test__/rss.js [--matrix N] [cellsRoot]

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
const rssMib = () => Math.round(rss() / MB);

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

// ---- the single run ------------------------------------------------------

// Returns the measurement; `quiet` (the --matrix child) prints only the
// JSON line on stdout and the human lines on stderr.
const singleRun = async (quiet, root) => {
  fs.rmSync(root, { recursive: true, force: true });
  fs.mkdirSync(root, { recursive: true });
  const say = quiet ? (...a) => console.error(...a) : (...a) => console.log(...a);

  const bare = await Engine.open({ root, embedder: 'none', actor: 'rss' });
  const bareMb = rssMib();
  say(`motore aperto senza modello ${String(bareMb).padStart(12)} MB`);
  await bare.close();

  const e = await Engine.open({ root, embedder: 'builtin', actor: 'rss' });
  const loadMb = rssMib();
  say(`modello caricato ${String(loadMb).padStart(21)} MB`);

  const fill = async (name, base) => {
    const cell = await e.cell(name);
    for (let i = 0; i < 64; i++) {
      await cell.put({
        body: i % 2 === 0 ? longBody(base + i) : `nota corta ${name}-${i}: una cosa da ricordare.`,
        source: 'rss',
        confidence: 'stated',
      });
    }
    return cell;
  };

  await fill('rss', 0);
  await (await e.cell('rss')).embedPending(64);
  const workMb = rssMib();
  say(`dopo embedPending di 64 note ${String(workMb).padStart(11)} MB`);

  const second = await fill('rss2', 10_000);
  const t0 = process.hrtime.bigint();
  await second.embedPending(64);
  const embedMs = Number(process.hrtime.bigint() - t0) / 1e6;
  say(`embedPending di 64 note (2ª cella) ${embedMs.toFixed(0).padStart(10)} ms`);

  await new Promise((r) => setTimeout(r, 2000));
  const twoMb = rssMib();
  say(`dopo 2 s ${String(twoMb).padStart(30)} MB`);
  await new Promise((r) => setTimeout(r, 8000));
  const tenMb = rssMib();
  say(`dopo 10 s ${String(tenMb).padStart(29)} MB`);

  const peakMb = Math.round(process.resourceUsage().maxRSS / 1024);
  await e.close();
  if (quiet) {
    fs.rmSync(root, { recursive: true, force: true });
  }
  return { bareMb, loadMb, workMb, twoMb, tenMb, peakMb, embedMs };
};

// ---- the human single run (the v0.4.4 behaviour, vmmap included) --------

const human = async (root) => {
  const m = await singleRun(false, root);
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
  console.log(`picco (maxRSS) ${String(m.peakMb).padStart(29)} MB`);
};

// ---- the --matrix N parent: N children in separate processes ------------

const median = (xs) => {
  const v = [...xs].sort((a, b) => a - b);
  const n = v.length;
  return n % 2 ? v[(n - 1) / 2] : (v[n / 2 - 1] + v[n / 2]) / 2;
};

const matrix = (runs) => {
  const rows = [];
  for (let i = 1; i <= runs; i++) {
    const child = spawnSync(process.execPath, [__filename, '--child'], { encoding: 'utf8' });
    if (child.status !== 0) {
      console.error(`figlio ${i} fallito:\n${child.stderr}`);
      process.exit(1);
    }
    const jsonLine = child.stdout.split('\n').find((l) => l.trim().startsWith('{'));
    if (!jsonLine) {
      console.error(`figlio ${i}: nessuna riga JSON:\n${child.stdout}`);
      process.exit(1);
    }
    const m = JSON.parse(jsonLine);
    console.log(
      `corsa ${i}/${runs}: carico ${m.loadMb} MB | lavoro ${m.workMb} MB | +10 s ${m.tenMb} MB | picco ${m.peakMb} MB | embed ${m.embedMs.toFixed(0)} ms`
    );
    rows.push(m);
  }
  const cols = [
    ['carico (MB)', 'loadMb'],
    ['dopo lavoro (MB)', 'workMb'],
    ['+10 s (MB)', 'tenMb'],
    ['picco (MB)', 'peakMb'],
    ['embed (ms)', 'embedMs'],
  ];
  console.log(`# ${runs} corse in processi Node separati — mediana / min / max`);
  for (const [name, key] of cols) {
    const xs = rows.map((r) => r[key]);
    const min = Math.min(...xs);
    const max = Math.max(...xs);
    console.log(
      `${name.padEnd(20)} ${String(median(xs)).padStart(8)} / ${min.toFixed(1)} / ${max.toFixed(1)}`
    );
  }
};

// ---- entry ---------------------------------------------------------------

const argv = process.argv.slice(2);
const isChild = argv.includes('--child');
let runs = 5;
let rootArg;
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === '--matrix') {
    runs = Number(argv[i + 1]) || 5;
    i++;
  } else if (argv[i] !== '--child') {
    rootArg = argv[i];
  }
}

if (argv.includes('--matrix')) {
  matrix(runs);
} else {
  const root = rootArg || fs.mkdtempSync(path.join(os.tmpdir(), 'engramma-rss-'));
  const main = isChild
    ? async () => {
        const m = await singleRun(true, root);
        console.log(JSON.stringify(m));
      }
    : () => human(root);
  main().catch((err) => {
    console.error(err);
    process.exitCode = 1;
  });
}
