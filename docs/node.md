# engramma-node — il motore dentro Node.js ed Electron

v0.4 (P2.7–P2.9). Il collegamento in-process per Homun: un modulo
**N-API** (napi-rs 3) che carica il motore una volta per processo. L'N-API
è stabile tra versioni di Node ed Electron: il `.node` non va ricompilato
a ogni aggiornamento di Electron (Homun oggi non ha moduli nativi; questo
è il primo, ed è il motivo della scelta).

## Costruzione

```console
$ cd crates/engramma-node
$ npm install          # solo @napi-rs/cli (già nel lock del commit base)
$ npm run build        # cargo --release + generazione di index.js e index.d.ts
$ ls -la engramma.darwin-arm64.node index.js index.d.ts
```

`npm run build` compila in release e produce, accanto a `index.js` e
`index.d.ts` (generati), il binario `engramma.darwin-arm64.node`
(macOS arm64; ~33 MB in v0.4 — il modello NON è incorporato, vive in
`ENGRAMMA_MODEL_DIR`, predefinito `~/.cache/engramma/models`). Il
caricatore generato cerca il `.node` accanto a sé.

I test: `npm test` (Node 24) e `npm run test:electron` (l'Electron di
Homun in modo Node, `ELECTRON_RUN_AS_NODE=1`; il percorso si sovrascrive
con `HOMUN_ELECTRON`). Nessuna dipendenza oltre a Node stesso.

## Il contratto

```ts
export interface EngineOptions {
  root: string                    // cartella dati (le celle stanno in root/cells)
  modelDir?: string               // ENGRAMMA_MODEL_DIR; senza: solo testo+grafo
  embedder?: 'builtin' | 'none'   // 'builtin' = Granite 97M via tract, se c'è
  actor: string                   // chi scrive: 'homun', 'homun:bot-<id>', …
}
export class Engine {
  static open(opts: EngineOptions): Promise<Engine>
  cell(name: string, opts?: { create?: boolean }): Promise<Cell>
  cells(): Promise<string[]>
  modelStatus(): Promise<{ present: boolean; id?: string; bytes?: number }>
  pullModel(onProgress?: (done: number, total: number) => void): Promise<void>
  search(cells: string[], q: SearchInput): Promise<SearchHit[]>   // RRF k=60, approssimata
  close(): Promise<void>
}
export class Cell {
  readonly name: string
  put(note: NoteInput): Promise<NoteView>
  patch(id: string, patch: NotePatchInput): Promise<NoteView>
  get(id: string): Promise<NoteView | null>       // id corto (8) come la CLI
  list(f?: { tags?: string[]; noteType?: string; status?: string[]; limit?: number; offset?: number }): Promise<NoteView[]>
  search(q: SearchInput): Promise<SearchHit[]>
  history(id: string): Promise<VersionView[]>
  neighbors(id: string, depth?: number): Promise<NeighborView[]>
  setStatus(id: string, status: string): Promise<void>
  confirm(id: string): Promise<void>
  delete(id: string): Promise<void>
  purge(id: string, reason: string): Promise<PurgeView>
  embedPending(limit?: number): Promise<number>
  changesSince(seq: number): Promise<{ head: number; ops: OpView[] } | { compacted: true; head: number }>
  verify(): Promise<VerifyView>
  compact(keepLast: number, opts?: { keepSnapshot?: boolean }): Promise<CompactView>
  vaultViews(): Promise<VaultView[]>
  vaultUnbind(dirOrViewId: string): Promise<void>
}
export interface SearchInput {
  text: string; k?: number; mode?: 'text' | 'vector' | 'hybrid'
  graphHops?: number; asOf?: string; includeQuarantined?: boolean; explain?: boolean
}
```

Notevoli:

- **i nomi dei campi sono camelCase** (`noteType`, `validFrom`,
  `graphHops`, `asOf`), come nell'`index.d.ts` generato; le date sono
  stringhe RFC 3339 (o `YYYY-MM-DD` dove ha senso);
- `SearchHit` porta `id`, `cell`, `title`, `snippet`, `score`, `support`
  e, con `explain: true`, il perché: liste, termini, ranghi, i salti del
  grafo (`via`) e il **peso del grafo per quella domanda** con i suoi due
  segnali (`explain.graph`: `{weight, off?, gap?, agreement?}` — «grafo
  spento: nessuna entità nominata» quando la porta è chiusa);
