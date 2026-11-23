# La sincronizzazione (v0.6.1, con il servizio in libreria dalla v0.6.2)

> La v0.6.1 chiude ciò che la v0.6 dichiarava e non era: l'abbinamento in
> rete (biglietto e codice), `sync run` e `sync serve`, il nodo cieco che
> serve davvero, e l'identità dell'operazione che non cambia più (DESIGN
> § 4.3.1). La v0.6.2 porta il ciclo **dentro la libreria**: la CLI è un
> guscio del servizio, il binding Node lo usa da Homun — una sola copia
> del ciclo, non due da tenere allineate.

La stessa memoria sui **tuoi** dispositivi: il portatile, il Mac di casa, più avanti il telefono. La condivisione con altre persone arriva nella v0.6.x, sopra questa base.

## Come si abbina un dispositivo

Due modi, un esito.

1. **Il biglietto** (senza server): `engramma devices invite [--no-relay]` lega l'endpoint e stampa un testo — l'**indirizzo vero** di chi invita (id, relay se c'è, indirizzi diretti; con `--no-relay` solo i diretti) e un segreto a 128 bit, base32 col prefisso di versione. Vale **dieci minuti** e **una volta**, e resta in attesa del dispositivo che entra. Sul dispositivo nuovo: `engramma devices join <biglietto> --name <nome>`. Le battute: SPAKE2 col **segreto del biglietto** come password, il MAC del segreto sul transcript (l'uomo nel mezzo col biglietto altrui muore lì), la **conferma di chiave** RFC 9382 § 4 da entrambi i parti — un biglietto sbagliato si scopre lì, senza chiedere nulla all'umano.
2. **Il codice breve** (col nodo cieco come punto d'incontro): `engramma devices code` stampa un codice tipo `7-tigre-lago` — un numero di canale e due parole italiane da una lista fissa di 2048 (è nel repo). Le due parti si incontrano sul nodo cieco e fanno SPAKE2 col codice come password: il nodo vede solo il numero di canale. **Un tentativo per codice**: un attaccante attivo ha una probabilità su 2048². (*La crate `spake2` non ha audit indipendente: l'uso è confinato a questo percorso.*)

**La conferma.** In entrambi i casi, alla fine, entrambi gli schermi stampano le stesse **sei cifre**. L'umano le confronta e conferma (una riga, `s`) su **entrambi** i dispositivi; se uno dei due rifiuta, nessuna `add` da nessuna parte. Solo dopo le due conferme chi invita firma la `add`, e il dispositivo nuovo riceve l'elenco, le chiavi d'epoca **avvolte per lui** e le celle condivise — le operazioni sigillate, con l'`adopt` che apre l'era senza firma (la prova di Merkle viaggia con ogni operazione senza firma: la radice copre, la prova dimostra).

**Il codice, in pratica.** `mailbox add <biglietto del nodo>` una volta per radice; poi `engramma devices code` (chi invita) e `engramma devices join <codice> --name <nome>` (chi entra). Il nodo abbina i due lati del canale e **rimesce blob opachi**: SPAKE2 e conferma di chiave viaggiano in fondo al filo, il nodo non vede il codice. Un tentativo sbagliato **brucia il canale**: un tentativo per codice, il secondo col codice giusto è rifiutato. Dopo le sei cifre, un biglietto fresco passa **sigillato** dentro il relay: l'ingresso vero corre poi in diretta fra i due dispositivi.

## Il servizio: `sync run` e `sync serve`

Il ciclo vive in `engramma-sync` (`service::SyncService`): la CLI analizza
i flag e stampa, il modulo Node risponde a Homun, e i test della CLI
girano immutati. Chi invita per primo nomina il proprio dispositivo:
**`--name`** su `devices invite` e `devices code` (predefinito
`portatile`, come sempre); da JS è `deviceName` in `sync()`.

**Un solo servizio per radice**: un lucchetto esclusivo del sistema
operativo (`<root>/sync.lock`, flock — si rilascia anche se il processo
muore) copre il servizio continuo e ogni passata singola. Un secondo
`sync serve` sulla stessa radice — un altro processo o un altro servizio
nello stesso — riceve un rifiuto dedicato: riga a parte e uscita 7 nella
CLI, `SYNC_BUSY` in JS.

