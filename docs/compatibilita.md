# La compatibilità dei dati (v0.6.17-beta.1)

Questa pagina è la promessa che Engramma fa a chi gli affida la memoria.
Ogni riga dice solo ciò che il codice già garantisce, e accanto ha il test
che lo prova: una promessa senza test non c'è. Se un cambiamento futuro
deve rompere una di queste righe, va annunciato nel `CHANGELOG.md` con la
versione che lo fa.

## I file delle celle

Una cella è un file SQLite (`<root>/cells/<nome>.sqlite`), schema 24 in
questa versione. Due promesse:

**1. Ogni versione futura apre una cella scritta da questa, senza perdite.**
Le migrazioni dello schema sono solo in avanti e la cella è un registro:
il rigioco delle operazioni ricostruisce esattamente lo stesso stato. La
prova è il meccanismo delle celle d'oro: ogni versione che cambia lo
schema aggiunge la sua cella e `crates/engramma-core/tests/golden_cell.rs`
apre **tutte** — oggi `v068`, `v0612`, `v0613`, `v0613-indietro`, `v0614`
e la `v0617` di questa release — con il motore di oggi, e `verify` è
verde su ognuna. La regola sta in `docs/DESIGN.md` § 8.

**2. Una versione più vecchia rifiuta una cella più nuova, e non la tocca.**
Il controllo è `SchemaTooNew` in `Cell::open` prima di ogni migrazione e
di ogni scrittura. Cosa vede chi usa la CLI:

```
engramma: the cell schema is newer than this binary (version 25, this
binary supports up to 24): the cell was left untouched, upgrade Engramma
first
```

uscita 6, e il file resta identico byte per byte. Le prove:
`crates/engramma-core/tests/schema_too_new.rs` (il motore rifiuta e la
versione scritta resta lì, letta e non migrata) e
`crates/engramma-cli/tests/schema_too_new_cli.rs` (la CLI: errore
leggibile, uscita 6, file identico byte per byte).

## La sync

Due dispositivi con versioni diverse del protocollo **si rifiutano a
vicenda**, prima che una sola operazione si muova. Il rifiuto è doppio:
l'ALPN della connessione è `engramma/sync/9` (il numero è la versione
delle regole di materializzazione) e l'handshake muore lì; e l'`hello`
dentro `engramma-sync/src/protocol.rs` rifiuta ogni versione che non sia
`PROTOCOL_VERSION` (9). Le regole diverse derivano stati diversi dalle
stesse operazioni: rifiutarsi è l'unica risposta sicura.

Cosa vede l'utente (il servizio dice chi aggiornare):

```
il dispositivo <nome> usa regole di materializzazione diverse:
aggiornalo (o aggiorna questo dispositivo) prima di sincronizzare
```

(`PEER_OUTDATED` dal binding Node). Il rifiuto avviene nella handshake,
prima di ogni scambio: non arriva nulla, non si muove nulla, nessuno
stato resta indietro e `verify` resta verde. La prova:
`crates/engramma-sync/tests/protocol_version.rs` — un endpoint con le
regole vecchie rifiuta il dialer nuovo, l'errore è `PeerOutdated` e
nomina il dispositivo; nessun frame si è mosso.

**Si aggiornano tutti e due i dispositivi** (o nessuno): la sync riprende
da dove era, perché le celle non sono state toccate.

## L'export

`engramma export` scrive una cartella di note Markdown con un
`engramma.json` che dichiara `format_version: 0`
(`FORMAT_VERSION` in `crates/engramma-core/src/port.rs`).

**Cosa garantisce:** le note restano leggibili come testo (intestazione
YAML + corpo Markdown, vedi `spec/`); gli id non cambiano; una cartella
esportata da questa versione si re-importa in una cella e ricostruisce
lo stesso stato delle note (`export_then_import_reproduces_state` in
`port.rs`; la chiusura delle validità sospese è coperta da
`import_closes_superseded_validity_at_the_citing_note`).

**Cosa non garantisce:** l'export non è un backup della cella — non porta
il registro delle operazioni, i vettori (dati derivati: si ricalcolano),
lo stato della sync, i referral espunti (le lapidi sono saltate) — e un
id dimenticato con `purge` resta dimenticato: l'import lo rifiuta e lo
dice. Una cartella esportata da una versione **futura** (con
`format_version` più alto) non viene rifiutata dal numero: i file che il
parser rigoroso di questa versione non capisce finiscono negli errori
dell'import e per quelli non si scrive nulla — niente viene piegato in
silenzio a una forma vecchia.

## Il contratto MCP

I nomi e gli argomenti degli strumenti di `docs/mcp.md` sono stabili per
tutta la serie `0.6.x-beta`: `memory_search`, `memory_get`,
`memory_neighbors`, `memory_tags`, `memory_timeline` (lettura) e
`memory_propose` (scrittura, solo con `--write`). Un cambiamento
rompente — un nome che sparisce, un argomento che cambia significato — si
annuncia nel `CHANGELOG.md` prima di uscire. La prova:
`crates/engramma-cli/tests/mcp.rs` inchioda la `tools/list` ai cinque
strumenti di lettura, con i loro schemi, e l'isolamento delle celle.

## Cosa non è promesso

Il formato interno dello SQLite (le tabelle, lo `schema_version`) non è
un'interfaccia: si cambia con le migrazioni, e la promessa vera sono le
due righe sopra. L'interfaccia a riga di comando (i nomi dei
sottocomandi e i flag) è di questa anteprima e può muoversi; la
stabilità del binding Node è quella dichiarata in `docs/node.md`
(«Cosa Homun può usare»). La ricerca testuale e semantica può migliorare
i punteggi: i risultati possono cambiare ordine fra versioni, senza che
nulla vada perso.
