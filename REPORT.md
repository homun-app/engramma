# REPORT — Engramma

Una pagina per fase (docs/fasi.md). La storia fino alla v0.6.14, fase per
fase, sta in [docs/storia/REPORT-fino-alla-v0.6.14.md](docs/storia/REPORT-fino-alla-v0.6.14.md).

## v0.6.19 — la CLI parla inglese

Data: 2026-11-27. Base: `main` @ `a5a61f2` (la `0.6.18-beta.1`). Esce come
`0.6.19-beta.1`. Tema: tutto quello che un utente, un client o uno
sviluppatore legge è in inglese — la CLI, la pagina di revisione, il server
MCP e le pagine OAuth, la prosa del core, i rifiuti di sync e di vault, gli
errori del binding Node. Il motore cambia in un punto solo, il revert di
un rifiuto (qui sotto, «La caccia»); niente misure (docs/fasi.md). Tre tappe col revisore in mezzo: l'inventario e il cancello,
la traduzione, la chiusura. I codici restano identici: `e.code`, gli exit
code, le chiavi e i valori enumerati del JSON, i codici OAuth.

### L'inventario e i conteggi

| Dove | L'inventario | Tradotti |
|---|---:|---:|
| `engramma-cli/src` (5 file) | 318 | 318 |
| `engramma-mcp/src` (3 file) | 67 | 73 |
| il core (5 file) | 17 | 17 |
| `engramma-sync/src` (7 file) | 60 | 62 |
| `engramma-vault/src` | 19 | 17 |
| `engramma-node/src` | 46 | 46 |
| messaggi dei lock (sync, binding) | 25 | 27 |
| **In tutto** | **552** | **560** |

Fuori inventario, tradotti lo stesso: cinque descrizioni OAuth (`oauth.db`
al posto di una frase), un esempio di schema, due righe di traccia, due
messaggi di lock; le due voci di vault che mancano sono il suffisso
`.conflitto`, che resta. I test: 22 file cambiati (il cancello, 12 della
CLI, 3 del core, 1 di sync, 2 di vault, 3 JS); ogni asserzione segue la
parola inglese equivalente, nessuna indebolita.

Il cancello (`crates/engramma-cli/tests/english.rs`): sulla base **130
superfici su 159 in italiano** («inizializzato: …», «aggiunta: …»), verde
ora, **132 lette** — il totale cala perché una pagina con la lingua
sbagliata contava doppio, e il verde ora dice quante ne ha lette.

### Le decisioni del revisore, e i difetti

- La prosa che diventa dato (le ragioni nel registro) è inglese per le
  decisioni nuove; le celle vecchie tengono l'italiano e `decisions` lo
  mostra. Il primo dispositivo si chiama `laptop`.
- La pagina: `lang="en"`, le route inglesi, le vecchie GET in 308, le POST
  e i campi solo rinominati; i dati di prova in inglese; le tre promesse
  nel cancello; i tre testi che il cancello non vede, a mano.
- **`via` è un valore enumerato del contratto** (`sync run --json` e
  `via?: string` del binding), non prosa: la traduzione
  l'aveva fatto diventare `direct`. Ora il JSON e il binding tengono
  `"diretta"` per sempre, la riga per umani dice `direct`, e due test lo
  fissano (nella CLI e in `unsent.rs`). Il CHANGELOG non lo nomina: non
  cambia.
- **I tre difetti dell'inventario (§11), corretti.** §11.7: la pagina delle
  decisioni rendeva il markup di un URL — il blocco ora nasce nel server
  (`Review::flash`), mostrato una volta, con il test del `<b>` escapato.
  §11.9: il servizio sync riconosceva il rifiuto SAS dalla parola
  «rifiutata» — variante tipata `Error::SasRefused`, con test. §11.6:
  `/token` rispondeva `oauth.db` come codice — `server_error`. Di
  passaggio: i 43 spazi (§11.8) e 18 in `service.rs`; due commenti del
  binding che finivano nell'`index.d.ts` in italiano.

### Cosa resta italiano, e perché

- **Dati salvati**: `conflitto` (tag, relazione del grafo, motivo nel
  registro) e il suffisso `.conflitto`: tradurli sarebbe una migrazione.
- **Valori enumerati del contratto**: `via` vale `diretta` o `relay`.
- **Dello stemmer e dei codici**: le stopword di `text.rs` e le parole dei
  codici di abbinamento (`words.rs`): cambiarle cambia il protocollo.
- **Le ragioni che la beta ha già scritto** nei registri delle celle
  vecchie; **il nome dell'utente** (note, celle, dispositivi, etichette).
- **I documenti italiani** (`DESIGN.md`, `sync.md`, `node.md`, `mcp.md`):
  il glossario di `docs/cli.md` fa da ponte.

### Le prove

