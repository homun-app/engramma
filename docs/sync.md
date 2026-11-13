# La sincronizzazione (v0.6)

La stessa memoria sui **tuoi** dispositivi: il portatile, il Mac di casa, più avanti il telefono. La condivisione con altre persone arriva nella v0.6.x, sopra questa base.

## Come si abbina un dispositivo

Due modi, un esito.

1. **Il biglietto** (senza server): `engramma devices invite` stampa un testo — l'indirizzo iroh di chi invita e un segreto a 128 bit, base32 col prefisso di versione. Vale **dieci minuti** e **una volta**. Sul dispositivo nuovo: `engramma devices join <biglietto> --name <nome>`. Chi invita autentica chi entra perché dimostra di conoscere il segreto (MAC sul transcript della connessione, mai il segreto in chiaro).
2. **Il codice breve** (col nodo cieco come punto d'incontro): `engramma devices code` stampa un codice tipo `7-tigre-lago` — un numero di canale e due parole italiane da una lista fissa di 2048 (è nel repo). Le due parti si incontrano sul nodo cieco e fanno SPAKE2 col codice come password: il nodo vede solo il numero di canale. **Un tentativo per codice**: un attaccante attivo ha una probabilità su 2048². (*La crate `spake2` non ha audit indipendente: l'uso è confinato a questo percorso.*)

**La conferma.** In entrambi i casi, alla fine, entrambi gli schermi stampano le stesse **sei cifre**. L'umano le confronta e conferma su **entrambi** i dispositivi. Solo dopo la `add` firma e il dispositivo nuovo riceve le celle condivise.

## Come si condivide una cella

```
engramma sync share <cella>
```

La cella registra un'operazione `adopt` (la radice di Merkle di tutto ciò che esisteva) e apre l'epoca 0 della sua chiave. Da lì ogni operazione viaggia **sigillata**: XChaCha20-Poly1305 sotto una chiave d'epoca, la chiave d'epoca avvolta per ciascun dispositivo con HPKE. `engramma sync unshare <cella>` smette di sincronizzarla **da qui**: le copie sugli altri dispositivi restano, e il comando lo dice.

## Il nodo cieco su un computer sempre accesso

```
engramma mailbox serve --root <dir>
engramma mailbox add <biglietto del nodo>
```

Un endpoint iroh (ALPN `engramma/mailbox/1`) che conserva e consegna buste sigillate quando i tuoi dispositivi non sono accesi insieme. **Non ha le chiavi**: non può leggere nulla di quello che conserva. Vede chiavi, orari e dimensioni — non testo, nomi, titoli, né l'identità delle operazioni (gli id che indicizzano sono HMAC, stabili attraverso le epoche). La ritenzione è di 180 giorni (configurabile). Quando elimini una nota col diritto all'oblio, la richiesta di cancellazione arriva **firmata** al nodo: le buste di quella nota se ne vanno davvero (cancellazione e VACUUM — nessun byte resta nel file; è provato con la ricerca di stringhe nel file).

## Il relay: pubblico, proprio, o nessuno

- di base il relay pubblico di n0 (iroh);
- uno proprio: `--relay <url>`;
- nessuno: `--no-relay`, solo connessione diretta.

La pubblicazione dell'indirizzo su DNS (pkarr) è **facoltativa e spenta di base**: manderebbe a terzi l'id e il relay del dispositivo. Il relay attraversa il NAT dentro QUIC e vede solo dati cifrati (le buste sono sigillate anche per lui, non solo per QUIC).

## Cosa vede chi

| | relay | nodo cieco | un altro dispositivo della lista |
|---|---|---|---|
| chiavi degli endpoint, IP, orari, dimensioni | sì | sì | sì (è un pari) |
| testo delle note, nomi delle celle, titoli, tag, autori | no | no | sì (è della lista) |
| `op_id`, HLC in chiaro | no | no (id opachi) | sì |
| chiavi di cella | no | no | sì (è della lista) |

Un estraneo che si finge un dispositivo: la connessione si chiude **prima di qualsiasi scambio** (la chiave iroh del chiamante deve essere quella di trasporto di un dispositivo vivo).

## Perdere un dispositivo

`engramma devices remove <nome>` da un altro dispositivo:

1. toglie il dispositivo dall'elenco;
2. **ruota la chiave di ogni cella condivisa** (un'epoca nuova): le operazioni nuove non sono più leggibili da quel dispositivo;
3. da allora rifiuta le operazioni firmate da lui che il dispositivo che rimuove non aveva già accettato.

**Quello che il dispositivo aveva già lo tiene**: la revoca non cancella i dischi altrui. Le sei cifre e il biglietto monouso sono le difese dell'**abbinamento**; la rotazione è la difesa del **dopo**.

## I conflitti

Le modifiche concorrenti non si perdono mai: vince l'ultima nell'ordine totale, e la perdente diventa una **copia di conflitto** — una nota attiva, col tag `conflitto` e un arco verso l'originale, identica su ogni copia. `engramma sync conflicts` le elenca; le tue correzioni su una copia sono operazioni normali. Una lapide vince sulle modifiche che non l'avevano vista; chi l'aveva vista può scrivere e la nota si riapre. La `purge` domina su tutto.

## Che cosa non si promette

Né sicurezza dopo compromissione (una chiave d'epoca rubata apre tutta l'epoca) né segretezza in avanti per messaggio. Per i dispositivi di una sola persona, con la rotazione alla rimozione, è il compromesso giusto oggi; MLS e i suoi successori (Keyhive, DCGKA) sono la strada per la condivisione con altre persone — e ancora non sono pronti. Le scelte sono svolute nel DESIGN § 10, con le fonti.
