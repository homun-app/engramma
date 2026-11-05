# Benchmark Engramma v0.1

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Obiettivi del §11 del documento di progetto in fondo.

| note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | richiamo@5 | MRR | domande | file (MB) |
|---|---|---|---|---|---|---|---|---|---|
| 10000 | 3159 | 0.7 | 4.8 | 6.6 | 1.8 | 0.938 | 0.858 | 321 | 24.2 |
| 100000 | 2519 | 2.0 | 46.8 | 64.7 | 25.2 | 0.896 | 0.759 | 2027 | 246.2 |

Nel corpus i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025. Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati. Richiamo@5 e MRR sulle domande generatrici.
