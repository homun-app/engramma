# Motore di memoria — documento di progetto (bozza 1)

> **Nome: Engramma** (scelto dall'utente il 2026-11-05: l'engramma è la traccia fisica di un ricordo; libero su crates.io, 14 progetti omonimi su GitHub). Bozza del 2026-11-05.

## 1. Visione

Un **motore di memoria open source**, scritto in Rust, che:

- tiene ciò che un assistente sa di una persona, di un gruppo o di un'azienda come **note leggibili** (markdown), legate in un **grafo**, con **tag**, **tempo**, **fonte** e **affidabilità**;
- risponde con una **ricerca mista**, cioè testo + significato + grafo + tempo, e **spiega perché** ogni risultato è uscito;
- si **incorpora** in qualsiasi programma (Homun prima di tutti) **senza servizi esterni**, su macOS, Windows, Linux, iOS e Android;
- con lo **stesso codice** gira come **server** e in **cluster** che scala orizzontalmente;
- si legge da altri assistenti (Claude, ChatGPT, Cursor…) via **MCP**, solo per gli ambiti autorizzati;
- è la base per la **condivisione cifrata** tra installazioni diverse (Homun ↔ Homun).

**Homun è il primo cliente del motore, non il suo padrone.** Il motore non sa niente di bot, chat o approvazioni: offre primitive. Le regole su cosa ricordare e quando le decide l'applicazione.

### Cosa NON fa
- **Non decide cosa ricordare.** Estrarre i fatti da una conversazione è compito dell'applicazione, con il suo modello.
- **Non chiama un LLM.** Calcola gli embedding con un modello piccolo e dedicato (§ 7), ma non genera testo.
- **Non è un database generico:** lo schema è quello della memoria.
- **Non fa ricerca su dati cifrati lato server** (§ 10).

### Perché non usare qualcosa che esiste (verificato il 2026-11-05)
- **SurrealDB** è multi-modello e incorporabile, ma ha licenza BSL: ogni versione diventa Apache solo dopo 4 anni, e nel frattempo vieta di offrirla come servizio. Non è compatibile con un progetto aperto che deve girare anche come servizio.
- **Kùzu** è archiviato (ottobre 2025). **CozoDB** è fermo dal 2024.
- **Turso**, cioè SQLite riscritto in Rust, è in beta (v0.8-pre a settembre 2026), con le scritture concorrenti ancora sperimentali. Lo strato di accesso ai dati sarà sostituibile (§ 4.5), così potremo adottarlo quando sarà maturo.
- **Mem0, Zep/Graphiti, Basic Memory** sono servizi o librerie con una sola delle facce (vettori, grafo, markdown). Nessuno unisce fonte e tempo per ogni fatto, quarantena, incorporamento senza servizi, cluster e sincronizzazione cifrata.

## 2. Concetti

| Concetto | Cos'è |
|---|---|
| **Nota** | L'unità di memoria: un'intestazione strutturata più un corpo markdown. Può essere un fatto, una persona, un progetto, un evento… |
| **Entità** | Un nodo del grafo con un nome (Giulia, Milano, progetto X). Nasce dai collegamenti `[[…]]`; può avere la sua nota. |
| **Relazione** | Un arco tipizzato tra entità o note (`abita_a`, `lavora_con`), con tempo, fonte e affidabilità. |
| **Tag** | Etichette gerarchiche (`casa/milano`). La ricerca per prefisso include i figli. |
| **Cella** | Uno **spazio di memoria** (la mia, quella della famiglia, quella di un bot): un'unità autonoma con il suo database, il suo indice e il suo registro. Si scala e si condivide per celle. |
| **Registro** | Il log ordinato delle modifiche di una cella. È la base della replica, della sincronizzazione, dello storico e dell'audit. |
| **Fonte** | Da dove viene una nota: per esempio `homun:chat/<bot>/<entry>`, `import:chatgpt/…`, `user`. |
| **Affidabilità** | `stated` (detto dall'utente), `inferred` (dedotto dall'assistente), `external` (da contenuti di terzi). Il valore `external` nasce **in quarantena**. |
| **Validità** | `valid_from` / `valid_to`: quando un fatto è vero nel mondo. È distinto da `created_at`/`updated_at`, cioè quando l'abbiamo saputo (modello **bitemporale**). |

## 3. Il formato delle note (specifica v0)

I campi sono **in inglese**: il formato vuole essere uno standard aperto. Il contenuto sta nella lingua dell'utente.

