// SPDX-License-Identifier: Apache-2.0

// L'esempio di docs/node.md, eseguibile: un assistente che scrive, richiama,
// conferma e dimentica. Si avvia con Node o con l'Electron di Homun in modo
// Node (vedi docs/node.md); il modello, quando c'è, è quello di
// ENGRAMMA_MODEL_DIR.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { Engine, version } = require('..');

async function main() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'engramma-esempio-'));
  console.log(`engramma-node ${version()} — esempio docs/node.md`);
  const engine = await Engine.open({ root, actor: 'homun:bot-7' });
  const status = await engine.modelStatus();
  console.log(`modello: ${status.present ? status.id : 'assente (solo testo+grafo)'}`);
  const cell = await engine.cell('utente');

  // 1. SCRITTURE
  const detto = await cell.put({
    noteType: 'fact',
    title: 'Casa di Fabio',
    body: 'Fabio vive a [[Milano]] dal 2024.',
    tags: ['casa'],
    source: 'conversazione:2026-11-08',
    confidence: 'stated',
  });
  console.log(`scritta: ${detto.id.slice(-8)} [${detto.status}] ${detto.title}`);
  const deduzione = await cell.put({
    body: 'Forse Fabio si trasferisce a [[Torino]] (voci, da verificare).',
    source: 'homun:bot-7:turno-42',
    confidence: 'external',
  });
  console.log(`dedotta: ${deduzione.id.slice(-8)} [${deduzione.status}] (in quarantena)`);

  // 2. RICHIAMO
  const embedded = await cell.embedPending();
  const hits = await cell.search({ text: 'dove abita Fabio?', k: 5, explain: true });
  console.log(`ricerca «dove abita Fabio?» (embedding: ${embedded} note):`);
  for (const h of hits) {
    const grafo = h.explain?.graph
      ? ` grafo ${h.explain.graph.weight.toFixed(2)}${h.explain.graph.off ? ` (${h.explain.graph.off})` : ''}`
      : '';
    console.log(`  ${h.score.toFixed(3)} ${h.support} «${h.title}» — ${h.snippet}${grafo}`);
  }

  // 3. QUARANTENA: la deduzione non è mai comparsa qui sopra; l'utente valida
  const nascosta = await cell.get(deduzione.id);
  console.log(`la quarantena per get(): ${nascosta === null ? 'null (oracolo chiuso)' : 'ERRORE: visibile'}`);
  await cell.confirm(deduzione.id);
  console.log(`confermata: ${(await cell.get(deduzione.id)).status}`);

  // 4. OBLIO
  const oblio = await cell.purge(detto.id, 'richiesta dell’utente');
  console.log(`purga: ${oblio.forgottenPayloads} payload dimenticati, scrubbed=${oblio.scrubbed}`);
  console.log(`dopo la purga, get(): ${await cell.get(detto.id)}`);

  const verifica = await cell.verify();
  console.log(`verify: ok=${verifica.ok} replay=${verifica.replay} ops=${verifica.ops}`);
  await engine.close();
  fs.rmSync(root, { recursive: true, force: true });
  console.log('esempio completato');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
