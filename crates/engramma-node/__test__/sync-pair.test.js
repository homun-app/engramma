// SPDX-License-Identifier: Apache-2.0

// v0.6.2 (S3): the pairing across TWO Node processes, on loopback
// without a relay — the SAS compared by the test and confirmed from both
// sides, the refusals (one side says no, nobody answers, the ticket
// expires by hand), confirm before the digits, the QR form entering.
// The S4 event tests (arrived, removed, the resting cycle) live here
// too: they need two processes with the service on.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const readline = require('node:readline');
const { spawn } = require('node:child_process');

const TMP = process.env.ENGRAMMA_TEST_TMP
  ? process.env.ENGRAMMA_TEST_TMP
  : fs.mkdtempSync(path.join(require('node:os').tmpdir(), 'engramma-node-'));

/** One device, its own Node process. */
class Peer {
  constructor(env = {}) {
    this.child = spawn(process.execPath, [path.join(__dirname, 'sync-peer.js')], {
      cwd: __dirname,
      stdio: ['pipe', 'pipe', 'inherit'],
      env: { ...process.env, ...env },
    });
    this.rl = readline.createInterface({ input: this.child.stdout });
    this.next = 1;
    this.pending = new Map();
    this.rl.on('line', (line) => {
      if (!line.trim()) return;
      const res = JSON.parse(line);
      const settle = this.pending.get(res.id);
      if (settle) {
        this.pending.delete(res.id);
        settle(res);
      }
    });
  }

  cmd(op, args = {}) {
    const id = this.next++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, (res) => {
        if (res.ok) resolve(res.out);
        else {
          const err = new Error(res.message);
          err.code = res.code;
          reject(err);
        }
      });
      this.child.stdin.write(`${JSON.stringify({ id, op, args })}\n`);
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new Error(`«${op}» non ha risposto in tempo`));
        }
      }, 120000).unref();
    });
  }

  async quit() {
    try {
      await this.cmd('quit');
    } catch {
      /* the pipe may close first */
    }
    this.child.kill();
    await new Promise((resolve) => {
      if (this.child.exitCode !== null) resolve();
      else this.child.on('exit', resolve);
    });
  }
}

const rootName = () => path.join(TMP, `p-${Math.random().toString(36).slice(2, 8)}`);

test('S3: full pairing between two Node processes, the SAS compared here', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    await a.cmd('cell', { cell: 'memoria' });
    await a.cmd('put', { cell: 'memoria', body: 'la nota che attraversa' });
    // the invite FIRST (it names the list as the caller asked), then the
    // share, then the join: the cells stream after the SAS
    const invite = await a.cmd('invite', { key: 'p' });
    await a.cmd('share', { cell: 'memoria' });
    assert.match(invite.ticket, /^engramma-invite-v1\./);
    assert.ok(invite.ticketQr);
    assert.equal(invite.rounds, 1, 'a ticket pairing shows one SAS');
    await b.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    const sasA = (await a.cmd('sas', { key: 'p' })).sas;
    const sasB = (await b.cmd('sas', { key: 'p' })).sas;
    assert.match(sasA, /^\d{6}$/, 'six digits');
    assert.equal(sasA, sasB, 'the two screens show the same digits');
    await a.cmd('confirm', { same: true, key: 'p' });
    await b.cmd('confirm', { same: true, key: 'p' });
    const doneA = await a.cmd('done', { key: 'p' });
    const doneB = await b.cmd('done', { key: 'p' });
    assert.equal(doneA.peer.name, 'mac');
    assert.equal(doneB.peer.name, 'laptop');
    assert.ok(doneB.cells.some((c) => c.name === 'memoria' && c.ops >= 1));
    const devices = await a.cmd('devices');
    assert.ok(devices.some((d) => d.name === 'mac' && d.isSelf === false));
    assert.ok(devices.some((d) => d.name === 'laptop' && d.isSelf === true));
  } finally {
    await a.quit();
    await b.quit();
  }
});

