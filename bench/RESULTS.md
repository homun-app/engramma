# Benchmark Engramma v0.3

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Ogni riga è la **mediana di 3 giri**. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). Obiettivi del §11 del documento di progetto in fondo.

| corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | testo+grafo p50 (ms) | testo+grafo p95 (ms) | richiamo@5 | MRR | domande | file (MB) | impronta prima (MB) | dopo compatt. base (MB) | dopo compatt. +istantanea (MB) | compact base (s) | compact +istantanea (s) | purge (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| viva | 100000 | 2331 | 1.5 | 7.3 | 10.8 | 7.1 | 7.1 | 10.4 | 0.940 | 0.683 | 4684 | 283.0 | 283.0 | 224.4 | 437.9 | 1.3 | 3.9 | 3564 |
| uniform | 100000 | 2312 | 1.8 | 7.5 | 11.1 | 7.1 | 7.2 | 10.5 | 0.979 | 0.739 | 4684 | 283.0 | 283.0 | 224.4 | 437.9 | 1.3 | 3.7 | 3727 |
| sem-97M | 100000 | 2522 | 1.5 | 7.1 | 10.3 | 6.9 | 6.9 | 9.7 | 0.940 | 0.683 | 4684 | 501.7 | 501.7 | 441.6 | 655.1 | 2.0 | 5.1 | 4206 |

## Ramo vettoriale (v0.2) e grafo in memoria (v0.3)

| modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | GraphIndex (ms) | nodi | archi | memoria grafo (MB) | archi spinti/domanda | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 97M | 100000 | 92 | 117 | 42.8 | 178 | 103044 | 184459 | 21.9 | 555 | 3.5 | 4.0 | 9.4 | 12.8 | 18.5 | 21.8 |

Richiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):

| corpus | note | modalità | fattuali | parafrasi | multi-salto | inglese |
|---|---|---|---|---|---|---|
| sem-97M | 100000 | testo | 0.960/0.807 (1913) | 0.942/0.593 (1533) | 0.895/0.536 (237) | 0.911/0.619 (1001) |
| sem-97M | 100000 | testo-g0 | 0.960/0.807 (1913) | 0.942/0.593 (1533) | 0.895/0.536 (237) | 0.911/0.619 (1001) |
| sem-97M | 100000 | vettori | 0.983/0.884 (1913) | 0.989/0.862 (1533) | 0.342/0.116 (237) | 0.993/0.852 (1001) |
| sem-97M | 100000 | rrf | 0.999/0.732 (1913) | 0.970/0.552 (1533) | 0.397/0.272 (237) | 0.935/0.465 (1001) |
| sem-97M | 100000 | rrf-g0.40 | 0.999/0.731 (1913) | 0.965/0.548 (1533) | 0.527/0.304 (237) | 0.933/0.461 (1001) |
| sem-97M | 100000 | mm-0.3 | 0.997/0.870 (1913) | 0.988/0.776 (1533) | 0.992/0.748 (237) | 0.967/0.672 (1001) |
| sem-97M | 100000 | mm-0.5 | 0.999/0.907 (1913) | 0.993/0.859 (1533) | 0.983/0.750 (237) | 0.988/0.789 (1001) |
| sem-97M | 100000 | mm-0.7 | 0.997/0.912 (1913) | 0.993/0.873 (1533) | 0.865/0.356 (237) | 0.993/0.827 (1001) |
| sem-97M | 100000 | mm-0.6-g0.40 | 0.999/0.910 (1913) | 0.992/0.862 (1533) | 0.987/0.795 (237) | 0.991/0.803 (1001) |
| sem-97M | 100000 | mm-0.7-g0.35 | 0.997/0.912 (1913) | 0.993/0.873 (1533) | 0.865/0.356 (237) | 0.993/0.827 (1001) |
| sem-97M | 100000 | mm-0.7-g0.25 | 0.998/0.912 (1913) | 0.993/0.878 (1533) | 0.536/0.165 (237) | 0.994/0.832 (1001) |
| sem-97M | 100000 | mm-0.7-g0.40 | 0.996/0.911 (1913) | 0.992/0.864 (1533) | 0.949/0.503 (237) | 0.990/0.818 (1001) |

Due varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati; «testo+grafo» è la stessa modalità testo con i salti accesi (il PPR ha sostituito la camminata SQL). Richiamo@5 e MRR sulle domande generatrici. «Ibrido caldo»: la LRU delle domande tiene tutto il corpus (capacità = numero di domande), niente re-incorporazioni. «Impronta» è cella + WAL + file d'istantanea (v0.3.2: il conto onesto): «prima» è l'impronta subito prima della compattazione, «dopo compatt. base» segue la compattazione senza istantanea (punto di controllo, nessuna seconda copia), «dopo compatt. +istantanea» segue quella con --keep-snapshot (v0.3.1: stato logico, una sola vive); i payload sono dimenticati fino a 1000 operazioni dalla fine in entrambi i casi, e «purge» è una purga a corpus pieno con l'istantanea presente, FTS optimize e VACUUM compresi. La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).
