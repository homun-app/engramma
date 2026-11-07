# Benchmark Engramma v0.3

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Ogni riga è la **mediana di 3 giri**. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). Obiettivi del §11 del documento di progetto in fondo.

| corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | testo+grafo p50 (ms) | testo+grafo p95 (ms) | richiamo@5 | MRR | domande | file (MB) | dopo compattazione (MB) | istantanea (MB) | purge (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| viva | 10000 | 3390 | 1.2 | 1.4 | 2.0 | 1.1 | 1.3 | 1.9 | 0.997 | 0.790 | 772 | 24.3 | 19.2 | 18.1 | 274 |
| uniform | 10000 | 3398 | 1.3 | 1.3 | 2.0 | 1.1 | 1.3 | 1.9 | 1.000 | 0.774 | 772 | 24.3 | 19.2 | 18.1 | 271 |
| sem-97M | 10000 | 3410 | 1.2 | 1.3 | 2.0 | 1.1 | 1.3 | 1.9 | 0.997 | 0.790 | 772 | 46.1 | 40.9 | 18.1 | 320 |
| sem-311M | 10000 | 3374 | 1.3 | 1.3 | 2.0 | 1.1 | 1.3 | 1.9 | 0.997 | 0.790 | 772 | 66.7 | 61.4 | 18.1 | 363 |
| viva | 100000 | 2422 | 1.7 | 9.3 | 12.6 | 9.1 | 9.2 | 12.5 | 0.940 | 0.683 | 4684 | 246.1 | 189.9 | 179.0 | 3378 |
| uniform | 100000 | 2694 | 2.6 | 9.4 | 12.7 | 9.1 | 9.2 | 12.5 | 0.979 | 0.739 | 4684 | 246.1 | 189.9 | 179.0 | 3551 |
| sem-97M | 100000 | 2191 | 1.7 | 9.2 | 12.5 | 9.1 | 9.1 | 12.4 | 0.940 | 0.683 | 4684 | 465.0 | 407.1 | 179.0 | 4148 |

## Ramo vettoriale (v0.2) e grafo in memoria (v0.3)

| modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | GraphIndex (ms) | nodi | archi | memoria grafo (MB) | archi spinti/domanda | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 97M | 10000 | 247 | 17 | 4.3 | 11 | 10008 | 17322 | 2.2 | 1001 | 1.2 | 1.6 | 2.2 | 3.0 | 11.0 | 11.7 |
| 311M | 10000 | 63 | 46 | 8.1 | 11 | 10008 | 17322 | 2.2 | 963 | 1.3 | 1.7 | 2.4 | 3.1 | 25.8 | 27.0 |
| 97M | 100000 | 193 | 144 | 42.8 | 123 | 100090 | 184459 | 21.6 | 695 | 4.4 | 5.1 | 11.6 | 14.8 | 20.3 | 23.5 |

Richiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):