test('S3: the QR form wears the QR alphabet and enters', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    const invite = await a.cmd('invite', { key: 'p' });
    assert.ok(
      [...invite.ticketQr].every((c) => /[0-9A-Z]/.test(c) || '$%*+-./:'.includes(c)),
      `solo caratteri alfanumerici del QR: ${invite.ticketQr}`,
    );
    await b.cmd('join', { ticketOrCode: invite.ticketQr, key: 'p' });
    const sasA = (await a.cmd('sas', { key: 'p' })).sas;
    const sasB = (await b.cmd('sas', { key: 'p' })).sas;
    assert.equal(sasA, sasB);
    await a.cmd('confirm', { same: true, key: 'p' });
    await b.cmd('confirm', { same: true, key: 'p' });
    await a.cmd('done', { key: 'p' });
    await b.cmd('done', { key: 'p' });
    const devices = await a.cmd('devices');
    assert.ok(devices.some((d) => d.name === 'mac'), 'the QR form entered');
  } finally {
    await a.quit();
    await b.quit();
  }
});

test('S3: a refused SAS leaves PAIRING_REJECTED on both sides, no add', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    const invite = await a.cmd('invite', { key: 'p' });
    await b.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    await a.cmd('sas', { key: 'p' });
    await b.cmd('sas', { key: 'p' });
    // THIS side says no
    await a.cmd('confirm', { same: false, key: 'p' });
    await b.cmd('confirm', { same: true, key: 'p' });
    await assert.rejects(a.cmd('done', { key: 'p' }), (err) => err.code === 'PAIRING_REJECTED');
    await assert.rejects(b.cmd('done', { key: 'p' }), (err) => err.code === 'PAIRING_REJECTED');
    const devices = await a.cmd('devices');
    assert.ok(!devices.some((d) => d.name === 'mac'), 'no add landed');
  } finally {
    await a.quit();
    await b.quit();
  }
});

test('S3: an unanswered SAS times out (short window), nobody hangs', async () => {
  const a = new Peer({ ENGRAMMA_SAS_TIMEOUT_MS: '1500' });
  const b = new Peer({ ENGRAMMA_SAS_TIMEOUT_MS: '1500' });
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    const invite = await a.cmd('invite', { key: 'p' });
    await b.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    await a.cmd('sas', { key: 'p' });
    await b.cmd('sas', { key: 'p' });
    // nobody answers: both time out on their own, none hangs
    await assert.rejects(a.cmd('done', { key: 'p' }), (err) => err.code === 'PAIRING_TIMEOUT');
    await assert.rejects(b.cmd('done', { key: 'p' }), (err) => err.code === 'PAIRING_TIMEOUT');
  } finally {
    await a.quit();
    await b.quit();
  }
});

test('S3: confirm before the digits is INVALID_ARGUMENT', async () => {
  const a = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await a.cmd('invite', { key: 'p' });
    // no joiner has knocked: the digits cannot be out
    await assert.rejects(a.cmd('confirm', { same: true, key: 'p' }), (err) =>
      err.code === 'INVALID_ARGUMENT',
    );
  } finally {
    await a.quit();
  }
});

test('S3: cancel closes this side, the other hears a clean refusal', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    const invite = await a.cmd('invite', { key: 'p' });
    await b.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    await a.cmd('sas', { key: 'p' });
    await b.cmd('sas', { key: 'p' });
    a.cmd('cancel', { key: 'p' });
    await assert.rejects(a.cmd('done', { key: 'p' }), (err) => err.code === 'CANCELLED');
    // B's human still answers yes — and meets a closed wire
    await b.cmd('confirm', { same: true, key: 'p' });
    await assert.rejects(b.cmd('done', { key: 'p' }), (err) => err.code === 'PAIRING_REJECTED');
  } finally {
    await a.quit();
    await b.quit();
  }
});

// ---------------------------------------------------------------------------
// S4: the events and the lifecycle, across the two processes with the
// service on both
// ---------------------------------------------------------------------------

