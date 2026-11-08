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

// v0.4.1 (P0.1): a rewrite cannot launder a quarantined note. The state
// changes only through setStatus/confirm/delete (and restore); a put with
// an existing id — or a patch — preserves it, and an `external` rewrite of
// an active note quarantines it (never the opposite).
test('a rewrite preserves the quarantine (P0.1)', async () => {
  const e = await engine();
  const cell = await e.cell('quarantena');
  const q = await cell.put({
    body: 'Il PIN di Fabio è 4321',
    source: 'homun:web',
    confidence: 'external',
  });
  assert.equal(q.status, 'quarantined');
  // the dangerous rewrite of the spec: same id, stated confidence, no
  // status — it must NOT become a "fact"
  const r = await cell.put({
    id: q.id,
    body: 'Il PIN di Fabio è 4321',
    source: 'homun:bot',
    confidence: 'stated',
  });
  assert.equal(r.status, 'quarantined', 'the rewrite launders the note');
  assert.equal(r.source, 'homun:bot');
  const hits = await cell.search({ text: 'PIN Fabio' });
  assert.ok(hits.every((h) => h.id !== q.id), 'the search stays blind to it');
  // the same rewrite ASKING for active is a CONFLICT
  await assert.rejects(
    cell.put({ id: q.id, body: 'x', source: 's', confidence: 'stated', status: 'active' }),
    (err) => err.code === 'CONFLICT',
  );
  // a patch keeps the state too
  const p = await cell.patch(q.id, { body: 'Il PIN di Fabio è 4322' });
  assert.equal(p.status, 'quarantined');
  // external on an ACTIVE note quarantines it through a rewrite
  const a = await cell.put({ body: 'Fabio lavora alla Acme', source: 'io', confidence: 'stated' });
  const ax = await cell.put({ id: a.id, body: 'Fabio lavora alla Acme', source: 'web', confidence: 'external' });
  assert.equal(ax.status, 'quarantined');
  // a tombstone is not resurrected by a put with its id
  await cell.delete(a.id);
  await assert.rejects(
    cell.put({ id: a.id, body: 'resurrezione', source: 's', confidence: 'stated' }),
    (err) => err.code === 'CONFLICT',
  );
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

// v0.4.1 (P1.6): one engine, one actor per cell — the log says which bot
// wrote (`homun:bot-<id>`), and two Cell objects over the same cell with
// different actors are the same cell underneath.
test('an actor per cell, one engine (P1.6)', async () => {
  const e = await engine();
  const bot7 = await e.cell('condivisa', { actor: 'homun:bot-7' });
  const daemon = await e.cell('condivisa', { actor: 'homun' });
  await bot7.put({ body: 'scritta dal bot 7', source: 'homun:chat/7/e1', confidence: 'stated' });
  await daemon.put({ body: 'scritta dal demone', source: 'homun', confidence: 'stated' });
  const ops = await daemon.changesSince(0);
  const actors = ops.ops.map((o) => o.actor).sort();
  assert.ok(actors.includes('homun:bot-7'), `${actors}`);
  assert.ok(actors.includes('homun'), `${actors}`);
  // without the option, the engine's actor (backwards compatible)
  const plain = await e.cell('altra');
  const n = await plain.put({ body: 'x', source: 's', confidence: 'stated' });
  assert.equal(n.id.length, 26);
  const opsPlain = await plain.changesSince(0);
  assert.ok(opsPlain.ops.every((o) => o.actor === 'homun'));
  await e.close();
});

// v0.4.1 (P1.5): a search hit carries what a result row needs — the id
// short form, the cell, the date, the tags, the type, the state — without a
// get per result.
test('SearchHit carries the row fields (P1.5)', async () => {
  const e = await engine();
  const cell = await e.cell('righe');
  const n = await cell.put({
    body: 'il compleanno di Giulia è a marzo',
    title: 'Compleanno',
    noteType: 'event',
    tags: ['famiglia/compleanni'],
    source: 'chat:2026-11-08',
    confidence: 'stated',
  });
  const hits = await cell.search({ text: 'compleanno Giulia', k: 3 });
  assert.equal(hits.length, 1);
  const h = hits[0];
  assert.equal(h.id, n.id);
  assert.equal(h.cell, 'righe');
  assert.equal(h.noteType, 'event');
  assert.deepEqual(h.tags, ['famiglia/compleanni']);
  assert.equal(h.status, 'active');
  assert.equal(typeof h.updatedAt, 'string');
  assert.match(h.updatedAt, /^\d{4}-\d{2}-\d{2}T/);
  assert.ok(h.updatedAt >= n.createdAt);
  await e.close();
});

// v0.4.1 (P1.4): import dates. A new note can be born in the past
// (createdAt/updatedAt, RFC 3339 — the migration needs them), never in the
// future; a rewrite keeps the birth date and moves only updated_at.
test('import dates: new notes in the past, rewrites keep the birth (P1.4)', async () => {
  const e = await engine();
  const cell = await e.cell('date');
  const birth = '2024-03-01T10:00:00Z';
  const n = await cell.put({
    body: 'memoria migrata da Homun',
    source: 'migrated:homun',
    confidence: 'stated',
    createdAt: birth,
    updatedAt: '2024-03-02T10:00:00Z',
  });
  assert.equal(n.createdAt, birth);
  assert.equal(n.updatedAt, '2024-03-02T10:00:00Z');
  // the rewrite a second later keeps the birth date
  const r = await cell.put({
    id: n.id,
    body: 'memoria riscritta dal bot',
    source: 'homun:bot-7',
    confidence: 'stated',
  });
  assert.equal(r.createdAt, birth, 'the rewrite moved the birth date');
  assert.ok(r.updatedAt > '2024-03-02T10:00:00Z', 'the edit time moved');
  // a date in the future is refused (5 minutes of tolerance)
  const future = new Date(Date.now() + 60 * 60 * 1000).toISOString().replace('.000Z', 'Z');
  await assert.rejects(
    cell.put({ body: 'x', source: 's', confidence: 'stated', createdAt: future }),
    (err) => err.code === 'INVALID_NOTE',
  );
  await assert.rejects(
    cell.put({ body: 'x', source: 's', confidence: 'stated', updatedAt: future }),
    (err) => err.code === 'INVALID_NOTE',
  );
  // on a rewrite createdAt is ignored (the note keeps its own)
  const r2 = await cell.put({
    id: n.id,
    body: 'di nuovo',
    source: 'homun:bot-7',
    confidence: 'stated',
    createdAt: '2030-01-01T00:00:00Z',
  });
  assert.equal(r2.createdAt, birth);
  await e.close();
});

// v0.4.1 (P1.3): x-* extensions ride through: Homun saves `x-homun` in the
// migration and must be able to read it back. `aliases` stays its own
// field; a patch sets keys one by one, `null` removes one.
test('extensions round-trip, also through a patch (P1.3)', async () => {
  const e = await engine();
  const cell = await e.cell('estensioni');
  const n = await cell.put({
    body: 'nota migrata da Homun',
    source: 'migrated:homun',
    confidence: 'stated',
    aliases: ['Fabio C.'],
    extensions: {
      'x-homun': { id: 'mem-42', tier: 'core', scope: 'user', source: 'template' },
      'x-count': 3,
      'x-tags': ['a', 'b'],
    },
  });
  assert.deepEqual(n.extensions, {
    'x-homun': { id: 'mem-42', tier: 'core', scope: 'user', source: 'template' },
    'x-count': 3,
    'x-tags': ['a', 'b'],
  });
  assert.deepEqual(n.aliases, ['Fabio C.'], 'aliases stays its own field');
  const got = await cell.get(n.id);
  assert.deepEqual(got.extensions['x-homun'], { id: 'mem-42', tier: 'core', scope: 'user', source: 'template' });

  // patch: one key in, one key out, the rest untouched
  const p1 = await cell.patch(n.id, {
    extensions: { 'x-homun': { id: 'mem-42', tier: 'log' }, 'x-count': null },
  });
  assert.deepEqual(p1.extensions['x-homun'], { id: 'mem-42', tier: 'log' });
  assert.equal(p1.extensions['x-count'], undefined);
  assert.deepEqual(p1.extensions['x-tags'], ['a', 'b']);
  assert.deepEqual(p1.aliases, ['Fabio C.'], 'the aliases survive the patch');
  await e.close();
});

// v0.4.1 (P0.2): a purge from Node reaches the views too — the file of the
// note goes NOW, not at the next sync, and a view nobody can reach is said
// honestly (`pending: ['vault']`, `scrubbed: false`, the problems listed).
// The view is bound with the CLI, like the spec's hand proof.
test('purge reaches a bound view (P0.2)', async () => {
  const cli =
    process.env.ENGRAMMA_BIN ||
    ['../../../target/release/engramma', '../../../target/debug/engramma']
      .map((p) => path.resolve(__dirname, p))
      .find((p) => fs.existsSync(p));
  assert.ok(cli, 'engramma binary not found: build it or set ENGRAMMA_BIN');
  const run = (args) => {
    const r = require('node:child_process').spawnSync(cli, args, { encoding: 'utf8' });
    assert.equal(r.status, 0, `${args.join(' ')}\n${r.stdout}${r.stderr}`);
    return r;
  };

  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const cell = await e.cell('vista');
  const n = await cell.put({
    body: 'il pin del lucchetto e quokkaviolace3',
    title: 'Segreto di prova',
    noteType: 'fact',
    source: 's',
    confidence: 'stated',
  });
  const viewDir = path.join(path.dirname(rootDir), 'vista-md');
  run(['--root', rootDir, '--cell', 'vista', 'vault', 'init', viewDir]);
  const bound = await cell.vaultViews();
  assert.equal(bound.length, 1, `${JSON.stringify(bound)}`);
  const exported = fs.readdirSync(path.join(viewDir, 'fact'));
  assert.equal(exported.length, 1, `${exported}`);

  // the purge forgets the note AND its file in the view, now
  const p = await cell.purge(n.id, 'prova oblio');
  assert.equal(p.forgottenPayloads, 1);
  assert.equal(p.scrubbed, true);
  assert.deepEqual(p.pending, []);
  assert.equal(fs.readdirSync(path.join(viewDir, 'fact')).length, 0, 'the file stayed');

  // a second note in a view that goes away: the purge says what it could
  // not reach, without throwing
  const n2 = await cell.put({
    body: 'secondo segreto zibellinotrem9',
    title: 'Altro segreto',
    noteType: 'fact',
    source: 's',
    confidence: 'stated',
  });
  const awayDir = path.join(path.dirname(rootDir), 'vista-away');
  run(['--root', rootDir, '--cell', 'vista', 'vault', 'init', awayDir]);
  const views2 = await cell.vaultViews();
  assert.equal(views2.length, 2, `${JSON.stringify(views2)}`);
  fs.renameSync(awayDir, `${awayDir}-moved`);
  const p2 = await cell.purge(n2.id, 'oblio con la vista via');
  assert.equal(p2.scrubbed, false);
  assert.deepEqual(p2.pending, ['vault']);
  assert.ok(Array.isArray(p2.views) && p2.views.length === 1, `${JSON.stringify(p2.views)}`);
  const away = views2.find((v) => v.root.endsWith('vista-away'));
  assert.ok(away, `${JSON.stringify(views2)}`);
  assert.equal(p2.views[0].viewId, away.viewId);
  assert.ok(p2.views[0].root.endsWith('vista-away'), `${p2.views[0].root}`);
  assert.ok(p2.views[0].reason.length > 0);
  // no note content rides in the problem
  assert.ok(!JSON.stringify(p2.views).includes('zibellinotrem'));
  await e.close();
});

// the test root goes away with the last engine: no temp left behind
process.on('exit', () => {
  if (!process.env.ENGRAMMA_TEST_TMP) {
    fs.rmSync(TMP, { recursive: true, force: true });
  }
});
