# Motore di memoria — documento di progetto

> **Nome: Engramma** (scelto dall'utente il 2026-11-05: l'engramma è la traccia fisica di un ricordo; libero su crates.io, 14 progetti omonimi su GitHub). **Stato al tag `fase-v0.4.5`** (bozza 1 del 2026-11-05, aggiornata alle fasi successive senza riscrittura).

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
- **Compattazione** (v0.3; **rivista il 2026-11-07, v0.3.2**):
  - i payload delle operazioni fino a un `seq` si dimenticano, mentre gli scheletri con gli hash restano: la catena è intera;
  - **di base la compattazione registra un punto di controllo senza file**: testa, digest dello stato logico alla testa e `redacted_by`. La cella SQLite è già lo stato durevole; una seconda copia (l'istantanea di v0.3/v0.3.1) serviva solo all'audit completo e valeva quasi tutta la cella (a 100k: 190 MB di cella + 179 di istantanea = +50%). Come rqlite, che usa il file SQLite stesso come istantanea;
  - l'istantanea resta **facoltativa** (`--keep-snapshot`): il comportamento v0.3.1, cioè il file dello stato logico (payload fino al taglio, vettori, registro modelli, legami della vista e contenuto FTS tolti), una sola per cella, purgata insieme alla cella;
  - resta sempre **una sola riga**: l'ultima compattazione sostituisce la precedente, con o senza file; il file vecchio si cancella **prima** della sua riga (una riga senza file sfuggirebbe a ogni purge successiva), e un fallimento della cancellazione è un errore della compattazione;
  - `verify` dice cosa ha potuto controllare (`replay`): `Full` (nessuna compattazione), `FromSnapshot` (istantanea: rigioca la coda) o `ChainOnly` (punto di controllo senza file: catena intera degli hash, ogni payload superstite col suo digest, coerenza degli indici derivati — archi riderivati dalle note, nessun vettore oltre la propria nota — e digest dello stato finché la testa non avanza dal punto di controllo);
  - `purge` su un punto di controllo ne azzera il digest (descriveva uno stato che conteneva la nota purgata) e aggiunge il proprio `op_id` a `redacted_by`; sull'istantanea con file vale il v0.3.1 (scrub del file, digest aggiornato);
  - `changes_since` sotto il taglio risponde `Compacted`: la replica parte dall'istantanea, o non può partire senza (§ 9).

### 4.4 Versioni e cancellazione
- Ogni modifica crea una riga in `note_versions`: lo storico è consultabile e ripristinabile.
- `delete_note` lascia una **lapide** (`status: deleted`), che si propaga alle repliche. La cancellazione definitiva (diritto all'oblio) è un'operazione esplicita, `purge`, che cancella anche le versioni e propaga la lapide.
- `purge` (v0.3) **dimentica i payload** di tutte le operazioni della nota (registro v2, § 4.3). Toglie anche vettori, righe FTS, archi dichiarati ed entità rimaste orfane, e il file della vista markdown. Il suo payload contiene solo l'id e un motivo. Le menzioni `[[Nome]]` in altre note restano: sono altre note. Le esportazioni già fatte sono fuori dal controllo del motore.
- **L'oblio è esatto (v0.6.7).** Una riga che esce da un indice FTS5 lascia le radici nei segmenti vecchi finché un `optimize` non li fonde: per questo la purga fonde. L'indice nascosto porta un **bit «sporco»** persistente (una riga di metadati, bookkeeping locale come `sync_shared`): ogni `delete` da quell'indice lo accende, qualunque sia la causa d'uscita — rilascio dalla quarantena, modifica, chiusura di una copia, purga — e la purga fonde quando il bit è acceso **o** la famiglia ha righe, spegnendo il bit insieme alla fusione. Vale per la purga locale, per quella in arrivo e per lo scrub dell'istantanea; i `delete-all` della compattazione e del rebuild spengono il bit perché lì i segmenti cadono davvero.
- Lo scrub di un'**istantanea** applica la stessa chiusura transitiva della purga viva: le copie di conflitto, le loro righe di derivazione, le righe FTS dei due indici, la coda delle proposte, gli archi — e la purga **in arrivo** fa lo stesso sulle istantanee del dispositivo che la riceve (il diritto all'oblio attraversa i dispositivi). **`scrubbed` si guadagna, non si dichiara**: dopo lo scrub la purga rilegge l'istantanea e conta le righe della famiglia rimaste; se ne trova una, `scrubbed` è `false` e `pending` nomina il file. **(v0.6.8, H2) Il taglio conserva ciò che il suo rigioco serve**: prima che i payload muoiano, i corpi delle copie di conflitto il cui perdente sta a o sotto il taglio traslocano nella loro riga di derivazione (`conflict_copies.state_json`, stato derivato, fuori da ogni digest) — nel vivo, nell'istantanea e nel rigioco di `verify`, che così copre anche i file scritti prima della regola senza toccarli. **Dichiarato aperto**: l'ANCORA della rimaterializzazione — lo stato-pre da cui un rigioco da istantanea riparte — invecchia col taglio contro cui fu scritta; sulle note i cui arrivi fuori ordine attraversano il taglio, `version` e i campi d'intestazione possono divergere fra vivo e ricostruzione (segni nel REPORT della v0.6.8). **(v0.6.8, H3) `scrubbed` si guadagna su TUTTI E DUE i file, e l'orfana si conta per rowid**: una sola funzione (`holds_family`) conta le righe della famiglia nel file vivo e in ogni istantanea — le righe `notes` e `conflict_copies`, le righe FTS contate PER ROWID (i rowid si leggono PRIMA dello scrub, in tutti e due gli indici: un mezzo-scrub che toglie la riga di `notes` e lascia quella dell'indice è il più probabile, e passa inosservato a chi conta attraverso la JOIN), le righe `docsize` orfane — senza la loro nota, di qualunque famiglia — e le righe della coda. Ogni risposta diversa da zero, `-1` compreso (il file che non si apre in lettura), rende `scrubbed: false` e `pending` nomina il file; un file illeggibile non viene nemmeno scrubbbato mezzo cieco. Il controllo sui byte resta compito dei test.

### 4.5 Lo strato di accesso ai dati
Il trait `CellStore` isola SQL, FTS e vettori: un'implementazione SQLite oggi, Turso o altro domani. Il resto del motore non contiene SQL.

### 4.3.1 La convergenza del registro (v0.6)

Lo stato di una copia è funzione dell'**insieme** delle operazioni che possiede, mai dell'ordine di arrivo (§ 4). Le regole:

- **L'identità in rete.** `content_hash` = blake3 del contenuto canonico `{op_id, hlc, actor, kind, note_id, base, payload_digest}` (più `base` quando vive). La stessa operazione ha la stessa identità su ogni copia; `prev_hash`/`hash` restano la **catena locale d'arrivo** di quel disco. Il payload digest già impegna corpo e sale: l'identità è stabile sotto redazione.
- **L'ordine totale.** `(hlc.physical_ms, hlc.counter, hlc.node, op_id)`. La parte fisica e il contatore sono a larghezza fissa; il nodo no (12 esadecimali prima dell'era firmata, l'id del dispositivo — 64 esadecimali — da quando il firmatario è armato, vedi sotto), e questo non rompe l'ordine: il confronto SQL `ORDER BY hlc, op_id` è binario su stringhe e il confronto Rust su `Hlc` è binario sugli stessi campi nello stesso ordine, quindi i due percorsi percorrono lo stesso ordine qualunque sia la larghezza del nodo.
- **La materializzazione per oggetto.** Un'operazione che nell'ordine totale cade **prima** dell'ultima già applicata al suo oggetto lo fa **rimaterializzare**: le righe di stato vanno, lo stato al taglio di compattazione torna dall'istantaneo di versione del primo arrivo sopra il taglio, e tutte le operazioni dell'oggetto si rigiocano in ordine totale, ciascuna nella metà che lo riguarda (un `supersede` vale per la nota nuova e per la vecchia; una `decide` per la proposta e per il bersaglio; la riga `supersedes` la scrivono entrambe le metà, `INSERT OR IGNORE`). Chi arriva ultimo applica diretto: il caso comune non costa di più. Un'operazione su un oggetto che non c'è ancora si **conserva** in attesa (`pending_objects`, tavola locale) e materializza quando arriva la put.
- **`base`: chi ha visto cosa.** Ogni operazione che modifica una nota esistente porta `base`, il `content_hash` dell'operazione il cui corpo l'autore aveva davanti. Con le copie durevoli quella non è «l'ultima della nota»: il perdente resta applicato (vive come copia) mentre la nota mostra il vincitore, e una patch d'intestazione scritta sopra il perdente è più nuova senza averlo mai visto. La `base` di una scrittura locale è dunque la **testa visibile** (v0.6.5, E1): l'operazione più recente nell'ordine totale che **ha il vincitore del corpo fra i suoi antenati, o è il vincitore stesso** — le patch d'intestazione sopra il vincitore comprese, i rami perdenti esclusi. La testa è **derivata** (la tabella `note_heads`, schema 22): la scrittura locale la legge con una sola sonda indicizzata. Per una nota chiusa da una lapide la `base` resta la **linea chiusa**: la lapide, o chi dopo di lei la lasciò chiusa; la riapertura («solo se la sua `base` è la lapide stessa») giudica la stessa linea, e una nota senza scritture del corpo applicate tiene la regola di ieri. Due operazioni con la stessa `base` sono **concorrenti**. Prima della v0.6 non c'è: sequenziali per dichiarazione.
- **Le regole dei conflitti del corpo (v0.6.4, D1).** Intestazione (titolo, tag, validità, tipo, confidenza, stato): l'ultima scrittura per campo nell'ordine totale — una patch di sola intestazione **non è mai** una scrittura del corpo e non produce conflitti. Le definizioni, tutte dall'**insieme** delle operazioni della nota:
  - **Scrittura del corpo:** un `put` della nota, o una `patch` il cui `set` contiene `body`.
  - **Ascendenza:** la catena delle `base`, attraverso ogni operazione della nota (intestazione compresa). Due scritture del corpo sono **concorrenti** se nessuna è antenata dell'altra. Una scrittura senza `base` è una radice: il doppio importo della v0.6.1 non aveva visto la nota.
  - **Vincitore:** fra le punte concorrenti (le scritture del corpo applicate senza discendenti corporei), la chiave **(classe dell'autore, hlc, op_id)** — la mano umana vince anche più vecchia (D2).
  - **Perdente:** ogni scrittura del corpo applicata che non è il vincitore né un suo antenato. Il corpo della nota è quello del vincitore, anche quando un'altra scrittura gli è passata sopra nell'ordine d'arrivo; un `restore` più recente del vincitore tiene il suo (l'ultima mano che imposta il corpo).
  - **La copia è durevole:** nasce per ogni perdente il cui corpo differisca da quello del vincitore e dalla biforcazione, e **si chiude solo con un testimone nell'insieme**: (a) un'operazione sull'id della copia (una modifica, il `delete` che la risolve, una decisione sulla sua proposta); (b) una scrittura del corpo applicata successiva al perdente nell'ordine totale che **lo contiene** — il diff3 rigoroso a righe `diff3(F, b, perdente)` restituisce b senza sovrapposizioni, con **F = nca(perdente, b)**, l'antenato comune dei due, estremi compresi (se il perdente è antenato di b, F è il perdente stesso: così il discendente è il caso particolare del contenimento) — o che **discende dal perdente** (la sua catena di `base` passa per lui: lo ha visto). Due perdenti con lo stesso corpo danno una copia sola (il secondo testo contiene il primo: il diff3 di due cambi identici è pulito). **Una copia chiusa con un testimone non si riapre mai: il testimone è un'esistenza** e non dipende dal vincitore — e dalla v0.6.7 non dipende nemmeno dalla lapide: una scrittura che contiene il perdente, o ne discende, è un testimone anche se poi una lapide la batte. La lapide cancella la nota intera, testo del perdente compreso, ed è la scelta dell'utente; quello che il testimone attesta («qualcuno ha visto il perdente») resta vero, e un insieme può solo crescere verso l'avere un testimone, mai uscirne (il seme `3753902730771729170` della caccia v0.6.6, ridotto a mano, è il test: la copia chiusa non rinasce mentre la nota resta nel cestino). Finché l'originale tiene una lapide attiva, **le sue copie seguono la lapide**: non stanno in `conflicts()`, non rispondono a nessuna ricerca (nemmeno alla vista ampia del proprietario) e la riga «conflitto» della coda resta solo nascunta — è il comportamento di un cestino. `conflict(copyId)` per id continua a rispondere, col campo `originalDeleted: true`: una lettura per id è onesta. Quando la nota si riapre (una modifica con `base` uguale alla lapide), le copie aperte tornano visibili con le regole di sempre; quelle chiuse restano chiuse. Le sole chiusure e riaperture che seguono il vincitore, dichiarate: i filtri della nascita (corpo uguale a quello del vincitore, corpo uguale alla biforcazione col vincitore), la perdita della condizione di perdente quando un grafo completato da un arrivo fuori ordine sposta il vincitore (il testo vive nella nota), la purga e la lapide con le loro regole proprie. La monotonia è guardata dalla proprietà della flotta `no_copy_closes_without_a_witness` (v0.6.6, F1: presente→assente esige un testimone o una dominanza, assente→presente solo una rinascita non meritata), accanto al giudice del testo `no_body_text_is_lost` — le due non si sostituiscono: una guarda il testo, l'altra la storia delle copie. **Il cestino ha un predicato solo (v0.6.8, H1)**: una nota sta nel cestino quando è cancellata, oppure quando è copia di conflitto di una nota cancellata — una definizione, in un posto solo (la vista SQL `notes_in_bin`, creata dallo schema e parlata da ogni percorso di lettura: `note_ids`/`list`, `get` — la copia risponde come una cancellata —, le due ricerche e il braccio vettoriale, i vicini, i percorsi e il grafo, i tag, le entità, la linea del tempo, `conflicts`, la coda, il consolidamento, l'export e la vista markdown, il server MCP). La forma è la VISTA, non una colonna derivata: una vista non può discostarsi dalle tabelle che legge, non costa nulla sulla strada di scrittura e non tocca il digest dello stato — l'ALPN resta `engramma/sync/4`. Il contratto è guardato dal test `cestino.rs`, che passa OGNI lettura pubblica che parla note (l'elenco in un posto solo), e dal cancello `every_public_read_is_classified`, che pretende ogni `pub fn` di `Cell` in un bucket: la prossima porta dimenticata cade da sola. **L'oracolo della monotonia è un riferimento (v0.6.8, H4)**: il giudice della chiusura non usa più il motore (`witness_for_test` chiamava `has_witness` — lo stesso insieme, lo stesso errore), ma il testimone di riferimento di `tests/common/`, la regola di questa sezione riscritta da capo — nessuna funzione dei conflitti del motore, ingenuo e quadratico, con un diff3 suo (LCS per programmazione dinamica; la crate `diffy` è stata provata e scartata: il suo merge risponde «conflitto» ai cambi identici sovrapposti, che il diff3 rigoroso fonde). Motore e riferimento devono essere d'accordo: ogni disaccordo fa cadere la proprietà col caso. Ogni proprietà proptest della suite sta nell'inventario `crates/engramma-core/tests/properties.txt`, controllato dalla macchina col test `properties_inventory` (v0.6.6, F2): una proprietà che sparisce dai sorgenti fa fallire la suite, non più l'occhio del revisore.
  - **Il tetto del diff3 (v0.6.5, E3):** l'allineamento è Myers, O((n+m)·D) in **spazio lineare** (sotto le 512 righe per lato resta la passeggiata della tabella v0.6.4, ora limitata a ~1 MB per foglia); oltre **32 768 righe o 4 MiB per lato** il contenimento risponde «non contiene» — la scelta prudente: la copia resta aperta e il testo non si perde mai. Sotto il tetto una fusione a 20 000 righe costa millisecondi (i numeri nel REPORT).
  - **Quarantena dell'agente (D2):** un perdente la cui classe è agente non è una nota attiva: la copia nasce **in quarantena**, invisibile alla ricerca, nella coda delle proposte con motivo «conflitto» (riga derivata come la copia, senza operazioni). Un perdente umano produce la copia attiva col tag `conflitto`, come sempre.
  - **Tetto:** al più **3** copie aperte visibili per nota, le più recenti nell'ordine totale; le altre restano aperte e appaiono quando una visibile si chiude. `conflicts()` elenca tutte le aperte; `resolveConflict(copyId, {keep, body?})` risolve con operazioni normali (`original` = il delete sulla copia; `copy` e `merged` = una patch dell'originale poi il delete).
  - La copia: id deterministico `blake3("engramma/conflict/v1" ‖ content_hash del perdente)` in forma ULID con la parte temporale dell'HLC del perdente, tag `conflitto`, arco verso l'originale — **stato derivato**, e la riga di derivazione (`conflict_copies`, schema 21) conserva tutto fuorché il **corpo**: il payload del perdente è la sua casa (misurato in D5: il corpo due volte era un quarto del costo). Le correzioni dell'utente sulla copia sono operazioni normali sul suo id, e la riga resta come base del suo replay.
- **La classe dell'autore (D2), dichiarata e non firmata:** umano è `user`, ogni actor che finisce in `:user` (`homun:user`), ogni actor che comincia con `user@` (`user@mcp:<etichetta>`); tutto il resto è agente, compresi gli actor sconosciuti — chi non si riconosce non batte una mano umana. L'`actor` sta nell'identità dell'operazione, quindi ogni copia della cella legge la stessa classe. Nella sync personale basta: tutti gli scrittori vivono sui dispositivi del proprietario. Nelle organizzazioni (v0.7) la classe dovrà legarsi a una firma.
- **La versione delle regole nel protocollo (D4):** l'ALPN `engramma/sync/4` (e `engramma/mailbox/4` per la casella) È la versione delle regole di materializzazione — un pari con regole diverse deriva stati diversi dallo stesso insieme, quindi non deve scambiare nemmeno un'operazione. Il rifiuto è dell'handshake: `PEER_OUTDATED`, col nome del dispositivo da aggiornare. Ogni cambio della derivazione alza la versione: la v0.6.5 porta la `3` (testa visibile, testimone per coppia, lapide per linea chiusa), la v0.6.7 la `4` (il testimone non dipende dalla lapide, le copie seguono la lapide, la purga domina anche nel rigioco).
- `decide` concorrenti sulla stessa proposta: vale la **prima** nell'ordine totale; le altre restano nel registro come «decisione superata» (`decisions`), senza effetto; `revert_decision` segue le stesse regole. `delete` contro modifica concorrente: vince la lapide se è l'ultima nell'ordine; una modifica successiva alla lapide la riapre **solo se la sua `base` è la lapide stessa**; una scrittura battuta dalla lapide, o cancellata dal suo rollback, non produce copia e non torna «vincitore»: la regola della lapide resta quella di oggi, e domina anche sulla durabilità — ma non ritira i testimoni (sopra). **La `purge` domina sempre**: quando l'insieme contiene una purge della nota X, ogni operazione di X si conserva **redatta**, X non materializza — qualunque sia l'ordine totale fra la purge e le scritture di X: una `put` che arriva dopo una purge già nell'insieme non la ricrea (v0.6.7: la dominanza vale anche nel rigioco, dove i payload sono caricati prima che il `scrub_log` della purge li rediga) — e le sue copie di conflitto vanno con lei; anche gli archi che la nominano come estremo.
- **La frontiera stabile e la compattazione.** In una cella condivisa si compatta solo sotto la **frontiera stabile** — la testa più profonda che **tutti** i dispositivi vivi hanno confermato (`device_acks`, una riga per sessione); sopra la frontiera i payload restano. Un dispositivo nuovo parte da un'**istantanea logica** alla frontiera, poi riceve le operazioni sopra.
- **`adopt`.** Le operazioni scritte prima della v0.6 non hanno firma. Chi condivide firma un'operazione `adopt` con la radice di Merkle dei `content_hash` di tutto ciò che esiste: un'operazione senza firma è accettata solo coperta da un `adopt` valido.

#### L'identità non cambia mai (v0.6.1)

La verifica della v0.6 ha trovato la convergenza rotta, e la causa è una
sola idea mancante: **l'identità di un'operazione si calcola una volta,
alla nascita, e non cambia più.** È la stessa idea del registro v2 — la
catena copre un *digest* del corpo, non il corpo, così la redazione non
rompe la catena — estesa all'identità di rete.

- **`payload_state`** (`whole` | `redacted` | `censored`, schema v20):
  la censura della purge sostituisce il *corpo* (sale deterministico,
  campi tenuti deterministici) e segna la riga; `payload_digest`,
  `content_hash` e `sig` restano quelli di nascita, per sempre. Nella
  v0.6 il censore ricalcolava digest e identità: una copia che aveva
  visto il supersede prima della purge conosceva un hash, l'altra un
  altro, la stessa `base` risolveva su una copia sola — e la convergenza
  dipendeva dall'ordine d'arrivo. La catena locale continua a coprire il
  digest (che non è cambiato): nessun rehash, mai.
- **`verify` per stato.** `whole`: il digest torna col corpo. `censored`:
  il sale è esattamente `blake3("censor:" ‖ op_id)` e il corpo ha la
  forma esatta della regola della censura (per `supersede`:
  `{old_id?, valid_to?, old_updated_at?, note: null}`; per `decide`
  accetta: `{proposal, decision: "accept", censored: true, target,
  target_closing?, decided_at?}`; per `revert_decision`: `{censored:
  true, decision?, decided_at?, restore_target?}`) — e le stringhe
  tenute sono ciò che dicono di essere (v0.6.1, R6: gli id `old_id`,
  `proposal`, `target` sono ULID; le chiusure `valid_to`,
  `target_closing` parsano come `Validity`; gli istanti
  `old_updated_at`, `decided_at` come `At` RFC 3339) — l'identità non
  copre più il corpo, la forma e il contenuto delle stringhe sono ciò
  che lo limita. `redacted`: nessun corpo, nulla da controllare.
- **`base` assente = non ancora arrivata.** Con l'identità immutabile,
  una `base` che non è da nessuna parte nel log significa una cosa
  sola: l'operazione aspetta in `pending_objects` — come una patch
  senza put — e applica quando la base arriva. L'attesa lega solo
  l'oggetto a cui la base appartiene (`op.note_id`): le altre metà di
  un'operazione a due oggetti (la nota nuova di un supersede, il
  bersaglio di una decide) materializzano subito. «Assente = sequenziale»
  sopravvive solo per le operazioni **sotto la frontiera**, dove la testa
  si cerca su tutto il log: gli scheletri sotto il taglio portano hlc e
  hash, che è tutto ciò che l'ordine chiede.
- **Il cammino rapido riconcilia.** Una body write che arriva in
  sequenza è il vincitore finale: i perdenti diventano zero e le copie
  stantie di un vincitore superato vanno via. Senza questo, il
  riferimento in ordine totale teneva copie fantasma che le copie
  rimaterializzate non avevano. E una body write **senza base** su una
  nota che già vive è concorrente per costruzione (lo stesso import su
  due macchine): le regole dei conflitti corrono anche lì — trovato dal
  test a tre dispositivi.