test('S4: a put on A arrives on B as an event, and the note is readable', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    await a.cmd('cell', { cell: 'memoria' });
    await a.cmd('put', { cell: 'memoria', body: 'la base' });
    const invite = await a.cmd('invite', { key: 'p' });
    await a.cmd('share', { cell: 'memoria' });
    await b.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    await a.cmd('sas', { key: 'p' });
    await b.cmd('sas', { key: 'p' });
    await a.cmd('confirm', { same: true, key: 'p' });
    await b.cmd('confirm', { same: true, key: 'p' });
    await a.cmd('done', { key: 'p' });
    await b.cmd('done', { key: 'p' });
    await b.cmd('cell', { cell: 'memoria' });
    // both services on; the books learn the other's live address the way
    // the Rust tests do it: by hand, on loopback (the pairing-era
    // endpoints die with the pairing)
    const infoA = await a.cmd('start');
    const infoB = await b.cmd('start');
    await b.cmd('remember', { device: 'laptop', serve: infoA });
    await a.cmd('remember', { device: 'mac', serve: infoB });
    await b.cmd('events-on');
    // the write, from A's OWN process (a different connection on the same
    // file: the guardian must see it — the C4 lesson, seen from JS)
    await a.cmd('put', { cell: 'memoria', body: 'la nota che attraversa js' });
    const { event } = await b.cmd('wait-event', { type: 'arrived', timeoutMs: 5000 });
    assert.ok(event, 'B never said the arrival out loud');
    assert.equal(event.cell, 'memoria');
    assert.ok(event.count >= 1);
    // and the note is READABLE on B (within the same bound: the event
    // lands when the session ENDS, the list right after)
    let list = [];
    for (let i = 0; i < 50; i++) {
      list = await b.cmd('list', { cell: 'memoria' });
      if (list.some((n) => n.body === 'la nota che attraversa js')) break;
      await new Promise((r) => setTimeout(r, 200));
    }
    assert.ok(
      list.some((n) => n.body === 'la nota che attraversa js'),
      'the note is readable on B',
    );
  } finally {
    await a.quit();
    await b.quit();
  }
});

test('S4: thirty seconds of silence, zero pass events (the resting cycle)', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    await a.cmd('cell', { cell: 'memoria' });
    await a.cmd('put', { cell: 'memoria', body: 'la base' });
    const invite = await a.cmd('invite', { key: 'p' });
    await a.cmd('share', { cell: 'memoria' });
    await b.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    await a.cmd('sas', { key: 'p' });
    await b.cmd('sas', { key: 'p' });
    await a.cmd('confirm', { same: true, key: 'p' });
    await b.cmd('confirm', { same: true, key: 'p' });
    await a.cmd('done', { key: 'p' });
    await b.cmd('done', { key: 'p' });
    const infoA = await a.cmd('start');
    const infoB = await b.cmd('start');
    await b.cmd('remember', { device: 'laptop', serve: infoA });
    await a.cmd('remember', { device: 'mac', serve: infoB });
    await a.cmd('events-on');
    await b.cmd('events-on');
    // one write first: it crosses, the cycle runs — then it must STOP
    await a.cmd('put', { cell: 'memoria', body: 'lultima scritta' });
    await a.cmd('wait-event', { type: 'pass', timeoutMs: 5000 });
    // thirty seconds of silence: no SECOND pass may arrive (the wait
    // drains the queue, so anything that fires now is news)
    await new Promise((r) => setTimeout(r, 30000));
    const { event } = await a.cmd('wait-event', { type: 'pass', timeoutMs: 2000 });
    assert.equal(event, null, 'the resting cycle ran a pass in the silence');
  } finally {
    await a.quit();
    await b.quit();
  }
});

