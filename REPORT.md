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
  «sorella» dal corpo e il titolo; le note chiave portano le relazioni
  nella sezione `## Relations` (il vincolo sintattico che la catena usa).
- Le cinque: «dove lavora il partner di Fabio», «come si chiama il medico
  del partner di Fabio», «dove lavora la sorella di Giulia», «chi è il
  marito della sorella di Giulia», «come si chiama il capoprogetto per cui
  lavora Fabio». In ibrido, all'istante fisso del test, tutte e cinque
  rispondono nei primi 5 (test `golden_semantic` col 97M).
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

Le cinque multi-salto in ibrido (primi risultati e `via`; all'istante del
test tutte rispondono nei primi 5):

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