| corpus | note | modalità | fattuali | parafrasi | multi-salto | inglese |
|---|---|---|---|---|---|---|
| sem-97M | 10000 | testo | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | testo-g0 | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | vettori | 1.000/0.938 (317) | 0.976/0.860 (252) | 0.889/0.330 (36) | 0.994/0.947 (167) |
| sem-97M | 10000 | rrf | 0.975/0.758 (317) | 0.845/0.615 (252) | 0.972/0.583 (36) | 0.832/0.478 (167) |
| sem-97M | 10000 | rrf-g0.40 | 0.975/0.758 (317) | 0.845/0.615 (252) | 0.972/0.583 (36) | 0.826/0.477 (167) |
| sem-97M | 10000 | mm-0.3 | 0.656/0.415 (317) | 0.679/0.439 (252) | 0.111/0.111 (36) | 0.509/0.242 (167) |
| sem-97M | 10000 | mm-0.5 | 0.899/0.658 (317) | 0.679/0.419 (252) | 0.861/0.861 (36) | 0.503/0.242 (167) |
| sem-97M | 10000 | mm-0.7 | 1.000/0.942 (317) | 0.948/0.823 (252) | 0.917/0.606 (36) | 0.982/0.855 (167) |
| sem-97M | 10000 | mm-0.7-g0.25 | 1.000/0.971 (317) | 1.000/0.973 (252) | 0.972/0.319 (36) | 1.000/0.969 (167) |
| sem-97M | 10000 | mm-0.7-g0.40 | 1.000/0.942 (317) | 0.948/0.823 (252) | 0.917/0.606 (36) | 0.982/0.855 (167) |
| sem-311M | 10000 | testo | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | testo-g0 | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | vettori | 0.997/0.963 (317) | 0.976/0.890 (252) | 0.972/0.391 (36) | 1.000/0.946 (167) |
| sem-311M | 10000 | rrf | 0.719/0.514 (317) | 0.560/0.400 (252) | 0.806/0.742 (36) | 0.533/0.200 (167) |
| sem-311M | 10000 | rrf-g0.40 | 0.722/0.514 (317) | 0.560/0.400 (252) | 0.806/0.742 (36) | 0.533/0.200 (167) |
| sem-311M | 10000 | mm-0.3 | 0.659/0.445 (317) | 0.675/0.423 (252) | 0.111/0.111 (36) | 0.515/0.246 (167) |
| sem-311M | 10000 | mm-0.5 | 0.830/0.581 (317) | 0.675/0.415 (252) | 0.861/0.861 (36) | 0.509/0.249 (167) |
| sem-311M | 10000 | mm-0.7 | 0.968/0.878 (317) | 0.865/0.751 (252) | 1.000/0.801 (36) | 0.922/0.700 (167) |
| sem-311M | 10000 | mm-0.7-g0.25 | 1.000/1.000 (317) | 1.000/0.996 (252) | 1.000/0.356 (36) | 1.000/0.982 (167) |
| sem-311M | 10000 | mm-0.7-g0.40 | 0.968/0.878 (317) | 0.865/0.751 (252) | 1.000/0.801 (36) | 0.922/0.700 (167) |
| sem-97M | 100000 | testo | 0.960/0.807 (1913) | 0.942/0.593 (1533) | 0.895/0.536 (237) | 0.911/0.619 (1001) |
| sem-97M | 100000 | testo-g0 | 0.960/0.807 (1913) | 0.942/0.593 (1533) | 0.895/0.536 (237) | 0.911/0.619 (1001) |
| sem-97M | 100000 | vettori | 0.981/0.883 (1913) | 0.983/0.848 (1533) | 0.599/0.227 (237) | 0.983/0.842 (1001) |
| sem-97M | 100000 | rrf | 0.999/0.776 (1913) | 0.975/0.629 (1533) | 0.574/0.328 (237) | 0.963/0.536 (1001) |
| sem-97M | 100000 | rrf-g0.40 | 0.999/0.776 (1913) | 0.975/0.629 (1533) | 0.574/0.328 (237) | 0.963/0.536 (1001) |
| sem-97M | 100000 | mm-0.3 | 0.997/0.869 (1913) | 0.988/0.776 (1533) | 0.992/0.754 (237) | 0.965/0.671 (1001) |
| sem-97M | 100000 | mm-0.5 | 0.999/0.907 (1913) | 0.993/0.853 (1533) | 0.987/0.805 (237) | 0.985/0.783 (1001) |
| sem-97M | 100000 | mm-0.7 | 0.996/0.911 (1913) | 0.992/0.864 (1533) | 0.949/0.503 (237) | 0.990/0.818 (1001) |
| sem-97M | 100000 | mm-0.7-g0.25 | 0.998/0.912 (1913) | 0.993/0.878 (1533) | 0.536/0.165 (237) | 0.994/0.832 (1001) |
| sem-97M | 100000 | mm-0.7-g0.40 | 0.996/0.911 (1913) | 0.992/0.864 (1533) | 0.949/0.503 (237) | 0.990/0.818 (1001) |

Due varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati; «testo+grafo» è la stessa modalità testo con i salti accesi (il PPR ha sostituito la camminata SQL). Richiamo@5 e MRR sulle domande generatrici. «Ibrido caldo»: la LRU delle domande tiene tutto il corpus (capacità = numero di domande), niente re-incorporazioni. «Dopo compattazione»: payload dimenticati fino a 1000 operazioni dalla fine (v0.3); «istantanea» è il file della compattazione (v0.3.1: stato logico, una sola vive) e «purge» è una purga a corpus pieno, FTS optimize e VACUUM compresi (v0.3.1). La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).
