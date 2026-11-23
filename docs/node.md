# engramma-node — il motore dentro Node.js ed Electron

v0.4.6. Il collegamento in-process per Homun: un modulo **N-API**
(napi-rs 3) che carica il motore una volta per processo. L'N-API è
stabile tra versioni di Node ed Electron: il `.node` non va ricompilato a
ogni aggiornamento di Electron (Homun oggi non ha moduli nativi; questo
è il primo, ed è il motivo della scelta). Da v0.4.4 le allocazioni Rust
del modulo (motore e tract) passano per **mimalloc**: in un `cdylib` il
`#[global_allocator]` vale per il modulo, non per Node o Electron, che
restano col loro allocatore (le misure stanno nel REPORT v0.4.4; dalla
v0.4.5 su macOS, su Linux resta quello di sistema — § 7 del DESIGN).

**Le due variabili d'ambiente dell'embedding** (v0.4.6.1):

- `ENGRAMMA_EMBED_THREADS` — quanti lavoratori incorporano in parallelo.
  Predefinito **1** (la regola della v0.4.5 sul picco, seguita davvero:
  v0.4.6, REPORT). Ogni lavoratore è un forward pass in più: a processo
  nudo, al budget 512, passare da 1 a 2 lavoratori porta le 64 note da
  5,8 a ~3,9 s e alza il picco (a 1024 × 2: 986 MB contro 912 a 1024 × 1,
  mimalloc); nell'artefatto il predefinito fa `embedPending` di 64 note in
  ~11,4 s. Per
  l'importazione iniziale di grandi storie è la manopola del compromesso:
  ogni lavoratore aggiunge ritmo e picco in uguale misura.
- `ENGRAMMA_EMBED_TOKEN_BUDGET` — il tetto di token di un forward pass
  (`righe × seq_lotto ≤ budget`). Predefinito **512** (v0.4.6.1: pari a
  `max_tokens`, un testo lungo per passata; su CPU i lotti grandi non
  fanno guadagnare tempo e gonfiano solo le attivazioni — a 1 lavoratore
  512 dà picco 878 MB e 100 note/s sul banco a 10k, 4096 dà 1126 MB e 75
  note/s); un valore non valido (0, non numerico) ricade sul predefinito. A parità di
  configurazione i vettori sono identici bit per bit; fra budget diversi
  la forma del lotto sposta l'ultima cifra dentro tract (coseno
  ≥ 0,999999, |Δ| ≤ 1e-5): cambia la memoria, non il significato.

**La RAM che aspettarsi** (macOS, Node 24, Granite 97M, 5 corse):
modello caricato ~894 MB; dopo `embedPending` di 64 note **~959 MB** a
+10 s, e lì resta — pagine liberate ma residenti, con qualunque
allocatore su macOS (959/959/959 MB nelle cinque corse; v0.4.6 a budget
4096: 1224). Il picco (`maxRSS`) è **1222 MB** e ora lo fa il
**caricamento del modello** (l'ottimizzazione del grafo in tract: 1220 MB
appena dopo `Engine.open`), non più l'embedding; nella v0.4.4 era ~2,3 GB.
Le soglie di riposo più basse richiedono l'embedding fuori processo:
opzione della v0.5.

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
  repair(): Promise<RepairView>          // v0.6.10, J2: { moved: [{old, new}] }
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
  `CONFLICT`, `CLOSED`, `IO`, `MODEL_MISSING`, `COMPACTED`,
  **`NOTE_PURGED`** (v0.6.9). I messaggi
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
- **il cestino ha una porta sola** (v0.6.8, H1 — cambio voluto di
  `list` e `get`): finché l'originale tiene una lapide, le sue copie di
  conflitto seguono la lapide **anche da `list` e `get`**: non compaiono
  nella lista delle note vive, non compaiono tra le lapide
  (`list({ status: ['deleted'] })` mostra l'originale cancellato, mai la
  copia) e `get(copyId)` risponde `null`, esattamente come per una nota
  cancellata. La lettura per id onesta resta `conflict(copyId)` con
  `originalDeleted: true`. Quando la nota si riapre (una modifica che
  ha visto la lapide), tutto torna visibile insieme: la copia torna in
  `list`, in `conflicts()` e in `get`, col testo integro. Il test del
  contratto JS è `H1: list() and get() speak the bin; the reopen brings
  the copy back` (`__test__/sync-pair.test.js`);
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
- **`dropCell` chiude la connessione, il GC non decide lui** (v0.4.3):
  le `Cell` già consegnate condividono una sola connessione; il drop la
  **ritira** (`Cell::retire` nel core: al suo posto una in memoria, quella
  vera chiusa) prima di cancellare i file. Senza questo, una `Cell` viva
  in JS teneva aperti `.sqlite`/`-wal`/`-shm` della generazione cancellata
  finché il GC non raccoglieva il wrapper (ore, nella cache di Homun).
  Dopo il drop `lsof` non trova più nessun descrittore sui file della
  cella, senza aspettare la GC;