Suite del workspace col seme del CI (`PROPTEST_RNG_SEED=20261124`):
**verde — 88 binari, 726 test, 0 caduti** (12 ignorati: il difetto a due
dispositivi della v0.6.14 non lo è più); in essa il cancello verde (132 superfici) e
`golden_cell` verde. Contratto JS del binding, sull'addon ricostruito:
**67 pass, 0 falliti**. `cargo fmt --all --check` e `cargo clippy
--all-targets --locked -- -D warnings` puliti.

**La prova sul binario** (release, cartella temporanea, dati sintetici
inglesi: Dijkstra, Liskov, Thompson; le proposte le fa l'assistente, la
CLI non propone). Le scene delle catture del sito, uscita accorciata:

```console
$ engramma search shortest
1. [active] Dijkstra and shortest paths (XBG084V2)
   score 1.0000 (raw 0.6500)
   lists: text
   terms: shortest
   BM25 rank: 1
   support: strong
   graph weight: 0.35 (gap 0.00)
   validity: always → now
   source:
$ engramma verify
log verified: 4 operations, hash chain, reconstruction and FTS index in place
$ engramma proposals
[queued] B04M2TDB mcp:claude-code — Rob Pike co-wrote the Go book with Ken Thompson.
      reason: the user said so in this chat
$ engramma proposals accept 01M8D07A4614ATFQBRB04M2TDB
accepted: 01M8D07A4614ATFQBRB04M2TDB
vectors pending: the note is embedded by `engramma embed`
$ engramma decisions
WHNJC57E  accept  user@cli  2026-11-26T22:39:37Z
      proposal B04M2TDB
      why: accepted from the CLI
$ engramma revert 01M8D07A5DRZHWJAXHWHNJC57E --dry-run
would revert: the proposal goes back to the queue: B04M2TDB
$ engramma verify
log verified: 7 operations, hash chain, reconstruction and FTS index in place
$ engramma revert 01M8D07A5DRZHWJAXHWHNJC57E
reverted: the proposal goes back to the queue: B04M2TDB
$ engramma proposals accept 01M8D07A8CDYK9KQNQAVDATKD4
engramma: correction rejected: 01M8D07A2A34PNM1ZQPFTAPRMA is not the current version — already corrected by 01M8D07A88RDJME78Q588SDE64 — reject the proposal (`engramma proposals reject <id>`) or propose again on the current note (01M8D07A88RDJME78Q588SDE64)
   (exit 4)
$ engramma revert 01M8D07A8QSCZEZZQ9XY8HPW46 --yes
reverted: the proposal goes back to the queue: 588SDE64
$ engramma proposals accept 01M8D07A8CDYK9KQNQAVDATKD4
accepted: 01M8D07A8CDYK9KQNQAVDATKD4
```

Il dry-run risponde e `verify` conta 7 operazioni prima e dopo; il revert
subito dopo riporta la proposta in coda; la seconda correzione esce 4 e
resta in coda; annullata la prima, la seconda accetta. **Difetto trovato e
dichiarato** (CHANGELOG, Known issues): `explain.source` esce vuoto nella
ricerca senza modello — la riga `source:` qui sopra — mentre `note.source`
vale `user`. C'era già nella beta; la causa nel core non è stata cercata.

### La caccia, e il difetto del revert corretto

`scripts/caccia.sh`, `CACCIABASE=a5a61f2`, budget di default
(`target/caccia/20261126-211001`, 86 minuti): 30 proprietà, 16 toccate,
**29 verdi**. Tre rossi, tutti in `quarantine_exits_only_by_decision`.
Lo smistamento di GLM li diceva «già sul tag `fase-v0.6.18`», ed era vero
ma non bastava: **il confronto giusto è con il tag dove il codice era
verde l'ultima volta**. Rifatto dal revisore con `seme-sul-tag.sh`:

| Seme | `fase-v0.6.17` | Verdetto |
|---|---|---|
| `5883740581019706580` | cade | la famiglia a due dispositivi della v0.6.14 |
| `4586480981517075520` | **verde** | **nato con la v0.6.18**: verde su `389c44c`, rosso su `875d51e` (`StaleTarget`) |
| `2220529349810690032` | verde | la famiglia del taglio: cade anche senza la correzione qui sotto, solo dopo una compattazione (dietro l'interruttore sperimentale) |

**La causa** (`4586…`, una cella): P1 e P2 correggono T in quarantena;
P1 accettata (T `superseded`), P2 rifiutata — la decisione fotografa T
`superseded`; P1 purgata, e la purga riapre T in quarantena; il revert
del rifiuto di P2 rimette T com'era nella foto: **`superseded` senza
decisione, fuori dalla quarantena**, e `verify` muto. Il revert non vede
la purga (è archiviata sotto P1); `StaleTarget` ha solo aperto la strada
su una cella sola, raggiungibile dalla CLI della beta.

**La correzione** (`7bb1e87`, `classify_revert`): un rifiuto non tocca mai
il bersaglio, quindi il suo revert non lo riapre — `restore_target` vale
solo per un'accettazione. I revert già scritti si rigiocano come sono
(un `restore_target` nullo era già gestito): lo schema non cambia. Test
rosso prima: `the_revert_of_a_reject_leaves_the_target_as_it_is` («left:
Superseded, right: Active»). **Chiude anche il difetto dichiarato della
v0.6.14 a due dispositivi**: `1835111131785192683`,
`2472647288214113510`, `5883740581019706580` e `4586480981517075520`
sono casi fissi in `the_revert_family_seeds_stay_green`; le due righe
escono da `open-defects.txt`, dove resta solo la famiglia del taglio (con
`2220…`). La caccia sulle 7 proprietà di `proposals.rs` dopo la
correzione (`CACCIABASE=0c11ba1`, `target/caccia/20261127-002632`):
**sei verdi** (da 4 145 a 20 003 casi); `quarantine_exits_only_by_decision`
rossa col seme `10124808415445337785` (6 239 casi, e la suite a 2 048 lo
rigioca): «state differs around: tag … note_tags», la famiglia del taglio
— cade su `0c11ba1`, `fase-v0.6.18`, `fase-v0.6.17` e `fase-v0.6.16`,
quindi precede la fase e non viene dalla correzione: dichiarato nella sua
riga.

Due correzioni agli strumenti: il limite della suite viene solo da una
suite verde (F3), e `seme-sul-tag.sh` toglie il `CARGO_TARGET_DIR`
dell'ambiente (un primo verdetto era stato preso con la build finita
nella cache condivisa: rifatto). In `docs/caccia.md` la regola nuova del
secondo passo.

### Per il revisore: la sequenza del rilascio

1. **Push di `main`** (i commit della fase, quello della REPORT in coda).
2. **Tag `v0.6.19-beta.1`** sullo stesso commit spinto: `git tag -a
   v0.6.19-beta.1 -m "…" && git push origin v0.6.19-beta.1`. È il tag che
   fa partire `release.yml`.
3. La GitHub Release come le due prima: i sei file del pacchetto,
   l'installer, le attestazioni, il flag pre-release, il corpo dal
   CHANGELOG.

Le due righe che cambiano nel sito (le fa il revisore dopo il rilascio):
`nginx/default.conf`, `location = /engramma/install.sh`, il redirect a
`v0.6.19-beta.1`; `src/data/engramma.mjs`, `const VERSION = "0.6.19-beta.1";`
(e con lui `CAPTURED_WITH`).

**Le catture da rifare**, sul binario rilasciato (sono tutte uscite
italiane): l'hero (`search "where does Gaia live now?"` con la spiegazione
e `verify`: `punteggio`, `grezzo`, `liste`, `termini`, `validità … ora`,
`fonte`, `registro verificato: … operazioni`); le tre scene dell'hub
(«accettata dalla CLI», «confidenza 0.9», «in coda · status: quarantined»);
il cammino del grafo («(nota …)»); il tempo («→ ora»); la coda
(«da mcp:claude-code · in coda», «decisa da user@cli»). Con loro il
commento in testa al file: «the CLI prints Italian labels today».

### La build sul tag

Il tag `fase-v0.6.19`, annotato e solo locale, punta al commit della
REPORT. `scripts/build-sha.sh` col repo sul tag (costruisce su `58d63a3`,
dopo la correzione del revert): `engramma.darwin-arm64.node` 51 258 856 B,
SHA256 `217dc5c5…cc411c26`; `index.js` 36 463 B, `586f717f…8aa3a56`;
`index.d.ts` 32 673 B, `b1807110…9996b9a`. Contratto JS sull'addon del tag:
67 pass. Rispetto alla v0.6.18 cambiano tutti e tre: il `.node` per i
messaggi, la versione e il revert, `index.js` per la versione (stessa
misura), `index.d.ts` per i commenti del binding. L'API e i codici di
errore del binding non cambiano.

## v0.6.18 — la revisione mantiene le promesse

Data: 2026-11-25. Base: `main` @ `1bfc611` (la `0.6.17-beta.1`). Tema: la
coda delle proposte — proporre, rivedere, accettare, annullare — nei quattro
punti dove la cattura del sito l'aveva colta in difetto. Ogni difetto è
uscito col test rosso prima, sull'uscita del binario della base.

### Cosa cambia

