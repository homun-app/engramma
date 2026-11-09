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

---

# REPORT — Engramma v0.1.1 (correzioni della verifica v0.1)

Data: 2026-11-05. Base: `main` @ `81b9f9f` (tag `fase-v0.1`). Un commit per
punto, sempre con il test che fallisce prima della correzione (per due punti
la dimostrazione del fallimento è stata fatta disabilitando temporaneamente
la correzione e mostrando il rosso, vedi sotto). Tutto offline, Rust 1.95,
nessuna crate nuova (unica variazione: la feature `hooks` di `rusqlite`, che
compila più codice della stessa crate — `Cargo.lock` invariato).

## Cosa è stato fatto, punto per punto

### P0.1 — la quarantena non trapela più

- `search`: i semi dell'allargamento nel grafo sono solo i candidati che
  superano i filtri (stato, quarantena, sostituite, `as_of`); gli archi
  dichiarati da note in quarantena o cancellate non si attraversano (salvo
  `include_quarantined`). Il riordino riusa le note già lette.
- `neighbors`, `path`, `timeline`, `tags`: di norma escludono archi e conteggi
  delle note dichiaranti in quarantena o cancellate (gli archi manuali senza
  nota restano sempre); nuovo parametro `include_quarantined`, CLI
  `--include-quarantined` sui quattro comandi.
- MCP: `memory_get` su una nota in quarantena o cancellata risponde con lo
  stesso identico errore di una nota inesistente (nessun oracolo); la CLI
  `get` la mostra, con la riga «⚠ stato: …».
- Domande dorate 31–33 (`campione`; `gara regionale crossfit`; `Foster
  Londra`) con `expect_empty`: tutte e tre fallivano prima (dieci note su
  Fabio con punteggio ~1,0). Il banco ora controlla anche che nessun `via`
  attraversi una nota nascosta (attivo da P1.9 in poi, quando gli id corti
  sono diventati discriminabili). «offerta Foster Partners Londra» non è
  stata usata: «partners» condivide lo stem «partner» con le relazioni
  `partner_of` di mezza volta attiva.

### P0.2 — SQL parametrico, frontiera in tabella temporanea

- `edges_touching_frontier`: frontiera in `CREATE TEMP TABLE frontier` +
  `CROSS JOIN` (fissa l'ordine: con un JOIN plain il planner scansionava
  `edges` intero per pagare l'ORDER BY a poco — verificato con EXPLAIN QUERY
  PLAN), relazioni come `?`, ordine per id e limite deterministici.
- Il parametro `filter: &str` è scomparso dall'API interna: nessuna funzione
  riceve più frammenti SQL.
- `neighbors` e `path` camminano per livelli con una interrogazione indicizzata
  a livello; `path` si ferma a 4 passi durante la visita.
- Test: relazione `') OR 1=1 --` ed entità `D'Angelo` senza archi estranei;
  filtro `rels` valido in entrambe le direzioni; `path` a profondità > 4 è
  `None` e il conteggio dei passi VDBE (progress handler, feature `hooks` di
  rusqlite) prova che il blob di 20 000 archi non viene toccato — prima
  760 638 passi, ora bounded.

### P0.3 — HLC con più processi

- Dentro la `BEGIN IMMEDIATE`: leggi `meta.hlc`, `clock.observe(…)`, solo
  dopo `tick()`. `now_ms()` da una lettura sola (`unix_timestamp_nanos() /
  1_000_000`).