- **le istantanee di compattazione non sono celle** (v0.4.3): `compact`
  con `keepSnapshot` scrive `<nome>-snap-<seq>.sqlite` accanto alla
  cella. Quella forma esatta — un solo riconoscitore nel core,
  `snapshot_of` — è l'**unica** cosa che conta come istantanea: la
  portano via il `dropCell`/`cell drop` della **propria** cella e il
  conto dei byte di `compact`, e `cells()`/`cell list` non la elencano
  mai. Una cella che si chiama `storia-snap-archivio` resta una cella a
  tutti gli effetti (il suffisso non sono cifre): il drop di `storia`
  non la tocca;
- **il nome a forma di istantanea è riservato** (v0.4.3): `<qualcosa>-snap-<cifre>`
  è rifiutato con `INVALID_ARGUMENT` in `cell()` e `dropCell` (e uso,
  exit 2, nella `cell create` della CLI) — una cella così chiamata sarebbe
  indistinguibile da un'istantanea di `<qualcosa>`: il suo file andrebbe
  via col drop dell'altra e non si potrebbe mai elencare. Una cella
  **già esistente** con quel nome (precedente alla regola) si apre ancora
  per nome dalla CLI (`--cell x-snap-7 …`, anche per `cell drop`: è la
  via d'uscita), ma `cells()`/`cell list` non la elencano e il
  collegamento Node la rifiuta;
- **nessuna `Cell` nasce morta** (v0.4.3): `cell(name)` prende il gate
  della generazione e apre la cella **sotto lo stesso lock** del drop.
  Prima, un `dropCell` che entrava tra i due passaggi sigillava proprio
  il gate che l'apertura stava per consegnare: la `Cell` di ritorno
  rispondeva `NOT_FOUND` per sempre — fresca ma già morta. Ora una
  `Cell` può morire solo di un oblio **iniziato dopo** che esisteva; se
  `dropCell` e `cell(name)` corrono insieme, l'apertura aspetta il drop
  e nasce su una cella nuova, viva;
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

### I conflitti da risolvere (v0.6.4; la lettura in v0.6.5; il cestino in v0.6.7)

- `cell.conflicts(): Promise<ConflictEntry[]>` — ogni copia di conflitto
  **aperta**: `{ copyId, originalId, loserActor, loserClass: 'human' | 'agent',
  loserAt, quarantined, visible }`. Nessun testo nell'elenco: il testo si
  legge con `conflict` (sotto). `visible: false` è una copia
  oltre il tetto di tre: resta aperta, appare quando una visibile si
  chiude. **v0.6.7:** le copie di una nota **cancellata** seguono la
  lapide — finché l'originale tiene una lapide attiva non stanno
  nell'elenco e non rispondono a nessuna ricerca (è il comportamento di
  un cestino, anche nella vista ampia `includeQuarantined`); quando la
  nota si riapre (una modifica con `base` uguale alla lapide) le copie
  aperte tornano con le regole di sempre, quelle chiuse restano chiuse. **v0.6.9 (I2): il
  cestino contiene le copie che nessuno ha preso.** Una copia che
  l'utente ha preso — modificata con la propria mano, il testimone che
  la chiude — è una nota sua: resta visibile (`list`, `get`, ricerche,
  linea del tempo) anche quando l'originale va nel cestino, e
  `conflict(copyId)` risponde `NOT_FOUND` (non c'è più conflitto da
  descrivere). Solo le copie ancora APERTE seguono la lapide.
