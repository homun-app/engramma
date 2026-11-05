# REPORT — Engramma v0.1

Data: 2026-11-05. Base: `main` @ `e018ca5`. Tutto costruito e provato offline
(`cargo --offline`), Rust 1.95.0, macOS arm64 (Apple Silicon). Nessuna crate
aggiunta oltre a quelle già fissate in `Cargo.lock`; il lockfile è invariato.

## Cosa è stato fatto, punto per punto

### 1. Formato (`engramma-core::format`, spec prima del codice)

- `spec/format-v0.md` scritta **prima** dell'implementazione: campi, tipi,
  valori ammessi, regole per collegamenti/relazioni/tag, forma canonica di
  serializzazione, regola dell'hash del registro.
- `spec/examples/valid/` (6 file) e `spec/examples/invalid/` (8 file, il nome
  dice perché): usati come test in `crates/engramma-core/tests/props.rs`
  (`spec_examples_valid_and_invalid`).
- Intestazione YAML con `saphyr`: chiavi `x-` conservate **nell'ordine**
  (in memoria e nel JSON canonico degli op, come array di coppie, perché gli
  oggetti JSON sono ordinati per chiave); chiave sconosciuta senza `x-`:
  errore in stretta, avviso (e chiave scartata) in importazione.
- Date ISO 8601: `valid_*` data o data-ora, `*_at` solo data-ora UTC al
  secondo. Le date coprono il giorno intero anche a confronto con data-ora.
- Corpo: collegamenti `[[Nome]]`/`[[Nome|testo]]` estratti dagli **eventi**
  di `pulldown-cmark` (i collegamenti arrivano spezzati in più eventi `Text`:
  si accumulano in «corse» di testo e si analizzano a fine corsa), ignorati
  nei blocchi di codice e nel codice in riga; `#tag` nel corpo ma non nei
  titoli; relazioni `- rel:: [[Entità]] (from … to …)` nella sezione
  `## Relations` (titoli di livello 1–2).
- Giro senza perdite: `parse(serialize(n)) == n` e `serialize(parse(x))`
  stabile, provati con proptest su note generate (titoli, corpi, tag,
  estensioni `x-` annidate, timestamp).
- Scoperta dei proptest che ha guidato la specifica: gli scalari YAML vanno
  quotati se iniziano con un indicatore (`'`, `[`, `-`, …), se contengono
  `": "`/`" #"` o trailing `:`, se sembrano numeri **anche in base 16/8/2 o
  con zero iniziale** (`0x10` è un intero nello schema core di YAML), e i
  caratteri di controllo richiedono il quoting doppio (il singolo piega i
  newline). Tutto questo è ora §5.4 della specifica.

### 2. Cella (`engramma-core::cell`)

- Schema del §4.2 com'è, senza `notes_vec`; migrazioni numerate in
  `meta.schema_version` (v1 = schema, **v2 = indice `edges(note_id)`**, vedi
  sotto); pragmas `journal_mode=WAL`, `foreign_keys=ON`, `synchronous=NORMAL`.
- Una scrittura per volta: lock di processo per file di cella (registro
  globale di mutex per percorso) **più** `BEGIN IMMEDIATE`.
- Operazioni: `put_note`, `patch_note`, `set_status`, `supersede`,
  `delete_note` (lapide), `link`, `unlink`, `confirm` — più `restore` e
  l'`init` della cella. Ogni operazione è **una riga in `ops` e la sua
  applicazione nella stessa transazione** (l'inserimento della riga è dentro
  `apply`, così il replay la riproduce identica, `seq` esplicito compreso).
- `apply(state, op)` è deterministica: niente orologi, casualità o I/O; ID,
  HLC e timestamp si generano alla creazione dell'operazione e viaggiano nel
  payload.
- HLC fatto a mano: `(millisecondi fisici, contatore, id nodo)`, confronto
  strutturale, forma testuale a larghezza fissa; test con orologio che torna
  indietro (proptest su letture arbitrarie).
- Hash concatenato: `hash = blake3(prev_hash || JSON canonico)`, chiavi
  ordinate a ogni livello e nessuno spazio (le mappe `serde_json` sono
  BTreeMap); genesi = 64 zeri. `verify_log` è la prima metà di `verify()`.
- Determinismo (il test più importante): dopo serie casuali di operazioni
  (proptest), la ricostruzione in memoria rigiocando `ops` dà le stesse
  tabelle riga per riga — `notes`, `note_versions`, `tags`, `note_tags`,
  `entities`, `edges`, `supersedes`, `ops`, `meta` — confronto incluso nel
  test `replay_equals_state`. I rowid interni di FTS restano fuori (come da
  testo); nota sotto sul perché non si può fare di più con FTS.