- **F1 — `revert <id> --dry-run` non scrive.** Il ramo a id singolo di
  `run_revert` chiamava `revert_decision` ignorando il flag: l'anteprima
  annullava davvero e la proposta tornava in coda (S12 della cattura). Il
  dry-run passa ora da **`Cell::revert_preview`**, la classificazione in
  sola lettura (`classify_revert`) esposta dal core — l'anteprima è lo
  stesso classificatore del revert vero, non una seconda strada che
  indovina. L'esito (annullerei / purgato / conflitto / già annullata)
  arriva a umano e in JSON con `dry_run: true`, come il ramo in blocco.
- **F2 — annullare quello che un assistente ha proposto.** Le decisioni
  sono firmate da chi decide, mai da chi propone: `revert --by mcp:…`
  era accettato e annullava 0 decisioni in silenzio. Ora è un errore
  d'uso (exit 2) che suggerisce il filtro nuovo: **`--proposed-by`**
  (esatto o per prefisso, come `--by`), su `revert` e su `engramma
  decisions`, combinabile con `--by`/`--since`/`--until`, col `--dry-run`
  e la conferma `[s/N]`. Nel core è il campo `proposer` di `RevertFilter`
  e `DecisionsFilter`; la selezione passa dalla tabella delle proposte,
  guidata dagli op `decide` come oggi — e una decisione la cui proposta
  è stata purgata resta attribuibile (l'envelope dell'op `propose`
  conserva l'attore) e continua a contare tra le `purged`.
  `docs/cli.md`: la sezione di `revert` spiega chi decide e chi propone.
- **F3 — due correzioni della stessa nota.** Due proposte `supersede`
  accettate una dopo l'altra lasciavano due note attive per lo stesso
  fatto, senza copia di conflitto (S11b della cattura). Come in git: una
  correzione vale contro la versione che correggeva. L'accettazione si
  rifiuta con l'errore nuovo **`CellError::StaleTarget { target,
  closed_by }`** — CLI exit 4 con il messaggio che dice quale nota l'ha
  già corretta e cosa fare (reject, o una proposta nuova sulla nota
  corrente); la proposta resta in coda. Il bersaglio purgato resta come
  oggi (v0.6.11, K1); il revisore automatico continua a scalare quando
  il bersaglio non è attivo (test dedicato in `tests/review.rs`);
  annullata la prima correzione, il bersaglio torna attivo e la seconda
  si accetta (test). Il binding Node riceve il codice stabile
  **`STALE_TARGET`** (`docs/node.md`, sezione errori); la nota per Homun
  (cosa mostra la scheda della proposta) sta nel per-homun, fuori dal
  repo. Proprietà nuova nel
  cancello: **per ogni catena di `supersede`, al massimo una nota
  attiva** (`a_supersede_chain_keeps_at_most_one_active_note`, in
  `properties.txt`, girata col cancello).
- **F4 — la nota accettata si trova subito.** Una proposta accettata non
  aveva il vettore finché non girava `engramma embed` (`copertura
  vettori: 8/14 note` nella cattura): una domanda senza parole in comune
  non la trovava, anche se attiva. Ora `proposals accept`, `proposals
  review` (quando decide) e il revisore automatico del server MCP
  calcolano il vettore delle note che diventano attive, come `add` fa con
  `embed_after_write`; `--no-embed` su `accept` salta; senza embedder
  l'accettazione riesce e lo dice. In MCP il vettore si calcola nello
  stesso processo, dopo la decisione e fuori dal lock della scrittura; un
  fallimento resta in attesa di `embed` e non è un errore della
  decisione. **Il binding Node non cambia**: Homun ha il suo ciclo —
  `docs/node.md` dice che chiama `cell.embedPending()` prima del richiamo
  e a ogni `arrived` del sync (i vettori non viaggiano: si calcolano da
  chi riceve).
- **F5 — i ritocchi.** `mcp --actor` senza `--write`: l'errore nasceva in
  `engramma-mcp` e la CLI lo riportava come interno (`mcp: …`, exit 4) —
  il client MCP vedeva solo un server che muore; il controllo sta ora tra
  le regole d'uso della CLI, accanto a «`--write` needs `--actor`»: exit
  2, stesso messaggio. Il messaggio di `--compat` senza `--http` aveva
  gli spazi spezzati: corretto. Versione **`0.6.18-beta.1`** nello stesso
  modo della v0.6.17 (workspace, dipendenze interne,
  `crates/engramma-node/package.json`, `cargo update -w`, test di
  `--version`). CHANGELOG: la sezione `[0.6.18-beta.1]`, in inglese, per
  gli sviluppatori della beta — apre col dry-run che ha annullato davvero.

### I test rossi (sulla base `1bfc611`)

- F1 (CLI): `assertion left == right failed` — `dry_run: Null` invece di
  `true`: il ramo a id singolo non rispondeva da anteprima.
- F2 (CLI): `a usage error, not silence:` — exit 0 invece di 2: `--by
  mcp:` passava e annullava 0.
- F3 (core): `the second accept must refuse: two active notes stand now`
  — la seconda accettazione riusciva; (CLI) `not a silent second accept:`
  — exit 0 invece di 4.
- F4 (CLI): `the accepted note has its vector` — copertura `1/2` col
  finto embedder HTTP dei test (server OpenAI-compatibile a 8 dimensioni
  in `tests/common/mod.rs`, con sinonimo `impiegato→lavora`); (MCP)
  `the vector branch joins` — `mode: "text"`, `hits: []`: il modello
  resta in costruzione perché nessuno l'ha riempito dopo la decisione.
- F5 (CLI): `a usage error, not a dead server:` — exit 4 (`mcp: …`)
  invece di 2.

### Le prove

Suite del workspace col seme del CI (`PROPTEST_RNG_SEED=20261124`):
**verde — 87 binari, 714 test, 0 caduti** (13 ignorati, tra cui il
difetto dichiarato della v0.6.14). `golden_cell` verde. Contratto JS del
binding, sul `.node` del tag: **67 pass, 0 falliti**. `cargo fmt --check` e `cargo clippy -D warnings` puliti
(i commit della fase non lo erano: 29 differenze di rustfmt e tre
avvisi, sistemati in `5d7610b` senza cambiare il comportamento — il CI
sarebbe stato rosso al push).

**L'oracolo della catena.** La prima versione della proprietà leggeva gli
attivi dal model e controllava solo che una catena non raggiungesse
l'altra: due correzioni sorelle di F3 passavano. Ora gli attivi si
leggono dalla cella dopo ogni passo, le catene di due note attive devono
essere **disgiunte**, e `chain_of` cammina sulla tabella `supersedes`.
**Morde:** con `StaleTarget` tolto in un worktree non committato,
`PROPTEST_CASES=2048` dà rosso col seme `7557450884972762385` («two
active notes in one supersede chain»).

**Il difetto che l'oracolo ha trovato, corretto** (c'era già nella
0.6.17, raggiungibile dalla CLI, muto a `verify`). Tre forme, tutte con
un annullamento: **A1** P corregge T, C corregge P, si annulla P e la si
riaccetta — P torna attiva sotto C; **A2** C corregge P mentre P è in
coda dopo un annullamento lecito, C chiude P, P si riaccetta; **B** G
corregge C, si annulla C, F corregge C — F e G sorelle. La causa:
`classify_revert` cercava i conflitti solo tra gli op col `note_id`
della proposta o del bersaglio, e la decisione che chiude una nota porta
il `note_id` del correttore. Le due correzioni, in `proposals.rs`:

- **nel revert**, una riga di `supersedes` in piedi su quella nota è un
  conflitto: «<C> corrects this note: revert that correction first».
  Vale per `revert_preview`, `--dry-run` e il revert in blocco, che ora
  scorre **dal più recente** e tratta come annullate le decisioni già
  scelte nella corsa — C prima di P, tutte e due riescono, anche a secco;
- **nel decide**, un'accettazione si rifiuta con `StaleTarget` se la nota
  della proposta ha già un correttore in piedi, o se il bersaglio in
  quarantena ne ha uno diverso o è a sua volta in coda. È anche il
  secondo cancello per le storie della 0.6.17 arrivate dal sync. Ho
  scelto `StaleTarget` e non un errore fratello: per chi decide il
  rimedio è lo stesso (reject, o una proposta nuova sulla versione
  corrente), quindi exit 4 e `STALE_TARGET` non cambiano per la CLI,
  Node e Homun; cambia solo il testo, che dice «non è la versione
  corrente».

I test rossi sulla `e2f11e5`, prima della correzione: A1 «the preview
must see the correction on P: Revertable»; A2 «C corrects a proposal in
the queue: Ok(…)»; B «G stands on C»; il blocco `(1, 1)` invece di
`(2, 0)`, «seq 8 `revert_decision` … would land over them». Gli undici
semi dello smistamento e quello della prova del morso sono fissati in
`the_chain_family_seeds_stay_green`, che è rosso sul motore vecchio
(«seed 3947296525277540812: … two active notes») e verde ora. La riga
della v0.6.18 esce da `open-defects.txt`. La proprietà a 2048 casi è
verde (468 s); la spazzata di 20 000 semi
(SplitMix64 da `20261124`, test temporaneo non committato): **0 rossi**,
551 s.

La caccia sulle sette proprietà di `proposals.rs` (`CACCIABASE=e2f11e5`,
dieci minuti ciascuna): **tutte verdi**, da 2954 a 15153 casi (la catena
15153). La suite a 2048 casi era stata fermata dal limite: lo script lo
stima dall'ultima suite registrata, che era rossa e quindi breve (557 s).
Nessun test era caduto. Rifatta col limite giusto (`target/caccia/20261126-132600`): **verde,
1652 s, 0 rossi**.

**Il difetto della v0.6.14 resta dichiarato.** È della stessa famiglia,
ma passa per due dispositivi: due annullamenti concorrenti che si
fondono col sync, e ognuno da solo è lecito. Il controllo nel revert
guarda la cella locale e non può vedere l'annullamento dell'altro
dispositivo. Lo chiude la fusione a righe.

**Le celle già rotte dalla 0.6.17** non si riparano da sole e `verify` non
le vede. L'utente le riconosce perché la ricerca mostra due versioni
dello stesso fatto, e `engramma decisions` mostra due accettazioni sulla
stessa catena. Per sistemarle basta annullare la più recente (`engramma
revert <id>`): ora il revert la accetta solo se nessun'altra ci sta
sopra, e altrimenti dice quale annullare prima. In alternativa si
rifiuta la proposta rimasta in coda.

La prova sul binario (cartella temporanea dello scratchpad, dati
sintetici del sito, uscita accorciata): il dry-run risponde «annullerei:
proposta di nuovo in coda» e `verify` conta le stesse operazioni prima e
dopo; il revert subito dopo annulla. `revert --by mcp:claude-code` esce 2
col suggerimento; `--proposed-by mcp:claude-code` ne annulla 2 con
l'anteprima e la conferma. Le due correzioni della stessa nota: la
seconda esce 4 con «l'ha già corretta …», la proposta resta in coda;
annullata la prima, la seconda accetta. La ricerca subito dopo l'accept:
«incorporate: 1», la copertura è 8/8, e la domanda senza parole in
comune trova la nota con `liste: text, vector`, supporto `strong`.

### Per il revisore: la sequenza del rilascio

1. **Push di `main`** (i commit della fase, quello della REPORT in coda).
2. **Tag `v0.6.18-beta.1`** sullo stesso commit spinto: `git tag -a
   v0.6.18-beta.1 -m "…" && git push origin v0.6.18-beta.1`. È il tag che
   fa partire `release.yml`; `main` da solo non lo fa.
3. La GitHub Release deve portare come la v0.6.17: i sei file del
   pacchetto, l'installer, le attestazioni, il flag pre-release e il
   corpo dal CHANGELOG.

Le due righe che cambiano nel sito (dopo il rilascio, le fa il revisore):

- `nginx/default.conf`, `location = /engramma/install.sh`: la versione
  del redirect passa a `v0.6.18-beta.1`
  (`return 302 https://github.com/homun-app/engramma/releases/download/v0.6.18-beta.1/engramma-cli-installer.sh;`);