- **La copia di conflitto è funzione dell'insieme, anche per chi
  scrive** (v0.6.3). L'insieme dei perdenti di una nota — e quindi le
  sue copie — si ricalcola a OGNI scrittura del corpo che la tocca,
  **scrittura locale compresa**: una scrittura locale è l'ultima del
  suo oggetto e la sua base è la testa del registro, quindi i suoi
  perdenti sono vuoti e ogni copia lasciata da un insieme più piccolo
  va via subito, anche sul dispositivo che scrive. Prima il percorso di
  scrittura locale non riconciliava mai: una copia scritta da una
  ricostruzione intermedia (l'arrivo fuori ordine che l'aveva resa
  perdente) sopravviveva per sempre sullo scrittore mentre ogni copia
  ricevente la derivava via — i due testimoni della verifica v0.6.2.
  L'**oracolo differenziale** controlla la regola:
  `rematerialize_for_test` ricostruisce tutto lo stato replicato dal
  **solo registro** (una materializzazione per oggetto, fino al punto
  fisso) e la proprietà `every_copy_equals_its_reconstruction`
  pretende che lo stato incrementale di ogni copia sia uguale alla
  propria ricostruzione, qualunque sia l'ordine d'arrivo.
- **L'`adopt` con la prova.** La radice di Merkle copre l'era senza
  firma; l'operazione senza firma viaggia con la **prova** che la sua
  foglia cammina fino a quella radice (`merkle_prove`/
  `merkle_root_with`: coppie in ordine, la foglia dispari raddoppiata a
  destra). Il ricevente — che può non tenere ancora nessun'altra foglia
  — verifica la prova contro la radice registrata dall'`adopt`.
- **L'insieme accettato di un rimosso è la radice, non l'orologio.** La
  `remove` porta la radice di Merkle dell'insieme delle operazioni del
  rimosso che chi rimuoveva aveva accettato (`accepted_root`,
  `accepted_count`, calcolata sull'intero albero delle celle condivise di
  quella radice). Un'operazione firmata da un rimosso entra **solo** se:
  è già nella copia (un duplicato), o arriva con la **prova di
  appartenenza** a una delle radici registrate (`merkle_root_with`, la
  stessa meccanica dell'`adopt`). L'HLC non conta per questa decisione:
  lo sceglie l'autore, e retrodatare era la porta aperta della chiave
  rubata. Chi **manda** un'operazione di un rimosso allega la prova,
  costruita sul proprio insieme quando la sua radice è una delle radici
  registrate; una copia che non può costruirla non manda l'operazione e
  lo segnala come evento.
- **Più `remove` dello stesso dispositivo** (concorrenti, da membri
  diversi): vale l'**unione** degli insiemi — una prova verso una
  qualunque delle radici registrate ammette l'operazione.
- **L'autore entra nell'identità.** Quando la cella ha il firmatario
  armato (`set_signer`), il nodo dell'HLC delle operazioni **locali** è
  l'id del dispositivo (l'orologio tiene la parte fisica e il contatore;
  cambia solo il nodo). Al cancello, ogni operazione firmata deve avere
  `device == hlc.node` — entrambi dentro il `content_hash` che la firma
  copre — e una discordanza è `BadSignature`: una stessa identità non può
  avere autori diversi su copie diverse. Le operazioni scritte prima
  dell'era firmata restano senza firma e passano dall'`adopt`.
- **Il filo.** Un flusso **unidirezionale** per frame, con `finish` e
  con il FIN consumato dal ricevente; e il congedo esplicito (`bye` +
  `bye-ack`, chi finisce tiene la connessione finché l'altro non
  chiude): una connessione abbandonata resetta i dati in volo, e
  l'ultimo frame spariva — prova nel test lockstep a venti frame.

## 5. Affidabilità, quarantena, fonte

- **Ogni scrittura richiede `source` e `confidence`**, altrimenti viene rifiutata.
- `confidence: external` → `status: quarantined`, cioè:
  - escluse dalla ricerca predefinita;
  - visibili con `include_quarantined`;
  - promosse ad `active` solo con `confirm(note_id, actor)`, scritto nel registro.
- **«Perché lo so?»:** ogni risultato porta la sua `source`. L'applicazione la trasforma in un collegamento, per esempio la conversazione di Homun.
- **Fatti che cambiano.** Una nota che sostituisce un'altra (`supersedes`) le mette `valid_to` e la marca `superseded`. Restano nello storico e rispondono alle domande «com'era prima».
- **Le proposte (v0.5).** Accanto alla quarantena dei contenuti `external` c'è la **coda delle proposte**: una nota che un assistente propone nasce in quarantena — la ricerca non la vede, il grafo non la semina — con i suoi metadati (chi propone, client dichiarato, modo `new`/`supersede`, motivazione). Le due cose sono distinte: `external` è quarantena di *contenuto* (terze parti), la coda è quarantena di *autore* (un modello). Se ne decide in § 8.5: politica per cella, revisore automatico, decisioni applicate nell'operazione che le registra, annullamento che non riscrive mai la storia.
- **L'invariante d'indistinguibilità (v0.5.3).** La visibilità di stato entra **dentro** ogni lista, prima di ranghi e normalizzazioni: la query BM25 porta il filtro di stato nel JOIN (gli stessi stati della lettura), i ranghi vettoriali si numerano dopo il filtro, il grafo espelle le note nascoste dall'uscita del push prima della finestra e della normalizzazione, e le frequenze/totali contano i soli visibili. Ne segue la proprietà che il codice mantiene come invariante (proptest dedicato, tre modalità): **la stessa domanda dà gli stessi risultati — stessi id, ranghi densi da 1, massimo normalizzato a 1, stessi coseni, supporti e percorsi — con o senza note in quarantena e lapidi che corrispondono alla domanda**. **(v0.6, P0) La proprietà è ora esatta, bit per bit:** le note in quarantena vivono in un indice FTS **separato** (`notes_fts_hidden`), quindi l'IDF, le frequenze dei documenti, i ranghi e le normalizzazioni della vista predefinita non le vedono affatto — l'eccezione dell'ε dell'IDF è sparita, e il proptest la chiede uguale anche nei punteggi (stesso `score`, `raw_score`, `fused_score`, coseni), con il caso «Zebedeo tonno» del revisore come test a sé. La vista del proprietario (`include_quarantined`) interroga entrambi gli indici e li fonde **per rango** (RRF con k=60 nel ramo testo, pari merito spezzato per id): i due bm25 sono numeri incommensurabili, e l'alternativa — un indice unico — è esattamente ciò che questa fase ha tolto. Sopra la stessa impronta non è un segreto: la proprietà riguarda il controfattuale remoto, non la vista del proprietario. Su HTTP, `explain` non è più un oracolo sulle note nascoste: i ranghi non partono da 2, il `fused_score` del primo è 1, nessun visibile è spinto fuori dalla finestra.

## 6. La ricerca mista

### 6.1 Testo
- FTS5 (BM25) su `title`, `body_norm` e `tags`.
- `body_norm` è il testo minuscolo, senza accenti, **con le parole ridotte alla radice** nella lingua della nota (`rust-stemmers`, Snowball: italiano, inglese e altre). Il tokenizzatore predefinito di FTS5 non sa l'italiano. La lingua si rileva (`whatlang`) o si dichiara.
- La stessa normalizzazione si applica alla domanda — ma **la lingua di una domanda corta non si indovina** (v0.5.3): «Zebedeo» era tedesco e «tonno» francese per `whatlang`, e la radice della domanda non coincideva con quella dell'indice (dalla v0.1). Una domanda è **incerta** con meno di 4 parole significative (stopword escluse) o quando `whatlang` dichiara la rilevazione inaffidabile (`is_reliable`); le note continuano a usare la rilevazione piena: hanno testo a sufficienza e la loro lingua vince comunque (l'indice è stato stemmato con quella). Su una domanda incerta ogni parola si cerca nella **forma intera** (che l'indice contiene quando la nota non ha lingua) più la radice in **ogni lingua presente nella cella** — l'istogramma delle lingue delle note visibili, in cache in memoria come il grafo e invalidato dalle scritture; senza lingue note, le sei di sempre. `q.lang` esplicito vince sempre; le coppie adiacenti (frasi) si fanno sulle radici della lingua più frequente. Misurato: il banco delle domande corte (`tests/golden/queries-short.json`, 48 domande da 1–3 parole sul vault dorato) passa da richiamo@5 0,979 a **1,000** (prima mancava «Esposito», indovinato portoghese); banco italiano e LongMemEval entro le soglie della fase (REPORT v0.5.3).

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
3. **Grafo come terza lista** (*rivisto il 2026-11-06, v0.3; semi e peso per domanda il 2026-11-08, v0.4*): un **PageRank personalizzato a spinta** (Andersen–Chung–Lang) corre su un grafo note↔entità **in memoria** (`GraphIndex`: CSR derivato, costruito pigramente, aggiornato dalle scritture) e i suoi punteggi entrano nella fusione come terza lista.
   - Era v0.2: un allargamento a passi fissi (0,6 / 0,36), con gli archi letti da SQLite a ogni domanda. Motivo, misurato a 100k: il grafo prendeva il 59% del tempo (p95 ibrido 59,8 ms) e in ibrido il multi-salto crollava a 0,063. La spinta: con ε = 10⁻⁴ tocca ~157 archi per domanda anche con hub da 16 741 archi, e trova la nota a due salti prima in 200 domande su 200. È la linea di HippoRAG 2.
   - **Semi dalla domanda (v0.4):** il vettore di partenza sono le **entità che la domanda nomina** — gruppi di 1–3 parole consecutive, normalizzati come l'indice (minuscole, senza accenti, niente stemming: i nomi propri non si stemmano), corrispondenza esatta contro `entities.name` e gli `aliases` delle note-entità (`GraphIndex` costruisce la mappa con l'indice e la aggiusta con le scritture) — più i candidati della fusione con punteggio normalizzato × `seed_passage_weight` (0,05, come HippoRAG 2). **Porta:** la domanda che non nomina nessuna entità del grafo non sveglia il grafo; l'explain dice «grafo spento: nessuna entità nominata».
   - **Peso per domanda (v0.4):** il peso della lista grafo non è più una costante ma `w_g(q) = w_max · gate(q) · incertezza(q)`, con l'incertezza in `[u_min, 1]` da due segnali gratis sui candidati in mano (linea QPP): lo **stacco** tra il primo e il secondo punteggio della fusione testo+vettori e l'**accordo** tra le due liste sul primo risultato. Prima netto e liste concordi → incertezza bassa → il grafo pesa poco. È `Weights.graph_policy` (`Fixed` = il comportamento v0.3.2, che resta il **predefinito**: la taratura v0.4 ha mostrato che i due segnali, su quel corpus, non separano le domande multi-salto dalle dirette — REPORT v0.4); la variante completa è `Fusion::Adaptive { alpha, cos_norm, w_max, u_min }`, e la CLI la prova con `ENGRAMMA_GRAPH=adaptive:<w_max>,<u_min>,<spw>`.
