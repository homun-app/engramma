// SPDX-License-Identifier: Apache-2.0

// v0.4 (P2.8): the JS contract of the point 7, on `node:test`. No extra
// dependencies. Text mode (`embedder: 'none'`) keeps the suite
// model-independent; `test:electron` runs the same file under Homun's
// Electron in Node mode.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { Engine, version } = require('..');

const TMP = process.env.ENGRAMMA_TEST_TMP
  ? process.env.ENGRAMMA_TEST_TMP
  : fs.mkdtempSync(path.join(require('node:os').tmpdir(), 'engramma-node-'));
const root = () => {
  const dir = path.join(TMP, `r-${Math.random().toString(36).slice(2, 8)}`);
  fs.rmSync(dir, { recursive: true, force: true });
  return dir;
};

async function engine(opts = {}) {
  return Engine.open({
    root: root(),
    embedder: 'none',
    actor: 'homun',
    ...opts,
  });
}

test('version is exported', () => {
  assert.match(version(), /^0\./);
});

test('put/get/search/patch/delete/purge, the whole cycle', async () => {
  const e = await engine();
  const cell = await e.cell('famiglia');
  const note = await cell.put({
    body: 'Fabio vive a [[Milano]] dal 2024, prima stava a Roma.',
    title: 'Casa di Fabio',
    noteType: 'fact',
    tags: ['casa'],
    source: 'homun:bot-7',
    confidence: 'stated',
  });
  assert.match(note.id, /^[0-9A-HJKMNP-TV-Z]{26}$/);
  assert.equal(note.status, 'active');
  assert.equal(note.title, 'Casa di Fabio');
  assert.equal(note.noteType, 'fact');
  assert.equal(note.version, 1);

  // the short id works, like the CLI
  const got = await cell.get(note.id.slice(-8));
  assert.equal(got.id, note.id);
  assert.equal(got.body, note.body);

  // search finds it
  const hits = await cell.search({ text: 'dove vive fabio', k: 5 });
  assert.ok(hits.length >= 1);
  assert.equal(hits[0].id, note.id);
  assert.equal(hits[0].cell, 'famiglia');
  assert.ok(hits[0].snippet.length > 0);
  assert.ok(hits[0].score > 0);

  // patch changes the body and bumps the version
  const patched = await cell.patch(note.id, { body: 'Fabio vive a [[Torino]] dal 2026.' });
  assert.equal(patched.version, 2);
  assert.match(patched.body, /Torino/);
  const history = await cell.history(note.id);
  assert.ok(Array.isArray(history) && history.length >= 1);

  // delete is a tombstone: get hides it, purge forgets it
  await cell.delete(note.id);
  assert.equal(await cell.get(note.id), null);
  const purge = await cell.purge(note.id, 'test oblio');
  assert.equal(typeof purge.forgottenPayloads, 'number');
  assert.ok(Array.isArray(purge.pending));
  assert.equal(await cell.get(note.id), null);
  await e.close();
});

test('quarantine: born external, hidden, then confirmed', async () => {
  const e = await engine();
  const cell = await e.cell('storia');
  const clean = await cell.put({
    body: 'nota pulita sul trasloco',
    source: 'homun',
    confidence: 'stated',
  });
  const ext = await cell.put({
    body: 'voci di corridoio sul trasloco',
    source: 'import:web',
    confidence: 'external',
  });
  assert.equal(ext.status, 'quarantined');
  // get hides it (the closed oracle: null, not an error)
  assert.equal(await cell.get(ext.id), null);
  // search hides it too
  const hidden = await cell.search({ text: 'voci corridoio trasloco' });
  assert.ok(hidden.every((h) => h.id !== ext.id));
  // and shows it when asked
  const shown = await cell.search({ text: 'voci corridoio trasloco', includeQuarantined: true });
  assert.ok(shown.some((h) => h.id === ext.id));
  // list sees it with the status filter
  const quarantined = await cell.list({ status: ['quarantined'] });
  assert.deepEqual(quarantined.map((n) => n.id), [ext.id]);
  // confirm promotes it
  await cell.confirm(ext.id);
  assert.equal((await cell.get(ext.id)).status, 'active');
  const _ = clean;
  await e.close();
});