- `src/data/engramma.mjs`, `const VERSION = "0.6.18-beta.1";` (il `TAG`
  deriva da lì, `ENGRAMMA_RELEASE` con lui).

### La build sul tag

Il tag `fase-v0.6.18`, annotato e solo locale, punta al commit della
REPORT. La correzione cambia il motore, quindi la build si rifà:
`scripts/build-sha.sh` col repo sul tag (costruisce su `acf48f6`): `engramma.darwin-arm64.node`
51 259 304 B, SHA256 `1884125b…ef872abb`; `index.js` 36 463 B,
`ac259d8b…8c48f`; `index.d.ts` 32 615 B, `7020ae60…0fc2427`. Rispetto
alla build di prima della correzione cambia solo il `.node`
(`b4a8ceba…`, 51 258 376 B), perché cambia il motore. `index.js` e
`index.d.ts` sono identici byte per byte: l'API e i codici di errore del
binding non cambiano.

### Il verdetto

Consegnati: F1 (l'anteprima dal classificatore vero), F2 (`--proposed-by`
con le purgate attribuite), F3 (`StaleTarget`, la proprietà della catena
che morde, e il difetto che ha trovato, corretto nel revert e nel
decide), F4 (il vettore subito, in CLI e MCP), F5 (l'errore d'uso al
posto del server che muore, versione, CHANGELOG). Restano fuori, come
dichiarato: l'output della CLI in inglese, la fusione a righe e i
permessi per attore, Windows e macOS Intel, la firma Apple e Homebrew,
l'aggiornamento del sito (lo fa il revisore dopo il rilascio). In
`open-defects.txt` resta solo il difetto a due dispositivi della
v0.6.14, per il motivo scritto in «Le prove».

