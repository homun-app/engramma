# Benchmark Engramma v0.2

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Ogni riga è la **mediana di 3 giri**. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). Obiettivi del §11 del documento di progetto in fondo.

| corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | richiamo@5 | MRR | domande | file (MB) |
|---|---|---|---|---|---|---|---|---|---|---|
| viva | 10000 | 3366 | 0.8 | 4.5 | 6.8 | 1.7 | 0.972 | 0.763 | 726 | 24.3 |
| uniform | 10000 | 2689 | 0.9 | 4.6 | 6.6 | 1.8 | 0.972 | 0.747 | 726 | 24.3 |
| sem-97M | 10000 | 3400 | 1.2 | 4.7 | 6.5 | 1.8 | 0.972 | 0.763 | 726 | 46.1 |
| sem-311M | 10000 | 3419 | 1.1 | 4.6 | 6.7 | 1.7 | 0.972 | 0.763 | 726 | 66.7 |
| viva | 100000 | 2689 | 2.1 | 43.1 | 72.6 | 23.7 | 0.899 | 0.637 | 4594 | 247.9 |
| uniform | 100000 | 2689 | 1.7 | 42.5 | 71.8 | 23.7 | 0.946 | 0.719 | 4594 | 247.9 |
| sem-97M | 100000 | 2650 | 2.1 | 43.2 | 73.3 | 23.8 | 0.899 | 0.637 | 4594 | 466.7 |

## Ramo vettoriale (v0.2)

| modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|
| 97M | 10000 | 187 | 24 | 4.3 | 15.7 | 17.5 | 17.0 | 19.2 | 16.8 | 19.3 |
| 311M | 10000 | 49 | 67 | 8.1 | 32.1 | 33.8 | 33.4 | 35.0 | 33.5 | 35.8 |
| 97M | 100000 | 110 | 148 | 42.8 | 64.2 | 80.2 | 78.9 | 100.5 | 78.7 | 100.1 |

Richiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):

| note | modalità | fattuali | parafrasi | multi-salto | inglese |
|---|---|---|---|---|---|
| 10000 | testo | 1.000/0.915 (301) | 1.000/0.648 (244) | 0.000/0.000 (20) | 1.000/0.747 (161) |
| 10000 | vettori | 1.000/0.965 (301) | 0.996/0.944 (244) | 0.000/0.000 (20) | 1.000/0.991 (161) |
| 10000 | rrf | 1.000/0.965 (301) | 1.000/0.969 (244) | 0.000/0.000 (20) | 1.000/0.845 (161) |
| 10000 | convex-0.3 | 1.000/0.932 (301) | 1.000/0.843 (244) | 0.000/0.000 (20) | 1.000/0.755 (161) |
| 10000 | convex-0.5 | 1.000/1.000 (301) | 1.000/0.949 (244) | 0.000/0.000 (20) | 1.000/0.854 (161) |
| 10000 | convex-0.7 | 1.000/1.000 (301) | 1.000/0.984 (244) | 0.000/0.000 (20) | 1.000/0.941 (161) |
| 100000 | testo | 0.964/0.817 (1883) | 0.908/0.514 (1529) | 0.000/0.000 (144) | 0.891/0.581 (1038) |
| 100000 | vettori | 0.988/0.892 (1883) | 0.982/0.872 (1529) | 0.000/0.000 (144) | 0.987/0.872 (1038) |
| 100000 | rrf | 1.000/0.964 (1883) | 1.000/0.973 (1529) | 0.000/0.000 (144) | 0.996/0.884 (1038) |
| 100000 | convex-0.3 | 0.994/0.926 (1883) | 0.997/0.727 (1529) | 0.000/0.000 (144) | 0.994/0.746 (1038) |
| 100000 | convex-0.5 | 1.000/0.968 (1883) | 0.998/0.792 (1529) | 0.000/0.000 (144) | 0.995/0.752 (1038) |
| 100000 | convex-0.7 | 1.000/0.978 (1883) | 0.999/0.860 (1529) | 0.000/0.000 (144) | 0.995/0.789 (1038) |

Due varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati. Richiamo@5 e MRR sulle domande generatrici. La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).
