# Benchmark Engramma v0.3

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Ogni riga è la **mediana di 3 giri**. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). Obiettivi del §11 del documento di progetto in fondo.

| corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | testo+grafo p50 (ms) | testo+grafo p95 (ms) | richiamo@5 | MRR | domande | file (MB) | dopo compattazione (MB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| viva | 10000 | 2609 | 1.3 | 3.5 | 8.9 | 1.2 | — | — | 0.997 | 0.790 | 772 | 24.3 | 19.2 |
| uniform | 10000 | 1947 | 1.0 | 1.8 | 2.9 | 1.6 | — | — | 1.000 | 0.774 | 772 | 24.3 | 19.2 |
| sem-97M | 10000 | 2595 | 1.2 | 1.3 | 1.9 | 1.1 | 1.3 | 1.8 | 0.997 | 0.790 | 772 | 46.1 | 40.9 |
| sem-311M | 10000 | 2421 | 1.2 | 1.3 | 1.8 | 1.1 | 1.3 | 1.8 | 0.997 | 0.790 | 772 | 66.7 | 61.5 |
| viva | 100000 | 1486 | 1.5 | 10.3 | 16.6 | 9.3 | — | — | 0.940 | 0.683 | 4684 | 246.1 | 190.0 |
| uniform | 100000 | 2514 | 1.7 | 9.6 | 12.9 | 9.3 | — | — | 0.979 | 0.739 | 4684 | 246.1 | 190.0 |
| sem-97M | 100000 | 2406 | 1.6 | 7.7 | 11.2 | 7.5 | 7.6 | 10.5 | 0.940 | 0.683 | 4684 | 465.0 | 408.8 |

## Ramo vettoriale (v0.2) e grafo in memoria (v0.3)

| modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | GraphIndex (ms) | nodi | archi | memoria grafo (MB) | archi spinti/domanda | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 97M | 10000 | 253 | 17 | 4.3 | 11 | 10008 | 17322 | 2.2 | 1001 | 1.2 | 1.4 | 2.1 | 2.6 | 10.3 | 11.0 |
| 311M | 10000 | 76 | 37 | 8.1 | 11 | 10008 | 17322 | 2.2 | 963 | 1.3 | 1.6 | 2.4 | 3.0 | 25.7 | 27.1 |
| 97M | 100000 | 216 | 125 | 42.8 | 123 | 100090 | 184459 | 21.6 | 695 | 4.2 | 4.7 | 10.1 | 13.3 | 19.4 | 22.5 |

Richiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):