- **`engramma sync run`**: una passata con ogni dispositivo vivo raggiungibile (e col nodo cieco configurato). Il riepilogo JSON dice, per cella, operazioni inviate e ricevute, rifiuti al cancello, conflitti, **non speditibili** e frontiera; per dispositivo, se la sessione è avvenuta, per quale strada (diretta o via relay) e perché no. **Un irraggiungibile non è un errore**: è una riga.
- **`engramma sync serve`**: ascolta (ALPN `engramma/sync/4`, la versione delle regole), applica il cancello a ogni accettazione, e sincronizza a ogni scrittura locale (`PRAGMA data_version`, rimbalzo 2 s) e ogni 5 minuti. Si ferma pulito su SIGINT/SIGTERM. Il demone gira **accanto** a Homun sulla stessa radice: due processi, un registro, la sessione che li attraversa è provata.
- **Gli indirizzi**: ogni radice ricorda l'ultimo indirizzo noto di ogni dispositivo (il libro degli indirizzi, aggiornato a ogni sessione e all'abbinamento; il biglietto dell'abbinamento gli consegna anche il relay, quando c'è). `--relay <url>` e `--no-relay` come nel biglietto; pkarr spento. La scelta si **salva nella radice** (un flag esplicito vince e la sostituisce): `sync serve` non la chiede a ogni avvio. Una radice che vuole essere raggiunta senza inviti tiene `sync serve` acceso.
- **La chiusura del servizio (v0.6.8, H5)**: `shutdown()` — che `Engine.close` chiama — ferma il ciclo continuo, **annulla gli inviti e le adesioni aperti** (il biglietto di chi invita e il codice di chi entra chiudono il loro endpoint: l'altro lato sente la connessione finire, non un attacco di silenzio), chiude l'endpoint e **aspetta i fili** entro tempi dichiarati (25 s per la risposta del servizio, 10 s per filo). E da ora anche **l'ultimo handle che cade** chiude da sé: chi azzera la sync — chiude il motore e cancella la radice — con un invito in volo non si ritrova `device.key` e `devices.db` ricreati nella radice cancellata (le `.tmp*` che tornavano). Un filo che sfora il tetto è un errore **che si vede** su stderr, mai un filo che resta vivo in silenzio; dopo la chiusura niente scrive più nella radice, e `device.key` nasce solo da un'azione esplicita (`start`, un abbinamento), mai da un filo in background.

## Come si condivide una cella

```
engramma sync share <cella>
```

La cella registra un'operazione `adopt` (la radice di Merkle di tutto ciò che esisteva) e apre l'epoca 0 della sua chiave. Da lì ogni operazione viaggia **sigillata**: XChaCha20-Poly1305 sotto una chiave d'epoca, la chiave d'epoca avvolta per ciascun dispositivo con HPKE. `engramma sync unshare <cella>` smette di sincronizzarla **da qui**: le copie sugli altri dispositivi restano, e il comando lo dice.

## Il nodo cieco su un computer sempre accesso

```
engramma mailbox serve --root <dir>      # il nodo: stampa il suo biglietto
engramma mailbox add <biglietto>         # ogni radice gli consegna l'elenco firmato
engramma mailbox status                  # ultimo contatto, buste, byte, ritenzione
engramma mailbox remove                  # la radice lo dimentica
```

Un endpoint iroh (ALPN `engramma/mailbox/1`) che resta in esecuzione fino al segnale, conserva e consegna buste sigillate quando i tuoi dispositivi non sono accesi insieme. **Non ha le chiavi**: non può leggere nulla di quello che conserva. Vede chiavi, orari e dimensioni — non testo, nomi, titoli, né l'identità delle operazioni (gli id che indicizzano sono HMAC, stabili attraverso le epoche). Accetta solo le chiavi di trasporto dei dispositivi **vivi** degli elenchi che conosce: un rimosso è respinto alla porta. La ritenzione è di 180 giorni (configurabile). Quando una nota va col diritto all'oblio — su qualsiasi copia — la richiesta di cancellazione **firmata** parte alla sessione successiva e resta in coda finché il nodo conferma: le buste di quella nota se ne vanno davvero (cancellazione e VACUUM — nessun byte resta nel file; è provato con la ricerca di stringhe nel file).

## Il relay: pubblico, proprio, o nessuno

- di base il relay pubblico di n0 (iroh);
- uno proprio: `--relay <url>`;
- nessuno: `--no-relay`, solo connessione diretta.

**Il biglietto porta il relay, e il relay si aspetta.** Prima di stampare
un indirizzo che deve servire a un altro — il biglietto di `devices
invite`, quello fresco del codice breve, il biglietto del nodo cieco,
l'indirizzo che `sync serve` pubblica — l'endpoint aspetta fino a **10
secondi** che il proprio relay si colleghi: subito dopo il bind il relay
non c'è ancora, e un biglietto senza relay è un biglietto che chi è dietro
NAT non può usare. Se il tempo scade il biglietto esce **senza** relay e
la CLI **lo dice**: una riga sullo stderr e, nel JSON, `"relay": null`
con un `relay_warning`. Mai in silenzio. Con `--no-relay` non si aspetta
nulla.

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

## La versione delle regole (v0.6.5; la 4 in v0.6.7; la 5 in v0.6.10; la 6 in v0.6.11; la 7 in v0.6.12)

L'ALPN **è** la versione delle regole di materializzazione:
`engramma/sync/8` (e `engramma/mailbox/8` per la casella postale).
La v0.6.12 porta la `7`: **il prefisso congelato è immutabile** — le
righe di versione nate a o sotto il taglio di compattazione non si
toccano, le metà dominate dalla purga (e le scritture battute dalla
lapide) fotografano il pre-stato e consumano il loro slot di versione,
l'ancora della rimaterializzazione è la prima fotografia sopra il
taglio nell'ordine totale e non invecchia mai, e una cella condivisa
rifiuta con un errore leggibile l'arrivo che cada dentro il prefisso. Un
dispositivo con regole diverse, davanti allo stesso insieme di
operazioni, deriva stati diversi — quindi non devono scambiare
nemmeno un'operazione. Il rifiuto avviene all'handshake: chi chiama
riceve `PEER_OUTDATED` (in JS, con il nome del dispositivo da
aggiornare), chi risponde non vede nemmeno la connessione. Ogni cambio
della derivazione alza la versione: la v0.6.5 portava la `3` (la
`base` di una scrittura locale diventa la testa visibile, il testimone
di contenimento forcella per coppia, la riapertura della lapide
giudica la linea chiusa); la v0.6.7 porta la `4` perché il testimone
non dipende più dalla lapide (una scrittura che la lapide ha battuto
attesta lo stesso ciò che il suo autore aveva davanti) e le copie di
una nota cancellata seguono la lapide (invisibili, non cercabili,
fuori da `conflicts()`); la v0.7 delle organizzazioni alzerà ancora. La v0.6.10 porta la `5` perché la purga
domina per OPERAZIONE: un'operazione a due oggetti (una `decide`, una
`supersede`, un `link`) con un lato purgato non rigioca nessuna metà. La
v0.6.11 porta la `6` perché quella dominazione toglieva anche il
CONTENUTO PROPRIO dell'altro oggetto — la nota nuova di un `supersede`
spariva quando si purgava la vecchia — e la regola nuova la divide per
metà: il contenuto proprio resta, i riferimenti al purgato vanno (con
le riaperture dichiarate); regole 5 e 6 derivano stati diversi dallo
stesso insieme, e devono rifiutarsi a vicenda.

