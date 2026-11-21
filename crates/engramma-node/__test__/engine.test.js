// SPDX-License-Identifier: Apache-2.0

// v0.4 (P2.8): the JS contract of the point 7, on `node:test`. No extra
// dependencies. Text mode (`embedder: 'none'`) keeps the suite
// model-independent; `test:electron` runs the same file under Homun's
// Electron in Node mode.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { Engine, version, buildInfo } = require('..');
const { assertCliMatchesAddon } = require('./bincheck.js');

// v0.4.2: the CLI binary the view tests spawn. ENGRAMMA_BIN wins; then
// $CARGO_TARGET_DIR (a v0.4.1 leftover: with it set, the old lookup only
// tried <repo>/target and found nothing), then the repo's own target.
const engrammaBin = () =>
  process.env.ENGRAMMA_BIN ||
  [process.env.CARGO_TARGET_DIR, '../../../target']
    .filter(Boolean)
    .flatMap((t) => [`${t}/release/engramma`, `${t}/debug/engramma`])
    .map((p) => path.resolve(__dirname, p))
    .find((p) => fs.existsSync(p));

// v0.6.10 (J3): the bench REFUSES a CLI that is not the one these
// sources built — the commit and the schema the binary prints must be
// the addon's own. A test that passes or falls according to which
// binary lies around in target/ measures nothing (the v0.6.9
// verification met one three days old). When no binary is around, the
// per-test assertion below keeps saying so as it always did.
{
  const cli = engrammaBin();
  if (cli) assertCliMatchesAddon(cli, buildInfo());
}

// v0.6.11 (K3): the bench folder of this file — its own, new, and
// cleaned at the end of the run (failures included); ENGRAMMA_TEST_KEEP
// keeps it and says where it is
const TMP = require('./tmpdir.js').testTmp('engine');
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

