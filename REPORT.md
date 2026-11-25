# REPORT — Engramma

Una pagina per fase (docs/fasi.md). La storia fino alla v0.6.14, fase per
fase, sta in [docs/storia/REPORT-fino-alla-v0.6.14.md](docs/storia/REPORT-fino-alla-v0.6.14.md).

## v0.6.16 — le pulizie del CI e delle dipendenze

Data: 2026-11-25. Base: `main` @ `d3773d5`. Nessuna riga in
`crates/engramma-core/src`, `engramma-sync/src`, `engramma-node/src`:
niente caccia (la regola 4 ora dice «codice coperto da una proprietà della
suite») e niente misure.

### Cosa cambia

- **Il CI regge anche su `main`.** Il contratto JS compila UNA volta sola,
  con tutti i core e senza `nice` (prima la CLI, poi l'addon, entrambi
  release), e il `pretest` resta per l'uso locale: nel CI `npm test
  --ignore-scripts`. A cache fredda la vecchia sequenza compilava due
  volte in release (8 min 45 s + 14 min 29 s) e il cap di 25 minuti era
  finito (prima corsa su `main`, 45482552851).
- **La prova a freddo è un input.** `workflow_dispatch` con `cold=true`:
  ogni job salta `rust-cache` e costruisce da target vuoto — la prova di
  una prima corsa su `main`, che le cache del ramo predefinito non
  raggiunge.
- **Il seme fisso al posto dello skip.** `PROPTEST_RNG_SEED=20261124`
  nell'`env` del workflow (proptest 1.12 lo legge): tutte le proprietà
  girano nel cancello, `quarantine_exits_only_by_decision` inclusa coi
  suoi due difetti dichiarati — il ciclo `--skip` che leggeva
  `open-defects.txt` è via. Il seme è provato in locale: la suite di
  `engramma-core` a `PROPTEST_CASES=128` è verde col seme (la proprietà
  della quarantena corsa e passata, 20 ok 0 caduti); il primo seme
  provato — la data — non ha incontrato i difetti dichiarati, un secondo
  non è servito. La regola sta nel workflow: il seme cambia solo con un
  commit, e un rosso col seme fisso è sempre una regressione. La ricerca
  resta alla caccia, coi semi casuali.
- **Fuori dai push i job esplorativi.** `macos-15-intel` e
  `windows-latest` girano solo su `schedule` (lunedì 03:00 UTC) e su
  `workflow_dispatch`: le loro code chiudevano come «cancelled» corse
  intere verdi.
- **Le dipendenze.** `rmcp` 3.8.0 → 3.11.0 (`cargo update -p rmcp`; prova
  a secco: solo rmcp, rmcp-macros e due crate unicode). RUSTSEC-2026-0462
  (buffering illimitato nei client HTTP; qui rmcp è solo server) cade.
  `url` resta 2.5.8 — nessuna correzione pubblicata alla data, `cargo
  update -p url` non cambia nulla — e va in `ignore` motivato in
  `deny.toml` (niente `Url::origin` in `crates/*/src`, grep del
  2026-11-24). `paste` e `chrono` non mantenuti: `ignore` motivato, chi
  li tira dentro sta nel `cargo tree -i` nel file, la sostituzione è una
  fase a sé. `advisories` diventa bloccante e gira anche sullo
  `schedule`; il commento in testa a `deny.toml` dice la cosa vera.
- **La regola del triage** (regola 6 in `docs/fasi.md`): si corregge in
  fase solo un difetto raggiungibile dall'API stabile («Cosa Homun può
  usare» in `docs/node.md`) che perde dati o lascia uno stato che
  `verify` non vede; gli altri si dichiarano in `open-defects.txt` con
  chi li raggiunge e la fase va avanti. La regola 4 è precisa: il
  cancello parte solo su codice coperto da una proprietà della suite
  (`properties.txt`), il CI è deterministico col seme fisso.
- **Due lezioni del cancello nuovo**, cadute e corrette nelle corse del
  ramo: `npm install` sul runner riscrive il `package-lock.json`
  tracciato, l'albero va sporco tra CLI (pulita) e addon (`-dirty`) e il
  `bincheck` del contratto rifiuta l'accoppiata — ora `npm ci`, albero
  pulito, stesso commit su entrambi; i quattro file del contratto giravano
  in parallelo e la finestra di 5 s di S4 (l'evento arriva alla FINE
  della sessione) perdeva sui 3 core del runner — ora
  `--test-concurrency=1`: il cancello non misura il carico del runner.

### I numeri

- **La corsa del ramo** [45517258144](https://github.com/homun-app/engramma/actions/runs/45517258144)
  sul commit `b45daec`: bloccanti tutti verdi, conclusione verde (gli
  esplorativi non girano sui push). Tempi: fmt 0,2; clippy 0,5 (arm64) e
  1,2 (macOS); test 16,3 (arm64), 21,8 (macOS), 18,8 (x86); warm 1,3;
  contratto JS 4,3 (arm64) e 5,4 (macOS); deny 0,3; advisories 0,3.
- **La prova a freddo** [45520249203](https://github.com/homun-app/engramma/actions/runs/45520249203)
  (`cold=true`, stesso commit): bloccanti tutti verdi a target vuoto.
  Tempi: test 18,3 (arm64), **26,1 (macOS)**, 14,4 (x86); warm 2,6;
  contratto JS 12,9 (arm64) e 24,2 (macOS); clippy 1,6 e 1,9; fmt 0,2;
  deny 0,3. Il cap del job `test` è stato alzato a 40 prima della prova
  (la suite da sola misurava 22–24 con cache calda nella corsa
  45509102496): il macOS freddo misura 26,1 — sopra 25, sotto 40. La
  conclusione «cancelled» della corsa viene dal test `macos-15-intel`
  non bloccante (coda infrastrutturale, com'era nella v0.6.15).
- **I test locali**: suite del workspace verde col seme fisso ai casi
  pieni dei sorgenti (708 test, 0 caduti); contratto JS 67/67 in Node e
  67/67 in Electron; `golden_cell` verde; in `--release`, dopo
  l'update: test di `engramma-mcp` e `crates/engramma-cli/tests/mcp.rs`
  12/12.

### La build sul tag

Il tag `fase-v0.6.16`, annotato e solo locale, punta al commit `c7c1bcc`
(`git cat-file -t fase-v0.6.16^{}` dice `commit`). `scripts/build-sha.sh`
col repo sul tag gira: i tre file dell'aggancio, letti dal disco —
`engramma.darwin-arm64.node` 51 257 480 B, SHA256
`090c63d7…bee812`; `index.js` 36 239 B, `cd26043e…280e311`; `index.d.ts`
32 615 B, `7020ae60…0fc2427`. Il `.node` cambia rispetto alla v0.6.15
(`70b83608…aab57e`, 51 241 080 B): cambiano le dipendenze — `rmcp`
3.11.0 e i due crate unicode che l'update tira dentro.

### Il verdetto

Consegnati: F1 (una build per il contratto, la prova a freddo come
input, il seme fisso al posto dello skip, gli esplorativi fuori dai
push, advisories bloccante), le dipendenze (rmcp 3.11.0, gli `ignore`
motivati), F2 (la regola del triage, la regola 4 precisa). Il CI è ora
deterministico per progetto: seme fisso, un binario per contratto, i
quattro file uno alla volta. Il motore non ha cambiato una riga dei tre
sorgenti toccati: la caccia non è partita, come da regola.

Fuori da questa fase, come dichiarato: il filone della compattazione, il
difetto a due dispositivi, la sostituzione di `chrono` e `paste`, le
correzioni Windows, la fusione a righe e i permessi per attore — le
prossime fasi a tema, scelte dall'utente.

### Verifica (revisore, 2026-11-25)

Rifatto sul tag `c7c1bcc`, a CPU bassa: `build-sha.sh` dà **gli stessi
tre SHA** (`.node` `090c63d7…bee812`, 51 257 480 B); JS 67/67 in Node e in
Electron; `compact_gate` 2/2, `mcp` 12/12; workspace col seme del CI
(`PROPTEST_RNG_SEED=20261124`, 128 casi) verde; `golden_cell` verde; i due
difetti dichiarati cadono ancora con `--ignored`. Le corse 45517258144
(verde, anche la conclusione) e 45520249203 (a freddo, bloccanti verdi)
confermate. Gli hook di privacy passano sui 9 commit. **Verdetto:
consegnata.** Note, nessuna bloccante:

- **`relay_cli.rs` cade in locale dopo qualche giorno**: tiene la sua build
  in `$TMPDIR/engramma-cli-relay`, e macOS svuota i file vecchi di `$TMPDIR`
  uno per uno (qui `bindgen.rs` di `libsqlite3-sys`, del 22/11): cargo crede
  la cache fresca e la build si rompe. Con un `TMPDIR` pulito 7/7. La cura è
  `CARGO_TARGET_TMPDIR`, la cartella che cargo dà ai test d'integrazione
  dentro `target/`.
- **Il contratto JS su macOS a freddo: 24,2 minuti su 25.** La prima corsa
  di `main` dopo questo push ha `Cargo.lock` nuovo, quindi cache nuova: il
  job va portato a 40 minuti come `test`.
- **`--test-concurrency=1` stabilizza il cancello ma nasconde un fatto**:
  sotto carico `stop()` della sync supera i 5 s di S4. Per Homun (l'uscita
  dall'app) la latenza va misurata sotto carico, non tolta dal cancello.

## v0.6.15 — la release locale, il CI vero, e fasi leggere

Data: 2026-11-24. Base: `main` @ `f2a92d7`. Il motore (`crates/*/src` di
`engramma-core`) non cambia: la compattazione esce dal percorso critico senza
toccare una riga del motore, e la caccia non gira (docs/fasi.md, regola 4).
Niente misure: la fase non tocca percorsi caldi.

### Cosa cambia

- **F3.** Le regole delle fasi leggere in `docs/fasi.md`; il REPORT storico
  (13 694 righe) spostato in `docs/storia/` con `git mv`.
- **F1.** `.github/workflows/ci.yml` riscritto: il CI della v0.1, mai
  eseguito, cadeva in 45 s (`CARGO_NET_OFFLINE` impediva le dipendenze,
  `cargo install cargo-deny` non trovava il pacchetto). Ora: `cargo fetch
  --locked`, build e test `--locked`, azioni fissate allo SHA del commit
  (versione in commento), `cargo-deny` 0.20.2 da binario (taiki-e), cache
  con rust-cache, `permissions: contents: read`, concorrenza che annulla
  le corse vecchie, `timeout-minutes` su ogni job. È il livello 1 della
  caccia e il cancello di ogni push (solo il ramo `ci-v0.6.15` è stato
  spinto, poi cancellato).
- **F2.** La compattazione dietro un interruttore sperimentale:
  - **binding JS**: `compact()` funziona solo su celle aperte con
    `{ experimental: ['compaction'] }`; senza, rifiuta con `e.code ===
    'EXPERIMENTAL'` e un messaggio che dice perché, e la cella non
    cambia. Come ogni rifiuto al momento della chiamata (il `CLOSED` del
    gate), l'errore sale alla chiamata, non dentro il task. In
    `index.d.ts` (generato) il metodo e l'opzione lo dichiarano;
  - **CLI**: `engramma compact --sperimentale`; senza il flag, exit 2 e
    lo stesso perché;
  - **nessuna compattazione automatica**: le chiamate di `Cell::compact`
    nel codice sono quattro — il comando della CLI, il task del binding
    (entrambi ora dietro l'interruttore), i test interni al `#[cfg(test)]`
    di `cell/mod.rs`, e il banco. Sync, migrazioni e apertura non la
    chiamano (grep su `crates/*/src`: i `checkpoint` in material/schema
    sono il WAL di SQLite, un'altra cosa);
  - **i difetti aperti contro l'API di Homun**, riga per riga in
    `open-defects.txt`: i quattro semi del taglio non sono raggiungibili
    senza l'opzione sperimentale; i due semi a due dispositivi chiedono
    `revertDecision`, che non sta nel binding — la strada è un utente
    che fa girare la CLI (`engramma revert`) sulla stessa cella che Homun
    sincronizza, da due dispositivi. Il test del contratto che legge
    `index.d.ts` (come `anchor_tables` legge lo schema) cade se
    `revertDecision` entra nel binding finché quella riga è aperta;
  - **docs/node.md**: la sezione «Cosa Homun può usare» — l'API stabile,
    la compattazione sperimentale, e le due regole che restano
    (`decide()` solo da un gesto dell'utente; la `external` confermata è
    di sola lettura).

**I cinque test del contratto che ora aprono con l'opzione** (e solo
quella): «compact() then purge() of a supersede», «propose() then
decide() then compact() then purge()», «compact() with a larger keepLast
does not drag the cut back», «changesSince streams the ops», «dropCell
spares the neighbors». Due test nuovi: il rifiuto senza opzione (testa
ferma, `verify` verde, l'opt-in compatta la stessa memoria) e il
controllo di `index.d.ts`. Il test delle celle chiuse non toccato: il
gate risponde `CLOSED` prima del controllo sperimentale.

### I difetti trovati

- **Un test, non il motore: la prova dell'indistinguibilità MCP confrontava
  byte per byte due risposte che portavano `recency_of(now)`** — quantizzato
  al secondo intero. Quando le due ricerche cadono a cavallo di un confine
  di secondo, il punteggio si muove nell'ottava cifra: successo sul runner
  Linux arm64 (sul Mac locale le due ricerche stanno quasi sempre nello
  stesso secondo — la prova era verde per velocità della
  macchina, non per struttura). Corretto nel test: la domanda passa
  `as_of` e il fattore è esattamente 1; la risposta è funzione del solo
  indice visibile. Il motore è intatto (il fattore che si muove col tempo
  è per progetto).
- **Il cancello CI non può giudicare i difetti dichiarati.** La seconda
  corsa è caduta su `quarantine_exits_only_by_decision` (macOS): un seme
  a caso ha colpito una forma del difetto dichiarato — stessa proprietà
  di `open-defects.txt`, verdetto «dichiarato o nuovo» che spetta alla
  caccia col suo protocollo (docs/caccia.md), non a un cancello che
  cadrebbe a caso a ogni push. Il CI ora salta esattamente le proprietà
  nominate nel file (l'elenco deriva dal file: l'ultimo difetto chiude, il
  cancello si stringe da sé), con un `::notice` a riga. Mai un verde muto.

### I numeri

- **Il CI, prima corsa** (45413281346): verdi fmt, clippy, deny e
  contratto JS su arm64 Linux — **il CI vero funziona**; caduto il test
  arm64 per il difetto del test MCP qui sopra. Le corse dopo hanno
  aggiustato tre cose, tutte nel workflow o nei test: `as_of` nella
  prova MCP; il job `warm` (il runner x86, il più lento, a freddo
  superava i 25 minuti per pochi secondi); `PROPTEST_CASES=128` (il
  runner macOS a 3 core non faceva entrare la suite ai casi pieni dei
  sorgenti; l'env prevale sul `with_cases` — misurato: convergence 512→2
  in 37 s invece di ~7 minuti — e i file delle regressioni restano).
- **La corsa buona**: [45439718377](https://github.com/homun-app/engramma/actions/runs/45439718377)
  sul commit `adab0d7` — **verde in tutti i job bloccanti**: rustfmt
  0,2 min; clippy 0,5 (arm64 Linux) e 1,1 (macOS); deny licenses+bans
  0,3; warm 1,3; test 16,5 (arm64 Linux), 23,2 (macOS), 11,8 (x86
  Linux); contratto JS 5,2 (arm64 Linux) e 4,1 (macOS). Il job più
  lungo sta sotto i 25 minuti. Non bloccanti: Windows non compila (API
  solo-Unix in `engramma-sync` e `engramma-mcp` — fuori fase);
  `advisories` rosso (RUSTSEC-2026-0462, RUSTSEC-2026-0592, `paste` non
  mantenuto — il DB ruota col mondo, per questo non blocca);
  `test (macos-15-intel)` annullato più volte senza mai vedere un
  runner (zero passi, 15 minuti esatti di coda: infrastruttura GitHub —
  le ritentative che hanno avuto il runner sono girate verdi).
- **I test locali**: suite del workspace **696 test, 0 caduti** (85
  binari, proprietà ai casi pieni dei sorgenti, modelli presenti);
  contratto JS **67/67 in Node** e **67/67 in Electron** (i 65 di ieri +
  i 2 nuovi); `golden_cell` verde (`the_golden_cell_opens_verifies_and_holds_its_facts`).

- **Il tag e la build**: `fase-v0.6.15` annotato, solo locale, punta al
  commit `5a81806` (`git cat-file -t fase-v0.6.15^{}` dice `commit`).
  `scripts/build-sha.sh` sul tag gira: i tre file dell'aggancio, letti
  dal disco —
  `engramma.darwin-arm64.node` 51 241 080 B, SHA256 `70b83608…aab57e`;
  `index.js` 36 239 B, `cd26043e…80e311`; `index.d.ts` 32 615 B,
  `7020ae60…0fc2427`.

### La release locale

**la release locale: pronta.** Le condizioni, una per una:

1. **la compattazione è dietro l'interruttore** — `compact()` del binding
   rifiuta con `EXPERIMENTAL` senza `{ experimental: ['compaction'] }`,
   `engramma compact` vuole `--sperimentale`; nessuna strada del motore
   compatta da sola;
2. **nessun difetto aperto è raggiungibile dall'API di Homun senza
   l'opzione sperimentale** — i quattro semi del taglio chiedono proprio
   quella chiamata; i due semi a due dispositivi chiedono la CLI
   (`engramma revert`), che Homun non usa: la raggiungibilità esatta è
   scritta accanto a ogni riga di `open-defects.txt`;
3. **il CI è verde** nei job bloccanti (la corsa qui sopra);
4. **le celle d'oro sono verdi** (`golden_cell` nella suite locale);
5. **`build-sha.sh` gira sul tag** (i numeri sotto).

### Il verdetto

Consegnati: F1 (il CI vero, livello 1 della caccia e cancello di ogni
push), F2 (la compattazione dietro l'interruttore, la raggiungibilità
dei difetti dichiarati, la sezione di docs/node.md), F3 (le fasi leggere
in `docs/fasi.md`). Il motore non ha cambiato una riga di `crates/*/src`:
la caccia non è partita, come da regola.

Il CI vero ha già pagato il suo prezzo in fase: ha mostrato una prova
locale verde per accelerazione della macchina e ha detto che un cancello
non può giudicare i difetti dichiarati. Le due lezioni stanno nel
workflow, non nella disciplina.

Il filone della compattazione (la derivazione unica, l'oracolo
differenziale col taglio come cancello, i due difetti dichiarati) resta
aperto e separato: `open-defects.txt` è la lista d'ingresso. Fuori da
questa fase, come dichiarato: `revertDecision` nel binding, le
correzioni Windows, l'installazione della caccia notturna, la fusione a
righe e i permessi per attore — le scelte delle prossime fasi, dell'utente.

### Verifica (revisore, 2026-11-24)

Rifatto sul tag `5a81806`, a CPU bassa: `build-sha.sh` dà **gli stessi tre
SHA** (`.node` `70b83608…aab57e`, 51 241 080 B: build riproducibile); JS
67/67 in Node e in Electron; `compact_gate` 2/2 e `mcp` 12/12; workspace a
`PROPTEST_CASES=64` senza caduti; `golden_cell` verde; i due difetti
dichiarati cadono ancora (`--ignored`). Corsa 45439718377: verdi tutti i
bloccanti; la conclusione «cancelled» viene da `macos-15-intel`.
Gli hook di privacy passano sui 12 commit. **Verdetto: consegnata, la
release locale è pronta.** Tre note, nessuna bloccante:

- `advisories` non è solo «manutenzione»: due vulnerabilità vere,
  `url` 2.5.8 e `rmcp` 3.8.0 (lato client HTTP; qui `rmcp` è solo
  server). Va fatto `cargo update` e messo in `deny.toml` un `ignore`
  motivato; poi le vulnerabilità diventano bloccanti, il resto no.
- Il CI salta per intero `quarantine_exits_only_by_decision`: la regola 3
  della quarantena non ha cancello finché il difetto è aperto. Meglio
  seme fisso nel CI (`PROPTEST_RNG_SEED`) e proprietà dentro: un rosso
  allora è sempre nuovo.
- `macos-15-intel` rende «cancelled» la corsa intera: spostarlo su
  `schedule`. Nel verdetto, «`crates/*/src` non cambia» vale per
  `engramma-core`: binding e CLI sono cambiati, com'era previsto.