4. **Filtri temporali:** con `as_of` vale solo ciò che era valido in quella data. Di norma, solo ciò che vale oggi.
5. **Riordino:** `punteggio = (1−w_g)·fusione + w_g·massa_grafo` (la convessa v0.2.1; la RRF resta un'opzione) `× peso_affidabilità × peso_recenza` (solo modo testo). I pesi sono configurabili e i valori predefiniti sono tarati sul benchmark (§ 11). **α della fusione convessa = 0,5** (v0.4.5): la storia completa — 0,6 (v0.3.1) → 0,7 (v0.3.2, regola robusta sul solo banco italiano) → 0,5 — è decisa su DUE banchi: sul banco italiano a 100k gli ammissibili per la regola robusta (MRR pesato ≥ 0,8429, minimo fra i vicini ±0,1 di α ≥ 0,8383) sono {0,5; 0,6; 0,7}, e fra questi vince chi ha il turno ndcg@5 più alto su LongMemEval dev (0,7303 per 0,5), con conferma su 320 domande tenute da parte prima di guardare i loro numeri (sessione any@5 0,9750 contro 0,9625 del solo testo, all@5 0,8500 contro 0,8344, turno ndcg@5 0,6813 contro 0,6620). Il commento in `search.rs` (`Fusion::default`) porta regola e numeri.
6. **Spiegazione per ogni risultato:** `matched_terms`, `semantic_rank`, `via: [[Giulia]] → lives_in`, `confidence`, `valid_from`, `source` — e, per domanda, il **peso del grafo** con i suoi due segnali (`explain.graph`: peso, perché è spento, stacco, accordo).

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
- **Il modello predefinito** — *deciso in v0.2 misurando sul benchmark italiano (§ 11), confermato in v0.4* — è **IBM Granite Embedding Multilingual R2 97M**: Apache 2.0, ModernBERT, 384 dimensioni, italiano tra le lingue di addestramento, nessun prefisso (MTEB Multilingual Retrieval 60,3). Il 311M (768 dimensioni con Matryoshka, 65,2) resta un'opzione per chi dà più RAM. **EmbeddingGemma 308M** (62,5) ha **licenza Gemma, non open source**: supportato come cartella fornita dall'utente, non incluso né scaricato di serie.
  - I modelli si scaricano con `engramma model pull`, con revisione e sha256 fissati.
- **Il lotto di incorporazione (v0.4.6):** i testi sono **ordinati per lunghezza in token vera** e tagliati in lotti con un **tetto di token**: `righe × seq_lotto ≤ ENGRAMMA_EMBED_TOKEN_BUDGET` (predefinito **512** dalla v0.4.6.1, pari a `max_tokens`: su CPU i lotti grandi non comprano tempo e gonfiano solo le attivazioni — a 1 lavoratore, mimalloc, 512 → picco 878 MB e 64 note in 5,8 s, 4096 → 1126 MB e 6,4 s; banco a 10k 100 contro 75 note/s; un valore non valido — 0, non numerico — ricade sul predefinito). Un **pool fisso di lavoratori** — **predefinito 1**, `ENGRAMMA_EMBED_THREADS` per stringerlo o allargarlo — prende i lotti da una coda, a fette dei buffer, senza le doppie copie della v0.4.4; l'offset di ogni lotto è la **somma prefissa** di `righe × seq`. *Cosa non funzionava nella v0.4.5:* il `tokenizer.json` di Granite R2 porta il **riempimento BatchLongest**, quindi `encode_batch` riempiva ogni testo al più lungo della chiamata — l'ordinamento non ordinava niente, una nota corta pagava il calcolo della più lunga (il calo del banco a 100k da 201 a 74 note/s) e gli offset `inizio × seq` erano giusti **solo** perché il riempimento uniformava i seq: tolto il riempimento, senza la somma prefissa un lavoratore leggeva fuori dai limiti. Da v0.4.6 il tokenizer è caricato **senza** riempimento (`with_padding(None)`; il suo `pad_id`, se configurato, è quello dei nostri lotti) e la troncatura resta sua (32768, sopra il nostro taglio a `max_tokens`) così `approx_tokens` vede la lunghezza vera. **La proprietà vera dei vettori:** identici bit per bit a parità di configurazione (un ordine diverso dà gli stessi lotti); **fra budget diversi** la forma del lotto cambia l'ordine delle somme dentro tract e l'ultima cifra si muove — la promessa onesta è coseno ≥ 0,999999 e |Δ| ≤ 1e-5 per componente (test di fedeltà).
- **La RAM del modello, misurata a matrice (v0.4.6.1, macOS arm64, Granite 97M f32 da 390 MB, 5 corse in processi separati — mediana / min / max):** la griglia della v0.4.6 ({2048, 4096, 8192} × {1, 2, 4, 6}) sceglieva (4096, 1) — 1126 MB di picco con mimalloc — ma non aveva budget piccoli a 1 lavoratore; misurati in verifica: **(512, 1) picco 878 MB, +10 s 888 MB, 64 note 5,8 s** (sistema: 1024 / 1132 MB), (1024, 1) 912 MB, (2048, 1) 993 MB. Lezione: una regola «il punto migliore della griglia» vale quanto la griglia — va esplorata fino al bordo di entrambe le dimensioni. Nell'artefatto Node 24: modello caricato **894 MB**, dopo `embedPending` di 64 note **959 / 959 / 959 MB** a +10 s (1224 a budget 4096), `embedPending` ~11,4 s; il picco (`maxRSS`) **1222 MB** è ora quello del **caricamento del modello** (1220 MB già dopo `Engine.open`: l'ottimizzazione del grafo in tract), non dell'embedding — abbassarlo vuol dire toccare il carico, non i lotti. **La soglia di 700 MB a +10 s NON è raggiunta**: le pagine liberate restano residenti su macOS con qualunque allocatore (causa misurata nella v0.4.5: `malloc_zone_pressure_relief` su darwin 27 restituisce 0 MB — provato, inutile, da non riprovare). Portare l'embedding **fuori processo** resta un'opzione architetturale, nel binario parallelo del § 12. Numeri nel REPORT v0.4.6.
- **mimalloc è l'allocatore dei due artefatti finali** (il `.node` di Homun e il binario `engramma`), *non* delle librerie: l'allocatore lo sceglie chi produce l'eseguibile. Su **macOS vince nettamente** anche nella v0.4.6 (modello caricato 846 contro 1100 MB a processo nudo; al punto scelto (512, 1): +10 s 888 contro 1132 MB, picco 878 contro 1024); su **Linux glibc restituisce e trattiene meno** (v0.4.5: 637 contro 1166 MB a +10 s; i numeri v0.4.6 nel REPORT), quindi da v0.4.5 l'allocazione è **per piattaforma**: mimalloc solo dove vince.
- **La cache NNEF del modello è stata provata e scartata** (v0.4.5): il grafo **ottimizzato** di tract non è serializzabile in NNEF (le op `Opt*` non hanno serializzatore) e nemmeno quello tipizzato (`StridedSlice`); servirebbero serializzatori custom. Misurato prima di arrendersi: il carico a freddo costa 0,6–2,4 s e picca a ~1,11 GB. Da non riprovare con tract 0.23.
- **I pesi in f16 sono stati provati e scartati** (v0.4.4): `FloatPrecisionTranslator` su Granite 97M non abbassa la memoria dopo il carico (803 vs 822 MB a processo nudo, 983 vs 1192 MB dopo il lavoro) e peggiora la fedeltà — coseno contro f32: medio 0,99948, peggiore 0,99438. Da non riprovare.
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

### 8.1.1 Le celle d'oro (v0.6.8, H7)

Da questa fase gli utenti tengono dati veri in celle scritte da questa
versione: ogni versione futura deve aprirle e migrarle senza perdite.
Una **cella d'oro** in `crates/engramma-core/tests/golden/` — scritta
dalla versione che la introduce, con uno **script ripetibile nel repo**
(`examples/make_golden_cell.rs`: orologi e conii iniettati, dati
sintetici, mai dati veri) — contiene note di tutti i tipi e di tutti
gli stati, quarantena nata `external` e confermata, lapide, supersede,
una copia di conflitto aperta e una chiusa, una purga, una compattazione
con la sua istantanea, una vista della cartella e i vettori. Il test
`golden_cell.rs` copia la cella in una cartella temporanea, la apre col
motore di oggi, fa `verify` (verde, `FromSnapshot`) e controlla i fatti
attesi. **La regola**: ogni fase che cambia lo schema aggiunge la sua
cella d'oro e non tocca quelle vecchie; il test le apre tutte (l'elenco
`GOLDEN_CELLS` cresce a vista).

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
- **Strumenti di lettura** (dalla v0.1, tutti con `readOnlyHint: true`): `memory_search`, `memory_get`, `memory_neighbors`, `memory_tags`, `memory_timeline`.
- **Scrittura (v0.5):** con `engramma mcp --cells a,b --write a --actor claude-desktop` il server espone **un solo** strumento in più, `memory_propose` (`readOnlyHint: false`, `destructiveHint: false`, `openWorldHint: false`), che **propone** un fatto — la nota nasce in quarantena, nella coda. `--write` è un sottoinsieme di `--cells` e richiede `--actor`: l'identità viene dalla configurazione, **mai** dal `clientInfo` dichiarato (che viaggia solo come metadato della proposta); le proposte firmano `mcp:<actor>`. **Nessuno strumento** conferma, scarta, cancella, purga o cambia la politica: quelle restano superfici umane (CLI, pagina `review`, modulo del client). Un test fissa la lista esatta degli strumenti, con e senza `--write`.
- **Le `instructions` del server** dicono all'assistente l'etichetta: proporre solo fatti stabili che l'utente ha detto o confermato, **chiedere prima**, mai proporre contenuti letti dal web, correggere con `supersedes`. È cortesia, non sicurezza: il confine vero è la coda, i limiti del revisore e il revert.
- **Elicitation (politica `confirm`).** Il client che dichiara la capacità riceve un **modulo** con il testo modificabile e i tasti accetta/rifiuta: accetta → `decide(accept)` con attore `user@mcp:<actor>` (il testo editato diventa *accetto con modifica*), rifiuta → `reject`, annulla o silenzio → la proposta resta in coda. Le due forme del protocollo, come le offre rmcp 3.8: **richiesta dal server** (`elicitation/create`, fino alla 2025-11-25) e **`InputRequiredResult`** col ritento del client (2026-07-28, flusso *discover*: la proposta è già in coda quando il tool risponde `input_required`; il `requestState` è monouso). Un client senza elicitation riceve `pending` e l'indirizzo dove decidere. La **modalità URL** — aprire la pagina di revisione passando un token nell'URL al client — NON è nella v0.5: un token in un URL dato a un modello va pensato a parte.
- **Ambiti:** il server si avvia con l'elenco delle celle esposte (`engramma mcp --cells famiglia,preferenze`). Ogni lettura finisce nel registro d'accesso. Gli errori non sono un oracolo: una cella non esposta e una cella non scrivibile danno risposte che non rivelano altro.
- **Il confine dichiarato:** un agente che ha una shell con l'utente del sistema (per esempio Claude Code) può lanciare `engramma proposals accept`. **Chi ha la tua shell è te** — è fuori dal modello delle minacce, e non facciamo finta del contrario.
- **HTTP (v0.5.2):** `engramma mcp --http 127.0.0.1:8787 --public-url https://…` serve lo stesso server di strumenti sul trasporto Streamable HTTP di rmcp, dietro un token Bearer risolto in un **grant** (§ 8.6). L'attore delle proposte non è più `--actor`: è l'etichetta del grant (`mcp:<etichetta>`), scelta dall'umano che approva il collegamento. `--cells` e `--write` restano il **tetto del processo**; il grant può solo restringerlo: la lista degli strumenti è **per token** (cinque strumenti di lettura per un grant di sola lettura), le celle visibili sono l'intersezione grant ∩ `--cells`, e una cella fuori dal grant risponde esattamente come una che non esiste. Con `--compat chatgpt` si aggiungono `search` e `fetch`, involucri a schema fisso di `memory_search`/`memory_get` (id completi, `url` sempre vuota: la nota non ha indirizzo pubblico, e ChatGPT allora non cita la fonte). Come si espone e chi vede che cosa: `docs/http.md`.