// v0.6.11 (K1): the read-only preview of a purge — the notes whose
// closing was this note's work REOPEN when it is forgotten. Homun warns
// before the purge: «tornerà valido: …». The supersede is staged by the
// CLI (the binding has no supersede of its own: corrections arrive from
// the agent's tools), the same road the view tests walk.
test('purgePreview() names the note that would reopen, and moves nothing', async () => {
  const cli = engrammaBin();
  if (!cli) return; // no binary around: the per-test assertion below says so
  const run = (args) => {
    const r = require('node:child_process').spawnSync(cli, args, { encoding: 'utf8' });
    assert.equal(r.status, 0, `${args.join(' ')}\n${r.stdout}${r.stderr}`);
    return r;
  };
  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  try {
    const cell = await e.cell('riaperture');
    const old = await cell.put({
      body: 'il mio colore preferito è il rosso',
      source: 'homun:test',
      confidence: 'stated',
    });
    const file = path.join(TMP, 'nuova-nota.md');
    fs.writeFileSync(file, 'il mio colore preferito è il nero\n');
    run([
      '--root', rootDir,
      '--cell', 'riaperture',
      'supersede', old.id, file,
      '--source', 'homun:test', '--confidence', 'stated',
    ]);
    const notes = await cell.list();
    const next = notes.find((n) => n.body.includes('nero'));
    assert.ok(next, 'the supersede created the new note');

    const preview = await cell.purgePreview(next.id);
    assert.equal(preview.reopens.length, 1, 'the old note would reopen');
    assert.equal(preview.reopens[0].id, old.id);

    // read-only: asking twice changes nothing, and the old note is still
    // closed
    const again = await cell.purgePreview(next.id);
    assert.deepEqual(again, preview);
    const still = await cell.get(old.id);
    assert.equal(still.status, 'superseded');

    // and a note that closes nothing reopens nothing
    const quiet = await cell.purgePreview(old.id);
    assert.deepEqual(quiet.reopens, []);
  } finally {
    await e.close();
  }
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
  // v0.4.2: the ARGUMENTS of a call are INVALID_ARGUMENT (the notes stay
  // INVALID_NOTE) — an empty query text is an argument, not a note
  await expectCode('INVALID_ARGUMENT', cell.search({ text: '   ' }));
  await expectCode('INVALID_ARGUMENT', cell.search({ text: 'x', mode: 'boh' }));
  await expectCode('NOT_FOUND', e.cell('inesistente', { create: false }));
  // confirm of a NON-quarantined note is QUARANTINED
  const active = await cell.put({ body: 'y', source: 's', confidence: 'stated' });
  await expectCode('QUARANTINED', cell.confirm(active.id));
  assert.deepEqual(Object.keys(codes).sort(), [
    'INVALID_ARGUMENT',
    'INVALID_NOTE',
    'NOT_FOUND',
    'QUARANTINED',
  ]);
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

// v0.4.1 (P2.9): the short id of a view is its LAST 8 characters
// everywhere — messages, unbind, vaultUnbind — like the notes' (the first 8
// of a ULID are the timestamp: two views minted minutes apart share them).
test('vaultUnbind takes the last 8 of the view id (P2.9)', async () => {
  const cli = engrammaBin();
  assert.ok(cli, 'engramma binary not found: build it or set ENGRAMMA_BIN');
  const run = (args) => {
    const r = require('node:child_process').spawnSync(cli, args, { encoding: 'utf8' });
    assert.equal(r.status, 0, `${args.join(' ')}\n${r.stdout}${r.stderr}`);
    return r;
  };
  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const cell = await e.cell('sgancio');
  await cell.put({ body: 'una nota qualsiasi', source: 's', confidence: 'stated' });
  const viewDir = path.join(path.dirname(rootDir), 'vista-sgancio');
  run(['--root', rootDir, '--cell', 'sgancio', 'vault', 'init', viewDir]);
  const [view] = await cell.vaultViews();
  assert.ok(view, 'the view is bound');
  const last8 = view.viewId.slice(-8);
  const first8 = view.viewId.slice(0, 8);
  // the first 8 are the timestamp: NOT a short id anymore
  await assert.rejects(cell.vaultUnbind(first8), (err) => err.code === 'NOT_FOUND');
  await cell.vaultUnbind(last8);
  assert.equal((await cell.vaultViews()).length, 0);
  await e.close();
});

// v0.4.1 (P1.8): the REPORT v0.4 already said it — "CONFLICT: una nota
// external non può nascere attiva" — but the binding silently quarantined
// an explicit `status: 'active'`. Now it is the caller's conflict; without
// a status the note is born quarantined, as ever.
test('external cannot be born active: CONFLICT (P1.8)', async () => {
  const e = await engine();
  const cell = await e.cell('ext');
  await assert.rejects(
    cell.put({ body: 'voci dal web', source: 'web', confidence: 'external', status: 'active' }),
    (err) => err.code === 'CONFLICT',
  );
  const q = await cell.put({ body: 'voci dal web', source: 'web', confidence: 'external' });
  assert.equal(q.status, 'quarantined');
  const q2 = await cell.put({
    body: 'altre voci',
    source: 'web',
    confidence: 'external',
    status: 'quarantined',
  });
  assert.equal(q2.status, 'quarantined');
  await e.close();
});

// v0.4.1 (P1.7): after close, nothing reopens silently. Every method of the
// engine and of every one of its cells answers CLOSED; closing twice is not
// an error; operations already in flight finish before close resolves.
test('after close, everything answers CLOSED (P1.7)', async () => {
  const e = await engine();
  const cell = await e.cell('chiusa');
  const n = await cell.put({ body: 'x', source: 's', confidence: 'stated' });
  // an operation in flight while close arrives: it finishes
  const inFlight = cell.put({ body: 'in volo', source: 's', confidence: 'stated' });
  await e.close();
  await inFlight; // no error: it was called before the close
  await e.close(); // closing twice is not an error
  const closed = async (call) => {
    try {
      await call();
      assert.fail('expected CLOSED');
    } catch (err) {
      assert.equal(err.code, 'CLOSED', `message: ${err.message}`);
    }
  };
  await closed(() => cell.put({ body: 'y', source: 's', confidence: 'stated' }));
  await closed(() => cell.get(n.id));
  await closed(() => cell.search({ text: 'x' }));
  await closed(() => cell.list({}));
  await closed(() => cell.history(n.id));
  await closed(() => cell.neighbors(n.id));
  await closed(() => cell.confirm(n.id));
  await closed(() => cell.delete(n.id));
  await closed(() => cell.purge(n.id, 'dopo la chiusura'));
  await closed(() => cell.embedPending());
  await closed(() => cell.changesSince(0));
  await closed(() => cell.verify());
  await closed(() => cell.compact(1));
  await closed(() => cell.vaultViews());
  await closed(() => cell.vaultUnbind('01AAAAAAAA'));
  await closed(() => e.search(['chiusa'], { text: 'x' }));
  await closed(() => e.cells());
  await closed(() => e.cell('altra'));
  await closed(() => e.modelStatus());
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
  const cli = engrammaBin();
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

// v0.4.2 (P0.1): `list` has no hidden ceilings. An explicit `limit` is
// respected AS GIVEN (a local process, the ids are already in memory) —
// Homun asks `limit: 100_000` on a bot's cell and must not be answered by a
// silent cut at 1000; absent still means 100, as ever. `offset` pages
// without holes or duplicates. `limit: 0` and `k: 0` are INVALID_ARGUMENT:
// a page of zero is a caller's bug, not a result.
test('list: no hidden ceilings, explicit limit respected (P0.1)', async () => {
  const e = await engine();
  const cell = await e.cell('tetti');
  const TOTAL = 1500;
  for (let i = 0; i < TOTAL; i++) {
    await cell.put({
      body: `nota numero ${i} su argomento-${i % 7}`,
      source: 'homun:bot-a1',
      confidence: 'stated',
    });
  }
  // absent limit = 100, as ever
  assert.equal((await cell.list()).length, 100);
  // an explicit limit is respected AS GIVEN: no silent cut at 1000
  assert.equal((await cell.list({ limit: 100_000 })).length, TOTAL);
  assert.equal((await cell.list({ limit: TOTAL })).length, TOTAL);
  // offset+limit page without holes or duplicates
  const seen = [];
  const PAGE = 137;
  for (let off = 0; off < TOTAL; off += PAGE) {
    seen.push(...(await cell.list({ offset: off, limit: PAGE })).map((n) => n.id));
  }
  assert.equal(seen.length, TOTAL, `${TOTAL} notes paged`);
  assert.equal(new Set(seen).size, TOTAL, 'no duplicates across the pages');
  assert.deepEqual(seen, (await cell.list({ limit: TOTAL })).map((n) => n.id));
  // zero is a bug, not an empty page
  await assert.rejects(
    cell.list({ limit: 0 }),
    (err) => err.code === 'INVALID_ARGUMENT',
  );
  await e.close();
});

// v0.4.2 (P0.1): the OTHER silent ceilings of the binding are gone too.
// Search `k` is respected as given (absent = 10); the engine's own maxima
// (graphHops 0–2, neighbors depth 1–4) are ERRORS now, never silent clamps.
test('search k respected; hops/depth beyond the range are errors (P0.1)', async () => {
  const e = await engine();
  const cell = await e.cell('soffitti');
  for (let i = 0; i < 150; i++) {
    await cell.put({ body: `argomento-${i % 5} nota numero ${i}`, source: 's', confidence: 'stated' });
  }
  assert.equal((await cell.search({ text: 'argomento', k: 10 })).length, 10);
  const big = await cell.search({ text: 'argomento', k: 200 });
  assert.equal(big.length, 150, 'k beyond the old ceiling of 100 is respected');
  await assert.rejects(
    cell.search({ text: 'argomento', k: 0 }),
    (err) => err.code === 'INVALID_ARGUMENT',
  );
  await assert.rejects(
    cell.search({ text: 'argomento', graphHops: 3 }),
    (err) => err.code === 'INVALID_ARGUMENT',
  );
  const n = await cell.put({ body: 'una nota che cita [[Roma]]', source: 's', confidence: 'stated' });
  await assert.rejects(cell.neighbors(n.id, 5), (err) => err.code === 'INVALID_ARGUMENT');
  // within the range they work as ever
  assert.ok(Array.isArray(await cell.neighbors(n.id, 4)));
  assert.ok((await cell.search({ text: 'Roma', graphHops: 2 })).length >= 1);
  await e.close();
});

// v0.4.2 (P0.2): the tombstones list. `list({ status: ['deleted'] })` shows
// a deleted note as its last version with status 'deleted' — what the user
// sees to decide restore or forget; before, the unknown status was dropped
// by a filter_map and the answer was an EMPTY list, no error, so a deleted
// note kept its text in the history forever with no way to find it. An
// unknown status is INVALID_ARGUMENT now. `get` stays a closed oracle:
// null for quarantined and tombstoned, as ever.
test('tombstones list; unknown statuses are INVALID_ARGUMENT (P0.2)', async () => {
  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const cell = await e.cell('lapidi');
  const BODY = 'segreto quokkaviolaceo zibellinotremante';
  const n = await cell.put({ body: BODY, title: 'Segreto', source: 's', confidence: 'stated' });
  await cell.delete(n.id);
  // the closed oracle stays closed
  assert.equal(await cell.get(n.id), null);
  // but the tombstone LISTS, as its last version
  const tombs = await cell.list({ status: ['deleted'] });
  assert.equal(tombs.length, 1, `${JSON.stringify(tombs)}`);
  assert.equal(tombs[0].id, n.id);
  assert.equal(tombs[0].status, 'deleted');
  assert.equal(tombs[0].body, BODY);
  // and it does not pollute the default list (as ever: living notes only)
  assert.equal((await cell.list()).length, 0);
  // purge on the tombstone's id forgets it for good, and afterwards the
  // list does not carry it anymore
  const p = await cell.purge(n.id, 'dimentica per sempre');
  assert.equal(p.scrubbed, true, `${JSON.stringify(p)}`);
  assert.equal((await cell.list({ status: ['deleted'] })).length, 0);
  // an unknown status is an ERROR, never a silent empty filter
  await assert.rejects(
    cell.list({ status: ['foo'] }),
    (err) => err.code === 'INVALID_ARGUMENT',
  );
  await assert.rejects(
    cell.list({ status: ['active', 'bar'] }),
    (err) => err.code === 'INVALID_ARGUMENT',
  );
  await e.close();
  // no root of the text survives anywhere under the root (forgetting.rs's
  // lesson, from JS: the whole lowered words of an invented vocabulary)
  const needles = [];
  for (const tok of BODY.toLowerCase().split(/[^a-zà-ÿ]+/)) {
    for (const len of [tok.length, tok.length - 1, tok.length - 2]) {
      const stem = tok.slice(0, len);
      if (stem.length >= 5 && !needles.includes(stem)) needles.push(stem);
    }
  }
  const stack = [rootDir];
  while (stack.length) {
    const d = stack.pop();
    if (!fs.existsSync(d)) continue;
    for (const ent of fs.readdirSync(d, { withFileTypes: true })) {
      const pp = path.join(d, ent.name);
      if (ent.isDirectory()) {
        stack.push(pp);
        continue;
      }
      const raw = fs.readFileSync(pp);
      for (const needle of needles) {
        assert.ok(!raw.includes(needle), `«${needle}» sopravvive in ${pp}`);
      }
    }
  }
});

// v0.4.2 (P0.3): Engine.dropCell — the oblivion of a whole cell. Homun
// deletes a bot and with it its cells: a purge per note (~30 ms each, minutes
// of queue with thousands of notes) was fragile (P0.1) and slow. dropCell
// forgets the cell's files (WAL and snapshots included), takes the note
// files out of the bound views, and the Cell objects already handed out
// answer NOT_FOUND; a new cell(name) creates a fresh empty one.
const NO_ROOTS = (dir, bodies) => {
  // forgetting.rs's lesson from JS: the whole lowered words of an invented
  // vocabulary (and their truncations, a poor man's stemming)
  const needles = [];
  for (const body of bodies) {
    for (const tok of body.toLowerCase().split(/[^a-zà-ÿ]+/)) {
      for (const len of [tok.length, tok.length - 1, tok.length - 2]) {
        const stem = tok.slice(0, len);
        if (stem.length >= 5 && !needles.includes(stem)) needles.push(stem);
      }
    }
  }
  const stack = [dir];
  while (stack.length) {
    const d = stack.pop();
    if (!fs.existsSync(d)) continue;
    for (const ent of fs.readdirSync(d, { withFileTypes: true })) {
      const pp = path.join(d, ent.name);
      if (ent.isDirectory()) {
        stack.push(pp);
        continue;
      }
      const raw = fs.readFileSync(pp);
      for (const needle of needles) {
        assert.ok(!raw.includes(needle), `«${needle}» sopravvive in ${pp}`);
      }
    }
  }
};

test('dropCell forgets a whole cell, 1500 notes and 20 tombstones (P0.3)', async () => {
  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const cell = await e.cell('bot-a1');
  const MARKER = 'quokkalapide';
  for (let i = 0; i < 1500; i++) {
    await cell.put({
      body: `memoria segreta ${MARKER} numero ${i} del bot eliminato`,
      source: 'homun:chat/bot-a1/e1',
      confidence: 'stated',
    });
  }
  const tombs = [];
  for (let i = 0; i < 20; i++) {
    const t = await cell.put({
      body: `lapide segreta ${MARKER} numero ${i}`,
      source: 'homun:chat/bot-a1/e1',
      confidence: 'stated',
    });
    await cell.delete(t.id);
    tombs.push(t);
  }
  assert.ok((await e.cells()).includes('bot-a1'));
  const drop = await e.dropCell('bot-a1', 'bot eliminato dall\'utente');
  assert.equal(drop.notes, 1520, `${JSON.stringify(drop)}`);
  assert.equal(drop.scrubbed, true);
  assert.deepEqual(drop.pending, []);
  assert.deepEqual(drop.views, []);
  // the cell is not listed anymore, and nothing under the root carries a
  // root of any body (cell, WAL, snapshot — everything went)
  assert.ok(!(await e.cells()).includes('bot-a1'));
  NO_ROOTS(rootDir, [`memoria segreta ${MARKER}`, `lapide segreta ${MARKER}`]);
  // a fresh cell(name) is born empty
  const fresh = await e.cell('bot-a1');
  assert.equal((await fresh.list()).length, 0);
  await e.close();
});

test('dropCell with an open Cell: NOT_FOUND, in-flight ops finish (P0.3)', async () => {
  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const cell = await e.cell('bot-b2');
  const n1 = await cell.put({ body: 'prima nota del bot b2', source: 's', confidence: 'stated' });
  // an operation in flight while dropCell arrives: it was called first, it
  // finishes (like close)
  const inFlight = cell.put({ body: 'nota in volo', source: 's', confidence: 'stated' });
  const drop = await e.dropCell('bot-b2', 'bot eliminato');
  await inFlight; // no error: it entered before the drop
  assert.equal(drop.notes, 2);
  // the Cell object already handed out answers NOT_FOUND, every method
  const notFound = async (call) => {
    try {
      await call();
      assert.fail('expected NOT_FOUND');
    } catch (err) {
      assert.equal(err.code, 'NOT_FOUND', `message: ${err.message}`);
    }
  };
  await notFound(() => cell.put({ body: 'x', source: 's', confidence: 'stated' }));
  await notFound(() => cell.get(n1.id));
  await notFound(() => cell.list());
  await notFound(() => cell.search({ text: 'nota' }));
  await notFound(() => cell.delete(n1.id));
  await notFound(() => cell.purge(n1.id, 'dopo la cancellazione'));
  // a new cell(name) is a FRESH one, not the old object's ghost
  const fresh = await e.cell('bot-b2');
  assert.equal((await fresh.list()).length, 0);
  const born = await fresh.put({ body: 'nuova vita', source: 's', confidence: 'stated' });
  assert.ok((await fresh.get(born.id)).body.includes('nuova vita'));
  await e.close();
});

// dropCell reaches the bound views like purge_everywhere: the note files of
// the cell go NOW; a view nobody can reach is said honestly
// (pending: ['vault'], scrubbed: false, the problem listed, no contents).
// Two scenarios, because a note has ONE file wherever it is (structural:
// a second `vault init` REBINDS every note to the new view) — a reachable
// view and an unreachable one cannot both hold bindings at once.
test('dropCell reaches the views, and says the unreachable one (P0.3)', async () => {
  const cli = engrammaBin();
  assert.ok(cli, 'engramma binary not found: build it or set ENGRAMMA_BIN');
  const run = (args) => {
    const r = require('node:child_process').spawnSync(cli, args, { encoding: 'utf8' });
    assert.equal(r.status, 0, `${args.join(' ')}\n${r.stdout}${r.stderr}`);
    return r;
  };

  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const SECRET = 'quokkavista';
  const fill = async (name, n) => {
    const cell = await e.cell(name);
    for (let i = 0; i < n; i++) {
      await cell.put({
        body: `segreto ${SECRET} numero ${i} da dimenticare`,
        source: 's',
        confidence: 'stated',
      });
    }
    return cell;
  };

  // 1. a REACHABLE view: the drop takes its note files now
  const here = path.join(path.dirname(rootDir), 'vista-drop-qui');
  const c1 = await fill('bot-v3', 5);
  run(['--root', rootDir, '--cell', 'bot-v3', 'vault', 'init', here]);
  assert.equal((await c1.vaultViews()).length, 1);
  const dropHere = await e.dropCell('bot-v3', 'bot eliminato');
  assert.equal(dropHere.notes, 5);
  assert.equal(dropHere.scrubbed, true, JSON.stringify(dropHere));
  assert.deepEqual(dropHere.views, []);
  // the note files went with the cell (the binding's default type is
  // `note`, so they live under note/)
  const left = fs.readdirSync(path.join(here, 'note'));
  assert.equal(left.length, 0, `${left}`);
  assert.ok(!(await e.cells()).includes('bot-v3'));

  // 2. a view nobody can reach: the drop says so, it does not throw
  const away = path.join(path.dirname(rootDir), 'vista-drop-via');
  await fill('bot-v4', 4);
  run(['--root', rootDir, '--cell', 'bot-v4', 'vault', 'init', away]);
  fs.renameSync(away, `${away}-moved`);
  const dropAway = await e.dropCell('bot-v4', 'bot eliminato');
  assert.equal(dropAway.notes, 4);
  assert.equal(dropAway.scrubbed, false);
  assert.deepEqual(dropAway.pending, ['vault']);
  assert.equal(dropAway.views.length, 1, `${JSON.stringify(dropAway.views)}`);
  assert.ok(dropAway.views[0].root.includes('vista-drop-via'), dropAway.views[0].root);
  assert.ok(dropAway.views[0].reason.length > 0);
  // no note content rides in the problem
  assert.ok(!JSON.stringify(dropAway.views).includes(SECRET));
  // the cell went anyway (the folder that stayed is the caller's to finish)
  assert.ok(!(await e.cells()).includes('bot-v4'));
  await e.close();
});

// v0.4.3 (P0.1): snapshots and cells share the folder, and only the EXACT
// compact format `<cell>-snap-<digits>.sqlite` is a snapshot. `storia-snap-2`
// is storia's snapshot; `storia-snap-archivio` is a CELL whose name merely
// looks like one — dropCell('storia') must forget storia (its file, WAL and
// snapshot) and leave the neighbor whole on disk, readable from a FRESH
// engine. And `cells()` never lists a snapshot: a cell cannot have that
// name anymore, so a file in that shape is always a snapshot.
test('dropCell spares the neighbors; a snapshot is not a cell (P0.1)', async () => {
  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const storia = await e.cell('storia');
  await storia.put({ body: 'la nota di storia', source: 's', confidence: 'stated' });
  // a REAL snapshot, the format compact writes: storia-snap-<head>.sqlite
  await storia.compact(1, { keepSnapshot: true });
  const cellsDir = path.join(rootDir, 'cells');
  const snap = fs.readdirSync(cellsDir).find((n) => /^storia-snap-\d+\.sqlite$/.test(n));
  assert.ok(snap, `compact wrote a snapshot: ${fs.readdirSync(cellsDir)}`);

  // the neighbor whose name merely LOOKS like a snapshot, and one more cell
  const archivio = await e.cell('storia-snap-archivio');
  await archivio.put({ body: "la nota dell'archivio", source: 's', confidence: 'stated' });
  const storia2 = await e.cell('storia2');
  await storia2.put({ body: 'la nota di storia2', source: 's', confidence: 'stated' });

  // cells() lists the cells, NEVER the snapshot
  const listed = await e.cells();
  assert.ok(listed.includes('storia'), `${listed}`);
  assert.ok(listed.includes('storia-snap-archivio'), `${listed}`);
  assert.ok(!listed.some((n) => /^storia-snap-\d+$/.test(n)), `the snapshot is listed: ${listed}`);

  await e.dropCell('storia', 'prova del punto 1');
  // storia went: the db, its WAL/shm and ITS snapshot
  const left = fs.readdirSync(cellsDir).sort();
  assert.ok(!left.includes('storia.sqlite'), `${left}`);
  assert.ok(
    !left.some((n) => /^storia-snap-\d+\.sqlite(-wal|-shm)?$/.test(n)),
    `storia's snapshot survived: ${left}`,
  );
  // the neighbors are whole on disk, no orphan wal/shm of the dropped cell
  assert.ok(left.includes('storia-snap-archivio.sqlite'), `${left}`);
  assert.ok(left.includes('storia2.sqlite'), `${left}`);
  // and they re-read WHOLE from a fresh engine (the data, not a RAM ghost)
  await e.close();
  const e2 = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const arch2 = await e2.cell('storia-snap-archivio', { create: false });
  const archNotes = await arch2.list();
  assert.equal(archNotes.length, 1, `${JSON.stringify(archNotes)}`);
  assert.match(archNotes[0].body, /archivio/);
  const due = await e2.cell('storia2', { create: false });
  assert.equal((await due.list()).length, 1);
  await e2.close();
});

// v0.4.3 (P0.1): the snapshot-SHAPED name is reserved. A cell called
// `x-snap-7` would be indistinguishable from a snapshot of `x`: its file
// would ride with x's drop and never be listed. INVALID_ARGUMENT, like
// every other bad argument of the call. `storia-snap-archivio` stays valid:
// it does not end with digits.
test('a cell name that looks like a snapshot is reserved (P0.1)', async () => {
  const e = await engine();
  await assert.rejects(e.cell('x-snap-7'), (err) => err.code === 'INVALID_ARGUMENT');
  await assert.rejects(e.dropCell('x-snap-7', 'prova'), (err) => err.code === 'INVALID_ARGUMENT');
  const ok = await e.cell('storia-snap-archivio');
  assert.equal((await ok.list()).length, 0);
  await e.close();
});

// v0.4.3 (P1.2): a Cell kept alive in JS must not keep the dropped cell's
// FILES open until the GC collects its wrapper. Homun caches Cell objects
// for hours; dropCell retires the shared connection at once — lsof must
// find no descriptor on the cell's files the moment the drop resolves,
// without any gc() in between. Skipped explicitly when lsof is not there.
test('dropCell closes the connection a live Cell still shares (P1.2)', async (t) => {
  const rootDir = root();
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const held = await e.cell('tenuta');
  for (let i = 0; i < 20; i++) {
    await held.put({ body: `nota segreta quokkaaperta-${i} della cella tenuta`, source: 's', confidence: 'stated' });
  }
  await e.dropCell('tenuta', 'il GC non decide lui quando chiudere');
  // the Cell object is STILL alive in JS — and yet no descriptor on the
  // cell's files may survive the drop. v0.4.4 (P2): on Linux the right
  // window is /proc/self/fd (lsof is not in the Docker image); on macOS
  // it stays lsof.
  let openOn;
  if (fs.existsSync('/proc/self/fd')) {
    const cellsDir = path.join(rootDir, 'cells');
    openOn = fs.readdirSync('/proc/self/fd')
      .flatMap((fd) => {
        try {
          const target = fs.readlinkSync(path.join('/proc/self/fd', fd));
          return target.startsWith(cellsDir) ? [`fd ${fd} -> ${target}`] : [];
        } catch {
          return [];
        }
      });
  } else {
    const lsof = require('node:child_process').spawnSync(
      'lsof',
      ['-p', String(process.pid)],
      { encoding: 'utf8' },
    );
    if (lsof.error) {
      await e.close();
      return t.skip(`lsof non disponibile: ${lsof.error.message}`);
    }
    assert.equal(lsof.status, 0, lsof.stderr);
    openOn = lsof.stdout
      .split('\n')
      .filter((l) => l.includes(path.join(rootDir, 'cells')));
  }
  assert.equal(openOn.length, 0, `descrittori ancora aperti: ${openOn.join(' | ')}`);
  // and the live Cell object answers NOT_FOUND from the seal, as ever (the
  // gate refuses at CALL time — a sync throw, which the async wrapper turns
  // into the rejection assert.rejects waits for)
  await assert.rejects(
    async () => {
      await held.list();
    },
    (err) => err.code === 'NOT_FOUND',
  );
  await e.close();
});

// v0.4.3 (P2.3): no Cell is born dead. The open took the generation gate
// first and opened the cell file after, with the lock released in between:
// a dropCell entering that window sealed the very gate the open was about
// to hand out, and the returning `cell(name)` answered NOT_FOUND forever —
// a fresh Cell over a fresh file, dead on arrival (its `cell(name)` had
// been called before the drop, but the Cell object did not exist yet).
// 200 cell()/dropCell() pairs race on one name: every Cell that comes
// back must answer list(), unless a drop completed AFTER the Cell was
// created (the legitimate death: an oblivion that took the generation the
// Cell belongs to, started after it existed).
test('no Cell is born dead: 200 cell()/dropCell() pairs (P2.3)', async () => {
  const e = await engine();
  const seed = await e.cell('stress');
  await seed.put({ body: 'una nota perché ogni drop abbia qualcosa da dimenticare', source: 's', confidence: 'stated' });

  // one monotonic clock for both resolutions, written as they happen: the
  // tick lands in the CALLER's slot (openAt[i] belongs to cells[i]), not in
  // resolution order
  let tick = 0;
  const openAt = new Array(200);
  const dropAt = [];
  const opens = [];
  const drops = [];
  for (let i = 0; i < 200; i++) {
    drops.push(
      e.dropCell('stress', `stress ${i}`).catch((err) => {
        // a drop that arrives when the file is not there lost the race:
        // NOT_FOUND is the honest answer of THAT call
        if (err.code !== 'NOT_FOUND') throw err;
      }).then(() => { dropAt.push(tick++); }),
    );
    opens.push(
      e.cell('stress').then((c) => { openAt[i] = tick++; return c; }),
    );
  }
  const cells = await Promise.all(opens);
  await Promise.all(drops);

  let dead = 0;
  // every Cell answers, in order of creation; a NOT_FOUND is acceptable
  // only when some drop completed AFTER that Cell was created
  for (let i = 0; i < cells.length; i++) {
    try {
      await cells[i].list();
    } catch (err) {
      assert.equal(err.code, 'NOT_FOUND', `unexpected: ${err.message}`);
      const aDropCameLater = dropAt.some((d) => d > openAt[i]);
      assert.ok(
        aDropCameLater,
        `la Cell #${i} è nata morta: nessun drop completato dopo la sua creazione`,
      );
      dead += 1;
    }
  }
  // with the race over, a fresh cell(name) must be alive
  const after = await e.cell('stress');
  assert.equal((await after.list()).length, 0);
  await e.close();
});

// v0.4.2 (P1.4): pullModel attaches the embedder to the OPEN engine. Until
// v0.4.1 the embedder bound at open: after a pull the engine kept answering
// text+graph and embedPending said MODEL_MISSING until close/open — Homun
// restarted the engine in production to work around it. Now the pull ends
// with the model verified AND loaded: modelStatus().present, hybrid search
// on the vectors, embedPending working, no close/open. The model files come
// from the real ENGRAMMA_MODEL_DIR, MIRRORED AS SYMLINKS into a temp
// modelDir: the pull finds them present, verifies the pinned sha256 and
// skips — no network, no copy (the disk is tight). Cells opened BEFORE the
// pull pick the embedder up: they read the engine's slot when they run.
test('pullModel attaches the embedder to the OPEN engine (P1.4)', async (t) => {
  const modelRoot = process.env.ENGRAMMA_MODEL_DIR
    ? path.resolve(process.env.ENGRAMMA_MODEL_DIR)
    : path.join(require('node:os').homedir(), '.cache', 'engramma', 'models');
  const real = path.join(modelRoot, 'granite-embedding-97m-multilingual-r2');
  const hasModel = fs.existsSync(real)
    && fs.readdirSync(real).length > 0
    && fs.readdirSync(path.join(real, fs.readdirSync(real)[0])).length > 0;
  if (!hasModel) {
    return t.skip(`the builtin model is not in ${modelRoot}: pull it first`);
  }
  const mirrorModel = (src, dst) => {
    fs.mkdirSync(dst, { recursive: true });
    for (const ent of fs.readdirSync(src, { withFileTypes: true })) {
      const s = path.join(src, ent.name);
      const d = path.join(dst, ent.name);
      if (ent.isDirectory()) mirrorModel(s, d);
      else if (!fs.existsSync(d)) fs.symlinkSync(s, d);
    }
  };

  const rootDir = root();
  const modelDir = path.join(path.dirname(rootDir), 'modelli-a-caldo');
  const e = await Engine.open({
    root: rootDir,
    modelDir,
    embedder: 'builtin',
    actor: 'homun',
  });
  // the model is NOT there: text+graph, and the honest errors
  assert.equal((await e.modelStatus()).present, false);
  const cell = await e.cell('caldo'); // opened BEFORE the pull, like Homun's
  const n1 = await cell.put({ body: 'il trasloco di fabio a milano', source: 's', confidence: 'stated' });
  const n2 = await cell.put({ body: 'fabio lavora allo studio aurora', source: 's', confidence: 'stated' });
  await assert.rejects(cell.embedPending(), (err) => err.code === 'MODEL_MISSING');
  await assert.rejects(
    cell.search({ text: 'trasloco', mode: 'vector' }),
    (err) => err.code === 'MODEL_MISSING',
  );
  // the pull: the mirrored files verify and skip, and the engine (OPEN)
  // loads the embedder
  mirrorModel(real, path.join(modelDir, 'granite-embedding-97m-multilingual-r2'));
  let progressCalls = 0;
  await e.pullModel(() => { progressCalls += 1; });
  assert.ok(progressCalls > 0, 'the progress callback ran');
  const status = await e.modelStatus();
  assert.equal(status.present, true, JSON.stringify(status));
  assert.ok(status.bytes > 0);
  // embedPending works NOW, on the cell opened before the pull
  const done = await cell.embedPending();
  assert.equal(done, 2, `${done}`);
  // the hybrid search runs on the vectors: the explain says so
  const hits = await cell.search({ text: 'dove vive fabio', k: 2, explain: true });
  assert.ok(hits.length >= 1);
  assert.ok(hits.some((h) => h.explain && typeof h.explain.semanticRank === 'number'),
    'no hit carries a semantic rank');
  assert.equal(hits[0].id, n1.id);
  // and the engine keeps working after (no close/open anywhere above)
  await cell.put({ body: 'una nota dopo il collegamento', source: 's', confidence: 'stated' });
  assert.equal(await cell.embedPending(), 1);
  const _ = n2;
  await e.close();
});

// v0.4.2 (P1.5): ENGRAMMA_PULL_BASE replaces the download base of
// pullModel — Homun's request 1: offline installs and closed-environment
// mirrors, like the CLI tests already do with a local server. The pinned
// revision and the per-file sha256 stay attached: a different base can
// only deliver the SAME bytes or fail. A local server serves the model
// files from the real ENGRAMMA_MODEL_DIR; one file arrives ALTERED → the
// pull is an error, nothing lands, the model is NOT present.
test('ENGRAMMA_PULL_BASE: a local base, an altered file refuses to land (P1.5)', async (t) => {
  const modelRoot = process.env.ENGRAMMA_MODEL_DIR
    ? path.resolve(process.env.ENGRAMMA_MODEL_DIR)
    : path.join(require('node:os').homedir(), '.cache', 'engramma', 'models');
  const real = path.join(modelRoot, 'granite-embedding-97m-multilingual-r2');
  if (!fs.existsSync(real)) {
    return t.skip(`the builtin model is not in ${modelRoot}: pull it first`);
  }
  // the server serves the pinned paths ({repo}/resolve/{revision}/{file})
  // straight from the real model folder, with ONE file altered
  const requests = [];
  const server = require('node:http').createServer((req, res) => {
    requests.push(req.url);
    const m = req.url.match(/^\/.+\/resolve\/([^/]+)\/(.+)$/);
    const rev = m ? m[1] : '';
    const rel = m ? m[2] : '';
    // the on-disk layout is <id>/<revision>/<file>
    const file = path.join(real, rev, rel);
    if (!m || !fs.existsSync(file)) {
      res.writeHead(404).end('gone');
      return;
    }
    const raw = fs.readFileSync(file);
    // the FIRST file (tokenizer.json, the spec's own order) arrives altered
    const body = rel === 'tokenizer.json' ? Buffer.concat([raw, Buffer.from('X')]) : raw;
    res.writeHead(200, { 'content-length': body.length }).end(body);
  });
  await new Promise((done) => server.listen(0, '127.0.0.1', done));
  const { port } = server.address();
  process.env.ENGRAMMA_PULL_BASE = `http://127.0.0.1:${port}`;

  try {
    const rootDir = root();
    const modelDir = path.join(path.dirname(rootDir), 'modelli-base');
    const e = await Engine.open({
      root: rootDir,
      modelDir,
      embedder: 'none',
      actor: 'homun',
    });
    assert.equal((await e.modelStatus()).present, false);
    await assert.rejects(
      e.pullModel(),
      (err) => err.code === 'IO' && /sha256/.test(err.message),
    );
    // the base was OURS: the request hit the pinned path on 127.0.0.1
    assert.ok(requests.length >= 1, 'the pull never asked our base');
    assert.match(requests[0], /^\/ibm-granite\/granite-embedding-97m-multilingual-r2\/resolve\/835ad14087e140460703cf0fae09f97d469d65c2\//);
    assert.ok(requests[0].endsWith('/tokenizer.json'));
    // nothing landed: no partial file, the model is not present
    assert.equal((await e.modelStatus()).present, false);
    const leftovers = fs.existsSync(modelDir)
      ? fs.readdirSync(path.join(modelDir, 'granite-embedding-97m-multilingual-r2'), { recursive: true })
      : [];
    assert.ok(
      leftovers.every((p) => !String(p).includes('.part') && !String(p).endsWith('tokenizer.json')),
      `${leftovers}`,
    );
    await e.close();
  } finally {
    delete process.env.ENGRAMMA_PULL_BASE;
    server.close();
  }
});

// v0.6.4 (D3): the contract's shapes on a quiet cell — an empty list,
// and the two refusals.
test('conflicts(): empty on a quiet cell; resolveConflict refuses cleanly', async () => {
  const e = await engine();
  try {
    const cell = await e.cell('quieta');
    assert.deepEqual(await cell.conflicts(), []);
    await assert.rejects(
      cell.resolveConflict('01AAAAAAAAAAAAAAAAAAAAAAAA', { keep: 'original' }),
      (err) => err.code === 'NOT_FOUND',
    );
    await assert.rejects(
      cell.resolveConflict('01AAAAAAAAAAAAAAAAAAAAAAAA', { keep: 'nonsense' }),
      (err) => err.code === 'INVALID_ARGUMENT',
    );
  } finally {
    await e.close();
  }
});

// v0.6.9 (I1): «forgotten forever» means the id does not come back. A
// purged id answers NOTE_PURGED on every write that touches it — the
// path is real with Homun: re-importing an old export or a backup that
// still holds the purged note does exactly this put. To write similar
// content again takes a NEW note; the import report names the skipped
// ids as `skippedPurged` (the CLI's JSON — docs/node.md tells how
// Homun reads it).
test('NOTE_PURGED: a purged id never comes back (I1)', async () => {
  const e = await engine();
  try {
    const cell = await e.cell('oblio-js');
    const n = await cell.put({
      body: 'segreto quokkaviolaceo zibellinotremante',
      title: 'Segreto',
      source: 's',
      confidence: 'stated',
    });
    const purged = await cell.purge(n.id, 'diritto all’oblio');
    assert.equal(purged.scrubbed, true, `${JSON.stringify(purged)}`);

    // the same id again (an old export, a backup): refused, by code,
    // with the id in the message
    await assert.rejects(
      cell.put({ id: n.id, body: 'il ritorno', source: 's', confidence: 'stated' }),
      // (the engine speaks ULIDs upper-case, the binding lower-case:
      // the compare is case-blind)
      (err) =>
        err.code === 'NOTE_PURGED' &&
        err.message.toUpperCase().includes(n.id.toUpperCase()),
    );
    await assert.rejects(
      cell.patch(n.id, { body: 'il ritorno in patch' }),
      (err) => err.code === 'NOTE_PURGED',
    );
    // a NEW id writes normally: the refusal is about the id, not the pen
    const fresh = await cell.put({ body: 'appunto nuovo', source: 's', confidence: 'stated' });
    assert.equal((await cell.get(fresh.id)).body, 'appunto nuovo');
    // verify stays green, and the report says nothing was repaired
    const v = await cell.verify();
    assert.equal(v.ok, true, `${JSON.stringify(v)}`);
    assert.deepEqual(v.phantomPurged, [], `${JSON.stringify(v)}`);
  } finally {
    await e.close();
  }
});

// v0.6.10 (J2): verify() è una diagnosi in sola lettura e la riparazione
// è un passo esplicito. La cella del fixture v0.6.8 (una riga viva su un
// id purgato, scritta dal motore vecchio) viene copiata nella radice:
// verify() non cambia un byte, nomina il fantasma e dice che serve
// repair; repair() sposta il contenuto sotto un id nuovo, dimentica il
// vecchio, restituisce la mappa a chi l'ha chiamata — e non la salva
// da nessuna parte.
test('verify signals the phantom, repair moves it under a new id (J2)', async () => {
  const rootDir = root();
  const cellsDir = path.join(rootDir, 'cells');
  fs.mkdirSync(cellsDir, { recursive: true });
  const golden = path.join(
    __dirname, '..', '..', 'engramma-core', 'tests', 'golden', 'v068-put-after-purge.sqlite',
  );
  fs.copyFileSync(golden, path.join(cellsDir, 'fantasma-j2.sqlite'));
  const e = await Engine.open({ root: rootDir, embedder: 'none', actor: 'homun' });
  const cell = await e.cell('fantasma-j2');
  try {
    // the phantom is the live state the v0.6.8 engine left
    const phantom = '01' + '0'.repeat(16) + '00000895';
    assert.notEqual(await cell.get(phantom), null, 'the fixture holds the phantom row');

    // the diagnosis: it names the phantom, it says repair is needed,
    // and it changes nothing (judged with the cell open: closing it may
    // checkpoint a pre-existing WAL, and the close is not verify's act)
    const before = fs.readFileSync(path.join(cellsDir, 'fantasma-j2.sqlite'));
    const v = await cell.verify();
    assert.equal(v.ok, false, JSON.stringify(v));
    assert.deepEqual(v.phantomPurged, [phantom], JSON.stringify(v));
    const after = fs.readFileSync(path.join(cellsDir, 'fantasma-j2.sqlite'));
    assert.deepEqual(after, before, 'verify is read-only: not one byte moves');

    // the repair: the content lives under a NEW id, the map says which
    const r = await cell.repair();
    assert.equal(r.moved.length, 1, JSON.stringify(r));
    const [m] = r.moved;
    assert.equal(m.old, phantom);
    assert.match(m.new, /^[0-9A-HJKMNP-TV-Z]{26}$/);
    assert.notEqual(m.new, phantom);
    const moved = await cell.get(m.new);
    assert.equal(moved.body, 'quokkaviolaceo zibellinotremante ritorno');
    assert.equal(moved.title, 'la nota che non torna');
    assert.equal(await cell.get(phantom), null, 'the old id is gone');
    const s = await cell.search({ text: 'quokkaviolaceo', k: 10 });
    assert.ok(s.every((h) => h.id !== phantom), 'the search answers the new id only');
    assert.ok(s.some((h) => h.id === m.new), 'the moved content is findable');

    // verify is green after, and repairing again does nothing
    const v2 = await cell.verify();
    assert.equal(v2.ok, true, JSON.stringify(v2));
    assert.deepEqual(v2.phantomPurged, []);
    const r2 = await cell.repair();
    assert.deepEqual(r2.moved, [], 'the repair is once');
  } finally {
    await e.close();
  }
});

// v0.6.10 (J3): the refusal itself. A fake binary — or an old one —
// pointed at by ENGRAMMA_BIN makes the bench refuse to start with a
// message that names both sides, instead of dropping three random view
// tests with a schema error nobody asked for.
test('the bench refuses a CLI that is not the addon\'s own build (J3)', async () => {
  const info = buildInfo();
  assert.match(info.commit, /^[0-9a-f]+(-dirty)?$/);
  assert.equal(typeof info.schema, 'number');

  // a fake binary that speaks the OLD shape: the refusal names it
  const stub = path.join(TMP, 'engramma-vecchio.sh');
  fs.writeFileSync(stub, '#!/bin/sh\necho "engramma 0.1.0-dev (commit deadbee, schema 20)"\n');
  fs.chmodSync(stub, 0o755);
  assert.throws(
    () => assertCliMatchesAddon(stub, info),
    (err) =>
      err.message.includes('deadbee') &&
      err.message.includes('schema 20') &&
      err.message.includes(info.commit) &&
      err.message.includes('ricostruisci o imposta ENGRAMMA_BIN'),
    'the message names both sides and the way out',
  );

  // a binary that says nothing about its commit: refused all the same
  const mute = path.join(TMP, 'engramma-muto.sh');
  fs.writeFileSync(mute, '#!/bin/sh\necho "engramma 0.1.0-dev"\n');
  fs.chmodSync(mute, 0o755);
  assert.throws(() => assertCliMatchesAddon(mute, info), /non dice il suo commit/);

  // one that agrees: nothing happens (the suite runs)
  const good = path.join(TMP, 'engramma-uguale.sh');
  fs.writeFileSync(
    good,
    `#!/bin/sh\necho "engramma 0.1.0-dev (commit ${info.commit}, schema ${info.schema})"\n`,
  );
  fs.chmodSync(good, 0o755);
  const parsed = assertCliMatchesAddon(good, info);
  assert.equal(parsed.commit, info.commit);
  assert.equal(parsed.schema, info.schema);
});