- `cell.conflict(copyId): Promise<ConflictDetail>` (v0.6.5, E5) — una
  copia per intero, i tre testi della scelta:
  `{ ...ConflictEntry, originalDeleted, originalBody, copyBody, forkBody }`.
  `originalDeleted: true` (v0.6.7) dice che l'originale tiene una
  lapide attiva e la copia è nascosta: una lettura per id resta onesta
  e nomina il perché.
  `originalBody` è il corpo dell'originale com'è; `copyBody` è il corpo
  del perdente (il suo payload ne è la casa); `forkBody` è il corpo alla
  biforcazione fra il perdente e il vincitore (`null` sotto il taglio di
  compattazione) — la colonna di mezzo di una vista a tre colonne.
  **La copia in quarantena si legge qui e solo qui**: `get` resta
  l'oracolo chiuso e risponde `null` per ogni nota nascosta, copie
  comprese. Errori: `NOT_FOUND` per un id che non è una copia aperta
  (inesistente o chiusa).
- `cell.resolveConflict(copyId, { keep: 'original' | 'copy' | 'merged',
  body? }): Promise<string>` — operazioni normali con l'actor della
  cella: `original` è un `delete` sull'id della copia; `copy` e `merged`
  prima patchano l'originale (col corpo della copia, o col corpo dato) e
  poi chiudono. La risoluzione con `copy` può chiudere la copia già con
  la patch (il corpo uguale contiene il perdente): il delete diventa
  inutile, e non è un errore. Due risoluzioni concorrenti con corpi
  uguali non creano nulla; con corpi diversi nasce un conflitto nuovo,
  ed è giusto così. Errori: `NOT_FOUND` per un id che non è una copia
  (o una copia chiusa senza operazioni sue), `CONFLICT` per una copia
  chiusa che tiene la sua riga (risolta con un delete, modificata).
- `PEER_OUTDATED` (D4): il pari parla regole di materializzazione
  diverse — l'ALPN `engramma/sync/3` è la versione delle regole (v0.6.5:
  la testa visibile e il testimone per coppia cambiano la derivazione) —
  e **nessuna operazione passa**; il messaggio nomina il dispositivo da
  aggiornare (o dice di aggiornare questo).
- **La convenzione degli actor** (D2): umano è `user`, ogni actor che
  finisce in `:user` (`homun:user`), ogni actor che comincia con `user@`
  (`user@mcp:<etichetta>`); tutto il resto è agente (`homun:bot-<id>`,
  `homun:story`, `mcp:<etichetta>`). Su due punte concorrenti vince
  l'umana anche se più vecchia, e il perdente d'agente nasce in
  quarantena, nella coda. L'actor è dichiarato e non firmato: nella
  sync personale basta (tutti gli scrittori sono sui dispositivi del
  proprietario); nelle organizzazioni la classe sarà legata a una firma.

## Quello che è dimenticato non torna (v0.6.9, I1)

- **`NOTE_PURGED`.** «Dimentica per sempre» vuol dire che **quell'id non
  torna**: ogni scrittura che ha l'id purgato come oggetto — `put`,
  `patch`, `delete`, `restore`, `setStatus`, `confirm`, i `link` e gli
  `unlink` da o verso la nota, `supersede`, le proposte che la
  nominano — è rifiutata con `e.code === 'NOTE_PURGED'` e l'id nel
  messaggio. (v0.6.11: la `decide` il cui BERSAGLIO è purgato non è più
  fra i rifiuti — la decisione atterra la proposta come nota nuova,
  vedi la sezione K1 qui sotto.) Per riscrivere un contenuto simile serve una **nota
  nuova, con un id nuovo**: passa un `id` diverso (o nessuno) alla
  `put`. `purge` sull'id continua a funzionare (l'oblio non chiude
  l'oblio), come sulla lapide.