### 8.4 Homun (v0.4)
- **Collegamento in-process:** il daemon carica il modulo `napi-rs` — `crates/engramma-node`, **N-API** (stabile tra versioni di Node ed Electron: il `.node` non si ricompila a ogni aggiornamento). Il contratto JS (`Engine`, `Cell`) è in `docs/node.md`: ogni metodo che tocca disco o modello è asincrono sul pool di libuv, gli errori sono `Error` con `code` stabile e senza contenuti di note (l'oracolo chiuso vale anche per `get`: una nota in quarantena risponde `null`). Il modello sta nel demone, una volta (la RAM misurata sta nel § 7); la ricerca su più celle fonde le classifiche con RRF k=60, dichiarata approssimata. Dal v0.4.4 le allocazioni Rust del modulo passano per **mimalloc** (§ 7): Node ed Electron restano col loro allocatore.
- **Firma su macOS:** il `.node` va **fuori dall'asar** e firmato con l'identità dell'app prima della notarizzazione (`codesign --options runtime`; `docs/node.md`).
- **Migrazione:** le note di oggi (`agents/<id>/memory`, `user/memory`) diventano celle:
  - `bot-<id>` per le note di un bot;
  - `utente` per quelle condivise;
  - `core` diventa un tag;
  - `source` vale `migrated:homun`.
- **Strumenti del bot:** `recall_memory` e `update_memory` restano nell'interfaccia, ma usano il motore. Nascono `search_history` e `read_history`, con l'indice delle conversazioni in **una cella `storia-<id>` per bot** (v0.4.4: non una cella `storia` unica — la cronologia di un bot è sua, cancellabile in blocco con la sua cella, e il nome `storia` non riserva niente a nessuno).
- **Il contratto di `dropCell`** (v0.4.3, con Homun): i nomi a forma di istantanea (`<qualcosa>-snap-<cifre>`) sono **riservati** alle istantanee di compattazione — una cella così chiamata verrebbe cancellata insieme alla cella di cui porta il nome, e la creazione la rifiuta; la connessione condivisa si chiude **subito**, anche se un oggetto `Cell` resta vivo in JS (il GC non decide lui quando l'oblio è fatto); nessuna `Cell` nasce morta (gate di generazione e apertura sotto lo stesso lock).
- **Stato dell'integrazione:** Homun fissa la versione con un lock degli hash del `.node`, di `index.js` e di `index.d.ts`: dal 2026-11-10 usa la v0.4.3 (tag Homun `fase-memoria-engramma-v043`); la v0.4.4 (allocatore) è pronta per un aggancio successivo. Le note per l'integrazione stanno fuori dal repo, una per versione.
- **Quarantena:** ciò che il bot ricava in un turno contaminato si scrive come `external` — nasce in quarantena e la ricerca non la vede finché qualcuno non la conferma.
- **Oblio:** `purge` non mente — se il WAL o una vista markdown trattengono ancora byte, la risposta lo dice (`pending`) e il chiamante finisce il lavoro.
- **La sezione Memoria di Homun** è un'interfaccia sul motore: elenco, ricerca, grafo, «perché lo so?» (l'explain, peso del grafo compreso), conferma della quarantena, storico.
- **La sincronizzazione è dentro il modulo** (v0.6.2, § 12): `Engine.sync()` apre il servizio della radice — abbinamento col biglietto (entrambe le forme, il QR lo disegna l'app), SAS confermato dall'interfaccia (impossibile saltarlo), passate e ciclo continuo, rimozione, eventi (`arrived`, `removed`, …) a JS. Il ciclo è quello della CLI, in una sola copia: la CLI è un guscio del servizio, con un lucchetto esclusivo per radice (`sync.lock`).

## 8.5 Proposte, revisori, storico e rollback (v0.5)

Claude, ChatGPT e Cursor leggono Engramma via MCP; la v0.5 fa sì che un assistente possa **proporre** un fatto («vuoi salvarlo su Engramma?»), che un modello di fiducia possa **decidere al posto dell'utente** se lui lo configura, e che ogni decisione sia **annullabile**.

### Il modello delle minacce

1. **Avvelenamento della memoria.** Un assistente legge una pagina ostile e «propone un fatto». Se quel fato diventasse attivo, Homun e gli altri lo leggerebbero come vero. Per questo ogni proposta nasce **in quarantena**, e la ricerca non la vede.
2. **Chi propone non conferma.** Nessuno strumento MCP conferma, scarta o cancella. Si decide da superfici umane (CLI, pagina locale `review`, modulo di elicitation) o da un revisore automatico che l'**utente** ha configurato.
3. **Il revisore automatico può essere ingannato** dal testo della proposta. Quindi: decide dentro limiti fissati dalla politica; per ogni errore o dubbio **fallisce in sicurezza** (la proposta va all'umano); ogni decisione resta nello storico ed è **annullabile**, anche in blocco.
4. **Il confine dichiarato:** un agente con una shell dell'utente può lanciare la CLI. Chi ha la tua shell è te: fuori dal modello delle minacce.
5. **Il testo delle note è ostile per la pagina web.** Ogni testo è escapato; il token, l'host e l'origine si controllano; la pagina non interpreta il markdown (i collegamenti restano testo).

### La politica per cella

Un'**operazione** del registro (`set_policy`), così lo storico dice chi ha permesso cosa, e quando. Si legge con `engramma policy show <cella>` e si cambia **solo** dalla CLI (`engramma policy set <cella> write=… reviewer=… …`): mai dal web, mai da MCP. `write: off` è il predefinito — una cella che nessuno ha aperto resta chiusa, su ogni superficie.

- **`write`:** `off` | `propose` (tutto in coda) | `confirm` (modulo del client se lo supporta, altrimenti coda) | `auto` (decide il revisore).
- **I limiti di `auto`:** `reviewer` (nome di uno configurato), `min_confidence` (0,8 predefinito: sotto, la decisione va all'umano), `kinds` (tipi di nota ammessi), `allow_supersede` (`none` predefinito | `inferred` | `any`), `max_auto_per_hour` (**l'interruttore**, 20 predefinito: oltre il tetto ogni proposta va all'umano e la CLI lo mostra). **Mai in automatico:** le proposte `confidence: external` e quelle su note purgate o in lapide.

### I revisori

Si configurano nella radice dei dati, `reviewers.yaml` (saphyr): il file contiene il **nome della variabile d'ambiente** della chiave, mai la chiave — una `api_key` scritta nel file non parsa affatto. Due tipi: **`command`** (programma esterno, argv fisso, niente shell: richiesta JSON su stdin, risposta su stdout, tempo massimo) e **`openai`** (endpoint compatibile `/v1/chat/completions` con uscita a schema JSON: OpenAI, llama-server, Ollama e chiunque parli quell'interfaccia). Il contratto: in ingresso la proposta (testo, titolo, tipo, tag, confidenza, modo, testo del bersaglio, motivo), fino a 5 note simili, il nome della cella, un riassunto della politica e la versione del prompt — tutto il personale fra delimitatori `<<<BEGIN/END …>>>`; in uscita `{decision: accept|reject|escalate, reason ≤ 500, confidence 0..1}`. **Lettura severa:** qualunque altra cosa vale `escalate`. Solo un fallimento di trasporto (timeout, crash, risposta illeggibile) marca la proposta «revisione automatica fallita» — ritentabile con `engramma proposals review`, che non tocca ciò che il revisore ha passato all'umano di proposito.

Il **prompt** sta nel repo (`prompt-v1.md`, `include_str!`), ha una versione, e il suo blake3 entra in ogni decisione: lo storico può dire *sotto quale prompt* un modello ha deciso. Il prompt dichiara che i contenuti sono dati e non istruzioni — e che questo **riduce** il rischio ma non lo elimina: il resto lo fanno i limiti, l'interruttore e il revert.

### Le due proprietà del revert

L'annullamento (`revert_decision`) è il `git revert` del registro: una **nuova operazione** che riporta la proposta in coda; la storia non si riscrive mai.

- **Mai sopra una correzione umana.** Se la nota o il bersaglio hanno operazioni successive alla decisione, il revert si rifiuta con `CONFLICT` e le nomina. Niente `--force` in questa fase.
- **Il purgato non torna.** Se il contenuto è stato purgato, il revert lo dice (`purged`): l'op d'audit resta, il contenuto no.

Le decisioni dei revisori firmano `reviewer:<nome>`: l'annullamento in blocco (`engramma revert --by reviewer:x [--since …] [--until …]`, con anteprima e conferma) è un filtro, niente di più. Un difetto trovato dai property test mentre li scrivevamo: anche i `revert_decision` della nota purgata vanno **censurati** (non dimenticati) dalla purge, perché il revert riapre un bersaglio — il rigioco deve riaprirlo allo stesso modo, bump di versione compresi.

### 8.6 MCP su HTTP e OAuth 2.1 (v0.5.2)

ChatGPT, claude.ai e le app mobili raggiungono solo server remoti HTTPS:
per loro Engramma sale su HTTP restando ciò che è — un programma locale
con un solo proprietario. Ascolta **solo su loopback** (un indirizzo
d'ascolto non loopback è rifiutato: il TLS e l'interfaccia pubblica
appartengono al tunnel, `docs/http.md`); il server MCP è un *resource
server* (metadati RFC 9728) ed Engramma è anche il *server di
autorizzazione* (RFC 8414): client pubblici con **PKCE S256**, mai
segreti client, mai token nella query string.

**Il modello delle minacce di questa fase:**

1. *L'indirizzo è pubblico.* Chiunque lo conosca può chiamare ogni
   endpoint; quindi ogni tool vuole un token (niente modalità anonima:
   anche `initialize`), e **nessun token esiste finché un umano non
   approva sul computer dove gira Engramma** digitando il codice che il
   **suo browser** mostra (lo schema del flusso dispositivo, RFC 8628
   § 5.4: il codice, da solo, non autorizza niente — conta dove viene
   digitato). Nessun segreto indovinabile sta su Internet.
2. *Furto del token.* Accesso di un'ora; refresh a **rotazione** con
   revoca dell'intera famiglia al riuso di uno già consumato (OAuth 2.1
   § 4.1.3 vale anche per il codice d'autorizzazione, monouso da 60 s);
   token conservati solo come `blake3`; revoca immediata da CLI, con
   effetto alla richiesta successiva (provato con una sessione aperta).
   **`/revoke` di un refresh porta con sé gli accessi della stessa
   famiglia** (RFC 7009 § 2.1); revocare un accesso tocca solo lui e il
   client continua a rinfrescare (v0.5.3).
3. *Phishing del consenso.* L'approvazione mostra host del client_id e
   host del redirect; la pagina d'attesa (senza JavaScript, CSP chiusa,
   `X-Frame-Options: DENY`, `no-referrer`, `no-store`) mostra gli
   stessi nomi e l'avviso per i redirect di loopback; e la conferma
   passa per il codice riscritto a mano. **Tetti in attesa (v0.5.3):**
   due richieste per `client_id` e dieci in tutto (10 minuti ciascuna) —
   un client solo non riempie la coda; uno spammatore con molti domini
   resta possibile ed è la ragione per cui esiste `engramma oauth deny
   --all`. `/authorize` ha anche un tetto globale di 30 richieste al
   minuto (pagina 429 con `Retry-After`, come `/token`) e una **cache
   negativa del CIMD**: un documento rifiutato o irraggiungibile non si
   riscarica per cinque minuti (l'URL esatto, un evento solo).
4. *SSRF dal CIMD.* Il `client_id` è un URL scelto da chi chiama: lo
   scaricamento risolve il DNS e **si collega solo a indirizzi
   pubblici** (loopback, RFC 1918, link-local con i metadati cloud,
   CGNAT, ULA, multicast, non specificati, IPv4 mappati in IPv6, **6to4
   — giudicato dall'IPv4 che incorpora, come i mappati —, NAT64 locale
   RFC 8215, Teredo e IPv4-compatibili — rifiutati** (v0.5.3), e i
   blocchi di documentazione sono scartati, qualunque forma testuale —
   proptest), senza redirect, 5 KiB, 5 secondi, solo JSON. L'eccezione
   loopback per i test esiste solo nella configurazione della
   libreria, mai dalla CLI. `private_key_jwt` da solo è rifiutato
   dichiarando il perché (un documento che dichiara *anche* `none` —
   come quello vero di ChatGPT — passa). Il rifiuto sul filo resta
   generico; l'**evento locale porta la causa** (DNS, nessun indirizzo
   pubblico, timeout, stato HTTP, TLS, regola infranta).
5. *Rebinding DNS e CSRF.* `Host` ammessi solo l'autorità pubblica e i
   nomi di loopback (403 altrimenti, su tutta la superficie); ogni
   richiesta con `Origin` è rifiutata (i client remoti chiamano da
   server, non da browser: `enforce_origin_validation` di rmcp su
   `/mcp`, guardia uguale su `/token`, `/register`, `/revoke`).
6. *Confusione token-sessione.* Una sessione MCP dei protocolli
   precedenti appartiene al grant che l'ha creata (un altro token
   risponde 404, come una sessione ignota — provato scambiandole); un
   `requestState` 2026-07-28 è legato al grant, resta monouso ed è
   **256 bit casuali** come gli altri segreti, con **TTL di 10 minuti**
   (uno scaduto risponde come uno sconosciuto) e tetto di 1000 turni
   (v0.5.3). Anche la mappa delle sessioni ha un tetto (1000, escono le
   più vecchie) e un'ora di inattività, e il `DELETE` accettato dal
   trasporto la svuota: nessuna delle due cresce per sempre.
7. *L'avvelenamento della memoria resta quello della v0.5:* un client
   remoto può solo **proporre**; coda, politica, revisore e revert sono
   quelli di § 8.5, riprovati via HTTP (quarantena invisibile,
   `external` rifiutato, oracolo chiuso).
8. *Chi vede in chiaro:* il fornitore dell'assistente vede ciò che il
   client legge; il fornitore del tunnel vede tutto se termina lui il
   TLS (Cloudflare, ngrok) e niente con Tailscale Funnel (`docs/http.md`).

**Le scelte:**

- **Token opachi, non JWT:** server di autorizzazione e server di
  risorse sono lo stesso processo con lo stesso archivio; la ricerca
  locale dà revoca immediata, nessuna chiave da ruotare, nessun
  algoritmo da scegliere. I JWT servono quando è la risorsa a validare
  da sola: il cluster della v0.7+.
- **`oauth.db` staccato dal registro delle celle:** sono credenziali,
  non memoria — non devono viaggiare con la sincronizzazione della v0.6
  e la loro revoca dev'essere immediata e locale. Schema proprio con la
  stessa guardia delle celle (uno schema più novo si rifiuta);
  permessi 0600; di ogni segreto solo il digest; una tabella `events`
  senza segreti né testo di note.
- **CIMD come default, DCR spento:** la registrazione dinamica
  (RFC 7591) è deprecata dalla specifica, apre una superficie in
  scrittura a chiunque e nessuno dei due client la serve davvero. Con
  `--allow-dcr`: solo client pubblici, redirect validi, 20 inutilizzati
  al massimo, quelli mai usati scadono in 24 ore. I redirect si
  confrontano **esatti**, con l'unica eccezione di RFC 8252 § 7.3 (la
  porta si ignora solo per `http` su loopback: serve a Claude Code).
