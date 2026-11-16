// SPDX-License-Identifier: Apache-2.0

// v0.6.2 (S3/S4): ONE sync device, in its own process. The parent test
// speaks JSON lines on stdin/stdout: {id, op, args} in, {id, ok, out}
// (or {id, ok: false, code, message}) out. The events pile up in a queue
// the parent can wait on. The process exits on 'quit' or on stdin end.

const readline = require('node:readline');
const path = require('node:path');
const { Engine } = require('..');

const state = {
  engine: null,
  sync: null,
  root: null,
  pairings: new Map(),
  events: [],
  waiters: [],
};

function answer(id, ok, out) {
  // `t` stamps WHEN the child answered: the parent can prove a command
  // was still running at a moment of its choosing (C5.4)
  if (out instanceof Error || (out && out.code && out.message)) {
    process.stdout.write(
      `${JSON.stringify({ id, ok: false, code: out.code, message: out.message, t: Date.now() })}\n`,
    );
    return;
  }
  process.stdout.write(`${JSON.stringify({ id, ok, out, t: Date.now() })}\n`);
}

async function run(op, args) {
  switch (op) {
    case 'open': {
      state.root = args.root;
      state.engine = await Engine.open({
        root: args.root,
        embedder: 'none',
        actor: args.actor || 'homun',
      });
      state.sync = await state.engine.sync({
        deviceName: args.name,
        relay: 'none',
      });
      return { opened: true };
    }
    case 'close':
      await state.engine.close();
      return { closed: true };
    case 'cell':
      state[args.cell] = await state.engine.cell(args.cell);
      return { cell: args.cell };
    case 'put': {
      const note = await state[args.cell].put({
        body: args.body,
        source: 'homun',
        confidence: 'stated',
      });
      return { id: note.id };
    }
    case 'put-many': {
      // C5.4: a bulk of writes in ONE command — the parent's round trip
      // must not be the slow part of "many notes to send"
      const ids = [];
      for (let i = 0; i < args.count; i++) {
        const note = await state[args.cell].put({
          body: `${args.body} ${i}`,
          source: 'homun',
          confidence: 'stated',
        });
        ids.push(note.id);
      }
      return { count: ids.length };
    }
    case 'get': {
      const note = await state[args.cell].get(args.id);
      return { there: note !== null, body: note ? note.body : null };
    }
    case 'patch': {
      // v0.6.4 (D3): a body patch or a HEADER-only patch (title), the
      // two shapes the conflict rule must tell apart
      const set = {};
      if (args.body !== undefined) set.body = args.body;
      if (args.title !== undefined) set.title = args.title;
      const note = await state[args.cell].patch(args.id, set);
      return { id: note.id, body: note.body };
    }
    case 'conflicts':
      return await state[args.cell].conflicts();
    case 'resolve': {
      const original = await state[args.cell].resolveConflict(args.id, {
        keep: args.keep,
        body: args.body,
      });
      return { original };
    }
    case 'share':
      return await state.sync.share(args.cell);
    case 'unshare':
      await state.sync.unshare(args.cell);
      return { unshared: args.cell };
    case 'status':
      return await state.sync.status();
    case 'list': {
      const notes = await state[args.cell].list({});
      return notes.map((n) => ({ id: n.id, body: n.body }));
    }
    case 'remember': {
      // the loopback row a first session would write (the same hand the
      // Rust tests play): the OTHER device's row, this root's devices.db
      const { DatabaseSync } = require('node:sqlite');
      const devices = await state.sync.devices();
      const other = devices.find((d) => d.name === args.device);
      if (!other) {
        throw Object.assign(new Error(`nessun dispositivo ${args.device}`), { code: 'NOT_FOUND' });
      }
      const loopback = args.serve.addrs
        .map((addr) => {
          const port = addr.slice(addr.lastIndexOf(':') + 1);
          return `127.0.0.1:${port}`;
        })
        .filter((v, i, arr) => arr.indexOf(v) === i);
      const db = new DatabaseSync(path.join(state.root, 'devices.db'));
      db.exec(
        'CREATE TABLE IF NOT EXISTS device_addrs (device TEXT PRIMARY KEY, addrs TEXT NOT NULL, seen_at TEXT NOT NULL)',
      );
      db.prepare(
        'INSERT OR REPLACE INTO device_addrs (device, addrs, seen_at) VALUES (?1, ?2, ?3)',
      ).run(other.device, JSON.stringify({ node_id: args.serve.node, relay: '', addrs: loopback }), 'test');
      return { remembered: args.device };
    }
    case 'devices':
      return await state.sync.devices();
    case 'invite': {
      const p = await state.sync.invite();
      state.pairings.set(args.key ?? 'last', p);
      return {
        ticket: p.ticket,
        ticketQr: p.ticketQr,
        code: p.code,
        expiresAt: p.expiresAt,
        relay: p.relay,
        relayWarning: p.relayWarning,
        rounds: p.rounds,
      };
    }
    case 'code': {
      const p = await state.sync.code();
      state.pairings.set(args.key ?? 'last', p);
      return { code: p.code, rounds: p.rounds };
    }
    case 'join': {
      const p = await state.sync.join(args.ticketOrCode);
      state.pairings.set(args.key ?? 'last', p);
      return { rounds: p.rounds };
    }
    case 'sas': {
      const p = state.pairings.get(args.key ?? 'last');
      return { sas: await p.sas() };
    }
    case 'confirm': {
      const p = state.pairings.get(args.key ?? 'last');
      await p.confirm(args.same);
      return { confirmed: args.same };
    }
    case 'done': {
      const p = state.pairings.get(args.key ?? 'last');
      return await p.done();
    }
    case 'cancel': {
      const p = state.pairings.get(args.key ?? 'last');
      p.cancel();
      return { cancelled: true };
    }
    case 'remove':
      return await state.sync.remove(args.name);
    case 'runOnce':
      return await state.sync.runOnce();
    case 'start':
      return await state.sync.start();
    case 'stop':
      await state.sync.stop();
      return { stopped: true };
    case 'events-on': {
      state.sync.onEvent((e) => {
        if (process.env.DIAG_EVENTS) console.error('EVENT', JSON.stringify(e));
        state.events.push(e);
        for (const w of state.waiters.splice(0)) w();
      });
      return { on: true };
    }
    case 'wait-event': {
      const deadline = Date.now() + (args.timeoutMs ?? 10000);
      const matches = (e) => !args.type || e.type === args.type;
      while (true) {
        const i = state.events.findIndex(matches);
        if (i >= 0) return { event: state.events.splice(i, 1)[0] };
        if (Date.now() > deadline) {
          return { event: null, saw: state.events.map((e) => e.type) };
        }
        await new Promise((resolve) => {
          state.waiters.push(resolve);
          setTimeout(resolve, Math.max(1, deadline - Date.now()));
        });
      }
    }
    case 'events-count': {
      const kinds = {};
      for (const e of state.events) kinds[e.type] = (kinds[e.type] ?? 0) + 1;
      return { count: state.events.length, kinds };
    }
    default:
      throw Object.assign(new Error(`unknown op ${op}`), { code: 'INVALID_ARGUMENT' });
  }
}

const rl = readline.createInterface({ input: process.stdin });
rl.on('line', async (line) => {
  if (!line.trim()) return;
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    return;
  }
  if (req.op === 'quit') {
    answer(req.id, true, { quit: true });
    if (state.engine) {
      try {
        await state.engine.close();
      } catch {
        /* already gone */
      }
    }
    process.exit(0);
  }
  try {
    answer(req.id, true, await run(req.op, req.args ?? {}));
  } catch (err) {
    answer(req.id, false, { code: err.code ?? 'IO', message: err.message });
  }
});