- **`get` è un oracolo chiuso**: una nota inesistente, in quarantena o
  lapidata risponde `null`, sempre — lo stesso principio del motore;
- **`purge` non mente**: se qualcosa trattiene ancora byte, la risposta
  lo dice (`pending: ['wal', …]`, `scrubbed: false`) e il chiamante finisce
  il lavoro; `INCOMPLETE_FORGETTING` è la forma estrema, quando l'oblio
  non può nemmeno cominciare;
- **`hybrid` senza modello degrada a testo+grafo**; `vector` senza model—lo
  è un errore `MODEL_MISSING`; `pullModel` scarica la revisione fissata
  verificando lo sha256 a chunk (già presente: verifica e salta);
- gli **errori** sono `Error` con `code` stabile: `NOT_FOUND`,
  `QUARANTINED`, `INVALID_NOTE`, `CONFLICT` (per esempio una nota
  `external` non può nascere attiva), `IO`, `MODEL_MISSING`, `COMPACTED`.
  I messaggi non rivelano contenuti di note;
- **concorrenza**: `Engine` e `Cell` si usano da molte promesse in
  parallelo; le scritture sulla stessa cella si serializzano nel motore.
  Due processi sulla stessa cella restano possibili (WAL), ma Homun ne
  userà uno.

## Un assistente che ricorda

```js
const { Engine } = require('engramma-node');

const engine = await Engine.open({
  root: '/Users/<utente>/Library/Application Support/Homun/engramma',
  actor: 'homun:bot-7',          // ogni scrittura porta chi l'ha fatta
});
const cell = await engine.cell('utente');

// 1. SCRITTURE — ciò che l'utente dice è `stated`; ciò che il bot deduce
//    in un turno contaminato è `external`: nasce in quarantena e non
//    esiste per la ricerca finché qualcuno non la conferma
await cell.put({
  noteType: 'fact',
  title: 'Casa di Fabio',
  body: 'Fabio vive a [[Milano]] dal 2024.',
  tags: ['casa'],
  source: 'conversazione:2026-11-08',
  confidence: 'stated',
});
const deduzione = await cell.put({
  body: 'Forse Fabio si trasferisce a Torino (voci, da verificare).',
  source: 'homun:bot-7:turno-42',
  confidence: 'external',
});

// 2. RICHIAMO — prima si dà da fare al modello, poi si cerca
await cell.embedPending();
const hits = await cell.search({
  text: 'dove abita Fabio?', k: 5, explain: true,
});
for (const h of hits) {
  console.log(h.title, h.score, h.support, h.explain?.graph?.weight);
}

// 3. QUARANTENA — il bot non ha mostrato nulla di non confermato; quando
//    l'utente valida, la nota entra nel ricordo
await cell.confirm(deduzione.id);

// 4. OBLIO — irrazionale è sbagliato: la purga dimentica i payload e la
//    risposta dice se è già tutto andato
const view = await cell.purge(hits[0].id, 'richiesta dell'utente');
if (!view.scrubbed) {
  // il WAL o una vista markdown trattengono ancora byte: si riprova alla
  // prossima scrittura / sync, e il chiamante lo sa
}
await engine.close();
```

## Firmare il `.node` dentro un'app macOS

Homun è notarizzato con **hardened runtime**: ogni eseguibile e ogni
libreria nativa va firmata, e il `.node` **non può stare dentro
`app.asar`** (un modulo nativo deve essere mappato dal file system).

1. tenere `engramma.darwin-arm64.node` **fuori** dall'asar — con
   `electron-builder`, aggiungerlo a `asarUnpack` oppure metterlo in
   `extraResources` e caricarlo con
   `require(path.join(process.resourcesPath, 'engramma.darwin-arm64.node'))`;
2. firmarlo con la stessa identità dell'app, PRIMA della notarizzazione:

   ```console
   $ codesign --force --options runtime \
       --sign "Developer ID Application: …" \
       engramma.darwin-arm64.node
   $ codesign --verify --strict engramma.darwin-arm64.node
   ```

3. `electron-builder` rifirma tutto il bundle dopo la copia: se il
   `.node` è in `extraResources`, la rifirma lo copre da sé; verificarlo
   comunque nel pipeline (`codesign --verify --deep --strict` sull'app).

Il demone di Homun gira senza finestre: il modulo va caricato LÌ, una
volta sola — il modello occupa ~400 MB di RAM — e l'interfaccia chiede
tutto al demone via RPC.