- Versioni: ogni modifica salva l'istantanea precedente in `note_versions`;
  `restore(id, version)` è a sua volta un'operazione.
- `supersede`: `valid_to` alla vecchia dalla `valid_from` della nuova (o
  dall'override), marca `superseded`, scrive `supersedes`.
- Quarantena: `external` nasce sempre `quarantined` (il `put` lo impone);
  `confirm` la porta ad `active` registrando l'attore; nessuna operazione può
  portare una nota `external` ad `active` se non `confirm` (guardia in
  `write_note`; l'importazione ricostruisce lo stato confermato con
  `put`+`confirm`, vedi scelte).
- Entità: nascono da collegamenti e relazioni, nome senza distinzione di
  maiuscole (prima grafia canonica); gli archi ereditano validità e
  affidabilità dalla nota che li dichiara, con l'override `(from … to …)`
  della riga di relazione.

### 3. Testo

- `body_norm`: minuscole → diacritici del latino tolti con **tabella scritta
  a mano** (Latin-1 Supplement + Latin Extended-A, nessuna crate Unicode) →
  radici Snowball nella lingua della nota (`whatlang` su titolo+corpo;
  it/en/fr/de/es/pt, altrimenti niente riduzione). `lang:` vince sul
  rilevamento. Il titolo entra in `body_norm` (la colonna FTS `title` resta
  grezza: senza stemming non matcherebbe le query ridotte; documentato).
- La domanda passa dalla stessa normalizzazione, con **stopword it/en**
  filtrate prima **e dopo** lo stemming («è» → «e»), e in più una frase
  adiacente degli stem per premiare l'adiacenza dei nomi. A lingua ignota
  ogni token si cerca nella forma grezza più gli stem delle sei lingue.
- Test dorati di radice: traslocato/trasloco/traslochi si trovano a vicenda,
  città/citta pure (in `text.rs` e nel vault).

### 4. Ricerca (`engramma-core::search`)

- Pipeline §6.3 senza vettori: BM25 di FTS5 con **pesi di colonna**
  (titolo 10, corpo 3, tag 2), 50 candidati → espansione nel grafo
  (`graph_hops` 0–2, archi validi in `as_of`) → filtri → riordino
  `bm25_norm × copertura × w_conf × w_recency × w_grafo`.
- **Fusione RRF**: funzione `rrf_fuse` scritta e testata (con una lista
  mantiene l'ordine; con due vince la sovrapposizione). Nel riordino v0.1 si
  usa `bm25_norm` come chiede il prompt; l'RRF entra in v0.2 con la seconda
  lista.
- Pesi predefiniti in `Weights` documentati: `stated 1.0, inferred 0.8,
  external 0.5`; semi-vita recenza 180 giorni; un passo nel grafo 0.6, due
  0.36 (per le note scoperte dal grafo si applica un passo extra di decadimento:
  nessuna corrispondenza testuale è già un gradino di evidenza in meno).
- `explain` per risultato: termini trovati, posizione BM25, percorso nel
  grafo (`[[Giulia]] -works_at-> [[Studio Rossi]]`, con le note renderizzate
  `(nota <id8>)`), affidabilità, validità, fonte.
- Filtri: tipi, tag **con figli**, scope, `as_of`, affidabilità minima,
  quarantena (mai di norma), sostituite (solo se erano la verità in `as_of`),
  validità di norma (ciò che vale «oggi»; gli **eventi non scadono**: il loro
  `valid_to` è quando sono finiti, non quando hanno smesso di essere veri).
  I risultati senza segnale (punteggio zero) sono trattenuti: «nessun
  risultato sopra soglia».
- Le altre interrogazioni del §6.4: `get`, `history`, `neighbors`,
  `path` (cammino minimo, ≤ 4 passi), `tags` (prefisso con figli e conteggi),
  `entities`, `timeline` (note su un'entità per validità; niente lapidi né
  quarantena), `changes_since`.

### 5. Import/export

- Export: `<dir>/<type>/<slug>-<id8>.md` (+ suffisso `-2`, `-3`… se slug e
  id8 collidono) e `<dir>/engramma.json` con `format_version`, `cell`,
  `exported_at`, `note_count`. Lapidi mai esportate, supersederi sì.
- Import Engramma: stesso stato; test di proprietà export → import con
  confronto nota per nota e arco per arco (degli archi delle note vive: le
  lapidi non si esportano, quindi i loro archi cancellati non fanno parte
  dello stato esportato).
- Import Obsidian: intestazione generata (`id`, `type: note`,
  `source: import:obsidian/<percorso>`, `confidence: stated`, `created_at`
  dal file), cartelle nascoste e non-markdown saltate, **symlink mai seguiti
  e mai oltre la radice** (il walker usa `symlink_metadata`), riepilogo di
  importate/saltate/errori. I file con intestazione Engramma valida entrano
  come note Engramma (per questo il vault dorato si importa con `--obsidian`).

### 6. CLI `engramma`

Tutti i comandi richiesti: `init`, `cell list|create|drop`, `add` (file o
stdin, `--source`/`--confidence` obbligatori), `get`, `edit` (`$EDITOR` su un
temporaneo, differenza applicata come `patch_note`; id e `created_at`
immutabili), `rm`, `confirm`, `supersede`, `search`, `neighbors`, `path`,
`tags`, `entities`, `timeline`, `history`, `restore`, `import`, `export`,
`verify`, `mcp`. Opzioni comuni `--cell` (predefinita `default`) e `--json`
(schemi documentati in `docs/cli.md`). Prefissi di id risolti (≥ 4 caratteri,
ambiguità segnalata). Codici di uscita: 2 uso, 3 nota non trovata, 4 interno.
Temporanei in `$TMPDIR/engramma-edit/`.

### 7. `engramma mcp` (stdio, sola lettura)

- `--cells a,b`: espone solo quelle; senza il parametro rifiuta di partire
  con un errore esplicito.
- Strumenti `memory_search`, `memory_get`, `memory_neighbors`,
  `memory_tags`, `memory_timeline`, descrizioni in inglese scritte per un
  modello (cosa restituiscono, quando usarle, che la quarantena non compare e
  perché), schemi `schemars`, ogni risultato con i campi del `explain` e la
  `source`.
- Test d'integrazione (`crates/engramma-cli/tests/mcp.rs`): avvia il binario
  come sottoprocesso e parla JSON-RPC su stdio (`initialize` → `tools/list`
  → `tools/call memory_search`), verifica che una cella non esposta sia
  irraggiungibile **anche chiamandola per nome** e che senza `--cells` esca
  con errore.
- README: configurazioni per Claude Desktop e `claude mcp add`.

### 8. Test

- Unitari per modulo (format, yaml, body, text, hlc, id, schema, cell, ops,
  search, port): **58** nella lib.
- proptest (`tests/props.rs`): giro del formato, registro rigiocato = stesso
  stato, export → import = stesso stato, HLC monotono con orologio impazzito.
- Dorati: `tests/golden/vault-it/` — **62 note in italiano** scritte a mano
  (Fabio e Giulia: trasloco Roma→Milano settembre 2026, lavori, colleghi,
  preferenza alimentare che cambia, viaggio a Torino, due voci dal web in
  quarantena) e `tests/golden/queries.json` — **30 domande** con risposta
  attesa sulle categorie richieste (fattuali, temporali, aggiornamento,
  multi-salto, quarantena, astensione). `golden_files_are_canonical` prova
  che ogni file del vault è già in forma canonica.
- `engramma verify` sulla cella dorata: nel test `golden_vault_imports_and_verifies`
  (e provato a mano, sotto).
- Totale: 69 test (58 lib + 4 dorati + 5 proprietà + 2 MCP), tutti verdi
  anche `--release`.

### 9. Benchmark (`bench/`, binario `engramma-bench`)

- Generatore deterministico (xorshift a seme fisso): persone con nome
  univoco, città, aziende, progetti, storie di domicilio/lavoro/cibo che
  cambiano nel tempo (catene di supersede), tag gerarchici, riempitivi
  realistici; ~330 domande a 10k note e ~2 030 a 100k con la nota attesa
  (fattuali, temporali con `as_of`, aggiornamento, multi-salto).
- Misure: inserimento (note/s), apertura cella, p50/p95 di ricerca
  testo+grafo, **p50 solo testo** (per profilazione), richiamo@5, MRR,
  dimensione file dopo checkpoint del WAL. `bench/RESULTS.md` rigenerato
  dall'ultimo giro (numeri sotto).

### 10. Contorno

- `deny.toml` con l'elenco di licenze richiesto; `cargo deny check licenses
  bans` pulito **offline** (le advisory richiedono rete: saltate e dichiarate).
- `.github/workflows/ci.yml`: macOS arm64 (`macos-latest`) e x86_64
  (`macos-15-intel`), Ubuntu, Windows; fmt, clippy `-D warnings`, test, deny
  (licenses+bans). **Scritta e dichiarata non provata** (nessun runner qui).
- `README.md` in inglese (cos'è, installazione da sorgente, esempio completo
  da `init` a `search`, MCP con Claude Desktop e Claude Code, stato
  «v0.1, formato instabile»), `docs/cli.md` (tutti i comandi con gli schemi
  JSON), `docs/architecture.md` (moduli, flusso di una scrittura e di una
  ricerca, confini voluti della v0.1).

## Scelte che ho dovuto prendere, e perché

1. **Soggetto delle relazioni.** La sintassi `- lives_in:: [[Milano]]` non
   dice chi è il soggetto. Regola v0.1 (in specifica): il soggetto è
   l'**entità-titolo** quando la nota è di tipo `person|place|project|event|
   preference` e il titolo è un nome semplice; altrimenti la nota stessa
   (`src = note:<id>`). Così «dove lavora il partner di Fabio» funziona
   mettendo le relazioni nelle note persona (come nel vault dorato), senza
   estendere la sintassi del formato.
2. **`external` + `active` nel file.** Dopo `confirm` una nota esterna è
   attiva: il file deve poterlo rappresentare (altrimenti l'export di una
   cella confermata non reimporterebbe). Il parser accetta dunque
   `external` con qualunque stato; è la **cella** a imporre la nascita in
   quarantena e la promozione solo via `confirm`. L'import ricostruisce lo
   stato confermato come `put` + `confirm` (che timbra un proprio
   `updated_at`: per questo il test export→import confronta le note campo
   per campo tranne `updated_at`).
3. **Riordino v0.1.** Con una sola lista, l'RRF comprime i ranghi (un bm25
   5.0 contro 0.0 diventa 0.0164 contro 0.0161) e la recenza spazzerebbe via
   i fatti «vecchi ma veri». Il riordino usa `bm25_norm` come da testo del
   prompt, più un **fattore di copertura** (0.5 + 0.5 × termini matchati) e
   un passo extra di decadimento per le note scoperte dal grafo: i vicini di
   grafo sostengono, non sostituiscono, le corrispondenze testuali.
   Misurato sul benchmark e sul vault dorato.
4. **Eventi che non scadono.** Con `valid_to` = fine dell'evento, chiedere
   «com'è andato il viaggio» a novembre non deve escludere l'evento di
   ottobre. I tipi `event` non vengono filtrati per validità scaduta senza
   `as_of` (con `as_of` la finestra vale). È una semantica da ripensare in
   v0.2.
5. **FTS external-content.** La colonna `tags` di `notes_fts` non ha una
   colonna gemella in `notes` (lo schema del §4.2 è fatto così): per questo
   `rebuild` e `integrity-check` di FTS non vengono mai eseguiti (andrebbero
   a leggere `notes.tags`, inesistente). Delete e insert sono sempre
   appaiati con i valori esatti (i tag in ordine deterministico); la
   verifica passa dal confronto delle tabelle logiche nel replay.
6. **Indice `edges(note_id)` (migrazione v2).** Lo schema del §4.2 non lo
   prevedeva ma la reindicizzazione di ogni nota cancella gli archi derivati
   per `note_id`: senza indice, a 100k note l'inserimento crollava a ~190
   note/s (misurato). Con l'indice: ~2 500/s. Migrazione additiva,
   retrocompatibile.
7. **Espansione del grafo limitata.** Le entità-hub (una città citata da
   centinaia di note) trascinavano dentro tutto il quartiere: la camminata
   usa query indicizzate su `src/dst` e prende al più 400 archi per livello,
   in ordine di id (deterministico). p95 a 100k: da ~350 ms a ~65 ms,
   richiamo invariato.
8. **Nome del flag per il vault dorato.** `engramma import <cartella>` senza
   manifest è ambiguo (una cartella Obsidian può contenere note che
   sembrano Engramma): la cartella dorata si importa con `--obsidian`, che
   accetta comunque le note già intestate.

## Crate che avrei voluto (e non ho aggiunto, come richiesto)

- `unicode-normalization` per una NFKD vera invece della tabella latina a
  mano (la tabella copre italiano/inglese/francese/spagnolo/tedesco/portoghese;
  altre scriture restano accentate nello `body_norm` — il tokenizzatore FTS
  con `remove_diacritics 2` rimargina lo scarto).
- `rustc-hash` o `indexmap` per le mappe calde della ricerca (guadagni da
  misurare, non necessari per gli obiettivi).
- `criterion` per il microbenchmark (il rapporto usa `std::time::Instant`,
  sufficiente per p50/p95 su centinaia di query).

## Numeri del benchmark (macOS, Apple Silicon, release)

| note | inserimento (note/s) | apertura (ms) | ricerca p50 (ms) | p95 (ms) | solo testo p50 (ms) | richiamo@5 | MRR | file (MB) |
|---|---|---|---|---|---|---|---|---|
| 10 000 | 3 159 | 0,7 | 4,8 | 6,6 | 1,8 | 0,938 | 0,858 | 24,2 |
| 100 000 | 2 519 | 2,0 | 46,8 | 64,7 | 25,2 | 0,896 | 0,759 | 246,2 |

Contro gli obiettivi del §11:

- inserimento ≥ 2 000 note/s: **rispettato** (3 159 a 10k, 2 519 a 100k),
  con una `put_note` per nota (una transazione ciascuna, HLC, hash,
  reindicizzazione).
- ricerca testo+grafo p95 ≤ 30 ms: **rispettato a 10k (6,6 ms), mancato a
  100k (64,7 ms)**. Dove va il tempo (misurato con la colonna «solo testo»):
  ~25 ms sono FTS+caricamento dei ~90 candidati (una decina di letture per
  nota: frontmatter, corpo, body_norm, tag), ~20 ms la camminata del grafo
  (due livelli di query sugli archi). Le vie ovvie: una sola lettura per
  candidato (JOIN invece di query separate), candidati ridotti (tagliare a
  25+15) e cache dei percorsi. Nessun trucco ai dati: i numeri sono quelli
  misurati.
- apertura cella ≤ 50 ms: **rispettato con margine** (0,7–2,0 ms).
- Il file di 100k note pesa 246 MB: il registro delle operazioni con l'intero
  payload canonico è la parte grossa; la compattazione delle istantanee è
  prevista in v0.3 e qui si sente.

## Prove a mano (richieste dalla chiusura)

`engramma init --root <tmp>` → «inizializzato»; `import` del vault dorato
(`--obsidian`) → «importate: 62 / saltate: 0 / errori: 0»; `search "dove
abitava Fabio" --as-of 2026-08-15` → le note sostituite di agosto in cima
(«Giulia abitava a Roma» e «Fabio abitava a Roma», validità fino al
2026-08-31); `search "dove abita Fabio"` senza e con
`--json` → in cima «Fabio abita a Milano» (termini `abit, fab`, rango BM25 4,
validità 2026-09-01 → ora, fonte `user`); `neighbors Giulia` → i quattro
archi tipizzati (`partner_of`, `lives_in`, `works_at`, `sister_of`) più i
mention; `timeline Fabio` → la storia in ordine di validità (TaskForce →
Studio Rossi, Roma → Milano, vegetariano → onnivoro), quarantena esclusa;
`verify` → «registro verificato: 63 operazioni, catena hash e ricostruzione
a posto». (Uscite complete nelle note di lavoro della sessione.)

## Cosa non ho potuto provare

- **Windows e Linux**: compilati solo per il target locale (macOS arm64).
  La CI li copre, ma la CI stessa non è mai stata eseguita (workflow scritto
  e dichiarato non provato).
- **Advisory di `cargo deny`**: richiedono la rete; eseguito solo
  `licenses bans`, come da vincoli.
- **Rust 1.95 su altri target**: nessuna verifica che `rmcp`/`rusqlite`
  compilino identici su x86_64/Windows (la matrice CI lo farà).

## Domande aperte per la v0.2

1. **Soggetto delle relazioni**: la regola «titolo = nome» copre i casi del
   vault, ma una relazione in una nota `fact` resta appesa alla nota. Vale
   la pena estendere la sintassi (es. `from [[Fabio]] lives_in:: [[Milano]]`)
   o lasciare la semantica alle note persona?
2. **Validità degli eventi**: oggi gli eventi non scadono. Serve un modello
   esplicito (stato vs occorrenza) prima che i tipi di nota si moltiplichino?
3. **Recenza**: mezzo-vita a 180 giorni punisce i fatti correnti «vecchi»;
   con gli embedding in arrivo, tarare di nuovo i pesi sul benchmark (come
   chiede il §6.3.5) e considerare `max(updated_at, valid_from)`.
4. **Astensione**: oggi «nessun risultato» vale quando nessun termine
   matcha. Una soglia vera (punteggio minimo assoluto) richiede prima la
   taratura su misura con la ricerca semantica.
5. **FTS `tags` senza colonna gemella**: chiedere se correggere lo schema
   (colonna `tags` in `notes`, o FTS contentless) in v0.2, per poter usare
   `rebuild`/`integrity-check` invece del solo replay.
6. **Registro**: 246 MB per 100k note; oltre alla compattazione (v0.3),
   valutare se il payload debba portare l'intera nota o un diff.
