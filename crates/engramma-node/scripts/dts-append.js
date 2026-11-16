// SPDX-License-Identifier: Apache-2.0

// v0.6.2 (S2): `napi build` regenerates index.d.ts from the Rust doc
// comments, but a TS UNION cannot come out of napi-derive — the sync
// event's shape is appended here, after every build, idempotently.

const fs = require('node:fs');

const UNION = `/**
 * One event of the sync service (a union on \`type\`). Nothing here carries
 * note text, a ticket or a short code.
 *
 * - \`pass\`: a pass of the continuous service finished;
 * - \`arrived\`: new operations landed — update the view and call
 *   \`embedPending\` (the vectors do not travel, they are computed here);
 * - \`device\`: a device entered or left the list, heard from another;
 * - \`removed\`: THIS device was removed — the service stops by itself,
 *   the new keys will not arrive;
 * - \`unsent\`: operations that could not be sealed (R4);
 * - \`error\`: an error of the cycle that does not stop it.
 */
export type SyncEvent =
  | {
      type: 'pass'
      devices: Array<{
        name: string
        device: string
        reached: boolean
        whyNot?: string
        via?: string
      }>
      cells: Array<{
        cell: string
        sent: number
        received: number
        refused: number
        conflicts: number
        unsent: number
        head: number
        frontier: number
      }>
    }
  | { type: 'arrived'; cell: string; count: number }
  | { type: 'device'; name: string; short: string; added: boolean }
  | { type: 'removed' }
  | { type: 'unsent'; cell: string; count: number }
  | { type: 'error'; code: string; message: string }
`;

const path = 'index.d.ts';
let text = fs.readFileSync(path, 'utf8');
if (!text.includes('export type SyncEvent')) {
  text = text.replace(
    '/** The engine version, so the host can log what it loaded. */',
    `${UNION}\n/** The engine version, so the host can log what it loaded. */`,
  );
  fs.writeFileSync(path, text);
  console.log('index.d.ts: SyncEvent appended');
}