## I conflitti

Le modifiche concorrenti del **corpo** non si perdono mai (v0.6.4):
vince la punta col la chiave più alta — **(classe dell'autore, hlc,
op_id)**: la mano umana batte l'agente anche più vecchia — e ogni
perdente tiene una **copia di conflitto durevole**: una nota col tag
`conflitto` e un arco verso l'originale, identica su ogni copia. La
copia si chiude **solo con un testimone nell'insieme**: un'operazione
sul suo id (la correzione, il `delete` che la risolve), o una scrittura
del corpo che la contiene (il diff3 rigoroso dalla biforcazione) o che
da lei discende. Un ritocco al titolo non chiude niente, e non crea
niente: solo il corpo confligge. Il perdente **dell'agente** nasce in
quarantena, nella coda delle proposte; il tetto tiene tre copie aperte
visibili per nota. `engramma conflicts <cella>` le elenca,
`engramma conflicts resolve <cella> <copia> --keep original|copy|merged`
le risolve. Una lapide vince sulle modifiche che non l'avevano vista;
chi l'aveva vista può scrivere e la nota si riapre. La `purge` domina
su tutto.

## Che cosa non si promette

- **L'istanizzazione completa di un dispositivo nuovo** viaggia oggi come coda di operazioni sigillate (più la prova di Merkle per l'era senza firma): per una cella da 10k note funziona ed è misurata, ma l'ottimizzazione dell'istantanea al taglio (il file compatatto) resta da fare.
- **Il libro degli indirizzi vive di sessioni**: il primo contatto passa dall'abbinamento; un demone che cambia porta si raggiunge di nuovo al prossimo invito o incontro.
- Né sicurezza dopo compromissione (una chiave d'epoca rubata apre tutta l'epoca) né segretezza in avanti per messaggio. Per i dispositivi di una sola persona, con la rotazione alla rimozione, è il compromesso giusto oggi; MLS e i suoi successori (Keyhive, DCGKA) sono la strada per la condivisione con altre persone — e ancora non sono pronti. Le scelte sono svolte nel DESIGN § 10, con le fonti.

Né sicurezza dopo compromissione (una chiave d'epoca rubata apre tutta l'epoca) né segretezza in avanti per messaggio. Per i dispositivi di una sola persona, con la rotazione alla rimozione, è il compromesso giusto oggi; MLS e i suoi successori (Keyhive, DCGKA) sono la strada per la condivisione con altre persone — e ancora non sono pronti. Le scelte sono svolute nel DESIGN § 10, con le fonti.
