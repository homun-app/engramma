# Benchmark Engramma v0.3

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Ogni riga è la **mediana di 1 giri**. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). Obiettivi del §11 del documento di progetto in fondo.

| corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | testo+grafo p50 (ms) | testo+grafo p95 (ms) | richiamo@5 | MRR | domande | file (MB) | impronta prima (MB) | dopo compatt. base (MB) | dopo compatt. +istantanea (MB) | compact base (s) | compact +istantanea (s) | purge (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| viva | 10000 | 3082 | 0.6 | 1.3 | 2.1 | 1.1 | 1.3 | 2.0 | 1.000 | 0.791 | 772 | 27.3 | 27.3 | 22.0 | 42.9 | 0.1 | 0.3 | 298 |
| uniform | 10000 | 3064 | 0.6 | 1.3 | 2.0 | 1.1 | 1.3 | 1.9 | 1.000 | 0.773 | 772 | 27.3 | 27.3 | 22.0 | 42.9 | 0.1 | 0.3 | 288 |
| sem-97M | 10000 | 2981 | 0.7 | 1.3 | 2.0 | 1.1 | 1.3 | 2.0 | 1.000 | 0.791 | 772 | 49.1 | 49.1 | 43.7 | 64.6 | 0.1 | 0.4 | 338 |
| sem-311M | 10000 | 3061 | 0.7 | 1.3 | 2.0 | 1.1 | 1.3 | 1.9 | 1.000 | 0.791 | 772 | 69.6 | 69.6 | 64.2 | 85.1 | 0.2 | 0.5 | 431 |

## Ramo vettoriale (v0.2) e grafo in memoria (v0.3)

| modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | GraphIndex (ms) | nodi | archi | memoria grafo (MB) | archi spinti/domanda | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 97M | 10000 | 96 | 17 | 4.3 | 18 | 10544 | 17322 | 2.2 | 991 | 1.1 | 1.5 | 2.2 | 3.0 | 11.2 | 12.0 |
| 311M | 10000 | 35 | 36 | 8.1 | 18 | 10544 | 17322 | 2.2 | 966 | 1.3 | 1.8 | 2.5 | 3.8 | 26.8 | 28.3 |

Richiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):

| corpus | note | modalità | fattuali | parafrasi | multi-salto | inglese |
|---|---|---|---|---|---|---|
| sem-97M | 10000 | testo | 1.000/0.888 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | testo-g0 | 1.000/0.888 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | vettori | 1.000/0.944 (317) | 0.992/0.920 (252) | 0.611/0.207 (36) | 1.000/0.959 (167) |
| sem-97M | 10000 | rrf | 0.950/0.721 (317) | 0.821/0.565 (252) | 0.944/0.545 (36) | 0.760/0.413 (167) |
| sem-97M | 10000 | rrf-g0.40 | 0.927/0.700 (317) | 0.813/0.554 (252) | 0.972/0.560 (36) | 0.749/0.381 (167) |
| sem-97M | 10000 | mm-0.3 | 0.905/0.673 (317) | 0.679/0.439 (252) | 0.111/0.111 (36) | 0.509/0.242 (167) |
| sem-97M | 10000 | mm-0.5 | 1.000/0.930 (317) | 0.948/0.684 (252) | 0.861/0.861 (36) | 0.976/0.723 (167) |
| sem-97M | 10000 | mm-0.7 | 1.000/0.965 (317) | 0.976/0.882 (252) | 0.917/0.426 (36) | 0.994/0.948 (167) |
| sem-97M | 10000 | mm-0.6-g0.40 | 1.000/0.884 (317) | 0.933/0.588 (252) | 0.889/0.889 (36) | 0.958/0.530 (167) |
| sem-97M | 10000 | mm-0.7-g0.35 | 1.000/0.965 (317) | 0.976/0.882 (252) | 0.917/0.426 (36) | 0.994/0.948 (167) |
| sem-97M | 10000 | mm-0.7-g0.25 | 1.000/0.971 (317) | 1.000/0.973 (252) | 0.972/0.319 (36) | 1.000/0.969 (167) |
| sem-97M | 10000 | mm-0.7-g0.40 | 1.000/0.942 (317) | 0.948/0.823 (252) | 0.917/0.606 (36) | 0.982/0.855 (167) |
| sem-311M | 10000 | testo | 1.000/0.888 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | testo-g0 | 1.000/0.888 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | vettori | 1.000/0.989 (317) | 1.000/0.978 (252) | 0.889/0.306 (36) | 1.000/0.969 (167) |
| sem-311M | 10000 | rrf | 0.691/0.477 (317) | 0.528/0.390 (252) | 0.778/0.714 (36) | 0.467/0.180 (167) |
| sem-311M | 10000 | rrf-g0.40 | 0.669/0.463 (317) | 0.500/0.381 (252) | 0.778/0.714 (36) | 0.431/0.154 (167) |
| sem-311M | 10000 | mm-0.3 | 0.893/0.651 (317) | 0.675/0.423 (252) | 0.111/0.111 (36) | 0.515/0.246 (167) |
| sem-311M | 10000 | mm-0.5 | 0.981/0.918 (317) | 0.857/0.598 (252) | 0.861/0.861 (36) | 0.922/0.570 (167) |
| sem-311M | 10000 | mm-0.7 | 1.000/0.989 (317) | 0.972/0.881 (252) | 1.000/0.576 (36) | 1.000/0.949 (167) |
| sem-311M | 10000 | mm-0.6-g0.40 | 0.918/0.770 (317) | 0.790/0.465 (252) | 0.972/0.972 (36) | 0.778/0.379 (167) |
| sem-311M | 10000 | mm-0.7-g0.35 | 1.000/0.989 (317) | 0.972/0.881 (252) | 1.000/0.576 (36) | 1.000/0.949 (167) |
| sem-311M | 10000 | mm-0.7-g0.25 | 1.000/1.000 (317) | 1.000/0.996 (252) | 1.000/0.356 (36) | 1.000/0.982 (167) |
| sem-311M | 10000 | mm-0.7-g0.40 | 0.968/0.878 (317) | 0.865/0.751 (252) | 1.000/0.801 (36) | 0.922/0.700 (167) |

Due varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati; «testo+grafo» è la stessa modalità testo con i salti accesi (il PPR ha sostituito la camminata SQL). Richiamo@5 e MRR sulle domande generatrici. «Ibrido caldo»: la LRU delle domande tiene tutto il corpus (capacità = numero di domande), niente re-incorporazioni. «Impronta» è cella + WAL + file d'istantanea (v0.3.2: il conto onesto): «prima» è l'impronta subito prima della compattazione, «dopo compatt. base» segue la compattazione senza istantanea (punto di controllo, nessuna seconda copia), «dopo compatt. +istantanea» segue quella con --keep-snapshot (v0.3.1: stato logico, una sola vive); i payload sono dimenticati fino a 1000 operazioni dalla fine in entrambi i casi, e «purge» è una purga a corpus pieno con l'istantanea presente, FTS optimize e VACUUM compresi. La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).