```markdown
---
id: 01JC8Z6Q2W8M3N5V4T7Y9R0P1S        # ULID, generato dal motore
type: fact                             # fact | person | place | project | event | preference | note
title: Fabio abita a Milano            # facoltativo; altrimenti la prima riga
tags: [famiglia, casa/milano]
scope: famiglia                        # la cella logica (vedi § 4.1)
source: homun:chat/bot-casa/8812
confidence: stated                     # stated | inferred | external
status: active                         # active | quarantined | superseded | deleted
valid_from: 2026-09-01
valid_to:                              # vuoto = vale ancora
supersedes: [01JB…]                    # note sostituite
created_at: 2026-09-03T10:12:00Z
updated_at: 2026-09-03T10:12:00Z
x-homun: { bot: casa }                 # estensioni libere con prefisso x-, conservate al giro
---
Fabio abita a [[Milano]] con [[Giulia]] dal settembre 2026.

## Relations
- lives_in:: [[Milano]] (from 2026-09-01)
- partner_of:: [[Giulia]]
```

**Regole:**
- **Intestazione YAML vera.** Le chiavi sconosciute si conservano; quelle che non appartengono allo standard hanno il prefisso `x-`.
- **Collegamenti `[[Nome]]` e `[[Nome|testo]]`** creano archi `mentions` verso l'entità `Nome`. L'entità si crea da sola se manca.
- **Relazioni tipizzate** nella sezione `## Relations`, una per riga: `- <relazione>:: [[Entità]]`. L'opzionale `(from … to …)` dà la validità dell'arco. La sintassi è compatibile con Obsidian/Dataview: la cartella si apre in Obsidian.
- **Tag** nell'intestazione e come `#tag/figlio` nel corpo.
- **Giro senza perdite:** `parse(serialize(n)) == n`, verificato con test di proprietà.
- **Esportazione:** una cartella di file `.md`, uno per nota (`<type>/<slug>-<id-corto>.md`), più `engramma.json` (versione del formato, celle, modello di embedding). L'importazione accetta la stessa cartella e una cartella Obsidian qualsiasi.

## 4. La cella

### 4.1 Cos'è
Ogni cella è **un file SQLite** (più, da v0.2, i suoi vettori) e **si basta da sola**: la si copia, la si sposta su un altro nodo o la si replica come un'unità. Un'installazione locale ha poche celle; un server può averne migliaia.

> **Come funziona la vista (v0.3):** una **riconciliazione per scansione** deterministica, con gli eventi del file system (`notify`) solo come innesco: i watcher perdono eventi.
> - Il file è legato alla nota dall'`id` nell'intestazione, non dal percorso.
> - Scritture atomiche, soppressione dell'eco tramite hash.
> - In conflitto vince la cella, e la versione del file diventa una copia di conflitto.
> - Una guardia blocca le cancellazioni di massa.

> **Decisa con l'utente il 2026-11-05:** la **fonte di verità è la cella, con il suo registro**, non la cartella di markdown. Per replica, sincronizzazione e audit serve un registro ordinato, e una cartella di file non può darlo. La cartella markdown resta una **vista bidirezionale** in modalità locale (v0.3): un controllo dei file trasforma le modifiche fatte a mano, anche da Obsidian, in operazioni del registro. Su server e in cluster i file non esistono: si esportano su richiesta.

### 4.2 Schema (v0.1)

```sql
CREATE TABLE meta      (key TEXT PRIMARY KEY, value TEXT);          -- versione schema, id cella, HLC
CREATE TABLE notes     (id TEXT PRIMARY KEY, type TEXT, title TEXT, body TEXT,
                        frontmatter_json TEXT,       -- tutto il frontmatter (anche x-*)
                        scope TEXT, source TEXT, confidence TEXT, status TEXT,
                        valid_from TEXT, valid_to TEXT,
                        created_at TEXT, updated_at TEXT, version INTEGER,
                        body_norm TEXT,              -- testo normalizzato per FTS (§ 6.1)
                        tags_text TEXT);             -- gemella della colonna FTS tags (v0.1.1: l'indice è verificabile)
CREATE TABLE note_versions (note_id TEXT, version INTEGER, op_id TEXT, snapshot_json TEXT,
                            PRIMARY KEY (note_id, version));
CREATE TABLE tags      (name TEXT PRIMARY KEY, parent TEXT);
CREATE TABLE note_tags (note_id TEXT, tag TEXT, PRIMARY KEY (note_id, tag));
CREATE TABLE entities  (id TEXT PRIMARY KEY, name TEXT UNIQUE COLLATE NOCASE, type TEXT, note_id TEXT);
CREATE TABLE edges     (id TEXT PRIMARY KEY, src TEXT, rel TEXT, dst TEXT,   -- src/dst: entity o note
                        note_id TEXT,                -- la nota che dichiara l'arco
                        valid_from TEXT, valid_to TEXT, confidence TEXT, status TEXT);
CREATE TABLE supersedes (new_id TEXT, old_id TEXT, PRIMARY KEY (new_id, old_id));
CREATE TABLE ops       (seq INTEGER PRIMARY KEY, op_id TEXT UNIQUE, hlc TEXT, actor TEXT,
                        kind TEXT, note_id TEXT, payload_json TEXT, prev_hash TEXT, hash TEXT);
CREATE VIRTUAL TABLE notes_fts USING fts5(title, body_norm, tags_text, content='notes', content_rowid='rowid',
                                          tokenize='unicode61 remove_diacritics 2');
-- v0.2: CREATE VIRTUAL TABLE notes_vec USING vec0(embedding float[N]); + tabella embedding_meta(model, dim)
```

