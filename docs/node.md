# engramma-node — il motore dentro Node.js ed Electron

v0.4.2. Il collegamento in-process per Homun: un modulo **N-API**
(napi-rs 3) che carica il motore una volta per processo. L'N-API è
stabile tra versioni di Node ed Electron: il `.node` non va ricompilato a
ogni aggiornamento di Electron (Homun oggi non ha moduli nativi; questo
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

I test: `npm test` (Node 24), `npm run test:electron` (l'Electron di
Homun in modo Node, `ELECTRON_RUN_AS_NODE=1`) e `npm run
test:electron-main` — il demone di Homun è un processo Electron **senza
finestra**, quindi il modulo va provato nel processo principale vero:
`app.whenReady()`, un motore che apre, scrive, cerca e chiude, uscita 0.
Il percorso dell'Elettron si sovrascrive con `HOMUN_ELECTRON`; i test
che legano una vista markdown usano il binario della CLI (`ENGRAMMA_BIN`,
predefiniti `target/release` e `target/debug`). Nessuna dipendenza oltre
a Node stesso.

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
  cell(name: string, opts?: { create?: boolean; actor?: string }): Promise<Cell>
  cells(): Promise<string[]>
  dropCell(name: string, reason: string): Promise<DropView>   // v0.4.2: oblio di una cella intera
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
export interface NoteInput {
  id?: string                     // ULID; assente = nota nuova, presente = riscrittura
  noteType?: string; title?: string; body?: string; tags?: string[]; aliases?: string[]
  source: string; confidence: 'stated' | 'inferred' | 'external'
  status?: 'active' | 'quarantined'
  validFrom?: string; validTo?: string
  extensions?: Record<string, unknown>          // chiavi x-*
  createdAt?: string; updatedAt?: string        // RFC 3339, solo nota nuova (importazione)
}
export interface NoteView {
  id: string; noteType: string; title: string | null; body: string
  tags: string[]; aliases: string[]; scope: string | null
  source: string; confidence: string; status: string
  validFrom: string | null; validTo: string | null
  createdAt: string; updatedAt: string; version: number
  extensions: Record<string, unknown>           // chiavi x-*
}
export interface NotePatchInput {
  title?: string | null; body?: string; tags?: string[]
  source?: string; confidence?: string
  validFrom?: string | null; validTo?: string | null
  extensions?: Record<string, unknown | null>   // per chiave; null toglie
}
export interface SearchInput {
  text: string; k?: number; mode?: 'text' | 'vector' | 'hybrid'
  graphHops?: number; asOf?: string; includeQuarantined?: boolean; explain?: boolean
}
export interface SearchHit {
  id: string; cell: string; title: string; snippet: string
  score: number; support: string | null; explain?: HitExplain
  noteType: string; tags: string[]; updatedAt: string; status: string
}
export interface PurgeView {
  forgottenPayloads: number; scrubbed: boolean; pending: string[]
  views: Array<{ viewId: string | null; root: string; reason: string }>
}
export interface DropView {                     // v0.4.2
  notes: number                                 // le note dimenticate, lapidi comprese
  scrubbed: boolean; pending: string[]
  views: Array<{ viewId: string | null; root: string; reason: string }>
}
```

Notevoli:

- **i nomi dei campi sono camelCase** (`noteType`, `validFrom`,
  `graphHops`, `asOf`), come nell'`index.d.ts` generato; le date sono
  stringhe RFC 3339 (o `YYYY-MM-DD` dove ha senso);
- **lo stato di una nota lo cambiano solo `setStatus`, `confirm` e
  `delete`** (più il ripristino dallo storico): una riscrittura (`put`
  con id esistente, `patch`) **conserva lo stato** — una nota in
  quarantena riscritta resta in quarantena e fuori dalla ricerca, anche
  se la nuova versione dice `stated`; chiedere `status: 'active'` per
  iscritto è `CONFLICT`, come una nota `external` che chiede di nascere
  attiva. L'unica deriva consentita: una riscrittura `external` porta
  una nota attiva in quarantena, mai il contrario. Una **lapide** non
  resuscita: `put` e `patch` col suo id rispondono `CONFLICT`, si
  ripristina dallo storico. La regola vive nel core: CLI e `vault sync`
  ereditano la stessa (un file markdown modificato a mano non cambia lo
  stato della nota; la vista lo riscrive col vero);
- **la data di nascita è della nota, non della versione**: una
  riscrittura conserva `createdAt` e sposta solo `updatedAt`.
  `NoteInput.createdAt`/`updatedAt` (RFC 3339) servono
  **all'importazione** — una nota nuova può nascere nel passato, mai nel
  futuro (5 minuti di tolleranza, poi `INVALID_NOTE`); su una
  riscrittura entrambi sono ignorati. Il registro continua a usare il
  suo orologio (HLC): queste sono date della nota, non delle operazioni;
- **`extensions`**: `put` accetta `Record<string, unknown>` (le chiavi
  prendono il prefisso `x-` se non l'hanno), `NoteView` le riporta come
  valori JSON e `patch` le tocca **per chiave** (`null` toglie la chiave,
  un valore la setta, il resto resta). `aliases` resta un campo a sé;
- **`actor` per cella**: `e.cell(name, { actor: 'homun:bot-7' })` timbra
  le scritture di quella `Cell` con quel bot, un solo `Engine`; due
  `Cell` sulla stessa cella con attori diversi scrivono la stessa
  memoria e il registro dice chi;
- **`SearchHit` porta la riga intera**: `noteType`, `tags`, `updatedAt`,
  `status` (`active` fuori da `includeQuarantined`) — niente `get` per
  risultato — più `explain` quando chiesto: liste, termini, ranghi, i
  salti del grafo (`via`) e il **peso del grafo per quella domanda**
  (`explain.graph`: `{weight, off?, gap?, agreement?}`);
- **`get` è un oracolo chiuso**: una nota inesistente, in quarantena o
  lapidata risponde `null`, sempre — lo stesso principio del motore;
- **`purge` non lancia per un oblio incompleto: risponde**. La vista dice
  cosa trattiene ancora byte (`pending: ['wal', 'vault']`,
  `scrubbed: false`) e **quale vista** non ha potuto raggiungere
  (`views: [{viewId, root, reason}]`, senza contenuti di note); il file
  della nota in una vista raggiungibile sparisce subito, non alla sync
  dopo. Il codice `INCOMPLETE_FORGETTING` previsto dalla spec v0.4 **non
  serve e non c'è**: un oblio che non può nemmeno cominciare è un errore
  normale (`IO`, `NOT_FOUND`), e un oblio che comincia ma non finisce è
  un dato di ritorno, non un'eccezione — il chiamante (il demone di
  Homun, l'interfaccia) deve poter mostrare «quasi dimenticato» e
  riprovare, non gestire un lancio;
- **dopo `close`, niente riaperture silenziose**: ogni metodo di
  `Engine` e di ogni sua `Cell` risponde `CLOSED` (chiamare due volte
  `close` non è un errore). Le operazioni già in corso finiscono, e
  `close` aspetta che si siano assestate: una promessa ritardataria del
  demone non può scrivere a motore chiuso;
- **`vaultUnbind` e i messaggi delle viste** usano l'id corto = **ultime
  8** cifre dell'ULID (la parte casuale, come le note; le prime 8 sono il
  timestamp e due viste di pochi minuti distanti lo condividono); l'id
  intero è sempre accettato, uno corto ambiguo è un errore che elenca i
  candidati;
- **`hybrid` senza modello degrada a testo+grafo**; `vector` senza
  modello è un errore `MODEL_MISSING`; `pullModel` scarica la revisione
  fissata verificando lo sha256 a chunk (già presente: verifica e
  salta);
- gli **errori** sono `Error` con `code` stabile: `NOT_FOUND`,
  `QUARANTINED`, `INVALID_NOTE`, **`INVALID_ARGUMENT`** (v0.4.2),
  `CONFLICT`, `CLOSED`, `IO`, `MODEL_MISSING`, `COMPACTED`. I messaggi
  non rivelano contenuti di note. `INVALID_ARGUMENT` è per gli
  **argomenti della chiamata** (uno `status` sconosciuto in `list`, un
  `limit` di zero, `graphHops` oltre 2, `depth` oltre 4, una `mode` che
  non esiste), `INVALID_NOTE` resta delle **note** (source, confidence,
  noteType, date): il chiamante distingue un bug suo da una nota che non
  può scrivere;
- **concorrenza**: `Engine` e `Cell` si usano da molte promesse in
  parallelo; le scritture sulla stessa cella si serializzano nel motore.
  Due processi sulla stessa cella restano possibili (WAL), ma Homun ne
  userà uno;
- **nessun tetto muto** (v0.4.2): un `limit` esplicito di `list` o di
  `embedPending` si rispetta **così com'è** — è un processo locale, gli
  id stanno già in memoria; `limit` assente vale 100 (list) o 512
  (embedPending), `k` assente 10. Il tetto di 1000 del v0.4.1 tagliava
  in silenzio: 1200 note ne arrivavano 1000 e il chiamante credeva di
  avere tutto. `limit: 0` e `k: 0` sono `INVALID_ARGUMENT` (una pagina
  di zero è un bug del chiamante). I massimi **del motore** sono errori,
  mai tagli silenziosi: `graphHops` 0–2, `neighbors` depth 1–4. Anche
  la finestra dei candidati BM25 nel core segue `k` (pavimento 50: per
  `k ≤ 50` costa come sempre);
- **le lapidi si elencano** (v0.4.2): `list({ status: ['deleted'] })`
  mostra una nota eliminata come la sua ultima versione, con
  `status: 'deleted'` — quella che l'utente vede per decidere se
  ripristinarla (`history` + `restore` nella CLI) o dimenticarla per
  sempre (`purge` sull'id funziona anche sulla lapide). Ammessi:
  `active`, `quarantined`, `superseded`, `deleted`; uno stato
  sconosciuto è `INVALID_ARGUMENT`, mai un filtro vuoto che risponde
  `[]` senza dire perché. La lista senza `status` resta delle note vive
  e `get` resta l'oracolo chiuso (`null` per quarantena e lapide);
- **`dropCell(name, reason)`** (v0.4.2): l'oblio di una **cella intera**
  — le note (lapidi comprese), i file della cella (WAL e istantanee di
  compattazione compresi) e i file delle note nelle viste legate, come
  `purge` fa per una nota. La `DropView` dice `notes`, `scrubbed`,
  `pending` e quali viste non ha potuto raggiungere (`views`, senza
  contenuti): una vista irraggiungibile è `pending: ['vault']` e
  `scrubbed: false`, mai un'eccezione. Dopo: `cells()` non la elenca, le
  `Cell` già aperte rispondono `NOT_FOUND`, un nuovo `cell(name)` ne
  crea una **vuota**. Le operazioni in volo su quella cella finiscono
  prima, come per `close`. Il registro dei motivi non esiste: della
  cella non resta nulla in cui scriverlo — `reason` è obbligatoria e
  resta la traccia del chiamante (nessun contenuto). Nella CLI:
  `engramma cell drop <nome> --reason …`;
- **`pullModel` collega l'embedder al motore APERTO** (v0.4.2): finito
  il download (o trovati i file già presenti e verificati), il motore
  aperto carica il modello e da lì in poi `modelStatus().present` vale
  `true`, la ibrida usa i vettori, `embedPending` lavora — **senza**
  `close`/`open`. Le ricerche già in volo durante il collegamento
  finiscono col ramo che avevano. Se il caricamento fallisce la
  `pullModel` rifiuta (`IO`) e il motore resta in testo+grafo,
  funzionante. `embedder: 'none'` all'apertura è una scelta esplicita:
  la `pullModel` scarica ma non collega mai;
- **`ENGRAMMA_PULL_BASE`** (v0.4.2): la variabile d'ambiente, quando c'è
  e non è vuota, sostituisce la base di download di `pullModel`
  (predefinita `https://huggingface.co`) — installazioni offline e
  mirror negli ambienti chiusi, come i test della CLI con un server
  locale. La **revisione agganciata e lo sha256 per file restano**: una
  base diversa può solo consegnare gli **stessi** byte o fallire, mai
  cambiare il contenuto.

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
volta sola — con il Granite 97M collegato il processo tiene circa
1,1 GB di RSS (pesi f32 ~390 MB, il resto è il carico di tract: il
picco di `into_typed`/`into_optimized` che il malloc di macOS non
restituisce al sistema, ma riusa — 1000 `embedPending` e 2000 ricerche
ibride sopra non spostano la misura, REPORT §v0.4.2) — e l'interfaccia
chiede tutto al demone via RPC.
