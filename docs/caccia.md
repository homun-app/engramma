# La caccia: tre livelli (v0.6.14, N3)

La caccia della v0.6.13 è durata 6 h 33 min, è stata rifatta, e prima
c'erano ore di analisi: dava a ogni proprietà gli stessi 20 000 casi, e
così `purge_with_conflict_copies_leaves_no_roots` prendeva 50 minuti e
`hlc_monotonic` zero secondi. La profondità serve, ma non deve bloccare
ogni tag. Lo stato dell'arte è il fuzzing continuo di OSS-Fuzz e della CI
di FoundationDB: **un cancello breve e mirato per la fase, una caccia
profonda continua fuori dal percorso critico, e quello che trova diventa
una regressione o un difetto dichiarato.**

| livello | dove | quando | cosa |
|---|---|---|---|
| 1 | `cargo test` | a ogni commit | la suite a 128 casi, coi file delle regressioni (com'è) |
| 2 | `scripts/caccia.sh` | il cancello della fase | ogni proprietà con il suo budget di tempo, poi una corsa della suite a 2048; obiettivo sotto 1 h 45 min |
| 3 | `scripts/caccia-notte.sh` | di notte, installata dall'utente | la caccia lunga e continua, a rotazione, coi semi che cambiano ogni notte |

## Livello 2: il cancello (`scripts/caccia.sh`)

**Budget di tempo, non numero di casi.** Ogni proprietà riceve un tempo;
lo script calcola i casi dal costo per caso misurato nella caccia prima
(i registri in `target/caccia/<inizio>/misura.tsv`). Senza una misura
parte con una corsa breve di calibrazione (256 casi). Mai meno di 256
casi, mai più di 20 000. Nel registro va il numero di casi **davvero
corsi** — proptest stampa un rigo per caso (`PROPTEST_VERBOSE=2`), anche a
una corsa che cade.

**Più tempo dove la fase ha toccato il codice.** Una proprietà è
**toccata** se dalla base della fase è cambiato uno dei file dei moduli
che esercita, oppure il suo file di test — **il suo oracolo**. Un cambio
di oracolo obbliga a rifare la caccia di quella proprietà: è la regola
che mancò nella v0.6.13, dove il giudice della quarantena cambiò dopo la
caccia. La mappa proprietà → moduli è la terza colonna di
`crates/engramma-core/tests/properties.txt`; `properties_inventory`
controlla che ogni proprietà abbia la sua mappa e che ogni percorso
esista. Le toccate prendono 10 minuti, le altre 2 — i due valori sono
parametri dello script (`TOCCATE`, `ALTRE`). Se la somma supera
l'obiettivo (`OBIETTIVO`, 100 minuti, suite compresa), i budget si
riducono in proporzione: prima le toccate, mai sotto il budget delle
altre. Una proprietà che raggiunge i 20 000 casi senza esaurire il suo
budget non si riduce: il massimo dei casi costa quello che costa.

**Una corsa sola della suite a 2048** (`SUITE_CASI`; `0` per saltarla).
Il suo limite di tempo si stima dall'ultima corsa della suite **verde**
nei registri (l'ottava colonna di `misura.tsv`): una suite rossa si ferma
al primo fallimento ed è breve — nella v0.6.18 557 s davano un limite di
1135 s, e la suite verde dopo ne ha chiesti 1652 — e una fermata (`TEMPO`)
non ha misurato nulla. Senza una misura verde restano 1500 s
(`SUITE_RISERVA`) (v0.6.19).

**La regola del commit finale**, più leggera: un commit dopo il cancello
che tocca `crates/*/src`, o l'oracolo di una proprietà, obbliga a rifare
il cancello **solo per le proprietà toccate da quel commit**:

```sh
CACCIABASE=<commit del cancello> CACCIASOLO="proprietà_a proprietà_b" scripts/caccia.sh
```

Senza `CACCIABASE` la base è l'ultimo tag `fase-v*` che HEAD contiene.

Al termine lo script stampa la tabella — proprietà, toccata sì/no,
budget, casi corsi, secondi, esito, semi — e scrive `misura.tsv` nel
registro. Esce con un codice diverso da zero se c'è almeno un rosso; una
corsa che sfora il budget oltre il margine viene fermata (esito `TEMPO`).

Tutto gira in release, a priorità minima (`nice -n 19`, due job di cargo,
due fili di test), mai due corse pesanti insieme.

## Lo smistamento dei rossi: 30 minuti, tre passi

Ogni rosso, del cancello o della notte, ha **30 minuti** per tre passi
meccanici. La diagnosi della causa di un difetto pre-esistente **non**
spetta alla fase che lo trova: la fa la fase che lo chiude.

1. **La riproduzione deterministica del seme.** Ogni proprietà ha un
   corpo `fn <nome>_body(seed: u64)` nel suo file: la proprietà prende un
   seme e lo passa al corpo, che ricava ogni ingresso con un generatore
   seminato. Un test che chiama `<nome>_body(<seme>)` rigioca quel caso
   e nessun altro.
2. **Lo stesso seme sul tag precedente**:
   ```sh
   scripts/seme-sul-tag.sh <proprietà> <seme> <tag> [corpo]
   ```
   Un worktree del tag dentro `target/` (mai in `$TMPDIR`), che si
   costruisce **nella sua cartella di build** (il `target` del worktree, e
   se ne va con lui); nel file di test della proprietà un test che chiama
   il corpo col seme; il verdetto — **«cade anche sul tag»** (esce 1) o
   **«non cade sul tag»** (esce 0); il worktree si toglie da solo alla
   fine, anche se il test fallisce. Sui tag prima della v0.6.14 solo tre
   proprietà hanno un corpo (`purge_takes_only_its_family_body`,
   `frozen_prefix_body`, `quarantine_exits_body`): per le altre lo script
   lo dice (esce 3).

   **La cartella di build non si condivide con l'albero principale, e lo
   script controlla il motore.** Due alberi diversi nella stessa
   `CARGO_TARGET_DIR` danno manufatti con lo stesso nome, e cargo giudica
   fresco un manufatto i cui sorgenti, *quelli che ha elencato nella
   build prima*, non sono cambiati — non guarda l'albero che ha davanti.
   Si rischia di provare il motore di oggi col file di test del tag, e
   «cade anche sul tag» con il motore di oggi cade uguale. Nella v0.6.14
   andò così per tutta la fase, e ogni verdetto fu rifatto in fondo
   (REPORT). Lo script adesso non dà un verdetto se la **libreria** del
   tag non risulta compilata in quella corsa dal worktree (cargo,
   `--message-format=json`) e se lo **schema** di una cella nuova non è
   quello che il tag dichiara nel suo `schema.rs`. Costa una build intera
   a corsa: è il prezzo di un verdetto che significa quel che dice.
3. **Il giudizio.** Se il difetto **nasce da questa fase**, si corregge.
   Se **c'era già**, si **dichiara** aperto in `open-defects.txt` col seme
   e il verdetto dello script, e si va avanti.

   Il file `crates/engramma-core/tests/open-defects.txt` esiste solo
   quando c'è almeno un difetto aperto, e va via con l'ultimo, insieme ai
   due cancelli che lo guardano in `properties_inventory.rs`. Il 23
   novembre 2026 sono successi entrambi i casi (REPORT della v0.6.14):
   l'ultimo dichiarato, la perdita di testo esposta dal cestino, era
   chiuso dalla v0.6.9 — i suoi due casi registrati cadono sul tag
   `fase-v0.6.8` e passano da `fase-v0.6.9` in poi — e le cacce della
   fase ne hanno trovati altri due, che stanno nel file: uno il cancello
   (a due dispositivi: annullamenti concorrenti di proposte legate), uno
   le cacce dopo il cancello (la purga di un chiudente sopra il taglio:
   tre semi, due forme).

**Un rosso di una proprietà che ha già un difetto dichiarato si smista
uguale.** Non è per forza «quello»: nella stessa fase la proprietà della
quarantena è caduta due volte con **due difetti diversi** (il messaggio
li distingue: «la decisione annullata … non basta» contro «state differs
around …»), e il secondo, trovato dalla prova notturna, nasce dal
taglio. I tre passi sono gli stessi, e il verdetto «cade anche sul tag»
vale per quel seme, non per la proprietà.

**Un verde di una proprietà che ha un difetto dichiarato può essere un
verde breve.** Nella corsa finale della v0.6.14 la proprietà della
quarantena prese 1 615 casi — e fu verde, senza incontrare i due difetti
che le stanno dichiarati — perché la corsa prima era stata rossa e una
corsa rossa non lasciava il suo costo per caso: la calibrazione di una
corsa sola lesse 168 ms per caso, il doppio del vero. Ora il costo lo
lascia ogni corsa con dei casi. Ma la lezione resta: davanti al verde di
una proprietà con una riga in `open-defects.txt` si guarda quanti casi ha
corsi, e la si rifà a budget intero.

Una proprietà il cui ingresso era una terna o un valore strutturato
prima della v0.6.14 ha anche un gemello `_with(…)`, che prende gli
ingressi come furono scritti: i controesemmi che proptest aveva
persistito in quella forma sono test deterministici nei file delle
proprietà, e si rigiocano da lì.

## Livello 3: la caccia notturna (`scripts/caccia-notte.sh`)

Gira su `main` (parametro `RAMO`) a `nice -n 19` con due fili, in una
finestra di ore — default dalle 01:00 alle 07:00, parametro `FINESTRA` —
e si ferma da sola alla fine della finestra.

- **I semi cambiano ogni notte.** La variabile d'ambiente di proptest che
  semina il generatore è **`PROPTEST_RNG_SEED`** (proptest 1.12.0: il
  nome è letto nel sorgente della versione in uso, `src/test_runner/`).
  Lo script la deriva dalla data e dalla posizione della proprietà nella
  rotazione: una notte si rifà con lo stesso seme.
- **Le proprietà girano a rotazione, a partire dalle più costose** (il
  costo dell'ultima caccia che l'ha misurato): ogni proprietà prende una
  corsa lunga — una fetta, `FETTA` minuti, 40 di default — ogni pochi
  giorni.
- **I risultati** vanno in `target/caccia-notte/<data>/`: un file
  `semi.txt` con proprietà, seme e prima riga del messaggio («-» a una
  corsa verde), e il registro di ogni corsa.
- **Non committa niente e non tocca il repo**: scrive solo sotto
  `target/`, e spegne la persistenza dei file di regressione di proptest
  (`PROPTEST_DISABLE_FAILURE_PERSISTENCE`) — un rosso di notte non
  riscrive i `.proptest-regressions` del ramo. Un rosso si smista il
  giorno dopo con lo stesso budget del cancello.

Una prova, con la finestra accorciata a 20 minuti:

```sh
DURATA=20 scripts/caccia-notte.sh
```

### L'installazione è dell'utente

Lo script e il launchd di esempio
(`scripts/it.engramma.caccia-notte.plist`) stanno nel repo; **nulla li
installa**. Far girare qualcosa ogni notte sulla macchina è una decisione
dell'utente. Se la si prende:

1. nel plist si sostituiscono `/PERCORSO/DEL/repo/engramma` e
   `TUO-UTENTE` (la `PATH` deve trovare `cargo`, che sta in
   `~/.cargo/bin`);
2. `mkdir -p ~/Library/LaunchAgents`, si copia il plist lì e
   `launchctl load ~/Library/LaunchAgents/it.engramma.caccia-notte.plist`.

Per toglierlo: `launchctl unload` dello stesso file, poi si cancella. Se
il Mac dorme all'una, launchd lo lancia al risveglio e lo script calcola
quanto resta della finestra.

## Fuori da questa fase

Lo *swarm testing* (Groce et al.: ogni corsa abilita a caso un sottoinsieme
delle operazioni): un'idea per dopo.