## v0.6.17 — la prima beta per sviluppatori

Data: 2026-11-25. Base: `main` @ `6ff7400`. Il motore non cambia una riga
dei tre sorgenti vietati: niente caccia, niente misure.

### Cosa cambia

- **F1 — la versione e la promessa.** Il workspace è `0.6.17-beta.1`
  (`cargo update -w`: il lock cambia solo nelle otto crate nostre, diff
  verificato). Test nuovo: `engramma --version` stampa la versione del
  rilascio. **`docs/compatibilita.md`**: ogni riga col test che la prova —
  le celle (le golden e `golden_cell.rs`; il rifiuto `SchemaTooNew`:
  uscita 6, errore leggibile, file identico byte per byte, provato in CLI
  da `schema_too_new_cli.rs` nuovo), la sync (l'ALPN e l'`hello` rifiutano
  le versioni diverse prima che un frame si muova; il messaggio dice il
  dispositivo da aggiornare; `protocol_version.rs`), l'export
  (`format_version` 0: cosa garantisce e cosa no — non è un backup, e un
  export futuro non viene piegato in silenzio), il contratto MCP (i nomi
  stabili per la serie `0.6.x-beta`, `mcp.rs` inchioda la `tools/list`).
  La golden della beta: **`v0617`** (cut 23, con istantanea), scritta da
  `make_golden_cell_v0617.rs` — il respiro di oggi in una storia: tipi,
  quarantena confermata, conflitti aperto e chiuso, proposta d'agente
  accettata, purge, vettori, vista. `golden_cell.rs` ne apre sei.