- **Approvazione = superficie umana e locale:** `engramma oauth
  pending/approve/deny/grants/revoke/log`. L'etichetta dell'approvazione
  diventa l'attore `mcp:<etichetta>` delle proposte. La pagina di
  revisione non espone l'approvazione in questa fase: è legata a una
  cella, il grant è di tutto il processo (possibile seguito).
- **Ambiti:** `memory:read` è il pavimento; `memory:propose` esiste solo
  se il processo ha `--write` e l'approvazione tiene una cella
  scrivibile. **Gli strumenti di compatibilità (v0.5.3):** con
  `--compat chatgpt`, `search` e `fetch` esistono **solo** per il grant
  il cui `client_id` è il documento CIMD di ChatGPT
  (`https://chatgpt.com/oauth/client.json`, un campo di configurazione
  della libreria che i test sostituiscono con l'URL di loopback del
  loro documento): Codex (CIMD suo) e claude.ai usano gli strumenti
  normali, non li vedono nell'elenco e chiamarli per nome risponde
  *esattamente* come uno strumento che non esiste — lo stesso errore,
  byte per byte. Su stdio il flag è un errore d'uso. Gli ambiti sconosciuti (ChatGPT chiede `openid email
  profile`) si **ignorano**: non emettiamo `id_token` e non parliamo
  OIDC; la risposta del token dice cosa è concesso davvero. Il tetto
  globale di `/token` è 60 richieste al minuto (dietro un tunnel ogni
  chiamante è 127.0.0.1: le intestazioni inoltrate non sono affidabili
  e non si usano).
- **Registro d'accesso** su stderr: ora, id del grant, metodo MCP,
  strumento, stato, durata. Mai token né testo; provato a cattura.
- **Modalità URL dell'elicitation: fuori anche da qui.** Una pagina di
  conferma raggiungibile dal web richiede di autenticare il
  proprietario **sul web pubblico**: senza un segreto indovinabile
  serve una passkey (WebAuthn) — una crate e una fase a sé. Candidata
  per la v0.8 insieme a `engramma serve` (§ 12).

## 9. Server e cluster (architettura, v0.8 e v0.9)

- **`engramma serve`:** API HTTP/gRPC + MCP HTTP, autenticazione con token per ambito o OIDC, migliaia di celle per processo e celle aperte su richiesta, con un tetto di celle aperte.
- **Cluster:**
  - **Metadati di posizionamento** (quale cella sta su quale nodo): un piccolo gruppo Raft (`openraft`).
  - **Router senza stato** davanti ai nodi. Ogni domanda tocca una cella. Le domande su più celle si mandano a tutte in parallelo e si fondono con RRF.
  - **Replica per cella:** il registro (§ 4.3) si replica con Raft su più nodi, in gruppi multi-Raft che raggruppano più celle per gruppo. La replica è deterministica, quindi le copie restano identiche.
  - **Spostare una cella:** istantanea (API di backup di SQLite più i vettori), recupero del registro dal numero di sequenza dell'istantanea, poi cambio del posizionamento.
  - **Backup:** istantanee e registro su storage a oggetti (stile S3).
- **Scalabilità orizzontale:** si aggiungono nodi e si spostano celle. Una cella enorme (un'azienda) si divide **per ambito** in più celle: le celle sono il confine sia della scala sia dei permessi.

## 10. Cifratura e condivisione

### 10.1 I dispositivi del proprietario (v0.6)

- **Sul disco:** in locale ci si affida alla cifrazione del sistema (FileVault, BitLocker); più avanti, opzionale, la cifratura della cella (**da decidere:** SQLCipher, BSD, o cifratura a livello di pagina).
- **L'identità del dispositivo.** `<root>/device.key`: un seme da 32 byte, permessi 0600, mai lasciato dal disco. HKDF-SHA256 con etichette distinte ne deriva la chiave di **trasporto** iroh (Ed25519), quella di **firma** delle operazioni (Ed25519) e quella **HPKE** (X25519) — mai la stessa chiave per due usi. L'id del dispositivo **è** la chiave pubblica di firma: ogni firma nomina il suo autore per costruzione. Il portachiavi del sistema è un seguito.
- **L'elenco dei dispositivi** (`devices.db`): un piccolo registro firmato per radice dei dati — `create` (il primo, da sé), `add` (un membro garantisce), `remove` (un membro revoca, con la radice di Merkle e il conto delle operazioni del revocato che chi rimuoveva aveva accettato). **La revoca domina:** chi è stato rimosso non rientra; l'ultimo dispositivo non si rimuove; un non-membro vivo non firma nulla. L'elenco si riconcilia **prima** delle celle, a ogni sessione.
- **La firma delle operazioni.** Ed25519 sul `content_hash` (§ 4.3.1), colonne `device` e `sig`. Una firma che non torna, o un autore assente o rimosso, non entra. `actor` resta quello di sempre (`user`, `mcp:<etichetta>`…); l'autore crittografico è il dispositivo, e `revert --by device:<nome>` lo usa.
- **L'abbinamento.** Due modi, un esito: il **biglietto** (indirizzo iroh di chi invita più un segreto a 128 bit, base32 con prefisso di versione, dieci minuti e un uso; chi entra autentica chi invita dal MAC del segreto sul transcript — il segreto in chiaro non attraversa mai) e il **codice breve** (`7-tigre-lago`: canale più due parole da una lista fissa di 2048 parole italiane nel repo; SPAKE2 — RFC 9382, gruppo Ed25519 — col codice come password; **un tentativo per canale**: un attaccante attivo ha una possibilità su 2048². *La crate `spake2` non ha audit indipendente: l'uso è confinato a questo percorso*). In entrambi i casi entrambi gli schermi stampano le stesse **sei cifre** (SAS: blake3 del segreto condiviso e del transcript) e l'umano conferma — solo dopo firma la `add`.
- **Le buste.** Una chiave di cella per **epoca** (32 byte; l'epoca 0 da chi condivide, k+1 da chi rimuove), avvolta per ciascun dispositivo con **HPKE** (RFC 9180, modo base, X25519 HKDF-SHA256 ChaCha20-Poly1305, `info = etichetta ‖ cell_sync_id ‖ epoca`). L'operazione viaggia in **XChaCha20-Poly1305** (chiave `HKDF(chiave d'epoca, "engramma/op/v1")`, nonce casuale di 24 byte, AAD = `cell_sync_id ‖ epoca ‖ id opaco`). Sul filo fra dispositivi la busta sta comunque dentro QUIC: il sigillo serve al relay e al nodo cieco.
- **Gli id opachi.** `cell_sync_id` = HMAC della chiave d'epoca 0; l'id opaco di un'operazione = HMAC(chiave di mac stabile, `content_hash`): le operazioni si riconoscono attraverso le epoche, e il nodo cieco non vede né `op_id` né HLC.
- **Il nodo cieco** (`engramma/mailbox/1`): conserva e consegna buste sigillate per elenco e `cell_sync_id`, indicizzate per id opaco e null'altro; **non ha chiavi di cella**. Accetta solo dalle chiavi di trasporto dei dispositivi vivi; i nomi nell'elenco viaggiano cifrati per i soli dispositivi (lui vede chiavi). La `purge` arriva **firmata** e i byte delle buste nominate se ne vanno davvero (DELETE e VACUUM). Ritenzione: 180 giorni di base.
- **Il modello delle minacce e i suoi limiti.** Il relay di n0 e il nodo cieco (ALPN `engramma/mailbox/3`) vedono chiavi, indirizzi, orari e dimensioni; non vedono testo, nomi, titoli, tag, autori, né `op_id` in chiaro. Possono trattenere o scartare messaggi (disponibilità), non falsificarli né riordinarli: le operazioni sono firmate e lo stato è indipendente dall'ordine (§ 4.3.1). Un orologio oltre ora+5 minuti è rifiutato all'arrivo. **Cosa questa fase non promette:** né sicurezza dopo compromissione (PCS) né segretezza in avanti (FS) per messaggio — una chiave d'epoca rubata apre tutta l'epoca; la rotazione alla rimozione è il compromesso giusto per i dispositivi di una sola persona. **MLS (RFC 9420)** presuppone un servizio di consegna che ordina (OpenMLS 0.9); **BeeKEM/Keyhive** e **DCGKA** sono ancora pre-alpha: è la scelta da rivedere per la condivisione con altre persone (v0.6.x). Fonti: RFC 9180 (HPKE), RFC 9382 (SPAKE2), RFC 9420 (MLS), il [paper Keyhive](https://dstaley.com/thesis.pdf) (2025) e [DCGKA](https://eprint.iacr.org/2024/1665) (2024).
- **Quello che resta locale e non viaggia mai:** `device.key`, `oauth.db`, la configurazione; i legami della vista markdown; i vettori e il registro dei modelli (ogni dispositivo li ricalcola); le istantanee, la riga `hlc` di `meta`.

## 11. Come si misura

- **Benchmark nostro**, in italiano e inglese: storie sintetiche di una persona nell'arco di mesi (traslochi, lavori, persone, preferenze che cambiano), con domande:
  - fattuali, temporali («dove abitava a marzo?»);
  - di aggiornamento;
  - multi-salto («chi lavora con il partner di…»);
  - di astensione (la risposta non c'è).

  Si misurano il richiamo@k e il MRR sulla parte del motore: **non** serve un giudice LLM, perché si giudica se la nota giusta è tra le prime k.
- **Confronto esterno — LongMemEval** (v0.4.4, `bench/examples/longmemeval.rs`; metriche e divisione rifatte in v0.4.5): LongMemEval-S *cleaned*, 470 domande (le 30 di astensione escluse), una cella per domanda, nota = round, **metriche ufficiali** di `eval_utils.py` (`recall_any/all@k` e `ndcg@k` a livello turno e sessione — l'ndcg di sessione dal v0.4.5 è ESATTAMENTE quello ufficiale: lista non deduplicata, taglio `k_eff` al primo prefisso con k sessioni distinte, ideale che conta i round del corpus nelle sessioni giuste; ndcg = 0 se l'ideale è 0), k = 5 e 10, modalità testo (BM25, `graph_hops = 0`) e ibrida, `question_date` come «adesso» per la recenza. Lo studio di α (v0.4.5) divide le domande una volta per tutte: **dev** = le 150 del sottoinsieme fisso della v0.4.4, **test** = le altre 320 (sha256 della lista nel REPORT), e la scelta del predefinito avviene su dev + banco italiano, con il test guardato UNA volta, dopo. **Il numero di partenza (v0.4.5, su tutte le 470, α=0,5):** testo → sessione `any@5` 0,9702, `ndcg@5` 0,6607, turno `ndcg@5` 0,6829; ibrida → sessione `any@5` **0,9809**, `all@5` **0,8723**, `ndcg@5` **0,7536**, turno `any@5` **0,9106**, turno `ndcg@5` **0,6970** (corsa rifatta in verifica: la riga originale del REPORT era ad α=0,7). Su questo dataset ripulito BM25 è quasi saturo in cima, ma con α=0,5 la fusione lo batte su tutte le metriche, sessione e turno. I lavori pubblicati (dataset originale, indici diversi) riportano 0,8–0,97: il confronto resta un ordine di grandezza. LoCoMo solo con cautela: il suo giudice e le sue risposte di riferimento sono stati contestati nel 2026.
- **Prestazioni**, su un Mac M-series, cella da 100 000 note — **misurate** (v0.4.3, verifiche; confermate dal banco in v0.4.4 entro il rumore):
  - inserimento ~1 900–2 500 note/s senza embedding;
  - ricerca testo + grafo p95 **12,3 ms**;
  - ricerca mista calda (embedding della domanda già calcolato) p50 **10,5 ms**, p95 **13,9 ms**;
  - ricerca ibrida con embedding della domanda p95 ~24 ms;
  - apertura di una cella ~1 ms;
  - embedding ~135–148 note/s (Granite 97M, 10 filoni).

  Gli obiettivi della bozza (≥ 2 000/s, p95 ≤ 30 e ≤ 50 ms) sono dunque raggiunti con margine.
- **Il costo della convergenza (v0.6.1, banco 10k, contro `fase-v0.5.3`):** l'inserimento costa il **26–30% di tempo in più** (3192 → 2523 note/s), sopra il tetto del +10% della spec: **NON raggiunta, accettata dall'utente** il 2026-11-16 come prezzo della convergenza. Il tempo va nel commit (riga `ops` più larga, due indici e `op_objects` per operazione), non nella firma (5–12 µs). La ricerca testo+grafo non cambia. Le misure di sola scrittura si lanciano con `ENGRAMMA_SKIP_MODEL_TESTS=1`, che salta i modelli semantici.
- **Engramma è multilingua:** ogni banco e ogni modello si misura per lingua, almeno sulle sei lingue dello stemmer; nessuna soglia vale solo per l'italiano.
- **Correttezza:**
  - test di proprietà (giro del formato senza perdite; registro rigiocato = stesso stato; esportazione → importazione = stesso stato);
  - `engramma verify`: ricostruisce gli indici e li confronta.

## 12. Tabella di marcia

Rivista il 2026-11-14 (prima in `note-private/roadmap.md`, riportata qui alla chiusura della v0.6.1).

**La visione.** Engramma non è solo «la mia memoria su più dispositivi». È anche la **memoria di un'organizzazione**, con un livello di governo come quello di Tailscale:
- un'organizzazione con amministratori;
- persone con ruoli;
- celle personali, che restano private;
- celle aziendali, con permessi decisi da una politica firmata;
- revoca che ruota le chiavi e chiude anche le copie sui dispositivi.

**Principio:** l'autorità sono le chiavi degli amministratori, verificabili offline da ogni copia (come il Tailnet Lock di Tailscale). Un server di coordinamento è facoltativo: distribuisce, tiene l'audit e fa da nodo cieco, ma non è la fonte dell'autorità.

**Limite dichiarato:** nessun sistema cancella il passato; la revoca agisce sul futuro. Si può però decidere **quanto passato finisce sui dispositivi** (modalità di replica) e **per quanto resta leggibile senza contatto** (affitto delle chiavi).

### Fatto

| Fase | Contenuto | Tag |
|---|---|---|
| v0.1 → v0.1.1 | Formato, cella SQLite, registro con HLC e hash, FTS, grafo, quarantena, CLI, MCP in sola lettura | `fase-v0.1`, `fase-v0.1.1` |
| v0.2 → v0.2.1 | Embedding locale (tract, Granite R2 97M), vettori int8, ricerca mista, cambio di modello blu/verde | `fase-v0.2`, `fase-v0.2.1` |
| v0.3 → v0.3.2 | Grafo PPR, registro v2 con payload dimenticabili, `purge`, compattazione, vista markdown, consolidamento | `fase-v0.3` … `fase-v0.3.2` |
| v0.4 → v0.4.6.1 | Modulo Node e integrazione in Homun; disco, RAM, Linux, LongMemEval, α = 0,5 (§ 6.3, § 7, § 11). Slittati: il grafo adattivo e l'audit MCP con ambiti | `fase-v0.4` … `fase-v0.4.6.1` |
| v0.5 → v0.5.1 | Scrittura da altri assistenti: proposte, decisioni, rollback, politiche, revisori, pagina di revisione (§ 8.5) | `fase-v0.5`, `fase-v0.5.1` |
| v0.5.2 → v0.5.2.2 | MCP su HTTP con OAuth 2.1, provato con ChatGPT, claude.ai e Codex (§ 8.6) | `fase-v0.5.2` … |
| v0.5.3 | Domande corte, note nascoste senza impronta, rifiniture HTTP | `fase-v0.5.3` |
| v0.6 → v0.6.1 | Sincronizzazione cifrata fra i dispositivi del proprietario (§ 4.3.1, § 10.1, `docs/sync.md`): identità immutabile delle operazioni, firme verificate all'arrivo, abbinamento in rete (biglietto e codice breve), relay, `sync run/serve`, nodo cieco, rotazione alla rimozione, Linux. L'inserimento costa il 26–30% in più, accettato (§ 11) | `fase-v0.6`, `fase-v0.6.1` |
| **v0.6.2** | **API JS della sincronizzazione**: il ciclo sta nella libreria (`SyncService`) e la CLI ne è il guscio; `Engine.sync()` lo apre a Homun — stato, dispositivi, biglietto (anche nella forma QR alfanumerica, il QR lo disegna l'app), codice, ingresso, condivisione, rimozione, passata, servizio continuo, eventi; SAS da interfaccia che non si può saltare; un lucchetto per radice (`sync.lock`, `SYNC_BUSY`); il guardiano confronta **per cella** (i due casi limite della v0.6.1 chiusi). Contratto JS esteso, mai rotto: i 29 test della v0.4 identici e verdi più i nuovi | `fase-v0.6.2` |
| **v0.6.3** | **La copia di conflitto converge, e le minori**: la scrittura locale riconcilia l'insieme dei perdenti (i testimoni della verifica v0.6.2 chiusi, con l'oracolo differenziale dal registro); l'ascoltatore di `onEvent` non tiene vivo il processo e `close` svuota l'hub; il test di Electron chiude con `app.quit()`; l'artefatto Node si costruisce senza strip (dyld rifiutava il LINKEDIT disallineato dello strip di rustc, l'`arc4random` di v0.6.2 era il paravento); il biglietto in maiuscolo entra, il pid del lucchetto si scrive dopo la flock, il firmatario si arma alla scrittura dopo un cambio di condivisione, il SAS si attende come Future (runtime dell'abbinamento a un filo), i limiti dei test d'arrivo a 5 s | `fase-v0.6.3` |
| **v0.6.4 — La copia di conflitto è durevole** | Difetto di regola della verifica v0.6.3 chiuso: solo il corpo crea conflitti (ascendenza per `base`, vincitore per classe dell'autore), la copia è durevole e si chiude solo con un testimone (operazione sul suo id; scrittura contenente per diff3 rigoroso dalla biforcazione, o discendente), tetto di tre visibili, il perdente d'agente in quarantena e in coda, la mano umana vince anche più vecchia; `conflicts`/`resolveConflict` da JS e CLI; la versione delle regole viaggia nell'ALPN `engramma/sync/2` (`PEER_OUTDATED`); la crescita del database misurata e il corpo tolto dalla riga di derivazione; schema 21 | `fase-v0.6.4` |
| **v0.6.5 — Chi scrive parte da ciò che ha visto** | La correzione della verifica v0.6.4: la `base` di una scrittura locale è la **testa visibile** (derivata, `note_heads`, schema 22, letta con un indice; per la lapide la linea chiusa), il testimone di contenimento giudica da `nca(perdente, testimone)` estremi compresi ed è successivo **nell'ordine totale** (la chiave di classe non lo sposta — trovato dal giudice E4), una copia chiusa con testimone non si riapre mai (monotonia allargata assente→presente con le eccezioni dichiarate); il diff3 in spazio lineare (Myers sopra le 512 righe/lato) con tetto 32 768 righe / 4 MiB: 20 000 righe 1,4 ms e 5,6 MB; il giudice «nessun testo perso» che non legge la base (il modello registra i corpi letti), con i denti provati sul motore v0.6.4; `conflict(copyId)` da JS e CLI (la quarantena si legge lì); D5 misurato sulla flotta sequenziale (crescita 6,2% al 10% di conflitti, soglia 15); ALPN `engramma/sync/3` e `mailbox/3` | `fase-v0.6.5` |
| **v0.6.6 — La monotonia torna, e nessuna proprietà sparisce più in silenzio** | Fase piccola, di prova: la proprietà di monotonia `no_copy_closes_without_a_witness` rimessa accanto al giudice del testo (il difetto della caccia lunga: `434759c` l'aveva sostituita, il REPORT della v0.6.5 diceva «quattro proprietà verdi» su tre), denti provati sulla regola del testimone della v0.6.4 (seme e casi nel REPORT); l'**inventario delle proprietà** `properties.txt` col cancello `properties_inventory` (una proprietà che manca, in un verso o nell'altro, fa fallire la suite) e `scripts/caccia.sh` che legge l'inventario e gira a priorità minima; la purga non ottimizza l'indice nascosto che non ha toccato (contato prima dell'operazione, l'oblio della quarantena intatto); nessun cambio alle regole dei conflitti né alla derivazione, ALPN fermo a `sync/3` | `fase-v0.6.6` |
| **v0.6.7 — L'oblio è esatto, e la lapide si porta via i suoi conflitti** | La correzione dei tre difetti della verifica v0.6.6: il **bit sporco** dell'indice nascosto (una riga che ne esce lascia le radici nei segmenti; il bit, accesso da ogni delete, chiude il cancello della purga — G1); il **testimone non dipende dalla lapide** e le copie di una nota cancellata seguono la lapide (invisibili, non cercabili, fuori da `conflicts()`, `conflict(copyId)` dice `originalDeleted`; il giudice perde l'eccezione larga — G2); lo scrub dell'**istantanea** chiude come la purga viva, la purga in arrivo scrubba le istantanee del ricevente, e **`scrubbed` si guadagna** leggendo il file (G3); la purga domina anche nel rigioco (una put dopo una purge non ricrea la nota); la caccia si allarga (transizioni di stato nel generatore, la proprietà a due dispositivi `purge_with_conflict_copies_leaves_no_roots`, inventario a 26); ALPN `engramma/sync/4` e `mailbox/4`; due difetti preesistenti del verify trovati dalla proprietà nuova e dichiarati aperti nel REPORT | `fase-v0.6.7` |
| **v0.6.8 — Il cestino ha una porta sola, e `verify` dice il vero sulle celle condivise** | La correzione dei cinque difetti della verifica v0.6.7 più il difetto aperto dei bordi «mentions»: il **cestino ha un predicato solo** (un frammento unico guidato dal tavolo piccolo, ogni lettura lo parla; la forma scelta dalla misura: la vista e il correlato bocciati dal banco; il test di contratto e il cancello dei bucket di `Cell`); il **primo strato** del rigioco da istantanea è chiuso (i corpi delle copie traslocano nella riga di derivazione prima che il taglio dimentichi i payload), il **secondo — l'ancora della rimaterializzazione che invecchia col taglio — resta aperto**, dichiarato; `scrubbed` si guadagna su tutti e due i file e conta le righe FTS orfane per rowid; la monotonia ha un **oracolo di riferimento** (il giudice non chiama più il motore); `SyncService` si chiude davvero (`Drop` che annulla gli inviti e aspetta i fili: le `.tmp*` non tornano); il bordo `conflitto` conta come derivato; le **celle d'oro** nascono qui. ALPN fermo a `sync/4` (nessun cambio di derivazione) | `fase-v0.6.8` |

### Il criterio di release (deciso con la v0.6.8)

- **Engramma locale, senza sync:** si può distribuire dentro Homun dopo
  una fase verificata e consegnata dal revisore, con le celle d'oro al
  loro posto. La v0.6.8, se passa la verifica, è la prima.
- **La sync agli utenti:** si apre quando **una fase intera**, con la
  caccia completa e almeno una proprietà nuova o allargata, **non trova
  nessun difetto nuovo** nel motore, e dopo la fase di release candidate
  (v0.6.9): migrazioni dalle celle d'oro, crash e mancanza di corrente
  nei punti scelti, revisione di sicurezza della sync, rapporto di
  diagnosi esportabile dall'utente. Poi due–quattro settimane d'uso vero
  su dispositivi di prova.

### Davanti

| Fase | Contenuto | Dipende da | Note |
|---|---|---|---|
| **v0.7 — Organizzazioni** | Identità a tre livelli (organizzazione → persona → dispositivo); la **politica come codice firmata**: gruppi, celle per spazio di nomi, concessioni lettore/proponente/scrittore, chi può invitare e condividere cosa, approvazione dei dispositivi, scadenza delle chiavi; **celle personali** (l'organizzazione non ha le chiavi) contro **celle aziendali**; ruoli imposti dalla distribuzione delle chiavi (lettura) e dalla verifica all'arrivo (scrittura e proposta); **revoca centrale** di una persona con rotazione; audit firmato delle decisioni; la condivisione fra due privati come caso minimo | v0.6.3 | Prima una fase di **ricerca**: Tailscale (grants, Tailnet Lock, approvazione dei dispositivi), capacità firmate (UCAN, Biscuit, Keyhive), cifratura di gruppo (MLS e simili), SSO/OIDC per legare le persone. Poi la bozza nel DESIGN, poi la spec. Decisioni già prese: § 14 |
| **v0.7.1 — Protezione a riposo e revoca dei dispositivi** | **Cifratura a riposo** delle celle aziendali, file intero con FTS, vettori e WAL; **chiave in affitto** con scadenza fissata dalla politica e rinnovo a ogni contatto; **cancellazione crittografica** alla revoca (online) o blocco alla scadenza (offline); chiavi nel portachiavi del sistema o nel chip sicuro (Secure Enclave, TPM), anche `device.key`; **replica parziale** (per tag, ambito o periodo) | v0.7 | La replica parziale va progettata insieme alla convergenza (filtrare per oggetto senza rompere la frontiera stabile). Limite dichiarato: protegge da dispositivo perso e dall'ex dipendente comune, non da chi si prepara apposta |
| **v0.8 — `engramma serve` e pannello di controllo** | API, autenticazione legata all'SSO, molte celle in un processo (§ 9); **pannello di controllo facoltativo**: distribuzione della politica, console di amministrazione, **audit delle letture**; celle **solo online** (nessuna copia sui dispositivi, ogni lettura passa dal server e dall'audit); nodo cieco | v0.7.1 | Candidati: elicitation in modalità URL con passkey (§ 8.6), approvazione OAuth sulla pagina di revisione o nella console |
| **v0.9 — Cluster** | Posizionamento con Raft, router, replica, spostamento delle celle, backup su storage a oggetti (§ 9) | v0.8 | |
| **Poi** | Python, Swift, Kotlin, WASM; messaggi A2A sul canale cifrato | | |

### Binario parallelo: decisioni e qualità

Da inserire dove c'è spazio, perché non dipende dalle organizzazioni.

- **Decision model:**
  - prima una fase di **sola misura**: decisioni tipizzate (supporto di un'affermazione per `memory_check`, revisione delle proposte, doppioni e contraddizioni del consolidamento, tipo di domanda per un grafo adattivo), banchi d'oro **multilingua** con regole d'accettazione per lingua, modelli piccoli aperti confrontati con un LLM grande come insegnante;
  - poi, solo se serve, un encoder multilingue piccolo (100–400 milioni di parametri) addestrato dall'insegnante, in ONNX su tract;
  - non è mai l'unico cancello di sicurezza.
- **RAM:** l'embedding in un processo separato (la soglia di 700 MB a riposo del § 7 non è mai stata raggiunta).
- **Windows:** mai provato.

## 13. v0.1 nel dettaglio (storico)

Il primo prompt per GLM, tenuto com'era. Le fasi successive stanno nelle spec in `note-private/prompts/` e nel REPORT.

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

Chiuse lungo la strada:

1. ~~Il nome~~ → **Engramma** (deciso 2026-11-05; il pacchetto npm sarà `@engramma/core`).
2. ~~**Campi in inglese** nel formato~~ → adottati dalla v0.1, confermati.
3. ~~**Il modello di embedding predefinito**~~ → **Granite R2 97M** (v0.2, misurato; § 7). Il 311M resta un'opzione. Runtime (tract) deciso 2026-11-05.
4. ~~**Indice vettoriale**~~ → `FlatI8` in Rust: **sqlite-vec scartato** (un'estensione caricata a runtime, da tenere in passo con SQLite e da firmare diversamente in Electron; la scansione int8 nostra basta fino a ~1M vettori, § 11).
5. ~~**L'istantanea di compattazione**~~ → facoltativa (`--keep-snapshot`): la compattazione base è un punto di controllo senza seconda copia; l'istantanea serve a chi vuole un file coerente da portare via (v0.3.2).
6. ~~**L'allocatore degli artefatti finali**~~ → mimalloc nel `.node` e nel binario CLI, librerie neutre (v0.4.4, § 7); **per piattaforma** dalla v0.4.5 (macOS sì, Linux no: glibc trattiene meno a riposo — § 7).
7. ~~**α della fusione convessa**~~ → **0,5** (v0.4.5: due banchi + conferma su domande tenute da parte, § 6.3 e § 11; la storia 0,6 → 0,7 → 0,5 sta nel commento di `Fusion::default`).
8. ~~**Il costo d'inserimento della convergenza**~~ → accettato (2026-11-16): +26–30% di tempo contro il tetto del +10% (§ 11).

Chiuse dall'utente il 2026-11-14, per la v0.7:

9. **Autorità delle organizzazioni:** solo chiavi degli amministratori, verificabili offline da ogni copia. Un server, quando c'è (v0.8), distribuisce e tiene l'audit ma non decide.
10. **Identità delle persone:** chiavi nostre nella v0.7. L'SSO aziendale (OIDC) arriva dopo, come modo di legare una persona già esistente a un account aziendale: con `engramma serve` nella v0.8 o in una fase sua. Il modello della v0.7 deve lasciargli il posto, senza dipenderne.
11. **Celle personali:** illeggibili dall'organizzazione **per costruzione**. L'organizzazione non riceve mai le loro chiavi, né la revoca né la cancellazione remota le toccano, e nessuna politica può cambiarlo.
12. **Chiavi dell'organizzazione e recupero:**
    - **radice a quorum, fredda:** per esempio 2 su 3 chiavi hardware (YubiKey o simili) in mani o luoghi diversi, firma multipla e mai un segreto ricostruito; usata solo per cambiare gli amministratori; copia d'emergenza su carta o metallo facoltativa, mai in cloud;
    - **chiavi operative degli amministratori** nel chip sicuro (Secure Enclave, TPM) o su chiave hardware, non esportabili, con presenza della persona a ogni firma e **certificato a scadenza** (per esempio 90 giorni) firmato dalla radice;
    - **ritardo con veto** sugli atti della radice (nuovo amministratore, concessioni a tutta l'organizzazione): in vigore dopo per esempio 72 ore, notificati a ogni dispositivo, annullabili da qualunque altro amministratore;
    - ogni dispositivo che firma **mostra in parole chiare cosa firma**, e la conferma passa da un canale diverso;
    - la **politica è una catena** con versione e hash: nessuna copia accetta una versione più vecchia, e le revoche ne fanno parte;
    - **modo semplice** per l'organizzazione di una persona: un amministratore nel Secure Enclave, la radice su una YubiKey più un kit di recupero stampato;
    - **firme P-256 accanto a Ed25519:** il Secure Enclave firma solo P-256. Ed25519 resta per i dispositivi e iroh;
    - **dichiarato:** se si perdono tutte le chiavi, radice compresa, la politica non si cambia più. Quello che c'è vive fino alla scadenza degli affitti, poi si rifonda l'organizzazione. La compromissione della radice non apre comunque le celle personali.

Ancora aperte:

13. **Dove vive:** repository privato o pubblico su GitHub?
14. **Cifratura a riposo:** SQLCipher o cifratura di pagina. Si chiude nella v0.7.1.
15. **Windows:** non provato (macOS arm64 provato, Linux arm64 provato in Docker, x86_64 e Windows no; i punti sensibili — file aperti, nomi riservati, separatori — sono nel REPORT v0.4.4).
16. **Il rapporto con Homun:** il motore resta un progetto separato, con le sue versioni. Homun fissa una versione precisa (il lock con gli hash dei tre file). La v0.6.1 non va agganciata: a Homun serve la v0.6.2.

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
