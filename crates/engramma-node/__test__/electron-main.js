// SPDX-License-Identifier: Apache-2.0

// v0.4.1: the daemon of Homun is an Electron process without a window, NOT
// a Node run (`ELECTRON_RUN_AS_NODE`). This is the proof the module loads
// and works in a real MAIN process: it opens an engine, writes, searches,
// closes, checks CLOSED and exits 0 — `npm run test:electron-main`.
// v0.6.2 (S4): the sync service too, started before the quit.
// v0.6.3 (C3): the quit is `app.quit()` itself — no `app.exit` force: the
// exit code travels in `process.exitCode`, the events listener stays
// registered, a shared cell holds a write, and the RUNNER bounds the whole
// process at 10 s (a daemon that exits must not hang on a thread of ours).

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

    // v0.6.3 (C3): the S4 part — a shared cell with a write, the events
    // listener registered, the service ON, then `app.quit()` for real: the
    // process must END on its own, and the runner's 10 s bound is what
    // proves it (no app.exit anywhere in this file)
    const e2 = await Engine.open({
      root: path.join(tmp, 'root-sync'),
      embedder: 'none',
      actor: 'homun',
    });
    const sync = await e2.sync({ deviceName: 'portatile', relay: 'none' });
    const shared = await e2.cell('memoria');
    await shared.put({ body: 'scritta condivisa', source: 'homun', confidence: 'stated' });
    await sync.share('memoria');
    sync.onEvent(() => {}); // stays registered: the quit must not owe it anything
    const info = await sync.start();
    if (!info.node) throw new Error('il servizio non ha pubblicato il nodo');
    console.log(`electron-main ok — engramma-node ${version()}`);
  } catch (err) {
    console.error('electron-main FAILED:', err);
    code = 1;
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
    // the code crosses without app.exit: Electron propagates
    // process.exitCode through app.quit()
    process.exitCode = code;
    app.quit();
  }
});
