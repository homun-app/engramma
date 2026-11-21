// SPDX-License-Identifier: Apache-2.0

// v0.6.2 (S2): the sync API of the Engine — the service of S1 behind a
// JS contract. These are the SAME-PROCESS parts: the shapes, the stable
// codes, the sharing rules, `dropCell`'s CONFLICT. The pairing across
// two Node processes is S3 (sync-pair.test.js); the events and the
// lifecycle are S4.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { Engine } = require('..');

// v0.6.11 (K3): the bench folder of this file — its own, new, and
// cleaned at the end of the run (failures included)
const TMP = require('./tmpdir.js').testTmp('sync');
const root = () => {
  const dir = path.join(TMP, `s-${Math.random().toString(36).slice(2, 8)}`);
  fs.rmSync(dir, { recursive: true, force: true });
  return dir;
};

async function engine() {
  return Engine.open({ root: root(), embedder: 'none', actor: 'homun' });
}

test('sync(): one service per Engine, status and devices before anything', async () => {
  const e = await engine();
  const sync = await e.sync({ deviceName: 'laptop', relay: 'none' });
  assert.ok(sync, 'sync() answers a Sync object');
  // the same root, the same service: a second sync() is the same story
  const again = await e.sync();
  const a = await sync.status();
  const b = await again.status();
  assert.equal(a.device.shortId, b.device.shortId, 'one service per Engine');
  assert.equal(a.serving, false, 'the continuous service starts off');
  assert.equal(a.relay, 'none', 'the relay choice is saved in the root');
  assert.deepEqual(a.cells, [], 'nothing is shared yet');
  // the first device names itself as the caller asked — at the pairing,
  // not yet (the list is empty until an invite or a share creates it)
  assert.deepEqual(await sync.devices(), []);
  await e.close();
});

test('share/unshare/status/runOnce: the shapes', async () => {
  const e = await engine();
  const sync = await e.sync({ relay: 'none' });
  const cell = await e.cell('memoria');
  await cell.put({ body: 'prima nota', source: 'homun', confidence: 'stated' });
  await sync.share('memoria');
  let status = await sync.status();
  assert.equal(status.cells.length, 1);
  const row = status.cells[0];
  assert.equal(row.name, 'memoria');
  assert.equal(row.shared, true);
  assert.equal(row.epoch, 0);
  assert.equal(typeof row.devices, 'number');
  assert.ok(row.pending >= 0);
  assert.equal(row.unsent, 0);
  // the device list has the first device now (the share created it,
  // with the default name)
  const devices = await sync.devices();
  assert.equal(devices.length, 1);
  assert.equal(devices[0].name, 'portatile');
  assert.equal(devices[0].isSelf, true);
  assert.equal(devices[0].live, true);
  // a pass with no peers: a report, never an error
  const pass = await sync.runOnce();
  assert.ok(Array.isArray(pass.devices));
  assert.equal(pass.cells.length, 1);
  assert.equal(pass.cells[0].cell, 'memoria');
  assert.ok(pass.cells[0].frontier >= 0);
  // unshare, and unshare again: NOT_SHARED
  await sync.unshare('memoria');
  assert.deepEqual((await sync.status()).cells, []);
  await assert.rejects(sync.unshare('memoria'), (err) => err.code === 'NOT_SHARED');
  await e.close();
});

test('dropCell on a shared cell answers CONFLICT; after unshare it works', async () => {
  const e = await engine();
  const sync = await e.sync({ relay: 'none' });
  const cell = await e.cell('segreta');
  await cell.put({ body: 'nota da dimenticare', source: 'homun', confidence: 'stated' });
  await sync.share('segreta');
  await assert.rejects(
    e.dropCell('segreta', 'oblio'),
    (err) => err.code === 'CONFLICT' && /unshare/.test(err.message),
    'the message says to call unshare first',
  );
  // the cell is still there: the refusal did not delete anything
  assert.ok((await e.cells()).includes('segreta'));
  await sync.unshare('segreta');
  const view = await e.dropCell('segreta', 'oblio');
  assert.equal(typeof view.notes, 'number');
  await e.close();
});

test('the short code without a mailbox answers NO_MAILBOX', async () => {
  const e = await engine();
  const sync = await e.sync({ relay: 'none' });
  await assert.rejects(sync.code(), (err) => err.code === 'NO_MAILBOX');
  await e.close();
});

test('start twice changes nothing; stop on a stopped service changes nothing', async () => {
  const e = await engine();
  const sync = await e.sync({ relay: 'none' });
  const first = await sync.start();
  assert.equal(typeof first.node, 'string');
  assert.ok(Array.isArray(first.addrs) && first.addrs.length > 0);
  const second = await sync.start();
  assert.equal(second.node, first.node, 'the second start is the same service');
  let status = await sync.status();
  assert.equal(status.serving, true);
  await sync.stop();
  await sync.stop(); // stopping a stopped service: nothing
  status = await sync.status();
  assert.equal(status.serving, false);
  await e.close();
});

