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
  --locked`, azioni fissate allo SHA, matrice reale, il contratto JS, e
  `cargo-deny` da binario. È il livello 1 della caccia.
- **F2.** La compattazione dietro un interruttore sperimentale: `compact()`
  del binding JS funziona solo su celle aperte con `{ experimental:
  ['compaction'] }` (senza: errore `EXPERIMENTAL`); `engramma compact` vuole
  `--sperimentale`. Nessuna strada del motore compatta da sola.

(da completare a fine fase: i numeri, i difetti, il verdetto)