test('S4: removing B from A, B hears it and its service stops by itself', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    await a.cmd('cell', { cell: 'memoria' });
    await a.cmd('put', { cell: 'memoria', body: 'la base' });
    const invite = await a.cmd('invite', { key: 'p' });
    await a.cmd('share', { cell: 'memoria' });
    await b.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    await a.cmd('sas', { key: 'p' });
    await b.cmd('sas', { key: 'p' });
    await a.cmd('confirm', { same: true, key: 'p' });
    await b.cmd('confirm', { same: true, key: 'p' });
    await a.cmd('done', { key: 'p' });
    await b.cmd('done', { key: 'p' });
    const infoA = await a.cmd('start');
    const infoB = await b.cmd('start');
    await b.cmd('remember', { device: 'laptop', serve: infoA });
    await a.cmd('remember', { device: 'mac', serve: infoB });
    await b.cmd('events-on');
    const out = await a.cmd('remove', { name: 'mac' });
    assert.equal(out.removed, 'mac');
    assert.ok(out.rotated.some((r) => r.cell === 'memoria'));
    const { event } = await b.cmd('wait-event', { type: 'removed', timeoutMs: 30000 });
    assert.ok(event, 'B never heard the removal');
    const status = await b.cmd('status');
    assert.equal(status.serving, false, 'B stopped by itself');
  } finally {
    await a.quit();
    await b.quit();
  }
});

// ---------------------------------------------------------------------------
// v0.6.3 (C5.3/C5.4)
// ---------------------------------------------------------------------------

// v0.6.3 (C5.3): a cell shared by ANOTHER PROCESS under an Engine that
// holds it OPEN. B opens the cell first (its handle never reopens); a
// third process on the SAME root shares it (the CLI's road); only then B
// writes — the write must carry its device's signature and ARRIVE on the
// peer. Before the fix the arm was decided at the open: B's stale handle
// wrote unsigned and the peer's gate refused it forever.
test('C5.3: a share by another process under an open cell, and the write arrives', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    const rootB = path.join(TMP, `c53-${Math.random().toString(36).slice(2, 8)}`);
    fs.rmSync(rootB, { recursive: true, force: true });
    await b.cmd('open', { root: rootB, name: 'mac' });
    // B's handle of the cell — open BEFORE anything is shared
    await b.cmd('cell', { cell: 'memoria' });
    await b.cmd('put', { cell: 'memoria', body: 'la base' });
    // B invites; BEFORE the join, a third process on B's own root shares
    // the cell (what the CLI does while Homun holds the engine open)
    const invite = await b.cmd('invite', { key: 'p' });
    await new Promise((resolve, reject) => {
      const child = spawn(process.execPath, ['-e', `
        const { Engine } = require(${JSON.stringify(require.resolve('..'))});
        (async () => {
          const e = await Engine.open({ root: ${JSON.stringify(rootB)}, embedder: 'none', actor: 'cli' });
          const sync = await e.sync({ relay: 'none' });
          await sync.share('memoria');
          await e.close();
          console.log('shared');
        })().catch((err) => { console.error(err); process.exitCode = 3; });
      `], { stdio: ['ignore', 'pipe', 'inherit'] });
      let said = false;
      child.stdout.on('data', (d) => {
        if (String(d).includes('shared')) said = true;
      });
      child.on('exit', (code) => (code === 0 && said ? resolve() : reject(new Error(`la condivisione esterna è fallita (${code})`))));
    });
    // A joins the shared cell, both services run
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await a.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    await a.cmd('sas', { key: 'p' });
    await b.cmd('sas', { key: 'p' });
    await a.cmd('confirm', { same: true, key: 'p' });
    await b.cmd('confirm', { same: true, key: 'p' });
    await a.cmd('done', { key: 'p' });
    await b.cmd('done', { key: 'p' });
    await a.cmd('cell', { cell: 'memoria' });
    const infoA = await a.cmd('start');
    const infoB = await b.cmd('start');
    await b.cmd('remember', { device: 'laptop', serve: infoA });
    await a.cmd('remember', { device: 'mac', serve: infoB });
    await a.cmd('events-on');
    // B writes through the STALE handle, never reopened
    await b.cmd('put', { cell: 'memoria', body: 'scritta senza riaprire' });
    const { event } = await a.cmd('wait-event', { type: 'arrived', timeoutMs: 5000 });
    assert.ok(event, 'the write never arrived: unsigned, the gate refused it');
    let list = [];
    for (let i = 0; i < 50; i++) {
      list = await a.cmd('list', { cell: 'memoria' });
      if (list.some((n) => n.body === 'scritta senza riaprire')) break;
      await new Promise((r) => setTimeout(r, 200));
    }
    assert.ok(
      list.some((n) => n.body === 'scritta senza riaprire'),
      'the write of the stale handle is readable on the peer',
    );
  } finally {
    await a.quit();
    await b.quit();
  }
});

