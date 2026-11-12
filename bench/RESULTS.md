# Benchmark Engramma v0.3

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Ogni riga è la **mediana di 1 giri**. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). Obiettivi del §11 del documento di progetto in fondo.

| corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | testo+grafo p50 (ms) | testo+grafo p95 (ms) | richiamo@5 | MRR | domande | file (MB) | impronta prima (MB) | dopo compatt. base (MB) | dopo compatt. +istantanea (MB) | compact base (s) | compact +istantanea (s) | purge (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| viva | 10000 | 3290 | 0.6 | 1.4 | 2.0 | 1.2 | 1.3 | 2.0 | 0.997 | 0.790 | 772 | 24.4 | 24.4 | 19.2 | 37.4 | 0.1 | 0.2 | 279 |
| uniform | 10000 | 2903 | 0.4 | 1.4 | 2.1 | 1.2 | 1.4 | 2.2 | 1.000 | 0.774 | 772 | 24.4 | 24.4 | 19.2 | 37.4 | 0.1 | 0.2 | 284 |
| sem-97M | 10000 | 3181 | 0.6 | 1.4 | 2.0 | 1.1 | 1.3 | 1.9 | 0.997 | 0.790 | 772 | 46.2 | 46.2 | 40.9 | 59.1 | 0.1 | 0.4 | 341 |
| sem-311M | 10000 | 3247 | 0.5 | 1.4 | 2.1 | 1.2 | 1.3 | 1.9 | 0.997 | 0.790 | 772 | 66.7 | 66.7 | 61.4 | 79.7 | 0.2 | 0.6 | 388 |

## Ramo vettoriale (v0.2) e grafo in memoria (v0.3)

| modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | GraphIndex (ms) | nodi | archi | memoria grafo (MB) | archi spinti/domanda | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 97M | 10000 | 96 | 22 | 4.3 | 15 | 10544 | 17322 | 2.2 | 990 | 1.2 | 1.6 | 2.3 | 3.0 | 11.1 | 12.1 |
| 311M | 10000 | 30 | 36 | 8.1 | 16 | 10544 | 17322 | 2.2 | 966 | 1.3 | 1.7 | 2.4 | 3.3 | 25.7 | 27.4 |

Richiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):

| corpus | note | modalità | fattuali | parafrasi | multi-salto | inglese |
|---|---|---|---|---|---|---|
| sem-97M | 10000 | testo | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | testo-g0 | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | vettori | 1.000/0.944 (317) | 0.992/0.920 (252) | 0.611/0.207 (36) | 1.000/0.959 (167) |
| sem-97M | 10000 | rrf | 0.997/0.839 (317) | 0.861/0.665 (252) | 0.972/0.575 (36) | 0.892/0.590 (167) |
| sem-97M | 10000 | rrf-g0.40 | 0.975/0.758 (317) | 0.845/0.615 (252) | 0.972/0.583 (36) | 0.826/0.477 (167) |
| sem-97M | 10000 | mm-0.3 | 0.905/0.676 (317) | 0.679/0.439 (252) | 0.111/0.111 (36) | 0.509/0.242 (167) |
| sem-97M | 10000 | mm-0.5 | 1.000/0.933 (317) | 0.948/0.684 (252) | 0.861/0.861 (36) | 0.976/0.723 (167) |
| sem-97M | 10000 | mm-0.7 | 1.000/0.965 (317) | 0.976/0.882 (252) | 0.917/0.426 (36) | 0.994/0.948 (167) |
| sem-97M | 10000 | mm-0.6-g0.40 | 1.000/0.887 (317) | 0.933/0.588 (252) | 0.889/0.889 (36) | 0.958/0.530 (167) |
| sem-97M | 10000 | mm-0.7-g0.35 | 1.000/0.965 (317) | 0.976/0.882 (252) | 0.917/0.426 (36) | 0.994/0.948 (167) |
| sem-97M | 10000 | mm-0.7-g0.25 | 1.000/0.971 (317) | 1.000/0.973 (252) | 0.972/0.319 (36) | 1.000/0.969 (167) |
| sem-97M | 10000 | mm-0.7-g0.40 | 1.000/0.942 (317) | 0.948/0.823 (252) | 0.917/0.606 (36) | 0.982/0.855 (167) |
| sem-311M | 10000 | testo | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | testo-g0 | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | vettori | 1.000/0.989 (317) | 1.000/0.978 (252) | 0.889/0.306 (36) | 1.000/0.969 (167) |
| sem-311M | 10000 | rrf | 0.814/0.589 (317) | 0.659/0.464 (252) | 0.778/0.736 (36) | 0.635/0.305 (167) |
| sem-311M | 10000 | rrf-g0.40 | 0.719/0.514 (317) | 0.560/0.400 (252) | 0.806/0.742 (36) | 0.533/0.200 (167) |
| sem-311M | 10000 | mm-0.3 | 0.893/0.652 (317) | 0.675/0.423 (252) | 0.111/0.111 (36) | 0.515/0.246 (167) |
| sem-311M | 10000 | mm-0.5 | 0.981/0.920 (317) | 0.857/0.598 (252) | 0.861/0.861 (36) | 0.922/0.570 (167) |
| sem-311M | 10000 | mm-0.7 | 1.000/0.989 (317) | 0.972/0.881 (252) | 1.000/0.576 (36) | 1.000/0.949 (167) |
| sem-311M | 10000 | mm-0.6-g0.40 | 0.918/0.771 (317) | 0.790/0.465 (252) | 0.972/0.972 (36) | 0.778/0.379 (167) |
| sem-311M | 10000 | mm-0.7-g0.35 | 1.000/0.989 (317) | 0.972/0.881 (252) | 1.000/0.576 (36) | 1.000/0.949 (167) |
| sem-311M | 10000 | mm-0.7-g0.25 | 1.000/1.000 (317) | 1.000/0.996 (252) | 1.000/0.356 (36) | 1.000/0.982 (167) |
| sem-311M | 10000 | mm-0.7-g0.40 | 0.968/0.878 (317) | 0.865/0.751 (252) | 1.000/0.801 (36) | 0.922/0.700 (167) |

Due varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati; «testo+grafo» è la stessa modalità testo con i salti accesi (il PPR ha sostituito la camminata SQL). Richiamo@5 e MRR sulle domande generatrici. «Ibrido caldo»: la LRU delle domande tiene tutto il corpus (capacità = numero di domande), niente re-incorporazioni. «Impronta» è cella + WAL + file d'istantanea (v0.3.2: il conto onesto): «prima» è l'impronta subito prima della compattazione, «dopo compatt. base» segue la compattazione senza istantanea (punto di controllo, nessuna seconda copia), «dopo compatt. +istantanea» segue quella con --keep-snapshot (v0.3.1: stato logico, una sola vive); i payload sono dimenticati fino a 1000 operazioni dalla fine in entrambi i casi, e «purge» è una purga a corpus pieno con l'istantanea presente, FTS optimize e VACUUM compresi. La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).
