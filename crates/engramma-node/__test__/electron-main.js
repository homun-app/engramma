// SPDX-License-Identifier: Apache-2.0

// v0.4.1: the daemon of Homun is an Electron process without a window, NOT
// a Node run (`ELECTRON_RUN_AS_NODE`). This is the proof the module loads
// and works in a real MAIN process: it opens an engine, writes, searches,
// closes, checks CLOSED and exits 0 — `npm run test:electron-main`.

const { app } = require('electron');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

app.whenReady().then(async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'engramma-electron-'));
  let code = 0;
  try {
    const { Engine, version } = require('..');
    const engine = await Engine.open({
      root: path.join(tmp, 'root'),
      embedder: 'none',
      actor: 'homun',
    });
    const cell = await engine.cell('utente', { actor: 'homun:bot-7' });
    const note = await cell.put({
      noteType: 'fact',
      title: 'Casa di Fabio',
      body: 'Fabio vive a [[Milano]] dal 2024.',
      tags: ['casa'],
      source: 'homun:chat/7/e1',
      confidence: 'stated',
      extensions: { 'x-homun': { tier: 'core', scope: 'user' } },
    });
    if (note.status !== 'active') throw new Error(`status inatteso: ${note.status}`);
    if (note.extensions['x-homun'].tier !== 'core') throw new Error('extensions perse');

    const hits = await cell.search({ text: 'dove abita Fabio' });
    if (!hits.some((h) => h.id === note.id)) throw new Error('la ricerca non trova la nota');
    if (hits[0].noteType !== 'fact' || hits[0].status !== 'active') {
      throw new Error('SearchHit senza i campi della riga');
    }

    await engine.close();
    let closed = false;
    try {
      await cell.get(note.id);
    } catch (err) {
      closed = err.code === 'CLOSED';
    }
    if (!closed) throw new Error('get dopo close non ha risposto CLOSED');

    console.log(`electron-main ok — engramma-node ${version()}`);
  } catch (err) {
    console.error('electron-main FAILED:', err);
    code = 1;
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
    app.exit(code);
  }
});
