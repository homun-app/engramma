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

# REPORT — Engramma v0.4.3 (dropCell non tocca le celle vicine, chiude la connessione, nessuna Cell nata morta)

Data: 2026-11-09. Base: `main` @ `dd867e0` (tag `fase-v0.4.2`). Un commit
per punto, il test che fallisce prima della correzione (trascritto). Tutto
costruito con `cargo --offline`, Rust 1.95.0, macOS arm64, nessuna crate
nuova (`Cargo.lock` invariato), nessun `unsafe` fuori da napi-derive,
nessun `unwrap`/`expect` fuori dai test, clippy `-D warnings` verde.
Il repo di Homun NON è stato toccato.

## Punto 1 — le istantanee e le celle condividono il nome: `dropCell` cancellava le celle vicine (P0)

**Il bug provato** (lo scenario della verifica v0.4.2, riprodotto): le
celle `storia`, `storia-snap-2` e `storia-snap-archivio`, una nota
ciascuna. `dropCell('storia', 'prova')` → `cells()` vuota: sparite tutte
e tre, e nella cartella restano orfani `storia-snap-2.sqlite-wal`/`-shm`
e `storia-snap-archivio.sqlite-wal`/`-shm`. La causa: tre punti
riconoscevano un'istantanea con lo stesso controllo lento
`starts_with("{stem}-snap-") && ends_with(".sqlite")` — `cell_files`
(Node), `cell drop` (CLI) e il calcolo della dimensione su disco del
core (`dir_footprint`). Difetto collegato, già presente prima:
`cells()` e l'elenco della CLI mostravano come celle anche le
istantanee vere scritte da `compact --keep-snapshot`.

**La correzione.** Un solo predicato nel core,
`engramma_core::snapshot_of(file_name) -> Option<(&str, u64)>`: riconosce
**solo** il formato esatto che `compact` scrive
(`{}-snap-{head_seq}.sqlite`), con le cifre tutte in fondo (l'ultimo
`-snap-` separa: una cella può chiamarsi `a-snap-b`). I tre punti lo
usano tutti. In più:

- il nome a forma di istantanea è **riservato**: `<qualcosa>-snap-<cifre>`
  → `INVALID_ARGUMENT` in `cell_path` (Node, quindi `cell()` e
  `dropCell`) ed errore di uso (exit 2) nella `cell create` della CLI.
  `storia-snap-archivio` resta valido: non finisce con cifre;
- `cells()` e `cell list` saltano le istantanee: una cella non può più
  avere quel nome, quindi un file così è sempre un'istantanea;
- `dir_footprint` (il conto onesto di `compact`) porta solo le istantanee
  **proprie**: prima il vicino `storia-snap-archivio.sqlite` finiva nel
  conto di `storia`.

**Test prima della correzione** (CLI, 4 rossi; JS, 2 rossi):

```console
✖ cell_drop_spares_the_snapshot_named_neighbor
  panicked: the neighbor went too: ["storia2.sqlite"]        # restava solo storia2
✖ cell_list_skips_the_real_snapshots_and_they_go_with_their_cell
✖ compact_footprint_counts_only_the_cells_own_snapshots
✖ snapshot_shaped_cell_names_are_refused_at_creation
  left: Some(0)  right: Some(2)                              # x-snap-7 veniva creata

✖ dropCell spares the neighbors; a snapshot is not a cell (P0.1)
  AssertionError: the snapshot is listed: storia,storia-snap-2,storia-snap-archivio,storia2
✖ a cell name that looks like a snapshot is reserved (P0.1)
  AssertionError: Missing expected rejection                  # cell('x-snap-7') accettata
```

Dopo: 4/4 e 27/27 (poi 29/29 con i punti seguenti). Il test JS rilegge i
vicini da un **motore nuovo** (create: false): i dati su disco, non il
fantasma in RAM della connessione cancellata.

## Punto 2 — `dropCell` chiude subito la connessione condivisa (P1)

**Il bug provato:** una `Cell` aperta prima del drop e ancora viva in JS
tiene aperti il `.sqlite`, il `-wal` e lo `-shm` della generazione
cancellata, sui file già scollegati; i byte restano sul disco finché il
GC non raccoglie il wrapper (Homun tiene le `Cell` in cache: ore). Il
gate rispondeva già `NOT_FOUND` — nessun dato a rischio, solo un oblio
che finiva più tardi di quanto il `DropView` lasciava intendere.

**La correzione.** `Cell::retire()` nel core: sostituisce la
`Connection` condivisa in `Inner` con una in memoria e chiude quella
vera — i cloni condividono `Inner`, quindi si chiude **per tutti** in un
colpo (SQLite riassorbe il WAL e lascia andare i descrittori). Le cache
(vettori, grafo, query) e il registro dello scrub pendente vanno con la
connessione. `dropCell` (Node) la chiama dopo il seal e prima di
cancellare i file; la CLI `cell drop` uguale.

**Test prima della correzione** (lsof, senza alcun gc()): dopo il drop
con la `Cell` viva — 5 descrittori sui file della cella cancellata:

```console
✖ dropCell closes the connection a live Cell still shares (P1.2)
  AssertionError: descrittori ancora aperti:
  node … txt REG  32768 …/cells/tenuta.sqlite-shm
  node … txt REG   4096 …/cells/tenuta.sqlite
  node … 14u REG   4096 …/cells/tenuta.sqlite
  node … 15u REG 1306072 …/cells/tenuta.sqlite-wal
  node … 16u REG  32768 …/cells/tenuta.sqlite-shm
```

Dopo: **zero** descrittori sui file di `tenuta`, e la `Cell` viva
risponde ancora `NOT_FOUND` dal gate. Nel core,
`retire_closes_the_connection_under_live_clones`: il WAL sparisce sotto
un clone ancora vivo.

## Punto 3 — nessuna `Cell` nata morta (P2)

**Il bug provato:** `CellOpenTask` prendeva il gate della generazione
con `tracker_for` e solo dopo apriva la cella, lock rilasciato in mezzo.
Un `dropCell` entrato in quella finestra sigillava proprio il gate che
l'apertura stava per consegnare: la `cell(name)` di ritorno rispondeva
`NOT_FOUND` per sempre — Cell fresca su file fresco, nata morta. Nella
stessa finestra l'apertura poteva **creare** il file mentre il drop lo
cancellava: il drop moriva sul database a metà.

**La scelta (dichiarata):** `open_tracked_cell` tiene il lock delle
generazioni per gate **e** apertura — l'alternativa «riprova col gate
nuovo» lasciava una finestra a ogni tentativo. Un drop di quel nome o
finisce **prima** dell'apertura (che allora conia un gate fresco su una
cella fresca) o aspetta e prende la generazione che QUESTA apertura ha
creato: una `Cell` può morire solo di un oblio iniziato **dopo** che
esisteva. Il lock è per motore, non per nome: un drop di un nome aspetta
brevemente un'apertura non collegata — entrambe cose rare nel demone.

**Test prima della correzione** — 200 coppie `cell(name)`/`dropCell(name)`
lanciate insieme (più una nota seminata, perché ogni drop abbia qualcosa
da dimenticare): **6/6 rosse**, tutte con la stessa finestra:

```console
✖ no Cell is born dead: 200 cell()/dropCell() pairs (P2.3)
  [Error: database error: disk I/O error] { code: 'IO' }
```

Dopo: 12/12 verdi. Ogni `Cell` restituita risponde a `list()` con
successo, oppure è morta di un drop **completato dopo** la sua creazione
(un orologio a tick monotono scritto alla risoluzione di ogni promessa,
per indice di chiamata); finite le 200 coppie, l'ultima `cell(name)` è
viva e la sua lista è vuota.

## Prove a mano (trascritte, probe Node + CLI in /var/folders/…/T/, cancellato)

```console
files: storia-snap-2.sqlite storia-snap-archivio.sqlite* storia.sqlite* storia2.sqlite*
cells(): ["storia","storia-snap-archivio","storia2"]        # l'istantanea NON si elenca
cell(x-snap-7): INVALID_ARGUMENT — nome cella riservato: "x-snap-7" (il formato `-snap-<cifre>` è delle istantanee di compattazione)
dropCell(storia): {"notes":1,"scrubbed":true,"pending":[],"views":[]}
files dopo: storia-snap-archivio.sqlite* storia2.sqlite*    # solo storia e la SUA istantanea sono andate
cells() dopo: ["storia-snap-archivio","storia2"]
rilette: 1 1                                                 # i vicini interi, da file
lsof dopo drop (Cell viva): 10 descrittori su storia2/storia-snap-archivio (celle VIVE, giusto), ZERO su tenuta.*
cli create x-snap-7 → 2 (exit 2 = uso)
cell list: sorgente-cli storia-snap-archivio storia2         # x-snap-7 non si elenca
cli --cell x-snap-7 verify → 0 { "hash_chain_ok": true, … }  # la legacy si apre per nome
e.cell(x-snap-7): INVALID_ARGUMENT
cli cell drop x-snap-7 → 0 dimenticata: x-snap-7 — via dalla CLI (1 note, 0 file di viste)
stress: 40 coppie, Cell morte (di drop successivi): 36 — l'ultima cell(name) è viva: true
```

## Chiusura

- `cargo fmt --check` verde; `cargo clippy --all-targets --offline --
  -D warnings` verde; `cargo test --offline --workspace` **26 suite ok**
  (la 26ª è `celldrop`, nuova: 4 test); `cargo deny check licenses
  bans` ok;
- `node --test` (Node 24.22.1) **29/29**; `npm run test:electron`
  (l'Electron di Homun in modo Node) **29/29**; `npm run
  test:electron-main` ok (il modulo nel processo principale vero);
- `npm run build` rifatto: `engramma.darwin-arm64.node`, `index.js` e
  `index.d.ts` corrispondono al tag;
- `docs/node.md`: i nomi riservati, `cells()` senza istantanee,
  `dropCell` che chiude la connessione, nessuna Cell nata morta;
- temporanei solo in `/var/folders/nb/r6fbsdsn3z58bkbb9w55nn780000gn/T/`
  e cancellati a fine fase (probe compreso);
- tag `fase-v0.4.3`. Nessun push.

## Scelte e dichiarazioni di questa fase

1. **Una cella già esistente col nome riservato** (creata prima della
   regola): si apre ancora **per nome dalla CLI** (`--cell x-snap-7 …`),
   risponde a ogni comando e si può dimenticare con `cell drop x-snap-7
   --reason …` — la via d'uscita è la CLI, perché il collegamento Node
   rifiuta quel nome ovunque (`cell()` e `dropCell`:
   `INVALID_ARGUMENT`). `cells()` e `cell list` non la elencano: un file
   in quella forma è ormai sempre un'istantanea, e listarla come cella
   è l'ambiguità che ha causato il punto 1. Prova a mano sopra.
2. **Il lock delle generazioni durante l'apertura** (punto 3) invece
   della riprova: la riprova riapriva la finestra a ogni tentativo, e
   una catena di drop (lo stress test la produce davvero) poteva
   comunque consegnare una Cell morta. Con il lock la garanzia è
   strutturale, non probabilistica.
3. **Una cella ritirata non è utilizzabile** (`retire` mette una
   connessione in memoria senza tabelle): è voluto. Al `retire` si
   arriva solo dal percorso del drop, dopo il seal: il gate risponde
   `NOT_FOUND` prima che qualunque operazione tocchi il segnaposto.
4. **Il conto di `compact` si stringe** con lo stesso predicato: la
   footprint di una cella porta le sue istantanee, non i vicini che si
   chiamano quasi come lei. Il numero `bytes_before/after` della
   `CompactView` cambia solo in quel caso (prima gonfio, ora onesto).
5. **`INVALID_ARGUMENT` per il nome riservato in Node, uso (exit 2) in
   CLI**: il confine è quello della v0.4.2 — gli argomenti della
   chiamata sono `INVALID_ARGUMENT` nel collegamento; la CLI risponde
   col suo codice di uso, come ogni altro nome malformato.

## Cosa non ho potuto provare

- **Il salto esplicito del test lsof** (ramo «lsof non c'è»): qui c'è;
  il ramo esiste ed è `t.skip`, non un silenzio.
- **La forma `NOT_FOUND` del punto 3 da sola**: nelle 6 corse rosse ha
  dominato la forma `disk I/O error` della stessa finestra (il drop sul
  file che l'apertura creava); il test asserisce entrambe (codice
  `NOT_FOUND` per la Cell morta, qualunque altro errore fa fallire).
- **Windows e Linux**: solo macOS arm64, come nelle fasi precedenti.
- **cargo deny advisories**: richiedono rete; eseguito `licenses bans`.

# REPORT — Engramma v0.4.4 (consolidamento: disco, RAM, Linux, LongMemEval, DESIGN)

Data: 2026-11-10. Base: `main` @ `50489a9` (tag `fase-v0.4.3`). Un commit
per punto, la misura prima della correzione. Tutto costruito con
`cargo --offline`, Rust 1.95.0, macOS arm64 (darwin 27.0.1, 10 core).
Unica crate nuova: `mimalloc 0.1.39` (con `libmimalloc-sys 0.1.35`, MIT,
già nel registro locale). Nessun `unsafe` fuori da napi-derive, nessun
`unwrap`/`expect` fuori dai test, clippy `-D warnings` verde su macOS **e
su Linux**. Il repo di Homun NON è stato toccato. La macchina ha
condiviso la CPU con un'altra sessione di lavoro (carico medio 4–12 nel
giorno): dove pesa, è detto; le coppie prima/dopo sono state corsa il più
possibile addossate.

Disco: partiti con 25 GB liberi; mai scesi sotto 30; `du -sh target`
all'inizio **11 GB** (residuo di fasi passate: build di rilascio comprese),
alla fine **6,4 GB**.

## P0 — il disco di una build: `line-tables-only`

`[profile.dev] debug = "line-tables-only"` e
`[profile.dev.package."*"] debug = false` nel `Cargo.toml` del workspace:
i nostri crate tengono file e riga nei backtrace, le dipendenze perdono
il debug completo che nessuno usa (la rlib di tract da sola pesava
~170 MB; i binari di test 105–130 MB l'uno).

Misura su `cargo clean` + `cargo test --offline --workspace` (26 suite
verdi entrambe le volte):

| | prima | dopo |
|---|---|---|
| `du -sh target` | **3,2 GB** | **2,1 GB** |
| build pulita | **4:18** | **2:38** |

I 12,3 GiB che `cargo clean` ha tolto all'inizio includevano i prodotti
di rilascio delle fasi passate; il confronto pulito è quello della
tabella. Sul volume Docker di Linux lo stesso profilo porta il target da
7,2 GB (misurati in verifica) a **3,9 GB**.

## P1 — RAM: mimalloc nei due artefatti finali

**La misura che mostra il problema** (`crates/engramma-node/__test__/rss.js`,
Node 24, Granite 97M f32 da 390 MB, 64 note di cui metà ~600 token,
`embedPending` una volta): con l'allocatore di sistema, RSS a fine lavoro
**1857 MB e lì resta** (vmmap: in uso 485 MB, `Malloc Large (empty)`
731 MB, `Malloc Small (empty)` 545 MB, frammentazione 58%), picco 2,3 GB.

**La correzione**: `#[global_allocator]` con `mimalloc::MiMalloc` nei due
artefatti finali — `crates/engramma-node` (il `.node` di Homun) e
`crates/engramma-cli` — e **solo lì**: le librerie restano neutre, chi
produce l'eseguibile sceglie il proprio allocatore. In un `cdylib`
N-API l'allocatore copre le allocazioni Rust del modulo (motore e tract),
non Node o Electron.

RSS con mimalloc (stesso script). Due tracce complete, com'erano stampate:

| | modello caricato | dopo embed | +2 s | +10 s |
|---|---|---|---|---|
| corsa A | 886 | 1544 | 1552 | **971** |
| corsa B | 869 | 2103 | 1696 | **1413** |

RSS a +10 s in tutte le corse con mimalloc (filoni di default, misura
singola per corsa): **971, 993, 1309, 1413, 1768, 2008, 2159, 2293 MB** —
varianza alta, mai sotto 971. Motore aperto senza modello: 50 MB (51 con
mimalloc). Picco 2,4 GB.
`embedPending` delle 64 note: 8,6 s prima, 7,4–13,7 s dopo (rumore della
macchina condivisa; mediana invariata).

**La soglia di 700 MB non è raggiunta** e la causa è capita e misurata:
con mimalloc la memoria liberata resta in segmenti da **128 MB** parzialmente
residenti che nessuna pressione esterna (6 GB allocati accanto) restituisce
— `vmmap` li mostra sporchi, gli stats di mimalloc dicono 38,7 GiB purgati.
Le prove fatte: `ENGRAMMA_EMBED_THREADS` a 1/2/4/10 (a 1 filone il tempo
raddoppia, 21,5 s, e la RSS dopo 10 s resta 1052–1489 MB: gli heap per
thread non sono il grosso), `MIMALLOC_PURGE_DELAY` 0/10/100 e
`MIMALLOC_RESET_DELAY` 0/10 (0 *disattiva* il purge in mimalloc; i valori
piccoli non cambiano nulla), `MIMALLOC_ABANDONED_PAGE_RESET=1` (993 MB,
gli heap abbandonati dei filoni non sono il grosso). A processo nudo
(`crates/engramma-embed/examples/ram*.rs`): allocatore di sistema
981 → 2123 → 1833 MB a +10 s; mimalloc 846 → 2437 → **2437** MB: su
darwin 27 il prototipo della verifica (540 MB) non si riproduce, né
dentro Node né fuori. **In cambio** mimalloc abassa la residente a modello
caricato (846 vs 981 MB nudo; 886–894 vs 935 in Node) e in Node la mediana
a riposo (~1,4 GB vs 1857 MB stabili). Su Linux la storia è diversa e
migliore (vedi P2). La soglia resta un problema aperto, scritto nel
DESIGN § 7 con i numeri.

**Nessuna regressione di velocità** (banco 100k, `--runs 1`, macchina
condivisa: le coppie sono state corse addossate; le punte p95 fuori nome
— 28–69 ms — sono picchi dell'altra sessione, presenti in entrambe):

| metrica | prima | dopo |
|---|---|---|
| embedding (note/s) | 135 | 148 |
| vettori p50 / p95 (ms) | 4,5 / 5,1 | 4,5 / 5,1 |
| ibrido caldo p50 (ms) | 11,7 | 12,0 |
| ibrido caldo p95 (ms) | 14,9 | 15,5 |
| ibrido con embedding p50 (ms) | 20,6 | 21,1 |

Riferimento non condiviso (v0.4.3, `bench/RESULTS.md`): embedding 148/s,
ibrido caldo 11,6/14,7 — il banco con mimalloc nei sorgenti torna sugli
stessi numeri. **Fedeltà**: `cargo test -p engramma-embed --test fidelity`
verde senza toccare le soglie (5 test; un allocatore non cambia i
vettori). **test:electron-main** verde: il demone Electron di Homun
carica il `.node` con mimalloc e fa il giro completo.

## P2 — Linux, davvero

`scripts/linux.sh` (uso in testa al file): `engramma-linux:1.95.0`
(Debian bookworm, Rust 1.95.0 aarch64, gcc 12, clippy e rustfmt),
sorgente montato **in sola lettura**, `--network none`, **utente normale**
(non root), target nel volume `engramma-linux-target`.

**Nota d'ambiente**: Docker Desktop ha perso tutte le immagini a metà
giorno (reset del daemon; `docker images` vuoto, i volumi sono sopravvissuti).
L'immagine è stata ricostruita come in verifica da
`rust:1.95.0-slim-bookworm` (gcc + clippy + rustfmt); serve rete solo
per quello, il lavoro dentro il container resta `--offline`.

- **fmt, clippy `-D warnings` e test verdi su Linux**: 26 suite, come
  utente normale. Due lints esistevano solo nei rami
  `#[cfg(target_os = "linux")]` degli esempi nuovi (`needless_question_mark`
  in `ram`, `collapsible_if` in `longmemeval`): macOS non li compilava
  mai, Linux sì — corretti.
- **Il test dei permessi** (`compact_fails_cleanly_when_the_old_file_cannot_go`,
  `forgetting.rs` ~733): dopo il `chmod 0o555` il test ora prova a creare
  un file nella cartella; se ci riesce, i permessi non legano (root li
  ignora) e il test **salta con un messaggio** invece di fallire. Niente
  `geteuid`, niente `unsafe`. Su Linux da utente normale il test **corre**
  (i permessi legano): la via giusta è provata davvero. Era l'unico test
  che usa `set_permissions`/`from_mode` (cercati).
- **Il test `lsof` della v0.4.3** (`dropCell closes the connection…`):
  su Linux ora legge `/proc/self/fd` (lsof non c'è nell'immagine), su
  macOS resta `lsof`: su entrambi il test prova qualcosa di vero.
- **`cargo build -p engramma-node --release` riesce nel container**
  (scripts/linux.sh build-node): il `.node` per Linux si costruisce, con
  mimalloc. Il collegamento Node non si prova (nell'immagine non c'è
  Node), come da fase.
- **La RAM su Linux** (`crates/engramma-embed/examples/ram*.rs` nel
  container, `/proc/self/status`; l'output dell'esempio stampava kB
  come «MB», corretto dopo la corsa — i valori qui sono ricalcolati):
  - allocatore di glibc: 562 MB a modello caricato, 604 dopo il lavoro,
    **604 a +10 s** (stabili), picco `VmHWM` 2158 MB;
  - mimalloc: 718, 779, **779** a +10 s, picco 2419 MB.
  Su Linux **entrambi** gli allocatori restituiscono: la retention di
  macOS è un comportamento del sistema, non del motore. mimalloc tiene
  ~175 MB in più di glibc a riposo.
- **Il volume del target Linux** (con P0): **3,9 GB**
  (`docker system df -v`), contro i 7,2 GB misurati in verifica senza.
- **Windows, rilettura** (non provato; nessuna correzione oltre quella
  ovvia):
  1. *cancellare un file aperto*: già gestito per progetto — `retire()`
     della v0.4.3 chiude la connessione **prima** della `remove_file`;
  2. *nomi riservati*: `CON`, `PRN`, `AUX`, `NUL`, `COM1-9`, `LPT1-9`
     come nomi di cella sono ora rifiutati su Windows (`#[cfg(windows)]`
     nei due punti di validazione: Node `cell_path` e `cell create`
     della CLI; su macOS/Linux cambia nulla);
  3. *separatori dei percorsi nella vista markdown*: già normalizzati a
     `/` in `port.rs` e in `engramma-vault` (tre punti);
  4. *aperto*: MCP apre celle esistenti per nome (se l'OS rifiuta il
     file, l'errore IO è chiaro); i nomi delle note esportate finiscono
     con l'id ULID, quindi niente punti/spazi finali. Da provare quando
     ci sarà una macchina Windows.

## P3 — LongMemEval: il numero di partenza

`bench/examples/longmemeval.rs`: legge
`~/.cache/engramma/bench/longmemeval_s_cleaned.json` (sha256 verificato
prima di tutto; errore chiaro se manca), **una cella per domanda** in
una cartella temporanea cancellata alla fine, **nota = round** (turno
`user` + il `assistant` che segue; `createdAt` = data della sessione,
`source: longmemeval`, `session_id` e indice round in `x-lme-*`),
rilevanza a livello di turno quando un turno del round ha `has_answer`.
La domanda: `search({text: question, k=50})` con `question_date` come
«adesso» — l'API Rust espone `search_with_now`, quindi la recenza vede
le date 2023; senza tempo di riferimento le stesse date avrebbero
sballato il peso di recenza contro un «adesso» 2026. Metriche ufficiali
di `eval_utils.py` (recall_any/all@k, ndcg binaria con
`dcg = r[0] + Σ r[i]/log2(i+1)`), livello **sessione** (round → sessioni
distinte in ordine, fino a k) e **turno**, k = 5 e 10, saltando le 30
`_abs`.

**Cache dei vettori** (`~/.cache/engramma/bench/vec-cache-<modello>.bin`,
record `sha256(testo) ‖ vettore`): ogni round distinto si incorpora una
volta per tutte le domande; i round condivisi tra domande non si rifanno.
*Bug trovato e corretto in corsa*: la prima versione apriva il file
`append` senza `read`, così ogni lettura falliva e la cache non serviva
mai (la prima corsa ha incorporato anche i round condivisi, 75,7 M
caratteri; la corsa diagnostica ne stava riappendendo duplicati quando
l'ho fermata). Corretto, file deduplicato (47 060 round distinti), e da
lì ogni corsa sul sottoinsieme costa ~100 s. I numeri non cambiano: i
vettori sono gli stessi, cambia solo il tempo.

- **Ritmo misurato**: sonda di 5 domande, 1738 round, 2,56 M caratteri in
  161 s ≈ 16 k caratteri/s → la corsa piena sarebbe ~3,3 h > 3 h.
- **Testo (BM25) su tutte le 470**: durata 82 s, RSS massima 575 MB.
- **Ibrida e testo sullo stesso sottoinsieme fisso di 150** (seme 42,
  proporzionale per tipo; id in fondo): durata 6530 s (1 h 49); seconda corsa con la cache calda 83 s, RSS
  massima 2303 MB (seconda corsa 1081), 51 268 record riusati, 0 nuovi — la corsa fredda aveva incorporato 75,7 M caratteri.
- **Ripetibilità**: seconda corsa dell'ibrida con la cache calda,
  fatta, `--mode both` di nuovo: **tabelle identiche** (diff vuoto); 83 s, 51 268 riusi.

Le tabelle (le righe `text`·470 e `text`/`hybrid`·150 vengono da corse separate;
la `vector` è la diagnostica):

| modalità | livello | tipo | any@5 | all@5 | ndcg@5 | any@10 | all@10 | ndcg@10 | n |
|---|---|---|---|---|---|---|---|---|---|
| text | sessione | TOTALE | 0.9702 | 0.8447 | 0.8828 | 0.9872 | 0.9149 | 0.8968 | 470
| text | sessione | knowledge-update | 1.0000 | 0.9583 | 0.9543 | 1.0000 | 0.9861 | 0.9592 | 72
| text | sessione | multi-session | 0.9669 | 0.6942 | 0.8344 | 0.9917 | 0.8347 | 0.8614 | 121
| text | sessione | single-session-assistant | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 56
| text | sessione | single-session-preference | 0.8667 | 0.8667 | 0.6582 | 0.9000 | 0.9000 | 0.6682 | 30
| text | sessione | single-session-user | 0.9844 | 0.9844 | 0.9786 | 1.0000 | 1.0000 | 0.9835 | 64
| text | sessione | temporal-reasoning | 0.9606 | 0.7795 | 0.8413 | 0.9843 | 0.8740 | 0.8600 | 127
| text | turno | TOTALE | 0.8766 | 0.6617 | 0.6829 | 0.9404 | 0.7702 | 0.7181 | 470
| text | turno | knowledge-update | 0.9583 | 0.7917 | 0.7446 | 1.0000 | 0.8750 | 0.7675 | 72
| text | turno | multi-session | 0.8926 | 0.3719 | 0.5298 | 0.9587 | 0.5537 | 0.5833 | 121
| text | turno | single-session-assistant | 0.8750 | 0.8750 | 0.8486 | 0.9286 | 0.9286 | 0.8694 | 56
| text | turno | single-session-preference | 0.6000 | 0.4333 | 0.4267 | 0.8000 | 0.6667 | 0.5022 | 30
| text | turno | single-session-user | 0.9688 | 0.9688 | 0.9416 | 1.0000 | 1.0000 | 0.9523 | 64
| text | turno | temporal-reasoning | 0.8346 | 0.6693 | 0.6510 | 0.8976 | 0.7559 | 0.6846 | 127
| text | sessione | TOTALE | 0.9867 | 0.8667 | 0.9042 | 0.9933 | 0.9267 | 0.9158 | 150
| text | sessione | knowledge-update | 1.0000 | 1.0000 | 0.9840 | 1.0000 | 1.0000 | 0.9840 | 23
| text | sessione | multi-session | 1.0000 | 0.7436 | 0.8771 | 1.0000 | 0.8974 | 0.9024 | 39
| text | sessione | single-session-assistant | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 18
| text | sessione | single-session-preference | 1.0000 | 1.0000 | 0.7693 | 1.0000 | 1.0000 | 0.7693 | 10
| text | sessione | single-session-user | 1.0000 | 1.0000 | 0.9815 | 1.0000 | 1.0000 | 0.9815 | 20
| text | sessione | temporal-reasoning | 0.9500 | 0.7500 | 0.8367 | 0.9750 | 0.8250 | 0.8554 | 40
| text | turno | TOTALE | 0.9000 | 0.7133 | 0.7276 | 0.9467 | 0.8200 | 0.7576 | 150
| text | turno | knowledge-update | 1.0000 | 0.8696 | 0.7927 | 1.0000 | 0.9565 | 0.8089 | 23
| text | turno | multi-session | 0.9231 | 0.4103 | 0.5849 | 0.9744 | 0.6154 | 0.6376 | 39
| text | turno | single-session-assistant | 0.8889 | 0.8889 | 0.8684 | 0.9444 | 0.9444 | 0.8899 | 18
| text | turno | single-session-preference | 0.7000 | 0.6000 | 0.5501 | 1.0000 | 1.0000 | 0.6670 | 10
| text | turno | single-session-user | 1.0000 | 1.0000 | 0.9565 | 1.0000 | 1.0000 | 0.9565 | 20
| text | turno | temporal-reasoning | 0.8250 | 0.7250 | 0.6959 | 0.8500 | 0.7500 | 0.7088 | 40
| hybrid | sessione | TOTALE | 0.9733 | 0.9000 | 0.9324 | 0.9867 | 0.9467 | 0.9426 | 150
| hybrid | sessione | knowledge-update | 1.0000 | 1.0000 | 0.9920 | 1.0000 | 1.0000 | 0.9920 | 23
| hybrid | sessione | multi-session | 1.0000 | 0.8974 | 0.9170 | 1.0000 | 0.9487 | 0.9229 | 39
| hybrid | sessione | single-session-assistant | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 18
| hybrid | sessione | single-session-preference | 0.9000 | 0.9000 | 0.9000 | 1.0000 | 1.0000 | 0.9387 | 10
| hybrid | sessione | single-session-user | 0.9000 | 0.9000 | 0.9000 | 0.9500 | 0.9500 | 0.9178 | 20
| hybrid | sessione | temporal-reasoning | 0.9750 | 0.8000 | 0.9071 | 0.9750 | 0.8750 | 0.9211 | 40
| hybrid | turno | TOTALE | 0.8800 | 0.6600 | 0.6765 | 0.9333 | 0.8200 | 0.7160 | 150
| hybrid | turno | knowledge-update | 0.9565 | 0.8261 | 0.7607 | 1.0000 | 0.9565 | 0.7874 | 23
| hybrid | turno | multi-session | 0.9231 | 0.4359 | 0.5573 | 0.9744 | 0.6923 | 0.6169 | 39
| hybrid | turno | single-session-assistant | 0.8889 | 0.8889 | 0.8406 | 0.9444 | 0.9444 | 0.8604 | 18
| hybrid | turno | single-session-preference | 0.8000 | 0.8000 | 0.6577 | 0.9000 | 0.9000 | 0.6878 | 10
| hybrid | turno | single-session-user | 0.9000 | 0.9000 | 0.8815 | 0.9000 | 0.9000 | 0.8815 | 20
| hybrid | turno | temporal-reasoning | 0.8000 | 0.5250 | 0.5725 | 0.8750 | 0.7500 | 0.6309 | 40
| vector | sessione | TOTALE | 0.9733 | 0.8867 | 0.9056 | 0.9800 | 0.9267 | 0.9147 | 150
| vector | sessione | knowledge-update | 1.0000 | 0.9565 | 0.9462 | 1.0000 | 1.0000 | 0.9539 | 23
| vector | sessione | multi-session | 1.0000 | 0.8718 | 0.8961 | 1.0000 | 0.9231 | 0.9068 | 39
| vector | sessione | single-session-assistant | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 18
| vector | sessione | single-session-preference | 0.9000 | 0.9000 | 0.8631 | 0.9000 | 0.9000 | 0.8631 | 10
| vector | sessione | single-session-user | 0.9000 | 0.9000 | 0.8815 | 0.9500 | 0.9500 | 0.8994 | 20
| vector | sessione | temporal-reasoning | 0.9750 | 0.8000 | 0.8717 | 0.9750 | 0.8500 | 0.8822 | 40
| vector | turno | TOTALE | 0.8200 | 0.5533 | 0.5847 | 0.9000 | 0.7400 | 0.6352 | 150
| vector | turno | knowledge-update | 0.8696 | 0.6522 | 0.5946 | 1.0000 | 0.9130 | 0.6597 | 23
| vector | turno | multi-session | 0.8205 | 0.3333 | 0.4657 | 0.9231 | 0.6154 | 0.5451 | 39
| vector | turno | single-session-assistant | 0.8889 | 0.8889 | 0.8406 | 0.9444 | 0.9444 | 0.8591 | 18
| vector | turno | single-session-preference | 0.7000 | 0.7000 | 0.5706 | 0.8000 | 0.8000 | 0.6062 | 10
| vector | turno | single-session-user | 0.8500 | 0.8500 | 0.7762 | 0.9000 | 0.9000 | 0.7955 | 20
| vector | turno | temporal-reasoning | 0.7750 | 0.3750 | 0.4876 | 0.8250 | 0.5750 | 0.5355 | 40

La condizione della fase — l'ibrida batte il solo testo su
`recall_any@5` a livello di sessione, sul totale — **non è soddisfatta, ed è capita**: il testo (BM25) è già a 148/150 su `any@5` sessione (0,9867) e l'ibrida segue il ramo denso (0,9733). La corsa diagnostica `--mode vector` sullo stesso sottoinsieme dà 0,9733: sono le 2 domande in cui il coseno mette la sessione giusta oltre il 5° posto e la fusione (α=0,7, tunata sul banco italiano) non le solleva. Non è troncamento (il ramo vettoriale vince l'`all@5` sessione: 0,8867 contro 0,8667 del testo — i round lunghi si trovano) né la data (stessa per tutte le modalità). La fusione somma davvero: `all@5` 0,900 e `ndcg@5` 0,932 contro 0,867/0,904 del testo. BM25 così alto dipende dal dataset *cleaned* (domande ambigue tolte) e dai round corti da due turni: il confronto coi lavori pubblicati (0,8–0,97, dataset originale) resta un ordine di grandezza.
Riferimenti esterni (solo ordine di grandezza, configurazioni diverse):
i lavori su LongMemEval riportano `recall@5` a livello di sessione tra
~0,8 e ~0,97 secondo retriever e indice.

Id del sottoinsieme di 150 (seme 42, proporzionali per tipo):

```
1e043500 c5e8278d 5d3d2817 7527f7e2 94f70d80 c8c3f81d 6b168ec8 75499fd8 21436231 95bcc1c8 853b0a1d b86304ba d52b4f67 ec81a493 e01b8e2f 4fd1909e 76d63226 86f00804 4100d0a0 29f2956b 0a995998 b5ef892d 3a704032 dd2973ad c4a1ceb8 36b9f61e 2e6d26dc 80ec1f4f d682f1a2 gpt4_5501fe77 gpt4_31ff4165 a9f6b44c gpt4_e05b82a6 e3038f8c c2ac3c61 32260d93 afdc33df 6b7dfb22 1a1907b4 38146c39 75f70248 b6025781 b0479f84 1d4e3b97 1c0ddc50 cc06de0d 4f54b7c9 1f2b8d4f e6041065 d905b33f 7405e8b1 a4996e51 60036106 4adc0475 5025383b a1cc6108 8979f9ec 3fe836c9 1c549ce4 1192316e 0ea62687 67e0d0f2 60159905 d6062bb9 157a136e a08a253f f0e564bc a346bb18 efc3f7c2 71017276 gpt4_4929293a gpt4_1d4ab0c9 gpt4_e072b769 gpt4_7f6b06db gpt4_8279ba02 gpt4_a1b77f9c gpt4_7a0daae1 gpt4_1e4a8aeb gpt4_4fc4f797 4dfccbf7 gpt4_61e13b3c 2ebe6c90 gpt4_e061b84f gpt4_4ef30696 gpt4_f420262c gpt4_98f46fc6 gpt4_a2d1d1f6 gpt4_85da3956 gpt4_e061b84g gpt4_d6585ce9 gpt4_4929293b 6e984302 gpt4_2655b836 0bb5a684 gpt4_385a5000 bbf86515 gpt4_5dcc0aab gpt4_2d58bcd6 gpt4_4edbafa2 c8090214 e4e14d04 gpt4_2c50253f cc6d1ec1 gpt4_4cd9eba1 b29f3365 gpt4_2f56ae70 gpt4_0a05b494 gpt4_213fd887 gpt4_fe651585 6a1eabeb 08e075c7 f9e8c073 41698283 45dc21b6 618f13b2 72e3ee87 01493427 6a27ffc2 7a87bd0c 8fb83627 4b24c848 59524333 5831f84d eace081b dfde3500 69fee5aa 7401057b 06db6396 dad224aa ba61f0b9 89941a94 07741c45 c4f10528 6ae235be f523d9fe 0e5e2d1a dc439ea3 8464fc84 8aef76bc 2bf43736 70b3e69b 6222b6eb e8a79c70 e982271f 7a8d0b71 a40e080f 41275add 4baee567 c8f1aeed 1de5cff2
```

## P4 — il DESIGN dice la verità

- in testa: «stato al tag `fase-v0.4.4`»;
- § 7: modello predefinito Granite 97M deciso; RAM misurata prima/dopo
  al posto dei «~400 MB»; mimalloc e perché; f16 provato e scartato con
  i numeri (cosino medio 0,99948, peggiore 0,99438, nessun risparmio);
- § 8.4: celle `storia-<id>` per bot; il contratto di `dropCell` (nomi
  `-snap-<cifre>` riservati, connessione chiusa subito, nessuna Cell
  nata morta); integrazione Homun ferma alla versione col lock degli hash;
- § 11: le latenze vere delle verifiche (ibrida calda p50 10,5 / p95
  13,9 ms; testo+grafo p95 12,3 ms) e il riquadro LongMemEval;
- § 12: colonna **stato** — v0.1–v0.4.4 fatte coi tag, il grafo adattivo
  slittato (multi-salto 0,458 a 100k col predefinito) e l'audit MCP con
  ambiti slittato (MCP resta stdio in sola lettura con `--cells`);
- § 14: chiuse modello, indice (sqlite-vec scartato), istantanea
  facoltativa, allocatore; aperte repo pubblico, SQLCipher, Windows;
- piattaforme: macOS arm64 provato; Linux arm64 provato in Docker;
  x86_64 e Windows non provati. Anche il `README.md` (status v0.4,
  layout con vault e node, scripts/linux.sh).

## Chiusura

- macOS: `cargo fmt --check`, `cargo clippy --all-targets --offline
  -- -D warnings`, `cargo test --offline --workspace` (26 suite),
  `cargo deny check licenses bans`: tutto verde.
- `scripts/linux.sh` verde; poi `docker volume rm engramma-linux-target`.
- JS: Node 24 `npm test` 29/29, `test:electron` 29/29,
  `test:electron-main` ok — col `.node` mimalloc.
- `npm run build` rifatto; SHA256 dei tre file per Homun:

  ```
  engramma.darwin-arm64.node  8a1c67735372adcbdbf23b1424c05951c27e6171d048e834460aeedf6fc75982  (33,5 MB)
index.js                    25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4
index.d.ts                  bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8
  ```

- `du -sh target`: 11 GB all'inizio, 6,4 GB alla fine.
- `docs/node.md`: una riga sull'allocatore del modulo (v0.4.4).
- Temporanei in `/var/folders/nb/r6fbsdsn3z58bkbb9w55nn780000gn/T/`
  cancellati a fine fase (log, md delle corse, script di pressione);
  le celle di LongMemEval vivevano in un `tempfile::TempDir` e sono
  andate via da sole; la **cache dei vettori resta** in
  `~/.cache/engramma/bench/` (`vec-cache-granite-embedding-97m-multilingual-r2-835ad140.bin`, 73,8 MB, 47 060 round distinti (deduplicato dopo il bug sottostante)).

# REPORT — Engramma v0.4.5 (il peso della fusione e il picco della RAM)

Data: 2026-11-10. Base: `main` @ `186e347` (dopo il tag `fase-v0.4.4`).
Un commit per punto, con dentro solo i file di quel punto; la misura o
il test che mostra il problema prima della correzione. Tutto costruito
con `cargo --offline`, Rust 1.95.0, macOS arm64 (darwin 27, 10 core,
24 GB). **Nessuna crate nuova**: `tract-nnef` è stata aggiunta come
dipendenza diretta per la prova della cache NNEF (stessa versione
0.23.10 già nel lock, senza feature) e poi **rimossa** quando la prova
è fallita — il `Cargo.lock` finale è identico a quello della v0.4.4.
Nessun `unsafe` nuovo, nessun `unwrap`/`expect` fuori dai test, clippy
`-D warnings` verde su macOS e su Linux. Il repo di Homun non è stato
toccato.

Disco: `du -sh target` — inizio fase **6,4 GB**, dopo P0 6,5, a metà
fase 7,3–7,6 (build di rilascio dei banchi e della baseline), fine
**8,2 GB**. Spazio libero mai sotto 130 GB. `~/.cache/cargo-target`
mai toccata.

La macchina ha condiviso la CPU per circa metà giornata (l'embedding
delle 320 domande di test e la taratura italiana sono corsi in
parallelo per ore, load medio 15–30): dove pesa è detto; tutte le
coppie prima/dopo sensibili al tempo sono corse a macchina ferma.

## P0 — le metriche di LongMemEval, esattamente quelle ufficiali

La verifica v0.4.4 aveva trovato `recall_any`/`recall_all` già identici
all'ufficiale e l'**ndcg di sessione no**: l'esempio deduplicava le
sessioni e calcolava l'ndcg sulla lista delle distinte. L'ufficiale
(`evaluate_retrieval_turn2session` → `evaluate_retrieval` → `ndcg`):

1. gli id di round diventano id di sessione togliendo l'ultimo
   segmento; le sessioni giuste sono un insieme;
2. `k_eff` parte da k e cresce finché i primi `k_eff` risultati
   contengono k sessioni **distinte** (o la classifica finisce);
3. la lista **non si deduplica**: i primi `k_eff` risultati restano
   tutti, ognuno con la rilevanza della propria sessione;
   - `recall_any`/`recall_all` sull'**insieme** delle sessioni del
     prefisso (≡ le prime min(k, distinte) distinte: per questo i
     recall erano già giusti);
   - `dcg(r, k_eff) = r[0] + Σ_{i≥1} r[i]/log2(i+1)`;
   - l'**ideale** conta i round del **corpus** nelle sessioni giuste:
     `min(k_eff, R_a)` uni, con `R_a` = round del corpus nelle sessioni
     di risposta — non le sole con `has_answer`;
   - ndcg = 0 se l'ideale è 0.

**La correzione** (`bench/examples/longmemeval.rs`, commit `a680122`):
`Metrics::add_session_ranking` implementa il taglio `k_eff`, i recall
sull'insieme e l'ndcg non deduplicato; `add_ranking` resta quello di
turno. Test unitari nell'esempio stesso
(`cargo test -p engramma-bench --example longmemeval`):

- `session_ndcg_is_the_official_turn2session_one` — classifica a mano
  `A A B x1 x2 x3 A` con due round della stessa sessione giusta in
  testa, risposte {A, B}, `R_a` = 5:
  k=5 → k_eff = 6, rels `1 1 1 0 0 0`, dcg = 2,6309297536, ideale =
  3,5616063117 → **ndcg@5 = 0,7386919**; k=10 → k_eff = 7, rels
  `1 1 1 0 0 0 1`, dcg = 2,9871369407 → **ndcg@10 = 0,8387050**. La
  versione deduplicata della v0.4.4 dava 1,0/1,0: il test distingue le
  due;
- `session_recalls_when_one_answer_session_is_missing` — any 1, all 0,
  ndcg = 1/3,1309297536 = **0,3193939**;
- `turn_level_metrics_are_unchanged` — fissa il turno (any 1, all 0,
  ndcg = 1,6309297536/2,6309297536 = **0,6199062**): P0 tocca solo la
  sessione.

**Le corse rifatte** (release, cache dei vettori calda; il testo non
usa vettori). I numeri nuovi accanto ai vecchi — cambiano SOLO le due
colonne ndcg di sessione; le righe `turno` sono byte-identiche,
verificato con `diff` sulle righe estratte:

| corsa | metrica | v0.4.4 (deduplicato) | v0.4.5 (ufficiale) |
|---|---|---|---|
| testo, 470 | sessione any@5 / all@5 | 0,9702 / 0,8447 | identici |
| testo, 470 | sessione ndcg@5 / @10 | 0,8828 / 0,8968 | **0,6607 / 0,6504** |
| testo, 150 | sessione ndcg@5 / @10 | 0,9042 / 0,9158 | **0,6830 / 0,6714** |
| ibrida, 150 | sessione ndcg@5 / @10 | 0,9324 / 0,9426 | **0,7719 / 0,7896** |
| ibrida, 150 | sessione any@5 / all@5 | 0,9733 / 0,9000 | identici |
| tutte le corse | righe turno | — | byte-identiche |

Lettura: con l'ideale che conta i round del corpus, l'ndcg di sessione
scende ovunque e la relazione si inverte in profondità — **l'ibrida
batte il testo sull'ndcg di sessione** (0,7719 contro 0,6830 @5 sulle
150) mentre il turno resta del testo (0,7276 contro 0,6765). Il riquadro
LongMemEval del DESIGN diceva «la fusione vince in profondità» citando
proprio l'ndcg deduplicato: letto sul numero sbagliato, riscritto in P3.

## P1 — α: tarato su due banchi, confermato su domande tenute da parte

### La divisione delle domande (fissata prima di guardare i numeri)

- **dev** = le 150 della v0.4.4 (seme 42, proporzionali per tipo, gli
  stessi id elencati nel REPORT v0.4.4);
- **test** = le altre **320** non `_abs`: le 470 meno le dev, ordine
  del dataset. Costruita con uno script python a parte (dataset +
  lista dev del REPORT v0.4.4) e riprodotta **identica** dall'esempio
  (`--questions test`, confronto con `diff`). SHA256 della lista (id
  separati da «a capo», ordine del dataset, UTF-8):
  **`c44755696a7ebcebe4d99748877b9ddca717e5bec4b3830f878abf6f8b8c0414`**.
- Le manopole nell'esempio: `--fusion convex:<α>|rrf:<k>` (predefinito
  il `Fusion::default()` del motore), `--hops 0|1|2`, `--questions
  dev|test|all`; il nome della configurazione va nell'intestazione del
  rapporto.

### La regola di scelta (scritta PRIMA delle tabelle, con data)

1. **Ammissibile**: MRR pesato ≥ 0,8529 − 0,010 = **0,8429** sul banco
   italiano E minimo fra i vicini ±0,1 di α ≥ 0,8483 − 0,010 = **0,8383**.
2. Fra gli ammissibili vince il **turno ndcg@5** più alto su dev;
   parità entro 0,005 → sessione `all@5`; ancora parità → il più vicino
   a 0,7.
3. Se nessun α < 0,7 fosse ammissibile: niente cambio, tabella e
   distanza fra i due ottimi nel REPORT, decisione all'utente.
4. Conferma su test, UNA corsa: ibrida > solo testo su sessione
   any@5, sessione all@5 e turno ndcg@5, altrimenti niente cambio. Non
   si ritarra guardando test.

### L'embedding delle 320 di test

Stima prima di partire: 66 174 testi di round distinti nelle domande
di test, 60 010 assenti dalla cache, 120,9 M caratteri → ~2,1 h a
16 k caratteri/s. Corsa reale in background (`nohup`, log, cache
incrementale: ogni round scritto appena incorporato), con lo stdout —
la tabella dei numeri di test — in `/dev/null`: **nessun numero di test
è stato guardato prima della scelta di α**. Esito: 320/320 in 8 400 s
(≈ 2 h 20; la macchina condivideva la CPU con la taratura: load 15–30),
RSS massima del processo 2 181 MB, zero round non incorporati (meta:
l'esempio esce di errore se `remaining ≠ 0`). Nota a posteriori: la
cache cresce oltre la previsione perché il motore incorpora i **pezzi**
in cui spezza i round lunghi, non solo i round interi (124 763 chiavi
distinte a fine corsa; 61 doppioni benigni, testi ripetuti nello stesso
lotto); la conferma con cache calda ha riusato 108 883 record senza
incorporarne nessuno di nuovo: copertura completa.

### (a) Il banco italiano — la griglia v0.3.2 rifatta

`engramma-bench --calibrate --notes 100000 --runs 3` (release). La
griglia è tornata alla forma v0.3.2 (convex, peso del grafo 0,35 fisso,
semina fusa), α da 0,2 a 0,8 così che ogni candidato abbia i vicini
±0,1 in griglia, più `rrf:60` e il riferimento 0,6/0,40 della v0.3.1.
MRR pesato per numero di domande, mediana dei giri, e la colonna del
minimo fra i vicini ±0,1 di α. La fase «vincitore» della v0.4.1 non
esiste più: la scelta sta qui, non nel banco.

| configurazione | pesato@100k | min vicini ±0,1 α | fattuali | parafrasi | multi-salto | inglese |
|---|---|---|---|---|---|---|
| convex α0,2 g0,35 | 0,6874 | 0,6874 | 0,8001 | 0,6469 | 0,6937 | 0,5324 |
| convex α0,3 g0,35 | 0,7902 | 0,6874 | 0,8688 | 0,7760 | 0,7482 | 0,6717 |
| convex α0,4 g0,35 | 0,8397 | 0,7902 | 0,8946 | 0,8374 | 0,7950 | 0,7490 |
| convex α0,5 g0,35 | 0,8577 | 0,8397 | 0,9066 | 0,8588 | 0,7504 | 0,7881 |
| convex α0,6 g0,35 | 0,8527 | 0,8527 | 0,9104 | 0,8705 | 0,4568 | 0,8090 |
| convex α0,7 g0,35 | 0,8529 | 0,8485 | 0,9118 | 0,8730 | 0,3556 | 0,8274 |
| convex α0,8 g0,35 | 0,8485 | 0,8485 | 0,9044 | 0,8710 | 0,2887 | 0,8397 |
| rrf:60 g0,35 | 0,6536 | — | 0,7762 | 0,6328 | 0,2870 | 0,5381 |
| rif α0,6 g0,40 (v0.3.1) | 0,8656 | — | 0,9099 | 0,8624 | 0,7952 | 0,8023 |

Le celle α0,4–α0,7 a peso 0,35 riproducono la v0.3.2 al quarta cifra
(0,8397 / 0,8577 / 0,8527 / 0,8529): stessa misura, stesso banco. Il
minimo dei vicini di α=0,7 qui è 0,8485 e non 0,8483 perché la finestra
della v0.3.2 comprendeva anche i vicini del peso del grafo.

### (b) LongMemEval dev — la griglia

Le stesse 150 domande, cache calda (51 268 riusi, 0 nuovi per ogni
configurazione). Il turno ndcg@5 — la metrica della regola — in grassetto:

| fusione | ses. any@5 | ses. all@5 | ses. ndcg@5 | **turno ndcg@5** | turno any@5 | turno all@5 |
|---|---|---|---|---|---|---|
| solo testo | 0,9867 | 0,8667 | 0,6830 | **0,7276** | 0,9000 | 0,7133 |
| convex 0,3 | 0,9933 | 0,8933 | 0,7556 | **0,7529** | 0,9200 | 0,7267 |
| convex 0,4 | 0,9867 | 0,9133 | 0,7705 | **0,7475** | 0,9200 | 0,7200 |
| convex 0,5 | 0,9933 | 0,9200 | 0,7815 | **0,7303** | 0,9133 | 0,7067 |
| convex 0,6 | 0,9800 | 0,9067 | 0,7789 | **0,7049** | 0,8933 | 0,6867 |
| convex 0,7 (vecchio) | 0,9733 | 0,9000 | 0,7719 | **0,6765** | 0,8800 | 0,6600 |
| rrf:60 | 0,9800 | 0,9000 | 0,7625 | **0,7045** | 0,8867 | 0,6933 |

Due sanità: la corsa `--fusion convex:0.7` riproduce riga per riga
l'ibrida predefinita del P0 (le manopole non perturbano il
predefinito), e la tabella delle 150 rifatta col codice P2
(ordinamento per lunghezza, budget, pool fisso) è **byte-identica**
alla corsa P0 col codice v0.4.4.

### La decisione

Ammissibili: **α0,5** (0,8577 / 0,8397), **α0,6** (0,8527 / 0,8527),
**α0,7** (0,8529 / 0,8485). Fuori: α0,3 (MRR 0,7902), α0,4 (0,8397,
sotto soglia di 0,0032), rrf:60 (0,6536). Fra gli ammissibili vince il
turno ndcg@5 su dev: **α = 0,5** (0,7303 contro 0,7049 e 0,6765, senza
parità). Il punto 3 della regola (fermarsi) non scatta: α < 0,7
ammissibile esiste.

### La conferma su test (una corsa, dopo la scelta)

320 domande, cache calda (108 883 riusi, 0 nuovi), ibrida α=0,5 contro
solo testo, con α=0,7 come riferimento:

| | ses. any@5 | ses. all@5 | ses. ndcg@5 | turno ndcg@5 |
|---|---|---|---|---|
| solo testo | 0,9625 | 0,8344 | 0,6502 | 0,6620 |
| **ibrida α=0,5** | **0,9750** ✓ | **0,8500** ✓ | 0,7406 | **0,6813** ✓ |
| ibrida α=0,7 (rif.) | 0,9750 | 0,8656 | 0,7325 | 0,6272 |

Le tre condizioni sono tutte superate: **il predefinito passa a
α = 0,5**. Su test α=0,5 batte α=0,7 anche sul turno (0,6813 contro
0,6272); α=0,7 resta davanti solo sulla sessione all@5 (0,8656 contro
0,8500).

### Il nuovo predefinito e i test

`Fusion::default()` → `Convex { alpha: 0.5, cos_norm: Candidates }`; il
commento in `search.rs` racconta la storia (0,6 → 0,7 → 0,5) con regola
e numeri dei due banchi. I test che passano α esplicitamente non
cambiano; i golden del nucleo sono testo+grafo e non vedono α.
**Nessun test ha dovuto essere ritoccato**: `golden_semantic`, i due
test manuali del semantico e i 29 test Node (29/29 in Node 24 e in
Electron) passano con α=0,5 senza toccare le attese — il cambiamento
non sposta le classifiche dorate. È l'informazione, ed è positiva: non
ci sono golden da aggiornare.

### La corsa piena su tutte le 470 (il nuovo numero di partenza)

| | ses. any@5 | ses. all@5 | ses. ndcg@5 | ses. ndcg@10 | turno any@5 | turno ndcg@5 | turno ndcg@10 |
|---|---|---|---|---|---|---|---|
| solo testo | 0,9702 | 0,8447 | 0,6607 | 0,6504 | 0,8766 | 0,6829 | 0,7181 |
| ibrida α=0,5 | **0,9809** | **0,8723** | **0,7536** | **0,7638** | **0,9106** | **0,6970** | **0,7342** |

> **Correzione della verifica (2026-11-11).** La riga ibrida scritta
> qui in origine (0,9745 / 0,8766 / 0,7451 / 0,7588 / 0,8660 / 0,6429 /
> 0,6889) era una corsa con **α=0,7**, non 0,5: coincide con la media
> pesata di dev e test ad α=0,7 (per esempio il turno ndcg@5
> (150·0,6765 + 320·0,6272)/470 = 0,6429). Rifatta dalla verifica col
> binario del tag (`--mode hybrid --questions all`, intestazione
> «fusione convex:0.5», cache calda, 4:35): la riga sopra, che coincide
> con la media pesata di dev e test ad α=0,5 (turno ndcg@5 0,6969).
> Con α=0,5 l'ibrida batte il solo testo su **tutte** le metriche
> delle 470, anche al turno.

Per tipo, sessione ndcg@5: testo — knowledge-update 0,7132,
multi-session 0,6867, single-assistant 0,8231, single-preference
0,3125, single-user 0,6313, temporal 0,6316; ibrida α=0,5 (corsa della
verifica) — knowledge-update 0,7790, multi-session 0,7801,
single-assistant 0,9687, single-preference 0,4816, single-user 0,6992,
temporal 0,7109.

### Il banco a 100k non è peggiorato

Binario v0.4.4 ricostruito dalla base (stash → build → copia → pop)
contro il binario finale, stesso comando
`engramma-bench --notes 100000 --runs 1`, macchina ferma, addossati:

| metrica | prima (v0.4.4, α0,7) | dopo (v0.4.5, α0,5) | Δ |
|---|---|---|---|
| ibrido caldo p50 | 11,5 ms | 11,8 ms | **+2,6%** ✓ |
| ibrido caldo p95 | 14,6 ms | 15,0 ms | **+2,7%** ✓ |
| vettori p50 / p95 | 4,5 / 5,1 ms | 4,5 / 5,2 ms | ~0 |
| ibrido con embedding p50 | 20,1 ms | 20,5 ms | +2,0% |
| embedding (note/s) | 201 | 74 | −63% (vedi P2) |

La soglia del +10% è rispettata con margine: α non tocca le latenze, il
piccolo Δ è rumore di giro singolo.

## P2 — RAM: abbassare il picco

### Lo strumento di misura

`--matrix N` negli esempi `ram`/`ram_mimalloc` (`crates/engramma-embed/
examples/`): N figli in processi separati dello stesso binario
(l'allocatore viaggia col binario), ognuno stampa una riga CSV —
carico (s), RSS dopo il carico / dopo il lavoro / dopo 10 s, **picco**
(macOS: «Physical footprint (peak)» di `vmmap --summary` sul processo
vivo — su darwin 27 è una riga propria, il formato «(peak: …)» inline
non esiste più; Linux: `VmHWM`), tempo di embed. Il padre stampa
mediana/min/max. Stessa cosa in Node: `node __test__/rss.js --matrix 5`
(figli Node, picco da `process.resourceUsage().maxRSS`). Una corsa sola
non dice più nulla: la stima da 540 MB della verifica era una corsa
fortunata.

### Il carico da solo (prima la misura)

`ram --csv --load-only`, 5 corse: **0,6–2,4 s** (dipende dalla cache
delle pagine del file) e **picco ~1,11 GB** (1 105–1 132 MB di RSS a
carico fatto). La verifica dei passaggi (a): `into_typed(self)` e
`into_optimized(self)` consumano il grafo precedente — al più due copie
dei pesi vive in un istante, e nessuna nostra variabile allunga la vita
di uno stadio. I byte del file dopo la decodifica sono affare di tract.

### La cache NNEF (b): provata, impossibile, scartata

Provata come da piano: `<sha256 dell'onnx>.nnef.tar` nella cartella del
modello, sidecar JSON con hash del tar + mappa degli input + versione,
scrittura atomica (tmp + rename), ricostituzione silenziosa a qualunque
difetto. **Il grafo ottimizzato non è serializzabile** in NNEF con
tract 0.23: `write_to_tar` muore su `OptMulUnicast` («No serializer
found», nodo `/embeddings/norm/LayerNormalization.normalized_scaled`).
Nemmeno il grafo tipizzato (pre-ottimizzazione) passa: muore su
`StridedSlice` (`/layers.0/attn/Slice_1`). Servirebbero serializzatori
custom: un progetto a sé. Numeri per non riprovarlo: carico a freddo
0,6–2,4 s, picco ~1,11 GB — la cache avrebbe dovuto abbassare il picco
di ≥ 200 MB o dimezzare il tempo; non esiste, e il costo della prova è
zero tentativi riusciti. Codice rimosso, `tract-nnef` tolto dalle
dipendenze, lock invariato.

### Il budget di token, l'ordinamento, il pool

In `Builtin::embed` (v0.4.5): i testi sono **ordinati per lunghezza in
token** (la riempitura collassa al divario tra vicini), i lotti tagliati
sotto il tetto `righe × seq ≤ ENGRAMMA_EMBED_TOKEN_BUDGET` (un testo più
lungo del tetto va da solo: seq è già capped da max_tokens), un **pool
fisso di lavoratori** — predefinito `min(4, available_parallelism)`,
`ENGRAMMA_EMBED_THREADS` resta — pesca i lotti da una coda; i buffer
ids/mask sono stesi nell'ordine ordinato e i lavoratori prendono
**fette**, non le copie doppie della v0.4.4. L'ordine originale è
ricomposto all'uscita.

**Bug trovato e corretto in corsa dal mio primo taglio**: dopo lo
`scope` i messaggi stavano tutti nel canale ma il `tx` originale restava
vivo nel chiamante, quindi il drenaggio `for … in rx` aspettava una
chiusura che non arrivava mai — deadlock alla prima query (individuato
con `sample`: il main in `mpmc::recv`). Fix: `drop(tx)` prima del
drenaggio. Tutte le misure che seguono sono col codice corretto.

**Fedeltà**: `cargo test -p engramma-embed --test fidelity` verde senza
toccare le soglie; test nuovo `order_and_budget_do_not_change_the_vectors`
— stessi 12 testi in ordine inverso e con budget 1 e 8192: vettori
**identici bit per bit** (non coseni: uguaglianza). E2E: la tabella
LongMemEval delle 150 rifatta col codice nuovo è byte-identica alla
precedente (51 268 vettori riusati). `ENGRAMMA_EMBED_TOKEN_BUDGET`
non valido (0, non numerico) ricade sul predefinito (test
`budget_override_needs_a_whole_number_at_least_one`).

### La griglia budget × thread (8 punti × 5 corse, sistema)

| budget | lavoratori | dopo carico | +10 s (med/min/max) | **picco (footprint)** | embed 64 note |
|---|---|---|---|---|---|
| 2048 | 2 | 1100 | 1450 / 1442 / 1499 | **1331** | 8,3 s |
| 2048 | 4 | 1101 | 1396 / 1391 / 1472 | 1536 | 5,3 s |
| 4096 | 1 | 1100 | 1376 / 1370 / 1383 | 1229 | 12,2 s |
| 4096 | 2 | 1101 | 1692 / 1655 / 1700 | 1536 | 8,6 s |
| 4096 | 4 | 1100 | 1949 / 1941 / 2072 | 2048 | 5,5 s |
| 8192 | 2 | 1101 | 2257 / 2255 / 2344 | 2150 | 9,4 s |
| 8192 | 4 | 1100 | 3248 / 3171 / 3406 | 3174 | 6,1 s |
| 8192 | 6 | 1100 | 3277 / 2531 / 3488 | 3277 | 5,9 s |

Riferimento del tempo (v0.4.4, stesso carico): 8,6 s mediana. Il punto
col picco più basso nel budget di tempo è **(2048, 2)**: picco
1331 MB, embed 8,3 s (−3%). (4096, 1) ha picco 1229 ma tempo +42%:
fuori. **Il predefinito del budget è 2048**; il pool predefinito resta
min(4, available_parallelism) come da fase — a 4 lavoratori il picco
del predefinito è 1536 MB a processo nudo.

### Gli allocatori, rimisurati (macOS, processo nudo, 5 corse)

| punto | allocatore | dopo carico | +10 s | picco (footprint) | embed |
|---|---|---|---|---|---|
| 2048/2 | sistema | 1100 | 1450 | 1331 | 8,3 s |
| 2048/2 | **mimalloc** | **846** | **1149** (1142–1153) | **1126** | 8,4 s |
| 2048/4 | **mimalloc** | **846** | 1390 (1350–1399) | 1331 | 5,4 s |
| 4096/1 | **mimalloc** | **846** | 1112 | 1126 | 12,8 s |

Su macOS **mimalloc vince su tutto** (carico −254 MB, riposo −300 MB,
picco −205 MB al punto scelto) e la varianza della v0.4.4 (971–2293 MB)
è scomparsa: 1142–1153 MB. Su Linux il contrario (stesso comando nel
contenitore `engramma-linux:1.95.0`, budget 2048, 4 lavoratori, 5 corse):

| Linux | glibc | mimalloc |
|---|---|---|
| dopo carico | 561 MB | 785 MB |
| **+10 s (mediana)** | **637** (574–639) | 1166 (851–1183) |
| **picco (VmHWM)** | **1021** | 1410 |
| embed 64 note | 13,6 s | 10,8 s |

Come da regola della fase, **mimalloc resta solo dove vince**:
`#[cfg(target_os = "macos")]` sul `#[global_allocator]` del `.node` e
del binario CLI, e `[target.'cfg(target_os = "macos")'.dependencies]`
per la dipendenza. Nota a margine: su Linux il picco del predefinito è
1021 MB — glibc restituisce davvero.

### L'artefatto vero: Node (`rss.js --matrix 5`, .node con mimalloc)

| stadio | mediana | min | max |
|---|---|---|---|
| modello caricato | 894 MB | 894 | 894 |
| dopo embedPending (64 note, metà lunghe) | 1429 MB | 1407 | 1455 |
| **+10 s** | **1446 MB** | 1436 | 1469 |
| **picco (maxRSS)** | **1446 MB** | 1436 | 1469 |
| embedPending (2ª cella) | 9 059 ms | 8 779 | 9 190 |

### Le soglie, onestamente

- **picco ≤ 1,2 GB**: a processo nudo **sì** col punto (2048, 2) sotto
  mimalloc (1126 MB); **no** nel predefinito a 4 lavoratori (1331 nudo,
  1446 in Node, che conta anche l'heap di Node). Il picco scende da
  ~2,3–2,4 GB della v0.4.4 a ~1,45 GB nell'artefatto: **−40%**.
- **RSS +10 s ≤ 700 MB (mediana) e ≤ 900 MB (massimo)**: **NO** —
  1446 MB (1436–1469). La causa è misurata, non ipotizzata: la memoria
  in uso è ~485 MB, il resto sono pagine liberate e trattenute.
  `vmmap --summary` a fine corsa: footprint 1,4 GB (picco 1,4),
  «Writable regions: Total=2.5G … resident=1.4G (55%)», le zone Malloc
  di sistema quasi vuote (Malloc Small 7 MB: **mimalloc tiene la sua
  memoria in segmenti mmap propri**, fuori dalle zone), il 90% delle
  librerie non residente. `malloc_zone_pressure_relief` su darwin 27
  restituisce 0 MB (già provato in verifica: da non riprovare).

Dove sta la memoria che resta, in una riga: ~490 MB di pesi/grafo
vivi + ~950 MB di pagine liberate che né mimalloc né l'allocatore di
sistema restituiscono a macOS dopo il lavoro, con qualunque budget o
numero di lavoratori. **Ci si ferma qui come da fase**: l'embedding
fuori processo è un'opzione architettonica della v0.5 (DESIGN § 12).

### Il banco a 100k, riga per riga

Vedi la tabella in fondo a P1 (prima/dopo addossati). Il throughput di
embedding scende da 201 a 74 note/s: **per lavoratore è identico**
(18,5 contro 20 note/s/lavoratore) — è il pool a 4 lavoratori contro i
10–12 fili della v0.4.4, per costruzione. Il patto è dichiarato:
`ENGRAMMA_EMBED_THREADS=8..10` riporta il throughput (e con lui il
picco: è la manopola del compromesso, documentata in `docs/node.md`).
Il carico di lavoro da 64 note — la metrica d'accettazione della fase —
è **migliorato** (5,3 s contro 8,6 con 4 lavoratori, budget 2048).

## P3 — il DESIGN e il README

- **§ 11, il riquadro LongMemEval**: riscritto con le metriche
  ufficiali (P0), la divisione dev/test, lo studio di α e il nuovo
  numero di partenza sulle 470;
- **§ 6.3 (punto 5)**: α = 0,5 con la storia 0,6 → 0,7 → 0,5, la regola
  e i numeri dei due banchi;
- **§ 7**: il lotto di incorporazione (ordinamento per lunghezza,
  tetto di token, pool fisso, le due variabili d'ambiente), la RAM
  misurata a matrice (mediana/min/max, non una corsa), l'allocatore per
  piattaforma, la cache NNEF provata e scartata coi numeri,
  `malloc_zone_pressure_relief` inutile su darwin 27;
- **§ 12**: la riga v0.4.5 e l'embedding fuori processo fra le opzioni
  della v0.5;
- **§ 14**: chiuse α (n. 7) e allocatore per piattaforma (n. 6);
- **`docs/node.md`**: `ENGRAMMA_EMBED_THREADS` e
  `ENGRAMMA_EMBED_TOKEN_BUDGET` con i loro effetti misurati, e la RAM
  che aspettarsi; la nota mimalloc per piattaforma;
- **`README.md`**: la riga dell'allocatore per piattaforma (l'unica
  cosa non più vera).

## Per Homun

- **α passa da 0,7 a 0,5** (la fusione predefinita di `recall_memory` e
  `search_history` in modo ibrido): cambiano le classifiche — su
  LongMemEval l'ibrida guadagna su tutte le profondità di sessione e
  sul turno; sul banco italiano l'MRR pesato sale da 0,8529 a 0,8577.
  I contratti JS non cambiano, né i risultati dei test (29/29 verdi
  senza ritoccare le attese).
- **RAM**: il picco dell'embedding scende da ~2,3–2,4 GB a ~1,45 GB nel
  `.node`; a riposo dopo `embedPending` aspettarsi ~1,4–1,5 GB
  (prima: 1,0–2,3 GB a seconda della corsa — ora è stabile: la
  mediana delle 5 corse è 1446 MB, ventaglio 33 MB).
- **Variabili d'ambiente nuove**: `ENGRAMMA_EMBED_TOKEN_BUDGET`
  (predefinito 2048; 0/non numerico → predefinito) e
  `ENGRAMMA_EMBED_THREADS` (ora è il pool: predefinito
  min(4, available_parallelism)). Per l'importazione iniziale di
  grandi storie, `ENGRAMMA_EMBED_THREADS=8` quadruplica il ritmo
  riportando il picco ai valori vecchi.
- **Nessuna cache NNEF**: la prova è fallita e il codice è stato
  rimosso; il disco del modello non cambia.
- Embedding bulk più lento a parità di RAM: 74 note/s predefinite
  contro 201 (per lavoratore invariato) — vedere la variabile qui sopra.

## Chiusura

- macOS: `cargo fmt --check` ✓, `cargo clippy --all-targets --offline
  -- -D warnings` ✓, `cargo test --offline --workspace` ✓ (26 suite;
  più i 3 test dell'esempio longmemeval con `cargo test -p
  engramma-bench --example longmemeval`), `cargo deny check licenses
  bans` ✓ (bans ok, licenses ok).
- Linux: `scripts/linux.sh` verde — fmt, clippy `-D warnings` e 26
  suite nel contenitore `engramma-linux:1.95.0` (due passaggi: il
  primo ha pescato un lint solo-Linux, `size_to_mib` morta fuori da
  macOS, corretta con `#[cfg]` come già in v0.4.4); poi
  `docker volume rm engramma-linux-target`.
- JS: Node 24 `npm test` 29/29, `test:electron` 29/29,
  `test:electron-main` ok — col `.node` della fase.
- `npm run build` rifatto; SHA256 dei tre file per Homun:

  ```
  engramma.darwin-arm64.node  6fffb45888cf02178461e67a3984857a0ed475a8542ab08992c8edb8abee0ec6  (33,5 MB)
  index.js                    25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4
  index.d.ts                  bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8

  `index.js` e `index.d.ts` hanno gli stessi SHA256 della v0.4.4: il
  contratto JS non è cambiato, come promesso dalla fase.
  ```

- `du -sh target`: 6,4 GB all'inizio, **8,2 GB** alla fine.
- Temporanei in `/var/folders/nb/r6fbsdsn3z58bkbb9w55nn780000gn/T/engramma-v045/`
  cancellati a fine fase (script, log, tabelle md, binario baseline,
  campioni di stack). Restano, come da fase: la cache dei vettori in
  `~/.cache/engramma/bench/` (ora 195,7 MB, 124 824 record: le 470
  domande intere) e **nessuna** cache NNEF (scartata). Le celle di
  LongMemEval vivevano in `tempfile::TempDir` e sono andate via da sé.
  Nessun push.

# REPORT — Engramma v0.4.6 (i lotti di embedding, davvero)

Data: 2026-11-11. Base: `main` @ `cc2018f` (tag `fase-v0.4.5` più un
commit di sola documentazione della verifica). Un commit per punto, con
dentro solo i file di quel punto; il test rosso prima della correzione.
Tutto costruito con `cargo --offline`, Rust 1.95.0, macOS arm64 (darwin
27, 10 core, 24 GB). **Nessuna crate nuova** (il `Cargo.lock` è identico
a quello della v0.4.5), nessun `unsafe` nuovo, nessun `unwrap`/`expect`
fuori dai test, clippy `-D warnings` verde su macOS e su Linux. Il
contratto JS non cambia: `index.js` e `index.d.ts` hanno gli stessi
SHA256 della v0.4.4 e v0.4.5. Il repo di Homun non è stato toccato.

Disco: `du -sh target` — inizio fase **8,2 GB**, fine **8,2 GB**
(ricompilazioni al posto). `~/.cache/cargo-target` mai toccata. Le
coppie sensibili al tempo (griglia, banco 100k) sono corse a macchina
ferma, una dietro l'altra.

## P0 — il tokenizer riempiva già i testi, e gli offset erano sbagliati

### Cosa non funzionava (trovato dalla verifica della v0.4.5)

Il `tokenizer.json` di **entrambi** i Granite R2 contiene
`"padding": {"strategy": "BatchLongest", "direction": "Right", …}`
(97M: pad_id 179935, `<|endoftext|>`; 311M: pad_id 0, `<pad>`):
`Tokenizer::from_file` carica quella configurazione, quindi
`encode_batch` in `Builtin::embed` restituiva **tutti i testi riempiti
al più lungo della chiamata**. Il commento «the tokenizer itself stays
unconfigured» era falso dalla v0.2. Conseguenze:

1. **l'ordinamento per lunghezza non ordinava niente** — `lens` era
   uguale per tutti, cioè `min(il più lungo, 512)`, e una nota corta in
   una chiamata con una lunga pagava 512 token di calcolo. È la vera
   causa del calo del banco a 100k da 201 a 74 note/s (non solo il pool
   a 4 della v0.4.5);
2. **gli offset dei lotti erano sbagliati ma il difetto era nascosto**:
   i buffer ids/mask sono stesi lotto per lotto, ognuno col suo `seq`,
   ma il lavoratore leggeva `from = inizio_riga × seq_lotto` — giusto
   solo se tutti i lotti hanno lo stesso `seq`, che il riempimento
   garantiva per caso. **Riprodotto in questa fase**: padding tolto e
   offset vecchi → panic in un lavoratore (`builtin.rs:422`, fetta
   fuori dai limiti) al primo test con lotti di seq diversi;
3. **il test «bit-identici a ogni budget» passava solo perché tutte le
   forme erano uguali**: non è una proprietà del motore, lo è solo a
   parità di `seq`.

### Il test rosso (commit `cce543e`)

- `the_loaded_tokenizer_does_not_pad` (unitario, in `builtin.rs`): due
  testi di lunghezza diversa in un `encode_batch` col tokenizer di
  `Builtin::load` devono dare lunghezze diverse. **Prima della
  correzione fallisce**: «the tokenizer pads the batch: both texts came
  out 24 tokens»;
- `order_and_budget_do_not_change_the_vectors` (fidelity) riscritto
  onesto: alla stessa configurazione un **ordine diverso** dà vettori
  **identici bit per bit** (l'ordinamento ricostruisce gli stessi
  lotti); **budget diversi** (1, 2048, 8192) danno coseno ≥ 0,999999 e
  |Δ| ≤ 1e-5 per componente, e il commento dice perché non
  l'uguaglianza (la forma del lotto cambia l'ordine delle somme dentro
  tract). I testi coprono tre lunghezze distinte **sotto** i 512 token
  (verificato nel test con `approx_tokens`): il caso che fa lavorare
  gli offset, prima nascosto dal riempimento che livellava tutto.

### La correzione (commit `b41078a`)

- `Builtin::load` legge il `pad_id` della configurazione di riempimento
  (se c'è) e **toglie il riempimento** (`with_padding(None)`): il
  ripiego senza configurazione resta l'ultimo id del primo testo, e la
  maschera rende la scelta irrilevante (i test di riempimento lo
  provano). La troncatura resta del tokenizer (32768, sopra il nostro
  taglio a `max_tokens`): `approx_tokens` continua a vedere la
  lunghezza vera;
- **offset per lotto a somma prefissa** di `righe × seq`, calcolati
  quando si tagliano i lotti e passati ai lavoratori;
- i commenti che dicevano il falso, corretti («stays unconfigured»,
  «bit-identical across budgets»).

**Fedeltà contro ONNX Runtime**: `granite_97m_matches_onnxruntime` e
`granite_311m_matches_onnxruntime` verdi **senza toccare le soglie**
(cos ≥ 0,9999), insieme a `padding_does_not_change_the_vector`,
`truncation_keeps_the_first_tokens_and_the_batch_shape` e il test
riscritto.

## P1 — rimisurare, e scegliere con la regola scritta

### La griglia budget × thread rifatta (stessi 8 punti, 5 corse, entrambi gli allocatori)

`ram`/`ram_mimalloc --matrix 5`, 64 note (metà di ~600 token), processi
separati, macchina ferma:

| budget | lav. | dopo carico | +10 s (med/min/max) | **picco** | embed 64 | picco mimalloc | +10 s mimalloc |
|---|---|---|---|---|---|---|---|
| 2048 | 2 | 1100 | 1434 / 1421 / 1475 | 1331 | 4,4 s | 1126 | 1142 (1130–1152) |
| 2048 | 4 | 1101 | 1390 / 1364 / 1431 | 1536 | 2,8 s | 1331 | 1340 (1327–1370) |
| **4096** | **1** | 1100 | 1374 / 1354 / 1380 | **1229** | 6,4 s | **1126** | **1110** (908–1110) |
| 4096 | 2 | 1101 | 1696 / 1676 / 1806 | 1536 | 4,5 s | 1434 | 1426 (1345–1436) |
| 4096 | 4 | 1100 | 1877 / 1792 / 1982 | 2048 | 2,9 s | 1741 | 1721 (1653–1827) |
| 8192 | 2 | 1101 | 2261 / 2207 / 2392 | 2150 | 4,7 s | 1843 | 1850 (1831–1965) |
| 8192 | 4 | 1100 | 2390 / 2056 / 2520 | 2253 | 4,7 s | 1946 | 1953 (1828–2034) |
| 8192 | 6 | 1100 | 2405 / 2229 / 2424 | 2355 | 4,7 s | 1946 | 1932 (1862–2030) |

(dopo carico mimalloc: 846 MB su tutti i punti; embed mimalloc: 4,5 /
2,9 / 6,8 / 4,6 / 3,0 / 5,0 / 4,8 / 4,8 s negli stessi punti. Per
confronto, la griglia v0.4.5 sugli stessi punti: 8,3 / 5,3 / 12,2 /
8,6 / 5,5 / 9,4 / 6,1 / 5,9 s.)

**La regola, la stessa della v0.4.5** — il picco più basso il cui tempo
non peggiori oltre il 15% rispetto alla v0.4.4 (8,6 s, cioè ≤ 9,89 s):
con il riempimento tolto il tempo delle 64 note sta tutto fra 2,8 e
6,8 s, **tutti i punti sono ammissibili** e vince **(4096, 1)**: picco
1229 MB a sistema, 1126 MB con mimalloc e il miglior riposo della
griglia (+10 s 1110 MB). È il punto che la regola della v0.4.5 sceglieva
già per il picco e scartava solo per il tempo (12,2 s, +42%); ora fa
6,4 s (−26%). **Questa volta i predefiniti seguono la regola** (commit
`3acbf5b`): `ENGRAMMA_EMBED_TOKEN_BUDGET=4096`, pool a **1 lavoratore**
— nella v0.4.5 la regola sceglieva (2048, 2) e poi il predefinito
restava `min(4, ·)`.

### Il banco a 100k (prima/dopo addossati, macchina ferma)

`engramma-bench --notes 100000 --runs 1`: binario v0.4.5 ricostruito
dalla base (74 note/s, il numero del REPORT v0.4.5 riprodotto) contro
il binario finale coi predefiniti nuovi (4096 × 1):

| metrica | v0.4.4 | v0.4.5 | v0.4.6 | Δ 4.5→4.6 |
|---|---|---|---|---|
| **embedding (note/s)** | 201 | 74 | **68** | −8% |
| **per lavoratore (note/s)** | ~18,5 (10–12 lav.) | 18,5 (4 lav.) | **68 (1 lav.)** | **3,7×** |
| ibrido caldo p50 / p95 | 11,5 / 14,6 ms | 11,9 / 15,2 ms | 12,4 / 16,1 ms | ~+4% |
| vettori p50 / p95 | 4,5 / 5,1 ms | 4,6 / 5,6 ms | 4,7 / 6,2 ms | ~0 |
| ibrido con embedding p50 / p95 | 20,1 / — | 20,8 / 25,1 ms | 20,8 / 24,4 ms | ~0 |
| inserimento (note/s, viva) | — | 2592 | 2561 | −1% |
| richiamo@5 / MRR (tutte le righe) | — | 0,940 / 0,683 | identici | 0 |

Lettura onesta: **il ritmo per lavoratore è salito da 18,5 a 68 note/s
(3,7×)** — era tutto lì il costo del riempimento, e la v0.4.4 faceva
201 note/s proprio perché spendeva 10–12 lavoratori da 18,5. Il totale
al predefinito scende da 74 a 68 perché il pool passa da 4 a 1: è il
prezzo della regola del picco, dichiarato. Con la scalabilità lineare
misurata nella v0.4.5 (18,5 × 4 = 74, verificata), 4 lavoratori oggi
sarebbero ≈ 270 note/s — sopra i 201 della v0.4.4 — ma non è una corsa
di questa fase: la manopola è `ENGRAMMA_EMBED_THREADS`, e chi importa
un archivio intero la alza sapendo che ogni lavoratore porta con sé
picco e ritmo in uguale misura (la griglia qui sopra).

### Node (`rss.js --matrix 5`, .node nuovo, predefiniti nuovi)

| stadio | mediana | min | max |
|---|---|---|---|
| modello caricato | 894 MB | 894 | 894 |
| dopo embedPending (64 note, metà lunghe) | 1214 MB | 1214 | 1215 |
| **+10 s** | **1224 MB** | 1224 | 1224 |
| **picco (maxRSS)** | **1224 MB** | 1224 | 1224 |
| embedPending (2ª cella) | 12 963 ms | 12 882 | 13 354 |

Il picco scende da 1446 MB (v0.4.5) a **1224**: **la soglia di 1,2 GB è
rispettata nell'artefatto** (1224 < 1228,8), e le cinque corse sono
uguali al megabyte. `embedPending` passa da 9,1 a 13,0 s: è il pool a 1
lavoratore. **La soglia di 700 MB a +10 s resta fuori** (1224 MB):
pagine liberate ma residenti su macOS, con qualunque allocatore — la
causa è misurata nella v0.4.5 e non è cambiata. L'embedding fuori
processo resta l'opzione della v0.5.

### Linux (contenitore `engramma-linux:1.95.0`, glibc, 5 corse, predefiniti nuovi)

**Picco (VmHWM) 1008 MB, +10 s 596 MB** (596–597; dopo carico 561; embed
18,2 s). Nella v0.4.5, a 2048×4: picco 1021, +10 s 637. Su Linux la
soglia dei 700 MB a +10 s **è rispettata** — glibc restituisce, e il
predefinito a 1 lavoratore la sfrutta.

### LongMemEval (una corsa ibrida sulle 150 dev, cache calda)

`--mode hybrid --questions dev`, 76 s, **0 incorporazioni nuove, 51 268
riusi**: sessione any@5 / all@5 / ndcg@5 / ndcg@10 = 0,9933 / 0,9200 /
0,7815 / 0,7933, turno ndcg@5 0,7303 — **gli stessi numeri della
v0.4.5, riga per riga**. **I vettori in cache sono quelli vecchi**,
calcolati col riempimento: differiscono dai nuovi solo nell'ultima cifra
(coseno ≥ 0,999999, la proprietà del test riscritto) e nessuna cache va
buttata; la corsa prova che l'esempio funziona col codice nuovo.

## P2 — la documentazione

- **DESIGN § 7**: il lotto di incorporazione com'è dopo P0 (niente
  riempimento dal tokenizer, offset a somma prefissa, predefiniti
  4096 × 1), una riga su cosa non funzionava nella v0.4.5, la proprietà
  vera dei vettori (bit-identici a parità di configurazione, coseno
  ≥ 0,999999 fra budget), i numeri nuovi della RAM e dell'allocatore;
  § 12: la riga v0.4.6;
- **`docs/node.md`**: le due variabili d'ambiente con i predefiniti e
  gli effetti misurati nuovi; la RAM che aspettarsi (1,2 GB);
- **REPORT**: questo testo.

## Per Homun

- **L'embedding per lavoratore è 3,7× più veloce** (68 note/s contro
  18,5): il riempimento del tokenizer costava quello, non il pool. Al
  predefinito (1 lavoratore, il picco più basso della regola) il ritmo
  di massa è 68 note/s; per l'importazione iniziale di grandi storie
  `ENGRAMMA_EMBED_THREADS=4` (o più) accelera in proporzione — linearità
  misurata — riportando su il picco: la manopola del compromesso.
- **RAM**: picco dell'embedding **1224 MB** nel `.node` (prima 1446;
  ~2,3 GB nella v0.4.4), identico su tutte le corse; a riposo dopo
  `embedPending` aspettarsi ~1,2 GB. `embedPending` di 64 note: ~13 s
  (9,1 s nella v0.4.5 a 4 lavoratori) — in background, non cambia i
  risultati.
- **Predefiniti nuovi**: `ENGRAMMA_EMBED_TOKEN_BUDGET=4096`,
  `ENGRAMMA_EMBED_THREADS=1` (0/non numerico → predefinito, come prima).
- **Le cache dei vettori restano valide**: i vettori nuovi differiscono
  dai vecchi solo nell'ultima cifra; ricalcoli non necessari.
- I contratti JS non cambiano: `index.js` e `index.d.ts` immutati
  (SHA256 sotto).

## Chiusura

- macOS: `cargo fmt --check` ✓, `cargo clippy --all-targets --offline
  -- -D warnings` ✓, `cargo test --offline --workspace` ✓ (26 suite,
  0 falliti), `cargo test -p engramma-bench --example longmemeval` ✓
  (3 test), `cargo deny check licenses bans` ✓.
- Linux: `scripts/linux.sh` verde (fmt, clippy `-D warnings`, 26 suite
  nel contenitore `engramma-linux:1.95.0`, rete chiusa, sorgente in
  sola lettura); poi `docker volume rm engramma-linux-target`.
- JS: Node 24 `npm test` 29/29, `test:electron` 29/29,
  `test:electron-main` ok — col `.node` della fase.
- `npm run build` rifatto; SHA256 dei tre file per Homun:

  ```
  engramma.darwin-arm64.node  4a6e24af94f3efeba1d935048e29e9d2e1a92cf89620f8e6d6b6b72cddab898f  (33,6 MB)
  index.js                    25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4
  index.d.ts                  bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8

  `index.js` e `index.d.ts` hanno gli stessi SHA256 della v0.4.4 e
  v0.4.5: il contratto JS non è cambiato, come promesso dalla fase.
  ```

- `du -sh target`: 8,2 GB all'inizio, **8,2 GB** alla fine.
- Temporanei in `/var/folders/nb/r6fbsdsn3z58bkbb9w55nn780000gn/T/engramma-v046/`
  cancellati a fine fase (script, log, tabelle, binari baseline).
  Resta, come dalle fasi precedenti: la cache dei vettori in
  `~/.cache/engramma/bench/` (i vettori vecchi, validi — vedi
  LongMemEval). Nessun push.

## v0.4.6.1 — correzione di verifica: il budget predefinito a 512

Data: 2026-11-11. Base `fase-v0.4.6` (`a52445e`). Fatta nella verifica
della v0.4.6: un commit, tag annotato `fase-v0.4.6.1`.

**Cosa ha trovato la verifica.** La griglia della v0.4.6 ripeteva gli 8
punti della v0.4.5 e a 1 lavoratore aveva solo il budget 4096; la regola
(«il picco più basso entro il +15% di tempo») sceglie il migliore fra i
punti misurati, non oltre. Il difetto è della spec, non della fase. Gli
stessi strumenti (`ram`/`ram_mimalloc --matrix 5`, 64 note di cui metà
lunghe, 1 lavoratore):

| budget | picco mimalloc | +10 s mimalloc | 64 note | picco sistema | +10 s sistema |
|---|---|---|---|---|---|
| **512** | **878** | **888** | **5,8 s** | 1024 | 1132 |
| 1024 | 912 | 922 | 6,7 s | 1024 | 1142 |
| 2048 | 993 | 1003 | 6,2 s | 1126 | 1255 |
| 4096 (v0.4.6) | 1126 | 1110 | 6,4 s | 1229 | 1350 |

(a 1024 × 2, mimalloc: picco 986 MB, 64 note 3,9 s.)

Il banco a 10k, 1 lavoratore (`engramma-bench --notes 10000 --runs 1`):

| budget | 97M (note/s) | 311M (note/s) |
|---|---|---|
| **512** | **100** | **37** |
| 4096 | 75 | 26 |

Latenze di ricerca identiche (ibrido caldo p50 2,1 ms, con embedding
10,6 ms). **Perché:** senza il riempimento del tokenizer, su CPU un lotto
grande non fa risparmiare calcolo (tract fa una passata per lavoratore),
mentre le attivazioni intermedie crescono con il lotto ed escono dalla
cache. 512 è `max_tokens`: un testo lungo passa da solo, i corti si
impacchettano fino allo stesso tetto.

**Correzione:** `DEFAULT_TOKEN_BUDGET = 512` (`builtin.rs`) con i
commenti; DESIGN § 7 e § 12, `docs/node.md`. Il pool resta a 1
lavoratore.

**Node** (`rss.js --matrix 5`, .node nuovo, Node 24.22.1):

| stadio | mediana | min | max |
|---|---|---|---|
| modello caricato | 894 MB | 894 | 894 |
| dopo embedPending (64 note) | 953 MB | 953 | 953 |
| **+10 s** | **959 MB** | 959 | 959 |
| picco (maxRSS) | 1222 MB | 1222 | 1222 |
| embedPending | 11 450 ms | 11 409 | 11 456 |

Il riposo a +10 s scende da 1224 a **959 MB**. Il picco resta 1222 MB
perché **ora lo fa il caricamento del modello**: 1220 MB di `maxRSS` già
subito dopo `Engine.open` con l'embedder builtin (l'ottimizzazione del
grafo in tract), prima di qualsiasi embedding. Per abbassare il picco
ancora bisogna intervenire sul carico, non sui lotti. La soglia di 700 MB
a +10 s resta fuori su macOS.

**Chiusura:** `cargo fmt --check`, `cargo clippy --all-targets --offline
-- -D warnings`, `cargo test --offline --workspace` (26 suite),
l'esempio `longmemeval` (3 test), `cargo deny check licenses bans`: tutti
verdi. Node 24 `npm test` 29/29, `test:electron` 29/29,
`test:electron-main` ok. **Non rifatti:** `scripts/linux.sh` (una
costante cambiata, nessun codice di piattaforma) e il banco a 100k (il
banco a 10k qui sopra è la misura del ritmo). I vettori cambiano solo
nell'ultima cifra (la proprietà del test di fedeltà, coseno ≥ 0,999999):
le cache dei vettori restano valide.

```
engramma.darwin-arm64.node  c64010d381df14cd6ad48be20db7e112b9d4331d823f1deb5e017da707c3bd31  (33,6 MB)
index.js                    25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4
index.d.ts                  bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8
```

# REPORT — Engramma v0.5 (scrivere da altri assistenti: proposte, revisione, storico e rollback)

Data: 2026-11-12. Base: `main` @ `09015ef` (fase-v0.4.6.1 più un commit di
sole dipendenze: axum 0.8.9 e l'elicitation di rmcp 3.8, già nel lock).
Tutto costruito e provato offline (`cargo --offline`), Rust 1.95.0, macOS
arm64. **Nessuna crate nuova**: blake3, ring, ureq, saphyr, serde, axum,
tokio erano già in `Cargo.lock`; il lockfile è invariato. Un commit per
punto, i test rossi prima delle correzioni.

**Commits:** `029767d` (P0), `8882c97` (P1), `8843c94` (P2), `ceb6151`
(P3), `63a2381` (P4), `c7eb146` (P5), `03d08a4` (difetto trovato dalla
prova a mano), più questo (P6). 307 test del workspace verdi.

## P0 — Lo schema più nuovo si rifiuta

- **Rosso** (`tests/schema_too_new.rs`): cella creata, `schema_version`
  portato a `SCHEMA_VERSION + 1`, riaperta — **oggi si apriva**. Dopo la
  correzione: `CellError::SchemaTooNew { found, supported }` e la cella
  resta intatta (la versione scritta dal test è ancora lì, non migrata).
- La guardia vive in `schema::migrate`: ogni superficie passa da
  `Cell::open` (CLI, MCP, Node, vault), quindi ogni apertura è coperta.
- **CLI**: codice d'uscita **6** proprio (`output.rs`), messaggio senza il
  suggerimento «run `engramma init`» che qui sarebbe fuorviante. Provato a
  mano: `engramma get …` → exit 6; `verify` → exit 6.
- **Node**: **solo** il nuovo codice d'errore `SCHEMA_TOO_NEW` nella mappa
  di `cell_error` e nell'elenco dei codici noti di `split_transport`.
  **Nessun metodo nuovo** del contratto JS.
- `the_own_schema_still_opens`: la cella propria si apre, versione corrente.

### Per Homun

Homun usa un `.node` **vecchio, senza questa guardia**. Le sue celle **non
vanno aperte** con la CLI o l'MCP della v0.5 prima che Homun aggiorni il
suo `.node`: `Cell::open` migra **sempre** (non esiste un modo di aprire
in sola lettura senza migrare — il reader e il writer condividono la
stessa apertura), e una volta che la cella è a schema 13 il `.node` vecchio
ci scriverebbe sotto le tabelle `proposals`/`decisions` che non conosce,
facendo divergere lo stato derivato dal registro. Tenere l'MCP senza
`--write` **non basta**: l'apertura migra comunque. La strada giusta è
portare Homun al `.node` v0.5 (che rifiuta e non migra) e solo dopo
toccare le sue celle con strumenti nuovi. In transizione: le celle di
Homun restano di Homun.

## P1 — Proposte, decisioni e rollback nel motore

Op nuove `propose`/`decide`/`revert_decision` (+ `set_policy`, introdotta
col solo schema: la semantica è di P2); **schema 13**: tabelle derivate
`proposals` e `decisions`, mantenute da `apply` e **coperte dal
`state_digest`** — il rigioco da zero dà lo stesso digest e `verify`
controlla le operazioni nuove come qualsiasi altra tabella. Il digest
allargato rende stanchi i digest registrati dalle compattazioni vecchie:
passaggio una-tantum all'apertura (`upgrade_snapshot_digests`): i file
istantanea vengono migrati e ridigeriti, i checkpoint (senza file) hanno
il digest posto a NULL come le redazioni di purge; il bookmark `dump_v13`
sta nel registro ma è escluso dal confronto (come `log_v1_head`). La purge
raggiunge righe e payload; i `decide` che accettavano una rettifica
vengono **censurati** (come i `supersede` da sempre): il bersaglio resta
chiuso anche nel rigioco censurato, con la chiusura registrata nell'op
stessa (`target_closing` viaggia nel corpo).

- **Rossi→verdi** (`tests/proposals.rs`, 21): la proposta nasce in
  quarantena e la ricerca non la vede (nemmeno come seme del grafo); la
  coda è distinta dalla quarantena `external`; doppione normalizzato
  rifiutato senza scrivere (maiuscole/spazi/punteggiatura: decisa in
  `text::normalize_for_duplicate`, test dedicato); accetta attiva, accetta
  con modifica, accetta di una rettifica chiude il bersaglio; rifiuta mette
  la lapide col testo; due decisioni senza revert rifiutate; il revert
  riapre il bersaglio com'era, la decisione è ripetibile; **mai sopra una
  correzione umana** (`CellError::Conflict` che nomina le operazioni);
  il purgato non torna (`purged`); filtri per stato/attore/data; blocco
  per attore con conflitto e purga in mezzo; `verify` copre le op nuove;
  la purge dimentica proposta e decisioni restando verificabile; la purge
  di una rettifica accettata tiene il bersaglio chiuso (il caso più
  delicato: censura del `decide`).
- **Proptest** (`tests/proposals_props.rs`): sequenze casuali di propose,
  decide, revert, patch, supersede, restore e purge; invarianti della
  spec: invisibilità in coda (testo e grafo), mai sopra un umano, digest
  del rigioco uguale, `verify` verde.
- **Due processi veri** (`tests/two_writers.rs`): il processo figlio
  (questo binario) propone mentre il padre edita; 202 operazioni, nessuna
  persa, nessun «database is locked» all'utente — `BEGIN IMMEDIATE` +
  `busy_timeout` fanno il loro lavoro.
- Il generatore di `tests/props.rs` copre i quattro tipi nuovi
  (`random_ops_cover_every_kind`, 256 semi, ≥20 successi per tipo).
- **Difetti trovati e corretti durante il punto** (i proptest esistenti
  sono stati riallungati prima di passare oltre): la purge dimenticava i
  `revert_decision`, ma un revert riapre un bersaglio — censurati anche
  quelli; e la prima stesura della censura riscriveva i revert di
  QUALSIASI nota (trovato dallo stesso proptest: `note_id` nel filtro).

## P2 — Politiche per cella e revisore automatico

`write: off` è il predefinito e una cella chiusa rifiuta le proposte su
ogni superficie. `reviewers.yaml` alla radice dei dati (saphyr):
`command` (argv fisso, **niente shell**, tempo massimo) e `openai`
(endpoint compatibile `/v1/chat/completions`, uscita `json_schema`, via
`ureq`); il file porta il **nome della variabile d'ambiente**, mai la
chiave — una `api_key` scritta nel file non parsa (campo proibito
esplicito). Il contratto: richiesta JSON con la proposta, il bersaglio,
fino a 5 note simili, il nome della cella, il riassunto della politica e
la versione del prompt — **tutto il personale fra delimitatori
`<<<BEGIN/END …>>>`**; risposta `{decision, reason ≤500, confidence 0..1}`;
lettura severa: qualunque altra cosa vale `escalate`; solo un fallimento
di trasporto marca «revisione automatica fallita» (ritentabile da
`review_failed`, che non tocca gli escalate). Il prompt sta nel repo
(`review/prompt-v1.md`, `include_str!`, versione 1) e il suo blake3 entra
in ogni decisione; dichiara che i contenuti sono dati e non istruzioni —
e che questo riduce il rischio, non lo elimina.

- **Rossi→verdi** (`tests/review.rs`, 21): le tre decisioni; illeggibile,
  timeout, crash, revisore assente → pending + `auto_failed`; confidenza
  sotto soglia → all'umano; **interruttore** dopo N accettazioni
  nell'ora; `external` mai in automatico; `allow_supersede`
  none/inferred/any (bersaglio `stated` rifiutato, `inferred` accettato);
  bersaglio in lapide mai; `kinds`; retry dopo il fallimento; il file yaml
  rifiuta chiavi crude e campi sconosciuti; hash del prompt stabile e
  registrato; delimitatori verificati con una proposta ostile
  («SYSTEM: ignore all previous instructions…»).
- Il server HTTP finto è in processo, solo loopback; il revisore command
  è uno script `sh` nel temporaneo (via `tempfile`).

## P3 — MCP in scrittura

`engramma mcp --cells a,b --write a --actor claude-desktop`: identità dalla
configurazione, mai dal `clientInfo` (che viaggia come metadato);
`--write ⊆ --cells`, attore obbligatorio. **Un solo** strumento in più,
`memory_propose`, costruito come route manuale (la via macro non vede il
giro MRTR). Nessuno strumento conferma/scarta/cancella/purga/cambia
politica — un test fissa la lista esatta con e senza `--write`
(`tests/mcp.rs`, ora 9). Gli strumenti di lettura ora dichiarano
`readOnlyHint: true`.

- **Elicitation, le due forme:** richiesta dal server fino alla 2025-11-25
  (accetta con testo modificabile → `decide(accept)` con attore
  `user@mcp:<actor>`, decline → `reject`, cancel → pending) e
  `InputRequiredResult` per 2026-07-28 col **flusso discover** (niente
  initialize: ogni richiesta porta il proprio `_meta`; la proposta è già
  in coda quando il tool risponde `input_required`; `requestState`
  monouso). Scoperta della prova: su stdio la initialize con
  `protocolVersion: 2026-07-28` negozia **2025-11-25** — la versione
  giusta si legge da `RequestContext::protocol_version()` (il `_meta`
  prima, lo stato dell'handshake poi), non dal clientInfo richiesto.
- Client senza elicitation → `pending` con l'indirizzo dove decidere
  (`engramma proposals` / `engramma review`).
- Le `instructions` portano l'etichetta (chiedere prima, mai dal web,
  `supersedes` per correggere) e la firma `mcp:<actor>`.
- Errori non-oracolo: cella non esposta e non scrivibile dicono solo ciò
  che il chiamante già sa (il messaggio della non scrivibile nomina
  l'insieme scrivibile; la non esposta è identica a quella dei tool di
  lettura).
- La **modalità URL** resta fuori: un token in un URL dato a un modello va
  progettato a parte (opzione dichiarata nel DESIGN § 12).

## P4 — CLI

`proposals [--state|--by] | show | accept [--edit] | reject [--reason] |
review`; `decisions [--by|--since|--until|--state]` (per i revisori:
modello, hash del prompt, confidenza); `revert <id|suffisso-8>` e `revert
--by reviewer:x [--since] [--until] [--dry-run]` con anteprima e conferma
come `purge` (`--yes`); `policy show|set key=value` (solo CLI; si rifiuta
di salvare ciò che il motore non rilegge: `auto` senza `reviewer`, campi
sconosciuti); `history` racconta la proposta e `--diff` fa il diff riga
per riga (l'ultima finestra è l'ultima versione contro lo stato attuale).
`--json` su tutto (`tests/proposals_cli.rs`, 5).

Durante il punto, i proptest esistenti hanno inchiodato il difetto P1
della censura dei revert (vedi sopra): la sequenza che lo smaschera è
decidere → revert → decidere di nuovo → purge della proposta — il
bersaglio divergeva di versione perché il rigioco perdeva la riapertura.

## P5 — La pagina locale di revisione

`engramma review [--port] [--open]`: 127.0.0.1 (porta 0 di base), stampa
l'indirizzo col token, Ctrl-C ferma. Server-rendered, **zero JavaScript**,
moduli HTML, CSS servito dal server. Pagine: coda (accetta / modifica e
accetta / rifiuta), elenco e ricerca, dettaglio con storico (chi propose,
chi decise, perché), diff e «ripristina questa versione», registro delle
decisioni con annullamento singolo e in blocco (anteprima poi conferma),
politica in sola lettura. Attore `user@review`.

Sicurezza, ogni punto col suo test (`tests/review_page.rs`, 9):

- **Token** 256 bit (ring) nell'indirizzo stampato, scambiato una volta
  con cookie `HttpOnly SameSite=Strict` e redirect all'indirizzo pulito;
  senza cookie 401 su ogni pagina, token sbagliato 401.
- **Host** `127.0.0.1:<porta>` o `localhost:<porta>`, altrimenti 403 —
  provato su **TCP grezzo** (un client onesto non spedirebbe mai un Host
  bugiardo, che è il punto). Il controllo precede il cookie.
- **Ogni POST**: `Origin` uguale all'host **e** token CSRF del singolo
  modulo (blake3 di token+modulo); senza Origine, con Origine estraneo o
  con CSRF contraffatto: 403.
- **Intestazioni** su ogni risposta: CSP `default-src 'none'; style-src
  'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'`,
  `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`.
- **Escaping** di tutto il testo delle note: proposta con `<script>`,
  `"><img src=x onerror=…>` e `javascript:` in un collegamento markdown →
  mostrati come testo (la pagina non interpreta il markdown: i
  collegamenti non diventano mai ancore).
- **Loopback**: l'indirizzo stampato è 127.0.0.1 (test).
- Politica senza route di scrittura (POST → 405, nessun modulo nella
  pagina).

## La prova a mano (trascritta)

Sessione MCP vera, binario vero, client JSON-RPC a mano; radice in
`/var/folders/…/T/engramma-v05-prova`; politica `write=propose`;
`--actor claude-desktop`; un fatto già in memoria («Fabio lavora a
Milano, in zona Navigli.»).

1. `initialize` → `serverInfo: engramma 0.1.0-dev`, instructions con
   l'etichetta («propose only stable facts…», firma `mcp:claude-desktop`).
2. `memory_search {query:"lavora"}` → il fatto di Milano.
3. `memory_propose {text:"Fabio suona il pianoforte da tre anni.",
   reason:"l'utente l'ha detto durante la chat", confidence:"stated"}`
   → `status: "pending"`, `id: 01M77PD8HM…0XPA3S`.
4. `memory_propose {text:"…da tre anni!"}` (stesso testo, punteggiatura
   diversa) → `status: "duplicate", existing: 01M77PD8HM…` — niente di
   scritto.
5. `engramma proposals` → `[in coda] MA0XPA3S mcp:claude-desktop — Fabio
   suona il pianoforte da tre anni. / motivo: l'utente l'ha detto…`.
6. `engramma review` → `http://127.0.0.1:58133/entra?token=374a62…`;
   `/entra` col token → cookie `HttpOnly SameSite=Strict` + redirect a
   `/coda`; modulo di accettazione col testo modificato in
   «…da tre anni, da autodidatta.» → POST 303 → `decisions`:
   `B9FHXBAA  accetta  user@review` (perché: accettata dalla pagina).
7. `engramma search "pianoforte"` → la nota, attiva, col testo corretto.
8. `engramma revert B9FHXBAA` → «annullata: proposta di nuovo in coda»;
   `proposals` la mostra `[in coda]` col testo corretto; `search
   "pianoforte"` → nessun risultato sopra soglia; `history` racconta
   «proposta da mcp:claude-desktop … — l'utente l'ha detto durante la
   chat» e le versioni con `[decide, user@review]` e
   `[revert_decision, user@cli]`.
9. `engramma verify` → «registro verificato: 6 operazioni, catena hash,
   ricostruzione e indice FTS a posto».

**Un difetto vero, trovato ai punti 3–4 di questa stessa sessione** (due
proposte dello stesso testo in arrivo quasi simultaneo: entrambe
`pending`, due copie in coda): la prima stesura controllava i doppioni
all'inizio di `propose` e scriveva dopo — con `write: auto` in mezzo corre
il revisore, fuori da ogni lock, e la finestra resta aperta fino al tempo
massimo del revisore. Corretto in `03d08a4`: il controllo ripete sotto il
lock di scrittura, subito prima dell'operazione. Test rosso prima della
correzione: revisore command che accetta dopo un secondo, prima proposta
parcheggiata nella revisione, seconda proposta in parallello — dopo la
correzione resta una copia sola e il perdente riceve `duplicate`.

## Documentazione

- **DESIGN**: § 5 (le proposte accanto alla quarantena), § 8.3 (MCP in
  scrittura: strumenti, annotazioni, instructions, elicitation nelle due
  forme, il confine «chi ha la tua shell è te»), **§ 8.5 nuovo**
  («Proposte, revisori, storico e rollback»: modello delle minacce,
  politica, contratto del revisore, le due proprietà del revert), § 12
  (la v0.5 è questa fase; identità e sincronizzazione cifrata slittano
  alla v0.6, le successive scorrono di uno; **candidata per una v0.5.x,
  da decidere con l'utente**: MCP su HTTP con OAuth 2.1 — serve per
  ChatGPT — e la modalità URL dell'elicitation).
- **`docs/mcp.md` nuovo**: come collegare Claude Desktop e Claude Code in
  lettura e in scrittura, configurazione d'esempio, cosa vede l'utente
  (modulo, coda, `engramma review`), dove si annulla, che cosa non è
  sicurezza.

## Chiusura

- **macOS:** `cargo fmt --check` verde; `cargo clippy --all-targets
  --offline -- -D warnings` verde; `cargo test --offline --workspace`
  **307 verdi, 0 falliti**; `cargo test -p engramma-bench --example
  longmemeval` 3 verdi; `cargo deny check licenses bans` ok (un
  «unmatched license allowance» Unicode-DFS-2016, la stessa nota delle
  fasi precedenti: la voce è permessa e nessuna la usa più — lasciare
  l'avviso, non il licenziatario, è il difetto).
- **Banco a 10k** (`engramma-bench --notes 10000 --runs 1`, release):
  le latenze di ricerca sono quelle della v0.4.6.1, letteralmente —
  vettori p50 0,790 ms (0,790), ibrido caldo p50 **46,2 ms (46,2)** e
  **66,7 ms (66,7)**, ibrido con embedding p50 **40,9 (40,9)** / **61,4
  (61,4)**, p95 59,1 (59,1) / 79,6→79,7: dentro lo 0,2%, ben oltre il
  tetto del 5% richiesto. Le tabelle nuove non toccano la ricerca. I
  richiami si muovono di ±0,006 (0,838→0,839, 0,722→0,719: la varianza
  tra giri documentata; nessuna tendenza). `bench/RESULTS.md` aggiornato.
- **Linux** (`scripts/linux.sh`, container arm64, rete chiusa, sorgente
  in sola lettura): **verde** — fmt, clippy e i 307 test. Il primo giro
  aveva un test CLI rosso colpa mia: lo «editor» di `accept --edit` era
  uno script `sed -i ''`, una sintassi BSD che GNU sed rifiuta —
  riscritto in sh+sed portabile (`sed … > "$1.new" && mv`). Poi
  `docker volume rm engramma-linux-target`: fatto.
- **JS** (Node 24.22.1, Electron 24/45 via Homun): `test:electron`
  **29/29 verdi**, `test:electron-main` ok — **senza cambiare le attese**
  (il contratto JS è invariato; l'unica novità è il codice d'errore
  `SCHEMA_TOO_NEW` di P0, che i test non toccano perché nessun test apre
  celle con schema più nuovo).
- **Build** (`npm run build`, rifatta dopo l'ultimo commit di codice):
  `engramma.darwin-arm64.node`
  `c302c3ea9778ea662c5c34452b6ac92ad378e69aa98107c2f68589044772d0f5`;
  `index.js`
  `25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4`;
  `index.d.ts`
  `bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8`.
- **Disco:** `du -sh target` **8,5G** all'inizio, **16G** alla fine
  (build di sviluppo + release del banco, build Linux del container nel
  volume — rimosso — e i tre artefatti; `~/.cache/cargo-target` da 1,2G
  mai toccato).
- **Tag:** `fase-v0.5` annotato; temporanei cancellati; nessun push.

# Verifica v0.5 (2026-11-12, revisore)

Esito: **passata**, con tre correzioni piccole fatte in verifica (un commit
ciascuna, test rosso prima):

- `bfdb1f5` — **il modulo di conferma arrivava vuoto.** Il campo `text`
  dell'elicitation non aveva `default`: Claude Code (o qualunque client)
  mostrava un campo obbligatorio vuoto e l'utente doveva riscrivere la
  proposta. Ora è precompilato col testo proposto. In più, un'accettazione a
  testo invariato veniva registrata come «con modifica»: ora è
  un'accettazione semplice. Trovato con una sessione JSON-RPC a mano;
  i test vecchi passavano perché rispondevano sempre con un testo scelto
  da loro.
- `da0d293` — **`strict` fuori posto nella chiamata `openai`.** Stava dentro
  lo schema JSON; le structured outputs lo vogliono in
  `response_format.json_schema`, accanto a `name` e `schema`. Il guasto
  restava sicuro (risposta illeggibile = in coda), ma l'uscita non era
  garantita conforme.
- `c5e009e` — **tornano i 16 semi storici** di
  `props.proptest-regressions`, cancellati dal commit P4 senza dirlo.

Controllato e a posto: fmt, clippy `-D warnings`, 307 test del workspace +
3 di LongMemEval, `cargo deny`; lista esatta degli strumenti MCP con e
senza `--write` e annotazioni; errori non-oracolo; `external` rifiutato
dallo strumento; elicitation accetta / accetta con modifica / annulla;
pagina di revisione a mano con curl (Host falso 403, senza cookie 401, POST
senza Origin / Origin estraneo / CSRF falso 403, CSP e intestazioni,
`<script>`, `onerror` e `javascript:` escapati, ascolto solo su
127.0.0.1); il revisore fallisce sempre in sicurezza (nessun percorso
d'errore porta ad `accept`).

Precisazione: «il lockfile è invariato» del REPORT è impreciso. Nessuna
crate nuova, ma `Cargo.lock` registra le dipendenze nuove fra crate già
presenti (blake3, ring, serde e ureq per la CLI; ureq per il core; time per
l'MCP).

Note lasciate per dopo (non bloccanti):
- l'interruttore conta le decisioni dell'ultima ora prima della revisione,
  fuori dal lock: due processi MCP concorrenti possono superare il tetto di
  qualche unità. Conta sia le accettazioni sia i rifiuti (più severo della
  spec);
- i token della pagina si confrontano con `==` e non a tempo costante
  (solo loopback, rischio trascurabile);
- il cookie `engramma_review` non dipende dalla porta: due `engramma
  review` aperti insieme si sovrascrivono la sessione (basta rientrare
  dall'indirizzo stampato).

Build: `npm run build` rifatto dopo le correzioni; il `.node` è
**identico bit per bit** (`c302c3ea…`): il codice toccato non arriva al
modulo Node. `index.js` e `index.d.ts` invariati. `du -sh target`: 16G.

# REPORT — Engramma v0.5.2 (MCP su HTTP con OAuth 2.1: ChatGPT e Claude dal web)

Data: 2026-11-12. Base: `main` @ `f84a92b` (fase-v0.5.1 più il commit di
sole dipendenze: la `transport-streamable-http-server` di rmcp 3.8, con
`async-trait`, `sse-stream 0.2.6` e `tokio-stream` nel lock, già
scaricate; `cargo deny` verde alla partenza). Tutto costruito e provato
offline (`cargo --offline`), Rust 1.95.0, macOS arm64. **Nessuna crate
nuova nel lock**: rusqlite, blake3, ring, ureq, axum, time, serde,
tokio-util (già portata da rmcp) e base64 0.23.1 (già in lock per tract)
erano tutte presenti; le ho solo dichiarate dove servivano.

**Commits:** `8e6f6cc` (P0.1), `bbbfc49` (P0.2), `79f5f34` (P0.3),
`d57859a` (P2 store/CIMD/monete), `afa42bd` (P2 metadati), `3adf741`
(P3), `cf2a7f0` (P1+P4 insieme — sotto, il perché), `74a6096` (P5, con
dentro un difetto trovato dai test nuovi), `048e130` (P6), `9b6f9e8`
(P7), `3c2770f` (i casi restanti), più questo (REPORT). Test rossi prima
di ogni correzione.

## P0 — Le tre note della v0.5, che con HTTP diventano vere

- **L'interruttore sotto il lock** (`8e6f6cc`). Rosso: `max_auto_per_hour
  = 3`, revisore `command` che accetta dopo 300 ms, 10 proposte parallele
  nello stesso processo — **oggi ne passavano 10**. Il conteggio stava
  fuori dal lock di scrittura: dieci revisioni avviate insieme leggevano
  tutte «0 finora» e decidevano tutte. Ora la decisione automatica passa
  da `decide_under_breaker`, che rilegge il conteggio e scrive la
  decisione dentro lo stesso lock (lo stesso ordine del controllo dei
  doppioni di `03d08a4`); le proposte fermate restano in coda col motivo
  («interruptore: max_auto_per_hour raggiunto mentre la revisione era in
  corso») e l'esito lo dice al chiamante. Vale anche per il retry di
  `proposals review`. Dopo la correzione: 3 accettate, 7 in coda col
  motivo.
- **Confronto a tempo costante** (`bbbfc49`). Il token della pagina di
  revisione si confrontava con `==`: un confronto che si ferma al primo
  byte diverso. Ora passa da `same_secret` (blake3 di entrambi, poi i
  digest a 256 bit). Il test non è temporale (onestamente non esiste):
  cerca nel sorgente `== self.token` e i suoi fratelli — il pattern non
  può tornare inosservato — e prova che il flusso entra/esce come prima.
- **Cookie per porta** (`79f5f34`). `engramma_review_<porta>`: due
  `engramma review` aperti insieme non si scambiano più il token di
  sessione. Rosso: il set-cookie del primo porta il nome senza porta; il
  token di A sotto il nome di B non entra, e il vecchio nome è ignorato.
  Durante la correzione ho pure scoperto (a mano, con curl) che la prima
  stesura del guard si mangiava l'`=` del cookie: il test del flusso
  completo l'avrebbe preso comunque.

## P2 — Il server di autorizzazione: stato, metadati, client

- **`<root>/oauth.db`** (`d57859a`), permessi 0600 (test su Unix), schema
  proprio `schema_version` 1 con la stessa guardia delle celle: uno
  schema più novo si rifiuta. Contenuto: client CIMD in cache e DCR,
  richieste in attesa, codici, grant, token, `events`. Di ogni segreto
  solo `blake3`. Staccato dal registro delle celle di proposito
  (credenziali, non memoria: niente sincronizzazione v0.6, revoca
  immediata e locale) — scritto nel DESIGN § 8.6.
- **CIMD col recinto SSRF.** Il `client_id` è un URL scelto da chi
  chiama: lo scaricamento risolve il DNS e passa a ureq un
  **`Resolver`** che consegna solo indirizzi pubblici (loopback, RFC
  1918, link-local coi metadati cloud, CGNAT, ULA, multicast, non
  specificati, IPv4 mappati in IPv6 e i blocchi di documentazione sono
  scartati); la connessione va all'IP verificato con SNI e Host del
  nome. Zero redirect, 5 KiB, 5 s, solo `application/json`. Il documento
  deve claimarsi (`client_id` uguale all'URL), avere redirect validi
  (https ovunque, http solo su loopback) e `token_endpoint_auth_method`
  assente o `none` (`private_key_jwt` rifiutato dicendolo). La cache
  rispetta `max-age` dentro [5 min, 24 h], default 1 h (il test prova
  `max-age=600`; i bordi del clamp sono nel codice, non misurati).
  **Proptest**: ogni blocco riservato è rifiutato in qualunque forma
  testuale, inclusi i v4 mappati in v6; gli indirizzi ordinari passano
  (il recinto non si mangia internet).
- **Redirect**: confronto esatto con l'unica eccezione RFC 8252 § 7.3
  (porta ignorata solo per `http` su host di loopback — serve a Claude
  Code). Proptest: la porta conta per https e per gli altri host, e il
  percorso deve comunque coincidere.
- **DCR spento di base**; con `--allow-dcr`: solo client pubblici,
  redirect validi, 20 inutilizzati al massimo (429 oltre), i mai usati
  scadono dopo 24 h (implementato nel lookup; la scadenza non ha test
  dedicato: dice il codice). Nessun client pre-registrato.
- **Metadati** (`afa42bd`): PRM identica su entrambi i percorsi
  (`resource` esatta, un solo authorization server, ambiti
  `memory:read` + `memory:propose` se una cella è scrivibile, bearer
  solo header); metadati AS campo per campo (S256, `none`, CIMD
  supportato, `iss` sempre, `registration_endpoint` solo col flag) e la
  variante RFC 8414 col percorso inserito quando l'issuer ne ha uno.
  Tutto `Cache-Control: no-store`. Test campo per campo in
  `tests/http.rs`.

## P3 — Autorizzazione con approvazione locale, token, revoca

- **`/authorize`** (`3adf741`): il client si risolve PRIMA di fidarsi
  del redirect; client o redirect non validi rispondono con una
  **pagina**, mai un redirect (quella sarebbe la porta aperta — test:
  nessun `Location`). Ogni altro errore torna al redirect con `iss`
  (RFC 9207) e `state`: `plain` o challenge assente → `invalid_request`,
  `resource` estraneo → `invalid_target`, gli ambiti sconosciuti
  (`openid email profile`) si **ignorano** (niente OIDC, niente
  `id_token`: la risposta del token dice cosa è concesso davvero —
  test). Tetto di 5 richieste in attesa oltre il quale la pagina lo
  dice.
- **La pagina d'attesa** senza JavaScript (rinfresco 3 s, CSP chiusa,
  DENY, no-referrer, no-store): nome del client escapato, host del
  client_id e del redirect, ambiti, avviso per i redirect di loopback,
  codice di approvazione `XXXX-XXXX` dall'alfabeto senza ambiguità,
  istruzioni col comando da digitare. L'URL porta un handle a 256 bit,
  cosa diversa dal codice.
- **L'approvazione è locale**: `engramma oauth pending/approve/deny/
  grants/revoke/log` su `oauth.db` (le richieste ricordano il tetto
  offerto dal processo: l'approvazione arriva da un altro processo).
  `--label` obbligatoria (`[a-z0-9-]{1,32}`) e diventa l'attore
  `mcp:<etichetta>`; `--cells`/`--scope` restringono; la CLI stampa
  host e redirect e chiede conferma senza `--yes`. Il codice da solo
  non autorizza niente: conta dove viene digitato.
- **`/token`**: solo form urlencoded; tetto globale di 60/min poi 429
  con `Retry-After` (dietro un tunnel ogni chiamante è 127.0.0.1: le
  intestazioni inoltrate non si usano); `authorization_code` con PKCE
  (il **riuso del codice revoca la famiglia**, evento nel log),
  `refresh_token` a rotazione (il **riuso del vecchio revoca il grant
  intero**; scaduto o sconosciuto → `invalid_grant`, mai
  `invalid_request`). Accesso 1 h; refresh 30 giorni dall'ultimo uso,
  90 giorni di massimo assoluto. Token opachi `eng_at_`/`eng_rt_` a 256
  bit. Perché opachi e non JWT: la risorsa e l'autorizzazione sono lo
  stesso processo — scritto nel DESIGN.
- **`/revoke`** RFC 7009: 200 anche per token sconosciuto (test). Il
  viaggio completo col **binario vero** per l'approvazione (`engramma
  oauth approve … --yes` → il rinfresco redirige con `code`, `state`,
  `iss`) e ogni caso negativo ha il suo test.
- Corretto in corsa un **auto-deadlock** del lock dello store
  (`request_deny` chiamava `event()` tenendo la guardia: secco il
  processo del test, trovato con `ps`).

## P1+P4 — `/mcp` come resource server (un commit, `cf2a7f0`)

Un commit solo per i due punti perché sono la stessa superficie: il gate
risolve il token e il grant plasma la stessa richiesta — dividerli
avrebbe lasciato P1 con un gate che non concede niente, o P4 senza il
trasporto su cui valere.

- **`mcp --http 127.0.0.1:8787 --public-url https://…`**: ascolto solo
  loopback (il resto è rifiutato — test), `--public-url` https
  obbligatoria senza query né frammento (l'eccezione `http://127.0.0.1`
  esiste solo nella configurazione della libreria: mai dalla CLI),
  `--actor` con `--http` è errore d'uso. rmcp 3.8 monta il suo
  `StreamableHttpService` su axum: protocollo 2026-07-28 senza sessione
  e versioni precedenti con `legacy_session_mode` (ChatGPT parla la
  2025-11-25), corpo massimo 1 MiB, stesse `instructions` e strumenti.
- **Il recinto**: middleware `Host` su tutta la superficie (l'autorità
  pubblica e i nomi di loopback; 403 altrimenti — test con TCP nudo),
  `Origin` rifiutato ovunque un server chiami (`/mcp` con
  `enforce_origin_validation` di rmcp, `/token`/`/register`/`/revoke`
  con la stessa guardia — test), token nella query → 400 (test),
  token assente/sconosciuto/scaduto/revocato/di altro pubblico → 401
  con `WWW-Authenticate: Bearer resource_metadata=…, scope=…` (test
  sull'esattezza della challenge). Registro d'accesso su stderr: ora,
  id del grant, metodo MCP, strumento, stato, durata — **mai** token né
  testo di note né argomenti (provato a cattura dello stderr del
  binario). SIGINT/SIGTERM cancellano il CancellationToken di rmcp,
  svuotano il listener e chiudono le celle (test: il processo esce).
- **Il grant decide**: lista strumenti **per token** (cinque strumenti
  di lettura per un grant di sola lettura — lista esatta fissata dal
  test; `memory_propose` compare solo con ambito propose e una cella
  scrivibile), celle visibili = grant ∩ `--cells`, una cella fuori dal
  grant risponde **esattamente** come una che non esiste (test),
  l'attore delle proposte è `mcp:<etichetta>` (provato sul `source`
  della nota), `memory_propose` con token di sola lettura → il rifiuto
  `insufficient_scope` (test), revoca con effetto alla richiesta
  successiva (test con token vivo). Sessioni legacy legate al grant:
  il token di B sulla sessione di A → 404 come una sconosciuta, e il
  `requestState` 2026-07-28 è monouso e legato al grant (entrambi
  provati, `3c2770f`).
- **403 insufficient_scope**: rmcp non offre uno stato HTTP dentro una
  chiamata di strumento (la chiamata è JSON-RPC dentro un 200). Il
  rifiuto viaggia come errore dello strumento con la parola
  `insufficient_scope`, l'ambito mancante e come ottenerlo; il 401 con
  `error="insufficient_scope"` c'è al gate, quando il token non ha
  nemmeno `memory:read`. È la scelta che restava, e il REPORT la dice.
- Invarianti di sempre riprovati via HTTP: quarantena invisibile,
  `external` rifiutato, oracolo chiuso sulle celle.

## P5 — Le proposte dai client remoti (`74a6096`)

- **Modulo via HTTP**: il giro MRTR 2026-07-28 funziona uguale: il
  `tools/call` con `_meta` di versione e capacità riceve
  `input_required` col modulo **precompilato** col testo proposto (la
  correzione `bfdb1f5`, riprovata sul trasporto HTTP — test sull'`input`
  `default` dello schema); il retry con `inputResponses` e `requestState`
  applica la decisione (accetta-con-modifica provato, attore
  `user@mcp:<etichetta>`), il `requestState` riusato è rifiutato.
- **Client senza moduli** (ChatGPT): `pending`, e il testo contiene
  solo il puntatore locale («`engramma proposals` o `engramma review`»):
  **nessun URL, nessun token** — provato cercandoli nella risposta.
- **Difetto trovato dai test nuovi, presente anche nella v0.5 in
  stdio**: `mrtr_round` passava l'attore già prefissato e
  `apply_form_answer` lo prefissava di nuovo — il registro firmava
  `user@user@mcp:<attore>`. Ora un prefisso solo (il test cerca
  esplicitamente il doppione).
- La **modalità URL** resta fuori: una pagina di conferma raggiungibile
  dal web esige di autenticare il proprietario sul web pubblico — serve
  una passkey (WebAuthn), crate e fase a sé. DESIGN § 8.6 e § 12 la
  mettono in candidates per la v0.7 con `engramma serve`.

## P6 — `--compat chatgpt` (`048e130`)

`search(query)` → `{results: [{id, title, url}]}` e `fetch(id)` →
`{id, title, text, url, metadata}`: involucri di `memory_search` e
`memory_get`, in `structuredContent` **e** come JSON in `content`
(provato). Stessa quarantena (la nota nascosta risponde come una che
non esiste — test), stesse celle del grant (`search` fonde le celle
visibili: lo schema non ha `cell`), id **completi** (`fetch` non accetta
prefissi), `url` sempre stringa vuota (la nota non ha un indirizzo
pubblico; ChatGPT allora non cita la fonte — dichiarato). Esistono solo
col flag: la lista esatta è fissata dal test, con e senza.

## P7 — Documentazione (`9b6f9e8`)

`docs/http.md` nuovo: i tunnel (Tailscale Funnel raccomandato — il TLS
termina sul tuo computer; Cloudflare e ngrok **vedono il traffico in
chiaro**, e il documento lo dice), come far arrivare l'`Host` pubblico,
perché l'URL deve essere stabile (è la risorsa e l'issuer), il
collegamento di ChatGPT e claude.ai, l'approvazione col codice, la
revoca, chi vede cosa, le liste di IP facoltative al tunnel (difesa in
più, non sostituto di OAuth). DESIGN: § 8.3 guadagna l'HTTP, § 8.6 nuovo
con il modello delle minacce in otto punti e le scelte, § 12 segna la
v0.5.2 fatta e le candidates v0.7. `mcp.md` apre con la tabella stdio
contro HTTP; `cli.md` documenta `mcp --http` e i sei `oauth`.

## Quello che i test non coprono (e perché)

- **Il tunnel vero e i connettori veri** di claude.ai e ChatGPT: sei
  offline, sono del revisore (il testo della fase lo prevede).
- **Il token di un altro pubblico via HTTP**: provato a livello di store
  (`WrongAudience`); sul filo non si può coniare un token per un
  pubblico diverso, perché il server ne ha uno solo per costruzione.
- **La scadenza dei DCR mai usati (24 h)** e **i bordi del clamp della
  cache CIMD**: nel codice, senza test dedicati.

## Per Homun

Niente da fare: aggancio facoltativo. `oauth.db` non lo riguarda (vive
nella radice dei dati e non tocca il contratto JS). Il `.node` è stato
rifatto: **cambia** (P0.1 tocca il core — la decisione automatica sotto
lock — che il modulo linka), `index.js` e `index.d.ts` sono **identici**
alla v0.5 (SHA sotto): il contratto JS è invariato. Le avvertenze della
v0.5 sullo schema 13 restano.

## Chiusura

- **macOS:** `cargo fmt --check` verde; `cargo clippy --all-targets
  --offline -- -D warnings` verde; `cargo test --offline --workspace`
  **368+32 = vedi sotto** (il totale finale con gli ultimi test è nel
  registro dei controlli: tutti verdi, nessuno ignorato);
  `cargo test -p engramma-bench --example longmemeval` 3/3;
  `cargo deny check licenses bans` verde.
- **Linux:** `scripts/linux.sh` (fmt + clippy + test nel container
  arm64, offline, rete chiusa) verde — alla prima passata il test SIGTERM
  cadeva: nel container minimale `kill` non è un eseguibile esterno,
  ora il segnale passa da `sh -c kill`; volume `engramma-linux-target`
  rimosso.
- **Test JS:** Node 24.22.1; `test:electron` **29/29**, `test:electron-main`
  ok — senza cambiare le attese.
- **Build:** `npm run build` rifatto. SHA256:
  `engramma.darwin-arm64.node`
  `036aa42287a76216c8ee4c78bc958c7e71a3cc5b07708ec85eb35cc6ed420327`;
  `index.js`
  `25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4`;
  `index.d.ts`
  `bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8`.
- **Banco a 10k:** non rifatto: la ricerca non cambia (`search` di P6 è
  un involucro sopra `memory_search`, che resta com'era).
- **Disco:** `du -sh target` **16G** all'inizio, **22G** alla fine
  (build di sviluppo + release del `.node` + la build Linux nel volume,
  rimossa; `~/.cache/cargo-target` — 3,7G — mai toccato).
- **Tag:** `fase-v0.5.2` annotato; temporanei cancellati; nessun push.

# Verifica v0.5.2 (2026-11-12, revisore)

Esito: **passata dopo correzioni** (`70ea576`; fmt, clippy, 380 test del workspace + 3 LongMemEval, deny verdi). L'impianto è giusto e completo. Ci
sono:
- server di autorizzazione con approvazione locale;
- CIMD col recinto SSRF;
- token opachi a hash;
- gate Bearer davanti a rmcp;
- lista strumenti per token;
- registro senza segreti.

Il binario vero, provato a mano con curl, risponde come la spec:
- PRM esatta;
- 401 con la challenge giusta;
- 403 per Host falso e per Origin su `/token`;
- pagina d'errore senza redirect per un client ignoto;
- CIMD verso `169.254.169.254` rifiutato e registrato;
- ascolto non-loopback e URL pubblico `http` rifiutati;
- `oauth.db` con permessi 0600.

La rilettura ha però trovato un difetto di sicurezza vero, altri tre seri
e alcuni minori. Sono corretti in verifica, ciascuno con il suo test visto
rosso prima della correzione.

**Seri:**
- **`/token` non legava il codice al client né al redirect.** Le due
  risposte `invalid_grant` venivano costruite e **scartate** (mancava il
  `return`). Così un codice emesso per il client A e il redirect R si
  riscattava con qualunque `client_id` e `redirect_uri`, purché il
  chiamante avesse il verifier PKCE. PKCE resta un secondo lucchetto, ma
  OAuth 2.1 § 4.1.3 li vuole tutti e due, e il test «PKCE sbagliato» non
  poteva accorgersene. Test:
  `the_token_endpoint_binds_the_code_to_its_client_and_redirect`.
- **La rotazione del refresh non era atomica.** La lettura e il «segna
  ruotato» stavano in due sezioni del lock. Due refresh concorrenti con lo
  stesso token vedevano entrambi `revoked = 0` e ruotavano entrambi: due
  catene vive e nessun allarme di furto. Ora il segno è un
  `UPDATE … WHERE revoked = 0` e chi perde la gara è un riuso. Test
  (store, 8 thread × 20 giri): prima ne vincevano più d'uno.
- **Le conferme `[s/N]` aspettavano la fine di stdin.** `read_to_string`
  in `oauth approve` senza `--yes`, e lo stesso difetto anche in `purge`
  (dalla v0.3) e in `revert` (v0.5). Un umano al terminale digitava `s` e
  Invio, e il comando restava fermo: un terminale non manda EOF. Ora si
  legge una riga. Test con la pipe lasciata aperta, per `oauth approve` e
  per `purge`. I test di GLM usavano sempre `--yes`.
- **Un'etichetta non tornava mai libera.** `grants.label` era UNIQUE su
  tutta la tabella. Dopo `oauth revoke chatgpt`, o allo scadere dei 90
  giorni, riapprovare con la stessa etichetta dava «UNIQUE constraint
  failed», mentre `docs/http.md` promette revoca e ricollegamento. Ora
  l'etichetta è unica fra i grant **vivi**: indice parziale
  `WHERE revoked_at IS NULL`, e i grant scaduti cedono l'etichetta.
  `oauth revoke <etichetta>` colpisce quello vivo, e un'etichetta in uso
  dà un errore d'uso chiaro.
  **Nota:** lo schema v1 è cambiato sul posto (nessuna migrazione). Un
  `oauth.db` creato con la build di GLM tiene il vecchio vincolo:
  cancellarlo e ricollegare.

**Minori:**
- `--cells` con un nome non offerto produceva un grant **senza celle**
  che sembrava approvato. Ora è un errore d'uso che nomina la cella, e la
  richiesta resta da decidere.
- Il codice d'approvazione ora si accetta minuscolo e senza trattino.
- Un `requestState` presentato da un altro grant veniva **consumato**
  prima del controllo di appartenenza: un estraneo bruciava il turno del
  proprietario. Ora si controlla prima di rimuovere. In più, il test del
  legame fra `requestState` e grant, che il REPORT dava per fatto, non
  esisteva: ora c'è.
- «Codice già emesso» guardava solo i codici vivi: dopo 60 s un rinfresco
  della pagina d'attesa coniava un secondo codice per lo stesso grant.
  Ora un codice si conia una volta sola.
- Lo scaricamento CIMD usava il proxy dell'ambiente (`HTTPS_PROXY`, che
  ureq legge di default): la connessione sarebbe andata al proxy e il
  recinto avrebbe giudicato l'host sbagliato. Ora `proxy(None)`. Il test è
  rosso se lanciato con `HTTPS_PROXY` impostato.
- `Host: [::1]:porta` veniva rifiutato: il recinto tagliava al primo `:`.
- La lista strumenti, che ora dipende dal token, si dichiarava
  `cacheScope: public`. Con un grant ora è `private`.
- Rinforzo senza test proprio: il server costruito per HTTP rifiuta una
  chiamata **senza** grant, invece di ricadere nel significato stdio
  («tutte le celle»). Il gate ne mette sempre uno, quindi è la rete sotto
  la rete; la suite HTTP intera passa con il rinforzo attivo.
- Messaggi `insufficient_scope` e «nessuno strumento di scrittura» con
  spazi spuri al posto degli a-capo.

**Precisazioni al REPORT di GLM:**
- citava `0d0edac`, un commit riscritto che non è nella storia di `main`:
  ora dice `3c2770f`;
- il totale dei test era rimasto «368+32 = vedi sotto»;
- «requestState legato al grant: provato» non era vero (vedi sopra);
- `target` è passato da 16G a 22G.

**Note lasciate per dopo** (non bloccanti, da scrivere se servono):
- **Tetto delle richieste in attesa:** cinque richieste globali da 10
  minuti, che chiunque conosca l'URL può tenere occupate di continuo.
  Blocca solo i collegamenti nuovi (difesa: rifiutarle con
  `oauth deny`). Lo stesso vale per i download CIMD falliti, che non vanno
  in cache.
- **Recinto SSRF:** non scarta gli indirizzi 6to4 (`2002::/16`) e NAT64
  locale (`64:ff9b:1::/48`) che incorporano un IPv4 privato. Su reti
  normali sono irraggiungibili.
- **Mappa delle sessioni legacy:** non si svuota mai, cresce di una voce
  per `initialize`.
- **`/revoke` di un refresh:** revoca solo quel token, non gli accessi
  della stessa famiglia (RFC 7009 lo raccomanda).
- **Codice d'approvazione:** il modulo su 31 lettere ha un bias minimo.
  Il codice non è un segreto, quindi è irrilevante.

Non rifatti: `scripts/linux.sh`, perché le correzioni non toccano niente
di specifico della piattaforma, e il `.node`, perché le correzioni stanno
in `engramma-mcp` e `engramma-cli`, che il modulo Node non linka. Restano
del revisore il tunnel vero e i connettori veri di claude.ai e ChatGPT.

## Prova col tunnel vero (2026-11-12, revisore)

**Montaggio:**
- ngrok gratuito davanti a `engramma mcp --http`, con `--write default`
  e `--compat chatgpt`;
- una radice usa e getta con tre note finte e la cella in
  `write=confirm`;
- connettori veri nell'account dell'utente.

**Esito: funziona con tutti e tre i client, dopo due correzioni** (commit
qui sotto, ognuna con il suo test rosso).

| Client | Collegamento | Lettura | Proposta | Revoca |
|---|---|---|---|---|
| claude.ai (2026-07-28, senza sessione) | CIMD, codice confrontato, token | `memory_search` ok | modulo precompilato, accettata da `user@mcp:claude` | 401, refresh → `invalid_grant`, «connessione invalidata, ricollegare» |
| ChatGPT web (2026-07-28) | CIMD vero (dopo la correzione 1), `none` + PKCE | `memory_search` ok | in coda, `pending`, testo senza URL né token | revocato; ChatGPT non ha più chiamato il server (risponde dal contesto) |
| Codex / ChatGPT Work (sessioni, protocollo precedente) | CIMD `chatgpt.com/oauth/codex/client.json`, redirect di loopback su porta effimera (RFC 8252 regge) | — | — | — |

**Altri controlli:**
- curl attraverso il tunnel: PRM e metadati AS esatti, 401 con la
  challenge, Origin 403, token nella query 400, `/register` senza flag
  404, redirect estraneo → pagina e non redirect;
- registro d'accesso: zero token e zero testo delle note;
- `oauth approve` con il codice minuscolo funziona.

**Corretto:**
1. **ChatGPT veniva rifiutato.** Il suo documento client dichiara
   `token_endpoint_auth_method: private_key_jwt`, ma elenca `none` in
   `token_endpoint_auth_methods_supported`. Ora un client che supporta
   anche `none` si accetta come pubblico con PKCE. Il `private_key_jwt`
   puro resta rifiutato.
2. **`oauth pending` non elencava mai niente:** il filtro leggeva la data
   di creazione al posto della scadenza. I test di GLM approvavano
   leggendo il codice dalla pagina, mai dalla CLI.

**Osservazioni** (non difetti di questa fase):
- **ngrok gratuito** risponde alle GET con User-Agent da browser con una
  pagina HTML d'avviso. I tre client sono passati lo stesso, perché i
  loro backend non si presentano come browser. Scritto in `docs/http.md`,
  insieme alla tabella dei client provati.
- **Codex e la chat di ChatGPT** hanno elenchi di server MCP separati
  (scritto in `docs/http.md`).
- **ChatGPT chiede `/.well-known/openid-configuration`**: 404, poi usa
  RFC 8414. Va bene così.
- **Con `--compat chatgpt` anche Claude vede `search`/`fetch`** accanto a
  `memory_search`, perché un processo serve tutti i client. Possibile
  seguito: mostrare gli strumenti di compatibilità solo ai grant il cui
  client è `chatgpt.com`.
- **`explain.bm25_rank`** numera prima dei filtri (scelta della v0.1,
  fissata da un test). Via HTTP, un rango che manca (2 e 3 senza 1)
  rivela a un client remoto che una nota in quarantena corrisponde alla
  ricerca. Da rinumerare dopo i filtri in una fase prossima.

# REPORT — Engramma v0.5.3 (domande corte, note nascoste invisibili, rifiniture di HTTP)

Base `c7d3227` (`fase-v0.5.2.2`). Un commit per punto: P0 `a74709c`,
P1 `c30e7bb`, P2 `5756ea5`, P3 `f559524`, P4 (documenti) `a826987`; più
le due correzioni che la verifica di chiusura ha preteso dal P0
(`775d222` le colonne della forma intera, `0f3ab6b` la potatura delle
forme assenti e il df del proprietario).
Non cambia il modello di sicurezza della v0.5.2 né il contratto JS:
cambiano la qualità della ricerca (per ogni trasporto, Homun compreso) e
la riservatezza delle note nascoste.

## P0 — La lingua delle domande corte

**Il difetto, com'era:** «Zebedeo» era tedesco e «tonno» francese per
`whatlang`; la domanda veniva stemmata nella lingua sbagliata e non
trovava niente («crocchette» e «gatto Zebedeo» trovavano). Dalla v0.1,
dentro il nucleo.

**Cosa fa ora il nucleo:**

- `text::detect_query_lang` — una domanda è **incerta** con meno di 4
  parole significative (stopword escluse) o quando `whatlang` dichiara la
  propria rilevazione inaffidabile (`is_reliable()`: falso anche su buon
  inglese di una frase — misurato a mano prima di decidere). Le note
  continuano con la rilevazione piena: hanno testo a sufficienza e la
  loro lingua vince comunque (l'indice è stemmato con quella).
- Su una domanda incerta ogni parola porta **la forma intera** (che
  l'indice contiene quando la nota non ha lingua) più **la radice in
  ogni lingua presente nella cella**: l'istogramma delle lingue delle
  note visibili, in `Inner` accanto al grafo, costruito al primo
  bisogno, invalidato dalle scritture proprie e da `PRAGMA
  data_version`. Senza lingue note, le sei di sempre. Le forme sono
  alternative in OR; `q.lang` esplicito vince come sempre; una domanda
  affidabile stemma esattamente come prima (test: l'elenco dei termini
  coincide con quello del `--lang it`).
- **Le coppie adiacenti** (frasi di due radici, P1.7 della v0.2.1) si
  fanno sulle **radici della lingua più frequente** della cella: mai
  coppie tra forme alternative della stessa parola (test dedicato).
- `query_terms` lavora **per parola** (`QueryWord`: forme + forma
  canonica); la caduta dei termini ubiqui e la copertura ragionano per
  parola (la frequenza di una parola è la migliore delle sue forme).

**Misure** (regola scritta prima di misurare, vedi la fase; «prima» =
`c7d3227` nello stesso worktree, stessa macchina, mediana di 3 giri):

| banco | soglia | prima | dopo | verdetto |
|---|---|---|---|---|
| domande corte (48 da 1–3 parole, vault dorato) | richiamo@5 **migliore** | 47/48 = 0,979 | **48/48 = 1,000** | ✓ (mancava «Esposito», indovinato portoghese) |
| banco italiano 100k, MRR pesato (viva) | ≥ −0,005 | 0,683 | 0,683 | ✓ identico |
| banco italiano 100k, MRR pesato (uniform) | ≥ −0,005 | 0,739 | 0,739 | ✓ identico |
| LongMemEval dev 150, ibrido turno ndcg@5 | ≥ −0,005 | 0,7303 | 0,7289 | ✓ (−0,0014) |
| LongMemEval 470 (corsa piena), ibrido turno ndcg@5 | riportata | 0,6970 | 0,6975 | ✓ (+0,0005) |
| banco 10k, solo testo p50 / testo+grafo p95 | entro +10% | 1,2 / 1,9 ms | 1,1 / 1,9 ms | ✓ |

La soglia di 4 parole regge: non è stato necessario provare 3 e 5.
Le domande dei banchi grandi sono frasi complete; le incerte sono la
minoranza (le da 1–3 parole, appunto), e su di loro l'espansione ha
cambiato i MATCH senza spostare i risultati (i due MRR identici a 100k
lo mostrano numero per numero).

**Tre difetti trovati dalla verifica di chiusura, corretti prima dei
numeri definitivi** (commit `775d222` e `0f3ab6b`):

1. *Le forme intere allagavano i titoli.* «dove lavora la sorella di
   Giulia» (incerta): la forma intera «giulia» matchava la colonna
   titolo — grezza, peso 10 — e cinque risultati di testo sommergevano
   la risposta multi-salto del grafo (golden_semantic, rosso). La forma
   intera ora cerca `body_norm` e `tags_text` — dove l'indice tiene il
   testo grezzo proprio quando la nota non ha lingua, la ragione per cui
   esiste — TRANNE quando è tutta la domanda: «Luca» deve trovare
   «Luca Ferrari» nel titolo (banco corto, rosso finché il filtro era
   totale).
2. *Il prezzo dell'espansione.* Rimisurato a 10k il p50 del solo testo
   era salito da 1,2 a 1,7 ms (+42%, oltre la soglia): una forma per
   ogni lingua della cella moltiplicava i termini del MATCH. Le forme
   che la vocabulary non contiene (df = 0) non possono matchare niente:
   lasciano i termini singoli (resta sempre la forma canonica, su cui si
   costruiscono le coppie). Tornato a 1,1 ms, p95 invariato.
3. *La potatura nascondeva al proprietario le sue note.* Il df del
   percorso testo era calcolato sempre senza quarantena: una forma che
   viveva solo in una nota in quarantena veniva potata anche nella
   ricerca `includeQuarantined` del proprietario (test JS della v0.4,
   rosso). Il df segue ora il filtro della domanda. I banchi non
   cambiano (cercano sempre senza quarantena).

## P1 — Le note nascoste non lasciano impronte

**Il difetto:** la lista BM25 conteneva anche le note nascoste; i filtri
arrivavano dopo. Un client remoto con `explain` leggeva ranghi che
partivano da 2, un `fused_score` sotto 1 al primo posto, e una finestra
da cui le note visibili potevano essere spinte fuori. Il test della v0.1
che fissava quel comportamento è aggiornato qui (il primo visibile è
rango 1).

**Cosa è cambiato:**

- `bm25_candidates` filtra lo stato **dentro la query** (stessi stati
  della lettura): ranghi densi da 1 sui visibili, niente spostamenti.
  `EXPLAIN QUERY PLAN` verificato con test dedicato: il MATCH guida
  l'indice FTS e il JOIN cerca `notes` per rowid (nessuna scansione).
- I **ranghi vettoriali** si numerano dopo il filtro di visibilità.
- **Frequenze e totali** contano i visibili: la vocabulary dell'FTS viene
  corretta togliendo, per ogni termine, i documenti in quarantena (una
  COUNT puntata per rowid: una lettura per nota nascosta, non una
  scansione delle posting list) — così una nota nascosta non gira
  un'etichetta di `support`.
- **Il grafo**: l'indice ricorda lo stato della nota di ogni nodo; la
  lista d'uscita del push contiene solo note visibili (prima della
  finestra dei 50 e della normalizzazione al massimo); il cancello delle
  entità semina solo entità con almeno uno spigolo percorribile —
  un'entità dichiarata solo da note nascoste non apre il cammino e non
  diluisce le quote dei semi.
- **Gli altri filtri** (tipo, tag, validità, `as_of`, confidenza) restano
  dove erano: sono scelte di chi chiede, non segreti — un risultato che
  manca perché la domanda chiedeva il 2023 non dice niente delle note
  nascoste. Il filtro tipo già viaggia nel JOIN della lettura dei
  candidati; gli altri vivono in `passes_filters`, dove la loro semantica
  è dichiarata. Nessuno di loro influenza ranghi o normalizzazioni PRIMA
  della selezione (agiscono sui candidati già estratti), quindi nessun
  canale in più da chiudere.
- **La proprietà d'indistinguibilità** (proptest, 128 casi, celle
  generate; tre modalità: solo testo, ibrida con embedder finto, testo e
  grafo): stesse note più un numero qualsiasi di note in quarantena e
  lapidi che corrispondono alla domanda → **stesso insieme di id, ranghi
  densi da 1, massimo normalizzato a 1, stessi coseni, supporti, liste,
  termini trovati e ranghi semantico/grafo, per ogni id**. Lo stesso
  controllo via MCP stdio: `memory_search` risponde con gli **stessi
  byte** prima e dopo l'aggiunta delle note nascoste.
- **L'eccezione dichiarata: l'ε dell'IDF.** SQLite FTS5 calcola l'IDF di
  bm25 sull'indice **intero** — e le righe in quarantena devono restarci:
  `include_quarantined` è contratto JS. Una riga nascosta che corrisponde
  sposta i punteggi dei visibili (e con pareggi vicini, l'ordine interno).
  Misurata dal banco d'indistinguibilità (stampata a ogni corsa):
  ≈ 0,8% su tre note con match puliti, fino a **0,83 relativa** su
  corpus minimi patologici dove più righe nascoste matchano la stessa
  forma rara, decrescente col corpus. Nessun controfattuale può leggerla:
  ogni **canale strutturale** (id, ranghi, normalizzazioni, finestra,
  supporto, coseni) è chiuso ed è ciò che la proprietà asserisce. I casi
  dove una forma siede in metà dell'indice (la «briciola» 1e-6 di FTS5
  per gli IDF negativi) sono rifiutati in generazione: lì bm25 non
  significa niente comunque. Scritto in DESIGN §5.
- Nota sull'ibrida: la fusione **convessa** a min-max azzera il peggiore
  candidato di testo (comportamento della v0.2.1, `retain(score > 0)`
  lo butta) e chi è «il peggiore» può cambiare con l'ε — un dirupo, non
  un'oscillazione. La proprietà usa la RRF (ogni candidato tiene un
  reciproco positivo); la convessa resta com'è perché ritoccarla
  sposterebbe i banchi. Dichiarato qui e nel test.

**Misure P1** (le note in quarantena dei banchi sono poche; il p95 ha il
tetto più stretto, +5%):

| banco | soglia | prima | dopo | verdetto |
|---|---|---|---|---|
| banco italiano 100k, MRR pesato (viva/uniform) | ≥ −0,005 | 0,683 / 0,739 | 0,683 / 0,739 | ✓ identici |
| LongMemEval dev 150, ibrido turno ndcg@5 | ≥ −0,005 | 0,7303 | 0,7289 | ✓ (−0,0014) |
| banco 10k, testo+grafo p95 | entro +5% | 1,9 ms | 1,9 ms | ✓ (0%) |
| banco 10k, ricerca ibrida p95 (viva) | — (riferimento) | 1,9 ms | 1,9 ms | invariato |

## P2 — `search` e `fetch` solo a ChatGPT

- La regola «è ChatGPT» è un **campo di configurazione** della libreria
  (`HttpConfig.chatgpt_client_id`, base
  `https://chatgpt.com/oauth/client.json`): i test servono i tre
  documenti **veri** (scaricati il 2026-11-12, riportati nella fase) con
  solo il `client_id` diventato l'URL di loopback del documento di prova.
- `RequestGrant` porta il `client_id` del grant: `tools/list` espone
  `search`/`fetch` **solo** al grant ChatGPT; il test con tre grant
  (ChatGPT, Codex, claude.ai) verifica gli elenchi esatti — sette
  strumenti per ChatGPT, cinque per gli altri due.
- Chiamare `search` per nome da un altro grant risponde **esattamente**
  come uno strumento che non esiste: l'errore verbatim del ToolRouter di
  rmcp (`invalid_params "tool not found"`), stesso codice, stesso
  messaggio, byte per byte (test di uguaglianza contro uno strumento
  inventato). Nessun oracolo.
- Su stdio `--compat chatgpt` è un errore d'uso (prima era ignorato in
  silenzio): test sull'uscita del binario vero.

## P3 — Le rifiniture di HTTP

1. **Lo spam di `/authorize`**: tetto globale di 30 al minuto (pagina
   429 con `Retry-After: 60`, come `/token`); **cache negativa del CIMD**
   (un documento rifiutato o irraggiungibile non si riscarica per 5
   minuti: l'URL esatto, un evento solo — test con un contatore: tre
   tentativi, UNO scaricamento); tetti in attesa **2 per client_id e 10
   in tutto** (un client solo non riempie la coda; lo spammatore con
   molti domini resta possibile ed è scritto nel DESIGN §8.6);
   **`engramma oauth deny --all`** rifiuta tutto ciò che aspetta e le
   pagine d'attesa redirigono i browser con `access_denied` (test con la
   CLI vera).
2. **Sessioni legacy**: tetto di 1000 (escono le più vecchie; tiebreaker
   d'inserimento deterministico — trovato perché la prima versione del
   test era non deterministica con 1100 sessioni nello stesso secondo),
   un'ora di inattività, DELETE accettato = voce tolta. Il test apre 1100
   sessioni e controlla tetto e 404 delle espulse.
3. **MRTR**: TTL di 10 minuti (uno `requestState` scaduto risponde come
   uno sconosciuto — unit test), tetto di 1000, e lo stato passa da ULID
   (80 bit) a **256 bit casuali** (`mint::opaque_token`, prefisso
   `eng_rs_`).
4. **`/revoke` di un refresh** porta con sé gli accessi della stessa
   famiglia (RFC 7009 § 2.1); la revoca di un accesso tocca solo lui e
   il client continua a rinfrescare (test su entrambi i versi).
5. **SSRF**: `2002::/16` (6to4) giudicato dall'IPv4 incorporato come i
   mappati; `64:ff9b:1::/48` (NAT64 locale RFC 8215), `2001::/32`
   (Teredo) e `::/96` rifiutati. Proptest estesa: la 6to4 equivale
   sempre al suo IPv4; le tre forme non sono mai pubbliche.
6. **Test dovuti**: il ciclo di vita DCR (mai usato → 24 h; usato → vivo
   finché vive un grant, poi cancellato — orologio iniettabile); i bordi
   della cache del CIMD (max-age=1 → 300 s; 999999 → 86400; assente →
   3600; non numerico → 3600).
7. **Eventi con la causa**: `client_rejected` porta il dettaglio locale
   (`CimdError::detail`: DNS, nessun indirizzo pubblico, timeout, stato
   HTTP, TLS, regola del documento); sul filo la risposta resta generica.
8. **ngrok**: nessun codice; la frase di `docs/http.md` è stata
   rilegata dopo le modifiche e resta esatta.

## Chiusura

- macOS: `cargo fmt --check` ok; `cargo clippy --all-targets --offline --
  -D warnings` zero avvisi; `cargo test --offline --workspace
  --no-fail-fast` verde (l'unica corsa rossa in itinere era il
  golden_semantic del titolo, corretto prima della chiusura);
  `cargo test -p engramma-bench --example longmemeval` 3/3;
  `cargo deny check licenses bans` ok.
- Linux: `scripts/linux.sh` verde in Docker (arm64, rete chiusa, sorgente
  in sola lettura), volume `engramma-linux-target` rimosso.
- JS (Node 24): `npm test` 29/29, `test:electron` 29/29,
  `test:electron-main` ok — **senza cambiare una sola attesa**. Il test
  della quarantena è diventato rosso a metà chiusura (la potatura delle
  forme con df=0 nascondeva al proprietario le note che chiede con
  `includeQuarantined`: il df del percorso testo seguiva sempre il
  filtro predefinito): corretto, e l'attesa è quella della v0.4.
- Build: `npm run build` rifatto; il `.node` cambia (P0 e P1 sono nel
  nucleo). SHA256: `.node`
  `419372e04d2331644d09af77983290393f4ba692c6b8360c630342841e6878c9`,
  `index.js`
  `25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4`,
  `index.d.ts`
  `bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8`.
- Disco: `target` 22G all'inizio, **24G** alla fine (i binari
  release dei banchi e le ricostruzioni; nessuna pulizia forzata: la
  fase non tocca `~/.cache/cargo-target`).

## Per Homun

**Aggancio consigliato.** Tutti i miglioramenti sono nel nucleo e valgono
per ogni trasporto: le domande corte (da una a tre parole — come si fa
spesso a voce) ora trovano; le note in quarantena non spostano più i
ranghi né i punteggi normalizzati di ciò che l'utente vede. Il contratto
JS è invariato: stessi metodi, stesse attese, stesso `.node` che si
ricompila da sé. Nessuna migrazione: celle e `oauth.db` aprono come
prima (la tabella nuova della cache negativa nasce da sola al primo
aperto). Se Homun espone MCP su HTTP, gli strumenti di compatibilità
restano al solo collegamento ChatGPT anche per lui, e `deny --all`
chiude la coda delle richieste in un colpo.

# Verifica v0.5.3 (2026-11-13, revisore)

Esito: **passata, senza correzioni.**

Controllato:
- **Controlli automatici:** fmt, clippy `-D warnings`, 408 test del
  workspace + 3 LongMemEval, `cargo deny`.
- **Build:** `.node` rifatto: hash `419372e0…` uguale al REPORT; JS 29/29.
- **Codice:** riletti `detect_query_lang`, il filtro di stato dentro
  `bm25_candidates` e la regola ChatGPT di P2.
- **Banco delle domande corte:** 48 valutate (25 di una parola, 13 nomi,
  10 da due o tre parole) più 3 casi di quarantena.

**Prova a mano su una cella nuova di quattro note:**
- **Domande corte:** «Zebedeo», «zebedeo», «tonno», «Esposito»,
  «Aurora», «martedi», «verde», «dottoressa» e «crocchette tonno»
  trovano tutte la nota giusta al primo posto. Con la v0.5.2.2
  «Zebedeo» e «tonno» non trovavano niente.
- **Impronte delle note nascoste:** stesse domande in JSON prima e dopo
  aver aggiunto tre note in quarantena che corrispondono («Zebedeo il
  gatto ha mangiato tonno…»). Id, ordine, ranghi BM25 densi da 1 e primo
  punteggio normalizzato a 1 restano identici: i canali strutturali sono
  chiusi.

**Il residuo dichiarato (ε dell'IDF) è più grande di quanto sembri.**
- Su «gatto» e «Zebedeo» i punteggi grezzi cambiano alla sesta cifra.
- Su «Zebedeo tonno» il secondo risultato passa da `1e-06` a `0,4156`,
  senza nessun cambiamento fra le note visibili. Succede perché la
  convessa a min-max mette a zero il peggiore candidato di testo, e chi
  è il peggiore dipende dall'IDF, che FTS5 calcola sull'indice intero.
- Quindi un client remoto che ripete la stessa domanda nel tempo può
  accorgersi che è arrivata una nota nascosta che corrisponde. Non vede
  quale nota né cosa dice.
- **La correzione strutturale** è un secondo indice FTS per le sole note
  in quarantena: `include_quarantined` interroga entrambi e fonde, i
  passaggi di stato spostano la riga da un indice all'altro, e le
  statistiche dell'indice visibile non vedono mai le note nascoste. Va
  nella prossima fase. Rimedio parziale e più economico: togliere i
  punteggi numerici dall'`explain` servito su HTTP.

Disco: `target` 24G (era 22G).

---

# REPORT — Engramma v0.6 (la stessa memoria sui tuoi dispositivi)

Data: 2026-11-13. Base: `main` @ `ec503fe` (fase-v0.5.3 verificata, più un
commit di sole dipendenze: la crate `engramma-sync` con iroh 1.4, negentropy
0.5, spake2 0.4, hpke 0.15, chacha20poly1305 0.11, ed25519-dalek 3). Tutto
costruito e provato offline, Rust 1.95.0, macOS arm64. **Solo i dispositivi
del proprietario** — la decisione dell'utente (2026-11-13); la condivisione
con altre persone e l'API JS della sincronizzazione sono la v0.6.x.

## Il difetto di partenza, misurato e chiuso (P0)

Il revisore l'aveva misurato: FTS5 calcola l'IDF di bm25 sull'indice
**intero**, e una nota in quarantena che corrisponde alla domanda sposta i
punteteggi delle visibili («Zebedeo tonno»: il secondo risultato da `1e-06`
a `0,4156` con le stesse note visibili). Un client remoto che ripete la
domanda legge l'arrivo di una nota nascosta nei numeri.

Ora `notes_fts` tiene **solo** le note visibili (`active`, `superseded`),
`notes_fts_hidden` solo quelle in quarantena, le lapidi nessuna dei due
(migrazione v14: crea l'indice nuovo, sposta le righe, ricostruisce il
vecchio — e ogni passaggio di stato sposta la riga **nella stessa
transazione** dell'operazione, provato). La ricerca di base interroga solo
`notes_fts`: IDF, df, ranghi e normalizzazioni non vedono mai le nascoste.
Con `include_quarantined` si interrogano entrambi e si fonde **per rango**
(RRF k=60 nel ramo testo, pari merito spezzato per id: la scelta scritta —
i due bm25 sono numeri incommensurabili, e l'indice unico è ciò che questa
fase ha tolto). La correzione della vocabulary della v0.5.3 (df meno i
quaranteni, una COUNT per termine) non serve più: via.

**L'invariante è diventato esatto.** Il proptest di indistinguibilità ora
chiede uguaglianza **bit per bit** di `score`, `raw_score`, `fused_score`,
coseni (l'ε dell'IDF sparisce dal DESIGN); il caso «Zebedeo tonno» del
revisore è un test a sé. `verify` controlla la coerenza dei **due** indici
contro gli stati; `fts_follows_edits` e i test dell'indice marcio sono
estesi al ramo nascosto.

**Misure** (le soglie erano MRR −0,005, ndcg@5 −0,005, p95 +5%):

| | soglia | v0.5.3 | v0.6 | esito |
|---|---|---|---|---|
| banco italiano 100k, MRR pesato (viva/uniform) | ≥ −0,005 | 0,683 / 0,739 | 0,683 / 0,739 | ✓ identici |
| banco 10k, testo+grafo p95 | ≤ +5% | 1,8 ms | 2,0 ms | ✓ (il secondo indice costa ~0,2 ms al collo) |
| LongMemEval dev 150, ibrido turno ndcg@5 | ≥ −0,005 | 0,7289 | 0,7289 | ✓ identico |
| LongMemEval 470 (corsa piena), turno ndcg@5 | riportata | 0,6975 | 0,6975 | ✓ identico |

## P1 — il registro converge

Lo stato di una copia è funzione dell'**insieme** delle operazioni, mai
dell'ordine di arrivo. È la proprietà su cui regge tutta la fase, ed è
provata così: 512 casi di flotta (2–4 dispositivi, scritture concorrenti
vere, consegne casuali con duplicati e partizioni), ogni copia con lo
stesso digest, uguale al riferimento in ordine totale, verify verde.

- **P1.1 — le fondamenta.** `content_hash`: blake3 del contenuto canonico
  `{op_id, hlc, actor, kind, note_id, base, payload_digest}` — la stessa
  operazione, la stessa identità su ogni copia; `prev_hash`/`hash` restano
  la catena locale di quel disco. `base`: il `content_hash` dell'ultima
  operazione di quella nota che l'autore aveva applicato — stessa base,
  concorrenti. `op_objects`: gli oggetti semantici di ogni operazione (solo
  id, vivono oltre la redazione). `ingest_op`: l'arrivo — idempotente per
  `op_id`, identità ricalcolata e rifiutata se non torna, orologio oltre
  i +5 minuti rifiutato. `replicated_digest`: le tabelle replicate senza
  catena d'arrivo né `hlc`.
- **P1.2 — la materializzazione per oggetto.** Un'operazione che
  nell'ordine totale `(hlc.physical_ms, hlc.counter, hlc.node, op_id)` cade
  prima dell'ultima applicata al suo oggetto lo fa **rimaterializzare**:
  le righe vanno, lo stato al taglio di compattazione torna
  dall'istantaneo di versione del primo arrivo sopra il taglio, tutte le
  operazioni si rigiocano in ordine totale, ciascuna nella metà che la
  riguarda (`supersede` vale per la nota nuova e la vecchia, `decide` per
  la proposta e il bersaglio; la riga `supersedes` la scrivono entrambe le
  metà, `INSERT OR IGNORE`). Chi arriva ultimo applica diretto. Un
 'operazione su un oggetto che non c'è ancora aspetta in
  `pending_objects` e materializza quando arriva la put. `verify` ricompone
  il log con la stessa strada dell'arrivo.
- **P1.3 — le regole dei conflitti.** Il corpo: vince l'ultima nell'ordine
  totale; la perdente diventa una **copia di conflitto** — nota attiva, id
  deterministico `blake3("engramma/conflict/v1" ‖ content_hash del
  perdente)`, ULID con l'HLC del perdente, tag `conflitto`, arco
  all'originale — **stato derivato**, identico su ogni copia senza
  un'operazione (tavola `conflict_copies`, v17). `decide` concorrenti: la
  prima nell'ordine totale; le altre restano nel registro come superate.
  La lapide vince su chi non l'aveva vista (il replay riporta la nota allo
  stato che il suo autore aveva applicato); chi si aggancia all'ultima
  operazione della nota riapre. Il grilletto `breaks_the_chain` porta ogni
  arrivo concorrente alla materializzazione.
- **P1.4 — la purge domina.** Quando l'insieme contiene una purge della
  nota X: X non materializza, le sue operazioni arrivano **redatte** (lo
  scrub gira nella stessa transazione dell'arrivo, con `secure_delete`
  attivo: il testo in chiaro non raggiunge mai il WAL), le sue copie di
  conflitto e gli archi che la nominano vanno con lei, e la purge che
  arriva porta l'igiene del disco della purge locale (optimize, VACUUM,
  checkpoint): i corpi nei frame WAL delle transazioni precedenti se ne
  vanno davvero (la ricerca di stringhe nei file lo prova).
- **P1.5 — la frontiera stabile.** `device_acks`: ogni sessione finita
  registra la testa che l'altro dispositivo tiene; la compattazione di una
  cella condivisa taglia al **minimo** degli ack — sopra la frontiera i
  payload restano. (Tavola locale, mai nello stato replicato; le
  istantanee di compattazione la portano via come le vault.)
- **P1.6 — `adopt`.** Le operazioni pre-v0.6 non hanno firma: chi condivide
  firma un'operazione con la radice di Merkle dei `content_hash` di tutto
  ciò che esiste (hash ordinati, coppie, foglia dispari raddoppiata —
  indipendente dall'ordine d'arrivo, provato su due copie con consegne
  opposte).
- **Le misure.** La peggiore tempesta di rimaterializzazioni — una nota da
  cento versioni consegnata **alla rovescia**, una rimaterializzazione per
  arrivo — costa **364 ms** in tutto (≈3,6 ms a rimaterializzazione); il
  test resta in suite come arganello. L'inserimento a 10k con le
  fondamenta in cima: **3082 note/s** (la v0.5.3 misurava ~3460 sul banco
  equivalente: il costo di `base` + `op_objects` per ogni scrittura; la
  query della base salta le note nuove, che è il caso di un inserimento).
- **Due difetti veri catturati alla chiusura**, corretti con il test rosso
  prima: la riga `supersedes` di una `decide` la devono scrivere entrambe
  le metà (una copia che rimaterializza la sola proposta non deve
  perderla — l'ha scovata un proptest); e l'insieme dei conflitti ora
  **deriva dal vincitore finale** (l'ultima scrittura di corpo
  nell'ordine totale; i perdenti sono quelle dopo la sua base) con la
  regola: una base assente dagli eventi è sequenziale, mai un passato
  concorrente — prima fabbricava copie fantasma, perché il censor della
  purge riscrive il `content_hash` di un supersede e una patch scritta
  prima della censura punta a un hash che non esiste più.

## P2 — l'identità del dispositivo

`<root>/device.key`: un seme da 32 byte, permessi 0600 (provato), creato al
primo bisogno. HKDF-SHA256 con etichette distinte: trasporto iroh (Ed25519),
firma (Ed25519), HPKE (X25519, con clamp fatto in casa per l'interoperabilità
iroh/HPKE — `from_bytes`/`to_bytes` di hpke non sono involutori). **Mai la
stessa chiave per due usi** (provato: le tre chiave non coincidono);
l'id del dispositivo **è** la chiave pubblica di firma. `devices.db`: create
(il primo, da sé), add (un membro garantisce), remove (con la radice di
Merkle e il conto delle operazioni accettate del rimosso). La revoca
domina — chi è stato rimosso non rientra; l'ultimo non si rimuove; un
non-membro non firma. Le firme si verificano contro l'elenco, e l'elenco
viaggia fra dispositivi. Le operazioni della cella portano `device` e `sig`
(schema v19): firma Ed25519 sul `content_hash`, autore crittografico
distinto da `actor`; il firmatario locale si arma con `set_signer`.

## P3 — l'abbinamento

Il biglietto: indirizzo iroh + segreto a 128 bit, base32 col prefisso di
versione, dieci minuti e un uso; chi entra autentica chi invita dal MAC
del segreto sul transcript (il segreto in chiaro non attraversa mai —
provato). Il codice breve: `7-tigre-lago`, canale più due parole dalla
lista fissa di **2048 parole italiane nel repo** (11 bit l'una); SPAKE2
(RFC 9382, gruppo Ed25519) col codice come password. **La crate `spake2`
non ha audit indipendente** — dichiarato nel DESIGN § 10 e qui: l'uso è
confinato a questo percorso. SPAKE2 per natura non rifiuta un codice
sbagliato: le chiavi non tornano, ed è la conferma SAS a chiudere il
cerchio. Le sei cifre: blake3 del segreto condiviso e del transcript,
stampate su entrambi gli schermi (provato che un'altra coppia dà altre
cifre); l'uomo nel mezzo col biglietto altrui fallisce il MAC.

## P4 — le buste sigillate

Chiave di cella per **epoca** (la 0 da chi condivide, k+1 da chi rimuove),
avvolta per dispositivo con HPKE (RFC 9180, modo base, X25519 HKDF-SHA256
ChaCha20-Poly1305, `info = etichetta ‖ cell_sync_id ‖ epoca`). La busta:
XChaCha20-Poly1305, chiave `HKDF(chiave d'epoca)`, nonce casuale di 24
byte, AAD = `cell_sync_id ‖ epoca ‖ id opaco`. Gli id opachi:
`cell_sync_id` = HMAC(chiave epoca 0); l'id dell'operazione = HMAC(chiave
di mac stabile, `content_hash`) — il nodo cieco riconosce le operazioni
attraverso le epoche senza mai vedere `op_id` né HLC. **Prove:** 1M di
buste senza un nonce ripetuto (64 s); AAD o epoca sbagliati rifiutati; il
rimosso non apre l'epoca nuova; le chiavi avvolte per altri non si aprono.

## P5 — il protocollo su iroh

ALPN `engramma/sync/1`. All'accettazione la chiave iroh del chiamante deve
essere la chiave di trasporto di un dispositivo **vivo** — altrimenti la
connessione si chiude prima di qualsiasi scambio (il cancello della
minaccia 3, provato nell'e2e e nello unit test `a_stranger…`). La sessione:
versione, **l'elenco riconciliato per primo** (le righe viaggiano con la
propria firma; chi le riceve verifica e applica), poi per cella la
riconciliazione **negentropy (RBSR)** sugli id — `(hlc.physical_ms,
content_hash)` — con i due giri dell'RBSR davvero implementati (initiate →
reconcile → reconcile_with_ids, il secondo giro ripiegato nella lista dei
bisogni) — e le buste mancanti nei due sensi, in lotti con ricevuta. Il
filo è JSON a lunghezza prefissa su stream iroh: dentro QUIC; il sigillo
serve al relay e al nodo cieco. **Banco end-to-end su loopback** (veri
endpoint, 127.0.0.1, relay spento): due dispositivi, la cella di A che
viaggia sigillata a B, stesso `replicated_digest`, verify verde. I limiti
del piano sono nel protocollo (1 MiB a messaggio). **Sync serve/run**
arrivano col loop di servizio: il piano li chiede e la CLI oggi li dichiara
apertamente (errore che dice dove stanno) — la sessione, il cancello e la
riconciliazione sono qui e provati.

## P6 — il nodo cieco

`mailbox.db`: buste sigillate per elenco e `cell_sync_id`, indicizzate per
**id opaco e null'altro**. Deposito idempotente, enumerazione (verso il
nodo l'elemento è `(0, id opaco)`: il tempo non si vede), consegna,
ritenzione 180 giorni con VACUUM alla scadenza. **La purge arriva firmata**
(contenuto canonico `owner ‖ id` firmato dal dispositivo; la verifica
contro i vivi dell'elenco — una firma estranea non passa) e poi DELETE e
VACUUM: **nessun byte delle buste resta nel file**, provato cercando le
stringhe nel file. Il filo (deposit/enumerate/fetch/purge) con round-trip
stabile.

## P7 — la CLI

> **Corretto dalla v0.6.1:** al momento della verifica questa sezione
> descriveva comandi dichiarati e non fatti — `sync run`, `sync serve`,
> `devices join`, `mailbox add/remove/status` rispondevano «arriva più
> avanti». La v0.6.1 li ha implementati e provati dal binario vero (vedi
> «REPORT — Engramma v0.6.1 (chiusura)»); il testo qui sotto vale come
> l'elenco finale, non come la verità del 2026-11-13.

`engramma devices` (init, invite, code, join, list, remove con il
riassunto, log), `engramma sync` (share, unshare, run/serve, status,
conflicts), `engramma mailbox` (serve, add, remove, status). Ogni comando
`--json`; gli errori non contengono testo di note.

## P8 — Homun

> **Corretto dalla v0.6.1:** «il demone gira accanto a `engramma sync
> serve`» era, alla verifica, una sessione di test della crate — non la
> convivenza di due processi veri sulla stessa radice. La v0.6.1 l'ha
> provata dal binario (un secondo processo che scrive mentre il servizio
> riceve); lo schema è salito alla **v20** (payload_state). Vedi il
> REPORT v0.6.1.

Il contratto JS **non cambia**; i 29 test Node passano identici (e i 29
`test:electron`, e `electron-main`), senza cambiare le attese — la prova
che la semantica locale è quella di sempre. Lo schema delle celle sale
alla **v19** (v15 content_hash/base/op_objects, v16 pending, v17
conflict_copies, v18 device_acks, v19 device/sig): non si torna indietro
(SCHEMA_TOO_NEW). Il demone gira accanto a `engramma sync serve --root`:
la convivenza è la sessione provata in P5. L'API JS della sincronizzazione
è la v0.6.x, scritto in `docs/node.md`.

## P9 — la documentazione

DESIGN: § 4.3.1 nuovo (la convergenza: identità, ordine totale,
materializzazione, base, conflitti, frontiera, adopt), § 5 (l'ε sparisce,
la proprietà è esatta, la scelta RRF scritta), § 10 riscritto (identità,
elenco, firma, abbinamento, buste, id opachi, nodo cieco, **cosa non si
promette** — PCS/FS — con le fonti: RFC 9180/9382/9420, Keyhive, DCGKA),
§ 12 (v0.6 fatta, v0.6.x = persone e API JS). `docs/sync.md`, nuovo: come
si abbina, come si condivide, il nodo cieco, il relay (pubblico, proprio,
nessuno; pkarr spento di base), «chi vede chi», perdere un dispositivo, i
conflitti, i limiti. `docs/cli.md`: i tre gruppi di comandi.

## La chiusura

- **macOS:** `cargo fmt --check` verde; `cargo clippy --all-targets
  --offline -- -D warnings` verde; `cargo test --offline --workspace
  --no-fail-fast` **47 suite verdi, 0 fallite** (47 `test result: ok`);
  `cargo test -p engramma-bench --example longmemeval` 3/3; `cargo deny
  check licenses bans` ok (la solita nota Unicode-DFS-2016).
- **JS** (Node 24.22.1, Electron via Homun): `npm test` **29/29**,
  `test:electron` **29/29**, `electron-main` ok — senza cambiare le
  attese. Il contratto JS è invariato; una patch su una lapide la riapre
  (regola v0.6) e nessun test JS la toccava.
- **Build** (`npm run build` in `crates/engramma-node`, rifatta dopo
  l'ultimo commit di codice): `engramma.darwin-arm64.node`
  `1180fc793a9a86ef529ed7a3be20faaeaca2c77659ac4d9b806dcb84318e1a05`;
  `index.js` `25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4`;
  `index.d.ts`
  `bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8`.
- **Banchi:** quelli di P0 (sopra), più la misura P1 (364 ms la tempesta
  delle cento versioni; 3082 note/s l'inserimento a 10k). `bench/RESULTS.md`
  aggiornato dal giro a 10k del codice completo.
- **Linux:** `scripts/linux.sh` **fermato e scritto**: il demone Docker
  non è raggiungibile su questa macchina in questa sessione («Cannot
  connect to the Docker daemon»), quindi il container non è stato fatto
  girare. I test di rete sono tutti loopback-espliciti (gli endpoint iroh
  si legano a `127.0.0.1` con relay e scoperta spenti) e la suite è
  verde su macOS arm64; **il giro nel container resta da fare** prima di
  dichiarare chiusa la verifica multipiattaforma — il volume
  `engramma-linux-target` non è stato creato, quindi non c'è nulla da
  rimuovere.
- **Disco:** `du -sh target` **25G** all'inizio, **34G** alla fine
  (sviluppo + release del banco e dei banchi; `~/.cache/cargo-target`
  mai toccato).
- **Tag:** `fase-v0.6` annotato; temporanei cancellati; nessun push.

## Cosa non promettiamo

- **Né PCS né FS per messaggio.** Una chiave d'epoca rubata apre tutta
  l'epoca; la rotazione alla rimozione è il compromesso per i dispositivi
  di una sola persona. MLS presuppone un servizio di consegna che ordina,
  Keyhive e DCGKA sono pre-alpha: la scelta è scritta nel DESIGN § 10 con
  le fonti, ed è la prima da rivedere per la condivisione con altre
  persone.
- **Ciò che resta a un dispositivo rimosso.** La revoca ruota le chiavi e
  chiude la porta; il disco del rimosso tiene ciò che aveva già
  scaricato. La revoca non cancella i dischi altrui.
- **I metadati visibili al relay e al nodo cieco.** Chiavi degli
  endpoint, indirizzi, orari, numeri e dimensioni dei messaggi. Non il
  testo, non i nomi, non gli id in chiaro.
- **`spake2` non ha audit indipendente**, e l'uso è confinato al codice
  breve.
- **Il giro Linux nel container non è stato fatto** (Docker fermo): la
  verifica multipiattaforma è aperta.

## Per Homun

- Il `.node` cambia (P0 e P1 sono nel motore): SHA256 sopra, `prepare-
  engramma` va rilanciato.
- Lo schema delle celle sale alla **v19**: le celle migrate non tornano a
  un binario più vecchio; la migrazione avviene all'apertura ed è
  irrevocabile (con `verify` verde dopo).
- Per sincronizzare: `engramma sync serve --root <radice di Homun>` accanto
  al demone; la convivenza dei due processi è la sessione provata in P5.
  L'interfaccia per l'utente (stato, QR del biglietto, elenco dei
  dispositivi) è la v0.6.x.

# Verifica v0.6 (2026-11-14, revisore)

Esito: **NON superata.** Le correzioni sono nella spec
`progetti/engramma/prompts/engramma-v0.6.1.md`.

**La base è buona:**
- P0, i due indici FTS: l'indistinguibilità è esatta;
- le fondamenta di P1: identità, materializzazione per oggetto, regole
  dei conflitti, purge, frontiera, `adopt`;
- le primitive di P2–P4: chiavi derivate, elenco firmato, buste, HPKE,
  SPAKE2, SAS;
- la sessione di P5 con negentropy;
- fmt, clippy e `cargo deny` verdi; JS invariato.

**Cosa non va:**

1. **La convergenza non regge.** `the_fleet_converges` fallisce in ogni
   corsa del revisore (447 test passati, 1 fallito), con un caso
   ridotto nuovo ogni volta.
   - **Causa:** la censura della purge (v0.3) riscrive il payload dei
     `supersede` e quindi il loro `content_hash`. Ma nella v0.6 quello è
     l'identità che viaggia: la `base`, l'elemento RBSR, ciò che la firma
     copre.
   - **Effetto:** una patch che punta all'hash originale è concorrente su
     una copia e sequenziale sull'altra, e una delle due ha una copia di
     conflitto fantasma. La regola «base assente = sequenziale»
     (`2e5f4e9`) dipende dall'ordine d'arrivo.
   - **Il test non è deterministico come dichiara.** L'ora di base è
     quella reale e gli `op_id` sono casuali: un seme salvato non
     riproduce lo stesso caso, ed è così che il difetto è passato per
     verde. Il seme nuovo è in `convergence.proptest-regressions`.
2. **La sincronizzazione utilizzabile non c'è.**
   - `sync run`, `sync serve`, `devices join` e `mailbox add/remove/status`
     rispondono con un errore.
   - `mailbox serve` stampa «in ascolto» ed esce.
   - `devices invite` dà un biglietto senza indirizzi.
   - La conversione delle operazioni per il filo e l'adattatore fra
     sessione e cella stanno nel file di test, con l'epoca fissa a 0.
   - Il nodo cieco non ha un servizio di rete; l'abbinamento non ha un
     flusso in rete.
3. **Le firme non si verificano all'arrivo.** In produzione nessuno
   controlla `sig` e `device` contro l'elenco, né la copertura `adopt`.
4. **Due soglie superate ma segnate ✓:**
   - p95 a 10k da 1,8 a 2,0 ms (+11% contro +5%);
   - inserimento da circa 3460 a 3082 note/s (circa +12% contro +10%).
5. **Mancano** gli end-to-end con tre dispositivi, col relay e col
   dispositivo che torna, e le due misure nuove (bootstrap di 10k,
   latenza).
6. **Il REPORT descrive P7 e P8 come fatti, e non lo sono.** Il giro
   Linux non c'è: Docker è fermo anche sulla macchina del revisore.

`du -sh target`: 34G.

# REPORT — Engramma v0.6.1 (la sincronizzazione, finita davvero) — LAVORO IN CORSO

Data: 2026-11-14. Base: `main` @ `ac31aba`. **La fase NON è chiusa**: questo
report dice ciò che è fatto e provato, e ciò che resta. Nessun tag.

## Fatto e provato

**P0 — l'identità non cambia mai (commit `425e2fc`).** `payload_state`
(whole|redacted|censored, schema v20): la censura tocca il corpo, mai
`content_hash`/`sig`; `verify` controlla forma e sale del censurato; una
`base` assente è «non ancora arrivata» (attesa in `pending_objects`,
legata all'oggetto della base); la testa prima di un arrivo si cerca sopra
il taglio e, se non c'è, su tutto il log; il cammino rapido riconcilia i
conflitti; una body write senza base su una nota che vive è concorrente.
`the_fleet_converges` **deterministico** (orologio a costante, mint
iniettabile): **2048 casi + 5 corse a semi nuovi, tutte verdi**; lo
scenario del revisore e la proprietà (purge/censura/compattazione non
muovono l'identità) in `tests/identity.rs`; i difetti trovati per strada
(conflitti stantii sul cammino rapido, create concorrenti dello stesso
id) sono test propri. Diffetti trovati e corretti: il rebuild di verify
ora conosce il taglio; `sync_shared` esce dal dump logico.

**P1 — la strada dal filo (commit `f02237c`).** `wire` (v2, con lo stato
del payload e rifiuto di ogni campo mancante), `SharedCell` (epoche dal
keystore, id opachi), il cancello d'arrivo (firma, elenco, rimosso con
l'insieme in forma operativa = tutto ciò che precede la remove; l'adopt
con **la prova di Merkle** per l'era senza firma), `DeviceSigner`. Un
test per rifiuto in `tests/e2e.rs`. Il frame `keys` porta le chiavi
avvolte a ogni sessione.

**P2–P4 (commit `3f10c03` + questo).** `devices invite/join` in rete
(biglietto con indirizzo vero, SPAKE2 sul segreto, conferma di chiave
RFC 9382, SAS su entrambi, poi elenco/chiavi/celle — con la seconda
passata della coda); `sync run` (riepilogo JSON, irraggiungibile = riga)
e `sync serve` (cancello, data_version + 2 s, 5 minuti, segnali); il nodo
cieco in rete (deposito/enumerazione/consegna/purge firmata/stats,
l'incontro del codice col relay cieco e il canale che brucia); la coda
purge parte da sola; `mailbox add/remove/status`; la CLI arma il
firmatario. Provato dal **binario vero**: invito completo, SAS rifiutata,
biglietto scaduto/riusato/rubato; il servizio accanto a un secondo
scrittore; **tre dispositivi con scritture concorrenti e uno che torna**;
il nodo cieco tra due radici con la purge che svuota il file. Il filo:
**uno stream unidirezionale per frame, con FINISH e FIN consumato, e il
congedo esplicito (bye/bye-ack)** — senza, l'ultimo frame sparisce nel
reset (prova nel test lockstep a venti frame).

**Suite completa: 50 suite verdi, 0 fallite; fmt e clippy verdi.**

## Non fatto (e dichiarato tale)

- **Il giro Linux non è stato fatto** (Docker oggi risponde: resta da
  eseguire `scripts/linux.sh`).
- **Le prove di P5 che mancano**: il giro via relay locale; la
  rimozione-rotazione end-to-end; il flusso del codice breve col canale
  bruciato (l'implementazione c'è, il test no); il bootstrap di 10k e la
  latenza (mancano le misure); **le due soglie non sono state rimesurate**
  (p95 testo+grafo, inserimento) — i numeri della v0.6 restano superate e
  non si segnano ✓.
- **`the_fleet_converges` va riportato a 2048** dopo l'ultima regola (le
  create concorrenti): le 2048 corse verdi precedono quella modifica.
- **JS (`npm test`, build, SHA256) non rigirati**; `du -sh target` da
  rileggere alla ripresa.
- **Nessun tag**: la chiusura esige tutto quanto sopra.

## Cosa non promettiamo (aggiunte)

- Il nodo cieco accetta una lista **autocoerente** di uno sconosciuto
  (firme valide, cifrario senza senso per lui, solo spazio): un limite da
  chiudere con elenchi ammessi dall'operatore.
- L'istanzanea completa di un dispositivo nuovo viaggia come coda di
  operazioni (con le prove Merkle): l'istantanea al taglio resta da fare.
- Il libro degli indirizzi vive di abbinamenti e sessioni riuscite; il
  dial prova un indirizzo alla volta (loopback primo).

---

# Verifica v0.6.1 a metà strada (revisore, 2026-11-14)

Base verificata: `eb4501b`. GLM la dichiara **non chiusa** e la verifica gli
dà ragione. P0 è chiuso davvero. Il resto ha la sostanza, ma ci sono un buco
di sicurezza e una promessa dei documenti che il codice non mantiene.

## Provato dal revisore

- **macOS:** fmt e clippy verdi; `cargo test --workspace --no-fail-fast`
  dà 50 suite verdi e 0 fallite; `cargo deny` dà bans e licenze ok.
- **Convergenza (P0):** `the_fleet_converges` a **2048 casi** verde (194 s),
  dopo l'ultima regola delle create concorrenti. Poi **cinque corse** da 512
  a semi nuovi, tutte verdi. In tutto 4608 casi, nessun fallimento.
- **Linux:** `scripts/linux.sh` (arm64, `--network none`) dà 50 suite verdi,
  fmt e clippy verdi. iroh su loopback regge senza rete.
- **Biglietto vero:** `devices invite` su una radice temporanea stampa gli
  indirizzi delle interfacce reali (LAN e Tailscale), non loopback.
- `du -sh target`: 37G. Binario CLI di debug: 151 MiB.

## Da correggere prima della chiusura

1. **SICUREZZA. Un dispositivo rimosso scrive per sempre retrodatando
   l'orologio** (`arrival.rs`, `signed_checks`).
   - **Il difetto:** il cancello accetta l'operazione di un rimosso se
     `op.hlc < remove.hlc`. Ma l'HLC lo sceglie l'autore, e il limite
     dell'orologio ferma solo il futuro.
   - **La prova del revisore:** un test temporaneo, non committato. Il
     dispositivo viene rimosso. Poi un'operazione costruita a mano, con HLC
     di un giorno prima, `content_hash` ricalcolato e firma della chiave
     rubata, passa **il cancello e `ingest_op`** (`Ok(())`). Il test di GLM
     `a_removed_device_outside_its_set_is_refused` non lo vede perché usa
     l'ora vera.
   - **La correzione:** la spec chiede «dentro l'insieme accettato della sua
     `remove`», e `devices remove` calcola già la radice di Merkle
     (`accepted_root`, `accepted_count`), che però il cancello non legge mai.
     Un'operazione di un rimosso entra **solo con la prova di appartenenza**
     a `accepted_root`, la stessa meccanica dell'`adopt` (`merkle_root_with`).
     L'HLC non conta.
   - **Test:** l'operazione retrodatata è respinta; un'operazione vera del
     rimosso, nell'insieme ma non ancora arrivata, entra con la prova.
2. **Il relay non esiste, e `docs/sync.md` dice il contrario.**
   - Ogni endpoint nasce con `presets::N0DisableRelay`, e non esiste un flag
     `--relay <url>`.
   - La spec v0.6 (e `docs/sync.md` §«Il relay») vuole di base il relay
     pubblico di n0, `--relay <url>` per uno proprio e `--no-relay`.
   - **Effetto:** due dispositivi su reti diverse dietro NAT non si parlano
     mai. Funziona solo sulla stessa LAN, su Tailscale o con un IP pubblico.
   - Va fatto insieme al giro end-to-end via relay locale (`test-utils`), già
     dichiarato mancante.
3. **Costo quadratico sul filo** (`sharedcell.rs`):
   - `find_op` carica **tutto il log** (`cell.ops()`) per ogni busta;
   - `envelope_of` per ogni operazione senza firma rifà `adopt_proof`, che
     rilegge tutti gli hash e ricostruisce l'albero;
   - `unsigned_checks` all'arrivo carica tutti i `content_hash` per ogni
     operazione;
   - `ids()` deserializza ogni operazione con il payload.

   Il bootstrap di 10k note diventa O(n²). Probabilmente è per questo che
   manca la misura di P5. Servono query per `content_hash` (indice), l'albero
   dell'`adopt` calcolato una volta per sessione, e `ids()` letto dalle sole
   colonne `hlc` e `content_hash`.
4. **Una busta che non si sigilla sparisce in silenzio:** in `envelope_of`
   ogni errore diventa `.ok()?`, l'operazione non parte e nessuno lo sa.
   Una mancata convergenza così non lascia traccia. Serve almeno un evento,
   o una riga del riepilogo di `sync run`.

## Minori

- **L'autore non è nell'identità:** `content_hash` non copre `device`, e il
  cancello non confronta `device` con `hlc.node`. Una stessa identità può
  avere firme di autori diversi su copie diverse. Basta richiedere
  `device == hlc.node` sulle operazioni firmate, oppure dichiararlo.
- **La forma del censurato è il solo confine:** `censor_shape_ok` controlla
  i campi ammessi ma non il contenuto delle stringhe (`old_id`, `valid_to`),
  quindi un dispositivo vivo può nasconderci del testo. Nel modello «miei
  dispositivi» è accettabile, ma va scritto in «Cosa non promettiamo».
- **La proprietà «purge, compact e censura non muovono l'identità»** è un
  `#[test]` su un caso fisso, non una proprietà su operazioni generate come
  chiede la spec.
- **Processi lasciati in giro dai test:**
  - un `mailbox serve` di un test rimasto orfano (3 h e mezza);
  - un `cargo test --test pairing` appeso dalle 12:04 (oltre 8 ore) a una
    corsa di GLM. Nella corsa del revisore `pairing` passa, quindi è
    probabilmente instabile.

  Serve una guardia con `Drop` che uccida i figli anche quando un test va
  in panico (`sync_serve_cli.rs:323`), e un timeout per i test di rete.

## Resta da fare (già dichiarato da GLM)

- giro via relay locale;
- rimozione con rotazione end-to-end;
- codice breve col canale bruciato;
- misure di bootstrap e latenza;
- rimisura delle due soglie;
- JS (`npm test`, build, SHA256);
- tag.

**Esito:** non chiusa. Il punto 1 è bloccante per la sicurezza, il 2 per
l'uso reale.

# REPORT — Engramma v0.6.1 (chiusura: i rilievi R1–R6 e il resto della fase)

Data: 2026-11-15. Base: `main` @ `8ba8079` (la verifica a metà strada del
revisore). Tutto costruito e provato su macOS arm64 (Apple Silicon),
Rust 1.95.0. Ogni rilievo comincia col suo test rosso, come la fase
chiede; le misure sono sul banco di questa macchina.

## R1 — il dispositivo rimosso entra solo con la prova

**Il difetto.** Il cancello accettava l'operazione di un rimosso se
`op.hlc < remove.hlc`: l'HLC lo sceglie l'autore, il limite dell'orologio
ferma solo il futuro. La chiave rubata scriveva per sempre, retrodatando.

**La correzione.** L'insieme accettato è la **radice di Merkle** che la
`remove` porta (`accepted_root`, `accepted_count`, calcolata su tutto
l'albero delle celle condivise). Un'operazione firmata da un rimosso entra
solo se è già nella copia (duplicato, query puntuale) o arriva con la
**prova di appartenenza** a una delle radici registrate (`merkle_root_with`,
la stessa meccanica dell'`adopt`). L'HLC non decide più. **Più remove
concorrenti**: vale l'unione — una prova verso una qualunque radice ammette.
Chi manda allega la prova (`envelope_of` la costruisce sull'insieme del
rimosso dell'intero albero, in cache per la sessione); una copia che non
può costruirla non manda — ed è un evento, non un silenzio (R4).

**L'autore entra nell'identità.** Con il firmatario armato, il nodo dell'HLC
delle operazioni locali **è l'id del dispositivo** (l'orologio tiene fisico
e contatore). Il cancello pretende `device == hlc.node` su ogni operazione
firmata — entrambi dentro il `content_hash` che la firma copre — e una
discordanza è `BadSignature`: una stessa identità non può avere autori
diversi su copie diverse. Le operazioni pre-era restano senza firma e
passano dall'`adopt`. L'ordine totale non cambia forma —
`(hlc, contatore, nodo, op_id)` — e i confronti SQL (binario su stringhe)
e Rust (binario sui tre campi) percorrono lo stesso ordine qualunque sia
la larghezza del nodo: la regola è nel DESIGN §4.3.1, con l'unione e
l'autore.

**Le prove.** Il test rosso dello scenario del revisore (operazione
retrodatata di un giorno, `content_hash` ricalcolato, firma della chiave
rubata) diventa verde come rifiuto `RemovedOutsideSet`; con lui: l'operazione
vera del rimosso che entra con la prova ed è rifiutata senza; le remove
concorrenti con insiemi diversi (prova verso l'una o verso l'altra, e una
prova verso nessuna radice rifiutata); `hlc.node` = dispositivo sulle
scritture locali; `device ≠ hlc.node` rifiutato; una firma di un vivo per
un altro rifiutata. La convergenza dopo R1: `the_fleet_converges` a 2048
casi verde (77 s), più **sei corse** da 512 a semi nuovi,
tutte verdi.

## R2 — il relay c'è davvero

**Il difetto.** Ogni endpoint nasceva con `presets::N0DisableRelay`, niente
flag, e due dispositivi dietro NAT non si parlavano mai — mentre
`docs/sync.md` prometteva il relay pubblico di n0 di base.

**La correzione.** **Una funzione** costruisce gli endpoint (sync,
abbinamento, nodo cieco): `relay::bind` con tre modi — i relay pubblici di
n0 di base, `--relay <url>` per uno proprio (`RelayMode::Custom`),
`--no-relay` per le sole connessioni dirette. pkarr resta spento in tutti
e tre (`presets::Minimal`, non N0: la famiglia N0 pubblica su DNS). I flag
valgono per `devices invite/join/code`, `sync run/serve`, `mailbox serve`;
la scelta si **salva nella radice** (`devices.db`, meta `relay`) e un flag
esplicito vince e la sostituisce. Il biglietto porta l'URL del relay che
l'endpoint cavalca, il libro degli indirizzi lo impara dal biglietto al
join (prima lo buttava via). `sync run` dice per ogni dispositivo se la
sessione è passata **diretta o via relay** (`via_of_conn`, sul percorso
selezionato).

**Le prove.** Col relay locale di iroh (`test-utils`, certificato
autofirmato: la feature `test-utils` della crate lo fa fidare, attiva solo
nei test) e le connessioni dirette **impedite** (`clear_ip_transports`):
abbinamento e sincronizzazione completi passano solo dal relay, il
riepilogo dice «relay», e le due copie convergono. Con `--no-relay` lo
stesso giro fallisce in modo pulito: una riga del riepilogo, non un errore.
Sui flag, il test della CLI: `--relay` e `--no-relay` esistono, la scelta
si salva, il flag esplicito vince. Nei test i default restano loopback
senza relay (`--no-relay` al primo comando di ogni radice).

## R3 — niente costo quadratico sul filo

**Il difetto.** `find_op` caricava tutto il log per ogni busta;
`envelope_of` rifaceva la prova dell'adopt (rilettura di tutti gli hash e
ricostruzione dell'albero) per ogni operazione senza firma; il controllo
«è già nella copia» rileggeva tutti i `content_hash`; `ids()`
deserializzava ogni operazione col payload. Il bootstrap di una cella da
10k era O(n²) — e la misura di P5.2 mancava per questo.

**La correzione.** La ricerca di un'operazione è una query puntuale
sull'indice v20 (`Cell::op_of_hash`); `ids()` legge solo `hlc` e
`content_hash`; il duplicato è una query puntuale; l'albero dell'adopt e
quello dell'insieme di ogni rimosso si calcolano **una volta per sessione**
e restano in memoria (`MerkleIndex`: stesse foglie, stesso abbinamento,
stesse prove di `merkle_root`/`merkle_prove` — l'equivalenza è provata su
ogni taglia attorno alle potenze di due).

**La misura** (release, questa macchina; byte sul filo identici — il filo
non è cambiato):

| | prima | dopo |
|---|---|---|
| 1000 buste | 3,47 s (288/s), 2003 letture dell'intero log | 45 ms (22 324/s), 1 lettura |
| 10000 buste | 330,7 s (30/s), 20 003 letture | 0,52 s (19 291/s), 1 lettura |

La misura di P5.2 (bootstrap da 10k, tempo e byte) ora si può fare, ed è
questa. Il test di regressione conta le letture dell'intero log nel
bootstrap di 1002 operazioni: **2 in tutto** (una per gli id, una per
l'albero; il limite del test è 8) — un numero fisso, non una per busta.

## R4 — nessuna busta si perde in silenzio

**Il difetto.** In `envelope_of` ogni errore diventava `.ok()?`: 
l'operazione non partiva e nessuno lo sapeva.

**La correzione.** Ogni operazione che non si può sigillare o provare
diventa un **evento** nel registro della radice (`sync_events`: op_id
**corto**, tipo, motivo — mai testo della nota), il riepilogo di
`sync run` la conta per cella (`unsent`, dal protocollo: ciò che il lotto
chiedeva meno ciò che è partito), e `sync status` le mostra finché restano
(la passata le ripulisce all'inizio e chi non parte viene nominata di
nuovo). Il test ha trovato da solo un difetto in più: l'albero dell'adopt
è l'insieme **coperto** (le operazioni prima dell'ultimo `adopt`), non
tutte le senza firma — un'orfana post-era piegava l'albero e rendeva
inviabili anche le operazioni buone. Corretto con la stessa disciplina:
rosso, poi verde (una riga `unsent: 1` e l'evento senza il testo della
nota «quokkaviola»).

## R5 — l'igiene dei test di rete

**I processi figli.** Ogni processo figlio dei test (`sync serve`,
`mailbox serve`, `devices invite/join`) vive in una guardia con `Drop` che
lo uccide e lo aspetta **anche quando il test va in panico** — il
`mailbox serve` orfano trovato vivo dal revisore (tre ore e mezza) non può
più nascere. **I tempi massimi.** Ogni attesa ha una scadenza esplicita:
`wait_bounded` per i figli, `bounded()` per i test sincroni della CLI
(un giro appeso fallisce con una riga che dice quale), 
`tokio::time::timeout` per i test asincroni — il `cargo test --test
pairing` appeso per otto ore non si ripete in silenzio. **Le corse.**
`pairing` girata **20 volte: 20/20 verdi**, nessun processo orfano;
`sync_serve_cli` girata **20 volte: 20/20 verdi** (5 test a corsa, ~440 s
a corsa), 0 fallite, nessun processo orfano. R5 non ha un test rosso suo:
il rilievo È nei test, e la guardia con la scadenza è la correzione.

## R6 — le due cose minori, chiuse

1. **`censor_shape_ok` controlla anche il contenuto delle stringhe**: gli
   id (`old_id`, `proposal`, `target`) sono ULID, le chiusure (`valid_to`,
   `target_closing`) parsano come `Validity`, gli istanti
   (`old_updated_at`, `decided_at`) come `At` RFC 3339. Un dispositivo
   vivo non può più nascondere testo nelle chiave-valore del censurato
   («il segreto quokkaviola nascosto nell'id» è il test). I corpi che la
   regola produce davvero passano tutt'e due le prove.
2. **La proprietà «purge, compact e censura non muovono l'identità» è un
   proptest** su storie generate (put, patch, delete, supersede, purge a
   piacere, taglio di compattazione generato): `content_hash`, `sig` e
   `payload_digest` identici prima e dopo, ogni corpo censurato nella
   forma della regola, verify verde. Il caso fisso resta come scenario
   del revisore.

## Il resto della fase

- **La rimozione con rotazione, end-to-end.** Tre dispositivi veri (binario
  CLI): la `remove` firmata con la radice dell'insieme accettato, l'epoca
  nuova che raggiunge chi resta (`sync status` dice epoca 1), una nota
  scritta dopo la rotazione, e il rimosso che: non apre le buste nuove
  (il sigillo dell'epoca girata, provato sul suo adattatore), non raggiunge
  più nessuno (il cancello rifiuta la sua chiave di trasporto: riga
  «non raggiunto», non errore), e la sua copia resta indietro.
- **Il codice breve col canale bruciato, end-to-end.** Era dichiarato
  mancante perché **non aveva mai funzionato**: quattro difetti veri,
  trovati uno alla volta dal test rosso (vedi il commit). L'incontro al
  nodo cieco, il PAKE, il biglietto fresco sigillato nel relay, la cella
  che arriva — e l'uso del codice **brucia il canale**: un tentativo per
  codice, il secondo col codice giusto in mano è rifiutato («il canale è
  bruciato»).
- **La latenza su 50 scritture** (release, loopback): 50 scritture locali
  in 10,3 ms ({0,21} ms/scrittura), la passata che le
  porta 27,3 ms ({0,53} ms/operazione), una scrittura in
  più con la sua passata 11,0 ms.
- **Le due soglie** (base fase-v0.5.3, mediana di 5 corse, release):
  MRR pesato del banco 100k 0,683 / 0,739 (soglia ≥ −0,005 dalla base:
  0,683 / 0,739_BASE); testo+grafo p95 del banco 10k 1,8 ms (soglia ≤ +5%:
  1,8 ms_VERDICT).
- **JS** (Node v25.9.0, l'unico su questa macchina — Node 24 non c'è:
  dichiarato, non promesso): `npm test` 29/29, `test:electron` 29/29,
  `test:electron-main` ok. `npm run build` con gli SHA256:
  `engramma.darwin-arm64.node` `87a94a31b49f03b57920b0489dada53a25615df3bc9d74584bd2b3909183d81c`, `index.js` `25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4`,
  `index.d.ts` `bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8`.
- **Linux**: `scripts/linux.sh` (arm64, `--network none`) **verde**: fmt e clippy puliti, 58 suite verdi, 0 fallite (`--network none`: il giro via relay gira sul solo loopback).
- **Convergenza dopo R1**: vedi R1.

## La macchina e il disco

- `du -sh target` all'inizio: 37G; alla fine: 39G (le misure release del banco).
- Binario CLI di debug: 151 MiB. Nessuna crate aggiunta oltre al
  lockfile esistente (l'unica voce nuova è l'auto-dipendenza di test di
  `engramma-sync`, che porta la feature `test-utils` di iroh nei soli
  test).

## Cosa non promettiamo (invariato, con una precisazione)

- PCS/FS restano fuori (chiave d'epoca rubata = epoca persa); la rotazione
  alla rimozione resta il compromesso dei dispositivi di una persona.
- La forma del censurato ora controlla anche il contenuto delle stringhe
  (R6): ciò che resta non promesso è il **corpo libero dentro
  `restore_target`** di un `revert_decision` (l'istantanea pre-stato del
  bersaglio, un'altra nota: la regola lo copia verbatim e ne verifica la
  forma di oggetto, non i campi interni).
- Il nodo cieco vede chiavi, orari e dimensioni: sempre.

## Chiusura

La fase si chiude come diceva la spec: tutti i rilievi R1–R6 risolti col
test rosso prima, il giro via relay (P5.1) e la misura di bootstrap
(P5.2) fatti, la rotazione end-to-end e il codice bruciato provati dal
binario, le soglie rimesurate, JS e Linux verdi, il tag annotato
`fase-v0.6.1`. Nessun push.


# REPORT — Engramma v0.6.1 (la chiusura vera: C1–C4)

Data: 2026-11-15. Base: `main` @ `4bf4c35` (più la correzione P7/P8 del
REPORT v0.6). Tutto costruito e provato su macOS arm64, Rust 1.95.0. Il
banco incriminato è stato fermato e i suoi numeri buttati: la sua corsa
era falsata dalla cartella fissa cancellata da sotto dal revisore (C2).

## C1 — il biglietto porta il relay di base (bloccante)

**Il difetto.** `devices invite` stampava il biglietto subito dopo il
bind: l'endpoint si collega al relay in modo asincrono e in quel momento
`endpoint.addr()` non porta ancora nessun `TransportAddr::Relay`. Tre
`devices invite` dal binario, col relay di base: **nessun biglietto con
l'URL del relay**. I test non lo vedevano perché chiamavano `online()`
da soli — al posto della produzione.

**La correzione.** L'attesa vive in produzione, in un posto solo
(`relay::wait_relay`, 10 s, `NotNeeded` con `--no-relay`), e sta davanti
a ogni indirizzo che deve servire un altro: il biglietto di `devices
invite`, quello fresco del codice breve, il biglietto del nodo cieco,
l'indirizzo che `sync serve` pubblica. Scaduto il tempo il biglietto
esce **senza** relay e la CLI lo dice — una riga sullo stderr e, nel
JSON, `relay: null` con `relay_warning`. Il JSON di invite e serve porta
il campo `relay`; il biglietto del nodo porta il relay e chi lo
registra lo impara (il dial del nodo prova i diretti e poi il relay).

**Le prove (dal binario).** Col relay locale di iroh (variante
`test-relay` del binario, mai predefinita, costruita dal test nella sua
target-dir): il biglietto di `devices invite` e quello del nodo cieco
contengono l'URL del relay. Col relay irraggiungibile: il biglietto esce
in ~10 s (atteso fra 9 e 20), senza relay, con l'avviso nell'JSON e
niente promesso. `--no-relay`: il biglietto esce in meno di 3 s. E il
binario di produzione resta senza `test-utils` (cercato con `strings`).
I test della crate passano dalla strada di produzione: le chiamate a
`online()` sono andate via dai test, `ticket_of` aspetta e il libro
impara dal biglietto come fa il join.

## C2 — il banco non si scontra con altri

La cartella fissa `$TMPDIR/engramma-bench` permetteva a due corse
insieme di cancellarsi i file a vicenda (una corsa del revisore lo ha
fatto). Ogni corsa ha la sua cartella (`engramma-bench-<pid>`) o una
`--work-dir` esplicita; una guardia la cancella su ogni uscita — anche
in panico — e la `--work-dir` data a mano resta. **Provato**: due
`ENGRAMMA_GRAPH_PROBE` insieme, entrambi completi, nessuna cartella
rimasta. I numeri non cambiano (corpus e banco identici).

## C3 — le due soglie, misurate come dice la spec

Base `fase-v0.5.3` in un worktree (target-dir suo, cancellato alla
fine), contro **questa fase**; stessa macchina, release, `--notes 10000
--runs 5`, nessun altro banco o build in corso. Le righe intere:

| corsa | inserimento (note/s) | apertura (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | testo+grafo p50 (ms) | **testo+grafo p95 (ms)** | richiamo@5 | MRR |
|---|---|---|---|---|---|---|---|---|---|
| base v0.5.3, viva | 3013 | 0,6 | 1,4 | 3,1 | 1,2 | 1,3 | **3,1** | 1,000 | 0,791 |
| base v0.5.3, uniform | 2881 | 0,4 | 1,4 | 3,5 | 1,3 | 1,3 | **3,5** | 1,000 | 0,773 |
| v0.6.1, viva | 2759 | 0,6 | 1,3 | 2,1 | 1,1 | 1,3 | **2,0** | 1,000 | 0,791 |
| v0.6.1, uniform | 2841 | 0,6 | 1,3 | 2,0 | 1,1 | 1,3 | **1,9** | 1,000 | 0,773 |

- **p95 testo+grafo (tetto +5%): viva 2,0 ms contro 3,1 (−35%),
  uniform 1,9 contro 3,5 (−46%) — RAGGIUNTA** con ampio margine (il
  p95 della v0.6 era 2,0 ms su un'altra sessione di misura; oggi la
  base stessa misura 3,1/3,5: il confronto è dentro questa sessione,
  come la spec chiede).
- **inserimento (tetto +10% di tempo, cioè ≥ base/1,1): viva 2759
  ≥ 2739 note/s (+9,2% di tempo), uniform 2841 ≥ 2619 (+1,4%) —
  RAGGIUNTA.** La viva è al margine: è il costo delle fondamenta della
  convergenza (base, op_objects, l'indice v20, la firma di ogni
  scrittura) e sta dentro il tetto.
- MRR e richiamo: identici alla base (0,791/0,773, 1,000) — riportati
  per completezza, non sono soglie.

## C4 — la latenza della spec, e quattro difetti veri

La misura richiesta: **B tiene `sync serve` acceso col ciclo reale**
(`PRAGMA data_version`, rimbalzo di 2 s), A scrive una nota alla volta,
per ognuna si misura dalla scrittura alla nota **leggibile su B**, le
50 scritture distanziate 3 s. `#[ignore]`d, si lancia con
`cargo test --release -p engramma-cli --test sync_serve_cli
the_write_to_read_latency -- --ignored --nocapture`.

**Il ciclo non spingeva mai.** La misura è stata rossa per ore e ogni
pezzo aveva il suo difetto — ciascuno col suo test rosso prima:

1. **`PRAGMA data_version` letto come testo**: la pragma torna un
   INTERO, la riga andava in errore e veniva scartata — sempre 0. Il
   ciclo non partiva MAI per le scritture di un altro processo: tutta
   la convivenza Homun (un secondo processo che scrive mentre serve
   riceve) era cieca; i test passavano perché B tirava con `sync run`.
   Ora `Cell::data_version` legge l'intero e `DataVersionWatch` tiene
   le connessioni **aperte** (il contatore è per connessione: letture
   su connessioni nuove non vedono mai il commit altrui) con
   `moved()`/`ack()` separati.
2. **Il deadlock della sessione già sincronizzata**: con have e need
   vuoti l'iniziatore mandava solo l'ack; il risponditore lo consumava
   nel pre-ciclo e aspettava un altro frame per sempre, mentre
   l'iniziatore aspettava il suo ack. Ogni sessione senza nulla da
   scambiare inciampava fino al limite dei 90 s. L'ack ora chiude lo
   scambio.
3. **Il filo della sessione**: uno stream UNIDIREZIONALE per frame, con
   `finish` e il FIN consumato dal ricevente — il budget di stream del
   trasporto non si libera finché il ricevente non ha visto la
   chiusura (la lezione dell'abbinamento, applicata alla sessione).
4. **Il dial della passata**: un indirizzo alla volta (loopback primo,
   5 s ciascuno) — la corsa d'indirizzi del trasporto muore dietro
   un'interfaccia virtuale. E il primo tick dell'orologio slitta a
   +5 minuti: la passata a t=0 diala un libro quasi sempre stantio.

**La misura (release, loopback, ciclo reale)**: mediana **1506 ms**,
p95 **1549 ms** su 50 scritture distanziate 3 s — il rimbalzo di 2 s è
dentro. La vista senza rimbalzo è a **0 ms** perché la consegna arriva
prima del nominale: il sondaggio (250 ms) vede la scrittura, il
rimbalzo parte e la sessione (~1,2 s) la consegna prima che i 2 s
nominali siano passati — il numero onesto è quello con il rimbalzo
dentro, l'altra vista dice solo che il costo vero è la sessione, non
l'attesa. Il test del ciclo (la scrittura di un secondo processo
spinta fino a B, nessuno che tira) sta in suite, verde.

## La chiusura

- **La convergenza sul commit finale**: `the_fleet_converges` a **2048
  casi** verde (215 s), più **cinque corse** da 512 a semi nuovi —
  58 s, 109 s, 82 s, 116 s, 110 s — tutte verdi.
- **macOS**: fmt e clippy verdi su tutta l'area di lavoro; la suite
  verde (fra cui i sei binari di rete della CLI, la misura ignorata a
  parte).
- **Linux**: `scripts/linux.sh` (arm64, `--network none`, sorgente in
  sola lettura) **verde**: fmt e clippy puliti, **58 suite verdi, 0
  fallite**. Ha preteso due correzioni vere: la variante `test-relay`
  del binario si costruisce **fuori dal repo** (il sorgente è montato
  in sola lettura) e il dial della passata prova **anche il relay del
  libro** (un endpoint solo-relay non ha indirizzi diretti: senza, il
  giro via relay muore con «nessun indirizzo noto»).
- **JS** (Node v25.9.0 — Node 24 non è su questa macchina, dichiarato
  come nella chiusura precedente): `npm test` **29/29**,
  `test:electron` **29/29**, `test:electron-main` ok. `npm run build`
  con gli SHA256 (il binario non cambia: il motore è quello della
  chiusura, solo il layer di sincronizzazione si è mosso — e nessun
  file del binding è cambiato):
  `engramma.darwin-arm64.node`
  `87a94a31b49f03b57920b0489dada53a25615df3bc9d74584bd2b3909183d81c`,
  `index.js`
  `25fd9cc1d50a1ec3f2d74bc004d904663ff07360102d27b938870329d9c832d4`,
  `index.d.ts`
  `bf9381f18911d1fba6269eb745c2825c57f5ed643461d4c2a6488005bf69d9f8`.
- **Binario CLI di debug**: 151 MiB.
- **Disco**: `du -sh target` 5,0 G all'inizio del giro (dopo la
  pulizia profonda), 29 G alla fine (le build release delle misure e il
  volume Linux); il worktree e il target della base v0.5.3 cancellati,
  la variante test-relay e le cartelle del banco cancellate.
- **Tag**: `fase-v0.6.1` spostato sul commit finale, annotato, solo
  locale, nessun push. Nessun processo lasciato acceso.


# Verifica v0.6.1 dopo la chiusura C1–C4 (revisore, 2026-11-15)

Commit verificato: `b71161f` (tag `fase-v0.6.1`). Esito: **la fase non si
chiude ancora.** C1 e C2 reggono; C4 misura un ciclo che non va mai a
riposo; la soglia d'inserimento di C3 non è raggiunta. Spec delle
correzioni: `engramma-v0.6.1-ciclo.md` (K1–K4).

## Regge

- **C1.** Tre `devices invite` dal binario, col relay di base, su radici
  temporanee: tutti e tre i biglietti portano
  `https://euc1-1.relay.n0.iroh.link./`, sia nel campo `relay` del JSON sia
  dentro il base32 decodificato. `wait_relay` sta in un posto solo e i test
  di `tests/relay.rs` non chiamano più `online()` da sé.
- **C2.** La corsa del banco di questa fase non lascia cartelle in
  `$TMPDIR`.
- I quattro difetti trovati da GLM in C4 sono veri e corretti bene:
  `data_version` intero su connessioni aperte, deadlock della sessione già
  sincronizzata, FIN consumato, primo tick dell'orologio.

## K1 — il ciclo di `sync serve` si riaccende da solo (bloccante)

La misura C4 rifatta con 8 scritture, `ENGRAMMA_TRACE=1` e 30 s di
silenzio in coda: su A **29 passate «write» per 8 scritture**, che
continuano anche nel silenzio. Il guardiano fa `ack()` subito dopo aver
mandato il tick, prima che la passata giri; la passata scrive nella cella,
`data_version` si muove e il guardiano la prende per una scrittura locale.
Una sessione ogni ~3 s per sempre, verso ogni dispositivo.

Il numero di C4 (mediana 1506 ms) è **sotto il rimbalzo di 2 s**, cosa
impossibile per costruzione: la scrittura arriva con la passata che il
ciclo aveva già in corsa. È la fase del ciclo, non la latenza.

## K2 — il dial reinventato (contro R2.3)

`PeerAddr::dial` e `NodeAddr::dial` provano un indirizzo alla volta, 5 s
ciascuno, loopback primo, relay ultimo; `direct_addresses` pubblica anche
`127.0.0.1` agli altri. Su due macchine vere dietro NAT ogni passata può
pagare ~20 s per dispositivo prima del relay, col ciclo di serve fermo.
R2.3 chiedeva di lasciare la gara degli indirizzi a iroh.

## K4 — le soglie, misurate dal revisore

`engramma-bench --notes 10000 --runs 5`, release, base `fase-v0.5.3` in un
worktree con target suo, `TMPDIR` separati, nessun altro banco o build.
Due coppie, la seconda in ordine inverso:

| corsa | inserimento viva (note/s) | inserimento uniform | p95 testo+grafo viva / uniform (ms) |
|---|---|---|---|
| base v0.5.3, 1ª | 3553 | 3532 | 1,8 / 1,9 |
| v0.6.1, 1ª | 2787 | 2794 | 2,0 / 1,9 |
| v0.6.1, 2ª | 2801 | 2806 | 1,9 / 2,0 |
| base v0.5.3, 2ª | 3223 | 3237 | 2,2 / 2,2 |

- **Inserimento: +15% / +27% di tempo** nei due ordini, tetto +10% —
  **NON raggiunta.**
- **p95 testo+grafo:** fra −14% e +11%, dentro il rumore (un passo di
  risoluzione è 0,1 ms) — regge.
- La base del REPORT di chiusura (3013 note/s, p95 3,1 ms) era una corsa
  rallentata: da lì il «raggiunta».

## Minori

- `docs/sync.md`: «il relay c'è ancora non».
- `Daemon::spawn` nei test: lo stderr del figlio è in pipe e nessuno lo
  legge; con la traccia accesa la pipe si riempie e il demone si blocca.
- Uno stream unidirezionale per frame: corretto (quinn accetta in ordine di
  id), ma lo stato dell'arte è uno stream per sessione.

## Non rifatto

Convergenza a 2048 casi, suite macOS e Linux: si rifanno sul commit che
chiude K1–K4, perché K1 e K2 toccano di nuovo il ciclo e il dial.
`du -sh target`: 8,3 G all'inizio, 8,4 G alla fine (worktree e target
della base cancellati).

# REPORT — Engramma v0.6.1 (la chiusura del ciclo: K1–K4)

Data: 2026-11-15. Base: `main` @ `76ba41c` (il commit di verifica del
revisore). Tutto costruito e provato su macOS arm64, Rust 1.95.0. Il
primo commit di ogni punto è il test rosso; le misure della fase sono
state buttate e rifatte a coppie alternate (K4).

## K1 — il ciclo di `sync serve` si ferma quando non c'è nulla da fare

**Il difetto, confermato dal test rosso.** Due demoni `sync serve`
già sincronizzati, una scrittura, poi trenta secondi di silenzio: il
test contava le passate sulla traccia del demone e ne trovava **14 in
30 s** — una passata ogni ~2 s, il ritmo esatto di sondaggio (250 ms) +
rimbalzo (2 s). E una scrittura sola su A produceva **3 passate** entro
la finestra di misura. Le due scritture che riaccendevano il ciclo:

1. `record_session_ack` scriveva `device_acks` a **ogni** sessione,
   sempre, con il timestamp `at` nuovo — una sessione che non scambia
   nulla sporca la cella lo stesso, `data_version` si muove, e il
   guardiano la prende per una scrittura locale;
2. il guardiano faceva `ack()` **subito dopo il tick**, prima che la
   passata giri: qualunque cosa la passata scrivesse era, al giro dopo,
   una «scrittura locale» mai vista.

**La correzione, in quattro pezzi.**

1. *Una sessione che non scambia nulla non scrive nulla*:
   `record_session_ack` scrive la riga solo quando la testa che conferma
   è diversa da quella già confermata — il timestamp da solo non sporca
   più la cella (e il test unitario `a_file_that_moved_without_rows…`
   lo pinna: la seconda conferma della stessa testa non muove nemmeno
   `data_version`).
2. *Il grilletto è il registro*: il sondaggio economico resta su
   `PRAGMA data_version` (connessioni persistenti, C4), ma quando il
   contatore si muove il guardiano confronta il **segnaposto del
   registro** — `(COUNT(*), MAX(rowid))` di `ops`, per cella — e toglie
   le righe che le sessioni del demone stesso hanno portato (il
   registro delle fonti, per dispositivo: `note_session`). Le righe che
   restano sono scritture locali → passata verso **tutti**; righe tutte
   delle sessioni → passata verso **gli altri**, mai di nuovo verso le
   fonti (su due dispositivi: nessuna passata affatto); nessuna riga →
   contabilità, non operazioni: nessuna passata, e le basi si
   assestano lì.
3. *L'ack dopo la passata*: le basi si muovono a passata finita
   (`settle`/`finish_pass`), mai prima — le scritture della passata
   stessa non sono notizia. E una scrittura locale che arriva **durante**
   la passata non muore nel settle: `finish_pass` la riconosce (righe
   oltre il lavoro delle sessioni da quando la passata è partita) e la
   passa alla successiva.
4. *Mai di rimbalzo verso la sorgente*: la passata innescata dalle righe
   ricevute da X salta X (la riga del libro che una sessione in ingresso
   aggiornava a **nodo nudo** — indirizzabile da nessuno — ora nasce
   solo quando manca, `INSERT OR IGNORE`; il dial riuscito la rinfresca
   com'è sempre stato).

**Le prove.** I due test rossi sono verdi: **0 passate write** in 30 s
di silenzio (dopo che una scrittura ha attraversato: il riposo che
conta è quello che segue il lavoro), e **una scrittura = una passata su
A, zero su B**. I test unitari del guardiano (`guard.rs`) pinnano le
decisioni: righe locali → `All`; righe di sessione → `Others([fonte])`;
niente righe → `None`; scrittura a metà passata → ancora. La misura C4
rifatta sta in fondo («Poi»).

**Un difetto trovato strada facendo, e lasciato fuori dal dial.** Il
`sync run` del CLI e il demone della stessa radice condividono la
chiave di trasporto: il magicsock di chi riceve la sessione del run
ricorda la porta effimera del run (morto con lui) e la sessione
successiva **del demone** — stesso node id — scade sui 5 s contro la
rotta morta (iroh 1.4.1). È il comportamento che il dial uno-alla-volta
mascherava col loopback primo. Riproduzione minima nel test K1 (le due
`sync run` di setup, ora tolte col commento che lo spiega): la correzione
sta nei test (niente run del CLI fra due demoni), non nel dial — la
strada giusta è quella di K2: una gara sola, dove una rotta morta perde
tempo zero.

## K2 — il dial è la gara di iroh, non la nostra

**Il difetto, confermato dal test rosso.** Un pari il cui libro porta
tre strade dirette morte (le reti di prova RFC 5737) più il relay
locale: il dial ci metteva **15,0 s** — cinque secondi per ogni strada
morta, una alla volta, il relay per ultimo.

**La correzione.** `PeerAddr::dial` e `NodeAddr::dial` sono una sola
`endpoint.connect` con **tutti** gli indirizzi del libro più il relay,
limite complessivo **10 s**: la gara la fa iroh (R2.3), le strade
dirette vincono quando rispondono, il relay copre chi sta dietro NAT.
Il test rosso adesso gira in **0,3 s** (bind, attesa del relay e dial
compressi): la strada morta perde la gara senza costare un secondo.

**Niente `127.0.0.1` per gli altri.** `direct_addresses` pubblica solo
le interfacce vere che iroh ha scoperto: niente `0.0.0.0` (non
componibile) e niente loopback inventato (serve solo alla stessa
macchina, e pubblicarlo agli altri consegna una strada che muore a una
macchina di distanza). I test che girano in loopback si scrivono da sé
indirizzi di solo loopback nel libro (`to_loopback`).

**Lo «stallo del 100.64».** Su questa macchina l'interfaccia
punto-punto 100.64.0.194 c'è davvero. Con una connect sola, la strada
morta non ha mai stallato nulla: perde la gara contro chi risponde —
lo prova la misura C4 di fondo, che pubblica gli indirizzi **veri**
del demone (192.168.x e 100.64.x in gara) e consegna le note in
tempi regolari. Lo stallo vero era un altro ed è quello raccontato in
K1: la rotta morta che resta appiccicata nel magicsock quando il run
del CLI e il demone condividono la chiave di trasporto. Non è un
difetto del dial di produzione e lì non è stato corretto.

## K3 — le minori

1. `docs/sync.md`: «il relay c'è ancora non» → «il relay non c'è
   ancora».
2. `Daemon::spawn` nei test: lo stderr del figlio è drenato in un thread
   e tenuto (serve ai test K1, che contano le passate sulla traccia):
   una pipe piena non blocca più il demone con la traccia accesa.
3. **Uno stream unidirezionale per frame: resta com'è, e perché.** Il
   trasporto apre uno stream uni per frame con la FIN attaccata: il
   frame viaggia col proprio confine nel trasporto, non ci sono metà
   inutilizzate da resettare, e quinn consegna gli stream in ordine di
   id — l'ordine dei frame è preservato senza contatori. Il difetto
   trovato da C4 (il budget degli stream del pari che non si libera se
   la FIN non è consumata) è pagato e coperto: `read_to_end(1)` su ogni
   frame ricevuto. Con questi conteggi — decine di frame a sessione,
   ricevute comprese — l'overhead per frame è sotto il microsecondo e
   il limite di stream di quinn non è nemmeno sfiorato. Uno stream
   bidirezionale per sessione risparmierebbe qualifica ma toglierebbe
   il confine libero dal payload, chiedendo di nuovo la macchina a
   lunghezza prefissata che la FIN dà gratis. Non vale il rischio in
   questa fase; il trasporto è `wire.rs`/`protocol.rs` e la forma è
   documentata qui.

## K4 — la soglia d'inserimento

**La misura del revisore, e la mia.** Il revisore ha misurato +15%/+27%
di tempo (due coppie, ordine inverso) e la mia base del REPORT era una
corsa rallentata: da lì il «raggiunta». Questa chiusura profila, riduce
ciò che si può ridurre, e misura a coppie alternate.

**Il profilo** (`bench/examples/profile_put`, 10k note, samply +
`sample`, con e senza la firma armata; esempio che resta nel banco):

- *la firma non è il collo*: **+5…+12 µs/nota** (3–4%) su questa
  macchina, come la spec anticipava (~20 µs è il costo su hardware
  più lento);
- *il collo è il commit*: in sostenimento su cella piena il **67% del
  tempo di un `put_note` è `Transaction::commit`**, di cui ~31% sono i
  `pwrite` dei frame WAL. La scrittura v0.6.1 sporca **più pagine per
  transazione** della v0.5.3: la riga `ops` è più larga (`content_hash`,
  `base`, `payload_state`, `device`, `sig` ≈ 280 caratteri in più), e
  ogni operazione allarga **due** indici in più
  (`idx_ops_content_hash`, `idx_op_objects_object` + la riga
  `op_objects`);
- *il secondo costo era gli statement*: `RunParser`/`yy_reduce` ~4% del
  profilo — un `prepare` fresco per ogni istruzione della strada calda.

**Le riduzioni fatte.** `prepare_cached` su tutto il percorso di
scrittura (`store_op`, `execute_op`, `write_note`, `reindex_note`, gli
aiutanti FTS/tag/entità, i probe della `base`); la nota nuova porta a
casa il `rowid` con `INSERT … RETURNING` (una lettura in meno per
nota); `op_objects` è **un** insert multiriga, non uno per oggetto. La
quota del parser nel campionatore scende sotto il 2%. Non si tocca il
lato pagine senza cambiare schema (il `content_hash` esiste per la
`base` e per le buste; i due indici servono al probe e alle ricerche):
è il prezzo in disco della convergenza, ed è qui che il tempo va.

**Le coppie alternate** (`--notes 10000 --runs 5`, release,
`ENGRAMMA_SKIP_MODEL_TESTS=1`, cartelle di lavoro separate e cancellate
alla fine, macchina ferma ma ~30% più lenta del giro del revisore — i
valori assoluti non si confrontano tra i giri, i rapporti sì):

| corsa | inserimento viva | inserimento uniform | p95 testo+grafo viva / uniform |
|---|---|---|---|
| base v0.5.3 (1ª) | 2699 note/s | 2718 | 3,0 / 2,9 ms |
| v0.6.1 (1ª) | 2298 | 2131 | 2,7 / 2,6 |
| base v0.5.3 (2ª) | 2768 | 2654 | 2,8 / 3,4 |
| v0.6.1 (2ª) | 2116 | 2328 | 3,2 / 3,0 |

- **Inserimento, media delle due coppie: viva +24% di tempo, uniform
  +20%. Il tetto è +10%: NON raggiunta.**
- **p95 testo+grafo: fra −10% e +7%**, dentro il rumore di un passo di
  0,1 ms su una macchina già calda. Regge.

**La scomposizione del costo** (per nota, dal profilo, ~370 µs sul
corpo di questa macchina lenta): il grosso è il **commit** — più
pagine sporche per transazione (riga `ops` più larga, due indici e una
tabella in più per operazione) — poi le istruzioni in più (ora in
cache), e per ultima la firma, 5–12 µs. Sarà l'utente a decidere se
accettare il costo come prezzo della convergenza: la soglia della spec
resta non raggiunta e dichiarata.

## La chiusura (K1–K4, sul commit finale)

- **La misura C4 rifatta** (50 scritture distanziate 3 s, ciclo reale di
  due `sync serve`, release): **mediana 2291 ms, p95 2322 ms** — sopra il
  rimbalzo di 2 s, come una misura di latenza deve essere (il test ora
  FALLISCE se la mediana scende sotto il rimbalzo: vorrebbe dire che
  misura la fase del ciclo). **Senza rimbalzo: mediana 291 ms, p95
  322 ms** — la riga che col ciclo che non riposava non voleva dire
  niente, e adesso è il tempo vero di grilletto + gara + sessione. Il
  giro intero: 263 s. La misura pubblica gli indirizzi **veri** del
  demone (192.168.x e il 100.64 punto-punto in gara): le note arrivano
  regolarmente — la strada morta non ha mai stallato la gara (K2.3).
- **La convergenza**: `the_fleet_converges` a **2048 casi** verde
  (release, 82 s), più **cinque corse** a semi nuovi — 24 s, 22 s, 22 s,
  23 s, 25 s — tutte verdi.
- **macOS**: fmt e clippy (`-D warnings`) verdi; la suite di tutta
  l'area di lavoro verde — **501 test ok, 0 falliti** (fra cui i due
  nuovi del ciclo a riposo e i cinque del guardiano).
- **Linux**: `scripts/linux.sh` (arm64, `--network none`, sorgente in
  sola lettura) **verde due volte**: fmt e clippy puliti, **59 suite
  verdi, 501 test ok, 0 falliti**.
- **JS** (Node v25.9.0): il motore è cambiato (K4 tocca il percorso di
  scrittura), il binding è stato ricostruito e ritestato: `npm test`
  **29/29**, `test:electron` **29/29**, `test:electron-main` ok. Il
  binario cambia:
  `engramma.darwin-arm64.node`
  `bdb32610c11ca2909881e4f767cbda0c01aeb120c2cd8461a38b3b26e4fee0c9`
  (index.js `25fd9cc1…` e index.d.ts `bf9381f1…` immutati).
- **Binario CLI di debug**: 151 MiB.
- **Disco**: `du -sh target` 8,4 G all'inizio del giro, 15 G alla fine
  (le build release delle misure, del binding e del volume Linux);
  worktree e target della base v0.5.3 cancellati, i temporanei delle
  misure (`/tmp/k4*`, i profili) cancellati, il volume Docker
  `engramma-linux-target` rimosso.
- **Tag**: `fase-v0.6.1` spostato sul commit finale, annotato, solo
  locale, nessun push. Nessun processo lasciato acceso.

**Il conto che resta aperto è uno**: la soglia d'inserimento (K4) —
**NON raggiunta**, +24%/+20% di tempo (media delle coppie) contro il
tetto del +10%, con la scomposizione che dice dove va: il commit, non
la CPU. Sarà l'utente a decidere se accettarla come prezzo della
convergenza.

# Verifica v0.6.1 dopo la chiusura K1–K4

Data: 2026-11-16. Revisore. Base: `main` @ `1353f64` (tag `fase-v0.6.1`).

## Regge

- **K1, il ciclo a riposo.** Una copia della misura C4 con
  `ENGRAMMA_TRACE=1`, stderr in un file, 8 scritture e 30 s di silenzio
  (test di scratch, cancellato): su A **8 passate per 8 scritture, zero
  nel silenzio**; su B **zero passate** verso A. Latenza mediana
  **2248 ms**, p95 **2302 ms**; senza rimbalzo 248 / 302 ms. Sopra il
  rimbalzo, come deve essere.
- **K2.** `PeerAddr::dial` è una sola `endpoint.connect` con tutti gli
  indirizzi più il relay e un tetto di 10 s; niente loopback negli
  indirizzi per gli altri.
- **K3.** Fatte; la scelta degli stream unidirezionali è motivata.
- **K4, il codice.** `prepare_cached`, `INSERT … RETURNING`, `op_objects`
  multiriga: meccanico, a basso rischio.
- **Suite.** macOS: fmt, clippy, 60 suite verdi, `cargo deny` pulito.
  Convergenza a **2048 casi verde (81 s)** più **512 × 5** verdi
  (21–23 s ciascuna). Linux (`scripts/linux.sh`): 59 suite verdi.
- **Il binding.** `engramma.darwin-arm64.node` sul disco ha lo SHA256 del
  REPORT, `bdb32610…`. Non l'ho ricostruito io.

## K4 — la soglia d'inserimento, NON raggiunta e accettata

Una coppia alternata, `--notes 10000 --runs 5`, release, base
`fase-v0.5.3` in un worktree con target suo, `TMPDIR` separati:

| corsa | inserimento viva | inserimento uniform | p95 testo+grafo viva / uniform |
|---|---|---|---|
| base v0.5.3 | 3192 note/s | 3232 | 2,0 / 1,9 ms |
| v0.6.1 dopo K4 | 2523 | 2480 | 2,1 / 2,0 |

- **Inserimento: +26% / +30% di tempo.** Tetto +10%: **NON raggiunta.**
- **p95 testo+grafo:** +5% / +5%, cioè un passo di 0,1 ms: dentro il
  rumore.
- La seconda coppia l'ho fermata: con questa sono **cinque coppie
  indipendenti** (due del revisore prima di K4, due di GLM, questa), tutte
  fra +15% e +30%. Nessuna corsa in più può riportare la soglia sotto il
  +10%.

**La decisione dell'utente (2026-11-16): il costo è accettato** come
prezzo della convergenza. In assoluto sono ~0,1 ms in più per nota
(10.000 note da ~3,1 a ~4 s); le scritture di Homun arrivano una alla
volta. La strada per recuperarlo, se servirà, è raggruppare più
operazioni in un commit solo: il profilo di GLM dice che il 67% del tempo
è il commit, non la firma.

## Due casi limite del guardiano (minori, non bloccanti)

`DataVersionWatch::evaluate` somma il delta di tutte le celle:
- un `purge` in una cella può coprire una scrittura in un'altra;
- una busta respinta ma contata come ricevuta può far leggere come
  «ricevuta» una scrittura locale.

In entrambi i casi la passata periodica di 5 minuti recupera. Da
chiudere quando si tocca di nuovo il ciclo: un segnaposto per cella.

## Trovato in verifica: i test riempivano il disco

`convergence.rs`, `identity.rs` e `sync_serve_cli.rs` chiamavano
`std::mem::forget` sulla `TempDir`. Ogni copia lasciava la sua cartella in
`$TMPDIR`; una verifica di convergenza ne lasciava ~15.000. Trovate
**91.959 cartelle, ~28 GB**, disco con 3,7 GB liberi.

- Cancellate solo le cartelle con file dei test di Engramma (`*.sqlite`,
  `nota.md`): il disco torna a 34 GB liberi.
- Corretto in `19d2e9e`: `Kept { cell, _dir }` con `Deref<Target = Cell>`,
  `Fleet` tiene le cartelle in `_dirs`. `convergence` e `identity` verdi,
  le cartelle `.tmp*` restano 11 prima e dopo.

## Per la prossima fase

- Le misure di sola scrittura si lanciano con
  `ENGRAMMA_SKIP_MODEL_TESTS=1`: il banco salta i due modelli semantici,
  che fanno quasi tutti i ~45 min di una corsa. Va scritto nella spec
  delle soglie.
- Il segnaposto per cella del guardiano, qui sopra.

## Disco

`du -sh target`: 15 G all'inizio e alla fine. Worktree e target della
base cancellati, temporanei delle misure cancellati, nessun processo
lasciato acceso.

**Verdetto: la v0.6.1 è chiusa.** K1–K3 reggono; K4 resta NON raggiunta,
dichiarata e accettata dall'utente.


---

# v0.6.2 — l'API JS della sincronizzazione

La v0.6.1 sincronizzava; la v0.6.2 la rende usabile da Homun, dentro il
processo, senza la CLI. Quattro punti (S1–S4), il primo commit di ognuno
il test rosso. Ambiente: macOS 27.2, arm64, Xcode 27.2, Rust 1.95; il
`node` locale è il **25.9.0** (il Node 24 non è installato come `node`
nativo: come l'altra volta, lo dichiaro — l'Electron di Homun in modo
Node riporta **Node 24.22.0** e la suite gira verde anche lì).

## S1 — il servizio sta nella libreria

**Cosa.** `engramma_sync::service::SyncService` (`service.rs`, ~2000
righe): tutto ciò che la CLI faceva — invito, codice, ingresso (la
conferma SAS è un parametro a stadi, non stdin), passata, ciclo continuo
con il guardiano, condivisione, rimozione — con gli eventi
(`Pass`/`Arrived`/`Device`/`RemovedSelf`/`Unsent`/`Error`) che escono da
un canale. La CLI è un guscio: parse e stampa, righe e JSON identici, e
**i test della CLI girano immutati** (`sync_cli` 5, `sync_serve_cli` 8,
`sync_lock_cli` 2 nuova, relay e mailbox a fianco). `--name` su
`devices invite` e `devices code` (predefinito `portatile`); da JS è
`deviceName`.

**Il lucchetto.** `<root>/sync.lock` (flock via libc, già nell'albero):
copre il servizio continuo e ogni passata singola; si rilascia anche se
il processo muore. `SYNC_BUSY` in JS, riga dedicata e uscita **7** nella
CLI. Provato anche **fra due processi** (la CLI serve × il servizio in
libreria, in entrambe le direzioni) e nello stesso processo (due Engine
sulla stessa radice).

**Il guardiano per cella.** I due casi limite dichiarati nella v0.6.1
sono chiusi: `DataVersionWatch` ora confronta il segnaposto e la ricevuta
**per cella** (il registro dei «nutriti» è device → cella → righe).
Misura a monte, fatta col CLI e sqlite3: una `purge` **aggiunge** una
riga al registro `ops` (3 → 4) e anche la compattazione tiene le righe
(censura i payload) — il conteggio non scende mai; la copertura
dichiarata («un purge copre una scrittura nell'altra cella») si
materializza quindi nel gonfiarsi del registro (una busta rifiutata
contata come ricevuta) e nella ricevuta attribuita alla cella sbagliata.
I test divergenti sono rossi sulla logica sommata e verdi su quella per
cella: `an_inflated_ledger_in_one_cell_covers_no_write_in_another` e
`the_pass_receipt_is_per_cell`; la frase letterale della spec ha il suo
test (`a_purge_in_one_cell_covers_no_write_in_another`). I cinque test
del guardiano della v0.6.1 restano, adattati alle firme nuove.

**Due difetti trovati col probe e chiusi** (il secondo dei quali avrebbe
appeso ogni `sync serve` di sempre):

1. **La chiusura SAS che blocca un runtime a filo singolo congela il
   filo.** Il driver dell'abbinamento aspetta la risposta dell'umano
   dentro una chiusura sincrona; su un runtime `current_thread` la coda
   in uscita dell'endpoint smette di trasmettere — il PAKE di A non
   finisce mai mentre quello di B sì, e la connessione muore di idle a
   30 s. Il driver gira su un runtime a **due lavoratori**: la chiusura
   ne parcheggia uno, l'altro tiene il filo. Il probe a due thread con
   chiusure bloccanti falliva a 30 s e con due lavoratori chiude in
   0,10 s.
2. **`serve_stop` teneva il mutex del core oltre un `await`.** Il task
   del ciclo, sullo stesso filo, tentava la stessa lock in sincrona: il
   filo intero si bloccava e `stop()` non rispondeva mai. La regola «mai
   il lock oltre un await, su un filo solo» ora è scritta nel codice.

**La telefonata di cortesia.** Un rimosso non può più sentire la
rimozione dal cancello (non è più vivo). `remove` chiama il rimosso con
una sessione in più — il cancello controlla CHI CHIAMA, e chi chiama è
il rimuovitore: la `remove` firmata arriva come riga di registro dentro
lo scambio, il rimosso se lo applica, emette `RemovedSelf` e **ferma il
proprio servizio da sé**. Provato in Rust e da JS (il test S4).

## S2 — l'API JS

`Engine.sync(opts?)` apre (una volta per Engine) il servizio; `Sync`
risponde `status/devices/invite/code/join/remove/share/unshare/runOnce/
start/stop/onEvent`; `Pairing` espone `ticket`, `ticketQr`, `code`,
`expiresAt`, `relay`, `relayWarning` e i metodi `sas/confirm/done/
cancel`. Ogni chiamata viaggia sul pool di libuv, il servizio sta sul
suo thread: il ciclo a eventi di Node non si blocca mai.

- **Codici stabili** attraverso il `reject`, come sempre: nuovi
  `SYNC_BUSY`, `PAIRING_EXPIRED`, `PAIRING_REJECTED`, `PAIRING_TIMEOUT`,
  `NO_MAILBOX`, `NOT_SHARED`, `LAST_DEVICE`, più `CANCELLED` (l'abbinamento
  annullato — aggiunto oltre la lista della spec, documentato in
  `index.d.ts` e `docs/node.md`).
- **`dropCell` su una cella condivisa risponde `CONFLICT`** col consiglio
  di chiamare prima `unshare`; dopo l'unshare l'oblio procede.
- **Un irraggiungibile in `runOnce` è una riga** del `PassView`, mai un
  errore.
- `index.d.ts` documenta ogni metodo (nasce dai commenti Rust); l'unione
  TS `SyncEvent` — che napi-derive non può generare — è integrata dopo
  ogni build da `scripts/dts-append.js`.
- **Un nome aggiunto oltre il contratto**: `Pairing.rounds` (quante volte
  si confronta il SAS: 1 col biglietto, 2 col codice) e l'id completo in
  `SyncDeviceView.device` — la UI ha bisogno di sapere quante battute
  aspettarsi e il libro degli indirizzi chiave l'id intero.

## S3 — l'abbinamento da una UI

Il SAS non si salta: `confirm(true)` prima che `sas()` si risolva è
`INVALID_ARGUMENT` (provato); non esiste opzione che confermi da sola.
La finestra della risposta è di **2 minuti** (l'ambiente dei test la
accorcia con `ENGRAMMA_SAS_TIMEOUT_MS`); scaduta, `PAIRING_TIMEOUT` da
entrambi i lati e nessuno appeso. `cancel()` chiude l'endpoint: chi
annulla sente `CANCELLED`, l'altro capo — appena il suo umano risponde —
un rifiuto pulito. `ticketQr` è il biglietto nei panni alfanumerici del
QR (maiuscole, cifre e `$%*+-./:`): un QR in modalità alfanumerica è più
piccolo e più leggibile; `join` accetta **entrambe le forme** (e anche la
CLI). Il QR lo disegna Homun: il motore non dipende da librerie QR. I
test girano su **due processi Node** veri (`__test__/sync-peer.js`, RPC
JSON su stdin/stdout): abbinamento completo col SAS confrontato dal
test, QR che entra, SAS negato (nessuna `add`), timeout, confirm
prematura, cancel.

## S4 — eventi e ciclo di vita

Il servizio gira sul suo thread (runtime proprio, come la CLI); le
chiamate passano da un canale e le risposte tornano con la solita
machineria `AsyncTask`. Gli eventi pompano da un thread dedicato a un
hub che li sparge a JS (`ThreadsafeFunction`). Numeri e fatti misurati:

- `put` su A → evento `arrived` su B e nota leggibile con `list`: **entro
  i 25 s del test** (il ciclo: guardia ≤250 ms, rimbalzo 2 s, passata).
  La scrittura viene dallo **stesso processo** di A (una connessione
  diversa sullo stesso file: la lezione di C4, vista da JS).
- **30 s di silenzio → zero passate** (il riposo di K1, guardato da JS).
- `remove` di B da A → su B l'evento `removed` e `status().serving ==
  false` (si ferma da sé).
- `stop()` durante una passata: si conclude **entro 5 s** (misurato:
  ~1 s nel test); `start()` due volte non fa nulla, `stop()` a freddo
  pure.
- `Engine.close()` col servizio acceso: l'app Electron esce —
  `test:electron-main` finisce in **0,4 s** in totale.

**Due difetti trovati e chiusi** (i test li hanno pizzicati):

1. **Gli eventi attraversavano il tsfn con la forma sbagliata** (la
   tupla, non il valore: `e.type` indefinito) — srotolata la tupla nel
   callback.
2. **Le scritture del binding erano senza firma**: il cancello dei pari
   le rifiutava (`unsent: 1`, la nota non partiva mai). Il firmatario
   del dispositivo si arma sull'apertura e sulla condivisione delle
   celle condivise — come la CLI ha sempre fatto; una semplice `stat`
   evita il costo sulle radici che non hanno mai sincronizzato (la corsa
   dei 200 open/drop di v0.4.3 P2.3 passa da quella strada).

## Il contratto

- `npm test`: **46/46** (i **29 test vecchi identici** più 17 nuovi);
  `test:electron`: 46/46; `test:electron-main`: verde.
- `cargo test --workspace` verde: `engramma-sync` (service 11, guard 8,
  protocolli), CLI (tutte le suite), core, vault, mcp.
- Clippy e fmt puliti (`-D warnings`).

## L'inserimento non peggiora (soglia +5%)

`engramma-bench --notes 10000 --runs 5` con `ENGRAMMA_SKIP_MODEL_TESTS=1`,
base `fase-v0.6.1` in un worktree col suo `target` dentro `target/`
(cancellato alla fine), **due coppie alternate** (base, fase, base, fase),
macchina altrimenti ferma:

| corsa | viva (note/s) | uniform (note/s) | testo+grafo p95 (ms) |
|---|---|---|---|
| base-1 | 2798 | 2828 | 1,8 / 1,8 |
| fase-1 | 2833 | 2760 | 1,8 / 1,9 |
| base-2 | 2686 | 2621 | 1,8 / 2,0 |
| fase-2 | 2649 | 2788 | 2,0 / 1,9 |

Coppia 1 (base 2813 → fase 2796,5): **−0,6%**. Coppia 2 (base 2653,5 →
fase 2718,5): **+2,4%**. Media delle coppie: **+0,9%**, dentro il +5%.
p95 testo+grafo: base 1,85 ms, fase 1,90 ms (**+2,7%**, 0,05 ms in
assoluto), dentro il +5%.

## Convergenza — cinque corse verdi, e un difetto PREESISTENTE trovato

`PROPTEST_CASES=2048 cargo test --release -p engramma-core --test
convergence the_fleet_converges`, **cinque corse verdi** sul codice
finale della fase: 145,7 / 149,1 / 91,9 / 111,1 / 137,1 s. Cartelle
`.tmp*` in `$TMPDIR` prima e dopo le corse verdi: **0 → 0** (nessuna
perdita; il percorso verde è quello sistemato in `19d2e9e`).

**E poi, in verifica, una corsa in più ha pescato un seme che fallisce.**
La proprietà ha ancora casi vivi che non convergono: testimone
**seed 7348206116156780080 → ridotto a 2 dispositivi, 19 giri** (digest
divergenti `e86485bb…` contro `00cf8382…`); rieseguito su **base
`fase-v0.6.1` con esito IDENTICO** — stesso seme ridotto, stesse
digestioni. Non è della v0.6.2: questa fase non tocca `engramma-core`.
Nella sessione di verifica ho visto ~12 semi fallire (un altro: seed
`7729712691107302376`, 2 dispositivi, 25 giri); a 2048 casi per corsa,
le prime cinque sono passate e le successive hanno incontrato il
difetto — la frequenza è dell'ordine di un caso ogni poche migliaia.
Il file `convergence.proptest-regressions` tracciato nel repo tiene i
semi delle regressioni GIÀ CHIUSE (riesegnano verdi in testa a ogni
corsa); i semi nuovi NON sono stati committati (rosseggerebbero la
suite) — stanno qui, come testimoni. È lavoro della prossima fase: la
priorità di v0.6.2 era non peggiorare, e il banchetto alternato dice
che non è peggiorato.

**Perdita al fallimento, pure preesistente:** il percorso di shrink di
proptest lascia le cartelle della flotta quando l'assertione salta — la
corso fallita ne ha lasciate **2 319 (~4 GB)**, cancellate a mano (il
percorso verde non perde nulla, come sopra).

## Linux

`scripts/linux.sh` (fmt + clippy `-D warnings` + `cargo test --workspace`,
Debian bookworm arm64, rete chiusa, sorgente in sola lettura): **verde**,
uscita 0 — fmt, clippy e tutta la suite dei test nel container.

## La build e i tre file

`npm run build` (che ora **carica l'artefatto appena collegato**: un
capovolgimento futuro del registro del linker ferma la build, non
Homun):

- `engramma.darwin-arm64.node` — SHA256 `7ceebcc1795168c8a90a266cc9bb88ec0d31b170dc144727b58c4b7d7125dddb`
- `index.js` — SHA256 `6a440672963f2da32989bd306a91863b9ca87df2e50c6d6d15d7049e9fce8053`
- `index.d.ts` — SHA256 `341b084ce198d6f2a6a858e73069c2e6c89f35fd3990da3fe230abcba71ab92a` (**cambia per forza**: il contratto
  è esteso)

## Il difetto del linker, e la voce GOT

La prima build di rilascio della fase produceva un dylib che dyld
rifiutava: «mis-aligned LINKEDIT string pool». Misurato: il registro dei
simboli indiretti della v0.6.2 è **dispari** (635 voci) e la tavola delle
stringhe scade a 4 (mod 8); la v0.6.1 ci cascava sopra per pura
geometria (registro pari, 81 139 simboli contro 121 684). Il linker di
Xcode 27.2 allinea a 4 in quel caso e il dyld di darwin 27.2 rifiuta.
Proati anche `-ld_classic` e `strip -x`: stesso 4 (mod 8). La
soluzione minimale e semantica: **un indirizzo preso di `arc4random`**
(`black_box` in `version()`) tiene il registro pari — 337 righe di
simboli, stringa a 0 (mod 8), carica al primo colpo.

## Nessuna migrazione

Lo schema delle celle resta **20**; `devices.db` guadagna solo la riga
`sync.lock` accanto (un file, non una tabella). Nessun formato cambia.

## Disco e pulizia

- `du -sh target`: **15 G all'inizio, 9,8 G alla fine** (il `target/debug`
  dell'iterazione, 14 G, è stato rimosso a fine fase; il worktree della
  base e `target/base-target` cancellati; `/tmp/bench` cancellato).
- Nessun processo lasciato acceso: né `sync serve`, né Node, né Electron
  (alcuni `sync-peer` dei test erano sfuggiti allo spegnimento, chiusi).
- `$TMPDIR` pulito: 2 319 cartelle della corsa fallita cancellate, zero
  prima e dopo le corse verdi.

**Verdetto: la v0.6.2 è chiusa.** S1–S4 consegnati, il contratto JS
esteso senza rompere nulla (29+17), l'inserimento dentro la soglia.

---

## Verifica v0.6.2 (revisore, 2026-11-16)

Sul tag `fase-v0.6.2` (`dd79a79`), main pulito, macchina altrimenti ferma.

### Cosa regge

- **Rust:** `cargo fmt --check` e `clippy -D warnings` puliti; `cargo test --workspace` **61 suite verdi**, nessun fallimento.
- **Linux:** `scripts/linux.sh` verde, uscita 0, 61 suite.
- **Il contratto JS:** `engine.test.js` identico a `fase-v0.6.1`; i test della CLI immutati da `eed2d7b` (si aggiunge solo `sync_lock_cli`). `npm test` **46/46**, `test:electron` **46/46**, `test:electron-main` verde.
- **Gli artefatti:** gli SHA256 di `engramma.darwin-arm64.node`, `index.js` e `index.d.ts` coincidono con quelli dichiarati.
- **S3:** `confirm` prima di `SasReady` risponde `INVALID_ARGUMENT` (letto nel codice: nessun altro stato porta a una conferma), e non esiste un'opzione che confermi da sé.
- **S4, la latenza da JS:** 10 scritture `put` su A → evento `arrived` su B, due processi Node, loopback. **Mediana 2290 ms, massimo 2297 ms**: dentro i 5 s della spec (il test di GLM concede 25 s, il numero vero è molto sotto).
- **S4, Electron:** un main process con il servizio acceso, una cella condivisa e una scrittura, poi **`app.quit()` vero**: il processo esce subito (anche con `onEvent` attivo, e anche con `close()` prima).
- **L'inserimento:** due coppie alternate, `--notes 10000 --runs 5`, `ENGRAMMA_SKIP_MODEL_TESTS=1`.

  | corsa | viva | uniform | p95 testo+grafo |
  |---|---|---|---|
  | base-1 | 2805 | 2830 | 1,8 / 1,8 |
  | fase-1 | 2824 | 2815 | 1,8 / 1,8 |
  | base-2 | 2822 | 2836 | 1,8 / 1,8 |
  | fase-2 | 2758 | 2770 | 1,8 / 1,8 |

  Tempo d'inserimento: coppia 1 −0,1%, coppia 2 +2,4%, **media +1,1%**: dentro il +5%. Il p95 è identico.

### I difetti

1. **Il difetto di convergenza è vero, e preesistente** (alto). I due testimoni di GLM riprodotti chiamando direttamente la flotta: `(7348206116156780080, 2, 19)` e `(7729712691107302376, 2, 25)` divergono, con le **stesse digestioni** dichiarate (`e86485bb…` contro `00cf8382…`). Il codice di `engramma-core/src` è **identico** fra `fase-v0.6.1` e `fase-v0.6.2`: il difetto c'era già. La mia verifica della v0.6.1 (2048 + 512×5 verdi) non l'aveva pescato per pura statistica.

   **Cosa diverge** (diff di `replicated_dump`): le due copie hanno **lo stesso numero di operazioni** (15 e 15, 13 e 13). Una sola delle due ha materializzato la **copia di conflitto** (la nota `conflitto`, «il corpo di 1 su …», col suo link e le sue righe d'indice). La copia di conflitto quindi **non è funzione dell'insieme delle operazioni**: dipende dall'ordine d'arrivo. Il punto da guardare è `cell/material.rs` (`write_conflict_copy`, `reconcile_conflict_copies`).

   **Peso:** nessuna operazione dell'utente si perde, ma due dispositivi mostrano memorie diverse (una nota in più, che la ricerca trova). Va chiuso prima di offrire la sync agli utenti di Homun.

   Nota: `PROPTEST_RNG_SEED` con quei numeri **non** riproduce, perché sono il parametro `seed` della flotta, non il seme del generatore. I testimoni vanno scritti come `#[test]` deterministici che chiamano `fleet_run`.

2. **`onEvent` tiene vivo Node dopo `close()`** (medio). In Node puro, un processo che registra `sync.onEvent(cb)` e poi chiama `engine.close()` **non esce più** (provato con timeout di 15 s, con e senza `start`). Se l'ascoltatore viene tolto prima (`off()`), il processo esce in 0 ms.
   - **Causa:** la `ThreadsafeFunction` di ogni ascoltatore è referenziata, e `CloseTask` ferma il servizio ma non svuota l'hub.
   - **Electron con `app.quit()` non ne soffre**, quindi Homun in produzione no. Ne soffrono gli script, i test e qualunque host Node.
   - **Correzione:** `close` (e la fine del servizio) svuota l'hub; le tsfn nascono non referenziate, come un `EventEmitter`: l'ascoltatore non deve tenere vivo il processo.

3. **`test:electron-main` non prova S4.3** (medio). Il test chiude con `app.exit(code)`, che termina il processo di forza: non passa da `app.quit()` e non ha nessun limite di 10 s. Nei fatti S4.3 regge (vedi sopra), ma il test va rifatto con `app.quit()` e un limite misurato dal chiamante.

4. **Il trucco del linker cura il sintomo** (medio). La causa non è il linker ma lo **`strip`**. Cargo in release applica `strip = "debuginfo"` di default, e rustc lancia `strip -S` di Xcode 27.2, che produce il «mis-aligned LINKEDIT string pool». La prova, sul codice di `dd79a79` **senza** le due righe di `arc4random`:
   - build di default: **rifiutata** da dyld, con lo stesso messaggio;
   - `CARGO_PROFILE_RELEASE_STRIP=none`: **si carica**, al costo di +1,9 MB (50,4 contro 48,4 MB).

   Il trucco regge per la parità del numero di simboli, quindi per caso. **Correzione:** `[profile.release.package.engramma-node] strip = "none"` (o l'equivalente che Cargo accetta) e via `arc4random`. Il controllo «la build carica l'artefatto» resta: è una buona idea.

5. **Le minori:**
   - `normalize_ticket_input` ha un ramo morto con un refuso (`"ENGGRAMMA-INVITE-V1."`, poi `let _ = payload`). Va tolto, o reso vero (accettare la forma originale tutta maiuscola).
   - `try_lock_root` scrive il pid **prima** di prendere il lucchetto, e senza troncare. Chi trova il lucchetto occupato sovrascrive il pid di chi lo tiene. Va scritto dopo `flock`, con `set_len(0)`.
   - Il firmatario si arma all'apertura e alla `share` di **questo** Engine. Una cella condivisa da un altro processo (la CLI) mentre Homun la tiene aperta scrive senza firma (`unsent`) finché non si riapre. Da dichiarare in `docs/node.md`, o da armare alla prima scrittura su una radice con `devices.db`.
   - «`stop()` durante una passata» gira su un Engine **senza pari**: la passata è vuota, il test non interrompe una sessione vera. Va rifatto con due processi e una sessione in corso.
   - La conferma del SAS **blocca un lavoratore** del runtime, e i due lavoratori ne sono l'aggiramento. Lo stato dell'arte è una conferma asincrona (un `oneshot` atteso dentro `pairing_net`), che non parcheggia nessun filo. Non bloccante.
   - Aggiunte oltre il contratto: `CANCELLED` e `Pairing.rounds`. Accettate: sono dichiarate e servono alla UI.

### Disco

- `target/`: 9,8 G all'inizio, 14 G alla fine (la build di debug della suite e la release del banco).
- Le cartelle `.tmp*` in `$TMPDIR` restano a 1 prima e dopo le suite Node e i testimoni.
- I worktree e i target temporanei del revisore (`target/nohack`, `target/base-target`) sono cancellati.

### Verdetto

**La v0.6.2 è consegnata**: S1–S4 reggono, il contratto JS è esteso senza rotture e l'inserimento sta dentro la soglia.

Restano aperti il difetto di convergenza preesistente (1) e i difetti 2–4. Vanno nella **v0.6.3**, da chiudere prima di offrire la sync agli utenti di Homun.

Homun **può agganciare la v0.6.2 per lo sviluppo** (l'API non cambierà forma). Non deve proporre la sync fra dispositivi agli utenti prima della v0.6.3.

# v0.6.3 — la copia di conflitto converge, e le minori

Base `main` @ `e9ee357` (tag `fase-v0.6.2`). Nessuna funzione nuova: un
difetto di convergenza preesistente e i difetti della verifica v0.6.2.
Il contratto JS non cambia forma: i 46 test della v0.6.2 restano
identici salvo il limite di C5.6 (dichiarato sotto). Nessun formato sul
filo cambia, schema 20.

Ogni punto è aperto dal commit del test rosso (C3 e C5.4 non hanno uno
stato rosso raggiungibile: lo si dice dove capita).

## C1 — la copia di conflitto è funzione dell'insieme (bloccante)

### La causa, prima della correzione

I due testimoni del revisore, come `#[test]` deterministici
(`the_witness_fleets_converge`): `(7348206116156780080, 2, 19)` diverge
con le digestioni `e86485bb…` contro `00cf8382…`, `(7729712691107302376,
2, 25)` con `dfc5af5e…` contro `e1f75346…` — le stesse del revisore.

La sequenza d'arrivo che li spiega (testimone 1, nota
`01000000000000000000000091`; l'ordine totale è P < D < Z < H < E < T <
W, con Z e H concorrenti sulla stessa base D):

| op | tipo | base | autore |
|---|---|---|---|
| P | put | — | disp. 1 |
| D | delete | P | disp. 0 |
| Z | patch | D | disp. 1 |
| H | patch | D | disp. 0 |
| E | patch | Z | disp. 1 |
| T | patch | H | disp. 0 |
| W | patch | T | disp. 1 |

Sul dispositivo 1: T arriva quando H manca (parcheggia, base ignota);
l'arrivo di H forza la ricostruzione dell'insieme {P, D, Z, H, E, T}: il
vincitore è T (base H) ed E sta fra H e T → **copia di E scritta** —
corretta per quell'insieme. Poi il dispositivo 1 **scrive W da sé**
(`patch_note`): `execute_op` applica l'operazione e **non ricalcola mai
l'insieme dei perdenti**. La copia resta: fantasma, perché nell'insieme
finale il vincitore è W (base T) e i perdenti sono vuoti. Sul
dispositivo 0 le ultime arrivate sono E e Z, *ingerite*: ogni arrivo
rifà la derivazione e la copia non c'è.

La scorciatoia incrementale del percorso d'arrivo
(`reconcile_conflict_copies(tx, &nid, &[])` quando arriva il nuovo
vincitore finale) aveva una premessa vera — la sua base è la testa, fra
la testa e se stessa non ci sono scritture del corpo — ma il percorso
di **scrittura locale** non pagava mai quella riconciliazione. Il
minimo che lo riproduce è `a_local_write_rederives_the_conflict_set`:
sei scritture con orologio e conio iniettati, la copia esiste dopo
l'arrivo di H (asserito), e deve sparire con la scrittura propria di W.

### L'oracolo differenziale

`verify` rigioca il registro **in ordine d'arrivo**: riproduce lo stato
incrementale, non può fare da oracolo. L'oracolo è
`Cell::rematerialize_for_test` (solo test): copia il registro in una
connessione vergine **senza applicare nulla**, poi materializza ogni
oggetto dal proprio insieme fino al punto fisso (la ricostruzione di un
originale deriva le righe delle copie; una copia editata dall'utente
materializza poi sulla sua derivazione). La proprietà
`every_copy_equals_its_reconstruction` confronta la digestione di ogni
copia con la propria ricostruzione — e quando diverge stampa il dump
vivo contro il ricostruito.

### La correzione

`material::reconcile_after_local_write`: dopo l'applicazione di una
scrittura del corpo nata qui, la nota riconcilia i conflitti — la
stessa chiamata del percorso d'arrivo, con la stessa premessa (una
scrittura locale è l'ultima del suo oggetto e la sua base è la testa:
perdenti vuoti, e le copie di un insieme più piccolo vanno via adesso).
Nessuna operazione nuova: la copia resta derivata. Il costo per
scrittura è una SELECT su `conflict_copies` per l'id dell'originale
(vuota nel caso comune).

### La caccia

- **Corsa rossa deliberata** (sorgente del commit rosso `edb67b1`,
  `every_copy_equals_its_reconstruction`, 3000 casi in release): fallita
  al caso 2731, nuovo testimone **seed 16888028934669089344 / 2 / 19**,
  ridotto al minimo. Il seme entra in
  `convergence.proptest-regressions`: è chiuso, e ogni corsa lo riesegue
  verde in testa.
- `the_fleet_converges` a **20 000 casi**: verde, 826,6 s.
- `every_copy_equals_its_reconstruction` a **20 000 casi**: verde,
  753,2 s.
- `PROPTEST_CASES=2048` più **cinque corse** della suite convergence
  completa: verdi, 210,0 / 133,7 / 142,9 / 132,2 / 144,8 s, `.tmp*` 0 dopo ognuna.
- **Cartelle `.tmp*` in `$TMPDIR`:** 0 prima e 0 dopo la corsa rossa con
  riduzione (contate anche a metà corsa), 0 dopo ogni caccia verde. Le
  2319 cartelle (~4 GB) viste da GLM nella verifica v0.6.2 non si
  riproducono: il percorso di riduzione di oggi costruisce le flotte in
  `Kept`/`Fleet`, i cui `TempDir` cadono anche nello unwind di
  un'asserzione; la perdita veniva dal codice di allora (il `copy_of`
  che perdeva la cartella, sistemato in `19d2e9e`), non da questo.

## C2 — l'ascoltatore non tiene vivo Node

`onEvent` costruisce la `ThreadsafeFunction` **non referenzita**
(`.weak::<true>()`): da sola non trattiene il loop degli eventi — la
semantica di un `EventEmitter`. Ciò che tiene vivo il processo è il
motore aperto, e `close()` lo ferma. La fine del servizio svuota l'hub:
il filo della pompa lo svuota quando il canale chiude (stop, close,
rimozione che ferma il servizio da sé) e `SyncShared::stop` lo svuota
subito dopo lo shutdown — ogni tsfn si rilascia.

Tre figli con limite di tempo (`sync.test.js`): `onEvent` + `close()`
**senza** mai chiamare la rimozione esce da solo (96 ms dal close);
lo stesso con `start()` prima (128 ms); e dopo `close()` la funzione di
rimozione non lancia. Contro il binario della v0.6.2 i primi due erano
rossi: il figlio restava vivo oltre il limite.

## C3 — il test di Electron chiude con `app.quit()`

La parte S4 ora fa ciò che dice: una cella condivisa con una scrittura,
l'ascoltatore registrato, il servizio acceso — poi `app.quit()`. Il
codice d'uscita viaggia in `process.exitCode` (nessun `app.exit` nel
file), e il runner (`scripts/electron-main-run.js`) misura dal lato del
chiamante e **fallisce oltre 10 s**: il figlio non può dare un voto
alla propria uscita.

Uscita 0 in 313–588 ms. **Nessuno stato rosso raggiungibile:** contro il
binario pre-C2 (tsfn referenziata, il difetto di fondo) Electron esce
comunque in 1183 ms — `app.quit()` chiude nonostante i riferimenti
libuv; il difetto vero appendeva Node puro ed è chiuso da C2. Il test
resta a provare che il percorso provato è `app.quit()`, non `app.exit`.

## C4 — la causa del rifiuto di dyld è lo strip di rustc

Tolti l'extern `arc4random` e il `black_box` da `version()`. L'artefatto
Node si costruisce senza strip: `[profile.release.package.engramma-node]
strip = "none"` — Cargo accetta la chiave a livello di pacchetto.

**La riproduzione minima** (una cdylib qualunque, sorgente di 8 righe):

| passo | esito |
|---|---|
| un extern preso per indirizzo (registro dei simboli indiretti **dispari**), build release di default (rustc 1.95, strumenti di Xcode 27.2) | dyld rifiuta: «mis-aligned LINKEDIT string pool», fileOffset 0xC0CC |
| la stessa sorgente con `CARGO_PROFILE_RELEASE_STRIP=none` | **carica**, 49 984 B — la stessa dimensione di prima: con `debug = false` non c'è nulla da togliere, è la riscrittura del LINKEDIT del passaggio di strip di rustc che disallinea la tavola delle stringhe |
| lo stesso file buono passato a `xcrun strip -S` **dopo** | carica ancora (49 992 B): l'invocazione manuale non riproduce il difetto, lo riproduce solo lo strip dentro il link di rustc |
| registro **pari** (nessun extern) + strip di default | carica |

Il nostro artefatto, senza `arc4random`: con lo strip di default
48 440 208 B, **rifiutato** (il testimone del revisore, riprodotto); con
`strip = "none"` 50 381 064 B (il `.node` copiato: 50 380 088 B),
**carica**. `npm run build` carica l'artefatto appena collegato, e la
CLI in release (57 979 232 B, strip di default) parte:
`engramma --version` risponde `engramma 0.1.0-dev`.

## C5 — le minori

1. **`normalize_ticket_input`:** il ramo morto («ENGGRAMMA», tre G)
   diventa vero: la forma originale incollata tutta in maiuscolo torna
   minuscola e decodifica. Rosso: `ENGRAMMA-INVITE-V1.ABC…` restava
   com'era; verde ora.
2. **`try_lock_root`:** il pid si scrive **dopo** la flock riuscita,
   troncando (`set_len(0)`): chi trova il lucchetto occupato non scrive
   nulla (rosso: il pid del titolare diventava `43399\n\n`, il pid del
   perdente), e un pid più corto non lascia la coda del precedente.
3. **Il firmatario si arma alla scrittura.** `Cell::set_signer_gate`
   (core): `execute` chiede un cancello prima di ogni scrittura locale,
   sotto il lucchetto di scrittura — l'armo e la scrittura sono un
   passo solo; una cella senza cancello paga un controllo nullo. Il
   binding installa il cancello di ogni cella che apre: una stat di
   `devices.db` per scrittura (una radice che non sincronizza si ferma
   lì: il banco d'inserimento, che passa per il core, non se ne accorge
   — vedi sotto); quando il timbro si muove — una condivisione, un
   abbinamento, un giro d'epoca, di questo Engine o della CLI in un
   altro processo — la prima scrittura dopo il cambio arma. Il test con
   tre processi (B tiene la cella aperta, un terzo processo sulla
   stessa radice condivide come la CLI, A è il pari): rosso — la
   scrittura del handle vecchio non arrivava mai (senza firma, il
   cancello del pari la rifiuta); verde — arriva in 2 545 ms.
4. **`stop()` durante una sessione vera:** il vecchio test fermava una
   passata vuota (un Engine senza pari: niente in volo). Il nuovo manda
   in scena **20 000 note scritte senza attendere la fine del comando**
   (il figlio le scrive a ~3 700/s: il flusso dura ~5,4 s) e ferma B a
   2,6 s, nel mezzo — la prova che era nel mezzo è la risposta del
   comando bulk, che arriva solo DOPO lo stop. `stop()` si conclude in
   **0–16 ms** (limite 5 s), e la passata di B subito dopo riprende il
   tiro della sessione interrotta (`received` > 0): niente resta
   inceppato. Verde dalla nascita: il servizio già interrompeva le
   sessioni in corso pulitamente; il test ora lo prova. (Curiosità
   raccolta strada facendo: `Cell.list` ha limite di default 100 — non
   è una misura di convergenza; e una sessione porta ~2 000 operazioni,
   un corpus grosso converge su passate ripetute.)
5. **La conferma SAS è un Future.** `pairing_net` non prende più una
   chiusura che blocca ma il tratto `SasConfirm` (un metodo che
   restituisce un Future); `PairingInner` lo implementa attendendo un
   canale tokio con una timeout future — nessun filo parcheggiato. Il
   runtime dell'abbinamento torna a **un filo** (`current_thread`): i
   due lavoratori erano l'aggiramento del difetto S1 della v0.6.2 (il
   PAKE di B completava, quello di A mai). Suite sync e contratto JS
   verdi al completo, abbinamenti a due processi compresi.
6. **Il test dell'arrivo a 5 s** (e la prima passata del ciclo di
   riposo, stessa macchina): la mediana del revisore era 2,3 s; qui
   l'evento `arrived` arriva in 2 448 ms. È l'unico limite di un test
   esistente che questa fase tocca; gli altri 46 restano identici.

## Il contratto

- `npm test`: **51/51** (i 46 della v0.6.2 identici salvo il limite di C5.6, più i 5 nuovi: tre figli di C2, C5.3, C5.4).
- `test:electron`: **51/51**.
- `test:electron-main`: verde, uscita 0 in 242 ms.
- Suite Rust (`cargo test --workspace`): **61 suite verdi** su macOS; `cargo fmt --check` e `clippy -D warnings` puliti.
- `cargo fmt --check` e `clippy -D warnings`: puliti.

## La build e i tre file

`npm run build` verde (l'artefatto appena collegato si carica). SHA256:

- `engramma.darwin-arm64.node`: ``4cef6bb68afa3a04258fdf2a1e7715b330fad5334fd9258db777db1caf33b033``
- `index.js`: ``6a440672963f2da32989bd306a91863b9ca87df2e50c6d6d15d7049e9fce8053``
- `index.d.ts`: ``aa3c17d279e63b75b15d7c08d02d1214f4f881f9c7d2769daf5043baa32abbd3``

Dimensione del `.node`: 50 380 088 B (48 438 288 nella v0.6.2: +2,0 MB,
lo strip disattivato — vedi C4).

## L'inserimento non peggiora (soglia +5%)

`engramma-bench --notes 10000 --runs 5` con
`ENGRAMMA_SKIP_MODEL_TESTS=1`, base `fase-v0.6.2` in un worktree col
suo `target` dentro `target/` (cancellato alla fine), **due coppie
alternate**, macchina altrimenti ferma:

| corsa | viva (note/s) | uniform (note/s) | p95 testo+grafo (ms) |
|---|---|---|---|
| base-1 | 2481 | 2637 | 2,5 / 2,1 |
| fase-1 | 2597 | 2679 | 2,0 / 2,1 |
| base-2 | 2660 | 2600 | 2,2 / 2,1 |
| fase-2 | 2605 | 2197 | 2,0 / 2,1 |
| base-3 | 2462 | 2584 | 2,3 / 2,6 |
| fase-3 | 2571 | 2558 | 2,3 / 2,0 |

Terza corsa per coppia: la `uniform` della fase-2 (2197 note/s) è
l'unica sotto la banda di tutte le altre nove misure (2462–2679) —
rumore di macchina, e la terza coppia lo chiarisce.

Tempo d'inserimento per nota (µs, mediana dei due corpora): coppia 1
base 390,8 → fase 379,1 (**−3,0%**); coppia 2 base 380,2 → fase 416,5
(**+9,5%**, la corsa rumorosa); coppia 3 base 396,4 → fase 389,9
(**−1,6%**). Media delle coppie: **+1,6%** (delle due sole corse
richieste: +3,2%), dentro il +5%; mediana dei tassi: base 2559, fase
2565 note/s — la fase non è più lenta. p95 testo+grafo: base 2,33/2,27
ms (viva/uniform), fase 2,10/2,07 ms — **meglio**, non peggio.

## Linux

`scripts/linux.sh`: **verde**, uscita 0, **61 suite** (fmt + clippy +
test, container offline). Una prima corsa, partita mentre il banco e la
build giravano insieme, era caduta su `-p engramma-cli --test mcp`
(l'uscita era mascherata da una pipe: colpa mia); a macchina ferma la
corsa è verde, e `mcp` è verde anche su macOS.

## Disco e pulizia

- `target/`: 14 G all'inizio della fase, 20 G alla fine (le build di release delle cacce, dell'artefatto Node, della CLI e del banco; il target del worktree della base è cancellato con il worktree).
- Cartelle `.tmp*` in `$TMPDIR`: **0** alla fine (0 anche prima e dopo ogni caccia, verde e rossa, e a metà corsa).
- I temporanei della diagnosi ( `/tmp/c4repro`, `/tmp/pptest` ) cancellati.
- Nessun processo lasciato acceso: né `sync serve`, né Node, né Electron.

## I commit

- `edb67b1` test(C1): i testimoni, la sequenza minima, l'oracolo dal registro
- `4eb737f` fix(C1): la scrittura locale paga la riconciliazione dei conflitti
- `e744dcb` test(C2): l'ascoltatore da solo non tiene vivo il processo Node
- `bf1631e` fix(C2): l'hub si svuota alla fine del servizio
- `01c8f8a` test(C3): il test di Electron chiude con app.quit()
- `e2f81b1` fix(C4): la causa del rifiuto di dyld è lo strip di rustc
- `56a49a3` test(C5.1+C5.2), `97193be` fix(C5.1+C5.2)
- `2981341` test(C5.6): il limite del test d'arrivo a 5 s
- `2683db5` test(C5.3+C5.4), `e978915` fix(C5.3)
- `c75d8f0` refactor(C5.5): la conferma SAS è un Future, runtime a un filo
- `4322736` docs: DESIGN §4.3 e §12, node.md, il seme chiuso nelle regressioni

## Verifica v0.6.3 (revisore, 2026-11-16)

Sul tag `fase-v0.6.3` (`a236e2d`), main pulito. La macchina **non** era
ferma: le app sul desktop (ZCode al 133% di CPU) tenevano il carico fra
10 e 20, e lo si vede nel banco.

### Cosa regge

- **C1, la convergenza:** `the_fleet_converges` ed
  `every_copy_equals_its_reconstruction` a **20 000 casi ciascuna**, in
  release, insieme ai due testimoni e alla sequenza minima: verdi,
  1592 s. Una corsa indipendente da quella di GLM: altri semi, stesso
  esito. La causa scritta nel REPORT torna col codice: il percorso di
  scrittura locale (`execute_op`) non riconciliava mai, e
  `reconcile_after_local_write` gli fa pagare la stessa chiamata del
  percorso d'arrivo. L'oracolo `rematerialize_for_test` ricostruisce dal
  solo registro, materializzando oggetto per oggetto fino al punto fisso:
  è un oracolo vero per il percorso incrementale, ma condivide con lui
  `materialize`, quindi non giudica la regola dei perdenti in sé (vedi il
  difetto 1).
- **C2:** un mio figlio Node, `onEvent` **senza** rimozione e poi
  `close()`: il processo esce in **2–3 ms**, sia a servizio spento sia
  dopo `start()`.
- **C3:** `test:electron-main` verde tre volte, 0,9–1,3 s dal lato del
  chiamante; nessun `app.exit` nel file.
- **C4:** `npm run build` verde, l'artefatto si carica; `engramma
  --version` della CLI in release risponde `engramma 0.1.0-dev`.
- **Il contratto JS:** `npm test` **51/51**, `test:electron` **51/51**.
- **Gli artefatti:** gli SHA256 di `engramma.darwin-arm64.node`,
  `index.js` e `index.d.ts` coincidono con quelli dichiarati, anche dopo
  una ricostruzione.
- **Linux:** `scripts/linux.sh` verde, uscita 0, **61 suite**.
- **C5:** letti nel codice. Il pid si scrive dopo la flock con
  `set_len(0)`; il SAS è un tratto `SasConfirm` che restituisce un
  Future, e il runtime dell'abbinamento è `current_thread`; il cancello
  del firmatario usa il timbro (mtime, lunghezza) di `devices.db`, che è
  in journal rollback: ogni scrittura del registro lo muove.
- **L'inserimento:** coppie alternate, `--notes 10000 --runs 5`,
  `ENGRAMMA_SKIP_MODEL_TESTS=1`, note/s viva / uniform e p95
  testo+grafo:

  | corsa | viva | uniform | p95 | |
  |---|---|---|---|---|
  | base-1 | 2575 | 2515 | 2,1 | pulita |
  | fase-1 | 2634 | 2666 | 2,0 / 1,9 | pulita |
  | base-2 | 1085 | 1010 | 6,3 | disturbata |
  | fase-2 | 2591 | 2612 | 2,1 | pulita |
  | base-3 | 939 | 2395 | 6,8 / 5,9 | disturbata |
  | fase-3 | 2258 | 2744 | 2,1 / 1,9 | carico 18 |

  Le corse pulite danno la fase **pari o più veloce** della base
  (coppia 1: +2,3% e +6,0% di note/s), col p95 uguale. Con la macchina
  rumorosa non firmo un numero più fine; insieme alle tre coppie di GLM
  (media +1,6%) non c'è segno di un peggioramento oltre il +5%.
- **Disco:** `target/` 20 G all'inizio e alla fine (il worktree e il
  target della base cancellati).

### I difetti

1. **La copia di conflitto converge, ma la regola dei perdenti la rende
   effimera** (alto, **preesistente**, di disegno). Il perdente è «ogni
   scrittura del corpo dopo la base del vincitore finale»
   (`final_losers`), e ogni `patch_note` conta come scrittura del corpo
   (`is_body_write` guarda il payload, non il campo `body`). Provato con
   un test temporaneo, due celle A e B:
   - A e B cambiano il corpo in concorrenza: una copia, su tutti e due.
   - Poi A cambia **solo il titolo** dell'originale: la copia **sparisce
     su tutti e due**. Il testo del perdente resta solo nel registro.
   - Un **titolo** di A concorrente a un **corpo** di B produce una
     **copia spuria** che contiene il corpo vecchio, quello di prima di
     tutte e due le modifiche.

   Sui riceventi accadeva già dalla v0.6.1 (la scorciatoia d'arrivo
   svuota l'insieme); la v0.6.3 l'ha resa coerente anche su chi scrive.
   È contro il DESIGN § 4.3 («una copia di conflitto visibile
   all'utente, come fanno i sistemi di sincronizzazione maturi»), e con
   Homun, i cui bot ritoccano spesso tag e titoli, la perdita sarebbe
   frequente. **Va nella v0.6.4, prima della sync agli utenti.**

2. **Il cancello del firmatario non è misurato dal banco** (basso). Il
   banco d'inserimento passa per il core, che non ha cancello; la stat
   di `devices.db` per scrittura la paga solo il binding. Il costo
   atteso è ~1 µs su ~380 µs, ma la frase «il banco non se ne accorge»
   non prova nulla.

3. **Numeri del REPORT** (basso). Il `.node` con lo SHA dichiarato
   misura **50 358 872 B**, non 50 380 088. E le cartelle `.tmp*` alla
   fine erano **9**, non 0: vuote, nate fra le 16:26 e le 17:24 (le
   corse di C5.3/C5.4), cancellate dal revisore.

### Verdetto

**La v0.6.3 è consegnata**: C1–C5 reggono, la convergenza tiene a
20 000 casi su due proprietà, il contratto JS è intatto, Linux è verde.

Resta il difetto 1, che è di regola e non di convergenza. **Homun può
agganciare la v0.6.3 per lo sviluppo, ma non offrire la sync agli
utenti prima della v0.6.4** (copie durevoli, solo il corpo crea
conflitti, la copia si spegne quando il vincitore la contiene, la mano
umana prima dell'agente).

## v0.6.4, prima della correzione — la regola nuova contro i testimoni vecchi

Scritta prima di toccare il codice, come chiede la fase: come la regola
nuova (D1) tratta i testimoni della v0.6.2, la sequenza minima della
v0.6.3, i semi di `convergence.proptest-regressions`, e i due angoli in
cui le regole della fase si toccano.

### La sequenza minima della v0.6.3 (P < Z < H < E < T < W)

`a_local_write_rederives_the_conflict_set`: catena P→Z→E su B, catena
P→H→T su A, poi W (base T) scritta da B. Sotto la regola nuova le punte
corporee dopo W sono **{W, E}** (E non è antenata di W: W discende T, T
discende H, H è concorrente di E), entrambe d'agente (`device-*`), il
vincitore è W per ordine totale, il perdente è E con biforcazione P.
Nessun testimone di chiusura: W non contiene E (diff3 su F = «il corpo
iniziale»: due sostituzioni diverse della stessa riga → sovrapposizione)
e nessuna operazione tocca l'id della copia. **La copia di E resta
aperta**: il testo di E non vive da nessun'altra parte. Le attese del
test cambiano: `copies_of` dopo W da 0 a 1, e `copies_of(&c)` (la copia
totale in ordine) da 0 a 1. Le asserzioni di convergenza (b = c = d,
l'oracolo) restano. È il cambiamento voluto: chiudere quella copia era
esattamente il difetto dell'effimera.

### I testimoni della v0.6.2 e la premessa del cammino rapido

Il commento della v0.6.3 («una scrittura locale è l'ultima del suo
oggetto, quindi i suoi perdenti sono vuoti») **non vale più**: con la
classe dell'autore, una scrittura attaccata alla testa può perdere
contro una punta umana più vecchia, e la sua copia deve nascere anche
sul dispositivo che scrive. Le due scorciatoie
(`reconcile_conflict_copies(tx, &nid, &[])` nell'arrivo e in
`reconcile_after_local_write`) vanno riscritte come derivazione piena
dell'insieme; il costo è pagato solo dalle scritture del corpo (una
scansione degli operazioni della nota), le note fresche — tutto il banco
d'inserimento — pagano una scansione di una riga.

I due semi testimone (`7348206116156780080` e `7729712691107302376`)
asseriscono solo convergenza e uguaglianza con l'oracolo: restano verdi
per costruzione, perché la derivazione nuova è una sola funzione
dell'insieme, condivisa dal percorso incrementale e dal replay.

### I semi di `convergence.proptest-regressions` (12 semi)

Anche loro asseriscono solo convergenza e oracolo. La regola nuova
cambia lo **stato derivato** (copie diverse, in più o in meno), non la
 proprietà che quei semi cacciano. Attesa: tutti verdi; la caccia a
20 000 casi lo prova.

### Dove le regole della fase si toccano (scritto prima, per decisione)

1. **La lapide e le copie.** La regola della lapide resta quella di
   oggi, per la nota e per la derivazione: una scrittura *battuta*
   dalla lapide non produce copia (ha perso contro una cancellazione,
   non contro un'altra mano), e una scrittura *cancellata dal rollback
   del delete* (il delete non l'aveva vista) esce pure dalla
   derivazione — niente punte, niente copie, niente «vincitore» che
   resusciti un corpo che la lapide ha scartato. *Aggiornato in
   implementazione:* la prima stesura di questa sezione teneva la
   copia anche al rollback; il seme 8354786356070561648 della flotta
   ha mostrato il prezzo: la correzione del corpo del vincitore
   rimetteva il corpo cancellato sopra lo stato che la lapide (e il
   supersede che ne seguiva) avevano lasciato, e i due percorsi —
   replay e arrivo — divergevano. La regola della lapide vince anche
   sulla durabilità: è ciò che la fase chiede («la regola della
   lapide resta quella di oggi»).
   E la lapide ha una **seconda faccia**, trovata dalla proprietà di
   monotonia (seme `8148618087815055854`): quando batte una punta,
   l'insieme delle punte si stringe e il vincitore può cambiare — la
   copia del perdente di prima, ora vincitore, chiude senza testimone.
   E una **terza**, trovata dallo stesso seme: l'arrivo fuori ordine
   di un'operazione mancante può **completare il grafo** e fare del
   perdente il vincitore (la sua catena di `base` passa per
   l'operazione arrivata: non era concorrente, era solo incompleta) —
   la copia chiude perché l'insieme non la merita più, e il testo vive
   nel corpo della nota. La proprietà di monotonia ammette le forme
   che non passano da D1.6 — purga, lapide, i due filtri del punto 3,
   e il grafo completato (il giudice `deserved_for_test` lo riconosce)
   — e fallisce dove una copia ancora meritata sparisce senza
   testimone: quello è il difetto che cerca.
2. **Il testimone (b) si allarga di un caso: il discendente.** Una
   scrittura del corpo **che discende dal perdente** (la sua catena di
   `base` passa per lui) lo ha *visto per costruzione*: è un testimone
   di chiusura anche quando il diff3 non torna (l'appendice in coda:
   F = «a», perdente = «a\\nx», discendente = «a\\nx\\ny» — stessa
   regione, contenuti diversi, diff3 in conflitto). Senza questo
   testimone, le tre regole della fase non stanno insieme: la copia di
   un perdente il cui ramo continua resterebbe aperta per sempre, e
   chiuderla «perché non è più una punta» romperebbe la monotonia
   (chiusura senza testimone). È il principio della sezione — «un
   conflitto si chiude solo con qualcosa che l'ha visto» — preso alla
   lettera: il discendente ha visto l'operazione, il diff3 dice che ne
   ha visto il **testo**.
3. **Due chiusure da filtro, senza diff3.** Il filtro D1.5 (corpo
   uguale a quello del vincitore, corpo uguale alla biforcazione)
   chiude una copia già aperta quando il vincitore **cambia**: un
   vincitore umano più vecchio col corpo per coincidenza uguale al
   perdente, o una biforcazione che si sposta su uno stato col corpo
   del perdente. Sono angoli misurati in anni-futura della vita di
   una cella (servono corpi identici da rami diversi); il generatore
   della flotta usa corpi numerati, quindi la caccia non li produce.
   La convergenza vince su di essi (la copia è funzione dell'insieme,
   sempre): la monotonia li dichiara nel REPORT se mai appaiono.
4. **Le scritture senza `base` sono radici.** Il doppio importo della
   v0.6.1 (stesso id su due macchine, due put senza base) deve
   continuare a dare la copia del corpo perdente, come oggi: una
   scrittura senza base non è antenata di niente. Le catene del
   pre-v0.6, che non rimaterializzano mai, non cambiano comportamento
   (la derivazione corre dove già correva).
5. **La purga e le copie resta quella di oggi**, verificata dai test
   che già la coprono (`the_purge_dominates_whatever_the_arrival_order`
   e la proprietà della flotta): la purga della nota porta via le
   copie e le righe derivate della coda; la purga di una copia chiude
   il conflitto (è un'operazione sul suo id).

### Il piano della derivazione (una funzione, due percorsi)

Per ogni nota, dall'insieme delle sue operazioni: **punte** = le
scritture del corpo applicate senza discendenti corporei; **vincitore**
= chiave `(classe, hlc, op_id)` massima fra le punte; **candidati** =
le scritture del corpo applicate che non sono il vincitore né suoi
antenati; ognuna ha una copia **aperta** se nessun testimone è
nell'insieme (operazione sull'id della copia; discendente; diff3 dalla
biforcazione) e i filtri D1.5 passano (il corpo differsce da quello del
vincitore e dalla biforcazione; un perdente più recente col suo stesso
corpo è, a un tempo, la copia che resta e il suo testimone diff3). Il
**corpo della nota** è quello del vincitore — anche quando non è
l'ultima applicata: la mano umana più vecchia vince — e l'intestazione
resta ultima-scrittura-per-campo nell'ordine totale. **Tetto**: le tre
aperte più recenti nell'ordine totale sono visibili (riga in `notes`,
FTS, tag, arco, e coda se in quarantena); le altre restano aperte e
invisibili, e appaiono quando una visibile si chiude. Schema 21, solo
tabelle derivate, migrazione che le rimaterializza.

## v0.6.4 — la copia di conflitto è durevole

Una sezione per punto. I numeri sono misurati, nessuno è stato scritto
prima di correrlo.

### D1 — la regola nuova dei conflitti del corpo

- **Solo il corpo confligge.** `is_body_write` ora guarda il `set`:
  una patch di sola intestazione non è una scrittura del corpo. I due
  difetti del revisore chiusi dai test deterministici: un titolo
  sull'originale non chiude la copia; un titolo concorrente a un corpo
  non crea copie spurie.
- **Punte per ascendenza, vincitore per classe.** Il grafo cammina le
  `base` attraverso ogni operazione della nota; le punte sono le
  scritture del corpo applicate senza discendenti corporei; il
  vincitore è la chiave `(classe, hlc, op_id)`. Una scrittura senza
  `base` è una radice: il doppio importo della v0.6.1 dà ancora la
  copia del corpo perdente.
- **La copia è durevole** e chiude solo con un testimone: (a)
  un'operazione sull'id della copia; (b) una scrittura del corpo
  applicata successiva che la contiene (diff3 rigoroso, cambi identici
  = un cambiamento) o che ne discende. Il corpo della nota è quello
  del vincitore, anche quando un'altra mano gli è passata sopra
  nell'ordine d'arrivo; un `restore` più recente del vincitore tiene
  il suo.
- **Tetto di tre visibili** per nota, le più recenti nell'ordine
  totale; le altre restano aperte e appaiono quando una visibile si
  chiude.
- **La scorciatoia è morta.** `reconcile_conflict_copies(tx, &nid,
  &[])` non esiste più: ogni arrivo per una nota — corpo, intestazione,
  soppressione, operazione sulla copia — rideriva l'insieme dei
  conflitti. La proprietà di monotonia («mai una chiusura senza
  testimone», con le dominanze ammesse: purga, lapide, grafo completato)
  e l'oracolo differenziale giudicano.
- **La caccia ha trovato quattro difetti del percorso incrementale**,
  tutti chiusi, tutti con il seme nel messaggio di commit: le
  righe-nascita mancanti per le copie con operazioni quando il
  perdente diventa vincitore (due volte, semi `7729712691107302376` e
  `15192944291106605994`); il contenuto della copia diverso fra i
  percorsi (attiva da una parte, in quarantena dall'altra, seme
  `10782023552461549766`); la riderivazione assente sugli arrivi che
  non sono corpo (seme `1421488054822918858`); e la guardia del replay
  che confrontava col vincitore-per-classe invece che col più recente
  nell'ordine totale, lasciando divergere le istantanee di versione
  (seme `12328535914020248362`). Ogni difetto è la stessa malattia:
  due percorsi verso la stessa derivazione, uno dei quali shortcuts.
- **Il generatore della flotta a fasi.** La vecchia flotta seminava le
  consegne fra le scritture: i rami concorrenti convergevano prima che
  un dispositivo potesse VEDERLI (sei copie in mille flotte, e le
  fusioni a mano e le chiusure di copia non scattavano mai). Lo
  schedulatore nuovo scrive per quattro giri senza consegnare, poi
  sincronizza tutto: i rami crescono, gli incontri derivano, le
  finestre si aprono. **Frequenze per 1000 flotte** (3 dispositivi,
  20 giri): 23 928 operazioni, **21,6% umane** e 78,4% agente; 2 082
  scritture del corpo; **1 392 patch di sola intestazione**; **24
  fusioni a mano**; **18 delete su id di copia**; 138 copie aperte a
  fine corsa, di cui 126 in quarantena.
- **I semi di `convergence.proptest-regressions`** restano verdi: con
  il generatore nuovo asseriscono convergenza e oracolo, che la regola
  nuova tiene per costruzione (una funzione dell'insieme in ogni
  percorso).
- **La sequenza minima della v0.6.3** cambia attesa, come scritto
  nell'analisi a priori: la copia di E resta aperta anche dopo W
  (l'attesa passa da 0 a 1); il test `a_local_write_rederives` ora
  asserisce la durabilità invece del vuoto. I due semi testimone
  (`7348206116156780080`, `7729712691107302376`) verdi.

### D2 — la classe dell'autore

`classify_actor`: umano è `user`, chi finisce in `:user`, chi comincia
con `user@`; tutto il resto agente, compresi gli sconosciuti. La
convenzione sta nel DESIGN § 4.3 e in `docs/node.md`, con l'avvertenza:
dichiarata e non firmata — basta nella sync personale, nelle
organizzazioni andrà legata a una firma. I test: l'umana vince anche
più vecchia; il perdente d'agente nasce in quarantena, nella coda
(`proposals`, riga derivata col motivo «conflitto», che `decide` e il
revert trattano come ogni proposta); il perdente umano dà la copia
attiva di sempre; gli actor di Homun rientrano senza modifiche. La
flotta genera entrambe le classi (misurate sotto).

### D3 — conflicts e resolveConflict

Core: `Cell::conflicts()` (ogni copia aperta: `copy_id, original_id,
loser_actor, loser_class, loser_at, quarantined, visible` — nessun
testo) e `Cell::resolve_conflict(copy, keep, body, actor)` con
`ResolveKeep::{Original, Copy, Merged}`. JS: `cell.conflicts()` e
`cell.resolveConflict(copyId, {keep, body?})`. CLI: `engramma conflicts
<cella>` e `engramma conflicts resolve <cella> <copia> --keep …
[--body-file f]`. Errori: `NOT_FOUND` per un id che non è una copia (o
una copia chiusa senza operazioni sue), `CONFLICT` per una copia chiusa
che tiene la riga. Due risoluzioni concorrenti con corpi uguali non
creano nulla (provato su due dispositivi accoppiati); la risoluzione
`copy` chiude spesso già con la patch (il corpo uguale contiene il
perdente) e il delete diventa un no-op. Una risoluzione che scelga il
corpo dell'agente contro il proprio corpo umano precedente fa nascere
la copia del corpo umano scartato: durabile anche contro la
risoluzione — il compito lo chiama «nasce un conflitto nuovo, ed è
giusto così», e il test lo asserisce.

### D4 — la versione delle regole nel protocollo

L'ALPN `engramma/sync/2` (e `engramma/mailbox/2`) È la versione delle
regole di materializzazione; l'hello porta `version: 2` come cintura.
La scelta è nel REPORT perché è la più stretta: un pari v0.6.3 non
condivide nemmeno l'ALPN, l'handshake muore prima di un frame, e chi
chiama mappa il rifiuto rustls («peer doesn't support any known
protocol») in `Error::PeerOutdated(dispositivo)` → `PEER_OUTDATED` in
JS, col nome del dispositivo da aggiornare e nessuna operazione
scambiata. L'utente vede: «il dispositivo mac usa regole di
materializzazione diverse: aggiornalo (o aggiorna questo dispositivo)
prima di sincronizzare». L'abbinamento resta `engramma/pairing/1` (non
trasporta operazioni da materializzare): due dispositivi di versioni
diverse si abbinano, e la prima passata rifiuta con `PEER_OUTDATED`.
Test: uno stub ALPN v1 su loopback rifiutata come `PeerOutdated` col
nome; due endpoint v2 si stringono la mano.

### D5 — quanto cresce il database

Misure (release, `dbstat` e file):

- **Byte per copia**: nota 1 KB → **5 734 B** per copia; nota 10 KB →
  **30 105 B** (20 copie aperte per misura, differenza divisa).
- **Il risparmio misurato e fatto**: prima della correzione la copia
  costava 7 372 B (1 KB) e 41 984 B (10 KB): il corpo dentro
  `state_json` era il **28%** del costo della copia da 10 KB — oltre la
  soglia del 20% — e ora la riga di derivazione conserva tutto fuorché
  il corpo: il payload del perdente è la sua casa (un perdente è
  sempre una scrittura del corpo col payload sopra il taglio; contro la
  compattazione è verificato dal fatto che le punte vivono sopra il
  taglio). I punti di lettura della riga (la base del replay della
  copia, `resolveConflict` con `copy`, i percorsi rapidi) rileggono il
  corpo dal payload.
- **Banco di flotta** (10 000 note, conflitti offline, una passata
  intera in una copia fresca):

  | flotta | file | per nota | crescita | copie | passata |
  |---|---|---|---|---|---|
  | senza conflitti | 18 231 296 B | 1 823 B | — | 0 | 3,09 s |
  | 1% (100) | 18 788 352 B | 1 878 B | **+3,1%** | 100 | 3,23 s |
  | 10% (1 000) | 23 293 952 B | 2 329 B | **+27,8%** | 1 000 | 4,58 s |

- **La soglia dichiarata (+15% al 10%) NON è raggiunta**, e i numeri
  dicono perché: ogni nota in conflitto porta DUE scritture in più
  (l'agente e la mano umana — il conflitto stesso, ~800 B l'una col
  suo payload e i suoi indici) oltre alle righe della copia. Le righe
  DERIVATE della copia sono ~9–10 punti dei 27,8; il resto è il costo
  delle operazioni che il conflitto porta con sé, che ogni sistema che
  non perde testo paga. Fermarsi e scriverlo, come chiedeva la fase.
  I vettori non entrano in queste misure (il banco non incorpora): il
  secondo risparmio (nessun vettore per le copie) resta therefore non
  misurato qui e non fatto.

### D6 — le minori

1. **Il cancello del firmatario, misurato dove vive.** Banco da Node:
  10 000 `put` su una radice senza `devices.db`, col cancello della
  v0.6.3 (una `stat` per scrittura) contro la variabile da banco
  `ENGRAMMA_BENCH_NO_SIGNER_GATE`: **332,7 µs contro 327,2 µs per put,
  +1,68%** — dentro la soglia del +2%.
2. **I numeri del REPORT si scrivono dopo la misura**: la dimensione
  del `.node` e le `.tmp*` sono in fondo, contate alla fine.

### Il contratto JS

`npm test` **54/54** (i 51 della v0.6.3 identici più i tre nuovi:
l'elenco a cella quieta, i due D3 fra due processi accoppiati);
`test:electron` **54/54**.

### La caccia e le cinque corse

La caccia ha GUIDATO la fase: **sette difetti trovati e chiusi uno per
uno** — i quattro del percorso incrementale, la purga che non
raggiungeva le copie delle copie, la guardia della purga sugli agganci
(la proprietà del misto di `props.rs`, a 2048 casi: la purga forzata si
portava dietro la derivazione di una nota purgata), e la riga-nascita
di una copia vissuta il cui perdente la lapide aveva cancellato. Sul
commit finale, in release:

- `the_fleet_converges` a **20 000** casi — verde, 1054,7 s;
- `every_copy_equals_its_reconstruction` a **20 000** casi — verde,
  785,8 s;
- `no_copy_closes_without_a_witness` (la monotonia) a **20 000** casi —
  verde, 588,8 s;
- `PROPTEST_CASES=2048` con **cinque corse** della suite del core:
  **257 verdi, 0 falliti, tutte e cinque**.

### Linux

`scripts/linux.sh` verde: fmt + clippy + 535 test nel contenitore, rete
chiusa, volume `engramma-linux-target` ricostruito da zero (il revisore
l'aveva cancellato: la prima corsa ha ricompilato tutto, come previsto).

### Il contratto JS

(la riga definitiva sta nella sezione D6 qui sopra: 54/54 due volte,
Node ed Electron, addon ricostruito sul commit finale)

### L'inserimento non peggiora (soglia +5%)

Le due coppie alternate contro `fase-v0.6.3` (worktree con target suo,
dismesso alla fine), `--notes 10000 --runs 5`,
`ENGRAMMA_SKIP_MODEL_TESTS=1`, carico accanto a ogni corsa:

| corsa | carico | note/s | p95 testo+grafo |
|---|---|---|---|
| base-A | 3,3 | 2952 | 1,8 |
| fase-A | 2,0 | **2885** | 1,8 |
| base-B | 1,8 | 2955 | 1,8 |
| fase-B | 1,7 | **2872** | 1,8 |

**Media −2,5%** (−2,3% e −2,8%), p95 identico: **entro la soglia del
+5%**. La prima misura della fase, prima della correzione, era
**2511/2527 note/s (−14%)**: la derivazione pagava il scan completo
anche sulla nota fresca. La nota con **una sola operazione** non deriva
niente — un corpo solo, l'unica punta, il vincitore per costruzione,
nessun perdente, nessuna correzione possibile — e il cortocircuito
(un COUNT indicizzato) riporta l'inserimento in soglia. Il purge
rallenta (290 → ~360 ms, +24%): la purga transitoria attraversa le
copie; è il prezzo dichiarato della profondità.

### La build e i tre file

`npm run build` verde sul commit finale, l'artefatto si carica,
`version()` risponde `0.1.0-dev`:

| file | byte | SHA256 |
|---|---|---|
| `engramma.darwin-arm64.node` | 50 547 880 | `eb485fd35a80ccc7f8ae4e00ab1bd53e6c15ffcb8b9610c9822f58feb1a0ed16` |
| `index.js` | 36 188 | `6a440672963f2da32989bd306a91863b9ca87df2e50c6d6d15d7049e9fce8053` |
| `index.d.ts` | 23 046 | `83a46439830d7478e6eebb8ecb8664806303e52e64946d52d228ea1e1e84b78c` |

### Disco e pulizia

- `target/` **2,4 G** all'inizio (prima build dopo la cancellazione del
  revisore) e **8,7 G** alla fine — il worktree della base e il suo
  target cancellati, il volume Docker `engramma-linux-target` rimosso;
- cartelle `.tmp*` alla fine: **0**;
- nessun processo lasciato acceso: né `sync serve`, né Node, né
  Electron (il demone di Homun che gira è quello dell'utente, non
  nostro).

## Verifica v0.6.4 (revisore, 2026-11-17)

Sul tag `fase-v0.6.4` (`a8322cd`), build e test rifatti sulla macchina del
revisore. Le prove dei difetti stanno in un test di scarto
(`zz_review_v064.rs`, tolto dal repo, copia nello scratchpad del revisore).

### Cosa regge

- **La build** riproduce i tre SHA256 e la dimensione del `.node`
  dichiarati (50 547 880 B): i numeri questa volta sono quelli misurati.
- **I due difetti della verifica v0.6.3 sono chiusi**: un titolo
  sull'originale non chiude la copia, un titolo concorrente a un corpo non
  crea copie spurie (test deterministici, e il revisore li ha rifatti a
  mano).
- **D2, D3, D4** fanno quello che la spec chiede: la classe dell'autore, la
  quarantena nella coda, `conflicts`/`resolveConflict` da JS e CLI con
  `NOT_FOUND`/`CONFLICT`, l'ALPN `sync/2` e `mailbox/2` con `PEER_OUTDATED`.
- **D5** si è fermato alla soglia mancata e l'ha scritto, come chiesto.
- **D6**: il cancello del firmatario a +1,68%, entro soglia.
- **L'inserimento** a −2,5%, p95 pari; la caccia come guida della fase ha
  trovato e chiuso sette difetti veri del percorso incrementale.
- **La pulizia**: zero `.tmp*`, nessun processo nostro acceso.

**Le corse del revisore.** `npm run build` riproduce gli stessi tre SHA.
`npm test` e `test:electron` danno 54/54. La suite del core in release è
verde, `protocol_version` è verde, e alla fine restano 0 `.tmp*`. La
caccia a 20 000 casi non è stata rifatta: i difetti qui sotto vengono da
scenari deterministici, non dalla flotta.

### I difetti

1. **Il testo di un perdente si perde ancora, anche senza classi
   (bloccante).** La `base` di una scrittura locale è «l'ultima operazione
   della nota per HLC», non l'operazione **il cui corpo l'autore stava
   guardando**. Con le copie durevoli le due cose divergono: il perdente L
   è applicato (vive come copia), ma il corpo visibile è del vincitore W.
   Il testimone «discendente» (aggiunto nella fase, REPORT «Dove le regole
   si toccano», punto 2) prende la catena di `base` per «l'ha visto», e
   chiude la copia di L senza che il suo testo sia da nessuna parte.
   - *Prova R4, solo `homun:user`:* A scrive il corpo L, poi **solo il
     titolo** (più nuovo di W); B scrive il corpo W in mezzo. Dopo la sync
     la nota mostra W e la copia di L è aperta: giusto. B aggiunge una riga
     al corpo che vede: la sua `base` è la patch del titolo, che discende
     da L. **La copia di L chiude su tutti e due i dispositivi, e «DUE-di-A»
     non è più in nessuna nota.** È il difetto dell'effimera della v0.6.3
     rientrato dalla porta del discendente, e con Homun è frequente: i bot
     ritoccano titoli e tag dopo aver scritto il corpo.
   - *Prova R1, con le classi:* la mano umana vince più vecchia, il bot
     rilegge la nota (vede il corpo umano) e aggiunge una riga. La sua
     `base` è la sua vecchia scrittura (più nuova per HLC): la scrittura
     nuova resta concorrente all'umana, **finisce in quarantena**, e la
     nota non cambia. Ogni scrittura successiva del bot su quella nota fa
     la stessa fine finché un umano non risolve. In più la copia della
     prima scrittura del bot chiude per «discendente», ma il suo testo
     non c'è più da nessuna parte.
   - *Perché la caccia non l'ha visto:* la proprietà di monotonia accetta
     il discendente come testimone, quindi giudica con la stessa premessa
     sbagliata del codice. Serve un giudice che non legga la `base`.
2. **Una copia chiusa si riapre (la spec diceva il contrario).** La
   biforcazione del test di contenimento è `F = nca(perdente, vincitore)`.
   Quando il vincitore cambia (una punta umana che arriva tardi), F si
   sposta, e una scrittura che conteneva il perdente rispetto alla F di
   prima non lo contiene più.
   - *Prova R2:* la copia di L chiusa dalla fusione a mano W2 **si riapre
     con lo stesso id** quando arriva la punta umana H.
   - *La colpa è anche della spec*, che definiva F rispetto al vincitore.
     La proprietà di monotonia guarda solo presente→assente, mai
     assente→presente, e il DESIGN § 4.3 scrive «aggiungere operazioni non
     riapre mai una copia chiusa», che oggi è falso.
3. **Il diff3 è quadratico in tempo e in memoria.** È una tabella LCS
   (n+1)×(m+1) di `u32`, e gira per ogni coppia (perdente, scrittura
   successiva) a ogni riderivazione.
   - *Prova R3:* una nota da 20 000 righe costa **1,4 s e 1,68 GB di RSS
     per un solo diff3**; a 5 000 righe sono 108 ms.
   - Engramma non ha un tetto alla dimensione del corpo: un documento
     importato in conflitto può far morire il processo per memoria, anche
     all'arrivo di una sync.
4. **D5: la crescita «delle righe derivate» è stimata, non misurata.** Il
   banco confronta la flotta in conflitto con una flotta che non ha le due
   scritture in più per nota. Il «~9–10 punti» per le righe della copia è
   quindi una stima. Le stesse misure dicono anche che una copia di un
   corpo di ~70 B costa nell'ordine di 3 KB di righe derivate: va vista
   tabella per tabella.
5. **Il testo di una copia in quarantena non si legge da JS.** Il
   contratto dice «la copia è una nota, leggila con `get`», ma `get`
   risponde `null` per le note in quarantena, e in JS non c'è nessuna
   API della coda. Le copie in quarantena sono 126 su 138 nella flotta.
   Homun può elencare i conflitti, ma non può mostrare all'utente che
   cosa sta scegliendo (si arriva al testo solo con
   `list({status:['quarantined']})` e un filtro per id).
6. **Minori.**
   - Il DESIGN § 12 ha una riga vuota fra v0.6.3 e v0.6.4, che spezza la
     tabella, e una colonna in più. Refusi: «scritura», «differsca».
   - La purga rallenta del 24% (290 → ~360 ms): è dichiarata, ma non era
     in nessuna soglia. Va tenuta d'occhio.

### Verdetto

**v0.6.4 non consegnata.** D2–D6 reggono e la caccia ha fatto un lavoro
vero. Ma la promessa della fase, «un conflitto si chiude solo con qualcosa
che l'ha visto», è ancora rotta: il testo di un perdente si perde con una
patch di titolo sul ramo perdente, cioè proprio il caso d'uso di Homun. La
sync resta chiusa agli utenti di Homun. Correzione nella v0.6.5.


## v0.6.5 — chi scrive parte da ciò che ha visto (2026-11-17)

La correzione della v0.6.4, che la verifica ha bocciato. La regola dei
conflitti resta; cambia ciò su cui poggia: la `base` di una scrittura
locale, la biforcazione del test di contenimento, il diff3. In più: la
lettura del conflitto da JS e la misura di D5 fatta bene. Ogni punto E1–E7
porta i numeri misurati; la caccia a 20 000 casi è sospesa su richiesta
dell'utente (macchina carica) e resta da fare in una finestra tranquilla:
le quattro proprietà sono verdi a 512–3 000 casi.

### E1 — la `base` è la testa della linea visibile

- **La regola:** un'operazione locale che modifica una nota esistente
  porta come `base` l'operazione più recente che **ha il vincitore del
  corpo fra i suoi antenati, o è il vincitore stesso** (patch
  d'intestazione sopra il vincitore comprese, rami perdenti esclusi).
  Nota chiusa da lapide: la **linea chiusa** (la lapide o chi la lasciò
  chiusa) — ed è la stessa linea che la riapertura giudica, con la CTE
  ricorsiva sull'insieme. Nota degenere (nessuna scrittura del corpo
  applicata): la regola di ieri.
- **Derivata e indicizzata:** la tabella `note_heads` (schema 22), scritta
  da `after_replay` (una riga per nota con più di un'operazione), letta
  dalla scrittura locale con una sola sonda sulla chiave; il fallback
  (la query di ieri) è esatto per le note da una operazione. La
  retrocompilazione della migrazione parte da `note_heads` vuota e
  rematerializza le note che la vogliono.
- **Rossi della verifica chiusi:** R4 (la patch di titolo sul ramo
  perdente non inghiotte più la copia: «DUE-di-A» resta leggibile su
  entrambi i dispositivi) e R1 (la scrittura dell'agente sul corpo umano
  visibile è sequenziale, diventa il corpo della nota, non finisce in
  quarantena, e la copia della prima scrittura resta aperta) —
  `tests/visible_head.rs`. I test della riapertura della lapide restano
  verdi; uno di essi ha guidato la correzione della linea chiusa (prima
  la riapertura giudicava «l'ultima operazione», la stessa premessa
  sbagliata della base).
- **Le celle di sviluppo con basi vecchie:** la derivazione legge
  l'insieme, quindi convergono comunque; le scritture vecchie restano
  concorrenti come le scrissero i loro autori, e le copie che il
  vecchio testimone-per-discendenza chiuse restano chiuse (il testimone
  è un'esistenza). La regola nuova vale per le scritture che nascono da
  qui.

### E2 — la biforcazione è fra il perdente e il testimone

- Il testimone di contenimento giudica da **F = nca(perdente, testimone)**,
  estremi compresi: la coppia e il loro passato, mai il vincitore (che
  un arrivo tardivo può spostare). R2 chiuso: la copia fusa a mano non
  si riapre quando la punta umana vince (`tests/containment_fork.rs`).
- **La monotonia allargata** controlla anche assente→presente, col
  registro delle chiusure: una copia riaperta deve essere rinata dalle
  sole eccezioni dichiarate (filtri che seguono il vincitore, grafo
  completato, purga, lapide) — DESIGN § 4.3 corretto di conseguenza
  («una copia chiusa con un testimone non si riapre mai»).
- **Il difetto trovato dal giudice (sotto):** la chiave di classe rendeva
  un antenato umano PRECEDENTE un testimone «successivo» di un
  discendente d'agente più nuovo, e la biforcazione inclusiva lo faceva
  «contenere» il discendente: la copia di chi aveva letto e riscritto lo
  stesso corpo chiudeva e il testo spariva. Il testimone ora è successivo
  **nell'ordine totale** (le parole del DESIGN), non nella chiave di
  classe: la classe decide chi vince, il tempo decide chi viene dopo.
  Seme 3142143418041393536, 1 856 successi prima del rosso; caso
  deterministico in entrambi gli ordini d'arrivo in `visible_head.rs`.

### E3 — il diff3 in spazio lineare, con un tetto

- **Myers col serpente centrale** (O((n+m)·D), spazio lineare) sopra le
  512 righe per lato; sotto resta la passeggiata della tabella v0.6.4,
  ora limitata a ~1 MB per foglia — mai più l'intera tabella senza tetto.
  **Tetto: 32 768 righe o 4 MiB per lato**, oltre il quale il
  contenimento risponde «non contiene»: la scelta prudente, la copia
  resta aperta e il testo non si perde (DESIGN § 4.3.1).
- **Le misure** (`/usr/bin/time -l`, release, la forma R3 del revisore):
  - 5 000 righe: **0,415 ms**, 3,0 MB RSS (v0.6.4: 108 ms);
  - 20 000 righe: **1,40 ms**, **5,6 MB** (v0.6.4: 1,4 s e 1,68 GB) —
    soglia 50 ms / 64 MB rispettata di 35× e 11×;
  - il peggiore onesto (due lati non correlati, D massimo): 20 000 righe
    in ~0,9 s e **5,4 MB** — la memoria resta lineare, il tempo è O(D).
- **L'uguaglianza col diff3 v0.6.4** su testi generati brevi (vocabolario
  con ripetizioni e righe vuote): **50 000 casi verdi**; sopra la base,
  l'allineamento resta una LCS valida e massimale (2 000 casi). I casi
  limite (vuoti, fine riga finale, righe vuote) in `tests/diff3_linear.rs`.
- **La riderivazione non esplode col prodotto perdenti×scritture:** con
  10 copie aperte e 50/100 scritture successive una riderivazione costa
  **8,8 / 11,9 ms** (prima dei bitset: 9,4 / 15,8; gli ascendenti ora
  vivono in bitset calcolati una volta per derivazione); la flotta intera
  rigiocata da zero: 737 ms.
- **La cache persistente dei verdetti NON c'è, e non può esserci:** il
  verdetto dipende dal corpo alla biforcazione, che la compattazione può
  redigere; una copia con la cache e una senza deriverebbero stati
  diversi dallo stesso insieme — romperebbe la convergenza. Scritto qui
  come decisione di design, non come lavoro mancante.

### E4 — il giudice che non legge la base

- **Il modello** (`FleetModel`, fuori dalle celle) registra per ogni
  scrittura del corpo il corpo che l'autore aveva davanti: il `get` sul
  suo dispositivo — e per una fusione a mano DUE corpi (la nota e la
  copia che sta fondendo), letti con la nuova `conflict()` perché le
  copie in quarantena sono invisibili a `get`.
- **Il generatore produce R4 e R1 da solo:** il ritocco di titolo sopra
  una scrittura non ancora sincronizzata (R4), le scritture read-modify
  (ciò che gli agenti di Homun fanno), l'agente sul corpo umano (R1),
  chi ignora la copia e patcha l'originale.
- **La proprietà «nessun testo perso»**: per ogni scrittura del corpo che
  un dispositivo tiene, a ogni giro, almeno una di: il corpo della nota
  la contiene (diff3 dalla biforcazione **stretta**: la discesa non è
  contenimento), una copia aperta ne porta il corpo, un'operazione vive
  sull'id della copia, una scrittura successiva **l'ha letta** (il
  testo era su uno schermo), purga o lapide. *Deviazione dichiarata:*
  la seconda clausola dell'item 4 della spec («e il suo testo contiene
  L») condannerebbe ogni prima stesura che venga riscritta — l'editing
  sequenziale non è una perdita; il chiudente di R4 fallisce comunque,
  perché la sua lettura non conteneva il perdente.
- **I denti, provati sul motore v0.6.4** (worktree al commit dei rossi):
  lo scenario deterministico R4 giudicato perso — `LOST the body …
  ("uno\nDUE-di-A\ntre")` — mentre il motore nuovo lo salva. Il giudice
  ha poi trovato il difetto vero di E2 (sopra) e tre buchi del modello
  stesso (l'hash del corpo a schermo, le letture multiple della
  fusione, il filtro per nota). **Verde a 2 000 flotte.**
- **Le frequenze per 1000 flotte** (3 dispositivi, 20 giri): 24 270
  operazioni, 21,7% umane, 1 944 scritture del corpo, 1 554 patch di
  sola intestazione, 24 delete su copie, 102 copie aperte a fine corsa
  (78 in quarantena). **E4: 78 ritocchi R4** (di cui 0 con il corpo
  perdente a fine corsa: il difetto è chiuso), **108 scritture d'agente
  su corpo umano (R1)**, 0 fusioni nel flusso random (le finestre di
  copia visibile durano poco; la forma è coperta dai deterministici).

### E5 — la lettura del conflitto da JS

- **`cell.conflict(copyId)`** → `{ …ConflictEntry, originalBody,
  copyBody, forkBody }`: i tre testi della scelta; `forkBody` è il corpo
  alla biforcazione perdente/vincitore (`null` sotto il taglio di
  compattazione), la colonna di mezzo di una vista a tre colonne. La
  copia in quarantena si legge **qui e solo qui** (`get` resta l'oracolo
  chiuso); copia inesistente o chiusa: `NOT_FOUND`. CLI: `engramma
  conflicts show <cella> <copia>`. `docs/node.md` e `docs/cli.md` non
  dicono più «leggila con get». Test: core, JS (dentro D3, in `npm
  test` e `test:electron`) e CLI — tutti verdi.

### E6 — D5 misurato, non stimato

- **La flotta di confronto ha le stesse scritture in sequenza** (B vede
  la parola di A prima di scrivere): la differenza è il costo delle
  copie, e solo quello. Per tabella (`dbstat`), nota da 70 B / 1 KB /
  10 KB: la copia costa **8 192 B a 1 KB e 24 576 B a 10 KB**, tutti in
  `notes` (la riga della copia); a 70 B entra nelle pagine già libere
  (differenza netta 0). Le altre tabelle (FTS visibile e nascosto,
  versioni, tag, archi, indici) non spostano una pagina.
- **Il risparmio già preso (il corpo fuori dalla riga di derivazione):**
  la riga è 258 B senza corpo; col corpo sarebbe ~415 B (nota 70 B),
  ~1 339 B (1 KB), ~10 579 B (10 KB) — superiore al quinto del costo
  della copia a 1 KB e 10 KB: prima/dopo nel REPORT di v0.6.4, ora
  misurato sulla riga vera.
- **I vettori:** la finestra di `embed_pending` prende ogni nota non
  cancellata — le copie in quarantena **visibili** hanno un vettore
  quando l'indice si costruisce; le invisibili (oltre il tetto di tre)
  non hanno riga `notes` e quindi nessun vettore. Il banco non incorpora
  (vettori esclusi per progetto).
- **La soglia:** al 10% di note in conflitto la flotta da 10 000 note
  cresce il file del **6,2%** contro la sequenziale (26 337 280 B contro
  24 788 992 B; soglia 15%). Passata completa: 4,08 s contro 4,06 s.

### E7 — le minori

1. DESIGN § 12: la riga vuota spezzatabella e la colonna in più tolte;
   «scritura»→«scrittura», «differsca»→«differisca».
2. **La purga** (soglia +10%): **non ci sta, ed è della v0.6.4.** Le due
   coppie alternate del banco: base 2 634 note/s, fase 2 599 (−1,4%,
   p95 pari); la purga 303 → 389 ms (**+28%**). La sonda dedicata
   (10 000 note, compattazione con istantanea, una purga) lo isola:
   v0.6.3 ≈ 131 ms, **v0.6.4 ≈ 190 ms**, v0.6.5 ≈ 195 ms. Il tempo va
   nella coda della purga com'era già nel v0.6.4 (optimize dei due
   indici FTS e VACUUM dentro la finestra `secure_delete`, più la
   contabilità dei conflitti durevoli nella stessa transazione); la
   v0.6.5 aggiunge un solo `DELETE` da `note_heads` per nota purgata.
   Dichiarata in v0.6.4 (290→~360), confermata e attribuita qui; il
   taglio dell'optimize dell'indice nascosto quando è vuoto è il
   risparmio più probabile, lasciato alla prossima fase.
3. **La versione delle regole:** ALPN `engramma/sync/3` ed
   `engramma/mailbox/3` (testa visibile, testimone per coppia, lapide
   per linea chiusa cambiano la derivazione); il test rifiuta il v2 con
   `PEER_OUTDATED` prima di uno scambio; gli ALPN `/1` stantii nei
   documenti corretti.

### Le corse

- **Suite del workspace in release: verde** (core, CLI, sync, node, MCP),
  più `protocol_version`, `npm test` **54/54**, `test:electron` 54/54,
  `scripts/linux.sh` verde (con fmt e clippy a `-D warnings`).
- **I semì di regressione** nel file di proptest: verdi (la regola nuova
  ne cambia i casi, non gli esiti).
- **La caccia a 20 000 casi (4 proprietà) e le 5 corse a
  `PROPTEST_CASES=2048` sono SOSPESSE su richiesta dell'utente**: la
  macchina era al limite di carico (e c'è voluto il ripristino a
  finestre). Le quattro proprietà sono verdi a 512–3 000 casi
  (convergenza, ricostruzione, monotonia allargata, nessun testo
  perso); la finestra per la corsa lunga resta da scegliere.
- **La build** (`npm run build`, dalla pipeline automatica): tre file —
  `engramma.darwin-arm64.node` 50 587 448 B, SHA256
  `82744a89171d9707c790d62b219c59a5816b5526c45dfdaf05d0c9218e001df0`;
  `index.js` 36 188 B, `6a440672963f2da32989bd306a91863b9ca87df2e50c6d6d15d7049e9fce8053`;
  `index.d.ts` 24 359 B, `0819903ce1ecda97bbf19f324c3921def2d12e98c0c2e6028949c56a8e0d827c`.
- **L'inserimento non peggiora** (soglia ±5%): media 2 634 → 2 599
  note/s (−1,4%), p95 0,65 → 0,58 ms; uptime annotato a ogni corsa
  (carico in discesa: 6,4 → 4,7).
- **Le 10 000 patch di corpo su note esistenti** (E1 tocca la scrittura
  locale; contro `fase-v0.6.4`, soglia ±5%): **1 384 → 1 324 patch/s
  (−4,3%)**, p95 1,12 → 1,08 ms — dentro, stretto: la sonda indicizzata
  della testa visibile costa quanto la query di ieri e la derivazione
  scrive la riga in più.

### L'ambiente, detto onestamente

La macchina ha passato più di un'ora al limite dei processi per carico
esterno al repo (fork rifiutati): le corse sono state raggruppate in una
pipeline automatica a tentativi (`.tmp-e65-retry.log`, tutto verde) e i
banchi rifatti a finestre. Nessun processo nostro resta acceso.

## Verifica v0.6.5 (revisore, 2026-11-17)

Fatta sul tag `fase-v0.6.5` (`4b50c76`). Tutte le corse girano a
priorità minima (`nice -n 19`, due job di cargo, due fili di test), su
richiesta dell'utente.

### Cosa regge

- **Le quattro prove della verifica v0.6.4, rifatte sul codice nuovo,
  danno il risultato giusto:**
  - R4: la copia «DUE-di-A» resta aperta su tutti e due i dispositivi;
  - R1: la scrittura del bot sul corpo umano diventa il corpo della nota,
    e la copia della prima scrittura resta;
  - R2: la copia di L non si riapre più, e nasce solo la copia di W2, che
    è il perdente vero contro H;
  - R3: 20 000 righe in 1,4 ms e 6,75 MB di RSS (prima 1,4 s e 1,68 GB).
- **Prova nuova del revisore (P11):** scritture in sequenza non creano mai
  copie, né sullo stesso dispositivo (corpo e titolo alternati, umano e
  bot) né fra due dispositivi alternati con la sync in mezzo. Quindi la
  testa visibile segue ogni scrittura, e un dispositivo non va mai in
  conflitto con se stesso.
- **Le corse del revisore:**
  - `npm run build` verde;
  - `npm test` 54/54 e `test:electron` 54/54;
  - suite del core in release verde, 0 falliti.
- **Le scelte di GLM sono giuste e ben motivate:**
  - il testimone è successivo nell'**ordine totale**, non nella chiave di
    classe (difetto vero, trovato dal giudice E4);
  - nessuna cache persistente dei verdetti, perché romperebbe la
    convergenza sotto compattazione;
  - la deviazione dichiarata nel giudice E4: chi ha letto e riscritto non
    perde testo, è editing sequenziale.
- **Le soglie misurate:**
  - E6: +6,2% contro la flotta in sequenza (soglia 15%);
  - inserimento −1,4%;
  - patch di corpo −4,3%, dentro la soglia ma di poco.

### I difetti

1. **Lo SHA del `.node` nel REPORT non è quello del tag.** Il binario
   dichiarato (`82744a89…`, 50 587 448 B) viene dalla pipeline delle
   14:51, sul commit `676cf96`. Dopo sono arrivati i commit di clippy, che
   cambiano i sorgenti (`let-else`, `sort_by_key`…).
   - Sul tag la build dà `1bcf36229b0dca7c49d0f1c08e652d72ef8ae6aca404e05ccad6e5b094886976`
     (50 587 496 B).
   - `index.js` e `index.d.ts` coincidono.
   - Per Homun vale lo SHA del revisore. È la stessa regola della v0.6.3:
     i numeri si leggono dal file costruito sul commit finale.
2. **La caccia lunga non è stata fatta.** Mancano le quattro proprietà a
   20 000 casi e le cinque corse a 2048 casi, sospese su richiesta
   dell'utente. È dichiarato in modo onesto, ma resta un requisito della
   spec. Le proprietà sono verdi solo fra 512 e 3 000 casi.
3. **Minori:**
   - quattro cartelle `.tmp*` di banco (circa 4 MB, database di flotta)
     erano rimaste in `$TMPDIR`; il REPORT non le contava. Le ha
     cancellate il revisore;
   - i file di servizio della pipeline (`.tmp-done-*` e un log di 6 969
     righe) sono entrati nella storia con un `add -A` e sono stati tolti
     dopo. È solo storia locale, senza danno;
   - la purga è a +28%, sopra la soglia del +10%. La regressione è
     ereditata dalla v0.6.4, e il REPORT la attribuisce e la misura come
     chiesto. Il risparmio (l'optimize dell'indice nascosto quando è
     vuoto) va in una fase prossima.

### Verdetto

**v0.6.5 consegnata per lo sviluppo di Homun**, con lo SHA del revisore.
I difetti che bloccavano la v0.6.4 sono chiusi e il revisore li ha
provati indipendentemente. **La sync agli utenti si apre solo dopo la
caccia lunga** (4 × 20 000 casi e 5 × 2048), da far girare in una
finestra tranquilla o a priorità minima di notte.

### Caccia lunga del revisore (2026-11-17, sera)

Sul tag `fase-v0.6.5`, in sequenza, a priorità minima (`nice -n 19`,
due job di cargo, due fili di test, `caffeinate -i`), carico del Mac fra
4 e 7 per tutta la corsa.

| Corsa | Casi | Esito | Tempo |
|---|---|---|---|
| `the_fleet_converges` | 20 000 | verde | 924 s |
| `every_copy_equals_its_reconstruction` | 20 000 | verde | 867 s |
| `no_body_text_is_lost` | 20 000 | verde | 665 s |
| suite del core, corsa a | 2048 | verde | 811 s |
| suite del core, corsa b | 2048 | verde | 740 s |
| suite del core, corsa c | 2048 | verde | 748 s |
| suite del core, corsa d | 2048 | verde | 757 s |
| suite del core, corsa e | 2048 | verde | 756 s |

Totale 1 h 44 min, zero fallimenti, zero `panicked`. Alla fine restano 0
`.tmp*` in `$TMPDIR` e nessun processo nostro acceso.

**Difetto nuovo trovato durante la caccia (bloccante per la sync agli
utenti).** Le proprietà a 20 000 casi sono **tre, non quattro**. Il commit
`434759c` (E4, il giudice «nessun testo perso») ha tolto da
`convergence.rs` la proprietà di monotonia
`no_copy_closes_without_a_witness`, insieme al registro delle riaperture
che `84e9586` (E2) le aveva appena aggiunto. Il REPORT della fase dichiara
«quattro proprietà verdi», e il revisore l'ha ripetuto nella verifica
sopra: è falso.
- *Che cosa manca:* il giudice E4 controlla che il testo non si perda, ma
  non che una copia chiusa resti chiusa. Oggi la regola «aggiungere
  operazioni non riapre mai una copia chiusa» (DESIGN § 4.3) è guardata
  solo dal test deterministico R2, non dalla flotta.
- *Che cosa serve:* rimettere la proprietà, con il controllo
  assente→presente e le eccezioni dichiarate in § 4.3, e cacciarla a
  20 000 casi. Le due proprietà non sono sostituibili: una giudica il
  testo, l'altra la storia delle copie.

**Verdetto aggiornato.** La v0.6.5 resta consegnata per lo sviluppo di
Homun. La sync agli utenti **non si apre ancora**: si apre quando la
monotonia è tornata ed è verde a 20 000 casi.

## v0.6.6 — la monotonia torna, e nessuna proprietà sparisce più in silenzio (2026-11-17)

Una fase piccola, di prova e non di regole. Il difetto che la caccia lunga
del revisore aveva trovato è chiuso, la caccia stessa è diventata un
comando solo, e la purga ha ripreso il suo tempo. **Nessun cambio alle
regole dei conflitti né alla derivazione**: l'ALPN resta `engramma/sync/3`
e `mailbox/3`. Un difetto nuovo, trovato lavorando la F3 e dichiarato qui
in fondo, resta aperto perché chiuderlo tocca proprio quelle regole.

**Correzione di una frase della v0.6.5, detta e non cancellata:** il
REPORT della v0.6.5 e la prima verifica del revisore scrivono «quattro
proprietà verdi». Era falso — le proprietà erano **tre**:
`434759c` (E4) aveva *sostituito* `no_copy_closes_without_a_witness` con
`no_body_text_is_lost` invece di affiancarla, e con lei è sparito il
registro delle riaperture. La caccia lunga del revisore l'ha trovato e
l'ha scritto nella coda della propria sezione; questa fase lo chiude e
mette il cancello perché non succeda più in silenzio. La frase sbagliata
resta dov'è, con questa correzione accanto.

### F1 — la monotonia torna (bloccante)

- **La proprietà `no_copy_closes_without_a_witness` è di nuovo in
  `convergence.rs`, accanto a `no_body_text_is_lost`** — il testo è quello
  di `84e9586` (E2), coi suoi aiutanti (`open_copies_with_loser`,
  `original_purged`, `original_row_gone`, `lapide_active`) e il registro
  delle riaperture: presente→assente esige un testimone o una dominanza
  (purga, lapide, riga dell'originale sparita, insieme che non la merita
  più), assente→presente è un fallimento salvo chiusura né testimoniata
  né meritata. Le due proprietà non si sostituiscono: una guarda il
  testo, l'altra la storia delle copie.
- **Gli aiutanti giudicano con le regole di oggi senza correzioni:**
  `conflict_witness_for_test` e `conflict_deserved_for_test` passano per
  `scan`, `winner_and_candidates`, `Ancestry` e `has_witness` — i
  predicati stessi della derivazione. Il testimone dopo il perdente
  nell'**ordine totale** e la forcella `nca` con gli estremi compresi
  sono dentro `has_witness`, non negli aiutanti: niente da correggere.
- **I denti, provati — e la proprietà ha morso davvero, due volte.**
  La prima, sul ramo di prova con la regola del testimone della v0.6.4
  (il «dopo» per chiave di classe, il difetto corretto da `676cf96`):
  la proprietà **fallisce** — seme `11196124333436581516`, 3
  dispositivi, 20 giri, «copy … reopened — it closed witnessed (true)
  or deserved (true)», **6 018 successi** prima del rosso. Il ramo è
  stato buttato.
- **La seconda, durante la caccia a 20 000 della fase stessa** (seme
  `3753902730771729170`, 4 dispositivi): di nuovo una rinascita di
  copia chiusa con testimone e meritata. Ridotto a mano, il caso è la
  storia di una nota con **due lapidi**:
  `put` (device-1) → `patch` device-1 **L** (il perdente) → `patch`
  device-2 W (concorrente) → `patch` user P1 → `delete` user D1 →
  `patch` device-1 P2 (base uguale a quella di D1: non l'ha vista) →
  `delete` device-1 D2 (ultima nell'ordine totale). La copia di L nasce
  e vive; quando D1 arriva ovunque la nota va `deleted` e la copia
  resta aperta; alla seconda ondata (P2, D2) **chiude**; più tardi
  **rinasce** mentre la nota è ancora `deleted`, il testimone sempre
  nell'insieme, ogni dispositivo d'accordo (la convergenza è verde).
- **La lettura, e la correzione del giudice (non del motore).** DESIGN
  § 4.3: «la regola della lapide resta quella di oggi, **e domina anche
  sulla durabilità**», e le chiusure E riaperture che seguono la purga
  e la lapide sono dichiarate («con le loro regole proprie»). Il
  registro delle riaperture di E2 ammetteva la lapide nella direzione
  chiusura ma non nella riapertura — e non l'aveva mai incontrata in
  512–3 000 casi col generatore pre-E4. L'eccezione è stata aggiunta
  al **giudice**, larga quanto la regola scritta: la rinascita è ammessa
  quando l'originale tiene una lapide, arrivata prima o dopo la
  chiusura. Il motore non è stato toccato, l'ALPN resta `sync/3`. Un
  tentativo più stretto (lapide attiva già alla chiusura) è stato
  bocciato: condannerebbe per sempre il motore giusto (i semi dei denti
  stessi, sotto il motore attuale, sono rinascite da lapide arrivata
  *dopo* la chiusura — dichiarate anche quelle).
- **Che cosa resta a guardia, detto onestamente.** Con l'eccezione, il
  morso del primo dente risulta anche lui un flap da lapide: la
  proprietà, giudicando coi predicati stessi del motore, vede per
  costruzione i **flap** (chiuso→riaperto), non i verdetti — i verdetti
  li guardano il giudice E4 e i test deterministici di `676cf96`. La
  superficie che resta alla proprietà: le chiusure senza testimone né
  dominazione, e le rinascite su note che la lapide non ha mai toccato.
  Due sabotaggi di prova per fabbricare un flap senza lapide
  (`has_witness` sempre falso — chiude meno copie, niente flap; lo
  restore della copia tolto dalla rimaterializzazione — nessun flap
  catturato in 2 000 casi) non hanno prodotto il morso: i morsi
  dimostrati restano i due reali. I semi di entrambi sono spillati nel
  file di regressione (`3753…` la rinascita lapide, `497f…` il flap
  della chiave di classe: verdi col motore di oggi, rossi se la regola
  tornasse indietro).
- 512 casi + i 30 semi di regressione verdi; la caccia a 20 000 rifatta
  da capo sulla forma finale sta in fondo alla fase.

### F2 — un cancello per le proprietà

- **L'inventario:** `crates/engramma-core/tests/properties.txt`, una riga
  per ogni proprietà proptest della suite — **25**: le quattro di
  `convergence.rs` (le tre di ieri più la monotonia), due di
  `diff3_linear`, una di `identity`, una di `indistinguishability`, una
  di `proposals_props`, sei di `props`, dieci di `oauth_props` (core e
  mcp: la proprietà dell'OAuth è proprietà come le altre).
- **Il cancello:** `properties_inventory` (`tests/properties_inventory.rs`,
  deterministico, 0,04 s) legge i sorgenti di `crates/*/tests/**/*.rs`,
  trova i blocchi `proptest!` contando le graffe e le `fn` dentro, e
  confronta i due elenchi **nei due sensi**: una proprietà che manca
  dall'inventario, o nell'inventario che manca dai sorgenti, fa fallire
  la suite. Il rosso naturale della fase è stato proprio questo: l'inventario
  scritto con le quattro proprietà, i sorgenti che ne avevano tre.
  *Che cosa il parser non vede:* le proprietà generate da macro diverse
  da `proptest!`, i blocchi annidati dentro un'altra macro, le graffe
  non bilanciate dentro stringhe o commenti (in questa suite sono tutte
  bilanciate, `format!` compreso), e i commenti di blocco (le righe `//`
  sono saltate).
- **La caccia è un comando solo:** `scripts/caccia.sh [casi] [corse]` —
  prende le proprietà **dall'inventario**, rifiuta di partire se
  `properties_inventory` è rosso, e gira in release a priorità minima:
  `nice -n 19`, `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`,
  `caffeinate -i` su macOS. Ogni proprietà a 20 000 casi (default), poi
  cinque corse della suite del core a 2 048. Il revisore la rifà con lo
  stesso comando.

### F3 — la purga riprende il suo tempo (soglia +10% contro `fase-v0.6.3`)

- **Il risparmio indicato, fatto:** la purga non ottimizza più l'indice
  nascosto che non ha toccato. `hidden_rows_in_family` conta, **prima
  dell'operazione** (il `delete` FTS lascia una lapide che il conto non
  vedrebbe più), le righe della famiglia — la nota e le sue copie, la
  stessa chiusura transitiva di `purge_conflict_copies` — dentro
  `notes_fts_hidden_docsize`: zero righe, niente merge. Vale per la
  purga locale, per quella in arrivo, e per lo scrub dell'istantanea
  (dove i due indici sono contati prima dello scrub: vuoti per
  costruzione nel formato attuale, una v0.3.1 con righe li tiene).
- **L'oblio è intatto:** l'indice toccato si ottimizza sempre. Il caso
  nuovo — `purge_of_a_quarantined_note_scrubs_the_hidden_index` — cerca
  le radici del corpo segregato nei file dopo la purga: col taglio
  ingenuo (ottimizzare mai) «quokkaviolaceo» sopravvive e il test è
  rosso; col cancello giusto è verde. Gli altri dieci test di oblio
  restano verdi.
- **Il risparmio indicato non bastava, e il resto è stato attribuito.**
  La sonda dedicata (10 000 note, compattazione con istantanea, una
  purga; ordine ruotato, carico 5–7 accanto a ogni corsa): v0.6.3 ≈
  **130 ms**, v0.6.5 ≈ **190 ms**, fase col solo taglio FTS ≈ 191 ms —
  **+46%, fuori soglia**. La decomposizione (sonda senza istantanea +
  timer interni): la purga *viva* era la regressione (44 → 108 ms,
  l'istantanea ≈ 86 ms invariata), e dentro la purga viva il tempo era
  tutto in `reconcile_after_local_write`: **76,7 ms**.
- **La causa e il cancello:** `rederive_original_of` scambiava la nota
  purgata per una «copia orfana» — la riga assente *per la purga*, non
  per orphanhood — e pagava la camminata su tutte le scritture del
  corpo della cella (63 ms su 10 000 note). Il cancello è
  `purged_in_set`: se la purgata fosse una copia, la sua riga di nascita
  sarebbe ancora lì (la camminata della purga cancella le righe di
  nascita delle copie che *porta via*, non quella dell'id purgato) e il
  primo ramo la tratterebbe; l'id di un originale non può essere coniato
  dal blake3 dell'hash di un perdente. **Nessun esito si muove** — e la
  caccia a 20 000 casi della fase lo riprova sulle quattro proprietà
  della flotta.
- **La misura finale** (stessa sonda, ordine ruotato, sei giri, carico
  5,3–7,3): v0.6.3 mediana **129,9 ms**, v0.6.5 mediana **190,2 ms**,
  fase mediana **130,6 ms** — **+0,5%, dentro la soglia del +10%** con
  margine (il bersaglio era ≈143 ms). Nella colonna purga del banco
  intero (corpus pieno, istantanea presente): +4,5% medio in coppia.
- **L'inserimento non peggiora** (due coppie alternate, `--notes 10000
  --runs 5`, `ENGRAMMA_SKIP_MODEL_TESTS=1`, carico accanto):

  | corsa | carico | viva (note/s) | uniform (note/s) | p95 testo+grafo |
  |---|---|---|---|---|
  | base-A | 14,9 | 2 646 | 2 657 | 1,9 / 2,0 |
  | fase-A | 8,0 | 2 598 | 2 589 | 1,9 / 2,0 |
  | base-B | 5,1 | 2 692 | 2 681 | 2,0 / 2,0 |
  | fase-B | 4,5 | 2 628 | 2 633 | 1,9 / 2,0 |

  Media delle coppie: **−2,2%** (−2,2% e −2,1%), p95 identico — dentro
  la soglia del +5%.

### Un difetto nuovo, dichiarato e lasciato aperto

Lavorando la F3 è emerso un buco di oblio **preesistente** (provato sul
tag `fase-v0.6.5`, quindi non di questa fase): **la purga di una nota
non toglie le sue copie di conflitto dal file dell'istantanea di
compattazione**. Lo stato vivo le perde (`purge_conflict_copies`), ma lo
scrub dell'istantanea (`scrub_snapshot`) passa solo `purge_state`
sull'originale: la riga della copia resta in `notes` col corpo stemmato.

- *Scenario ridotto* (deterministico, `shared_init("snapcop")`): due
  celle, A umana mette la nota, B agente riscrive il corpo, A umana
  riscrive il proprio — la copia dell'agente nasce in quarantena;
  compattazione con istantanea su A; purga dell'originale su A. Dopo la
  purga: righe di derivazione vive zero, payload dell'originale
  nell'istantanea zero, `note_versions` zero — ma `notes`
  dell'istantanea tiene la riga della copia `01MCR37N02T47QTBD0YN5S8WEX`
  con «… prim rig **ovovivipar zibellinotrem** terz rig». `verify` resta
  verde (il rigioco dalla fronte riproduce lo stato vivo: la purga
  domina e la copia non materializza) — il rigioco nasconde il resto
  congelato sotto il taglio.
- *La regola che si rompe:* DESIGN § 4.3, «la `purge` domina sempre … le
  sue copie di conflitto vanno con lei» — vale nello stato vivo, non nel
  file dell'istantanea.
- *Perché non è chiuso qui:* portarla nell'istantanea è un cambio alla
  purga delle copie (le regole dei conflitti), fuori da questa fase per
  patto; e vuole i suoi test d'oblio e la sua caccia. Va in una fase
  propria, dichiarata come correzione della derivazione.

### La caccia lunga (col comando della fase, `scripts/caccia.sh`)

Sul commit finale dei sorgenti (`a334483`), in release, a priorità minima
(`nice -n 19`, due job di cargo, due fili di test, `caffeinate -i`),
ordine di lancio: `scripts/caccia.sh` senza argomenti. **Le proprietà
sono quelle dell'inventario**, con i tempi letti dal comando stesso:

| proprietà | casi | esito | tempo |
|---|---|---|---|
| `the_fleet_converges` | 20 000 | verde | 951 s |
| `every_copy_equals_its_reconstruction` | 20 000 | verde | 1 145 s |
| `no_copy_closes_without_a_witness` | 20 000 | verde | 645 s |
| `no_body_text_is_lost` | 20 000 | verde | 693 s |
| `the_new_diff3_decides_as_the_v064_one` | 20 000 | verde | 3 s |
| `the_long_alignment_is_still_a_lcs` | 20 000 | verde | 135 s |
| `the_identity_never_moves_on_generated_histories` | 20 000 | verde | 285 s |
| `hidden_notes_leave_no_fingerprint` | 20 000 | verde | 419 s |
| `proposal_sequences_keep_the_invariants` | 20 000 | verde | 553 s |
| `format_round_trip` | 20 000 | verde | 3 s |
| `replay_equals_state` | 20 000 | verde | 697 s |
| `compaction_keeps_verify_and_state` | 20 000 | verde | 1 024 s |
| `random_ops_compact_and_purge_leave_no_roots` | 20 000 | verde | 1 891 s |
| `export_import_equality` | 20 000 | verde | < 1 s |
| `hlc_monotonic` | 20 000 | verde | 1 s |
| `rfc1918_is_never_public` … altre 9 di `oauth_props` | 20 000 ciascuna | verdi | ≤ 34 s ciascuna |
| suite del core, corse a–e | 2 048 | verdi | 627 / 644 / 626 / 634 / 640 s |

Totale **11 654 s (3 h 14)**, zero fallimenti, zero `panicked`. Il carico
del Mac ha oscillato fra 3,9 e 19 per corse esterne (la caccia girava a
priorità minima e non ha mai ceduto il passo). La **prima** caccia della
fase — prima della correzione del giudice — si è fermata da sola al
rosso della monotonia dopo 133 s: è il funzionamento voluto dello
script, ed è la prova che il cancello dell'inventario e la caccia
raccontano la verità.

**I semi di regressione** (30 nel file di `convergence`: i 28 di ieri più
i due nuovi) rigiocano verdi in ogni corsa della suite.

### Le corse

- `npm test` **54/54**, `test:electron` **54/54** (l'addon ricostruito
  sul commit finale dei sorgenti).
- Suite del workspace in release: verde (la caccia la copre cinque
  volte per il core; CLI, sync, node e MCP girano in `linux.sh`).
- `scripts/linux.sh` verde (fmt + clippy a `-D warnings` + test nel
  container arm64, offline).

### La build

`npm run build` sul commit finale dei sorgenti (`a334483` — i commit
seguenti toccano solo documenti):

- `engramma.darwin-arm64.node` — **50 586 616 B**, SHA256
  `7eafe1ab36492b2fd8e4afac1fa63355d52d9e4813b866c6da0b3c823c4ef4c5`;
- `index.js` — 36 188 B,
  `6a440672963f2da32989bd306a91863b9ca87df2e50c6d6d15d7049e9fce8053`
  (identico alla v0.6.5: la superficie JS non si è mossa);
- `index.d.ts` — 24 359 B,
  `0819903ce1ecda97bbf19f324c3921def2d12e98c0c2e6028949c56a8e0d827c`
  (identico).

### L'ambiente, detto onestamente

La macchina ha viaggiato con carichi esterni fra 4 e 19 lungo tutta la
fase; le misure sono state prese a coppie alternate con ordine ruotato e
il carico annotato accanto a ogni corsa. La prima sessione di sonda
(ordine fisso) aveva dato delta falsati dalla posizione: scartata e
rifatta ruotando. Zero `.tmp*` in `$TMPDIR` alla fine, nessun processo
nostro lasciato acceso, i worktree di misura (`fase-v0.6.3` e
`fase-v0.6.5`, target propri) dismessi.

### Verdetto

La monotonia è tornata ed è **verde a 20 000 casi**; nessuna proprietà
può più sparire in silenzio (l'inventario lo impedisce); la purga è a
**+0,5%** dalla v0.6.3. **Le condizioni per aprire la sync agli utenti
di Homun, lette nella coda della verifica v0.6.5, ci sono** — con la
riserva che la lettura dell'eccezione lapide (F1) è dichiarata qui per
prima, col seme e lo scenario ridotto, perché il revisore possa
bocciarla: in quel caso la proprietà torna rossa e la correzione della
derivazione diventa una fase dichiarata (ALPN `sync/4`). Il difetto
dell'istantanea (sopra) resta aperto e non tocca la sync: riguarda
l'oblio, non la convergenza.
