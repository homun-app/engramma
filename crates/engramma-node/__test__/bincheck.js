// SPDX-License-Identifier: Apache-2.0

// v0.6.10 (J3): the bench refuses a CLI that is not the one the sources
// built. `engramma --version` prints the commit (with the `-dirty` mark)
// and the schema; the addon exposes the same two things (`buildInfo()`).
// When they disagree the SUITE does not start: a test that passes or
// falls according to which binary lies around in target/ measures
// nothing — the v0.6.9 verification found three view tests falling on a
// CLI three days old, with no one the wiser.

'use strict';

const { execFileSync } = require('node:child_process');

/** Parse `engramma 0.1.0-dev (commit <hash>[-dirty], schema <n>)`. */
function parseVersionLine(line) {
  const m = /\(commit ([0-9a-f]+(?:-dirty)?), schema (\d+)\)/.exec(line);
  if (!m) return null;
  return { commit: m[1], schema: Number(m[2]) };
}

/**
 * The refusal: compare one CLI binary with the addon's own build info.
 * Throws with a message that names both sides and the way out; returns
 * the parsed info when they agree (nothing else changes).
 */
function assertCliMatchesAddon(cli, info) {
  let line;
  try {
    line = execFileSync(cli, ['--version'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  } catch (e) {
    throw new Error(`la CLI in ${cli} non gira (--version): ${e.message}`);
  }
  const parsed = parseVersionLine(line);
  if (!parsed) {
    throw new Error(
      `la CLI in ${cli} non dice il suo commit e il suo schema («${line}»): ` +
      'ricostruiscila o imposta ENGRAMMA_BIN',
    );
  }
  if (parsed.commit !== info.commit || parsed.schema !== info.schema) {
    throw new Error(
      `la CLI in ${cli} è del commit ${parsed.commit} (schema ${parsed.schema}), ` +
      `l'addon del commit ${info.commit} (schema ${info.schema}): ` +
      'ricostruisci o imposta ENGRAMMA_BIN',
    );
  }
  return parsed;
}

module.exports = { parseVersionLine, assertCliMatchesAddon };