// v0.6.3 (C5.4): stop() with a REAL session in flight — thousands of
// notes crossing — must conclude within 5 s (the old test stopped during
// an EMPTY pass: nothing was ever in flight).
test('C5.4: stop() during a session with many notes concludes within 5 s', async () => {
  const a = new Peer();
  const b = new Peer();
  try {
    await a.cmd('open', { root: rootName(), name: 'laptop' });
    await b.cmd('open', { root: rootName(), name: 'mac' });
    await a.cmd('cell', { cell: 'memoria' });
    await a.cmd('put', { cell: 'memoria', body: 'la base' });
    const invite = await a.cmd('invite', { key: 'p' });
    await a.cmd('share', { cell: 'memoria' });
    await b.cmd('join', { ticketOrCode: invite.ticket, key: 'p' });
    await a.cmd('sas', { key: 'p' });
    await b.cmd('sas', { key: 'p' });
    await a.cmd('confirm', { same: true, key: 'p' });
    await b.cmd('confirm', { same: true, key: 'p' });
    await a.cmd('done', { key: 'p' });
    await b.cmd('done', { key: 'p' });
    const infoA = await a.cmd('start');
    const infoB = await b.cmd('start');
    await b.cmd('remember', { device: 'laptop', serve: infoA });
    await a.cmd('remember', { device: 'mac', serve: infoB });
    await b.cmd('events-on');
    // A writes five thousand notes WITHOUT waiting for the bulk to end:
    // while it streams, A's watcher fires pass after pass (the debounce
    // counts from the first writes, ~2.5 s) and B receives session after
    // session — the service is provably BUSY, not over an empty pass
    const bulkSent = Date.now();
    const bulk = a.cmd('put-many', {
      cell: 'memoria',
      count: 20000,
      body: 'la nota che riempie la passata',
    });
    await new Promise((r) => setTimeout(r, 2600));
    const t0 = Date.now();
    await b.cmd('stop');
    const elapsed = Date.now() - t0;
    const stopReturned = Date.now();
    console.log(`stop() durante il flusso da 20000 note: ${elapsed} ms`);
    assert.ok(elapsed < 5000, `stop() durante il flusso ha impiegato ${elapsed} ms`);
    assert.equal((await b.cmd('status')).serving, false);
    // the corpus was STILL BEING WRITTEN when stop() returned: the bulk's
    // answer lands only now, well after the stop — the sync was in
    // flight (the parent clock is enough: the reply was not queued yet)
    const bulkDone = Date.now();
    await bulk;
    assert.ok(
      Date.now() - bulkSent > 2600 && bulkDone >= stopReturned,
      'the bulk had not answered when stop() returned',
    );
    assert.ok(
      Date.now() > stopReturned + 250,
      `il flusso era già finito prima dello stop (risposta a ${Date.now() - stopReturned} ms dallo stop)`,
    );
    // and the interrupted stream is not wedged: B's own pass (runOnce)
    // resumes the pull — the silence after A's last write runs no pass
    // by itself (the resting cycle), so the pass is B's
    const pass = await b.cmd('runOnce');
    const cell = (pass.cells || [])[0] || {};
    assert.ok(
      cell.received > 0,
      `la ripresa non ha ricevuto nulla (${JSON.stringify(pass.cells)})`,
    );
  } finally {
    await a.quit();
    await b.quit();
  }
});
