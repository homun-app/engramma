# Benchmark Engramma v0.2.1

Generato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). Questa tornata: un giro, dopo la correzione bitemporale della recenza (commit e9298c2); le mediane di 3 giri delle latenze sono nel REPORT §v0.2.1 (tornata precedente: identica nel percorso semantico, che la correzione non tocca). La qualità è deterministica. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). Obiettivi del §11 del documento di progetto in fondo.

| corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | richiamo@5 | MRR | domande | file (MB) |
|---|---|---|---|---|---|---|---|---|---|---|
| viva | 10000 | 3620 | 1.1 | 6.3 | 10.7 | 1.1 | 0.997 | 0.790 | 772 | 24.3 |
| uniform | 10000 | 3563 | 1.0 | 5.7 | 9.6 | 1.1 | 1.000 | 0.774 | 772 | 24.3 |
| sem-97M | 10000 | 3572 | 1.0 | 5.7 | 9.6 | 1.1 | 0.997 | 0.790 | 772 | 46.1 |
| sem-311M | 10000 | 3426 | 0.7 | 5.8 | 9.7 | 1.1 | 0.997 | 0.790 | 772 | 66.7 |
| viva | 100000 | 2092 | 1.6 | 43.5 | 54.3 | 9.2 | 0.940 | 0.683 | 4684 | 246.3 |
| uniform | 100000 | 2279 | 1.5 | 43.6 | 56.1 | 9.0 | 0.979 | 0.739 | 4684 | 246.3 |
| sem-97M | 100000 | 2087 | 2.2 | 43.8 | 54.5 | 9.1 | 0.940 | 0.683 | 4684 | 465.2 |

## Ramo vettoriale (v0.2)

| modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|
| 97M | 10000 | 202 | 22 | 4.3 | 31.7 | 78.5 | 28.4 | 55.7 | 23.9 | 33.6 |
| 311M | 10000 | 69 | 46 | 8.1 | 35.3 | 41.7 | 33.2 | 42.5 | 33.3 | 42.0 |
| 97M | 100000 | 183 | 165 | 42.8 | 51.2 | 70.7 | 57.0 | 68.1 | 57.4 | 70.8 |

Richiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):

| corpus | note | modalità | fattuali | parafrasi | multi-salto | inglese |
|---|---|---|---|---|---|---|
| sem-97M | 10000 | testo | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-97M | 10000 | vettori | 1.000/0.959 (317) | 0.996/0.973 (252) | 0.000/0.000 (36) | 1.000/0.971 (167) |
| sem-97M | 10000 | rrf | 1.000/0.946 (317) | 1.000/0.901 (252) | 0.389/0.095 (36) | 1.000/0.838 (167) |
| sem-97M | 10000 | mm-0.3 | 1.000/1.000 (317) | 1.000/0.986 (252) | 1.000/0.333 (36) | 1.000/0.961 (167) |
| sem-97M | 10000 | mm-0.5 | 1.000/0.987 (317) | 1.000/0.984 (252) | 1.000/0.262 (36) | 1.000/0.970 (167) |
| sem-97M | 10000 | mm-0.7 | 1.000/0.972 (317) | 1.000/0.982 (252) | 0.333/0.088 (36) | 1.000/0.976 (167) |
| sem-97M | 10000 | th-0.3 | 1.000/0.904 (317) | 1.000/0.784 (252) | 1.000/0.368 (36) | 1.000/0.755 (167) |
| sem-97M | 10000 | th-0.5 | 1.000/0.995 (317) | 1.000/0.931 (252) | 0.556/0.250 (36) | 1.000/0.802 (167) |
| sem-97M | 10000 | th-0.7 | 1.000/0.997 (317) | 1.000/0.962 (252) | 0.472/0.227 (36) | 1.000/0.913 (167) |
| sem-311M | 10000 | testo | 0.994/0.886 (317) | 1.000/0.697 (252) | 1.000/0.722 (36) | 1.000/0.763 (167) |
| sem-311M | 10000 | vettori | 1.000/0.997 (317) | 1.000/0.996 (252) | 0.000/0.000 (36) | 1.000/0.981 (167) |
| sem-311M | 10000 | rrf | 1.000/0.995 (317) | 1.000/0.899 (252) | 0.639/0.153 (36) | 1.000/0.832 (167) |
| sem-311M | 10000 | mm-0.3 | 1.000/1.000 (317) | 1.000/0.996 (252) | 1.000/0.324 (36) | 1.000/0.964 (167) |
| sem-311M | 10000 | mm-0.5 | 1.000/1.000 (317) | 1.000/0.996 (252) | 0.972/0.255 (36) | 1.000/0.982 (167) |
| sem-311M | 10000 | mm-0.7 | 1.000/1.000 (317) | 1.000/0.996 (252) | 0.667/0.157 (36) | 1.000/0.985 (167) |
| sem-311M | 10000 | th-0.3 | 1.000/0.876 (317) | 1.000/0.702 (252) | 1.000/0.450 (36) | 1.000/0.752 (167) |
| sem-311M | 10000 | th-0.5 | 1.000/0.942 (317) | 1.000/0.876 (252) | 0.778/0.382 (36) | 1.000/0.765 (167) |
| sem-311M | 10000 | th-0.7 | 1.000/0.996 (317) | 1.000/0.976 (252) | 0.750/0.375 (36) | 1.000/0.886 (167) |
| sem-97M | 100000 | testo | 0.960/0.807 (1913) | 0.942/0.593 (1533) | 0.895/0.536 (237) | 0.911/0.619 (1001) |
| sem-97M | 100000 | vettori | 0.984/0.885 (1913) | 0.990/0.871 (1533) | 0.025/0.008 (237) | 0.996/0.857 (1001) |
| sem-97M | 100000 | rrf | 0.999/0.777 (1913) | 0.989/0.643 (1533) | 0.097/0.031 (237) | 0.965/0.543 (1001) |
| sem-97M | 100000 | mm-0.3 | 0.997/0.869 (1913) | 0.988/0.778 (1533) | 0.245/0.065 (237) | 0.968/0.676 (1001) |
| sem-97M | 100000 | mm-0.5 | 0.999/0.907 (1913) | 0.993/0.864 (1533) | 0.139/0.038 (237) | 0.988/0.797 (1001) |
| sem-97M | 100000 | mm-0.7 | 0.998/0.912 (1913) | 0.993/0.878 (1533) | 0.063/0.019 (237) | 0.994/0.832 (1001) |
| sem-97M | 100000 | th-0.3 | 0.991/0.613 (1913) | 0.824/0.359 (1533) | 0.304/0.120 (237) | 0.725/0.351 (1001) |
| sem-97M | 100000 | th-0.5 | 0.995/0.617 (1913) | 0.827/0.360 (1533) | 0.266/0.098 (237) | 0.726/0.352 (1001) |
| sem-97M | 100000 | th-0.7 | 0.998/0.634 (1913) | 0.897/0.411 (1533) | 0.245/0.088 (237) | 0.772/0.368 (1001) |

Due varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati. Richiamo@5 e MRR sulle domande generatrici. La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).