- Test: due `Cell` sullo stesso file, la seconda con l'orologio iniettato
  (`#[cfg(test)] inject_clock`) in avanti di un'ora; scritture alternate con
  HLC strettamente crescente nell'ordine di `seq`. Prima della correzione
  (dimostrazione con l'`observe` disabilitato) seq 3→4 andava indietro
  di un'ora.

### P0.4 — l'hash vincola l'intera riga del registro

- L'envelope del payload porta anche `note_id`: `op_id`, `hlc`, `actor`,
  `kind`, `note_id` sono tutti nei byte hashati; le colonne sono un indice.
- `check_envelope` in `op_from_row` e in `apply` (il rigioco segue
  l'envelope); `make_op` rifiuta i payload che provano a sovrascrivere un
  campo dell'envelope; `verify` riporta la corruzione nel report invece di
  propagare errori.
- Vettore dorato dell'hash in spec §6.1 e in un test (payload fisso, hash
  atteso, chiavi inserite in ordine sparso): un cambio silenzioso del JSON
  canonico (per esempio `preserve_order` di serde_json) rompe un test, non
  le celle scritte.
- Il test con i due `note_id` scambiati fra due `put_note` passava la
  verifica intera sulla v0.1 (catena valida, rigioco identico): era il buco
  vero; la colonna `kind` manomessa cade pure, ora all'origine.

### P0.5 — una nota sostituita ha la validità chiusa

- Regola in spec §1.8 e nel validatore: `status: superseded` richiede
  `valid_to`; errore in stretta, avviso in importazione, dove la validità si
  chiude alla `valid_from` della nota che la cita in `supersedes` (import in
  due fasi: prima tutto il parse, poi le scritture — la nota che chiude può
  arrivare dopo nel cammino).
- Vault dorato: `060-fabio-vegetariano.md` riceve `valid_to: 2026-09-30`;
  domanda dorata 34 (`cosa mangia Fabio`, `as_of` 2026-10-15, `k` per
  domanda) → solo «onnivoro»; invariante nuova: nessun `superseded` senza
  `valid_to` nella cella dorata. Timeline a mano: «2020-01-06 → 2026-09-30».

### P1.6 — test di proprietà del registro più forti

- Bersaglio sorteggiato fra le note note (prima era sempre `first()`: una
  cancellazione zittiva tutto), 40–60 operazioni per caso, conteggio delle
  operazioni riuscite per tipo, `set_status` finalmente coperto (quarantena
  e rilascio delle note non esterne).
- Test su 256 semi: ogni `OpKind` riesce almeno 20 volte.
- Rigioco incrementale (§9): `ops[..k]` + `changes_since(k)` = rigioco
  completo, per k casuale e ai bordi, su 6 semi × 36 operazioni.

### P1.7 — colonna `tags_text` (migrazione v3) e indice FTS verificato

- `notes.tags_text`, valorizzata da `apply` (tag ordinati, spazio-joined);
  `notes_fts` ricreata su `tags_text` e ricostruita dal contenuto; backfill
  deterministico in Rust dentro la migrazione.
- `verify` esegue l'integrity-check FTS su cella viva e rigioco e confronta
  l'insieme dei rowid indicizzati con le note non lapide. Nota onesta:
  l'integrity-check di SQLite 3.54 su tabelle external-content NON confronta
  indice e contenuto (provato a mano: documento tolto, deriva del testo e
  duplicati passano tutti) — verifica solo la struttura interna; per questo
  il confronto degli insiemi lo facciamo da soli.
- **Bug vero scovato dal controllo nuovo**: il `delete` FTS su un rowid non
  indicizzato (ogni scrittura su una lapide: patch, restore, supersede del
  cancellato) corrompe la contabilità interna di FTS5; l'errore «database
  disk image is malformed» era ingoiato da un `let _` e la riga rifiutava di
  uscire dall'indice una cancellazione dopo. Riprodotto in sqlite3 puro e su
  un seme di proptest. Ora il delete si emette solo per note indicizzate e
  l'errore non si ingoia.

### P1.8 — `debug_column` in sola lettura

- Rifiuta gli statement non read-only (`Statement::readonly()`) e le query
  senza una sola colonna; `#[doc(hidden)]` e dichiarata fuori dal contratto
  pubblico. Prima il `DELETE FROM notes` del test girava davvero (provato
  con la guardia disabilitata).

### P1.9 — id corti = parte casuale

- `id8()` restituisce gli **ultimi 8** caratteri (quelli random: i primi 8
  sono il timestamp e collidono nelle importazioni di massa — nel vault
  dorato tutte le note erano «01GD0000»). Lo usano export, `via`, timeline e
  output CLI umano.
- `resolve_note_id` accetta prefisso **o suffisso** (≥ 4 caratteri) con
  ambiguità segnalata; spec §7 e `docs/cli.md` aggiornati.

### P2.10 — MCP: `cell` facoltativa con una sola cella

- `cell` è `Option` in tutti gli strumenti: con una cella esposta è il
  default; con più celle l'omissione è un errore che le elenca. README e
  descrizioni aggiornate.

### P2.11 — benchmark

- `--notes N` (ripetibile, i numeri nudi restano), `--out PATH` (senza più
  toccare `bench/RESULTS.md` quando è passato), `--runs K` (default 3,
  mediana per metrica). Il parsing degli argomenti è fattorizzato e
  unit-testato.
- Due varianti per dimensione: **viva** (fatti correnti con `updated_at`
  recente, la recenza lavora) e **uniform** (lo stesso `updated_at` per
  tutte: ranking solo testo+grafo). La differenza di MRR a 10k (0,858 contro
  0,810) misura quanto la recenza contribute: è reale ma non enorme.
- **Varianza** (il motivo della mediana): cinque giri singoli a 10k a
  macchina ferma hanno dato 2 761–3 441 note/s, p50 5,1–5,4 ms, p95
  7,8–9,4 ms. Il giro a freddo della verifica aveva dato 1 943 note/s e p95
  19 ms contro i 3 159/6,6 dichiarati dalla v0.1: i numeri della v0.1 erano
  un giro fortunato su macchina calda, non una media. Misurare con build
  concorrenti in corso gonfia vistosamente il p95 (un giro intermedio fatto
  mentre altri test compilavano ha dato p95 129,6 ms a 100k, contro 84,6
  della stessa macchina ferma).
- **Regressione cercata e in gran parte recuperata**: la prima versione
  della visibilità archi (LEFT JOIN su `notes`) portava il p50/p95 a 100k a
  75/130 ms anche a macchina ferma. La visibilità ora usa `edges.status`
  (che rispecchia la nota dichiarante a ogni riscrittura) senza JOIN: il
  divario residuo contro la v0.1 (p95 84,6 contro 64,7) è la tabella
  temporanea e l'UNION contro la vecchia query con letterali: il p95 a 100k
  rimane aperto per la v0.2 come da programma. Nota semantica: un arco
  manuale con `note_id` mantiene lo stato che aveva alla creazione (se la
  nota referenziata viene poi messa in quarantena, l'arco manuale resta
  visibile; gli archi derivati no).

### P2.12 — astensione dorata con parole presenti nel corpus

- Domande 35 («vegetariano») e 36 («backend»), `as_of` 2026-10-15,
  `expect_empty`: le parole esistono solo in due note sostituite con validità
  chiusa (060 e 080). Sulla v0.1 la prima usciva per la validità aperta, la
  seconda per l'allargamento del grafo dalla nota esclusa.

## Numeri del benchmark (macOS, Apple Silicon, release, mediana di 3 giri)

| corpus | note | inserimento (note/s) | apertura (ms) | p50 (ms) | p95 (ms) | solo testo p50 (ms) | richiamo@5 | MRR | file (MB) |
|---|---|---|---|---|---|---|---|---|---|
| viva    | 10 000  | 3 547 | 1,1 | 4,8 | 8,1  | 1,9 | 0,938 | 0,858 | 24,2 |
| uniform | 10 000  | 3 558 | 1,0 | 4,8 | 7,8  | 1,9 | 0,938 | 0,810 | 24,2 |
| viva    | 100 000 | 2 586 | 2,0 | 48,2 | 84,6 | 25,8 | 0,896 | 0,759 | 247,8 |
| uniform | 100 000 | 2 284 | 1,9 | 50,0 | 84,5 | 26,1 | 0,890 | 0,767 | 247,8 |

Contro gli obiettivi del §11: inserimento ≥ 2 000 note/s rispettato
(10k e 100k, entrambe le varianti); apertura ≤ 50 ms rispettata; p95
ricerca ≤ 30 ms rispettato a 10k (8,1 ms), mancato a 100k (84,6 ms) —
resta aperto per la v0.2 insieme alla taratura dei pesi sul ramo dei
vettori; il confronto viva/uniform mostra che il richiamo non dipende in
maniera critica dalla recenza (0,896/0,890 a 100k, 0,938 uguale a 10k).

## Prove a mano (richieste dalla chiusura)

- `search "campione"` → «nessun risultato sopra soglia» ✓ (e anche
  «gara regionale crossfit» e «Foster Londra» si astengono);
- `neighbors Fabio` → nessun arco della nota 0110, `(nota 000000XX)`
  distinguibili ora che gli id corti sono la parte casuale ✓;
- `timeline Fabio` → «2020-01-06 → 2026-09-30  Fabio vegetariano», niente
  più «→ ora» ✓;
- in più: `verify` sulla cella dorata («catena hash, ricostruzione e indice
  FTS a posto»), `get` su una nota in quarantena con l'avviso di stato,
  server MCP a cella unica senza argomento `cell`.

## Scelte e note di questa fase

1. **Visibilità archi su `edges.status`** (vedi P2.11): niente JOIN, a costo
   di un arco manuale con `note_id` che resta visibile se la nota referenziata
   finisce in quarantena. Gli archi derivati (la quasi totalità) seguono
   sempre la loro nota. Da ripensare se gli archi manuali con nota diventeranno
   comuni.
2. **Feature `hooks` di rusqlite**: serve al progress handler del test del
   cammino limitato. Nessuna crate nuova; lock invariato.
3. **`superseded` senza `valid_to` via API**: il formato e l'import lo
   impediscono, ma una `patch_note` che azzera `valid_to` su una nota
   sostituta può ancora produrlo (la cella non ha il guardia). Segnato come
   angolo noto; il validatore d'importazione lo riprende al giro dopo.
4. **Astensione**: resta «nessun risultato sopra soglia» quando nessun
   segnale sopravvive ai filtri; la soglia assoluta è rimandata alla
   taratura v0.2, come da REPORT v0.1.

## Cosa non ho potuto provare

- Come in v0.1: CI mai eseguita (nessun runner), advisory di cargo-deny
  offline, altre piattaforme.
- Il benchmark misura la macchina di sviluppo con altri processi utente:
  per questo la mediana di tre giri e l'intervallo dichiarato nella varianza.

---

# REPORT — Engramma v0.2 (significato)

Data: 2026-11-05. Base: `main` @ `35cdb76`. Tutto offline (`cargo --offline`),
Rust 1.95, macOS arm64. Nessuna crate nuova: l'unica dipendenza diretta
aggiunta è `ring` 0.17 (sha256 dei modelli) che era **già nell'albero** via
rustls/ureq — `Cargo.lock` non cambia versione di niente.

## Cosa è stato fatto, punto per punto

### 0. Residui della v0.1.1

- **Archi manuali e quarantena** (`21e530f`, test rosso prima): `write_note`
  aggiorna lo `status` degli archi manuali che citano la nota nella stessa
  operazione (deterministica nel rigioco) e `apply_link` fa nascere l'arco con
  lo stato della nota citata. Test: quarantena → l'arco sparisce da
  `neighbors`, `confirm` → torna, cancellazione → sparisce per sempre,
  `verify` ok.
- **`superseded` senza `valid_to` via API** (`285dd0e`, test rosso prima): la
  guardia è in `write_note`, quindi copre `patch_note` (azzerare `valid_to`)
  e `put_note` (replace con stato già `superseded`).
- **p95 a 100k**: non era obiettivo; il ramo vettoriale non lo tocca (il
  benchmark lo dimostra: le righe di testo restano quelle della v0.1.1). La
  «lettura unica per candidato» non è stata fatta: i candidati ibridi leggono
  per id come in v0.1; rimane aperta.

### 1. `engramma-embed` (`ca62762`)

- Il trait (`TextKind`, `Embedder`, `EmbedError`) sta in `engramma-core`,
  senza I/O di rete. Aggiunta mia, documentata: un metodo fornito
  `approx_tokens` (stima caratteri/4) che `Builtin` override con il conto
  esatto del tokenizer — serve a tagliare i blocchi senza mettere i
  tokenizer nel core.
- **`Builtin` (tract)**: carica `onnx/model.onnx` con `tract_onnx`, input i64
  a dimensioni simboliche (batch e sequenza) prese da `model.symbols` — con
  uno `SymbolScope` nuovo tract rifiuta l'unificazione delle dimensioni.
  Output `[batch, seq, dim]`, pooling CLS o mean pesata dalla maschera
  (letto da `1_Pooling/config.json`), L2. Troncamento e padding sono nostri,
  non del tokenizer: un tokenizer con truncation interna falsava anche il
  conto dei token (`approx_tokens` non vedeva mai più di 512). Batch
  indipendenti in parallelo con `std::thread::scope` fino a
  `available_parallelism()`.
- **Fedeltà** (`tests/fidelity.rs`, modelli veri): cos ≥ 0,9999 contro i
  vettori di riferimento ONNX Runtime 1.31.1 per **97M e 311M**; lo stesso
  testo da solo e in batch lungo cos ≥ 0,9999 (padding invariante);
  troncamento a 512 token funzionante e deterministico; batch vuoto vuoto.
  Senza modoli il test **fallisce** con un messaggio che dice come rimediare;
  `ENGRAMMA_SKIP_MODEL_TESTS=1` è l'opt-out esplicito per la CI.
- **`ModelSpec`**: manifest con repo, revisione fissata, sha256 e dimensione
  dei due file grossi, dim, pooling, max_tokens, prefissi facoltativi.
  Granite R2 non usa prefissi. EmbeddingGemma: solo cartella utente con
  `manifest.json` (licenza Gemma); nessun test con i suoi pesi.
- **Dove stanno i modelli**: `ENGRAMMA_MODEL_DIR` o
  `~/.cache/engramma/models`; all'apertura si verificano gli sha256 dei due
  file grossi con **cache dell'esito** invalidata da mtime e dimensione.
- **`model pull`**: scarica da `huggingface.co/<repo>/resolve/<rev>/<file>`
  in un file temporaneo, calcola lo sha256 mentre scarica e rinomina solo se
  combacia; errore = nessun residuo (test).
- **Ollama** (`/api/embed`) e **Http** (`/v1/embeddings`, bearer da variabile
  d'ambiente, mai nei log, normalizzazione locale). Tutti e due — e il
  `pull` — sono provati contro un **server finto su 127.0.0.1**
  (`TcpListener` in un thread): corpo della richiesta, risposta (incluso
  l'indice sparso di `/v1/embeddings`), HTTP 500, JSON malformato, timeout,
  dimensione sbagliata, 404, doppio pull che salta il file già presente.
- **Configurazione**: `<root>/config.json`; `engramma model use` la scrive
  per i builtin, i remoti si configurano a mano (schemi in `docs/cli.md`).

### 2. Archiviazione dei vettori (`4bfe2d4`)

- Migrazione v4: `embedding_models (model, dim, state, created_at)` e
  `note_vectors (note_id, chunk, model, content_hash, note_hash, vec)`.
  Aggiunta mia rispetto allo schema del prompt: la colonna `note_hash`
  (blake3 di modello+testo intero) rende la **staleness** verificabile senza
  tokenizer — per questo `verify` può riportare la copertura senza embedder.
- Derivate: fuori da `ops`, rigioco, `compare_states` ed export. Test
  richiesto: due celle con lo stesso contenuto logico, una con vettori e una
  senza → `verify` ok su entrambe.
- Testo incorporato: `title + "\n\n" + body`; oltre `max_tokens` blocchi per
  paragrafo con sovrapposizione di un paragrafo (l'overlap entra solo se
  lascia spazio al paragrafo in arrivo: un overlap che sfonda il budget è
  cortesia, il budget è la legge). `content_hash` per blocco, punteggio della
  nota = massimo dei blocchi.
- Le scritture non incorporano: le note cambiate restano in attesa;
  `embed_pending` lavora a lotti (32 note per chiamata `embed`) incorporando
  **fuori dal lock** e scrivendo in transazioni brevi; le lapidi cancellano i
  vettori nell'operazione stessa.
- `verify` riporta la copertura per modello (non è un errore).
- **Blu/verde**: `model use` crea il `building`; `embed_pending` riempie
  quello mentre il `ready` serve; a copertura 100% una transazione ritira il
  vecchio (cancellandone i vettori) e promuove il nuovo. Test: a metà
  costruzione la ricerca risponde col vecchio modello e mai con un indice
  misto; terzo modello rifiutato mentre uno costruisce; embedder sbagliato
  rifiutato.

### 3. Indice vettoriale (`07eb6f5`)

- `trait VectorIndex` (`search/upsert/remove/rebuild/len`) e `FlatI8`:
  quantizzazione simmetrica per vettore (scala f32 per riga), prodotto
  scalare intero su tutta la matrice con 8 accumulatori i32 indipendenti,
  primi m candidati; **ricalcolo esatto in f32** dei candidati rileggendo i
  vettori da `note_vectors` (tabella temporanea + JOIN); il dot f32 usa gli
  8 accumulatori `chunks_exact(8)` (quello ingenuo misurato 20× più lento).
- Ciclo di vita: costruzione **pigramente** alla prima ricerca (apertura
  cella non misurabile, < 3 ms); aggiornamento incrementale dalle scritture
  della stessa `Cell`; `PRAGMA data_version` cambiato (altro processo) →
  ricostruzione. Test due-celle-richiesto: la scrittura di B si vede nella
  ricerca di A.
- Filtri: i candidati vettoriali passano dagli stessi filtri del testo
  **prima** della fusione; se i superstiti sono meno di k, m raddoppia fino
  alla scansione completa.
- Test: richiamo@10 ≥ 0,98 contro l'esatta su 10 000 vettori casuali a seme
  fisso (384 dim) e 5 000 (768 dim); proptest indice incrementale =
  ricostruito dopo scritture casuali (stessi primi 10 per 30 query);
  richiamo@10 della pipeline int8+ricalcolo ≥ 0,98 **sui vettori veri del
  vault dorato** col 97M. Nota onesta: il solo ranking int8 grezzo su quei
  vettori vale 0,953 — il contratto è la pipeline con ricalcolo, come da
  prompt.
- **Quarantena vettoriale**: la nota in quarantena col vettore più vicino
  non esce e non fa da seme (test dedicato: filtra prima, e l'astensione
  toglie i candidati prima della camminata).

### 4. Ricerca mista (`e689db1` + `e6ca67f`)

- Pipeline: BM25(50) e vettori(50) → filtri → fusione → allargamento dal
  grafo sui semi fusi → riordino → explain. La domanda si incorpora fuori
  dal lock, con **cache LRU scritta a mano** (256 voci per modello; il test
  conta le chiamate all'embedder, i tempi da soli sono rumore).
- `Fusion::Rrf { k: 60 }` e `Fusion::Convex { alpha }` (bm25 min-max sui
  candidati; mancare da una lista vale 0), punteggio fuso riscalato su [0,1]
  — senza riscala i vicini di grafo (pavimento 0,05) battevano i veri match
  RRF (~1/61), scoperto a mano.
- **Explain** esteso: `semantic_rank`, `cosine`, `fused_score`, `lists`
  (`text`/`vector`/`graph`), blocco vincente; `Hit` espone `raw_score`
  accanto al punteggio normalizzato per la visualizzazione. In CLI e MCP.
- **Astensione**: i risultati solo-vettoriali richiedono cos ≥ tau; il
  filtro corre **prima** della camminata (il primo disegno lasciava
  rientrare i vicini dei candidati scartati: test e correzione).
- CLI `search --mode text|vector|hybrid` (ibrido di default con modello
  ready), `--fusion`, `--tau`; MCP con ramo vettoriale automatico e
  descrizioni aggiornate.

### 5. Misure e scelta del modello (questo commit + `ba94de0`)

Domande dorate nuove: **15 parafrasi** (test dedicato: nessuna radice condivisa
con la nota attesa dopo la normalizzazione di `text.rs`), **3 in inglese** su
note italiane, **2 astensioni semantiche**. Generatore del benchmark:
parafrasi a modello fisso e domande in inglese, categorie
fattuali/parafrasi/multi-salto/inglese.

### 6. Documentazione (`651bf25`)

`docs/architecture.md` (ramo vettoriale, dati derivati, blu/verde),
`docs/cli.md` (`model`, `embed`, `search --mode`, nuovi campi JSON, schemi
di config per Ollama/Http), `README.md` (scaricare il modello, Ollama,
licenze, `ENGRAMMA_SKIP_MODEL_TESTS`), `spec/format-v0.md` (una riga: i
vettori non fanno parte del formato).

## Le tabelle delle misure

### Fedeltà dei vettori (tract contro ONNX Runtime 1.31.1, righe del 2026-11-05)

| modello | frasi | cos minimo |
|---|---|---|
| granite-embedding-97m-multilingual-r2 | 4 | ≥ 0,9999 |
| granite-embedding-311m-multilingual-r2 | 4 | ≥ 0,9999 |

Padding (stesso testo da solo / in batch lungo): cos ≥ 0,9999. Troncamento a
512 token: riesce, deterministico, L2 intatta.

### Taratura sul vault dorato (richiamo@5 / MRR, 5 risultati, 2 passi)

Convessa = α·cos + (1−α)·bm25_minmax. «recenza» = peso v0.1 (mezza-vita
180 giorni) nel riordino; «senza» lo toglie dal ramo semantico.

**97M**

| fusione | recenza | fattuali (20) | parafrasi (15) | inglese (3) | multi-salto (3) |
|---|---|---|---|---|---|
| RRF k=60 | sì | 0,850/0,575 | 0,333/0,200 | 1,000/0,500 | 0,667/0,167 |
| RRF k=60 | no | 1,000/0,837 | 0,733/0,469 | 1,000/0,833 | 1,000/0,667 |
| convessa α=0,3 | no | 1,000/0,900 | 0,533/0,406 | 1,000/0,833 | 1,000/0,556 |
| convessa α=0,5 | no | 1,000/0,875 | 0,733/0,478 | 1,000/0,833 | 1,000/0,556 |
| **convessa α=0,7** | no | **1,000/0,875** | **0,800/0,491** | **1,000/0,833** | **1,000/0,556** |

**311M**

| fusione | recenza | fattuali (20) | parafrasi (15) | inglese (3) | multi-salto (3) |
|---|---|---|---|---|---|
| RRF k=60 | no | 1,000/0,838 | 0,733/0,432 | 1,000/1,000 | 1,000/0,833 |
| convessa α=0,3 | no | 1,000/0,900 | 0,600/0,377 | 1,000/1,000 | 1,000/0,556 |
| convessa α=0,5 | no | 1,000/0,900 | 0,733/0,452 | 1,000/1,000 | 1,000/0,556 |
| **convessa α=0,7** | no | **1,000/0,875** | **0,867/0,482** | **1,000/1,000** | **1,000/0,556** |

Due decisioni prese su questi numeri:

1. **La recenza esce dal riordino semantico** (resta per intero nel ramo
   solo-testo). Le differenze di coseno tra un match vero e il rumore sono
   centesimi; la mezza-vita a 180 giorni, tarata sugli scarti ampi del BM25,
   li mescolava: toglierla porta i fattuali a 1,000 su entrambi i modelli e
   le parafrasi da 0,20–0,53 a 0,53–0,87.
2. **Fusione predefinita: convessa α=0,7.** A parità di fattuali batte RRF
   sulle parafrasi su entrambi i modelli (0,800 contro 0,733 col 97M; 0,867
   contro 0,733 col 311M) — la stessa direzione della letteratura (ACM TOIS
   2023). RRF resta disponibile (`--fusion rrf`) ed è il default giusto
   senza dati, ma qui i dati ci sono.

### Curva di astensione (tau)

Coseni dei primi risultati per le domande che dovrebbero restare vuote
(97M / 311M):

| domanda | coseno massimo |
|---|---|
| «che auto guida Giulia?» (assente) | 0,878 / 0,872 |
| «quando si sono sposati…?» (assente) | 0,855 / 0,908 |
| «campione» (contenuto in quarantena) | 0,810 / 0,858 |
| «vegetariano» (sostituita) | 0,818 / 0,834 |
| le altre 8 astensioni testuali | 0,746–0,777 / 0,777–0,815 |

Coseni delle note attese per le parafrasi vincenti: 0,80–0,826 (97M),
0,81–0,877 (311M). **Le due bande si sovrappongono**: nessun tau assoluto
lascia vuote le astensioni senza vuotare anche le parafrasi. Punti della
curva (97M): tau 0,80 → 5/10 astensioni testuali giuste, parafrasi 0,400;
tau 0,84 → 8/10, parafrasi 0,067; tau 0,88 → le due semantiche si astengono
ma le parafrasi valgono 0. Con il 311M uguale, spostato di ~+0,02.

Scelta: **tau = 0,30 di default** — un pavimento contro il rumore basso,
non un rivelatore di menzogne. Le due astensioni semantiche dorate restano
**il punto aperto dichiarato** di questa fase (v. sotto); le astensioni
*testuali* continuano a passare nel modo testo, che è quello con cui sono
sempre state misurate.

## Prove a mano (uscite della CLI, trascritte)

```console
$ engramma --root $T init
inizializzato: /var/folders/…/engramma-smoke
$ engramma --root $T model list          # con ENGRAMMA_MODEL_DIR=~/.cache/engramma/models
modelli in /Users/<utente>/.cache/engramma/models
granite-embedding-97m-multilingual-r2  384 dim, 420 MB, scaricato, stato: —
granite-embedding-311m-multilingual-r2  768 dim, 1277 MB, scaricato, stato: —
$ engramma --root $T model use granite-embedding-97m-multilingual-r2
modello granite-embedding-97m-multilingual-r2@835ad140 in costruzione
(config in …/config.json): `engramma embed` lo riempie, la ricerca continua
con il modello attuale
$ engramma --root $T import tests/golden/vault-it --obsidian
incorporate: 62 (in attesa: 0)
importate: 62
saltate: 0 / errori: 0
$ engramma --root $T embed
incorporate: 0 note                      # l'import aveva già finito
$ engramma --root $T verify
registro verificato: 63 operazioni, catena hash, ricostruzione e indice FTS
a posto                                # + copertura 62/62 nel --json
```

La domanda parafrasi, nei due modi (primi 3 risultati):

```console
$ engramma --root $T search "in che città si è trasferita la coppia" --mode text
1. [active] Torino (00000022)   … liste: text, termini: citt
2. [active] Milano (00000021)   …
3. [active] Il capoluogo di Giulia (00000087) …
$ engramma --root $T search "in che città si è trasferita la coppia" --mode hybrid
1. [active] Torino (00000022)   liste: text, vector   coseno: 0,7862
2. [active] Il capoluogo di Giulia (00000087)  liste: text, vector
3. [active] Milano (00000021)   liste: text, vector   coseno: 0,7987
```

Nota: in ibrido le note dei luoghi arrivano dalle due liste insieme; la
nota «Giulia abita a Milano» (l'attesa della parafrasi dorate) compare al
posto 6 in ibrido e 3 in `--mode vector` (coseno 0,8268): la recency non
c'entra più, qui sono i vincoli di quartiere (Torino/Navigli) a pesare nel
coseno. La versione dorate della domanda (`--mode hybrid` con il 311M, il
modello predefinito scelto) la vede nelle prime 5.

```console
$ engramma --root $T search "campione"
nessun risultato sopra soglia          # modo testo (nessun termine vivo)
$ engramma --root $T search "campione" --mode hybrid
1. [active] Fabio (00000001)  liste: vector  coseno: 0,8097 …
```

L'astensione testuale resta perfetta; quella semantica è il punto aperto
dichiarato (curva sopra).

Cambio di modello blu/verde al 311M:

```console
$ engramma --root $T model use granite-embedding-311m-multilingual-r2
modello granite-embedding-311m-multilingual-r2@44399559 in costruzione …
$ time engramma --root $T embed
incorporate: 62 note
modello pronto: granite-embedding-311m-multilingual-r2@44399559
  (build di debug: 16,5 s, 561% CPU)
$ engramma --root $T model list
granite-embedding-97m-multilingual-r2  … stato: retired
granite-embedding-311m-multilingual-r2  … stato: ready
$ engramma --root $T search "in che città si è trasferita la coppia" --mode hybrid --k 3
1. [active] Torino (00000022)   coseno: 0,8289
2. [active] Il capoluogo di Giulia (00000087)  coseno: 0,8419
3. [active] Milano (00000021)   coseno: 0,8694     # servito dal 311M
```

### Prova MCP a mano

```console
$ printf '…initialize… …tools/call memory_search "in che città si è trasferita
  la coppia"…' | engramma --root $T mcp --cells default
modalità: hybrid
  'Milano'  liste=['text', 'vector']  coseno=0,8694  rango_semantico=6
  'Torino'  liste=['text', 'vector']  coseno=0,8289  rango_semantico=23
  'Roma'    liste=['text', 'vector']  coseno=0,8765  rango_semantico=1
```

Il server ha preso il ramo vettoriale da solo (la cella aveva il 311M ready
e `config.json` l'embedder corrispondente) e l'explain porta le liste, il
coseno e il rango semantico.

## Cosa non ho potuto provare

- **Windows e Linux**: come per v0.1/v0.1.1, compilato e provato solo sul
  macOS arm64 locale; la CI (mai eseguita) copre le altre piattaforme.
- **Advisory di cargo-deny**: richiedono rete; eseguito `licenses bans`.
- **EmbeddingGemma**: nessun test con i suoi pesi (licenza Gemma); la
  cartella-utente con manifest è provata con un manifest finto.
- **Il 311M a 100k note**: dichiarato — il costo di incorporazione e giri di
  qualità sulla macchina di sviluppo sforerebbe ogni tempo ragionevole; le
  sue misure sono a 10k.
- **La curva tau completa col 311M a 100k**: per lo stesso motivo.

## Domande aperte per la v0.3

1. **Astensione semantica**: la soglia assoluta non separa (curve nel
   REPORT). Servono segnali relativi: margine fra il primo e il resto,
   conformità query-risposta (il coseno fra la domanda *ricalcolata sul
   testo del risultato* e la domanda stessa), o un classificatore leggero
   tarato. È il primo candidato della v0.3.
2. **Recenza**: tolta dal ramo semantico su misura; andrà ritarata una
   normalizzazione comune (per esempio recenza sul punteggio fuso, con
   mezza-vita più lunga) invece che rimossa.
3. **Lettura unica per candidato** (p95 a 100k): i candidati di testo e
   vettori leggono per id; un JOIN per lotto ridurrebbe il p95 di entrambi
   i rami. Rimane dal REPORT v0.1.
4. **Peso del grafo nell'ibrido**: i vicini di grafo entrano con il
   pavimento 0,05 sul fuso normalizzato; con il vault dorato non ha
   disturbato, ma una misura dedicata (domande multi-salto ibride) manca.
5. **Chunking**: i blocchi per paragrafo funzionano per note-verbetto; note
   con un solo paragrafo enorme vanno spezzate a parole (fatto, ma senza
   sovrapposizione semantica: la continuità è del tokenizer, non del
   contenuto).
6. **Registro a 246 MB/100k**: invariato dalla v0.1; la compattazione
   resta prevista per la v0.3, e ora ci sono anche i vettori (derivati:
   ricostruibili, non compattano il registro).

### Benchmark (release, mediana di 3 giri, `bench/RESULTS.md` rigenerato)

Testo+grafo (per confronto con la v0.1.1 — il ramo vettoriale non lo tocca):

| corpus | note | inserimento (note/s) | apertura (ms) | p50 (ms) | p95 (ms) | richiamo@5 | MRR | file (MB) |
|---|---|---|---|---|---|---|---|---|
| viva | 10 000 | 3 366 | 0,8 | 4,5 | 6,8 | 0,972 | 0,763 | 24,3 |
| viva | 100 000 | 2 689 | 2,1 | 43,1 | 72,6 | 0,899 | 0,637 | 247,9 |
| viva+97M | 100 000 | 2 650 | 2,1 | 43,2 | 73,3 | — | — | 466,7 |

Ramo vettoriale:

| modello | note | embedding (note/s) | costr. indice (ms) | mem. indice (MB) | vettori p50/p95 (ms) | ibrido caldo p50/p95 (ms) | ibrido + embedding query p50/p95 (ms) |
|---|---|---|---|---|---|---|---|
| 97M | 10 000 | 187 | 24 | 4,3 | 15,7/17,5 | 17,0/19,2 | 16,8/19,3 |
| 311M | 10 000 | 49 | 67 | 8,1 | 32,1/33,8 | 33,4/35,0 | 33,5/35,8 |
| 97M | 100 000 | 110 | 148 | 42,8 | 64,2/80,2 | 78,9/100,5 | 78,7/100,1 |

Richiamo@5/MRR per modalità (estratto: le righe complete in
`bench/RESULTS.md`; multi-salto v. sotto):

| note | modalità | fattuali | parafrasi | inglese |
|---|---|---|---|---|
| 10k | testo | 1,000/0,915 | 1,000/0,648 | 1,000/0,747 |
| 10k | vettori | 1,000/0,965 | 0,996/0,944 | 1,000/0,991 |
| 10k | **convex-0,7** | **1,000/1,000** | **1,000/0,984** | **1,000/0,941** |
| 100k | testo | 0,964/0,817 | 0,908/0,514 | 0,891/0,581 |
| 100k | vettori | 0,988/0,892 | 0,982/0,872 | 0,987/0,872 |
| 100k | rrf | 1,000/0,964 | 1,000/0,973 | 0,996/0,884 |
| 100k | convex-0,7 | 1,000/0,978 | 0,999/0,860 | 0,995/0,789 |

Letture:

- **Il ramo vettoriale non peggiora il testo**: p50/p95 delle ricerche di
  testo restano 43/73 ms a 100k (v0.1.1: 48/85 sulla stessa macchina).
- **Obiettivo §11 «ricerca mista ≤ 50 ms a 100k con query già incorporata»:
  mancato** — 78,9/100,5 ms. Il ramo vettoriale in sé (scansione int8 +
  ricalcolo f32) costa ~2 ms; il resto è il pipeline completo (BM25 + grafo
  43 ms) più la lettura dei candidati e i 200 vettori ricalcolati per
  domanda (~30 ms). Le vie: meno candidati ricalcolati, la lettura unica
  per candidato già segnata, cache dei vettori ricalcolati. Onestamente
  dichiarato, con i numeri.
- **Embedding della query quasi gratis** nel p50 (colonna calda contro
  fredda: 78,9 contro 78,7 — ~10 ms del 97M si perdono nella varianza;
  il 311M li si vede: 33,4 → 33,5).
- **La fusione**: la convessa vince a 10k e sul vault dorato (parafrasi
  dure), RRF vince l'MRR a 100k sulle parafrasi sintetiche — che però
  condividono il nome della persona con la nota, quindi sono più facili.
  Il default resta **convessa α=0,7** perché vince dove la parafrasi è
  vera (nessuna radice condivisa) e a 10k; RRF resta a un flag di distanza.
- **Multi-salto del banco: 0,000 in ogni modalità — ed era 0/20 già nella
  v0.1.1** (verificato ricompilando il bench del tag `fase-v0.1.1`: la
  nota attesa non entra nei primi 5 contro centinaia di match diretti di
  «lavora» e il punteggio di grafo decaduto non basta). Non è una
  regressione di questa fase, ma il banco v0.1.1 non la mostrava perché
  l'aggregato non separava le categorie. Le multi-salto *dorate* (cella da
  62 note) funzionano. Da ripensare nel generatore o nel peso del grafo.

### Scelta del modello predefinito

| | 97M | 311M |
|---|---|---|
| qualità, parafrasi dorate (richiamo@5/MRR) | 0,800/0,491 | **0,867/0,482** |
| qualità, inglese dorato | 1,000/0,833 | **1,000/1,000** |
| qualità, fattuali dorati | 1,000/0,875 | 1,000/0,875 |
| embedding (note/s) | **110–187** | 49 |
| query ibrida p50 (10k) | **17 ms** | 33 ms |
| modello su disco | **420 MB** | 1 277 MB |
| vettori in cella (10k) | **+22 MB** | +42 MB |

**Predefinito: granite-embedding-311m-multilingual-r2.** Decidono le
parafrasi vere e l'attraversamento di lingua, che sono la ragione del ramo
semantico; il 97M resta equivalente sui fatti e costa un terzo, quindi è
l'opzione consigliata per celle grandi o macchine piccole — la scelta è una
riga di `config.json`. L'inserimento non cambia (≥ 2 000 note/s senza
embedding su entrambi).

## Chiusura della fase

- `cargo fmt --check`, `cargo clippy --all-targets --offline -- -D warnings`,
  `cargo test --offline --workspace` (anche `--release`),
  `cargo deny check licenses bans`: tutti verdi (uscita sotto).
- Prove a mano: sopra (init → model use → import → embed → search
  testo/ibrido → campione → verify → scambio al 311M → MCP).
- Tag: `fase-v0.2`. Nessun push (repo locale).

---

# REPORT — Engramma v0.2.1 (correzioni della verifica v0.2)

Data: 2026-11-06. Base: `main` @ `fc0b48f` (tag `fase-v0.2`). Un commit
per punto, con il test o la misura che fallisce prima della correzione.
Tutto offline, Rust 1.95, macOS arm64, nessuna crate nuova (`Cargo.lock`
invariato).

## P0.1 — la fusione convessa normalizza il coseno

La convessa sommava il coseno grezzo (scarto ~0,1 tra i candidati) contro
una BM25 min-max su [0,1]: α pesava due scale senza relazione. Test rosso
prima: `convex_fusion_normalises_the_cosine_scale` dava `{a: 0.86, b:
0.637}` con BM25 differente per 1% e coseno per 0,11.

- `Fusion::Convex` guadagna `cos_norm`: **Candidates** (min-max sui
  candidati vettoriali) o **Theoretical** (limiti fissi −1..1 del coseno
  tra vettori L2, come l'analisi delle funzioni di fusione per hybrid
  retrieval su ACM TOIS 2023 — stabile ma a scarto compresso: misura alla
  mano, si comporta quasi come il coseno grezzo).
- Le due domande del prompt ora mettono primo il vincente semantico
  (prove a mano sotto; test dorato col 97M).
- Bug trovato ritmando: l'harness dorato etichettava «97M» una tabella
  misurata con l'embedder del 311M su una cella 97M — il ramo vettoriale
  non entrava mai (parafrasi 0.000 in ogni fusione). `report()` ora carica
  il modello che misura.

Taratura (recenza fuori dal ramo semantico, come v0.2). Vault dorato:

| fusione | 97M parafrasi (15) | 97M fattuali (20) | 311M parafrasi | 311M fattuali |
|---|---|---|---|---|
| rrf | 0.867/0.569 | 1.000/0.837 | 0.867/0.532 | 1.000/0.838 |
| mm-0.3 | 0.667/0.506 | 1.000/0.875 | 0.800/0.499 | 1.000/0.900 |
| mm-0.5 | 0.933/0.594 | 1.000/0.900 | 1.000/0.591 | 1.000/0.900 |
| **mm-0.7** | **0.933/0.667** | 1.000/0.875 | **1.000/0.691** | 1.000/0.875 |
| th-0.3..0.7 | ≤0.867 | 1.000/≤0.900 | ≤0.867 | 1.000/≤0.900 |

Benchmark (nuovo corpus, vedi P1.7): a 10k mm-0.3 porta fattuali a MRR
1.000 e parafrasi 0.986; a 100k decide il default (criterio: **MRR medio
pesato per numero di domande sulle quattro categorie del benchmark a
100k**; a parità vince chi tiene le parafrasi, la ragione del ramo
semantico). Tabella e scelta in «Chiusura».

## P0.2 — `embed_pending` lineare

`VectorTx::pending()` rileggeva per ogni lotto da 32 tutte le note
(corpo compreso) e tutti i `note_vectors`, ricalcolando ogni hash:
O(N²/32). Misure della sonda (sopra) prima della correzione: 2,7 s a
10k, **279,2 s a 100k** (rapporto 103×).

Ora: finestre posizionali per chiave (`WHERE id > ?last ORDER BY id LIMIT
?take`, LEFT JOIN sul `note_hash` del blocco 0, hash solo per le righe
della finestra); `remaining` contato una volta all'inizio e decrementato;
le note toccate da altri processi durante la chiamata (ops con seq oltre
il watermark) vengono ricontrollate e riprese dalla chiamata dopo (test:
edit + nota nuova tra due chiamate → done 2, remaining 0); la copertura
di `verify` è una query di conteggio; la promozione blu/verde riconta
esatto solo a completamento. Test col contatore iniettato: la chiamata
che incorpora 192 note legge ~128 righe in più di quella da 64 (le sue
finestre), non 2×300.

## P0.3 — ricerca mista a 100k, p95 ≤ 50 ms

Profilo (sonda + `sample`): quattro cause + una quinta trovato rifacendo
le misure. Prima/dopo, sonda a 100k (mediana di 3; «prima» = tag
fase-v0.2):

| voce | prima | dopo |
|---|---|---|
| `embed_pending` (P0.2) | 279,2 s | **1,4 s** (rapporto 100k/10k = 14) |
| scansione int8 (`FlatI8::search`, m=200) | 8,61 ms | **1,37 ms** |
| solo vettori p50/p95 | 17,4/17,8 ms | **4,0/4,2 ms** |
| ibrido p50/p95 | 58,3/58,7 ms | **7,7/7,9 ms** |

- **top_int** riscritta come chiesto (chunks_exact zip, inverso della
  scala precalcolato, punteggi in Vec + `select_nth_unstable_by`):
  8,61 → 1,37 ms (obiettivo ≤ 1,5; prototipo del prompt 0,72).
- **PRAGMA** `cache_size` −65536 (64 MB) e `mmap_size` 256 MB
  (best-effort): A/B a mano, il guadagno vero stava altrove (sotto);
  tenuti perché il ricalcolo legge BLOB a pagina. Documentati in
  `docs/architecture.md`.
- **Termini onnipresenti fuori dalla MATCH**: frequenze documentali da
  `fts5vocab` (vista `notes_fts_vocab` creata con lo schema, additiva
  come `meta`). Scoperta da dichiarare: la fts5vocab di SQLite 3.54
  restituisce **una riga per termine** con `doc` = frequenza documentale
  e `cnt` = occorrenze totali (verificato a mano; le versioni precedenti
  emettevano una riga per termine/documento — il primo `COUNT(*)` dava
  sempre 1). Un termine esce solo se supera il 20% **e** almeno 1000
  documenti, e solo se resta un termine che matcha: su una cella da 6
  note ogni termine supera il 20% e il test temporale della v0.1 lo
  dimostrava (rimosso il pavimento, falliva). Sul corpus del benchmark
  nessun termine delle domande supera il 20% («lavor» e «abit» stanno al
  6%: l'effetto su richiamo e MRR è nullo per costruzione, il vincolo
  ≤ 0,01 è rispettato); sul corpus della sonda «lavora» è nel 100% delle
  note e la regola lavora (le domande diventano selettive sul nome).
- **Una lettura per candidato** (`read_candidates`): id in tabella
  temporanea, una query per nota+body_norm+tags_text, filtri
  status/type in SQL, validità bitemporale in `passes_filters` dove vive
  la sua semantica. Candidati di testo, vettoriali (blocchi di 64) e
  note del grafo, in entrambi i percorsi.
- **Il piano SQL** (la quinta causa, trovata profilando dopo le prime
  fix: `read_candidates` dominava in `sqlite3BtreeIndexMoveto`): il
  planner rovesciava il join della tabella temporale contro il filtro di
  stato — 100k probe per query. `CROSS JOIN` su lettura candidati e
  ricalcolo f32 (come la camminata dal v0.1.1): ibrido 61 → 8 ms p50.
  In più il dot del ricalcolo direttamente dai byte little-endian.

Obiettivo §11 (ibrido p95 ≤ 50 ms a 100k con query già incorporata):
**7,9 ms** con l'embedder finto; col 97M vero in «Chiusura». «Solo
vettori ≤ 3 ms»: 4,0 ms — mancato di 1 ms (resta il ricalcolo f32 di 200
candidati + la camminata; dichiarato).

## P1.4 — copertura in `verify`

Test CLI rosso prima (il JSON non aveva `coverage`). Ora: riga umana
`copertura vettori: <model> [<stato>] <covered>/<living> note` e campo
JSON `coverage: [{model, state, covered, living}]`. Semantica: presenza
del vettore (la freschezza è affare di `embed_pending`, mai un errore di
verifica).

## P1.5 — modello predefinito

Confronto rifatto dopo P0.1, benchmark 10k, parafrasi e inglese (il
criterio del prompt: il 311M resta default solo se vince di ≥ 0,03 di
MRR sulle parafrasi). La tabella qualità per modello esce dal benchmark
rigenerato (patch alla stampa: una riga per variante, prima solo la
prima per dimensione). Numeri e scelta in «Chiusura».

## P1.6 — support: strong/weak

`explain.support`: «weak» quando il significato da solo porta il
risultato (nessuna conferma di testo **discriminativa**: un match che
poggia solo su termini oltre il 20% del corpus — il nome del soggetto in
un vault su di lui — non conferma nulla) **e** il margine di coseno sul
candidato vettoriale successivo è sotto `Semantic::weak_margin`
(default tarato 0,03). MCP: la descrizione di `memory_search` spiega che
weak vuol dire «vicino per significato, non confermato dal testo:
verifica prima di affermare»; CLI `--strict` toglie i weak.

Taratura (97M, margini in ordine di coseno): i vincitori parafrasi
coprono 0,000–0,024, i vertici delle astensioni 0,000–0,024, e si
intersecano — «che auto guida Giulia» a 0,0239, il vincitore di «quali
cibi la donna digerisce male?» a 0,0244. A 0,03 le due domande nominate
escono vuote in `--strict` (test), ma 10/15 vincitori parafrasi cadono:
il bound ≤ 1 del prompt non è raggiungibile con questo segnale, come non
lo era la soglia assoluta — curve nel dettaglio qui sopra, `--strict`
 resta opt-in. L'etichetta non cambia la ricerca.

## P1.7 — il multi-salto misura

Diagnosi (`engramma-bench --multihop-debug`, 5 domande): la nota attesa
era ASSENTE dai primi ~70-100. Catena di cause, ciascuna corretta:

1. **il taglio degli extra era una lotteria per id** (BTreeMap →
   take(40)): tra migliaia di extra teneva i 40 con id più basso;
2. **gli hub allagavano la camminata**: l'azienda citata da ogni nota
   trascinava dentro i colleghi (5425 extra misurati a 10k per una
   domanda). Ora il punteggio ereditato è smorzato per grado
   (score/(1+ln(1+grado))) e il taglio è per punteggio;
3. **il generatore**: partner asimmetrici (la nota della partner non
   nomina la persona: nessun testo la trovava) e ~26 riempitivi a persona
   col nome nel titolo che intasavano i 50 candidati BM25; le note
   persona datate 2024-01 affogavano nella recenza. Partner reciproci,
   titoli dei riempitivi senza nome, note persona pagine vive;
4. **frasi bigramma nella MATCH** («anna ross»): la nota della partner
   («Partner di [[Anna Rossi]]») matcha con precisione, non tutto ciò che
   si chiama Anna.

Prima/dopo (benchmark 10k, ric/MRR): 0.000/0.000 in ogni modalità →
testo **1.000/0.722**, mm-0.3 1.000/0.333, rrf 0.389/0.095; a 100k in
«Chiusura». Le altre categorie non calano (stessa tabella).

## Chiusura

fmt, clippy `-D warnings`, test (158, debug e release), `cargo deny check
licenses bans`: verdi. Prove a mano (97M, transcritte qui sotto). Tag
`fase-v0.2.1`.

### La scelta della fusione (P0.1): convessa mm-0,7

Criterio dichiarato: **MRR medio del benchmark a 100k pesato per numero
di domande** (fattuali 1913, parafrasi 1533, multi-salto 237, inglese
1001); a parità vince chi tiene le parafrasi, la ragione del ramo
semantico. MRR pesato a 100k (97M):

| fusione | MRR pesato | parafrasi | note |
|---|---|---|---|
| mm-0.3 | 0.757 | 0.778 | migliore a 10k sui fattuali (MRR 1.000) |
| mm-0.5 | 0.825 | 0.864 | |
| **mm-0.7** | **0.838** | **0.878** | **predefinita** (è anche la scelta del vault dorato) |
| rrf | 0.645 | 0.643 | la v0.2 la preferiva a 100k col coseno grezzo |
| th-0.7 (migliore th) | 0.477 | 0.411 | i limiti teorici tengono lo scarto compresso |

La teorica perde ovunque in modo consistente: stabile sì, ma riporta la
scala del coseno a ~0,05 di scarto effettivo e il testo domina — la
misura che il prompt chiedeva di fare. RRF resta a `--fusion rrf`.
Il commento di `Semantic::new` ora dice quello che il codice fa.

### La scelta del modello (P1.5): 97M

Confronto rifatto dopo P0.1, benchmark 10k (252 parafrasi, 167 inglesi),
fusione predefinita:

| | 97M | 311M |
|---|---|---|
| parafrasi ric/MRR | 1.000/0.982 | 1.000/**0.996** |
| inglese ric/MRR | 1.000/0.976 | 1.000/0.985 |
| fattuali MRR | 0.972 | **1.000** |
| multi-salto | 0.333 | 0.157 |
| embedding (note/s) | **202** | 69 |
| ibrido caldo p50 (ms) | **28.4** | 33.2 |
| modello su disco | **420 MB** | 1 277 MB |

Il vantaggio del 311M sulle parafrasi è 0.014 di MRR, sotto la soglia
di 0.03 che lo terrebbe predefinito: **default 97M**, il 311M resta
l'opzione di qualità a un `model use` di distanza. Sul vault dorato il
97M perde una parafrasi su 15 («che cosa cresce all'aperto
dell'appartamento?», richiamo 0.933): dichiarata nel test, non nascosta.

### Tabelle di chiusura

Testo+grafo (mediana di 3; «viva»; la riga 100k include la correzione
bitemporale della recenza, commit `e9298c2`):

| corpus | note | inserimento (note/s) | apertura (ms) | p50 (ms) | p95 (ms) | solo testo p50 | richiamo@5 | MRR | file (MB) |
|---|---|---|---|---|---|---|---|---|---|
| viva | 10 000 | 3 620 | 1.1 | 6.3 | 10.7 | 1.1 | 0.997 | 0.790 | 24.3 |
| uniform | 10 000 | 3 563 | 1.0 | 5.7 | 9.6 | 1.1 | 1.000 | 0.774 | 24.3 |
| viva | 100 000 | 2 092 | 1.6 | 43.5 | 54.3 | 9.2 | **0.940** | **0.683** | 246.3 |
| uniform | 100 000 | 2 279 | 1.5 | 43.6 | 56.1 | 9.0 | 0.979 | 0.739 | 246.3 |

Ramo vettoriale (mediana di 3 dove indicato):

| modello | note | embedding (note/s) | costr. indice | vettori p50/p95 | ibrido caldo p50/p95 | ibrido + embedding p50/p95 |
|---|---|---|---|---|---|---|
| 97M | 10 000 | 218 | 20 ms | 22.3/28.7 | 22.0/29.7 | 21.6/28.5 |
| 311M | 10 000 | 63 | 54 ms | 35.5/42.2 | 33.1/41.8 | 33.3/43.0 |
| 97M | 100 000 | 183 | 165 ms | 51.1/64.7 | 57.9/70.6 | 57.6/68.5 |

Nota onesta sulle colonne «calde» a 100k: la LRU delle query incorpora
256 domande per modello e il banco ne fa 4 684 — ogni domanda «calda» è
di fatto una re-incorporazione (~25 ms col 97M). Il motore senza
l'incorporazione è la sonda: **7,7/7,9 ms p50/p95 ibrido a 100k**, ed è
quello l'obiettivo del §11 («con la query già incorporata»), rispettato
con margine. Richiamo@5/MRR per modalità e categoria (una tornata
post-correzione, 97M e 311M): in `bench/RESULTS.md`.

Multi-salto a 100k: testo 0.895/0.536; ibrido 0.063 (mm-0.7) – 0.304
(th-0.3) — misura qualcosa in ogni modalità; in ibrido il ramo vettoriale
preme le note «del settore» sopra la catena del partner: altro punto
aperto per la taratura del peso di grafo/fusione in ibrido.

### Prove a mano (97M, uscite trascritte)

```console
$ engramma --root $T search "cosa mangia Fabio adesso" --k 3
1. [active] Fabio onnivoro (00000061)
   liste: text, vector   termini: mang   rango BM25: 2   rango semantico: 1
   coseno: 0.9110   supporto: strong
2. [active] Viaggio a Torino (00000051)   coseno: 0.8220 (semantico 15°)
3. [active] Caffè amaro (00000062)        coseno: 0.8615   supporto: weak
$ engramma --root $T search "where does Fabio work" --k 3
1. [active] L'orario di Fabio (00000086)      # nota di lavoro (Studio Rossi)
   liste: text, vector   coseno: 0.9075   supporto: weak
2. [active] Fabio lavora allo Studio Rossi (00000081)   coseno: 0.9079
$ engramma --root $T verify
registro verificato: 63 operazioni, catena hash, ricostruzione e indice FTS a posto
copertura vettori: granite-embedding-97m-multilingual-r2@835ad140 [ready] 62/62 note
$ engramma --root $T search "campione"
1. [active] Fabio tifa Lazio …   liste: vector   coseno: 0.8224   supporto: weak
$ engramma --root $T search "campione" --strict
nessun risultato sopra soglia
$ engramma --root $T search "che auto guida Giulia" --strict
nessun risultato sopra soglia
```

E via MCP (`memory_search`, cella dorata): «Fabio tifa Lazio |
supporto: weak | coseno: 0.8224 | liste: ['vector']» — l'explain porta
il supporto e la descrizione dello strumento spiega che weak vuol dire
verificare prima di affermare.

### Obiettivi del §11

- inserimento ≥ 2 000 note/s: 2 092–3 620 ✓
- ricerca testo+grafo p95 ≤ 30 ms: 10.7 ms a 10k ✓; a 100k 54.3 ms —
  invariato dalla v0.2 (camminata e FTS del percorso di testo; il
  tavolo della fusione e la sonda mostrano il margine del ramo ibrido)
- **ricerca mista p95 ≤ 50 ms a 100k con query già incorporata:
  7.9 ms (sonda) ✓**
- apertura cella ≤ 50 ms: 1.0–2.2 ms ✓
- sonda: embed_pending lineare (14×) ✓; solo vettori 4.0 ms (aspirazione
  3 ms: mancato di 1 ms, dichiarato); ibrido p95 7.9 ms ✓

### Cosa resta aperto

1. «Solo vettori» a 3 ms: 4.0 — il ricalcolo f32 di 200 candidati e la
   camminata; una selezione a due stadi (ricalcolare prima i 64 migliori
   int8) è la strada naturale.
2. Ibrido e multi-salto: la fusione preme il ramo vettoriale sopra la
   catena del grafo (0.063 contro 0.895 del solo testo a 100k); serve un
   peso che rispetti il percorso, non solo i punteggi.
3. La LRU delle query (256) è piccola per banchi con migliaia di domande:
   le colonne «calde» dei benchmark pagano re-incorporazioni.
4. Il costo della «viva» a 100k (p95 54 ms) è la camminata del grafo del
   percorso di testo — ora con passo 5 000 smorzato; compattare la
   frontiera resta il lavoro del §registro/grafo.

# REPORT — Engramma v0.3 (grafo veloce e utile, registro dimenticabile, compattazione, vista markdown, consolidamento)

Data: 2026-11-06. Base: `main` @ `c9a7f2a`. Un commit per punto (vedi la
nota sotto sui file condivisi), test rosso prima della correzione dove il
punto lo prevedeva. Tutto offline, Rust 1.95, macOS arm64. Crate nuove:
nessuna — `notify` 8.2.1 e `notify-debouncer-full` 0.7.0 erano già nel
lock dalla chore `62dfade`; le feature aggiunte (`backup` di rusqlite,
`signal` di tokio) e la dipendenza piena di `tempfile` (già dev-dep) non
toccano il lock.

## Nota sui commit

I punti 0–3 condividono `cell/`: il registro v2 (punto 1) ha toccato
`execute`, `verify` e `compare_states`, e purge/compattazione (punti 2–3)
vivono negli stessi file. I commit seguono i punti per ciò che è
separabile (graph/search/bench/golden nel punto 0; registro nel 1; CLI
nel 2), ma `cell/{mod,ops,schema}.rs` arriva intero col commit del punto
3 e il commit del punto 0 non compila da solo. La fase è verificata
nell'insieme (fmt/clippy/test/deny verdi albero alla fine).

## Punto 0 — il grafo in memoria col PPR

### Cosa è stato fatto

- **`GraphIndex`** (`crates/engramma-core/src/graph.rs`): nodi = note ed
  entità (nomi intercati lowercase, chiavi `note:<ulid>`), adiacenza CSR
  non orientata sugli archi visibili, costruita pigramente alla prima
  ricerca, aggiornata dalle scritture della stessa `Cell` con una
  sovrapposizione (overlay) sui nodi toccati, ricostruita a cambio di
  `PRAGMA data_version` o quando l'overlay supera 1/8 degli archi. Ogni
  arco porta relazione (interncata), finestra di validità (in microsecondi
  interncati: `as_of` salta gli archi non validi), stato del dichiarante.
- **Due generi di adiacenza.** Archi *reali* = le righe di `edges`.
  Adiacenze *dichiarate* = la proiezione bipartita: la nota che dichiara
  una relazione viene collegata ai suoi estremi (una riga «Giulia
  works_at Studio Rossi» non tocca altrimenti la nota che l'afferma; la
  camminata v0.2 la scopriva attraverso `note_id`, il prototipo la
  legava strutturalmente). `neighbors` e `path` vedono solo gli archi
  reali; la spinta vede entrambi.
- **Camminata ponderata.** Una relazione tipizzata pesa 1, un `mentions`
  0,25: navigazione, non asserzione. Su una camminata piatta i ~40
  riempitivi che menzionano il soggetto affogavano l'unico arco
  `partner_of` (misurato: la lista grafo amplificava il vicinato del
  soggetto invece di estenderlo). È la linea di HippoRAG 2, che cammina
  sul grafo delle triple, non sulle menzioni.
- **Spinta Andersen–Chung–Lang**: si espande un nodo solo se il residuo
  ≥ ε·grado pesato; α teletrasporto, θ lo smorzamento degli hub
  (`θ/deg` sopra θ gradi). ε di **default 10⁻⁴**, non i 10⁻³ del
  prototipo: i semi qui condividono la massa in dieci e a 10⁻³ le note a
  due salti restavano sotto soglia e non si depositavano mai
  (diagnosi `--multihop-debug`, che ora misura anche l'ibrido).
- **Terza lista della fusione**: punteggi PPR delle note (semi esclusi),
  scalati sul massimo della lista — la min-max dava 0 esatto
  all'ultimo della lista e lo scartava (trovato col test «Zoro»).
  Convessa `(1−w)·fuso + w·g`, RRF con reciproco di rango pesato.
  `explain` porta `via` (catena dei predecessori a massa massima) e
  `graph_rank`; `SearchOutcome.graph_pushed_edges` conta gli archi
  spinti. `expand_graph`, `edges_touching_frontier` e `WALK_LIMIT` sono
  andati via del tutto: `neighbors` e `path` girano sull'indice (test
  VDBE a zero sul path dopo il build).
- **In modalità testo il grafo resta supporto** (la regola v0.2.1
  «support, never replace»): il boost porta copertura e recenza. Misurato
  di nuovo: senza la disciplina una nota-persona con un solo termine
  spodestava la risposta di «che lavoro fa Fabio».
- **LRU delle domande configurabile** (`Cell::set_query_cache_capacity`),
  default 4 096; il banco la alza al numero di domande.

### La taratura del peso (criterio dichiarato)

**MRR medio del benchmark a 100k pesato per numero di domande** (fattuali
1913, parafrasi 1533, multi-salto 237, inglese 1001; a parità vince chi
tiene le parafrasi), come in v0.2.1:

| peso grafo | MRR pesato | multi-salto ibrido | parafrasi (100k) |
|---|---|---|---|
| v0.2.1 (camminata SQL) | 0,839 | 0,063/0,019 | 0,993/0,878 |
| 0,25 | 0,849 | 0,536/0,165 | 0,993/0,878 |
| 0,30 | 0,850 | 0,692/0,248 | 0,993/0,877 |
| 0,35 | 0,853 | 0,865/0,356 | 0,993/0,873 |
| **0,40 (predefinito)** | **0,855** | **0,949/0,503** | 0,992/0,864 |

0,40 vince sul criterio ed è l'unico che porta il multi-salto ibrido
sopra il solo testo − 0,05 (0,949 contro 0,916). Il prezzo: le parafrasi
cedono 0,014 di MRR (richiamo 0,993→0,992, dentro l'1%) e sul vault
dorato (65 note, massa piatta) la pagina-persona del soggetto — a un
salto dall'hub — può stare sopra la risposta: le due domande manuali
ibride del test v0.2.1 ora vincono nei primi **due**, non più al primo
(test aggiornato con la deriva dichiarata).

### Le domande dorate multi-salto

Il prompt chiedeva ≥ 5 domande che il solo testo (con `graph_hops = 0`)
**non** risolve: c'è un test dedicato (`multihop_questions_need_the_graph`).
La storia è dovuta cambiare (il suggerimento era proprio «la sorella di
Giulia lavora altrove»):

- Sara lavora alla Biblioteca Angelica (nota nuova 112, con la relazione
  dichiarata); Davide sposato con Sara; la Dott.ssa Conti segue Giulia;
  Marco «capo dei progetti» (non più «capoprogetto di Fabio» nel corpo —
  il testo la trovava da solo); il matrimonio di Sara ha perso la parola
  «sorella» dal corpo e dal titolo; le note chiave portano le relazioni
  nella sezione `## Relations` (il vincolo sintattico che la catena usa).
- Le cinque di `tests/golden/queries.json` (v0.3.1, P2.7 — la lista qui
  sopra citava due domande che nel file non sono): «dove lavora il
  partner di Fabio», «chi è la sorella della partner di Fabio», «chi è il
  marito della sorella di Giulia», «dove lavora la sorella di Giulia»,
  «dove vive la sorella di Giulia». In testo+grafo, all'istante fisso del
  test, tutte e cinque rispondono nei primi 5 (test `golden`); in ibrido
  le attese accettano solo la nota che afferma il fatto e le due
  domande sulla sorella restano fuori dai primi 5 — numeri nella
  sezione v0.3.1.
- **Deriva onesta all'orologio vero**: alle prove a mano (oggi, recenza
  viva) tre su cinque rispondono nei primi 5; per le altre due il match
  lessicale (la Dott.ssa Ricci contiene «medico di base di Fabio»)
  supera la catena. Le catene ci sono — l'`explain` le mostra — ma il
  peso grafo 0,40 distribuisce la lista sulle pagine-persona. È lo stesso
  compromesso della taratura, dichiarato: su un vault piccolo la recenza
  e il lessico vincono ancora.

### Obiettivi (benchmark a 100k, 97M, mediana di 3, cache calda vera)

Cache calda vera = LRU con capacità uguale al numero di domande del
corpus (4 684 a 100k): il banco la imposta prima dei pass; il «warm» non
re-incorpora più nulla (il pass a freddo sulle stesse domande fa da
riscaldamento). Numeri nella tabella di chiusura.

## Punto 1 — registro v2, payload dimenticabili

- `payload_digest = blake3(sale ‖ JSON canonico del corpo)`,
  `hash = blake3(prev ‖ JSON canonico della busta {op_id, hlc, actor,
  kind, note_id, payload_digest})`: le colonne SONO la busta. Il corpo
  vive in `op_payloads(op_id, sale, corpo)`; il sale (16 byte) si genera
  alla creazione dell'operazione, come id e HLC — `apply` resta
  deterministica.
- **Redazione**: il rigioco di un'operazione dimenticata è un no-op
  (scheletro + tick dell'HLC). Spec in `spec/format-v0.md` §6.2 con i
  vettori dorati v2 (sale fisso) e il vettore v1 congelato per la
  migrazione.
- **Migrazione v5**: verifica la catena v1 (rotta → rifiuto), riscrive
  tutto in forma v2 con sali nuovi, bookmark `log_v1_head` e
  `log_v2_from_seq` in `meta` (fuori dal confronto di stato: nessuna
  operazione li scrive). Il test costruisce la cella v1 copiando lo stato
  di una cella reale e riscrivendo il registro con la regola v1.
- Test richiesti: dimenticare un payload mantiene la catena (la verifica
  completa passa quando l'effetto è interamente sostituito da una
  scrittura successiva, o dopo `purge`); un byte del corpo cambiato →
  verifica fallisce; il sale cambiato → fallisce; proptest: operazioni
  casuali (ora anche `purge`, vedi sotto) → il rigioco dà lo stesso
  stato.
- Scoperta dei proptest: il confronto di stato usa l'insieme **derivato**
  delle entità (estremi degli archi ∪ soggetti delle note vive): la
  tabella grezza è un indice il cui storia sotto redazione diverge
  legittimamente (l'ultimo che reclama un titolo vince, e una purga
  sgancia il reclamo). Anche i tag orfani vengono potati dalla purga.

## Punto 2 — purge

- `purge(note_id, reason, actor)`: nota, versioni, archi dichiarati
  (derivati e manuali), vettori, riga FTS, entità rimaste senza
  riferimenti (con **riderivazione del reclamo**: conta solo il reclamo
  di una nota viva) e tag senza portatori. `secure_delete = ON` durante
  l'operazione e `VACUUM` dopo: **nessun byte del corpo nel file**
  (test che legge il file grezzo).
- **Dimenticanza selettiva**: i payload delle operazioni della nota; le
  `supersede` dove la nota è la NUOVA vengono **censurate** (nota→null,
  sale nuovo, coda della catena ricalcolata) perché la chiusura della
  vecchia deve restare rigiocabile; dove la nota è la VECCHIA il payload
  resta (contiene i dati dell'altra nota) e `apply_supersede` impara a
  creare comunque la nuova quando la vecchia manca (rigioco redatto).
- `purge` è idempotente; le menzioni `[[Nome]]` in altre note restano
  (documentato nella CLI e nel REPORT); le esportazioni già fatte sono
  dichiarate fuori controllo nell'aiuto.
- Test richiesti tutti verdi, incluso il rigioco (verify ok dopo purge).

## Punto 3 — compattazione

- `compact(up_to_seq)`: `VACUUM INTO` un'istantanea della cella **allo
  stato attuale** (la testa), registrata in `snapshots(seq, head_hash,
  state_digest, file, created_at)`; i payload fino a `up_to_seq` (≤
  testa: `--keep-last` tiene una coda rigiocabile) vengono dimenticati.
- `state_digest(conn)` è pubblica: blake3 del dump canonico delle
  tabelle logiche (lo stesso rendering di `compare_states`, che ora
  condivide il codice).
- `verify` dopo la compattazione: catena intera; ripristina
  l'istantanea in memoria (API backup di rusqlite), ne controlla il
  digest registrato, **riapplica lo stesso taglio** alla copia (il file
  d'istantanea è anteriore alla dimenticanza) e rigioca solo la coda;
  trova la testa d'istantanea dal suo hash (che può superare il taglio).
- `changes_since` sotto il taglio → `CellError::Compacted
  { snapshot_seq }`; istantanea manomessa → verify fallisce
  (test dedicato).
- CLI `engramma compact [--keep-last N]` (predefinito 1000). Il banco
  misura il file prima e dopo (tabella di chiusura).
- Proptest: operazioni casuali con purge, compattazione, altre
  operazioni → verify ok e il digest dell'istantanea è quello del rigioco
  completo fatto prima di compattare.

## Punto 4 — la vista markdown

- **Riconciliazione per scansione** (`sync_once`), deterministica:
  `vault_files` (migrazione v7, dati derivati fuori dal registro) è lo
  stato dell'ultima riconciliazione; il file è legato alla nota
  dall'`id` nell'intestazione. Ogni riga della tabella del prompt ha il
  suo test: file cambiato/nota invariata, invariato/cambiata (riscrittura
  atomica: temporaneo + rename), entrambi (conflitto: vince la cella,
  copia `.conflitto-<ts>.md` che lo scanner ignora), nuovo senza id
  (nota nuova con le regole dell'import Obsidian, file riscritto CON
  l'id), sparito (lapide).
- Aggiunte oltre la tabella, misurate dai test: le note vive senza file
  (scritte dopo `init`) vengono esportate; una nota purgata o lapidata
  trascina con sé il suo file; lo spostamento/rinomina aggiorna il
  legame senza cancellare niente e senza far scattare la guardia.
- **Guardia**: più di 20 file **o** più del 10% delle note spariti
  insieme → `MassDelete`, niente cancellato, `--allow-deletions` per
  procedere. Nota: nei vault piccoli la soglia percentuale scatta prima
  di quella assoluta (dieci note, un file cancellato = 10%) — è la
  lettera del prompt, dichiarata.
- **Eco**: l'hash del file scritto da noi viene registrato; il test del
  punto fisso la verifica insieme a tutto il resto.
- Validazione: intestazione rotta → errore nel riepilogo, cella intatta;
  un file non può promuovere una nota `external` ad `active` (warning e
  quarantena). `.obsidian/`, `.trash/`, nascosti, symlink e copie di
  conflitto saltati.
- `watch`: notify + debouncer (500 ms) **come innesco**, scansione
  completa ogni 60 s come rete di sicurezza, fermo pulito. Test
  d'integrazione: una modifica a mano diventa una nota aggiornata entro
  3 s (passato qui su FSEvents; marcato potenzialmente instabile in CI
  come chiedeva il prompt).
- **Proptest del punto fisso**: otto semi, dodici giri di modifiche
  casuali da file e dalla cella con sincronizzazioni sparse; alla fine
  due `sync_once` di fila — la seconda non fa nulla, ogni nota viva ha
  il suo file con contenuto `serialize(nota)` esatto.
- CLI: `vault init`, `vault sync [--allow-deletions]`, `vault watch`.

## Punto 5 — candidati al consolidamento

- `consolidation_candidates(limit, cfg)` in sola lettura
  (`crates/engramma-core/src/consolidation.rs`):
  - **quasi duplicati**: coseno ≥ 0,95 (ricalcolato esatto dai vettori
    del modello ready; le proposte arrivano dall'indice int8 della
    ricerca) **e** Jaccard delle entità ≥ 0,5 (menzioni, relazioni,
    soggetto);
  - **contraddizioni probabili**: stessa entità soggetto, stessa
    relazione funzionale (predefinite `lives_in`, `works_at`,
    `partner_of`), destinazioni diverse, note attive, validità
    sovrapposte — anche **nella stessa nota** (il caso del prompt: due
    `lives_in` sulla pagina di Luca senza supersede).
- Vault dorato: aggiunti il doppione del bagno (nota 115, Jaccard 1,00
  con la 041) e i due `lives_in` aperti di Luca. Test col 97M: entrambi
  trovati, nessun falso positivo dalle catene chiuse. Il test senza
  modello copre le contraddizioni.
- CLI `engramma consolidate [--limit N] [--json]`; niente MCP.

## Punto 6 — documentazione

`docs/architecture.md` (GraphIndex e PPR, registro v2, compattazione,
vista, stato logico sotto redazione), `docs/cli.md` (purge, compact,
vault, consolidate), `spec/format-v0.md` (§6 riscritto: regola v2,
vettori dorati, redazione, migrazione, istantanee), `README.md` (vista
Obsidian, purge e i suoi limiti).

## Chiusura

`cargo fmt --check`, `cargo clippy --all-targets --offline -- -D
warnings`, `cargo test --offline --workspace` (159+18 test di integrazione
debug, **184 in --release**), `cargo deny check licenses bans`: verdi.
(Un flake noto e preesistente: il test bearer-token di
`engramma-embed/src/http.rs` fa girare un server TCP su una porta locale
e in parallelo ogni tanto perde la gara — riprodotto anche sull'albero
pulito a inizio fase.)

### Le tabelle

**Il punto 0 prima/dopo** (100k, 97M, mediana di 3, cache calda vera =
LRU con capacità 4 684 = tutte le domande; v0.2.1 dal REPORT precedente,
che misurava «caldo» con una LRU da 256 e quindi re-incorporava):

| misura | v0.2.1 | v0.3 |
|---|---|---|
| ibrido p50 / p95 (ms) | 48,5 / 59,8 | **7,7 / 11,2** |
| ibrido + incorporazione p50 / p95 (ms) | 57,6 / 68,5 | **19,4 / 22,5** |
| solo vettori p50 / p95 (ms) | 51,1 / 64,7 | 4,2 / 4,7 |
| solo testo (hops 0) p50 (ms) | 9,2 | 7,5 |
| testo+grafo p50 / p95 (ms) | ~43,5 / 54,3 (era la riga «ricerca») | **7,6 / 10,5** |
| multi-salto ibrido (ric/MRR) | 0,063 / 0,019 | **0,949 / 0,503** |
| multi-salto solo testo | 0,895 / 0,536 | 0,895 / 0,536 |
| richiamo@5 / MRR (viva) | 0,940 / 0,683 | 0,940 / 0,683 |
| inserimento (note/s) | 2 092 | 1 486 (due righe per op: scheletro + payload) |
| apertura cella (ms) | 1,6 | 1,6 |

Obiettivi del §11 e del prompt: ibrido p95 ≤ 50 → **11,2** ✓; solo testo
p95 ≤ 30 → **10,5** (testo+grafo a 2 salti) ✓; multi-salto ibrido ≥
solo testo − 0,05 → 0,949 ≥ 0,845 ✓; nessuna categoria peggiora oltre
0,01 → fattuali 0,998→0,996, parafrasi 0,993→0,992, inglese 0,994→0,990
(richiamo; le parafrasi cedono 0,014 di MRR: il compromesso dichiarato
della taratura) ✓; apertura ≤ 50 ms → 1,6 ✓.

**Il grafo in memoria** (sonda dedicata, prima di ogni ricerca):

| corpus | costruzione | nodi | archi | memoria |
|---|---|---|---|---|
| 10k | 11 ms | 10 008 | 17 322 | 2,2 MB |
| 100k | 123 ms | 100 090 | 184 459 | 21,6 MB |

Archi spinti per domanda (media ibrido): 1 001 a 10k, 695 a 100k — più
del prototipo (157) perché il grafo vero ha la proiezione dichiarata e
persone con più relazioni, ma sempre O(centinaia) contro le migliaia
della camminata SQL.

**La compattazione** (sonda: `compact(head − 1000)` poi `verify` ok):

| corpus | prima | dopo | tempo |
|---|---|---|---|
| 10k | 24,3 MB | 19,2 MB (−21%) | 0,1 s |
| 100k | 246,1 MB | 190,0 MB (−23%) | 1,8 s |

(Le celle col modello ready: 465,0 → 408,8 MB a 100k — la parte
recuperata sono i payload, i vettori restano.) Nella prima tiratura del
banco la compattazione non rimpiccioliva il file vivo: le pagine libere
restano a SQLite senza un `VACUUM` della cella — corretto (la
compattazione ora fa il VACUUM anche della cella viva), e la
ricompattazione alla stessa testa sostituisce la propria istantanea
(`VACUUM INTO` rifiuta i file esistenti).

Tabella completa e per-modalità in `bench/RESULTS.md`.

### Prove a mano (uscite trascritte)

```console
$ engramma --root $T init && engramma --root $T model use …97M…
$ engramma --root $T import tests/golden/vault-it --obsidian
incorporate: 66 (in attesa: 0) / importate: 66 / errori: 0
$ engramma --root $T verify
registro verificato: 67 operazioni, catena hash, ricostruzione e indice FTS a posto
copertura vettori: granite-embedding-97m-multilingual-r2@835ad140 [ready] 66/66 note
```

Le cinque multi-salto in ibrido (primi risultati e `via`; all'epoca le
attese accettavano anche il passo intermedio — v0.3.1, P2.7, ha ristretto
le attese alla sola nota che afferma il fatto, e due di queste cinque
possono cadere dai primi 5: numeri nella sezione v0.3.1):

```console
$ engramma --root $T search "dove lavora il partner di Fabio" --k 3
1. [active] Giulia (00000002)
   liste: text, vector, graph
   via: [[nota 00000081]] -mentions-> [[Milano]] -lives_in-> (nota 00000002)
$ engramma --root $T search "chi è il marito della sorella di Giulia" --k 3
1. [active] Sara (00000005)
   liste: text, vector, graph   via: [[nota 00000091]] -mentions-> [[Roma]] -lives_in-> (nota 00000005)
```

La vista:

```console
$ engramma --root $T vault init $V
vault inizializzato: …/vault
$ engramma --root $T vault sync $V          # subito dopo init
nulla da fare                              # l'eco non rientra
$ sed -i '' 's/amaro/dolce/' $V/preference/caffe-amaro-00000062.md
$ engramma --root $T vault sync $V
note aggiornate da file: 1
  - preference/caffe-amaro-00000062.md
$ engramma --root $T get 00000062 --json | grep body
"body": "[[Fabio]] prende il caffè dolce, senza zucchero: da sempre.\n"
$ EDITOR=script-che-modifica engramma --root $T edit 00000062   # dalla CLI
$ engramma --root $T vault sync $V
conflitti (vince la cella, copia .conflitto): 1
  - preference/caffe-amaro-00000062.md
$ grep caffè $V/preference/caffe-amaro-00000062.conflitto-2026-11-06T22_52_52.md
…caffè amaro, senza zucchero…              # la versione del file, salvata
$ engramma --root $T vault sync $V
nulla da fare                              # punto fisso
```

Purge e compattazione:

```console
$ engramma --root $T purge 01M6SH1T… --reason "prova oblio" --yes
purgata: 01M6SH1T1FM5GHE3XSGZ94W535 (1 payload dimenticati)
$ grep -c "prova-purge-zanzara-…" $T/cells/default.sqlite
0                                          # nessun byte nel file
$ engramma --root $T history 01M6SH1T1F…
engramma: note … not found
$ engramma --root $T compact --keep-last 1
compattata fino a seq 70 (testa 71): …/default-snap-71.sqlite
$ engramma --root $T verify
registro verificato: 71 operazioni, catena hash, ricostruzione e indice FTS a posto
$ engramma --root $T consolidate
[quasi duplicato] Ristrutturazione bagno (00000041) ↔ Il piano del bagno (appunto) (00000115)
  coseno 0.964, entità in comune 1.00
[contraddizione probabile] Luca Ferrari (00000007) ↔ Luca Ferrari (00000007)
  Luca Ferrari lives_in Roma (from 2026-09-01 to ora) e lives_in Torino (from 2024-01-01 to ora)
… (quasi duplicati anche per Luca Ferrari/Luca e la matematica, 0.961,
  ed Elena Bianchi/Il ruolo di Elena Bianchi, 0.954)
```

## Cosa non ho potuto provare

- **Windows e Linux**: solo macOS arm64, come nelle fasi precedenti.
- **Il watch su CI**: il test d'integrazione (3 s) passa qui su FSEvents;
  marcato come potenzialmente instabile in ambienti condivisi.
- **La migrazione v5 su celle v0.2.1 vere**: il test costruisce il file
  v1 copiando lo stato di una cella reale e riscrivendo il registro con
  la regola v1 congelata — non ho aperto celle prodotte dal binario del
  tag (il formato è equivalente per costruzione: stesse colonne, stessa
  regola, stesso hash dorato).
- **Il 311M a 100k**: come v0.2.1, dichiarato (il 97M è il modello
  predefinito).
- **cargo deny advisories**: richiede rete; eseguito `licenses bans`.

## Domande aperte per la v0.4

1. **Il compromesso del peso grafo** (0,40): le parafrasi cedono 0,014
   di MRR a 100k e i vault piccoli vedono le pagine-persona sopra le
   risposte. Un peso che dipenda dalla densità del grafo (o un PPR su
   semi meno numerosi ma riponderati) potrebbe tenersi entrambi.
2. **L'astensione semantica resta aperta** (v0.2.1): col vault cresciuto
   «che auto guida Giulia» tiene un sopravvissuto strong in --strict
   (margine 0,043 sulla soglia 0,03) e 11/15 parafrasi cadono — la
   curva è nel test aggiornato.
3. **Il collegamento Node** (napi-rs) e l'integrazione in Homun: la
   vista markdown e il registro dimenticabile cambiano il modo in cui
   Homun dovrebbe scrivere (attore `vault`? purge dal turno contaminato?).
4. **`neighbors`/`path` con l'indice**: ora ignorano gli archi con
   validità non più valida solo in ricerca (`as_of`); `neighbors` non ha
   filtro temporale — servirà?
5. **La guardia della vista** scatta per percentuale nei vault piccoli:
   una soglia minima assoluta (per esempio «meno di 5 file non conta»)
   sarebbe più usabile; la lettera del prompt diceva altrimenti.

# REPORT — Engramma v0.3.1 (correzioni della verifica v0.3)

Data: 2026-11-07. Base: `main` @ `755b304` (tag `fase-v0.3`). Dodici
commit, uno per punto più le pulizie; il test rosso prima della correzione
per ogni punto; ogni commit compila e passa i test da solo (l'obbligo che la
v0.3 aveva violato). Tutto offline, Rust 1.95, macOS arm64, nessuna crate
nuova, nessun `unsafe`, nessun `unwrap`/`expect` fuori dai test.

## Punto 1 — l'FTS tiene le radici, l'optimize le scaccia

Il test della v0.3 cercava la parola intera; l'indice conserva le radici
dopo lo stemmer, e i segmenti vecchi le tengono anche dopo il `'delete'`
(che scrive una lapide in un segmento NUOVO). `purge` esegue ora
`INSERT INTO notes_fts(notes_fts) VALUES('optimize')` dentro la finestra di
`secure_delete`: i segmenti si fondono, le voci cancellate se ne vanno, le
pagine liberate si azzerano. Costo a 100k: dentro la purge completa
(3,4–4,1 s, tabella sotto); il `sample` della purge non lo isola ma
l'optimize è O(indice) una tantum.

Il test dell'oblio (`tests/forgetting.rs`) cerca **ogni radice** dei
termini del corpo — passate dallo stemmer del motore, non la sola parola
intera — nel file grezzo: rosso prima della correzione («quokkaviolace»
sopravviveva), verde dopo.

## Punto 2 — il WAL si tronca, e la purge non mente

Dopo il `VACUUM`, `PRAGMA wal_checkpoint(TRUNCATE)`; se un lettore tiene
una transazione aperta il checkpoint risponde busy e la purge risponde
`PurgeReport { forgotten_payloads, scrubbed: false, pending: ["wal"] }`:
il retry scatta alla scrittura successiva e alla chiusura della cella
(`Cell::scrub_state()` interroga lo stato), la CLI esce con **codice 5**.
Nel percorso di purge non c'è più alcun `let _ =`: un `VACUUM` o PRAGMA
fallito è un errore (e `PRAGMA secure_delete` si legge come l'intero che
è: prima il fallback silenzioso lo scriveva sempre come stringa). Il busy
timeout si accorza a 250 ms intorno al checkpoint: con i 10 s della cella
un lettore lento avrebbe stallato la purge dieci secondi per dire «busy».

Dichiarazione: il pending vive in memoria; un processo che muore con il
WAL non troncato lascia i frame al prossimo checkpoint esplicito o alla
prossima purge — la riga non dice mai «fatto» ma nemmeno sopravvive al
riavvio.

## Punto 3 — la purge raggiunge le istantanee

Ogni istantanea registrata riceve la stessa transizione di stato e lo
stesso lavaggio del registro (payload dimenticati, supersede censurate),
poi optimize di FTS e VACUUM; la riga aggiorna il digest tenendo
`head_hash` come documento storico e la colonna nuova `redacted_by`
(migrazione v8) registra gli op_id delle purge applicate al file.

La parte delicata era la CENSURA dentro l'istantanea: il vivo e la copia
devono produrre lo stesso registro. Il sale della censura diventa
deterministico (blake3 dell'op_id: il corpo censurato non contiene più
nulla della nota purgata, non c'è nulla da scoppiare), e le supersede il
cui payload il vivo ha GIÀ dimenticato col taglio di compattazione
nell'istantanea si cancellano e basta — il digest originale deve restare
quello del vivo. `verify` accetta un digest redatto solo se il rigioco
della coda — purge comprese, sono operazioni in coda — dà lo stato vivo;
la posizione dell'istantanea si trova per l'ultimo op_id del file (la
censura rehash la coda), e il vincolo sull'head_hash resta per le
istantanee mai redatte. Regola scritta in `spec/format-v0.md` §6.4.

## Punto 4 — l'istantanea è lo stato logico, e resta sola

Dopo `VACUUM INTO` il file viene spogliato: payload fino al taglio,
vettori, registro dei modelli, legami della vista, contenuto FTS
(`delete-all`: si ricostruisce — e `verify` lo ricostruisce dalle sole
note vive prima di rigiocare la coda, perché il comando `rebuild`
indicerebbe anche le lapidi). Poi `VACUUM` e digest di ciò che resta. Si
tiene solo l'ultima: nuova scritta, riga registrata, poi vecchia riga (per
`(seq, file)`: alla stessa testa il seq si riusa e la riga nuova deve
sopravvivere — trovato dal proptest) e vecchio file. Una riga fantasma
senza file (crash tra riga e file) la droppa la prossima purge; la più
recente che manca è un errore, non un silenzio.

**Spazio, cella + istantanea** (banco, mediana di 3; «totale dopo» è ciò
che occupa davvero la directory):

| corpus | prima (cella) | dopo: cella | istantanea | totale dopo | copia piena sarebbe |
|---|---|---|---|---|---|
| 10k | 24,3 MB | 19,2 MB | 18,1 MB | 37,3 MB | ≈ 24,3 MB |
| 100k | 246,1 MB | 189,9 MB | 179,0 MB | 368,9 MB | ≈ 246,1 MB |

Con le copie piene della v0.3 il totale dopo era cella + una copia
integra per OGNI compattazione mai fatta (a mano: due compact → due
istantanee, la più grande della cella di partenza). Ora: una sola
istantanea, logicamente più piccola della cella viva (−27% a 100k), e il
taglio resta riapplicato in `verify` come passo idempotente — alle copie
piene delle celle v0.3 serve ancora, alle nuove non costa nulla.

Proptest esteso: operazioni, `compact` e `purge` in ordine casuale →
verify ok e nessuna radice delle note purgate in nessun file (parole
inventate a prefisso distinto: lo stem di una parola purgata non può
essere sottostringa di una viva — il primo generatore, col suffisso
discriminante, produceva falsi positivi).

## Punto 5 — l'intestazione Obsidian si legge

`format::parse_obsidian` (nuova): con `MissingField` l'intestazione
dell'utente si LEGGE — `title` (senza: il nome del file senza estensione,
come in Obsidian), `tags` (lista o stringa), `aliases` conservati, `type`
se valido, `created`/`date` se data — le chiavi sconosciute vanno in
`extensions` e restano nel file, il corpo è quello dopo l'intestazione, i
warning restano. La modalità import di `parse` ora CONSERVA le chiavi
sconosciute nelle estensioni (prima: warning e butto — il giro
dell'informazione mangiava i campi dell'utente); la strict rifiuta ancora,
e le chiavi dei campi standard con valori inutilizzabili si scartano col
warning per non duplicare la riga che il serializzatore possiede. La
riscrittura con l'id fonde: un solo blocco `---`. Vale per
`import --obsidian` e per la vista.

Dichiarazione: l'export di una nota con chiavi dell'utente le porta di
ferro nel file, e solo un'importazione in modalità import le rilegge (le
cartelle Engramma restano strict); la proprietà export→import senza
perdite vale per le estensioni `x-*`, che è ogni nota nata in Engramma.

## Punto 6 — una nota, un file

La fase «note vive senza file → esporta» vede i legami creati nello
stesso giro (prima: il file nuovo diventava nota E la fase esportava la
copia `note/<slug>-<id8>.md`, due righe in `vault_files` e le modifiche
che rimbalzavano). `vault_files` guadagna `UNIQUE(note_id)` (migrazione
v9, con dedup delle righe doppie), `vault_file_put` al conflitto sul nota
rilega al percorso nuovo. Una seconda copia con lo stesso id di un file
ancora presente non è uno spostamento: il legame vince, la copia viene
denunciata e lasciata stare — prima si rubavano il legame a vicenda a
ogni sync. Il proptest del punto fisso genera anche file nuovi con
intestazione Obsidian e verifica una riga e un file per nota.

## Punto 7 — le multi-salto accettano solo la risposta

`queries.json`: «chi è il marito della sorella di Giulia» attende ora
Davide e la nota del matrimonio (non Sara, che è il passo intermedio);
«dove lavora la sorella di Giulia» attende «Sara e la Biblioteca
Angelica» (non la pagina di Sara, non il nome del luogo). In testo+grafo
le cinque del file passano tutte nei primi 5. In ibrido le due domande
sulla sorella restano fuori dai primi 5 — dichiarato nel test con i
numeri, attesa non allargata:

- «chi è il marito della sorella di Giulia» → [Giulia, Sara, Il
  compleanno di Giulia, Fabio, Dott.ssa Conti]
- «dove lavora la sorella di Giulia» → [Giulia lavora allo Studio Rossi,
  Fabio, Giulia architetta, Sara, Studio Rossi]

Il REPORT v0.3 citava due domande («il medico del partner di Fabio», «il
capoprogetto per cui lavora Fabio») che in `queries.json` non sono mai
state: la sezione è stata allineata alle cinque reali del file.

## Punto 8 — l'hub non duplica

Scelto «escludi le coppie hub-nota» (l'alternativa «stesso type» uccideva
anche il doppione vero: il bagno dorato è un progetto contro un fatto).
Una pagina `person`/`place` parla del proprio soggetto per costruzione:
non duplica una nota di altro tipo. Test dorato: Luca ↔ «Luca e la
matematica» (0,961) ed Elena ↔ «Il ruolo di Elena Bianchi» (0,954) fuori,
il doppione del bagno dentro, nessuna coppia hub-nota.

## Punto 9 — dove va il tempo di una put_note a 100k

`engramma-bench --profile-insert --notes 100000 --seconds 25`, campionato
con `sample` (12 s sulla costruzione, 8 s sulla fase sostenuta).
Costruzione: 100 000 note in 75,5 s (1 324 note/s); sostenuta a cella
piena: 871 put_note/s. Il profilo (inclusive, thread principale):

- **≈ 69% nel commit** della transazione, di cui la gran parte nella
  scrittura dei frame WAL (`pwrite` sotto `walWriteOneFrame` ≈ 32%
  dell'intero thread) — è il prezzo di una transazione per nota;
- ≈ 5% nel checkpoint automatico del WAL (scatta ogni 1000 pagine, sul
  thread che scrive);
- ≈ 24% in `ops::apply`, di cui `reindex_note` + `write_note` ≈ 4%
  (rigenerazione FTS/entità della nota riscritta);
- ≈ 1–4% nella preparazione delle istruzioni (una `prepare` per chiamata:
  rusqlite non le mette in cache).

Verdetto: nessuno spreco evidente oltre la soglia dichiarata — il tempo è
nel commit WAL (le «due righe per operazione» ci stanno dentro: scheletro
e payload sono due INSERT nella stessa transazione, due frame in più su
cento pagine). Non si corregge; la preparazione memorizzabile delle
istruzioni è la prima candidata se la v0.4 vorrà riprendersi il passo.

## Punto 10 — la taratura congiunta α × peso del grafo

`engramma-bench --calibrate --notes 100000 --runs 3`: griglia completa,
97M, cache calda vera, MRR medio pesato per numero di domande (fattuali
1913, parafrasi 1533, multi-salto 237, inglese 1001); a parità entro
0,002 vince chi tiene le parafrasi.

| α \ g | 0,25 | 0,30 | 0,35 | 0,40 | 0,50 |
|---|---|---|---|---|---|
| 0,4 | 0,8215 | 0,8368 | 0,8397 | 0,8390 | 0,6993 |
| 0,5 | 0,8379 | 0,8425 | 0,8577 | 0,8577 | 0,7118 |
| 0,6 | 0,8440 | 0,8483 | 0,8527 | **0,8656** | 0,7065 |
| 0,7 | 0,8460 | 0,8497 | 0,8529 | 0,8552 | 0,7015 |

**Vince α = 0,6 con grafo 0,40**: 0,8656 contro 0,8552 del predefinito
v0.3 (oltre la barra di pareggio), multi-salto 0,795 contro 0,503,
parafrasi 0,862. Nuovo predefinito della fusione convessa.

Deriva dorata dichiarata (vault di 65 note: meno peso al testo, le
pagine-persona salgono): «cosa mangia Fabio adesso» ha la risposta terza
(era seconda; l'atteso entro i primi 5), «in che città si è trasferita la
coppia?» risponde prima «Milano»/«Roma» (il fatto è sesto). Dichiarate
nei test col numero, attese non allargate — stesso compromesso già
dichiarato in v0.3 per il peso del grafo, ora leggermente più marcato.

## Punto 11 — le colonne del banco dicono la verità

La colonna «GraphIndex (ms)» stampava 0: la seconda `build_graph` del
giro semantico misurava un no-op a caldo; ora la prima costruzione È la
misura e il giro semantico la riusa. Le colonne «testo+grafo» di viva e
uniform stampavano 0,0 perché i numeri vivevano solo nel rapporto
semantico: diventano campi di ogni giro. L'intestazione dice v0.3. Il
banco ora misura anche l'istantanea e il tempo di una purge a corpus
pieno, e cancella DAVVERO le proprie celle (il nome dell'istantanea era
costruito con `with_extension` e non corrispondeva: i file snap restavano
in `T/engramma-bench`; la sonda fake non si ripuliva affatto — gli 885 MB
della verifica).

## Chiusura

`cargo fmt --check`, `cargo clippy --all-targets --offline -- -D
warnings`, `cargo test --offline --workspace` (**195 test** in debug,
altrettanti in `--release`), `cargo deny check licenses bans`: verdi. (Flake
noto preesistente: il test bearer-token di `engramma-embed/src/http.rs`
in `--release` ogni tanto perde la gara sulla porta locale.)

### Le latenze: ieri, oggi e il confronto A/B

| misura (100k, 97M, mediana di 3) | v0.3 (ieri, REPORT) | v0.3 (ieri, RESULTS) | v0.3 (oggi, binario del tag) | v0.3.1 (oggi) |
|---|---|---|---|---|
| ibrido caldo p50 / p95 (ms) | 7,7 / 11,2 | 10,1 / 13,3 | **11,7 / 14,9** | 11,6–11,7 / 14,8 |
| testo+grafo p50 / p95 (ms) | 7,6 / 10,5 | — (colonna rotta) | 9,1 / 12,3 | 9,1–9,2 / 12,4 |
| solo vettori p50 / p95 (ms) | 4,2 / 4,7 | 4,2 / 4,7 | 4,5 / 5,1 | 4,4–4,5 / 5,1 |
| inserimento (note/s) | 2 092 | 1 486–2 514 | 2 368–2 695 | 2 191–2 688 |

I numeri assoluti di IERI e di OGGI non si confrontano: la macchina non
era nello stesso stato (il testo p50 è identico, il grafo uguale al
millisecondo, ma tutto ciò che tocca i vettori sale del 5–15%). Il
confronto onesto è il binario del tag `fase-v0.3` contro questo albero,
stesso giorno, stessa macchina: **11,7/14,9 contro 11,6–11,7/14,8** —
nessuna regressione del codice (il +10% della soglia è rispettato
alla lettera nel confronto like-for-like; contro i numeri di ieri del
REPORT la differenza è della macchina, non delle modifiche — nessuna di
esse tocca il percorso di ricerca, se non la costante α).

**Il costo dell'oblio a corpus pieno**: una purge a 100k (taglio FTS
`optimize`, `VACUUM`, checkpoint, lavaggio dell'istantanea) prende
3,4–3,6 s sulla cella di solo testo (190 MB) e 4,1 s su quella col modello
ready (407 MB). È un'operazione rara che deve durare quanto dura: la
diritto all'oblio non si misura in millisecondi.

Tabella completa e per-modalità in `bench/RESULTS.md`.

### Prove a mano (uscite trascritte)

Vault dorato importato, modello 97M pronto. La tabella P0 della verifica,
ripetuta con le **radici**:

```console
$ engramma --root $T purge 01M6TKBRSY… --reason "prova oblio" --yes
purgata: 01M6TKBRSY3YXAQZDXDY82Y29F (1 payload dimenticati)   # exit 0
$ for n in quokkaviolace zibellinotrem segret parol ordin; do …
quokkaviolace: sqlite=0 wal=0
zibellinotrem: sqlite=0 wal=0
segret: sqlite=0 wal=0
parol: sqlite=0 wal=0
ordin: sqlite=0 wal=0

$ engramma --root $T compact --keep-last 1
compattata fino a seq 69 (testa 70): …/default-snap-70.sqlite
$ engramma --root $T purge 6XD3NDAC --reason "oblio dopo compattazione" --yes
purgata: 01M6TKDBMXHN… (1 payload dimenticati)                # exit 0
default.sqlite / ovovivipar: 0    default-snap-70.sqlite / ovovivipar: 0
default.sqlite-wal / fluorescent: 0   default-snap-70.sqlite / fluorescent: 0
$ engramma --root $T verify
registro verificato: 71 operazioni, catena hash, ricostruzione e indice FTS a posto

# con un lettore (transazione di lettura aperta in un altro processo):
$ engramma --root $T purge … --reason "oblio col lettore" --yes
purgata: … (1 payload dimenticati)
engramma: oblio incompleto (wal): il WAL non è stato troncato, un lettore
teneva la cella aperta; verrà ritentato alla prossima scrittura   # exit 5
$ engramma --root $T add - …            # chiuso il lettore
aggiunta: 01M6TKG0YPRZ…                 # la scrittura ritenta e completa
rinocerontism: sqlite=0 wal=0           # nessuna radice
```

Il file Obsidian di P1 in un vault legato:

```console
$ cat $V/nuova-da-file.md
---
title: Nuova da file
tags: [frutta]
---
[[Fabio]] ama i fichi.
$ engramma --root $T vault sync $V
nuove note da file: 1
  - nuova-da-file.md
$ cat $V/nuova-da-file.md            # UN blocco ---, i campi dell'utente
---
id: 01M6TKHWF6KR8RD0NA8XM28GKH       # (dell'utente), l'id fuso dentro
type: note
title: Nuova da file
tags: [frutta]
source: import:obsidian/nuova-da-file.md
…
---
[[Fabio]] ama i fichi.
$ find $V -name "nuova*"              # UN solo file: nessuna copia note/
…/vault/nuova-da-file.md
$ engramma --root $T vault sync $V
nulla da fare
```

## Cosa non ho potuto provare

- **Windows e Linux**: solo macOS arm64, come nelle fasi precedenti.
- **cargo deny advisories**: richiede rete; eseguito `licenses bans`.
- **Il 311M a 100k**: come v0.2.1/v0.3, dichiarato (il 97M resta il
  predefinito); il 311M a 10k è nel banco.
- **La migrazione di celle v0.3 vere con istantanee**: le celle di test
  sono costruite dal motore stesso; una copia piena v0.3 attraversa la
  verify grazie al taglio riapplicato e alla migrazione di schema sulla
  copia ripristinata (test `compact_makes_a_logical_snapshot…`), ma non
  ho aperto una cella prodotta dal binario del tag CON istantanee già
  registrate — la migrazione v8/v9 non tocca i dati, solo la riga e
  l'indice.

## Domande aperte per la v0.4

1. **Il compromesso α=0,6**: il multi-salto a 100k guadagna molto (0,503 →
   0,795) e le parafrasi a 100k tengono, ma il vault piccolo vede le
   pagine-persona sopra le risposte più spesso di v0.3. Un peso che
   dipenda dalla densità del grafo resta il candidato.
2. **Il pending del WAL è in memoria**: un flag in `meta` sopravvivrebbe
   al crash (a costo di un'operazione fuori registro — da decidere se la
   cosa importa davvero).
3. **Le istruzioni preparate per ogni scrittura** (~1–4% della put_note):
   una cache di statement in `Inner` è il primo passo se l'inserimento
   dovesse tornare al passo della v0.2.1.
4. **Le due multi-salto ibride fuori dai primi 5** sul vault dorato: la
   catena c'è (il testo+grafo le risolve), è il rango che manca — il
   punto 1 le copre in parte.

---

# REPORT — Engramma v0.3.2 (compattazione senza seconda copia, e i residui della v0.3.1)

Data: 2026-11-07. Base: `main` @ `8ec3a4b` (tag `fase-v0.3.1`). Un commit
per punto, test rosso prima della correzione (per due punti la
dimostrazione del rosso è nel testo sotto). Tutto offline, Rust 1.95,
macOS arm64, nessuna crate nuova, nessun `unsafe`, nessun `unwrap`/`expect`
fuori dai test, `Cargo.lock` invariato.

## Punto 1 — la compattazione di base è un punto di controllo senza file

- `compact(up_to_seq, &CompactOptions { keep_snapshot })` → `CompactReport`
  (`cut_seq`, `head_seq`, `head_hash`, `state_digest`, `snapshot:
  Option<PathBuf>`, `bytes_before`/`bytes_after` su **cella + WAL +
  istantanee**). Di base **nessun `VACUUM INTO`**: la riga in `snapshots`
  registra testa, digest dello stato logico alla testa (calcolato **dopo**
  il taglio, nella stessa transazione: è il digest dello stato che resta) e
  `redacted_by`; il file di un'istantanea precedente si cancella.
  `--keep-snapshot` tiene il comportamento v0.3.1 (stato logico, una sola,
  purgata con la cella). Resta sempre **una sola riga**: l'ultima
  compattazione sostituisce la precedente, con o senza file.
- Migrazione v10: `snapshots.file` e `state_digest` annullabili (SQLite
  non rimuove un NOT NULL sul posto: la tabella si ricostruisce).
- **Bug trovato dal proptest**: alla ricompattazione con lo stesso taglio e
  lo stesso nome file, la riga precedente cancellata «per identità» era la
  riga appena registrata — la cella restava senza righe e `verify`
  rigiocava da zero un registro redatto, divergendo. La riga precedente
  sopravvive solo quando NON è la riga nuova.
- Bug evitato per costruzione: la lista delle righe precedenti si legge
  PRIMA dell'upsert (alla stessa testa l'upsert riscrive la riga in posto e
  il suo file vecchio sparirebbe dalla lista).

## Punto 2 — `verify` dice cosa ha potuto controllare

`VerifyReport.replay: Replay` con `Full`, `FromSnapshot { seq }`,
`ChainOnly { since_seq }`. Con `ChainOnly` (punto di controllo senza file)
`verify` controlla comunque: la catena intera degli hash (ogni payload
superstite contro il proprio `payload_digest` — la catena v2 copre il
digest salato), la coerenza degli **archi derivati** (riderivati da ogni
nota con la stessa analisi deterministica di `reindex_note`, ora
factorizzata in `derived_edges_of`: la tabella deve contenere esattamente
quelli; gli archi manuali non sono riderivabili e restano fuori),
**nessun vettore oltre la propria nota** (purge e lapidi li cancellano),
l'FTS (come sempre), e — finché la testa non avanza dal punto di controllo
— che il digest dello stato attuale sia quello registrato. `ok()` resta
vero con `ChainOnly`; la CLI stampa «rigioco: solo catena da seq N
(compattata senza istantanea)» e `--json` porta `replay.kind/since_seq`.

Dichiarazione: con la testa avanzata oltre il punto di controllo, una
manomissione dello *stato* (non del registro) non è più opponibile senza
istantanea: è il prezzo del punto di controllo, detto dall'uscita stessa
del comando. Il costo di `verify` in `ChainOnly` cresce con le note (la
riderivazione degli archi tocca tutte le note vive; è lo stesso ordine del
digest dello stato, che `FromSnapshot` paga comunque).

## Punto 3 — `purge` e il punto di controllo

Il digest di un punto di controllo descrive uno stato che conteneva la
nota purgata: non è un dato leggibile (è l'hash dell'intero stato logico),
ma da lì in poi non è più verificabile e non serve tenerlo. `purge` lo
mette a `NULL` e aggiunge il proprio `op_id` a `redacted_by`. Con il file
d'istantanea il comportamento v0.3.1 resta (scrub del file, digest
aggiornato, `redacted_by` che nomina la purge).

## Punto 4 — la vecchia istantanea non sfugge più

Ordine nuovo (già col commit del punto 1): registra la riga nuova, poi
cancella il **file** vecchio (`NotFound` conta come fatto), solo dopo la
sua riga; un errore al passo file è un errore di `compact` (transazione
annullata: le righe vecchie restano coi loro file) e la `purge` successiva
ripulisce quel file. Il commento sul crash dice ora la verità: la finestra
lascia una riga CON il suo file, non una riga fantasma. Dimostrazione del
rosso: con l'ordine v0.3.1 simulato (riga prima, `let _ =` sul remove), il
test della cartella in sola lettura fallisce — la compact «riusciva» e il
file restava orfano, invisibile a ogni purge.

## Punto 5 — le celle v0.3.1 con istantanea

La migrazione v10 gira all'apertura; il test costruisce la cella col
motore, la riporta alla forma v9 (`snapshots` con file e digest NOT NULL,
`schema_version` 9, digest ricalcolato sulla copia downgrade come
l'avrebbe registrato un binario v0.3.1): si apre, `verify` passa
`FromSnapshot`, e il primo `compact` senza flag registra il punto di
controllo e toglie il file. (Il file d'istantanea attraversa il ripristino
di `verify` anche quando la sua copia interna è a schema vecchio: la copia
ripristinata migra prima del rigioco della coda, come già da v0.3.1.)

## Punto 6 — l'oblio raggiunge la vista

- Migrazione v11: `vault_roots(root, bound_at)` — il percorso assoluto
  canonico, scritto a ogni `vault init`/`sync` — e `vault_conflicts(path,
  note_id, created_at)` — ogni copia `.conflitto` che il motore crea (lo
  scanner la ignora: nessuno l'avrebbe mai pulita). Dati derivati, fuori
  dal registro e dall'istantanea di compattazione.
- `Vault::forget_note(id)`: cancella il file legato alla nota e ogni sua
  copia di conflitto, poi le righe. Il nucleo non scrive nella vista; la
  CLI `purge` chiama `forget_note` per ogni vista registrata. L'MCP è di
  sola lettura e non espone purge: la chiamata è pronta per quando
  servirà (dichiarato).
- Una vista irraggiungibile (smontata, permessi) → `PurgeReport.pending`
  contiene `"vault"`, la CLI esce con **5** (stesso codice del WAL); la
  `vault sync` successiva finisce il lavoro, copie di conflitto comprese
  (lo spazzino di sync cancella le copie delle note che non esistono più).
- `delete_note` (lapide, reversibile) NON tocca le copie di conflitto.

## Punto 7 — il titolo dei file senza intestazione

`parse_obsidian` applicava il nome-file-senza-estensione solo con
l'intestazione presente (`&& has_header`): il file senza intestazione — il
caso più comune — restava senza titolo. Ora il fallback scatta sempre e la
riscrittura con l'id porta `title:` nel blocco nuovo. Prova a mano sotto
(«Mele rosse»). **I numeri dorati non si muovono**: tutti i 66 file del
vault dorato hanno intestazione Engramma completa con `title:` (verificato
a mano con grep: 66/66), quindi nessun titolo nuovo entra nell'indice FTS
— i test dorati passano con le attese di v0.3.1, non ritoccate.

## Punto 8 — gli avvisi della vista non si ripetono

Gli avvisi di un file (chiavi sconosciute, valori inutilizzabili) viaggano
solo con la lettura che li ha causati: file nuovo o cambiato. La seconda
`sync` di una vista ferma è un `SyncReport` vuoto, avvisi compresi.

## Punto 9 — i due test HTTP che fallivano a caso

Causa confermata nel codice: il listener della `FakeServer` è non
bloccante e su macOS/BSD il socket restituito da `accept` **eredita
`O_NONBLOCK`** (su Linux no); la prima `read` di `handle` rispondeva
`WouldBlock` con la richiesta ancora in viaggio, e `Err(_) => return`
chiudeva la connessione senza risposta. Correzione:
`stream.set_nonblocking(false)` subito dopo `accept` (il timeout di
lettura di 5 s resta).

Numeri della prova: il ciclo pre-correzione NON ha riportato il fallimento
a macchina ferma — 30 giri verdi di `cargo test -p engramma-embed --lib`,
e altri 30 verdi sotto carico parallelo di tre processi (il difetto è
caricato dal sistema e ~1/10 era la misura della verifica, non una
garanzia). Il test nuovo `a_slow_client_still_gets_its_response` rende la
finestra deterministica: un client che esita 150 ms tra connect e invio,
senza la correzione riceve la chiusura senza risposta (rosso), con la
correzione la risposta arriva (verde). Dopo la correzione: **100 giri
verdi di fila (100/100)**.

## Punto 10 — il banco e la scelta robusta

- La tabella per modalità guadagna la riga della **fusione predefinita**
  (`mm-0.6-g0.40`): mancava da sempre, ed è quella che conta.
- La griglia misura anche **0,45** (il peso 0,40 sta subito prima di un
  crollo: a 0,50 l'MRR pesato perde ~0,15, e il bordo del dirupo non era
  mai stato misurato).
- **Regola di scelta robusta**: vince la configurazione con il miglior MRR
  pesato **nel caso peggiore dei vicini** (il minimo su ±1 passo di α e
  del peso del grafo, i punti della griglia che esistono); a parità entro
  0,002 vince chi tiene le parafrasi. Griglia e minimo dei vicini sotto.

### La griglia (100k note, 97M, mediana di 3 giri, MRR pesato per domande)

| α \\ g | 0,25 | 0,30 | 0,35 | 0,40 | 0,45 | 0,50 |
|---|---|---|---|---|---|---|
| 0,4 | 0,8215 | 0,8368 | 0,8397 | 0,8390 | 0,8303 | 0,6993 |
| 0,5 | 0,8379 | 0,8425 | 0,8577 | 0,8577 | 0,8436 | 0,7118 |
| 0,6 | 0,8440 | 0,8483 | 0,8527 | **0,8656** | 0,8410 | 0,7065 |
| 0,7 | 0,8460 | 0,8497 | **0,8529** | 0,8552 | 0,8461 | 0,7015 |

Il minimo dei vicini (finestra completa ±1 passo di α e del peso, i punti
che esistono):

| α \\ g | 0,25 | 0,30 | 0,35 | 0,40 | 0,45 | 0,50 |
|---|---|---|---|---|---|---|
| 0,4 | 0,8215 | 0,8215 | 0,8368 | 0,8303 | 0,6993 | 0,6993 |
| 0,5 | 0,8215 | 0,8215 | 0,8368 | 0,8303 | 0,6993 | 0,6993 |
| 0,6 | 0,8379 | 0,8379 | 0,8425 | 0,8410 | 0,7015 | 0,7015 |
| 0,7 | 0,8440 | 0,8440 | **0,8483** | 0,8410 | 0,7015 | 0,7015 |

Lettura: il crollo comincia PRIMA del previsto — a **0,45** il peso del
grafo costa già ~0,025 di MRR pesato (0,8656 → 0,8410 alla α 0,6), e a 0,50
ne costa ~0,15. Il miglior MRR assoluto resta 0,6/0,40 (0,8656), ma sta sul
bordo del dirupo: il suo caso peggiore dei vicini è 0,8410. **Vince la
regola robusta: α = 0,7, grafo 0,35** (0,8529, caso peggiore 0,8483 — la
configurazione più lontana dal crollo che resta vicina al massimo).
Parafrasi 0,8730 (meglio delle 0,862 di 0,6/0,40), multi-salto 0,3556
(peggio delle 0,795: il prezzo dichiarato). **Il default in `search.rs` è
cambiato**: fusione convessa α 0,7, peso grafo 0,35. I 208 test restano
verdi con il nuovo default, test dorati compresi, attese non ritoccate.

Nota onesta sul banco: la prima stampa del minimo dei vicini aveva un baco
(calcolato riga per riga, ogni cella non vedeva i vicini successive); il
vincitore non cambia — verificato ricalcolando la finestra completa a mano
sulla griglia — ma i valori intermedi della prima stampa erano sbagliati.
Il banco ora calcola il minimo sulla griglia completa.

<!-- NUMERI-TARATURA -->

## Prove a mano (uscite trascritte)

**Compact di base a 100k** (cella costruita dal generatore del banco,
100 000 note + 3 768 put di sostenimento, poi la CLI — binario release):

```console
$ engramma --root $R compact --keep-last 1000          # nessun flag
compattata fino a seq 102768 (testa 103768): punto di controllo senza file
  (cella+WAL+istantanee: 251985920 → 193753088 byte)        # 240,3 → 184,7 MB
$ engramma --root $R verify
registro verificato: 103768 operazioni, catena hash e indici derivati a posto
rigioco: solo catena da seq 102768 (compattata senza istantanea)
$ engramma --root $R verify --json
{"ops":103768,"replay":{"kind":"chain_only","since_seq":102768},…,"ok":true}
$ ls cells/
default.sqlite                       # nessun *-snap-* accanto alla cella
```

Con `--keep-snapshot` sulla stessa cella (copia pulita): 240,3 MB →
**358,7 MB** (cella 184,7 + istantanea 174,0) e `verify` risponde
«ricostruzione (coda da istantanea di seq 102768)». Una `purge` a corpus
pieno sulla cella col punto di controllo: **2,2 s** (v0.3.1: 3,4–4,1 s con
lo scrub dell'istantanea; qui niente file da lavare — il banco misura
sotto anche il caso con il file).

**Conflitto, poi purge** (punto 6) — la ricerca delle radici in TUTTA la
cartella della vista:

```console
$ engramma --root $T add - --source user --confidence stated \
    --title "Nota segreta"      # corpo: Il pin del lucchetto e quokkaviolaceo42
$ engramma --root $T vault init $V
$ sed 's/quokkaviolaceo42/quokkaviolaceo99 dal file/' …md   # il file si muove
$ EDITOR=script-che-modifica engramma --root $T edit <id>   # e anche la cella
$ engramma --root $T vault sync $V
conflitti (vince la cella, copia .conflitto): 1
  - fact/nota-segreta-AKVF7D2M.md                            # copia registrata
$ engramma --root $T purge <id> --reason "prova oblio v0.3.2" --yes
purgata: 01M6VKF4R9QR7VT1A9AKVF7D2M (2 payload dimenticati)  # exit 0
$ find $V -type f
…/vault/engramma.json                          # file della nota E copia .conflitto: andati subito
$ grep -r -e quokkaviolace -e lucchetto $V
nessuna radice in nessun file della vista
```

**Mele rosse** (punto 7):

```console
$ printf 'Mi piacciono.\n' > obs/"Mele rosse.md"
$ engramma --root $T import obs --obsidian
importate: 1 / saltate: 0 / errori: 0
$ engramma --root $T search "mele" --mode text
1. [active] Mele rosse (SCTHY76Y)   liste: text   termini: mel   supporto: strong
$ engramma --root $T3 vault sync $V3            # stessa cosa nella vista
nuove note da file: 1
  - Mele rosse.md
$ cat $V3/"Mele rosse.md" | grep title
title: Mele rosse                                 # il blocco nuovo porta il titolo
$ engramma --root $T3 vault sync $V3
nulla da fare                                     # punto fisso
```

## Chiusura

`cargo fmt --check`, `cargo clippy --all-targets --offline -- -D warnings`,
`cargo test --offline --workspace` (**208 test** in debug, **208** in
`--release`, zero falliti), `cargo deny check licenses bans` (offline: le
advisory richiedono rete, come sempre dichiarato): verdi.

### La compattazione senza seconda copia (il punto della fase)

L'impronta è **cella + WAL + file d'istantanea** (il conto onesto), prima e
poi, nei due modi — banco, mediana di 3, taglio a testa−1000:

| corpus | prima | dopo: base | dopo: +istantanea | compact base | compact +istant. | purge (ms) |
|---|---|---|---|---|---|---|
| 10k (testo) | 24,4 MB | **19,2 MB** | 37,4 MB | 0,1 s | 0,4 s | 418–544 |
| 100k (testo) | 246,2 MB | **189,9 MB** | 369,0 MB | 1,1–1,7 s | 3,0–4,3 s | 4 963–7 955 |
| 100k (+97M) | 465,0 MB | **407,1 MB** | 586,1 MB | 2,0 s | 5,1 s | 4 407 |

Attesso dalla spec: «circa 246 → 190 MB di base, circa 246 → 369 MB con
--keep-snapshot» — misurato 246,2 → 189,9 e → 369,0. La purge del banco
corre con l'istantanea presente (il percorso v0.3.1, scrub compreso); sulla
cella col solo punto di controllo, a mano: **2,2 s**. I tempi di compact
della riga «viva» registrata (3,0/7,6 s) attraversano lo stesso picco di
carico delle righe plain sotto; il giro di controllo dà 1,1/3,5 s.

### Le latenze: il confronto like-for-like con v0.3.1

(100k, 97M, mediana di 3, cache calda vera; v0.3.1 = `bench/RESULTS.md` del
tag `fase-v0.3.1`.)

| misura | v0.3.1 | v0.3.2 | Δ |
|---|---|---|---|
| ibrido caldo p50 / p95 (ms) | 11,6 / 14,8 | 11,7 / 14,9 | +0,9% / +0,7% |
| ibrido + embedding p50 / p95 (ms) | 20,3 / 23,5 | 20,6 / 23,7 | +1,5% / +0,9% |
| testo+grafo p50 / p95 (ms, riga sem) | 9,1 / 12,4 | 9,3 / 12,6 | +2,2% / +1,6% |
| solo vettori p50 / p95 (ms) | 4,4 / 5,1 | 4,5 / 5,1 | +2,3% / 0 |
| inserimento (note/s) | 2 191 | 2 144 | −2,1% |
| embedding (note/s) | 193 | 188 | −2,6% |

**Soglia del ≤10%: rispettata ovunque** (niente nel percorso di ricerca è
cambiato: solo le costanti α e peso grafo).

Dichiarazione onesta: questa è la macchina di sviluppo dell'utente, non una
macchina di banco. Le righe **plain** a 100k della tornata registrata
(`viva` p50 22,8, `uniform` 13,4) hanno attraversato un picco di carico
della sessione utente (load ~8: WindowServer 40%, ZCode 28%, Chrome) — le
righe sem della stessa tornata, misurate nella finestra tranquilla,
combaciano con v0.3.1 al millisecondo. Giro di controllo dopo il picco
(una sola tornata, stessa cella, stesso seme): `viva` p50/p95 **9,4/12,7**,
`uniform` 9,4/12,8, inserimento 1 906–2 171 note/s — invariati rispetto a
v0.3.1 (9,2/12,5). `bench/RESULTS.md` conserva la tornata registrata
com'è; le tabelle di questa sezione portano il like-for-like.

Tabella completa e per-modalità in `bench/RESULTS.md` (la riga della
fusione predefinita c'è finalmente: `mm-0.7-g0.35`, accanto alla
`mm-0.6-g0.40` che era il default v0.3.1).

## Cosa non ho potuto provare

- **Windows e Linux**: solo macOS arm64, come nelle fasi precedenti.
- **cargo deny advisories**: richiedono rete; eseguito `licenses bans`.
- **Il 311M a 100k**: dichiarato (come v0.2.1/v0.3; il 97M resta il
  predefinito); le sue misure a 10k sono nel banco.
- **La taratura robusta su un secondo corpus**: la regola dei vicini nasce
  da UNA griglia (questa); la sua ragione è proprio non fidarsi del
  massimo puntuale, ma un corpus diverso resta la prova definitiva.

## Domande aperte per la v0.4

1. **Il costo di `verify` in ChainOnly cresce con le note** (la
   riderivazione degli archi tocca tutte le note vive): campionare o
   indicizzare se diventa un comando quotidiano.
2. **Il prezzo multi-salto del peso 0,35** (0,356 di MRR contro 0,795 a
   0,40 a 100k): un peso che dipenda dalla densità del grafo resta il
   candidato dichiarato da v0.3 — la regola robusta sceglie la sicurezza,
   non il picco.
3. **Il punto di controllo ammette solo il rigioco della coda con
   istantanea**: una replica che parte da una cella compattata senza file
   deve chiedere l'istantanea (o rigiocare da zero è impossibile) — da
   formalizzare nel §9 quando la replica arriva.
4. **`purge` dall'MCP**: il server è di sola lettura; `Vault::forget_note`
   è pronta per quando servirà.

Tag: `fase-v0.3.2`. Nessun push (repo locale).

# REPORT — Engramma v0.4 (pronto per Homun: grafo adattivo, collegamento Node, residui della v0.3.2)

Data: 2026-11-08. Base: `main` @ `8dcf37c` (tag `fase-v0.3.2`). Un commit
per punto, test rosso prima della correzione (per i punti la cui
correzione è un'API nuova, il rosso è stato il non-compilare dei test
stessi: dichiarato qui, come nelle fasi precedenti). Tutto offline,
Rust 1.95, macOS arm64, nessuna crate nuova (napi era già nel lock del
commit base; le feature `serde-json` di napi e `rusqlite` come
dev-dipendenza di engramma-vault compilano solo crates già presenti),
nessun `unsafe` fuori da napi-derive, nessun `unwrap`/`expect` fuori dai
test, `clippy -D warnings` verde.

## Punto 1 — una vista ha un'identità, e si può sganciare

- Migrazione **v12**: `vault_roots` diventa `vault_views(view_id PRIMARY
  KEY, root, bound_at, last_seen_at)`; `vault_files` e `vault_conflicts`
  appartengono a una vista (`PRIMARY KEY (view_id, path)`, l'unicità
  globale `UNIQUE(note_id)` del v0.3.1 resta). Il lato Rust della
  migrazione conia un ULID per ogni root registrata e adotta le righe
  file/conflitto esistenti alla vista più anziana.
- L'identità viaggia in `engramma.json` (`view_id`, ULID): `vault init`
  lo scrive (e lo conserva attraverso l'export che riscrive il
  manifest); una `sync` che trova un `view_id` noto in un percorso nuovo
  **aggiorna la root** — una vista, non due; un manifest senza `view_id`
  (vista v0.3.x) lo adotta alla prima sync dalla root; una cella v0.3.2
  (schema 11) migra e la prima sync della cartella adotta l'id coniato.
- **`engramma vault unbind <dir|view_id>`**: sgancia la vista e i SUOI
  legami (file, copie di conflitto); i file restano — non sono mai stati
  del motore. Accetta la cartella o l'id, anche di 8 caratteri.
- **`purge` per vista**: una vista irraggiungibile resta `pending:
  ["vault"]` (uscita 5) **solo se nella cella risultano file o copie di
  conflitto di quella nota legati a quella vista**; altrimenti là non
  c'è niente da cancellare e l'oblio è completo. Il messaggio nomina la
  vista e suggerisce sia `vault sync` sia `vault unbind`. E il
  `vault_roots().unwrap_or_default()` della v0.3.2 non inghiotte più
  nulla: viste non leggibili ⇒ `pending`.
- Il percorso della vista poteva anche appartenere a un'altra vista: la
  seconda copia di una nota resta tale anche attraverso viste diverse
  (la ricerca per nota è globale), mentre il confronto per percorso è
  per vista.

Test: vista spostata e sincronizzata altrove (una riga sola, root nuovo,
purge 0); cancellata poi unbind (purge 0); irraggiungibile con file
legato (5, messaggio che nomina la vista); senza file (0); migrazione
v11→v12 con adozione. La prova a mano è sotto.

## Punto 2 — le copie `.conflitto` di prima della v0.3.2

Lo scanner registra un `<nome>.conflitto-<ts>.md` non registrato il cui
`<nome>.md` è legato a una nota, per quella nota: la purge le raggiunge.
Le copie orfane (base non legato) restano dell'utente, elencate in un
avviso **una volta sola** — la riga marcatore (senza nota) zittisce le
sync successive e se ne va quando il file sparisce. Rosso prima: la
copia creata a mano sopravviveva alla purge e l'orfano non era segnalato.

## Punto 3 — il banco riporta l'MRR per categoria

La taratura stampa, per OGNI configurazione, le quattro categorie, la
loro media semplice (`category_mean`, con test) e resta l'MRR pesato col
caso peggiore dei vicini. Il criterio del punto 6 decide sulla media per
categoria: l'MRR pesato per numero di domande nascondeva il multi-salto,
che è il 5% delle domande.

## Punti 4–6 — il grafo adattivo

**Semi dalla domanda (P1.4, stile HippoRAG 2).** Il vettore di partenza
del PageRank sono le entità che la domanda nomina: gruppi di 1–3 parole
consecutive, normalizzati come l'indice (minuscole, senza accenti, **niente
stemming**: i nomi propri non si stemmano), corrispondenza esatta contro
`entities.name` e gli `aliases` delle note-entità. La corrispondenza più
lunga si tiene il suo span («anna rossi 2» non semina anche un'eventuale
«anna»). `GraphIndex` costruisce la mappa nomi/alias insieme all'indice
e la aggiusta in `patch_note`. I candidati della fusione testo+vettori
entrano con punteggio normalizzato × `seed_passage_weight` (0,05, come
HippoRAG 2). **Porta**: la domanda che non nomina nessuna entità del
grafo non sveglia il grafo — lista grafo vuota, explain «grafo spento:
nessuna entità nominata».

**Peso per domanda (P1.5).** `w_g(q) = w_max · gate(q) · incertezza(q)`
con incertezza in `[u_min, 1]` da due segnali gratis sui candidati in
mano (linea QPP): lo **stacco** primo-secondo della lista fusa
(riferimento `GRAPH_GAP_REF` = 0,25 sulla scala normalizzata) e
l'**accordo** testo/vettori sul primo risultato (metà e mezzo quando ci
sono entrambe le liste, solo lo stacco nel modo testo). Monotona,
limitata, spiegata: `explain.graph` porta peso, perché è spento, stacco
e accordo. `Weights.graph_policy` (`Fixed` = v0.3.2 | `Adaptive{w_max,
u_min}`), `GraphSeeding` (`Fused` = v0.3.2 | `Question{spw}`), e
`Fusion::Adaptive { alpha, cos_norm, w_max, u_min }` che soprassiede la
policy della query (la manopola della griglia).

### La sonda: perché le soglie non si raggiungono

`--probe-seeding` misura la semina dalla domanda a peso FISSO e stampa i
due segnali per categoria. Due fatti, a 10k (97M):

- la semina dalla domanda vale quanto la fusa (multi-salto 0,611 vs
  0,607 a g 0,40/α 0,7): la qualità multi-salto la decide il PESO, non i
  semi;
- **i due segnali non separano le categorie**: gli stacchi multi-salto
  (0,12–0,47) sovrappongono quelli fattuali (0,005–0,50), e l'accordo
  sulle domande multi-salto è spesso VERO — le liste concordano sul
  candidato sbagliato (le note del soggetto). Nessuna formula monotona
  e limitata può dare peso alto alle une e basso alle altre.

### La griglia (32 punti + 2 riferimenti, 97M; 10k mediana di 3, 100k un giro)

Criterio: media per categoria dell'MRR, nel caso peggiore tra le
dimensioni. Righe selezionate (tutte nell'uscita della taratura):

| configurazione | media@10k | media@100k | peggiore |
|---|---|---|---|
| ad α0,6 w0,40 u0,15 spw0,05 (**vincitore**) | 0,7646 | 0,8080 | 0,7646 |
| ad α0,7 w0,40 u0,15 spw0,05 | 0,7593 | 0,7608 | 0,7593 |
| ad α0,7 w0,45 u0,15 spw0,05 | 0,7197 | 0,8069 | 0,7197 |
| rif α0,7/g0,35 (v0.3.2) | **0,8050** | 0,7419 | 0,7419 |
| rif α0,6/g0,40 (v0.3.1) | 0,7234 | **0,8425** | 0,7234 |

Il vincitore e i riferimenti rifatti a 100k (mediana di 3, ibrido caldo):

| configurazione | fattuali | parafrasi | multi-salto | inglese | media | p50/p95 (ms) |
|---|---|---|---|---|---|---|
| vincitore α0,6 w0,40 u0,15 spw0,05 | 0,928 | 0,914 | 0,458 | 0,932 | 0,808 | 11,8 / 15,1 |
| rif α0,7/g0,35 (v0.3.2) | 0,912 | 0,873 | 0,356 | 0,827 | 0,742 | 11,6 / 14,7 |
| rif α0,6/g0,40 (v0.3.1) | 0,910 | 0,862 | 0,795 | 0,802 | 0,842 | 11,6 / 14,8 |

**Soglie (spec, non abbassate):**

- a 100k: fattuali +0,016, parafrasi +0,041, inglese +0,105 (tutte OK),
  p95 +2,7% (OK), **multi-salto 0,458 < 0,75: NO**;
- a 10k il vincitore perde parafrasi (0,840 vs 0,882, −0,042) e inglese
  (0,893 vs 0,948, −0,055) oltre soglia, e multi-salto 0,349: NO.

**Sentenza (quella prevista dalla spec): il predefinito resta α 0,7 /
grafo 0,35** (seme fuso, il comportamento v0.3.2 esatto), la variante
adattiva resta come opzione, e questi sono i numeri. Cosa manca, detto
onesta mente: (1) un segnale che riconosca le domande multi-salto — i
due segnali QPP dichiarati non lo fanno, su questo corpus; (2) una lista
grafo che non normalizzi sul massimo (HippoRAG 2 ORDINA per PageRank,
non fonde per peso: lì il multi-salto non paga dazio alle categorie
dirette). A 100k, intanto, il vincitore adattivo batte il predefinito
SU TUTTE le categorie — il costo dell'adattivo è a 10k, dove il corpus è
piccolo e i segnali più rumorosi.

Controprova sui test dorati: il predefinito non è cambiato, i 66 file
passano con le attese non ritoccate (225 test in debug e release).

## Punti 7–9 — il collegamento Node per Homun

`crates/engramma-node`, napi-rs 3, tutto asincrono sul pool di libuv:
`Engine` (open/cell/cells/modelStatus/pullModel con progresso per byte
verificato — `pull_progress` in engramma-embed — /search su più celle
con RRF k=60 dichiarata approssimata/close) e `Cell` (put/patch/get/
list/search/history/neighbors/setStatus/confirm/delete/purge/
embedPending/changesSince/verify/compact/vaultViews/vaultUnbind). Gli
errori sono `Error` JS con `code` stabile (`NOT_FOUND`, `QUARANTINED`,
`INVALID_NOTE`, `CONFLICT`, `IO`, `MODEL_MISSING`, `COMPACTED`): il code
viaggia nel prefisso «[CODE] » attraverso la barriera asincrona e ogni
`reject` ricostruisce l'errore con la proprietà `code` (napi rigetta
verbatim un valore trattenuto — dichiarato nel codice). L'oracolo chiuso
vale anche per `get`: quarantena e lapide rispondono `null`. Ibrido senza
modello degrada a testo+grafo (dichiarato), `vector` senza modello è
`MODEL_MISSING`. `CONFLICT`: una nota `external` non può nascere attiva.

- **Test JS** (`node:test`, nessuna dipendenza, `embedder: 'none'`):
  ciclo completo, quarantena, errori col code, 200 put + 200 search in
  parallelo, changesSince con compattazione, Engine.search su due celle:
  **8/8 in Node 24, 8/8 nell'Electron 45 di Homun** (`npm run
  test:electron`, `ELECTRON_RUN_AS_NODE=1`, percorso da `HOMUN_ELECTRON`;
  il repo di Homun non è stato toccato).
- **`npm run build`**: `engramma.darwin-arm64.node` release =
  **33 240 320 byte** (33,2 MB; il modello NON è incorporato), più
  `index.js` e `index.d.ts` generati. Il CI del workspace non richiede
  Node: la crate compila e clippy passa con `cargo test`.
- `docs/node.md`: costruzione, contratto (camelCase, oracolo chiuso,
  purge che non mente), l'esempio per un assistente e la firma del
  `.node` in un'app macOS notarizzata (fuori dall'asar,
  `codesign --options runtime`).

## Prove a mano (uscite trascritte)

**La vista spostata (punto 1):**

```console
$ engramma --root $T vault init $T/vista-vecchia
vault inizializzato: …/vista-vecchia
$ mv $T/vista-vecchia $T/vista-nuova
$ engramma --root $T vault sync $T/vista-nuova
nulla da fare                        # la cartella è identica: una vista, root aggiornato
$ engramma --root $T purge R4PMXB5H --reason "prova v0.4" --yes
purgata: 01M6XAJHSGBSMYJPJXR4PMXB5H (1 payload dimenticati)   # exit 0
$ find $T/vista-nuova -type f
…/vista-nuova/engramma.json         # il file della nota: andato subito, al NUOVO posto
$ grep -rl quokkaviolaceo $T/vista-nuova || echo pulita
pulita
```

**Tre domande dal vault dorato con explain (66 file, celle ibride).**
Col predefinito (α 0,7 / g 0,35 fisso) e con la variante adattiva
(`ENGRAMMA_GRAPH=adaptive:0.5,0.1,0.05`), il peso del grafo per domanda:

| domanda | predefinito | adattivo | prima risposta |
|---|---|---|---|
| «dove abita Fabio» (diretta) | 0,35 | **0,05** (stacco 0,27, concordi) | Fabio abita a Milano |
| «dove lavora il partner di Fabio» (multi-salto) | 0,35 | **0,47** (stacco 0,04, discordi) | Giulia |
| «dove vive Fabio dopo il trasloco» (parafrasi) | 0,35 | 0,28 (stacco 0,37, discordi) | Fabio abita a Milano* |

\* con l'adattivo la parafrasi trova la NOTA giusta in cima (col
predefinito saliva prima la persona). Il percorso del multi-salto
dell'adattivo è quello giusto: `via [[Fabio]] -partner_of-> (nota
00000002)`. E la porta, su una domanda che non nomina entità:

```console
$ ENGRAMMA_GRAPH=adaptive:0.5,0.1,0.05 engramma --root $T search "ha vinto una gara di crossfit"
1. [active] Palestra la mattina (00000064)
   grafo: grafo spento: nessuna entità nominata
```

**Lo script dell'esempio di `docs/node.md`, con l'Electron di Homun:**

```console
$ ELECTRON_RUN_AS_NODE=1 …/node_modules/electron/dist/…/Electron \
    crates/engramma-node/example/assistente.js
engramma-node 0.1.0-dev — esempio docs/node.md
modello: granite-embedding-97m-multilingual-r2@835ad140
scritta: FX7GDYZ2 [active] Casa di Fabio
dedotta: ZAQEVN7P [quarantined] (in quarantena)
ricerca «dove abita Fabio?» (embedding: 2 note):
  1.000 strong «Casa di Fabio» — Fabio vive a [[Milano]] dal 2024. grafo 0.35
la quarantena per get(): null (oracolo chiuso)
confermata: active
purga: 1 payload dimenticati, scrubbed=true
verify: ok=true replay=full ops=5
esempio completato
```

## Chiusura

`cargo fmt --check`, `cargo clippy --all-targets --offline -- -D
warnings`, `cargo test --offline --workspace` (**225 test** in debug,
**225** in `--release`, zero falliti), `cargo deny check licenses bans`:
verdi. I test JS: 8/8 in Node 24, 8/8 nell'Electron di Homun.

### Il banco (10k e 100k, viva/uniform/97M/311M, mediana di 3, cache calda vera)

Il percorso di ricerca non è cambiato (il predefinito è intatto) e il
banco lo conferma: le righe di qualità della fusione predefinita sono
**identiche al bit** a quelle della v0.3.2 (mm-0.7-g0.35 a 100k:
0,912/0,873/0,356/0,827) e l'impronta è la stessa (100k+97M: 465,0 MB →
407,1 base / 586,1 con istantanea; compact 2,2 s / 5,5 s; purge 4,6 s).

Like-for-like con la v0.3.2 (100k, 97M):

| misura | v0.3.2 | v0.4 | Δ |
|---|---|---|---|
| ibrido caldo p50 / p95 (ms) | 11,7 / 14,9 | 11,6 / 14,7 | −0,9% / −1,3% |
| ibrido + embedding p50 / p95 (ms) | 20,6 / 23,7 | 20,3 / 23,4 | −1,5% / −1,3% |
| solo vettori p50 / p95 (ms) | 4,5 / 5,1 | 4,5 / 5,0 | 0 / −2,0% |
| embedding (note/s) | 188 | 191 | +1,6% |
| inserimento (note/s) | 2 144 | 2 470 | +15% (la riga v0.3.2 attraversò un picco di carico; il suo giro di controllo dava 1 906–2 171) |

**Soglia del ≤10%: rispettata ovunque.** Le latenze della variante
adattiva vincente sono nella tabella della taratura (p95 15,1 ms,
+2,7%). Tabella completa e per-modalità in `bench/RESULTS.md` (colonne
della v0.3.2 e riga della fusione predefinita incluse, 311M a 10k
compreso).

## Cosa non ho potuto provare

- **Windows e Linux**: solo macOS arm64, come nelle fasi precedenti.
- **cargo deny advisories**: richiedono rete; eseguito `licenses bans`.
- **Il grafo adattivo come predefinito**: le soglie non si raggiungono
  (sopra, i numeri); la variante è documentata e provabile.
- **pullModel di rete**: offline — il percorso è esercitato dai test
  della v0.3 con il server locale di engramma-embed (`pull_progress` ne
  riusa la logica verificandone gli sha per chunk); con il modello già
  presente, `pullModel` verifica e salta senza rete (provato
  nell'esempio).

## Domande aperte per la v0.5

1. **Un segnale per il multi-salto**: lo stacco e l'accordo non bastano
   (sonda del punto 6); i candidati sono la densità del grafo attorno
   ai semi (quanta massa il PPR lascia entro due salti) o la posizione
   del migliore candidato del grafo rispetto alla fusione.
2. **La lista grafo normalizzata sul massimo**: è ciò che la fa
   competere col punteggio fuso; HippoRAG 2 ordina per PageRank e non
   paga questo dazio. Un esperimento dichiarato: massa assoluta invece
   che relativa al massimo.
3. **Il costo a 10k dell'adattivo** (parafrasi −0,042, inglese −0,055
   contro il predefinito): corpus piccolo, segnali più rumorosi — da
   ritarare su un secondo corpus, quando ci sarà.
4. **Homun**: il contratto del punto 7 è pronto e provato nel suo
   Electron; l'integrazione (migrazione delle note, `storia`, la
   sezione Memoria) è la fase prossima, su un altro repo.

---

# REPORT — Engramma v0.4.1 (il contratto JS regge per Homun)

Data: 2026-11-08. Base: `main` @ `e69deaa` (tag `fase-v0.4`). Un commit
per punto, con il test che fallisce prima della correzione (per la
funzione nuova `purge_everywhere` il rosso è stato il non-compilare del
test stesso che la chiama, come nelle fasi precedenti; il rosso VERO del
punto è nel test JS, trascritto sotto). Tutto offline, Rust 1.95, macOS
arm64, nessuna crate nuova (`Cargo.lock`: unica variazione l'arco
`engramma-node → engramma-vault`, come da vincoli), nessun `unsafe`
fuori da napi-derive, nessun `unwrap`/`expect` fuori dai test, clippy
`-D warnings` verde. Il grafo adattivo non è stato toccato.

## Punto 1 — una riscrittura non lava la quarantena (P0)

**Prova a mano (Node 24, `embedder: 'none'`), trascritta:**

```console
$ node prova-p1.js            # il caso della spec, parola per parola
1) nascita external       → quarantined
2) riscrittura stated      → quarantined
3) search «PIN Fabio»      → 0 risultati
4) riscrittura status:active → CONFLICT — una riscrittura conserva lo stato
   della nota: lo cambia `setStatus`, `confirm` o `delete`
5) put sulla lapide        → CONFLICT — la nota è una lapide: si ripristina
   dallo storico, una riscrittura non la resuscita
```

La regola vive **nel core** (`apply_put`/`apply_patch` in
`cell/ops.rs`), quindi CLI, `vault sync` e collegamento Node la ereditano
dallo stesso posto — e la eredita anche il rigioco, perché la decisione è
deterministica sul payload + stato:

- lo stato cambia solo con `setStatus`, `confirm`, `delete` (e `restore`);
  `put` con id esistente e `patch` lo conservano;
- l'unica deriva consentita: la nuova versione è `external` e la nota è
  attiva → quarantena (mai il contrario); una `external` riscritta
  `stated` resta in quarantena;
- chiedere per iscritto uno stato diverso da quello conservato è
  `Invalid` → `CONFLICT` in Node (mappato sul messaggio, il code viaggia
  nel prefisso «[CODE] » come tutto il resto);
- una riscrittura di una supersede conserva anche la chiusura di validità
  (l'invariante «superseded implica valid_to» attraversa la riscrittura;
  il test v0.1.1 che si aspettava l'errore è stato aggiornato alla
  semantica nuova);
- una **lapide** non resuscita: `put` e `patch` con il suo id → `CONFLICT`;
  la CLI `add` non ha `--id` (decisione: niente da cambiare — le
  riscrittture della CLI passano da `edit`, cioè da `patch`, che conserva
  lo stato per la regola del core).

Il collegamento legge la nota prima di costruire la riscrittura: senza
`status` costruisce quello conservato (la prova a mano qui sopra funziona
senza sorprese); con `status` esplicito la richiesta arriva al core, che
decide. La `vault sync` fa lo stesso dall'altra parte: **un file
modificato a mano non cambia lo stato della nota** — la cella vince quel
campo come vince i conflitti, l'avviso lo dice, e il file che mente
(`status: active` su una nota in quarantena) viene riscritto col vero,
sennò il punto fisso non tornerebbe più (test `a_hand_edited_file_cannot_
launder_a_quarantined_note`, rosso prima: il file lavava la nota).

Test: `core/tests/state.rs` (4, rossi prima), `vault/tests/sync.rs`
(rosso prima), `engine.test.js` `a rewrite preserves the quarantine`.

## Punto 2 — la purge da Node raggiunge le viste (P0)

**Prova a mano, trascritta** (vista legata dalla CLI, purge da Node):

```console
$ engramma --root $T init && engramma --root $T add - … "Nota segreta"
nota: 01M6YDC9C49E2YHP3V868DZ7QB
$ engramma --root $T vault init $T/vista
vista legata dalla CLI, file: nota-segreta-868DZ7QB.md
$ node -e '…cell.purge(id, "prova")…'
purge da Node   → {"forgottenPayloads":1,"scrubbed":true,"pending":[],"views":[]}
file della nota dopo la purge: 0
nessuna radice di quokkaviolace in tutta la root
```

Il blocco della CLI (forget_note per ogni vista legata, viste
irraggiungibili = pending) diventa **una funzione sola**:
`engramma_vault::purge_everywhere(cell, id, reason, actor) ->
(PurgeReport, Vec<ViewProblem>)`, usata da CLI e Node.

- la CLI stampa gli stessi messaggi ed esce con gli stessi codici: i test
  di `purge.rs` sono rimasti verdi senza ritocchi;
- in Node `PurgeView` porta `views: [{ viewId, root, reason }]` senza
  contenuti di note; una vista irraggiungibile che lega file o copie della
  nota → `pending: ['vault']`, `scrubbed: false` (test JS con la cartella
  spostata via).

## Punti 3–9 — il contratto che serve a Homun

- **P1.3 `extensions`**: `NoteView.extensions: Record<string, unknown>`
  (solo chiavi `x-*`, valori JSON, la conversione inversa esatta);
  `NotePatchInput.extensions?: Record<string, unknown | null>` tocca **per
  chiave** (`null` toglie), perché il patch del core rimpiazza la lista
  intera e il merge vive nel collegamento. `aliases` resta un campo a
  sé. Scelta dichiarata: `json_to_yaml` ora usa `YamlValue::from_json`,
  quindi gli oggetti diventano `Map` vere nel frontmatter (la v0.4 li
  metteva come lista di coppie YAML; `yaml_to_json` rilegge entrambe le
  forme, così le note scritte dalla v0.4 si rileggono bene lo stesso).
- **P1.4 le date**: la data di nascita è della nota, non della versione —
  la sostituzione conserva `createdAt` nel core (il registro continua col
  suo HLC: queste sono date della nota, non delle operazioni).
  `NoteInput.createdAt?/updatedAt?` (RFC 3339) servono
  **all'importazione**: accettati solo per una nota nuova, mai nel futuro
  (5 minuti di tolleranza → `INVALID_NOTE`); su una sostituzione entrambi
  ignorati.
- **P1.5 `SearchHit`** porta `noteType`, `tags`, `updatedAt`, `status`
  (`active` fuori da `includeQuarantined`): la riga di risultato senza un
  `get` per colpo.
- **P1.6 `CellOpts.actor`**: `e.cell(name, { actor: 'homun:bot-7' })`
  timbra le scritture di quella `Cell`; due `Cell` sulla stessa cella con
  attori diversi sono la stessa cella sotto, e il registro dice chi ha
  scritto cosa.
- **P1.7 `CLOSED`**: dopo `close` ogni metodo di `Engine` e di ogni sua
  `Cell` risponde `CLOSED` (chiamare `close` due volte non è un errore).
  Il conteggio degli in-flight parte **alla chiamata**, sul thread JS,
  sotto lo stesso mutex del sigillo: ogni promessa che esiste quando
  `close` arriva finisce il suo lavoro, e `close` aspetta che si siano
  assestate — nessuna finestra in cui una put in coda venga chiusa a metà
  o scivoli dentro dopo il sigillo.
- **P1.8**: `put({ confidence: 'external', status: 'active' })` →
  `CONFLICT` come diceva già il REPORT v0.4 (il collegamento smette di
  metterla in quarantena in silenzio); senza `status` si nasce in
  quarantena come sempre.
- **P2.9 l'id corto di una vista**: le **ultime 8** cifre ovunque
  (messaggi, `unbind`, `vaultUnbind`), l'id intero sempre accettato, lo
  corto ambiguo elenca i candidati. Le prime 8 (il timestamp, condivise
  da viste di pochi minuti distanti) non accettate più. Il test CLI di
  `purge.rs` passa alle ultime 8 (era l'unico uso documentato delle
  prime 8); `docs/cli.md` allineato. Dichiarato: l'ambiguità vera (due
  viste con le stesse ultime 8) non è riproducibile in un test end-to-end
  con ULID casuali — il ramo è coperto dal codice, non da un test.
- **P2.10 `docs/node.md`**: la regola dello stato (chi può cambiarlo),
  `CLOSED`, le date d'importazione, `extensions`, `actor` per cella — e
  perché `purge` **non lancia** per un oblio incompleto ma risponde con
  `pending` e `views`: un oblio che non può nemmeno cominciare è un
  errore normale, uno che comincia e non finisce è un dato di ritorno che
  il demone mostra e riprova. Il codice `INCOMPLETE_FORGETTING` della
  spec v0.4 non serve e non c'è.

## Il processo principale di Electron

`npm run test:electron-main`: `__test__/electron-main.js` fa
`app.whenReady()`, apre un motore (con `actor` per cella), scrive con le
`extensions`, cerca, controlla i campi della riga, chiude e verifica
`CLOSED`, esce con 0 — **non** `ELECTRON_RUN_AS_NODE`, perché il demone
di Homun è un processo Electron senza finestra. Verde nell'Electron 45
di Homun (`uscita 0`). Il repo di Homun non è stato toccato.

## Chiusura

- `cargo fmt --check`, `cargo clippy --all-targets --offline -- -D
  warnings`, `cargo test --offline --workspace` in debug e `--release`
  (**230 test** per entrambi, zero falliti), `cargo deny check licenses
  bans`: verdi;
- test JS: **17/17 in Node 24**, **17/17 nell'Electron di Homun**
  (`ELECTRON_RUN_AS_NODE`), più il test del processo principale
  (uscita 0);
- `npm run build` rifatto: `engramma.darwin-arm64.node` release =
  **33 356 096 byte** (33,4 MB; il modello NON è incorporato),
  `index.js`/`index.d.ts` rigenerati sul contratto di questa fase;
- **il banco**: il percorso di ricerca non è cambiato e il giro a 10k
  (mediana di 3) lo conferma — **le righe di qualità della fusione
  predefinita sono identiche al bit** a quelle della v0.4 in
  `bench/RESULTS.md` (97M: fattuali 1.000/0.965, parafrasi 0.976/0.882,
  multi-salto 0.917/0.426, inglese 0.994/0.948; 311M:
  1.000/0.989, 0.972/0.881, 1.000/0.576, 1.000/0.949), e le righe
  `viva`/`uniform` del solo testo pure (richiamo 0.997/0.790 e
  1.000/0.774). Le latenze del giro (p50 testo 1,4 ms a 10k) sono quelle
  della macchina di sviluppo a macchina ferma, come sempre nei REPORT;
  il 100k non è stato rifatto, come da spec;
- temporanei cancellati; tag `fase-v0.4.1`. Nessun push.

## Scelte e dichiarazioni di questa fase

1. **La regola dello stato vive in `apply`**: anche il rigioco la impone.
   Conseguenza onesta: una cella scritta dalla v0.4 con una riscrittura
   che cambiava stato (il bug del punto 1, esercitato a mano nella
   verifica v0.4) non supera più `verify` — il rigioco rifiuta
   quell'operazione. Nessuna cella di Homun esiste (la fase Homun deve
   ancora cominciare) e nessun test del repo è in questo caso; le celle
   della verifica v0.4 erano temporanee.
2. **Il file che mente viene riscritto dalla sync**: la cella vince il
   campo stato (come vince i conflitti), l'avviso lo dice, il file viene
   riscritto dal vivo — sennò il punto fisso non tornerebbe e l'avviso
   si ripeterebbe a ogni sync.
3. **CLI `add --id`**: non esiste (decisione: resta così); le
   riscrittture della CLI passano da `edit` → `patch`, che conserva lo
   stato per la regola del core.
4. **Le estensioni come `Map`**: la v0.4 scriveva gli oggetti come liste
   di coppie (una lista YAML nel file); ora diventano mappe vere. La
   lettura capisce entrambe, la scrittura usa quella giusta.

## Cosa non ho potuto provare

- **Windows e Linux**: solo macOS arm64, come nelle fasi precedenti.
- **cargo deny advisories**: richiedono rete; eseguito `licenses bans`.
- **L'ambiguità delle ultime 8 di una vista** (punto 9): due ULID
  casuali con le stesse ultime 8 cifre non si fanno riprodurre in un
  test end-to-end; il ramo esiste, il test no.
- **Il 100k del banco**: non richiesto (il percorso di ricerca non
  cambia); le righe 100k di `bench/RESULTS.md` restano quelle della
  v0.4, valide per confronto proprio perché identiche.

# REPORT — Engramma v0.4.2 (quello che Homun ha trovato: elenchi senza tetti nascosti, lapidi, oblio di una cella, modello a caldo)

Data: 2026-11-09. Base: `main` @ `513dc59` (tag `fase-v0.4.1`). Un commit
per punto, il test che fallisce prima della correzione (trascritto). Tutto
costruito con `cargo --offline`, Rust 1.95.0, macOS arm64, nessuna crate
nuova (`Cargo.lock` invariato), nessun `unsafe` fuori da napi-derive,
nessun `unwrap`/`expect` fuori dai test, clippy `-D warnings` verde.
Il repo di Homun NON è stato toccato (letto il suo `REPORT.md` e
`src/main/memory/` per capire come usa il contratto). I punti 1 e 2 per
primi, come chiede la spec; i numeri della RAM nel punto 6.

## Punto 1 — `list` senza tetti nascosti (P0)

**Il bug provato:** 1200 note col tag `bot/a1`,
`list({ tags: ['bot/a1'], limit: 100000 })` → **1000**: il
`clamp(1, 1000)` di `ListTask` tagliava in silenzio. In Homun è l'oblio
di un bot eliminato: con 5000 messaggi di storia ne restavano 4000 e il
log diceva «dimenticato».

**La regola adottata** (scritta in `docs/node.md`): un `limit` esplicito
si rispetta **così com'è** — è un processo locale, gli id stanno già in
memoria, nessun tetto. `limit` assente vale 100 come prima. Zero è
`INVALID_ARGUMENT`: una pagina di zero è un bug del chiamante, non una
risposta. Stessa regola per `k` di search e per il `limit` di
`embedPending`.

**Tutti i tetti muti del collegamento, trovati e tolti** (cercati in
`lib.rs`: `clamp`, `min(` su valori del chiamante):

| dove | prima | ora |
|---|---|---|
| `ListTask.limit` | `clamp(1, 1000)` | rispettato; `0` = `INVALID_ARGUMENT` |
| `build_query` k (Cell.search) | `clamp(1, 100)` | rispettato; `0` = `INVALID_ARGUMENT` |
| `EngineSearchTask` k | `clamp(1, 100)` (una seconda volta) | unificato in `parse_k`, rispettato |
| `EmbedPendingTask.limit` | `clamp(1, 8192)` | rispettato; `0` = `INVALID_ARGUMENT` |
| `build_query` graphHops | `min(2)` | 0–2 valido, oltre è `INVALID_ARGUMENT` |
| `NeighborsTask.depth` | `min(4)` | 1–4 valido, fuori è `INVALID_ARGUMENT` |

E **il tetto un piano sotto**: la finestra dei candidati BM25 del core
era un fisso `50` in ENTRAMBI i percorsi (testo e ibrido) — provato:
120 note con lo stesso termine, `k(150)` → **50**. Ora la finestra è
`max(50, k)`: per `k ≤ 50` (il profilo di sempre, Homun usa k 10) costa
come prima, sopra non taglia più in silenzio. Test nel core:
`k_beyond_the_candidate_window_is_respected` (120 note, k 150 → 120).

**Il test JS, prima e dopo** (1500 note, `limit: 100_000`):

```console
✖ list: no hidden ceilings, explicit limit respected (P0.1)
    actual: 1000,    expected: 1500,      # prima
✔ list: no hidden ceilings, explicit limit respected (P0.1)   # dopo
```

e la paginazione: pagine da 137 con `offset` crescente → 1500 id, zero
doppi, l'ordine di concatenazione è quello della cella; `limit: 0` →
`INVALID_ARGUMENT`. Il tetto dei 100 su `k`: `k: 200` su 150 note →
prima **50** (finestra BM25), ora **150**.

## Punto 2 — le lapidi in `list` e gli stati sconosciuti (P0)

**Il bug provato:** `list({ status: ['deleted'] })` → `[]` **senza
errore**: il `filter_map` scartava gli stati sconosciuti e il filtro
vuoto non lasciava passare nulla. Una nota eliminata teneva il testo
nello storico per sempre e Homun non aveva modo di trovarla per offrire
«Dimentica per sempre» (la `purge` sull'id di una lapide funziona già:
`forgottenPayloads: 2, scrubbed: true`, riletto nella prova).

**Ora:** `deleted` è uno status di prima classe in `list` — una lapide
si elenca come la sua ultima versione con `status: 'deleted'` (la riga
mantieni­ il contenuto finché una purge non lo dimentica). Il core
guadagna `Cell::note_ids_all` (tutti gli id, lapidi comprese);
`note_ids` resta la vista dei soli vivi che sync e lista di default
usano: senza `status` la lista resta delle note vive, `get` resta
l'oracolo chiuso (`null` per quarantena e lapide). Ammessi: `active`,
`quarantined`, `superseded`, `deleted`; uno sconosciuto è
`INVALID_ARGUMENT`, mai un filtro vuoto.

**`INVALID_ARGUMENT`** (codice nuovo, documentato): gli argomenti della
chiamata — status sconosciuto, `limit`/`k` zero, `graphHops` oltre 2,
`depth` fuori 1–4, `mode` inesistente, `asOf` malformata, query senza
testo, `search` senza celle. `INVALID_NOTE` resta delle note (source,
confidence, noteType, date). Il confine è dichiarato: il chiamante
distingue un bug suo da una nota che non può scrivere.

**Il test JS, prima e dopo:**

```console
✖ tombstones list; unknown statuses are INVALID_ARGUMENT (P0.2)
    actual: 0,    expected: 1,            # prima: la lapide non si elencava
✔ tombstones list; unknown statuses are INVALID_ARGUMENT (P0.2)  # dopo
```

e a mano, dentro il test: la lapide elencata ha corpo e status
`deleted`; `purge` sul suo id → `scrubbed: true` e **nessuna radice del
testo** in nessun file sotto la root (il metodo di `forgetting.rs`
portato in JS: parole intere minuscole e le loro troncate); dopo la
purge la lista non la porta più; `list({ status: ['foo'] })` e
`list({ status: ['active', 'bar'] })` → `INVALID_ARGUMENT`.

## Punto 3 — `Engine.dropCell(name, reason)`: l'oblio di una cella intera (P0)

Homun cancella un bot e con lui le sue celle: prima una `purge` per nota
(~30 ms l'una, minuti di coda con migliaia di note) e in modo fragile
(punto 1). `dropCell` dimentica tutta la cella: le note (lapidi
comprese), i file della cella (**WAL e istantanee `-snap-*.sqlite`
comprese**), e i file delle note nelle viste legate, come
`purge_everywhere`. `DropView = { notes, scrubbed, pending, views }`
come `PurgeView`: una vista non raggiungibile è `pending: ['vault']`,
`scrubbed: false` e il problema elencato (root e motivo, mai
contenuti) — mai un'eccezione. Il registro dei motivi non c'è: della
cella non resta nulla in cui scriverlo; `reason` è obbligatoria e resta
la traccia del chiamante.

**La concorrenza, il punto delicato.** Il `Gate` del v0.4.1 ora porta un
verdetto (codice+messaggio): quello del motore dice `CLOSED`, quello di
una GENERAZIONE di cella dice `NOT_FOUND`. `dropCell` tiene il lock
delle generazioni per tutto l'oblio: le operazioni in volo su quella
cella finiscono prima (il seal aspetta i permessi in volo), le `Cell`
già aperte rispondono `NOT_FOUND`, e un `cell(name)` arrivato a metà
aspetta lo stesso lock e poi crea una cella **vuota** con una generazione
fresca — mai un fantasma su un file cancellato. `close` e `dropCell` si
coordinano col gate del motore, come prima.

**I test JS** (tutti e tre rossi prima, verdi dopo):

- 1500 note + 20 lapidi → `{ notes: 1520, scrubbed: true, pending: [],
  views: [] }`; `cells()` non la elenca più; **nessun file sotto la
  root contiene una radice del testo**; un nuovo `cell(name)` nasce
  vuoto (lista 0);
- con una `Cell` aperta prima: l'operazione in volo (chiamata prima
  della drop) **finisce senza errore**, poi put/get/list/search/delete/
  purge su quella `Cell` → `NOT_FOUND`; il `cell(name)` nuovo è un'altra
  cella (scrive e rilegge);
- le viste: raggiungibile → i file delle note spariscono subito
  (`vista/note` vuota); irraggiungibile (rinominata via) → `pending:
  ['vault']`, `scrubbed: false`, `views[0]` con root e motivo e **zero
  contenuti**.

**Scoperta documentata nei test:** una nota ha **un** file, ovunque sia
— un secondo `vault init` RIBINDA ogni nota alla vista nuova e la prima
resta con copie orfane (semantica v0.3.1 P0.6, preesistente). Per questo
«una vista raggiungibile e una no» sono due scenari separati: non
possono tenere legami entrambe allo stesso tempo.

**Prove a mano trascritte** (probe Node + CLI):

```console
cells: [ 'solo-cli' ]
drop (never opened): {"notes":1,"scrubbed":true,"pending":[],"views":[]}
cells after: []
second drop: NOT_FOUND - cella solo-cli inesistente
unknown cell: NOT_FOUND
empty reason: INVALID_ARGUMENT
after close: CLOSED
```

```console
$ engramma … cell create bot-v3; … vault init $T/vista-qui
dimenticata: bot-v3 — bot eliminato (5 note, 5 file di viste)
$ ls …/vista-qui/fact     → (vuota: i file sono andati con la cella)
$ engramma … cell drop bot-x --reason prova --json
{ "dropped": "bot-x", "reason": "prova", "notes": 1, "files": 0,
  "scrubbed": true, "pending": [] }
```

La CLI `cell drop` prende `--reason` (obbligatoria) e usa la stessa
`drop_cell_everywhere`: prima cancellava solo il file `.sqlite` — le
viste legate tenevano le copie per sempre, e nemmeno i `-snap-*` venivano
toccati.

## Punto 4 — `pullModel` collega l'embedder al motore aperto (P1)

Richiesta 2 del REPORT di Homun: dopo un `pullModel` il motore APERTO
restava senza embedder (`MODEL_MISSING` su `embedPending`) finché
qualcuno non chiudeva e riapriva — e Homun lo faceva in produzione.

**Ora** l'embedder vive in uno slot condiviso del motore: il pull finito
(con i file verificati) lo carica, e ogni task lo legge quando gira.
Test JS col modello vero di `ENGRAMMA_MODEL_DIR` rispecchiato come
**symlink** in una modelDir temporanea (il pull verifica gli sha
agganciati e salta: niente rete, niente copie):

```console
✖ pullModel attaches the embedder to the OPEN engine (P1.4)   # prima
  [Error: embedPending senza modello: `pullModel` prima] { code: 'MODEL_MISSING' }
✔ … (P1.4)                                                     # dopo
   modelStatus.present: false → true · embedPending() = 2 sulla Cell
   aperta PRIMA del pull · ibrida con explain.semanticRank · scrittura
   e embed successivi · mai un close/open in tutto il test
```

Le ricerche già in volo durante il collegamento finiscono col ramo che
avevano (lo slot si legge una volta a inizio compute). Se il caricamento
fallisce la `pullModel` rifiuta `IO` e lo slot resta vuoto (motore in
testo+grafo, funzionante): il ramo è irraggiungibile nei test senza un
modello sha-corretto ma rotto — la verifica sha avviene PRIMA del
carico, ed è la garanzia stessa del pull. `embedder: 'none'` all'apertura
scarica ma non collega mai (scelta esplicita).

## Punto 5 — `ENGRAMMA_PULL_BASE` (P1)

Richiesta 1 del REPORT di Homun. La variabile d'ambiente, quando c'è e
non è vuota, sostituisce la base di download di `pullModel`
(predefinita `https://huggingface.co`): installazioni offline e mirror
negli ambienti chiusi. **La revisione agganciata e lo sha256 per file
restano agganciati**: una base diversa può solo consegnare gli STESSI
byte o fallire, mai cambiare il contenuto (scritto in `docs/node.md`).

Test JS con un server su 127.0.0.1 che serve i percorsi agganciati
(`{repo}/resolve/{revisione}/{file}`) dalla cartella del modello vero,
col `tokenizer.json` alterato di un byte:

```console
✖ ENGRAMMA_PULL_BASE: … (P1.5)   # prima: la pull ignorava la base e
  #  andava alla rete vera (14,8 s, risolta invece di rifiutare)
✔ … (P1.5)                        # dopo
  err.code = IO, /sha256/ nel messaggio
  requests[0] = /ibm-granite/granite-embedding-97m-multilingual-r2/
                resolve/835ad14087e140460703cf0fae09f97d469d65c2/tokenizer.json
  modelStatus.present = false, nessun .part né tokenizer.json landed
```

(la base è la NOSTRA: la richiesta è arrivata al server locale, sul
percorso agganciato, ed è stata la prima e unica).

## Punto 6 — Quanta RAM costa il modello (P1)

Misurato dal processo Node (`process.memoryUsage().rss`, con
`/usr/bin/time -l` intorno: picco **1431 MB**), modello Granite 97M
(~390 MB di pesi f32), 1000 note da 80–120 caratteri:

| stadio | RSS |
|---|---|
| processo Node, prima di tutto | 51 MB |
| `Engine` senza modello | 52 MB |
| dopo 1000 put | 61 MB |
| **modello collegato** (`pullModel` a caldo, 793 ms) | **1152 MB** |
| dopo 1000 `embedPending(1000)` (1147 ms) | 1174 MB |
| dopo 1000 ricerche ibride (536 ms) | 1178 MB |
| dopo altre 1000 ricerche | 1169 MB |
| dopo `close` + Engine e Cell fuori scope + gc | 1169 MB |

**La crescita oltre i pesi è del carico, non delle operazioni**:
+1091 MB al collegamento e poi PIATTA (2000 ricerche e 1000 embed non
spostano nulla: nessuna sessione per thread, nessuna copia accumulata
per query). La causa: il piano di tract (pesi + grafo ottimizzato,
~470 MB vivi, vedi sotto) più i transitori di `into_typed`/
`into_optimized`, pagine che il malloc di macOS **non restituisce** al
sistema — ma riusa: tre giri di carico/scarico con gc in mezzo danno
1156 → 1627 → 1654 MB (il secondo piano non raddoppia, il terzo quasi
non cresce: l'arena viene riusata, non è una perdita di oggetti vivi).
Nessuna correzione piccola era disponibile: caricare senza
`into_optimized` dimezzerebbe la RAM ma rallenterebbe ogni embedding —
una scelta di motore diversa, non un ritocco. Homun misura 251→896 MB
nel suo demone (+645): la stessa forma, tra il costo vivo (~470) e il
picco di carico (~1100), coerente con la misura presa dopo l'assestamento.

**Non serve rifare il banco della ricerca** (spec): il percorso di
ricerca non è cambiato; le prove di velocità sono nei tempi qui sopra
(1000 ibride in 536 ms, k 5, una cella).

## Chiusura

- `cargo fmt --check`, `cargo clippy --all-targets --offline -- -D
  warnings` verdi; `cargo test --offline --workspace` in debug **25
  suite ok** e in `--release` **25 suite ok**; `cargo deny check
  licenses bans` ok;
- `node --test` (Node 24.22.1) **25/25**; `npm run test:electron`
  (l'Electron di Homun in modo Node) **25/25**; `npm run
  test:electron-main` ok (il modulo nel processo principale vero);
- i test JS trovano la CLI anche con `CARGO_TARGET_DIR` impostato
  (riserva della v0.4.1, sistemata: `engrammaBin()` legge la variabile
  e poi `target/`; provato con e senza);
- `npm run build` rifatto: `engramma.darwin-arm64.node`, `index.js` e
  `index.d.ts` corrispondono al tag;
- temporanei solo in `/var/folders/nb/r6fbsdsn3z58bkbb9w55nn780000gn/T/`
  e cancellati a fine fase (sonde, prove RAM, cartelle dei test);
- tag `fase-v0.4.2`. Nessun push.

## Scelte e dichiarazioni di questa fase

1. **Nessun tetto, né in alto né in silenzio**: la decisione è non avere
   massimi su `limit`/`k` (processo locale, id in memoria) e trasformare
   i massimi del MOTORE (`graphHops` 0–2, `depth` 1–4) in errori. Lo
   zero è `INVALID_ARGUMENT` ovunque: una richiesta di nulla è un bug.
2. **La lista di default resta dei vivi**: aggiungere le lapidi a chi
   non chiede `status` avrebbe cambiato un contratto che Homun usa (i
   conteggi della sua interfaccia); chi vuole le lapidi le chiede.
3. **`dropCell` non tiene un registro dei motivi**: della cella non
   resta nulla in cui scriverlo — il contrario dell'oblio sarebbe
   scrivere «abbiamo dimenticato cosa e perché». `reason` valida e
   resta al chiamante.
4. **Una vista raggiungibile e una no, in due scenari**: una nota ha UN
   file legato (vincolo strutturale, il secondo `vault init` ribinda);
   il test onesto è due celle con una vista ciascuna. La scoprivo
   scrivendo il test sbagliato: la prima vista restava con copie orfane
   e zero righe. Semantica preesistente, fuori dagli scopi di questa
   fase, documentata qui e nel test.
5. **La finestra BM25 segue `k`, pavimento 50**: toccava il core, ma un
   `k` che il motore taglia a 50 senza dirlo è lo stesso contratto che
   mente — solo un piano più sotto. Per `k ≤ 50` (tutti i chiamanti di
   sempre) il profilo è identico.

## Cosa non ho potuto provare

- **Windows e Linux**: solo macOS arm64, come nelle fasi precedenti.
- **cargo deny advisories**: richiedono rete; eseguito `licenses bans`.
- **Il ramo «modello verificato ma non caricabile»** del punto 4: la
  verifica sha256 precede il carico, quindi serve un modello con l'hash
  giusto e i byte rotti — irraggiungibile senza una collisione. Il
  codice c'è (rifiuta `IO`, slot vuoto, motore in testo+grafo), il test
  no.
- **L'effettivo rilascio della RAM di tract alla distanza**: misurato
  che l'arena viene riusata (tre giri) e che gli oggetti vivi non
  crescono; il comportamento page-per-page del malloc di macOS non è
  osservabile da qui.
