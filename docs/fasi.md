# Le fasi leggere (v0.6.15)

Scritto nella v0.6.15, dopo sei fasi di fila sulla compattazione: ogni caccia
trovava un difetto sotto, e la compattazione — un'**ottimizzazione** — teneva
bloccata la release locale di una memoria che, per una persona, cresce di
poco. Da qui la rotta: la compattazione esce dal percorso critico (dietro
l'opzione `experimental: ['compaction']` del binding e `--sperimentale`
della CLI), il suo lavoro continua in un filone a parte, e le fasi diventano
leggere. Le regole:

1. **Il REPORT è una pagina per fase** (al massimo ~120 righe): cosa cambia,
   i numeri misurati, i difetti trovati, il verdetto. Le analisi lunghe
   vanno in DESIGN (se restano vere) o non vanno da nessuna parte. La storia
   fino alla v0.6.14 sta in `docs/storia/REPORT-fino-alla-v0.6.14.md`.
2. **I commit:** titolo di al massimo 72 caratteri, corpo di al massimo 5
   righe. Il dettaglio sta nel REPORT o nel codice.
3. **Le misure** si fanno solo se la fase tocca un percorso caldo
   (inserimento, ricerca, `verify`, sync, apertura della cella), contro il
   tag precedente, col doppio cancello della v0.6.14. Se la fase non lo
   tocca, niente misure.
4. **La caccia, a livelli:**
   - a ogni push, il CI (livello 1, `.github/workflows/ci.yml`) — dal
     v0.6.16 deterministico col seme fisso `PROPTEST_RNG_SEED`: un rosso
     del cancello è sempre una regressione;
   - il cancello di `scripts/caccia.sh` **solo** se la fase tocca codice
     coperto da una proprietà della suite — l'inventario
     `crates/engramma-core/tests/properties.txt` — e solo sulle
     proprietà toccate: il binding e la CLI non fanno partire il
     cancello (v0.6.16; prima la regola diceva «crates/*/src»);
   - il cancello intero prima di una release che cambia il motore;
   - la caccia notturna resta com'è (l'installazione è dell'utente).
5. **Un tema per fase, scelto dal prodotto:** cosa serve a Homun. Un difetto
   che una caccia trova in una funzione sperimentale va dichiarato, non
   inseguito.
6. **Il triage dei difetti** (v0.6.16). Un difetto si corregge nella
   fase in cui si trova solo se è raggiungibile dall'API stabile di
   Homun — la sezione «Cosa Homun può usare» di `docs/node.md` — **e**
   perde dati, oppure lascia uno stato che `verify` non vede. Tutti gli
   altri si dichiarano in `open-defects.txt`, con chi li raggiunge e
   cosa succede, e la fase va avanti.