| corpus | note | modalità | fattuali | parafrasi | multi-salto | inglese |
|---|---|---|---|---|---|---|
| sem-97M | 10000 | testo | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | testo-g0 | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | vettori | 1.000/0.938 (317) | 0.976/0.860 (252) | 0.889/0.330 (36) | 0.994/0.947 (167) |
| sem-97M | 10000 | rrf | 0.975/0.758 (317) | 0.845/0.615 (252) | 0.972/0.583 (36) | 0.832/0.478 (167) |
| sem-97M | 10000 | mm-0.3 | 0.656/0.415 (317) | 0.679/0.439 (252) | 0.111/0.111 (36) | 0.509/0.242 (167) |
| sem-97M | 10000 | mm-0.5 | 0.899/0.658 (317) | 0.679/0.419 (252) | 0.861/0.861 (36) | 0.503/0.242 (167) |
| sem-97M | 10000 | mm-0.7 | 1.000/0.942 (317) | 0.948/0.823 (252) | 0.917/0.606 (36) | 0.982/0.855 (167) |
| sem-97M | 10000 | mm-0.7-g0.25 | 1.000/0.971 (317) | 1.000/0.973 (252) | 0.972/0.319 (36) | 1.000/0.969 (167) |
| sem-97M | 10000 | mm-0.7-g0.40 | 1.000/0.942 (317) | 0.948/0.823 (252) | 0.917/0.606 (36) | 0.982/0.855 (167) |
| sem-97M | 10000 | th-0.3 | 0.666/0.459 (317) | 0.679/0.420 (252) | 0.639/0.583 (36) | 0.515/0.278 (167) |
| sem-97M | 10000 | th-0.5 | 0.868/0.648 (317) | 0.679/0.490 (252) | 0.972/0.738 (36) | 0.515/0.298 (167) |
| sem-97M | 10000 | th-0.7 | 0.953/0.854 (317) | 0.909/0.752 (252) | 0.972/0.433 (36) | 0.904/0.638 (167) |
| sem-311M | 10000 | testo | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | testo-g0 | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | vettori | 0.997/0.963 (317) | 0.976/0.890 (252) | 0.972/0.391 (36) | 1.000/0.946 (167) |
| sem-311M | 10000 | rrf | 0.722/0.516 (317) | 0.560/0.400 (252) | 0.806/0.742 (36) | 0.533/0.200 (167) |
| sem-311M | 10000 | mm-0.3 | 0.659/0.445 (317) | 0.675/0.423 (252) | 0.111/0.111 (36) | 0.515/0.246 (167) |
| sem-311M | 10000 | mm-0.5 | 0.830/0.581 (317) | 0.675/0.415 (252) | 0.861/0.861 (36) | 0.509/0.249 (167) |
| sem-311M | 10000 | mm-0.7 | 0.968/0.878 (317) | 0.865/0.751 (252) | 1.000/0.801 (36) | 0.922/0.700 (167) |
| sem-311M | 10000 | mm-0.7-g0.25 | 1.000/1.000 (317) | 1.000/0.996 (252) | 1.000/0.356 (36) | 1.000/0.982 (167) |
| sem-311M | 10000 | mm-0.7-g0.40 | 0.968/0.878 (317) | 0.865/0.751 (252) | 1.000/0.801 (36) | 0.922/0.700 (167) |
| sem-311M | 10000 | th-0.3 | 0.672/0.472 (317) | 0.679/0.374 (252) | 0.333/0.272 (36) | 0.515/0.297 (167) |
| sem-311M | 10000 | th-0.5 | 0.839/0.610 (317) | 0.679/0.469 (252) | 0.667/0.380 (36) | 0.515/0.299 (167) |
| sem-311M | 10000 | th-0.7 | 0.830/0.715 (317) | 0.778/0.666 (252) | 0.667/0.218 (36) | 0.731/0.475 (167) |
| sem-97M | 100000 | testo | 0.960/0.807 (1913) | 0.942/0.593 (1533) | 0.895/0.536 (237) | 0.911/0.619 (1001) |
| sem-97M | 100000 | testo-g0 | 0.960/0.807 (1913) | 0.942/0.593 (1533) | 0.895/0.536 (237) | 0.911/0.619 (1001) |
| sem-97M | 100000 | vettori | 0.981/0.883 (1913) | 0.983/0.848 (1533) | 0.599/0.227 (237) | 0.983/0.842 (1001) |
| sem-97M | 100000 | rrf | 0.999/0.776 (1913) | 0.975/0.629 (1533) | 0.574/0.328 (237) | 0.963/0.536 (1001) |
| sem-97M | 100000 | mm-0.3 | 0.997/0.869 (1913) | 0.988/0.776 (1533) | 0.992/0.754 (237) | 0.965/0.671 (1001) |
| sem-97M | 100000 | mm-0.5 | 0.999/0.907 (1913) | 0.993/0.853 (1533) | 0.987/0.805 (237) | 0.985/0.783 (1001) |
| sem-97M | 100000 | mm-0.7 | 0.996/0.911 (1913) | 0.992/0.864 (1533) | 0.949/0.503 (237) | 0.990/0.818 (1001) |
| sem-97M | 100000 | mm-0.7-g0.25 | 0.998/0.912 (1913) | 0.993/0.878 (1533) | 0.536/0.165 (237) | 0.994/0.832 (1001) |
| sem-97M | 100000 | mm-0.7-g0.40 | 0.996/0.911 (1913) | 0.992/0.864 (1533) | 0.949/0.503 (237) | 0.990/0.818 (1001) |
| sem-97M | 100000 | th-0.3 | 0.990/0.611 (1913) | 0.811/0.354 (1533) | 0.928/0.607 (237) | 0.721/0.349 (1001) |
| sem-97M | 100000 | th-0.5 | 0.995/0.616 (1913) | 0.796/0.340 (1533) | 0.937/0.622 (237) | 0.712/0.334 (1001) |
| sem-97M | 100000 | th-0.7 | 0.998/0.634 (1913) | 0.851/0.384 (1533) | 0.557/0.254 (237) | 0.758/0.350 (1001) |

Le colonne «GraphIndex (ms)» e «dopo compattazione» vengono dalla sonda
`ENGRAMMA_GRAPH_PROBE=1` (stesso generatore, stesso seme): nella prima
tiratura il banco misurava la costruzione del grafo DOPO il pass di testo
che l'aveva già costruita (0 ms) e la compattazione non compattava il file
vivo (le pagine libere restavano a SQLite); entrambi corretti — la
compattazione ora fa `VACUUM` anche della cella viva e la costruzione è
misurata prima di ogni ricerca. Le righe sem della colonna «dopo
compattazione» sono derivate (stesse operazioni, stessi byte di payload:
246,1−56,2 = 190,0 a 100k, più i vettori); le righe viva/uniform a 10k/100k
sono misurate. La spazzatura completa del peso del grafo sta nel REPORT
§v0.3 (taratura).

Due varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati; «testo+grafo» è la stessa modalità testo con i salti accesi (il PPR ha sostituito la camminata SQL). Richiamo@5 e MRR sulle domande generatrici. «Ibrido caldo»: la LRU delle domande tiene tutto il corpus (capacità = numero di domande), niente re-incorporazioni. «Dopo compattazione»: payload dimenticati fino a 1000 operazioni dalla fine (v0.3). La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).