- **Il percorso vero è l'import.** Reimportare un'esportazione vecchia
  o un backup che contiene la nota purgata fa proprio quella `put`. I
  percorsi d'import dell'engine non si fermano e non sbagliano: **salta
  la nota e la nominano**. Homun legge il rapporto così:
  - se importa da solo con `put` (la libreria non ha un metodo
    d'import cartella): cattura `NOTE_PURGED`, raccoglie l'id, continua
    con i file restanti — alla fine dice all'utente quali file
    riportano un id dimenticato e vanno buttati o riscritti;
  - se chiama la CLI (`engramma import --json`, per esempio dentro un
    flusso di migrazione): il rapporto JSON porta **`skippedPurged`**,
    la lista degli id saltati (accanto a `imported`, `skipped`,
    `errors`); la vista della cartella (il demone che guarda la
    cartella markdown) nomina gli id nello stesso campo del suo
    rapporto e lascia i file al loro posto.
- **(v0.6.10, J2) `verify` è una diagnosi: trova, non tocca.** Le celle
  scritte dalla v0.6.7/v0.6.8 possono contenere una `put` dopo la purga
  (il difetto del seme `1207…`): `verify` nomina quelle note in
  **`phantomPurged: [id…]`** — vuoto su una cella sana — e `ok` è
  **false**: la cella ha bisogno di repair. Non un byte si muove (il
  fantasma è contenuto che l'utente ha scritto dopo la purga: la regola
  dimentica l'id, non il testo).
- **`cell.repair()` è il passo esplicito.** Per ogni fantasma, in una
  transazione per ciascuno (un'interruzione lascia ogni nota o com'era o
  riparata; riparare due volte non fa niente la seconda): il contenuto
  va in una **nota nuova con un id nuovo** con una `put` normale
  (attore `engramma:repair`), poi la regola all'id vecchio. Risponde
  **`{ moved: [{ old, new }] }`** — la mappa vecchio → nuovo, che
  **Homun deve usare subito** per aggiornare i suoi riferimenti
  (i link nei messaggi, i promemoria, i riferimenti in conversazione):
  la mappa non si salva da nessuna parte nella cella, l'id dimenticato
  non deve restare scritto in nessun posto nuovo. Dopo `repair()`,
  `verify()` è verde (la stessa chiamata sistema anche le righe che i
  motori fino alla v0.6.9 scrissero con metà che la nuova regola di
  dominanza non rigioca più).
- **(v0.6.10, J3) il banco misura il binario giusto.** `engramma
  --version` e **`buildInfo()`** dicono entrambi il commit (con
  `-dirty`) e lo schema; il banco dei test confronta i due e **si
  rifiuta di partire** se non coincidono («la CLI in … è del commit X,
  l'addon del commit Y: ricostruisci o imposta ENGRAMMA_BIN»). Il
  `pretest` di `npm test` e `test:electron` costruisce la CLI: il caso
  normale è sempre fresco.

## La compattazione: l'ancora intera, e perché resta spenta ancora (v0.6.13)

La verifica della v0.6.11 lasciava la release locale a una condizione:
**con la compattazione spenta**, perché «dimentica per sempre» poteva
fallire per sempre su una nota il cui co-oggetto si rimaterializzava
attraverso un taglio (`VersionNotFound`, pulito ma permanente). La
v0.6.12 chiude quel difetto alla radice: **il prefisso congelato è
immutabile** — ciò che è nato a o sotto il taglio (le righe di versione,
l'ancora della rimaterializzazione, lo stato al taglio) non si cancella
e non si riscrive mai, una metà dominata dalla purga fotografa comunque
il pre-stato che ha osservato, e l'ancora non invecchia. Il contratto lo
dice nel test nuovo — `compact()` e poi `purge()` di una nota legata da
una sostituzione: la purga riesce, la vecchia riapre col suo corpo, e la
sua storia delle versioni resta identica (63/63, con i 62 di ieri
intatti). Il difetto di sync che restava aperto (l'ancora che
invecchia, H2 secondo strato) è chiuso anche lui, e la cella condivisa
rifiuta con un errore leggibile un arrivo che cada dentro il prefisso.

**v0.6.13: i due difetti della v0.6.12 sono chiusi — e la caccia ne ha
trovati due altri, dello stesso sangue.** L'ancora di una nota ora è
**l'oggetto intero**: la fotografia prende coda, registro e righe
`supersedes` accanto alla riga della nota, la ricostruzione li
ripristina allo stato del taglio prima di rigiocare la coda, e la
purga del chiudente riapre il lato vecchio. Il contratto lo dice nei
test nuovi (propose → decide → compact → purge: la coda sopravvive
intera; 64/64, con i 63 di ieri intatti), le celle d'oro v0612 e
v0613 aprono e verificano, e l'invariante nuovo — **dalla quarantena
si esce solo per decisione** — fa la guardia.

**Per Homun: la compattazione resta spenta lo stesso.** La caccia a
20 000 ha trovato DUE difetti nuovi nelle semantiche della coda e del
`restore` (dichiarati aperti in `open-defects.txt` coi semi):

- un `restore` dell'utente dopo annullamento e riaccettazione: la
  rimaterializzazione che una purga forza riordina le versioni, e la
  nota cambia stato senza che nessuna riga lo racconti;
- una proposta accettata e poi chiusa da un supersede: alla fine la
  coda dice `pending` e lo stato `superseded` — contenuto nato in
  quarantena visibile con la coda che non lo racconta più.

Fino alla fase che li chiude, `compact(keepLast)` non si chiama. Quando
si potrà: `keepLast` è quante operazioni restano vive sopra il taglio
(il CLI e il binding tagliano a `head − keepLast`); è la manutenzione di
una memoria che cresce, non una funzione dell'utente. Con la
compattazione spenta i due semi sono verdi, e la condizione della
v0.6.11 resta la strada sicura (la v0.6.12 al posto della v0.6.11).
I numeri e i giudizi sono nel REPORT della fase.

## Cosa riapre la purga, e come dirlo prima (v0.6.11, K1)

- **La regola: il contenuto proprio resta, i riferimenti vanno.** Una
  purga toglie la nota purgata, le sue copie di conflitto e ogni
  RIFERIMENTO a lei (le righe `supersedes`, gli archi, le chiusure che
  erano la SUA opera). Non toglie mai il contenuto proprio di un'altra
  nota: corpo, intestazione, esistenza, stato. Quindi:
  - dimenticare la VECCHIA di una sostituzione lascia la **nuova viva**
    (è il flusso principale di Homun: l'agente corregge, l'utente
    approva, poi dimentica il dato superato — e quello attuale resta);
  - dimenticare la **correzione accettata** prima che l'utente ne
    purghi il bersaglio lascia il bersaglio com'era;
  - una proposta il cui bersaglio è stato dimenticato **resta in coda**:
    il testo è dell'agente, l'utente può ancora vederlo e rifiutarlo,
    e se l'accetta atterra come **nota nuova** (niente da chiudere).
- **L'eccezione dichiarata: le RIAPERTURE.** Dimenticare la nota NUOVA
  di una sostituzione — o la PROPOSTA accettata di una correzione —
  **riapre** ciò che quella nota aveva chiuso: la vecchia torna allo
  stato che aveva prima della chiusura (di solito `active`). È «come se
  non fosse mai esistita», ed è giusto — ma per l'utente è una
  sorpresa: il dato vecchio torna attuale. **L'interfaccia deve dirlo
  PRIMA della purga.**
- **`cell.purgePreview(id)` è l'anticipo, di sola lettura (nuova in
  questa fase).** Prima non c'era un'API che lo dicesse; ora c'è:
  ```js
  const { reopens } = await cell.purgePreview(id);
  // reopens: [{ id, title }] — le note che tornerebbero valide
  ```
  Una domanda, una lettura, nessuna operazione. Homun la chiama sulla
  conferma della purga e, quando `reopens` non è vuoto, avverte:
  **«tornerà valido: …»** con i titoli (o i corpi abbreviati) delle
  note indicate — così l'utente sa che cosa sta per tornare attuale
  prima di confermare l'oblio.
- **Come si costruisce l'anticipo:** `purgePreview` legge le righe
  `supersedes` il cui CHIUSORE è l'id (la vecchia di un supersede, il
  bersaglio di una correzione accettata portano entrambe quella riga) e
  nomina quelle ancora in stato `superseded`. La stessa regola decide
  il rigioco: l'anticipo non può mentire.

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

## La sincronizzazione accanto al demone (v0.6.1)

In questa fase **il contratto JS non cambia**: Homun sincronizza le sue
celle facendo girare accanto `engramma sync serve --root <radice di
Homun>`. I due processi convivono: il demone scrive, `sync serve`
sincronizza a ogni scrittura (la rileva con `PRAGMA data_version`,
rimbalzo 2 s) e ogni 5 minuti, e si ferma pulito su SIGINT/SIGTERM. La
convivenza — un secondo processo che scrive sulla stessa radice mentre il
servizio riceve da un altro dispositivo — è provata dal banco end-to-end
della CLI: alla fine tutte le copie convergono e `verify` passa.

**Lo schema delle celle sale alla versione 20** (v15: `content_hash`,
`base`, `op_objects`; v16: `pending_objects`; v17: `conflict_copies`;
v18: `device_acks`; v19: `device`, `sig`; v20: `payload_state` e l'indice
su `content_hash` — l'identità non cambia più, DESIGN § 4.3.1): le celle
migrate **non tornano indietro** a un binario più vecchio (il
`SCHEMA_TOO_NEW` della v0.5 le rifiuta). Migrazione all'apertura,
irrovocabile, con `verify` verde dopo.

**L'API JS della sincronizzazione** (stato, abbinamento col QR del
biglietto, elenco dei dispositivi) è arrivata nella v0.6.2 — la sezione
qui sotto; questa fase aveva consegnato il demone `sync serve`, `sync
run` e la CLI completa (`devices`, `sync`, `mailbox`, docs/sync.md).

## La sincronizzazione dentro il processo (v0.6.2)

La v0.6.2 porta il servizio **dentro** il modulo: un oggetto per Engine,
sul suo thread, senza CLI accanto. Il ciclo è quello provato della
v0.6.1 (il cancello all'accettazione, la passata a ogni scrittura locale
e ogni 5 minuti, il riposo di K1) — la CLI è diventata un guscio dello
stesso servizio e i suoi test girano immutati.

```ts
export interface SyncOpts {
  deviceName?: string              // il nome di questo dispositivo (alla prima creazione, e quello che un join presenta); predefinito 'portatile'
  relay?: 'default' | 'none' | { url: string }   // come i flag della CLI; si salva nella radice
}
export class Engine {
  sync(opts?: SyncOpts): Promise<Sync>   // un servizio per Engine: le chiamate successive restituiscono lo stesso
}
export class Sync {
  status(): Promise<SyncStatusView>          // questo dispositivo, serving, relay, le celle condivise
  devices(): Promise<SyncDeviceView[]>       // nome, id, è-questo, ultimo contatto, diretto/relay, vivo/rimosso

  invite(): Promise<Pairing>                 // col biglietto (dieci minuti, un uso)
  code(): Promise<Pairing>                   // col codice breve (serve il nodo cieco: altrimenti NO_MAILBOX)
  join(ticketOrCode: string): Promise<Pairing>  // entra col biglietto (entrambe le forme) o col codice

  remove(name: string): Promise<RemoveView>  // rimuove e ruota le chiavi; il rimosso lo sente e si ferma da sé
  share(cell: string): Promise<ShareView>
  unshare(cell: string): Promise<void>       // NOT_SHARED se non è condivisa

  runOnce(): Promise<PassView>               // una passata: un irraggiungibile è una riga, mai un errore
  start(): Promise<ServeInfoView>            // il servizio continuo (SYNC_BUSY se un altro lo tiene)
  stop(): Promise<void>                      // si conclude entro 5 s, anche a metà sessione

  onEvent(cb: (e: SyncEvent) => void): () => void   // l'ascoltatore e la funzione che lo toglie
}
export class Pairing {
  readonly ticket?: string; readonly ticketQr?: string   // il biglietto nelle due forme
  readonly code?: string
  readonly expiresAt: string; readonly relay: string | null; readonly relayWarning?: string
  readonly rounds: number                     // quante volte si confronta il SAS (1 col biglietto, 2 col codice)
  sas(): Promise<string>                      // le sei cifre, quando entrambi le hanno
  confirm(same: boolean): Promise<void>       // la risposta dell'umano; prima delle cifre è INVALID_ARGUMENT
  done(): Promise<PairingResultView>          // l'esito: l'altro dispositivo e le celle
  cancel(): void                              // chiude ora: l'altro capo sente un rifiuto pulito
}
```

**L'abbinamento da una interfaccia**, per intero. Sul dispositivo che
invita:

```ts
const sync = await engine.sync({ deviceName: 'portatile', relay: 'none' });
await sync.share('memoria');                 // la cella da consegnare (dopo il primo invito se vuoi il nome tuo)
const pairing = await sync.invite();         // si risolve col biglietto pronto
showQr(pairing.ticketQr);                    // maiuscole, cifre e pochi simboli: QR alfanumerico, più piccolo e leggibile
const digits = await pairing.sas();          // le sei cifre, quando l'altro è collegato
if (await askHuman(digits)) await pairing.confirm(true);
const done = await pairing.done();           // { peer: { name: 'mac', … }, cells: […] }
```

Sul dispositivo che entra:

```ts
const sync = await engine.sync({ deviceName: 'mac', relay: 'none' });
const pairing = await sync.join(ticketScanned);   // accetta il testo E la forma QR
const digits = await pairing.sas();
if (await askHuman(digits)) await pairing.confirm(true);
await pairing.done();
```

Il SAS non si salta: `confirm(true)` prima che `sas()` si sia risolta è
`INVALID_ARGUMENT`, e non esiste opzione che confermi da sé. Se l'umano
non risponde entro due minuti è `PAIRING_TIMEOUT` (l'altro capo riceve
un rifiuto pulito, non un appeso); un `confirm(false)` è
`PAIRING_REJECTED` da entrambi i lati e nessuna `add` finisce
nell'elenco. Col **codice breve** le battute sono due (l'incontro sul
nodo cieco e il flusso del biglietto): `pairing.rounds` lo dice, e
`sas()`/`confirm()` si ripetono.

**Gli eventi** escono da `onEvent`: `pass` (una passata finita, per
dispositivo e per cella), `arrived` (operazioni nuove applicate: la UI
aggiorna la vista e chiama `embedPending` — i vettori non viaggiano, si
calcolano da chi riceve), `device` (uno entra o esce, sentito da un
altro), `removed` (questo dispositivo è stato rimosso: il servizio si
ferma da sé, le chiavi nuove non arriveranno), `unsent` (operazioni che
non si possono sigillare), `error` (un errore del ciclo che non lo
ferma). Nulla di tutto ciò porta testo di note, biglietti o codici.

```ts
const off = sync.onEvent((e) => {
  if (e.type === 'arrived') refresh(e.cell).then(() => cell.embedPending());
});
// …
off();
```

**Un ascoltatore da solo non tiene vivo il processo** (v0.6.3): la
semantica è quella di un `EventEmitter` — ciò che tiene vivo il
processo è il motore aperto, e `close()` svuota l'hub e rilascia ogni
chiamata, anche quella di chi non ha mai chiamato `off()`. Il processo
esce da solo entro un attimo dopo `close()` (il test lo misura da un
figlio: ~100 ms); `off()` dopo `close()` non lancia, toglie nulla.

**Il ciclo di vita**: il servizio gira su un thread suo; il ciclo a
riposo non fa passate (il test lo guarda per 30 s da JS). `start()` due
volte non fa nulla la seconda, `stop()` a servizio spento altrettanto,
`stop()` durante una passata si conclude entro 5 s. `Engine.close()`
ferma il servizio: un'app Electron che esce col servizio acceso non si
appende (provato in `test:electron-main`). Le scritture fatte con
`Cell.put` dello stesso processo accendono il guardiano come quelle di
un processo accanto: la firma del dispositivo si arma da sola sulle
celle condivise. L'armo si decide **alla scrittura** (v0.6.3): una
cella condivisa da un altro processo — la CLI — mentre l'Engine la tiene
aperta firma la prima scrittura dopo il cambio; una scrittura paga una
stat di `devices.db`, e una radice che non sincronizza non paga altro. **Un solo servizio per radice**: un secondo
`engramma sync serve` sulla stessa radice (o un altro servizio nello
stesso processo) riceve `SYNC_BUSY` — un lucchetto esclusivo del sistema
operativo, `<root>/sync.lock`.

**I codici d'errore nuovi**: `SYNC_BUSY`, `PAIRING_EXPIRED`,
`PAIRING_REJECTED`, `PAIRING_TIMEOUT`, `NO_MAILBOX`, `NOT_SHARED`,
`LAST_DEVICE` (e `CANCELLED` per l'abbinamento annullato), accanto ai
soliti `NOT_FOUND`, `INVALID_ARGUMENT`, `CONFLICT`, `CLOSED`, `IO`.
`dropCell` su una cella condivisa risponde `CONFLICT` e dice di chiamare
prima `unshare`. Il nodo cieco resta della CLI (`mailbox add/remove`):
non si gestisce da JS in questa fase.
