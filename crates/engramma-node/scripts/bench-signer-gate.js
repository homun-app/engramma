// SPDX-License-Identifier: Apache-2.0

// v0.6.4 (D6.1): what the write-time signer gate costs where it lives —
// the Node binding. 10 000 puts on a root WITHOUT devices.db (the
// insert bench's roots), with the gate (v0.6.3) and without
// (ENGRAMMA_BENCH_NO_SIGNER_GATE, a bench-only variable). Numbers in
// the REPORT.

const fs = require('node:fs');
const path = require('node:path');
const { Engine } = require('..');

async function run(label) {
  const root = fs.mkdtempSync(path.join(require('node:os').tmpdir(), 'gate-'));
  const engine = await Engine.open({ root, embedder: 'none', actor: 'user' });
  const cell = await engine.cell('bench');
  const started = Date.now();
  for (let i = 0; i < 10_000; i++) {
    await cell.put({
      body: `nota ${i} del banco del cancello: qualche parola di corpo`,
      source: 'bench',
      confidence: 'stated',
    });
  }
  const elapsed = Date.now() - started;
  await engine.close();
  fs.rmSync(root, { recursive: true, force: true });
  const per = elapsed * 1000 / 10_000;
  console.log(`${label}: ${elapsed} ms totale, ${per.toFixed(1)} µs per put`);
  return per;
}

(async () => {
  const gated = await run('con cancello (v0.6.3)');
  process.env.ENGRAMMA_BENCH_NO_SIGNER_GATE = '1';
  const naked = await run('senza cancello');
  const delta = 100 * (gated - naked) / naked;
  console.log(`differenza: ${delta.toFixed(2)}% (soglia +2%)`);
})();