test('errors carry a stable code', async () => {
  const e = await engine();
  const cell = await e.cell('errori');
  const codes = {};
  const expectCode = async (code, p) => {
    try {
      await p;
      assert.fail(`expected ${code}`);
    } catch (err) {
      assert.equal(err.code, code, `message: ${err.message}`);
      assert.ok(err instanceof Error);
      codes[code] = true;
    }
  };
  // the oracle: a well-formed but absent id answers null, not an error
  assert.equal(await cell.get('01AAAAAAAA'), null);
  await expectCode('NOT_FOUND', cell.patch('01ZZZZZZZZZZZZZZZZZZZZZZZZ', { body: 'x' }));
  await expectCode('INVALID_NOTE', cell.put({ body: 'x', source: 's', confidence: 'boh' }));
  await expectCode('INVALID_NOTE', cell.put({ body: 'x', source: '', confidence: 'stated' }));
  await expectCode('INVALID_NOTE', cell.put({ body: 'x', source: 's', confidence: 'stated', noteType: 'nonexistent' }));
  await expectCode('INVALID_NOTE', cell.search({ text: '   ' }));
  await expectCode('NOT_FOUND', e.cell('inesistente', { create: false }));
  // confirm of a NON-quarantined note is QUARANTINED
  const active = await cell.put({ body: 'y', source: 's', confidence: 'stated' });
  await expectCode('QUARANTINED', cell.confirm(active.id));
  assert.deepEqual(Object.keys(codes).sort(), ['INVALID_NOTE', 'NOT_FOUND', 'QUARANTINED']);
  await e.close();
});

test('200 puts and 200 searches in parallel, no errors', async () => {
  const e = await engine();
  const cell = await e.cell('parallela');
  await Promise.all(
    Array.from({ length: 200 }, (_, i) =>
      cell.put({
        body: `nota numero ${i} su argomento-${i % 7} con parola-${i % 13}`,
        title: `nota ${i}`,
        source: 'homun',
        confidence: 'stated',
      }),
    ),
  );
  const found = await Promise.all(
    Array.from({ length: 200 }, (_, i) =>
      cell.search({ text: `argomento-${i % 7}`, k: 3 }).then(
        (hits) => hits.length,
        (err) => {
          throw err;
        },
      ),
    ),
  );
  assert.equal(found.length, 200);
  assert.ok(found.every((n) => n > 0), `every query answered: ${found.filter((n) => n === 0).length} empty`);
  await e.close();
});

test('changesSince streams the ops', async () => {
  const e = await engine();
  const cell = await e.cell('flusso');
  const one = await cell.put({ body: 'prima', source: 's', confidence: 'stated' });
  const mid = (await cell.changesSince(0)).head;
  await cell.put({ body: 'seconda', source: 's', confidence: 'stated' });
  await cell.put({ body: 'terza', source: 's', confidence: 'stated' });
  const after = await cell.changesSince(mid);
  assert.equal(after.compacted, undefined);
  assert.ok(after.ops.length >= 2);
  assert.ok(after.head > mid);
  // every op carries its own seq, ascending
  const seqs = after.ops.map((o) => o.seq);
  assert.deepEqual(seqs, [...seqs].sort((a, b) => a - b));
  // compaction reports honestly
  const compact = await cell.compact(1);
  assert.ok(compact.cutSeq >= 1);
  const below = await cell.changesSince(0);
  assert.equal(below.compacted, true);
  assert.equal(typeof below.head, 'number');
  const still = await cell.changesSince(compact.cutSeq);
  assert.equal(still.compacted, undefined);
  const _ = one;
  await e.close();
});

test('Engine.search fuses two cells', async () => {
  const e = await engine();
  const a = await e.cell('casa');
  const b = await e.cell('lavoro');
  const home = await a.put({
    body: 'il trasloco a milano di fabio',
    source: 's',
    confidence: 'stated',
  });
  await b.put({
    body: 'fabio lavora da studio aurora a milano',
    source: 's',
    confidence: 'stated',
  });
  const hits = await e.search(['casa', 'lavoro'], { text: 'trasloco milano fabio', k: 10 });
  assert.ok(hits.length >= 2);
  assert.deepEqual([...new Set(hits.map((h) => h.cell))].sort(), ['casa', 'lavoro']);
  assert.equal(hits[0].id, home.id);
  // a cell that does not exist is NOT_FOUND
  await assert.rejects(e.search(['nulla'], { text: 'x' }), (err) => err.code === 'NOT_FOUND');
  await e.close();
});

test('modelStatus reports the builtin model (when present)', async () => {
  const e = await engine();
  const status = await e.modelStatus();
  assert.equal(typeof status.present, 'boolean');
  if (status.present) {
    assert.ok(status.bytes > 0);
    assert.match(status.id, /^granite-embedding-97m/);
  }
  await e.close();
});

// the test root goes away with the last engine: no temp left behind
process.on('exit', () => {
  if (!process.env.ENGRAMMA_TEST_TMP) {
    fs.rmSync(TMP, { recursive: true, force: true });
  }
});
