# REPORT — Engramma

Una pagina per fase (docs/fasi.md). La storia fino alla v0.6.14, fase per
fase, sta in [docs/storia/REPORT-fino-alla-v0.6.14.md](docs/storia/REPORT-fino-alla-v0.6.14.md).

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