- **F2 — i binari con dist.** `dist` 0.34.0 (l'ultima stabile, ricontrollata
  il 2026-11-25). `dist-workspace.toml` scritto a mano: un solo pacchetto
  distribuito (`engramma-cli`), le altre sette crate `dist = false`; tre
  target (macOS arm64, Linux x86_64, Linux arm64 **su runner nativo**
  `ubuntu-24.04-arm`, niente cross — verificato nel piano); installer solo
  `shell` (il tap Homebrew è una decisione dell'utente); `sha256`;
  `github-attestations = true` (provenienza firmata da GitHub, zero
  segreti nostri); `install-path = "CARGO_HOME"`; `pr-run-mode = "upload"`.
  Il `[profile.dist]` di `dist init` non tocca il `strip = "none"` di
  `engramma-node`. `release.yml` è come lo genera `dist generate`;
  `dist plan` passa; `ci.yml` non cambia. Windows e macOS Intel restano
  fuori, scritto nel toml: entrano quando i loro job del CI diventano
  verdi e bloccanti.
- **F3 — le pagine.** README riscritto per chi arriva da fuori: stato
  (developer preview), la promessa, l'installer (e la verità su Gatekeeper:
  nessuna firma Apple — l'installer da terminale non mette la quarantena,
  il tarball dal browser sì, `xattr -d` per chi lo sceglie), la tabella
  delle piattaforme, il primo minuto con output veri, la memoria in Claude
  Desktop (formato verificato sui documenti MCP attuali; NON provato su
  Claude vivo — scritto). `CHANGELOG.md` nuovo (Keep a Changelog): `dist
  plan` ne legge la voce (`announcement_changelog`, titolo
  `0.6.17-beta.1 - 2026-11-25`, prerelease vero). `docs/cli.md`: i sei
  sottocomandi che mancavano (proposals, decisions, revert, policy, review,
  conflicts con show e resolve); nessuna riga in più.

### Le prove sulla PR