Indici: `edges(src)`, `edges(dst)`, `edges(rel)`, `notes(status, valid_to)`, `notes(type)`, `note_tags(tag)`.

### 4.3 Il registro delle modifiche
- **Ogni scrittura è un'operazione** (`put_note`, `patch_note`, `set_status`, `delete_note`, `link`, `unlink`). L'operazione e la sua applicazione alle tabelle avvengono **nella stessa transazione**.
- Ogni operazione porta:
  - un `op_id` ULID;
  - un **orologio logico ibrido** (HLC: ordina le modifiche tra macchine con orologi diversi);
  - l'`actor` (chi ha scritto: utente, app, bot, pari);
  - un **hash concatenato** (`prev_hash`), così il registro è a prova di manomissione ed è verificabile.
- **Registro v2, payload dimenticabili** (*deciso il 2026-11-06, v0.3*):
  - la catena non copre il payload, ma un suo **digest salato** (`blake3(sale ‖ payload)`, la tecnica di SD-JWT, RFC 9901);
  - il payload vive in una tabella a parte e si può cancellare senza rompere la catena;
  - il sale distrutto rende il digest non collegabile al dato: un hash nudo di un dato personale può restare un dato personale, uno salato no;
  - è la base comune di `purge` (§ 4.4) e della compattazione.

  Si è preferito al crypto-shredding perché cancella davvero il dato e non richiede la gestione di chiavi; il crypto-shredding è contestato come cancellazione in alcune giurisdizioni.
- **Determinismo (la proprietà più importante):** rigiocando il registro da zero si ottiene **esattamente** lo stesso stato. Va verificato con test di proprietà. La replica (§ 9) e la sincronizzazione (§ 10) si reggono su questo.
- **Conflitti** (stessa nota cambiata in due posti) in v0.5:
  - sui campi dell'intestazione vince l'HLC più recente;
  - sul corpo, una **copia di conflitto** visibile all'utente, come fanno i sistemi di sincronizzazione maturi.

  Una fusione automatica del testo (CRDT) è rimandata e va misurata prima di adottarla.
- **Compattazione** (v0.3):
  - un'istantanea della cella a un `seq` si fa con `VACUUM INTO` ed è registrata con l'hash di testa e un **digest dello stato** (`state_digest`, il dump canonico delle tabelle logiche);
  - i payload delle operazioni fino a quel `seq` si dimenticano, mentre gli scheletri con gli hash restano: la catena è intera;
  - `verify` controlla l'istantanea e rigioca solo le operazioni successive;
  - `changes_since` sotto l'istantanea risponde `Compacted`: la replica parte dall'istantanea (§ 9).

