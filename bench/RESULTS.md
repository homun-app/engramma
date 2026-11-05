# Benchmark Engramma v0.1.1

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Ogni riga è la **mediana di 3 giri**. Obiettivi del §11 del documento di progetto in fondo.

| corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | richiamo@5 | MRR | domande | file (MB) |
|---|---|---|---|---|---|---|---|---|---|---|
| viva | 10000 | 3547 | 1.1 | 4.8 | 8.1 | 1.9 | 0.938 | 0.858 | 321 | 24.2 |
| uniform | 10000 | 3558 | 1.0 | 4.8 | 7.8 | 1.9 | 0.938 | 0.810 | 321 | 24.2 |
| viva | 100000 | 2586 | 2.0 | 48.2 | 84.6 | 25.8 | 0.896 | 0.759 | 2027 | 247.8 |
| uniform | 100000 | 2284 | 1.9 | 50.0 | 84.5 | 26.1 | 0.890 | 0.767 | 2027 | 247.8 |

Due varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati. Richiamo@5 e MRR sulle domande generatrici. La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).