test('after close, sync answers CLOSED', async () => {
  const e = await engine();
  const sync = await e.sync({ relay: 'none' });
  await sync.start();
  await e.close();
  // the calls throw at CALL time (the gate is checked on the JS thread):
  // an async wrapper turns that into the rejection to catch
  const closed = async (call) => {
    try {
      await call();
      assert.fail('expected CLOSED');
    } catch (err) {
      assert.equal(err.code, 'CLOSED', `message: ${err.message}`);
    }
  };
  await closed(() => sync.status());
  await closed(() => sync.runOnce());
  await closed(() => sync.stop());
  // closing twice is not an error, and the service went with the first
  await e.close();
});

test('S4: stop() during a pass concludes within 5 s', async () => {
  const e = await engine();
  const sync = await e.sync({ relay: 'none' });
  await sync.start();
  const cell = await e.cell('memoria');
  await cell.put({ body: 'scritta che parte', source: 'homun', confidence: 'stated' });
  // a pass fires ~2.5 s after the write (watch + debounce): stop right
  // in the middle of it — it must conclude well within the bound
  await new Promise((r) => setTimeout(r, 2600));
  const t0 = Date.now();
  await sync.stop();
  const elapsed = Date.now() - t0;
  assert.ok(elapsed < 5000, `stop() ha impiegato ${elapsed} ms`);
  assert.equal((await sync.status()).serving, false);
  await e.close();
});

test('a second Engine on the same root answers SYNC_BUSY at start', async () => {
  const dir = root();
  const e1 = await Engine.open({ root: dir, embedder: 'none', actor: 'homun' });
  const e2 = await Engine.open({ root: dir, embedder: 'none', actor: 'homun' });
  const s1 = await e1.sync({ relay: 'none' });
  const s2 = await e2.sync({ relay: 'none' });
  await s1.start();
  await assert.rejects(s2.start(), (err) => err.code === 'SYNC_BUSY');
  // a single pass takes the same lock
  await assert.rejects(s2.runOnce(), (err) => err.code === 'SYNC_BUSY');
  await s1.stop();
  // the lock went: the second engine may pass now
  const pass = await s2.runOnce();
  assert.ok(Array.isArray(pass.devices));
  await e1.close();
  await e2.close();
});

// ---------------------------------------------------------------------------
// v0.6.3 (C2): a listener must not keep a Node process alive. The child
// below registers onEvent, closes the engine and lets its script END: the
// process must exit by itself, and the unsubscribe must not throw after
// close. Before the fix the ThreadsafeFunction was referenced and close()
// left it in the hub: the child never exited.
// ---------------------------------------------------------------------------

const { spawn } = require('node:child_process');

/**
 * One child that registers onEvent, (maybe) starts, closes and ends.
 * `unsubscribe` says whether it ever calls the off function — the hang the
 * fix closes is the child that NEVER removes its listener.
 */
async function listenerChild(withStart, unsubscribe) {
  const dir = fs.mkdtempSync(path.join(TMP, 'c2-'));
  const script = `
    const { Engine } = require(${JSON.stringify(require.resolve('..'))});
    (async () => {
      const e = await Engine.open({
        root: ${JSON.stringify(dir)}, embedder: 'none', actor: 't',
      });
      const sync = await e.sync({ deviceName: 'laptop', relay: 'none' });
      const off = sync.onEvent(() => {});
      ${withStart ? 'await sync.start();' : ''}
      await e.close();
      ${unsubscribe ? 'off(); // after close() the unsubscribe must not throw' : '/* no removal: the listener stays */'}
      console.log('CLOSED');
    })().catch((err) => {
      console.error(err && err.stack ? err.stack : err);
      process.exitCode = 3;
    });
  `;
  const child = spawn(process.execPath, ['-e', script], {
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  let closedAt = null;
  child.stdout.on('data', (d) => {
    if (String(d).includes('CLOSED')) closedAt = Date.now();
  });
  const exit = await new Promise((resolve, reject) => {
    const killer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error('il figlio non è uscito da solo'));
    }, 15000);
    killer.unref?.();
    child.on('exit', (code) => {
      clearTimeout(killer);
      resolve(code);
    });
    child.on('error', reject);
  });
  fs.rmSync(dir, { recursive: true, force: true });
  return { exit, closedAt };
}

test('onEvent + close(), WITHOUT removal: the process exits within 2 s', async () => {
  const { exit, closedAt } = await listenerChild(false, false);
  assert.equal(exit, 0, 'the child closed cleanly');
  assert.ok(closedAt, 'the child reached the end of close()');
  // from the end of close() the process owes nothing to anyone — the
  // listener alone must not hold it (the reviewer measured a hang)
  assert.ok(Date.now() - closedAt < 2000, `uscito in ${Date.now() - closedAt} ms dal close`);
});

test('onEvent + start() + close(), without removal: same exit within 2 s', async () => {
  const { exit, closedAt } = await listenerChild(true, false);
  assert.equal(exit, 0, 'the child closed cleanly');
  assert.ok(closedAt, 'the child reached the end of close()');
  assert.ok(Date.now() - closedAt < 2000, `uscito in ${Date.now() - closedAt} ms dal close`);
});

test('after close() the unsubscribe does not throw', async () => {
  const { exit, closedAt } = await listenerChild(false, true);
  assert.equal(exit, 0, 'off() after close() is not an error');
  assert.ok(closedAt, 'the child reached the end of close()');
  assert.ok(Date.now() - closedAt < 2000, `uscito in ${Date.now() - closedAt} ms dal close`);
});