### 4.4 Versioni e cancellazione
- Ogni modifica crea una riga in `note_versions`: lo storico è consultabile e ripristinabile.
- `delete_note` lascia una **lapide** (`status: deleted`), che si propaga alle repliche. La cancellazione definitiva (diritto all'oblio) è un'operazione esplicita, `purge`, che cancella anche le versioni e propaga la lapide.
- `purge` (v0.3) **dimentica i payload** di tutte le operazioni della nota (registro v2, § 4.3). Toglie anche vettori, righe FTS, archi dichiarati ed entità rimaste orfane, e il file della vista markdown. Il suo payload contiene solo l'id e un motivo. Le menzioni `[[Nome]]` in altre note restano: sono altre note. Le esportazioni già fatte sono fuori dal controllo del motore.

### 4.5 Lo strato di accesso ai dati
Il trait `CellStore` isola SQL, FTS e vettori: un'implementazione SQLite oggi, Turso o altro domani. Il resto del motore non contiene SQL.

## 5. Affidabilità, quarantena, fonte

- **Ogni scrittura richiede `source` e `confidence`**, altrimenti viene rifiutata.
- `confidence: external` → `status: quarantined`, cioè:
  - escluse dalla ricerca predefinita;
  - visibili con `include_quarantined`;
  - promosse ad `active` solo con `confirm(note_id, actor)`, scritto nel registro.
- **«Perché lo so?»:** ogni risultato porta la sua `source`. L'applicazione la trasforma in un collegamento, per esempio la conversazione di Homun.
- **Fatti che cambiano.** Una nota che sostituisce un'altra (`supersedes`) le mette `valid_to` e la marca `superseded`. Restano nello storico e rispondono alle domande «com'era prima».

## 6. La ricerca mista

### 6.1 Testo
- FTS5 (BM25) su `title`, `body_norm` e `tags`.
- `body_norm` è il testo minuscolo, senza accenti, **con le parole ridotte alla radice** nella lingua della nota (`rust-stemmers`, Snowball: italiano, inglese e altre). Il tokenizzatore predefinito di FTS5 non sa l'italiano. La lingua si rileva (`whatlang`) o si dichiara.
- La stessa normalizzazione si applica alla domanda.

### 6.2 Significato (v0.2)
> **Rivisto il 2026-11-05, misurando:** in v0.2 niente `sqlite-vec`.
> - **Stato di sqlite-vec:** la versione stabile (0.1.9) fa solo ricerca esatta. DiskANN e IVF esistono soltanto nelle `0.1.10-alpha`.
> - **Misure su 100 000 vettori da 384 dimensioni:**
>
>   | metodo | p50 |
>   |---|---|
>   | sqlite-vec, float esatto | 28 ms |
>   | sqlite-vec, bit | 14 ms |
>   | Rust, int8 top-200 + ricalcolo f32 | 0,7 ms |
> - **Costo d'integrazione:** sqlite-vec richiederebbe un `unsafe` per registrare l'estensione.
- **I vettori sono dati derivati:**
  - stanno in una tabella `note_vectors` della cella (f32, con modello e hash del testo);
  - non entrano nel registro, nel rigioco né nell'export: si ricalcolano dalle note. Così il determinismo del § 4.3 non dipende dalla virgola mobile delle piattaforme.
- **Indice dietro il trait `VectorIndex`:**
  - v0.2: `FlatI8`, una scansione esatta su int8 in memoria più il ricalcolo in f32 dei primi candidati, costruita pigramente alla prima ricerca;
  - in seguito, per celle oltre il milione di note, un HNSW (`usearch`) o DiskANN di sqlite-vec quando sarà stabile.
- **Unità di indicizzazione:** un vettore per nota, oppure per blocco di paragrafi se la nota supera `max_tokens`. Il punteggio della nota è il massimo dei suoi blocchi.
- **Filtri:** i candidati vettoriali passano dagli stessi filtri di quelli testuali (quarantena compresa) **prima** della fusione e dell'allargamento nel grafo.

### 6.3 La domanda

```text
query {
  text, k=10,
  filters { types[], tags[] (con figli), scope, as_of (data), min_confidence,
            include_quarantined=false, include_superseded=false },
  graph_hops = 1 (0–2),
  weights { recency_half_life_days = 180, … }
}
```

1. **Candidati in parallelo:** 50 dal testo (BM25), 50 dal significato (dalla v0.2).
2. **Fusione RRF** (somma di `1/(60 + posizione)` per ogni elenco): non serve calibrare i punteggi tra loro.
3. **Grafo come terza lista** (*rivisto il 2026-11-06, v0.3*): un **PageRank personalizzato a spinta** (Andersen–Chung–Lang) parte dai primi risultati fusi e già filtrati. Corre su un grafo note↔entità **in memoria** (`GraphIndex`: CSR derivato, costruito pigramente, aggiornato dalle scritture) e i suoi punteggi entrano nella fusione come terza lista.
   - Era: un allargamento a passi fissi (0,6 / 0,36), con gli archi letti da SQLite a ogni domanda.
   - Motivo, misurato sulla v0.2.1 a 100k: il grafo prendeva il 59% del tempo (p95 ibrido 59,8 ms) e in ibrido il multi-salto crollava a 0,063.
   - Il prototipo di spinta: con ε = 10⁻³ tocca ~157 archi per domanda anche con hub da 16 741 archi, perché la soglia proporzionale al grado non espande gli hub, e trova la nota a due salti prima in 200 domande su 200.
   - È la linea di HippoRAG 2.
4. **Filtri temporali:** con `as_of` vale solo ciò che era valido in quella data. Di norma, solo ciò che vale oggi.
5. **Riordino:** `punteggio = RRF × peso_affidabilità × peso_recenza × peso_grafo`. I pesi sono configurabili e i valori predefiniti vanno tarati sul benchmark (§ 11).
6. **Spiegazione per ogni risultato:** `matched_terms`, `semantic_rank`, `via: [[Giulia]] → lives_in`, `confidence`, `valid_from`, `source`.

### 6.4 Altre interrogazioni
- `get(id)`, `history(id)`, `neighbors(entity|note, depth, rels[])`, `path(a, b)`.
- `tags(prefix)`, `entities(prefix)`, `timeline(entity)`.
- `changes_since(seq)`, per le applicazioni e le repliche.

### 6.5 Supporto al consolidamento (v0.3)
`consolidation_candidates()` restituisce le coppie quasi duplicate (vettori molto simili e stesse entità) e le **contraddizioni probabili** (stessa entità e relazione, valori diversi, entrambi `active`). **Decidere** cosa fondere spetta all'applicazione, con il suo modello e con l'utente.

## 7. Embedding

- Il trait `Embedder` sta nel nucleo (senza I/O) e ha tre implementazioni in `engramma-embed`:
  - **incorporato**, quello predefinito: ONNX via **tract**, in puro Rust. *Deciso il 2026-11-05:* dà gli stessi vettori di ONNX Runtime (cos 1,000000 su Granite R2), senza librerie native da distribuire, quindi anche su mobile e WASM. `ort` è più veloce (~2×), ma è ancora in rc e scarica binari: in futuro sarà al più un backend facoltativo. candle è scartato, perché il suo ModernBERT ha l'attivazione fissa (gelu) e Granite R2 usa `silu`;
  - **Ollama** (`/api/embed`);
  - **HTTP compatibile OpenAI** (`/v1/embeddings`), per il server.
- **Il modello predefinito si decide misurando, in v0.2**, sul benchmark italiano (§ 11). Candidati verificati il 2026-11-05:
  - **IBM Granite Embedding Multilingual R2**, Apache 2.0, ModernBERT, italiano tra le lingue di addestramento, nessun prefisso:
    - 97M: 384 dimensioni, MTEB Multilingual Retrieval 60,3;
    - 311M: 768 dimensioni con Matryoshka, 65,2.
  - **EmbeddingGemma 308M** (62,5), con **licenza Gemma, non open source**: supportato come cartella fornita dall'utente, non incluso né scaricato di serie.
  - I modelli si scaricano con `engramma model pull`, con revisione e sha256 fissati.
- **Cambio di modello «blu/verde».** Ogni vettore porta il modello e l'hash del testo. Il modello nuovo si costruisce (`building`) mentre il vecchio (`ready`) continua a servire la ricerca; lo scambio è atomico a copertura completa. La ricerca per significato non si spegne mai. *Rivisto il 2026-11-05:* prima si spegneva durante il ricalcolo.
- **Le scritture non calcolano embedding:** le note nuove restano in attesa, e `embed_pending` le elabora a lotti. L'inserimento resta veloce.

## 8. Come si usa

### 8.1 Pacchetti (workspace Cargo)

```text
engramma/
  crates/engramma-core      # libreria: formato, cella, registro, ricerca (nessun I/O di rete)
  crates/engramma-embed     # embedder (ONNX, Ollama, HTTP)
  crates/engramma-cli       # binario `engramma`: init, add, get, edit, rm, search, neighbors, tags,
                        #   history, import, export, verify, mcp, (serve)
  crates/engramma-mcp       # server MCP (stdio, poi HTTP)
  bindings/node         # napi-rs (Homun)
  bindings/python       # PyO3            (dopo v0.4)
  bindings/uniffi       # Swift/Kotlin    (dopo v0.4)
  spec/                 # specifica del formato, versionata, con esempi validi/non validi
  bench/                # benchmark (§ 11)
```

### 8.2 API del nucleo (Rust, abbozzo)

```rust
let engine = Engine::open(EngineConfig { root: path, embedder: Embedder::Builtin, .. })?;
let cell = engine.cell("famiglia")?;                       // la crea se manca
let id = cell.put(NoteInput { body, kind: Fact, tags, source, confidence: Stated, .. }, Actor::app("homun"))?;
cell.supersede(old_id, NoteInput { .. }, actor)?;
cell.confirm(id, Actor::user())?;                          // quarantena → active
let hits = cell.search(Query::text("trasloco di Giulia").graph_hops(1).k(10))?;  // con .explain
let near = cell.neighbors(EntityRef::name("Giulia"), 2, &[])?;
let hist = cell.history(id)?;  cell.restore(id, version, actor)?;
for op in cell.changes_since(seq)? { .. }                  // flusso delle modifiche
```

- **Concorrenza:** SQLite in modalità WAL. Più processi possono leggere la stessa cella, cioè il daemon di Homun e il server MCP. Le scritture passano da un solo scrittore per cella, con un lock di cella.
- **Errori:** tipizzati, mai un `panic` attraverso un confine FFI (napi-rs li trasforma in eccezioni JS).

### 8.3 Server MCP
- **Strumenti** (in sola lettura nella v0.1): `memory_search`, `memory_get`, `memory_neighbors`, `memory_tags`, `memory_timeline`. Più avanti `memory_propose`, che **scrive solo in quarantena**.
- **Ambiti:** il server si avvia con l'elenco delle celle esposte (`engramma mcp --cells famiglia,preferenze`). In HTTP si usano token per ambito. Ogni lettura finisce nel registro di audit della cella.

### 8.4 Homun (v0.4)
- **Collegamento in-process:** il daemon carica il modulo `napi-rs`.
- **Migrazione:** le note di oggi (`agents/<id>/memory`, `user/memory`) diventano celle:
  - `bot-<id>` per le note di un bot;
  - `utente` per quelle condivise;
  - `core` diventa un tag;
  - `source` vale `migrated:homun`.
- **Strumenti del bot:** `recall_memory` e `update_memory` restano nell'interfaccia, ma usano il motore. Nascono `search_history` e `read_history`, che usano un indice delle conversazioni in una cella `storia` dedicata.
- **Quarantena:** ciò che il bot ricava in un turno contaminato si scrive come `external`.
- **La sezione Memoria di Homun** è un'interfaccia sul motore: elenco, ricerca, grafo, «perché lo so?», conferma della quarantena, storico.

## 9. Server e cluster (architettura, non costruita prima della v0.6)

- **`engramma serve`:** API HTTP/gRPC + MCP HTTP, autenticazione con token per ambito o OIDC, migliaia di celle per processo e celle aperte su richiesta, con un tetto di celle aperte.
- **Cluster:**
  - **Metadati di posizionamento** (quale cella sta su quale nodo): un piccolo gruppo Raft (`openraft`).
  - **Router senza stato** davanti ai nodi. Ogni domanda tocca una cella. Le domande su più celle si mandano a tutte in parallelo e si fondono con RRF.
  - **Replica per cella:** il registro (§ 4.3) si replica con Raft su più nodi, in gruppi multi-Raft che raggruppano più celle per gruppo. La replica è deterministica, quindi le copie restano identiche.
  - **Spostare una cella:** istantanea (API di backup di SQLite più i vettori), recupero del registro dal numero di sequenza dell'istantanea, poi cambio del posizionamento.
  - **Backup:** istantanee e registro su storage a oggetti (stile S3).
- **Scalabilità orizzontale:** si aggiungono nodi e si spostano celle. Una cella enorme (un'azienda) si divide **per ambito** in più celle: le celle sono il confine sia della scala sia dei permessi.

## 10. Cifratura e condivisione

- **Sul disco:**
  - in locale ci si affida alla cifratura del sistema (FileVault, BitLocker);
  - più avanti, opzionale, la cifratura della cella. **Da decidere:** SQLCipher, licenza BSD, che è C ma si incorpora bene, oppure cifratura a livello di pagina.
- **Condivisione Homun ↔ Homun (v0.5):**
  - **Identità:** una coppia di chiavi per installazione (Ed25519/X25519).
  - **Abbinamento** con un codice o un QR e verifica dell'impronta.
  - **Le operazioni del registro** di una cella condivisa viaggiano **cifrate da un capo all'altro**:
    - la cella ha una sua chiave (XChaCha20-Poly1305), distribuita ai partecipanti cifrata con le loro chiavi;
    - in seguito, per i gruppi, **MLS (RFC 9420)**, che rinnova le chiavi quando qualcuno entra o esce.
  - **Un nodo che inoltra soltanto** conserva e passa operazioni cifrate e **non può cercare**: la ricerca la fa ogni partecipante sulla sua copia.

  È una scelta consapevole: oggi la ricerca su dati cifrati è lenta e limitata.
  - **Permessi per cella:** lettura, proposta (che finisce in quarantena), scrittura. Sono revocabili, e la revoca ruota la chiave.
- **Agenti di altri sistemi:** sopra il canale cifrato si possono scambiare messaggi A2A, ma il canale e i permessi restano i nostri. A2A di per sé non è cifrato da un capo all'altro.

## 11. Come si misura

- **Benchmark nostro**, in italiano e inglese: storie sintetiche di una persona nell'arco di mesi (traslochi, lavori, persone, preferenze che cambiano), con domande:
  - fattuali, temporali («dove abitava a marzo?»);
  - di aggiornamento;
  - multi-salto («chi lavora con il partner di…»);
  - di astensione (la risposta non c'è).

  Si misurano il richiamo@k e il MRR sulla parte del motore: **non** serve un giudice LLM, perché si giudica se la nota giusta è tra le prime k.
- **Confronto esterno:** un sottoinsieme di LongMemEval (aggiornamenti e astensione). LoCoMo solo con cautela: il suo giudice e le sue risposte di riferimento sono stati contestati nel 2026.
- **Prestazioni**, su un Mac M-series, cella da 100 000 note:
  - inserimento ≥ 2 000 note/s senza embedding;
  - ricerca testo + grafo con p95 ≤ 30 ms;
  - ricerca mista con embedding della domanda già calcolato, p95 ≤ 50 ms;
  - apertura di una cella ≤ 50 ms.

  Da confermare con le prime misure.
- **Correttezza:**
  - test di proprietà (giro del formato senza perdite; registro rigiocato = stesso stato; esportazione → importazione = stesso stato);
  - `engramma verify`: ricostruisce gli indici e li confronta.

## 12. Tabella di marcia

| Versione | Contenuto | Dipende da |
|---|---|---|
| **v0.1** | Formato e parser; cella SQLite; registro con HLC e hash; versioni e lapidi; FTS con radici italiano/inglese; entità, relazioni e tag; validità e quarantena; ricerca testo + grafo + tempo con spiegazioni; import/export della cartella; CLI; MCP stdio in sola lettura; benchmark e test di proprietà | — |
| v0.2 | Embedder (ONNX incorporato via tract, Ollama, HTTP); vettori derivati con indice `FlatI8`; ricerca mista (RRF o convessa, scelta misurando); scelta misurata del modello; cambio di modello blu/verde | v0.1 |
| v0.3 | Grafo in memoria con PPR come terza lista (costo e multi-salto); registro v2 con payload dimenticabili; `purge`; compattazione con istantanee; vista markdown bidirezionale (riconciliazione per scansione, `notify` come innesco); candidati al consolidamento | v0.2 |
| v0.4 | Collegamento Node (napi-rs); **integrazione in Homun**: migrazione, strumenti, `search_history`, sezione Memoria, quarantena dai turni contaminati, audit MCP con ambiti | v0.3 |
| v0.5 | Identità, abbinamento, sincronizzazione cifrata del registro tra pari, permessi per cella, conflitti | v0.4 |
| v0.6 | `engramma serve`: API, autenticazione, molte celle | v0.5 |
| v0.7 | Cluster: posizionamento Raft, router, replica multi-Raft, spostamento celle, backup su storage a oggetti | v0.6 |
| poi | Python, Swift, Kotlin, WASM; MLS per i gruppi; messaggi A2A sul canale | — |

## 13. v0.1 nel dettaglio (sarà il primo prompt per GLM)

**Repository nuovo** (`~/Projects/Bot/engramma`, nome da confermare), licenza **Apache-2.0**, Rust stabile, workspace come al § 8.1, con solo `engramma-core`, `engramma-cli` e `engramma-mcp`.

1. **Formato** (§ 3):
   - parser e serializzatore;
   - validazione con errori leggibili;
   - `spec/format-v0.md` con esempi validi e non validi usati come test;
   - collegamenti, relazioni tipizzate e tag dal corpo.
2. **Cella** (§ 4):
   - schema e migrazioni versionate;
   - ogni scrittura come operazione nella stessa transazione;
   - HLC, ULID, hash concatenato;
   - `note_versions`, lapidi, `supersedes` che chiude la validità.
3. **Testo** (§ 6.1): `body_norm` con le radici (`rust-stemmers`) e rilevamento della lingua (`whatlang`).
4. **Ricerca v0.1:**
   - BM25 + allargamento nel grafo + filtri (tipo, tag con figli, `as_of`, affidabilità, quarantena, sostituite) + riordino per affidabilità e recenza;
   - spiegazioni;
   - `neighbors`, `path`, `tags`, `entities`, `timeline`, `history`, `changes_since`.
5. **Import/export:**
   - cartella markdown ↔ cella, senza perdite;
   - import di una cartella Obsidian qualsiasi (le note senza intestazione ricevono `id`, `type: note` e `source: import:obsidian`).
6. **CLI `engramma`:** tutti i comandi del § 8.1 tranne `serve`, con uscita leggibile e `--json`.
7. **`engramma mcp`:** stdio, in sola lettura, `--cells`, i 5 strumenti del § 8.3, con descrizioni chiare per i modelli.
8. **Test:**
   - unitari;
   - **di proprietà** (`proptest`): giro del formato, registro rigiocato = stesso stato, export → import = stesso stato;
   - **dorati**, su una cartella di esempio in italiano: «Fabio, Giulia, trasloco a Milano, lavoro, preferenze che cambiano», circa 60 note;
   - `engramma verify`.
9. **Benchmark** in `bench/`:
   - generatore sintetico da 10 000 e 100 000 note;
   - domande con la nota attesa;
   - richiamo@5 / MRR e latenze p50 e p95, stampate in un rapporto.
10. **Integrazione continua:**
    - compilazione e test su macOS (arm64, x86_64), Linux e Windows;
    - `cargo deny` per le licenze: solo licenze permissive;
    - `cargo clippy -D warnings`;
    - nessuna rete nei test.

**Criteri di chiusura:**
- tutti i test verdi sulle tre piattaforme;
- benchmark eseguito, con i numeri nel rapporto;
- un utente può fare `engramma init`, `import` di una cartella Obsidian, `search` e `mcp`, e collegarlo a Claude Desktop per leggere la sua memoria.

## 14. Decisioni aperte

1. ~~Il nome~~ → **Engramma** (deciso 2026-11-05; il pacchetto npm sarà `@engramma/core`).
2. **Campi in inglese** nel formato: adottati per la v0.1 (raccomandazione; l'utente può rivederla).
3. **Dove vive:** repository privato finché non c'è la v0.1, poi pubblico su GitHub?
4. **Il modello di embedding predefinito:** si decide in v0.2 misurando Granite R2 97M contro 311M. Runtime (tract) e indice (`FlatI8`, niente sqlite-vec per ora) sono decisi il 2026-11-05.
5. **SQLCipher sì o no** per la cifratura della cella (v0.5).
6. **Il rapporto con Homun:** il motore resta un progetto separato, con le sue versioni. Homun fissa una versione precisa.

## Fonti (verificate il 2026-11-05)
- Licenza SurrealDB: https://surrealdb.com/license
- Kùzu archiviato: https://biggo.com/news/202510130126_KuzuDB-embedded-graph-database-archived · CozoDB fermo: https://github.com/cozodb/cozo/discussions/299
- Turso (stato beta): https://www.theregister.com/databases/2026/07/29/after-rewriting-sqlite-in-rust-turso-turns-its-sights-on-postgres/5279835 · https://explainx.ai/blog/turso-database-sqlite-rust-rewrite-guide-2026
- Benchmark della memoria e i loro limiti: https://mem0.ai/library/agent-memory/ai-memory-benchmarks-in-2026
- Fonte e avvelenamento della memoria: https://arxiv.org/pdf/2605.30771 · https://arxiv.org/pdf/2606.22030
- Memoria temporale: https://arxiv.org/pdf/2601.07468
- A2A e i suoi limiti di sicurezza: https://arxiv.org/pdf/2609.10871 · https://arxiv.org/pdf/2606.31498
- EmbeddingGemma: https://developers.googleblog.com/en/introducing-embeddinggemma/ · Granite Embedding R2 (ONNX, terza parte): https://huggingface.co/Hisham480/multilingual-r2-ONNX
- Memoria portabile: https://github.com/MacPaw/portable-memory · https://portable-ai-memory.org/spec/v1.0/
- (2026-11-05, v0.2) Rilasci di sqlite-vec (0.1.9 stabile, DiskANN/IVF solo in 0.1.10-alpha): https://github.com/asg017/sqlite-vec/releases · Granite Embedding Multilingual R2: https://huggingface.co/blog/ibm-granite/granite-embedding-multilingual-r2 · Fusione convessa contro RRF (ACM TOIS 2023): https://dl.acm.org/doi/10.1145/3596512 · Le misure di tract, sqlite-vec e scansione int8 sono nostre (prototipi nel commit 90a106e e nel prompt v0.2).
- (2026-11-06, v0.3) HippoRAG 2 e PPR per la memoria: https://github.com/osu-nlp-group/hipporag · Hub nei grafi di memoria (GAAMA): https://arxiv.org/pdf/2603.27910 · MemGraphRAG: https://arxiv.org/pdf/2606.00610 · Payload dimenticabili e crypto-shredding: https://verraes.net/2019/05/eventsourcing-patterns-forgettable-payloads/ · https://verraes.net/2019/05/eventsourcing-patterns-throw-away-the-key/ · Digest salati (SD-JWT, RFC 9901): https://rfc-editor.org/rfc/rfc9901 · Il prototipo PPR e le misure di latenza a 100k sono nostri (prompt v0.3, allegato `ppr`).