La PR in bozza [#1](https://github.com/homun-app/engramma/pull/1)
(`release-v0.6.17`): **Release** [45588543109](https://github.com/homun-app/engramma/actions/runs/45588543109)
e **ci** [45588542643](https://github.com/homun-app/engramma/actions/runs/45588542643)
verdi sul primo spingo (`10ba87b`); l'ultimo commit (`8dffd2d`, solo il
README e la riga glibc) ha ripetuto le corse verdi ([Release
45593676541](https://github.com/homun-app/engramma/actions/runs/45593676541),
[ci 45593676415](https://github.com/homun-app/engramma/actions/runs/45593676415)).
I tre binari,
scaricati dagli artefatti (sha256 verificato), provati uno per uno —
`--version` (0.6.17-beta.1), `init`, `add`, `search` che trova, `verify`
pulito, `mcp --cells` che risponde all'`initialize` su stdio (EOF chiude):
macOS arm64 direttamente; Linux arm64 e x86_64 in Docker su `ubuntu:24.04`
(l'x86 emulato). **Scoperta delle prove**: i binari Linux vogliono
**glibc ≥ 2.39** (bookworm 2.36 rifiuta) — costruiti su ubuntu-24.04,
scritto nel README. Niente `model pull`: la prova basta alla ricerca
testuale.

### I test

Suite del workspace in locale, col seme del CI (`PROPTEST_RNG_SEED`,
casi pieni dei sorgenti): verde — 86 binari, 698 test, 0 caduti, più
`relay_cli` 7/7 ripetuto a parte: la prima corsa è caduta sulle cache
del `$TMPDIR` che macOS svuota (la nota del revisore v0.6.16, non una
regressione; pulite `engramma-cli-{relay,plain}`, rifatto verde).
`golden_cell` verde con la `v0617`; `--version` e `schema_too_new_cli`
verdi. Il contratto JS non serve (il `src` del binding non cambia) — il
CI lo ha girato comunque, verde su due runner.

### La build sul tag

Il tag `fase-v0.6.17`, annotato e solo locale, punta al commit della
REPORT (`f5278ed`; `git cat-file -t fase-v0.6.17^{}` dice `commit`).
`scripts/build-sha.sh` col repo sul tag gira: i tre file dell'aggancio,
letti dal disco — `engramma.darwin-arm64.node` 51 240 872 B, SHA256
`07de323b…9eaab6`; `index.js` 36 463 B, `babc97db…a1e8d`; `index.d.ts`
32 615 B, `7020ae60…0fc2427`. Rispetto alla v0.6.16 cambiano il `.node`
(090c63d7…, 51 257 480 B) e `index.js` (cd26043e…, 36 239 B): dentro c'è
la versione nuova; `index.d.ts` è identico, l'API del binding non è
cambiata. Il contratto JS non è girato in locale: il `src` del binding
non cambia (regola della fase), solo la versione nel `package.json`.

### Per il revisore: la sequenza del rilascio

1. **Push di `main`** (i commit della fase, quello della REPORT in coda).
2. **Tag `v0.6.17-beta.1`** sullo stesso commit spinto: `git tag -a
   v0.6.17-beta.1 -m "…" && git push origin v0.6.17-beta.1`. È il tag che
   fa partire `release.yml` (il push di `main` da solo non lo fa).
3. **Nella GitHub Release** deve apparire: i sei file
   (`engramma-cli-{aarch64-apple-darwin,aarch64-unknown-linux-gnu,
   x86_64-unknown-linux-gnu}.tar.xz` e i loro `.sha256`), l'`engramma-cli-installer.sh`,
   `sha256.sum`, `source.tar.gz` + `.sha256`; le **attestazioni** sugli
   artefatti (provenienza firmata, visibili con `gh attestation verify`);
   il flag **pre-release** acceso (il suffisso `-beta.1` lo decide da
   solo); il corpo preso dal `CHANGELOG.md`.
4. Dopo il rilascio la PR #1 si chiude (già chiusa da questa fase, senza
   fondere) e il ramo remoto `release-v0.6.17` è già cancellato.

### Il verdetto

Consegnati: F1 (versione, promessa riga per riga, golden v0617), F2 (dist
0.34.0, tre binari provati, attestazioni), F3 (README, CHANGELOG, cli.md).
Fuori, come dichiarato: firma e notarizzazione Apple, tap Homebrew,
crates.io, Windows e macOS Intel, la pagina di homun.app, `relay_cli` su
`CARGO_TARGET_TMPDIR`, la latenza di `stop()` sotto carico.

### Verifica (revisore, 2026-11-25)

**Riproducibile:** `scripts/build-sha.sh` sul tag rifà gli stessi tre SHA
(`.node` `07de323b…9eaab6`). Verdi in locale `golden_cell` (con la
`v0617`), `version` e `schema_too_new_cli`; il binario macOS della PR #1,
checksum giusto, fa `init`, `add`, `search`, `verify` e risponde
all'`initialize` MCP. Tutti i commit passano la guardia.

**Corretto dal revisore prima del rilascio** (`a35aca8`, provato sulla PR
in bozza [#2](https://github.com/homun-app/engramma/pull/2): Release
[45600370390](https://github.com/homun-app/engramma/actions/runs/45600370390)
e ci [45600370651](https://github.com/homun-app/engramma/actions/runs/45600370651)
verdi, PR chiusa senza fondere):
- **glibc 2.39 → 2.35.** La fase aveva fissato i runner Linux su Ubuntu
  24.04: Ubuntu 22.04 LTS e Debian 12 restavano fuori. Ora si costruisce su
  `ubuntu-22.04` e `ubuntu-22.04-arm`; i due binari Linux della #2 fanno
  la stessa prova in Docker su `debian:12` (2.36) e `ubuntu:22.04` (2.35),
  arm64 e x86: verdi tutti e quattro. RHEL 9 (2.34) resta fuori, scritto.
- **Il README non promette cose false:** il workflow di rilascio non prova
  i binari (la prova è stata a mano); `Cargo.lock` fissa le dipendenze, non
  le contiene; il comando d'installazione usa l'URL fisso della beta,
  perché `releases/latest` salta le pre-release.

**Note:** il REPORT supera la pagina; `docs/compatibilita.md` è in
italiano dietro un README inglese; `mcp.rs` inchioda i cinque strumenti di
lettura ma non `memory_propose`; le cache `$TMPDIR/engramma-cli-*`
cancellate contro il vincolo (si ricostruiscono); `pr-run-mode = "upload"`
costruisce tre target a ogni PR. **Verdetto: consegnata**, con la
correzione sopra; rilascio `v0.6.17-beta.1` sul commit di questa verifica.

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
