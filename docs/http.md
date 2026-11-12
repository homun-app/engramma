# Engramma su HTTP — MCP per ChatGPT e claude.ai (v0.5.2)

Dalla v0.5.2 Engramma parla MCP anche su HTTP, con OAuth 2.1: **ChatGPT,
claude.ai e le app mobili** — che si collegano solo a server remoti in
HTTPS — possono leggere la tua memoria e proporti fatti. Il motore non
cambia natura: Engramma resta un programma locale con un solo
proprietario, ascolta **solo su loopback** e non gestisce TLS. Chi rende
pubblico l'indirizzo è il tunnel che scegli tu.

```
engramma mcp --cells famiglia --http 127.0.0.1:8787 \
  --public-url https://memoria.example.net \
  [--write famiglia] [--allow-dcr] [--compat chatgpt]
```

- `--http` è l'indirizzo locale (solo loopback: tutto il resto è
  rifiutato); `--public-url` è l'indirizzo **https** che il tunnel
  pubblica, ed è obbligatorio. Da lì nascono la risorsa canonica
  (`<public-url>/mcp`) e l'issuer di OAuth.
- `--write` nomina le celle che accettano proposte, come in stdio; ma su
  HTTP **niente `--actor`**: l'attore è l'etichetta che scegli tu quando
  approvi il collegamento (`mcp:<etichetta>`).
- Nessuna modalità anonima: anche `initialize` vuole un token, perché
  gli strumenti di lettura sono già dati personali.

## 1. Il tunnel

Engramma non termina TLS. Il tunnel davanti lo fa, e questo decide chi
vede il traffico in chiaro (§ 4).

### Tailscale Funnel (consigliato)

Il TLS termina **sul tuo computer**: nemmeno Tailscale vede la memoria
in chiaro.

```sh
# una volta: abilita Funnel sulla macchina e apri HTTPS sulla porta 443
tailscale serve --bg --https=443 http://127.0.0.1:8787
# l'indirizzo è stabile: https://<lamacchina>.<tailnet>.ts.net
engramma mcp --cells famiglia --http 127.0.0.1:8787 \
  --public-url https://<lamacchina>.<tailnet>.ts.net
```

Tailscale fa arrivare l'`Host` pubblico com'è: Engramma lo accetta
(insieme ai nomi di loopback) e rifiuta ogni altro Host con 403.

### Cloudflare Tunnel (nome stabile)

```sh
cloudflared tunnel create engramma
cloudflared tunnel route dns engramma memoria.example.net
cloudflared tunnel run --url http://127.0.0.1:8787 engramma
```

Attenzione: Cloudflare **termina lui il TLS** e vede il traffico in
chiaro (le risposte degli strumenti, il testo delle note). Vale lo
stesso per ngrok.

### ngrok (provare, non tenere)

```sh
ngrok http 127.0.0.1:8787 --domain=memoria-example.ngrok.app
```

Con il piano gratuito ngrok mette una pagina d'avviso davanti alle
richieste GET che si presentano come un browser: chi legge i metadati con
un User-Agent da browser riceve HTML invece del JSON. Nella prova del
2026-11-12 claude.ai, ChatGPT e Codex sono passati lo stesso, perché i
loro backend non si presentano come browser, ma non è una garanzia. Il
dominio gratuito casuale cambia a ogni avvio (vedi sotto). Va bene per
provare, non per restare collegati.

### Perché l'URL deve essere stabile

Un tunnel «veloce» cambia indirizzo a ogni avvio. Ma l'indirizzo È
l'identità della risorsa (`resource`, `issuer`, i token): cambiarlo
significa che ogni client deve ricollegarsi e tu riapprovare. Scegli un
nome che resti.

## 2. Collegare un client

- **ChatGPT**: menu dei connettori (o «Add custom MCP server» /
  «Connettori», la voce cambia col piano e con le politiche dello
  spazio di lavoro): URL `<public-url>/mcp`. Se usi la ricerca
  approfondita o le «conoscenze», avvia il server con
  `--compat chatgpt`: gli strumenti `search`/`fetch` a schema fisso
  esistono **solo per il collegamento di ChatGPT** (il cui documento
  client è `chatgpt.com/oauth/client.json`). Claude e Codex non li
  vedono nemmeno, e chiamarli per nome risponde come uno strumento
  inesistente. Su stdio il flag non ha senso ed è un errore d'uso.
- **claude.ai**: Impostazioni → Connettori → «Custom connector», URL
  `<public-url>/mcp`.
- **Codex** (anche il lato «Work» dell'app ChatGPT): ha un elenco di
  server MCP suo, separato dai connettori della chat. Si collega come
  client nativo, con redirect su `http://127.0.0.1:<porta>`, e la pagina
  d'attesa te lo segnala. Aggiungere il server lì non lo rende visibile
  nella chat di ChatGPT, e viceversa.

Provati davvero il 2026-11-12, attraverso un tunnel:

| Client | Documento client (CIMD) | Redirect | Protocollo |
|---|---|---|---|
| claude.ai | `https://claude.ai/oauth/mcp-oauth-client-metadata` | `claude.ai/api/mcp/auth_callback` | 2026-07-28, senza sessione |
| ChatGPT | `https://chatgpt.com/oauth/client.json` | `chatgpt.com/connector_platform_oauth_redirect` | 2026-07-28 |
| Codex | `https://chatgpt.com/oauth/codex/client.json` | loopback, porta qualsiasi | a sessioni (precedente) |

Il documento di ChatGPT preferisce `private_key_jwt`, ma fra i metodi
supportati elenca anche `none`. Engramma offre solo `none` (client
pubblico con PKCE), ed è quello che ChatGPT usa. Con un client che
supporta il modulo di conferma (claude.ai), una proposta si decide subito
nella chat. Senza modulo (ChatGPT) resta in coda: `engramma proposals` o
`engramma review`.

- Poi, **sul computer dove gira Engramma**:

```
engramma oauth pending                      # chi chiede di entrare
engramma oauth approve XXXX-XXXX --label chatgpt --yes
```

Il codice `XXXX-XXXX` è quello che compare **nel tuo browser**, sulla
pagina d'attesa che il client ti ha aperto: la sicurezza sta tutta nel
far coincidere i due. L'approvazione mostra host del client e host del
redirect: leggili. `--cells` e `--scope` possono solo restringere; il
refresh vive 30 giorni (90 al massimo assoluto) e si rinnova a
rotazione.

## 3. Spegnere, revocare

- Il server si ferma con Ctrl-C (o SIGTERM): chiude le connessioni e le
  celle.
- `engramma oauth grants` elenca i collegamenti; `engramma oauth revoke
  <etichetta>` (o `--all`) li taglia: **il token muore alla richiesta
  successiva**. `engramma oauth log` è la cronologia (approvazioni,
  rifiuti, revocazioni, allarmi).

## 4. Chi vede cosa

- **Il fornitore dell'assistente** (OpenAI, Anthropic) vede ciò che il
  client legge: le risposte degli strumenti, quindi il testo delle note
  che cerca.
- **Il fornitore del tunnel**: con Tailscale Funnel vede solo TLS; con
  Cloudflare Tunnel o ngrok termina lui il TLS e vede tutto.
- **Il registro d'accesso** di Engramma (stderr, una riga per
  richiesta) porta l'ora, l'id del grant, il metodo e lo strumento:
  mai token, mai testo.
- Chi conosce l'indirizzo pubblico può *chiamare* gli endpoint, ma senza
  token non ottiene niente, e nessun token esiste finché non approvi tu
  sul computer giusto.

## 5. Difese in più (facoltative)

Le liste di IP ammessi si mettono **al tunnel**, non dentro Engramma:
per esempio Cloudflare (`160.79.104.0/21` per Anthropic; le liste
pubbliche di OpenAI). È una difesa in più, non un sostituto di OAuth.

## 6. Come funziona, in breve

`/authorize` ha un tetto globale di 30 richieste al minuto (poi una
pagina 429 con `Retry-After`) e un documento client rifiutato o
irraggiungibile non si riscarica per cinque minuti. In attesa ci stanno
al più **due richieste per client e dieci in tutto**: per svuotare la
coda in un colpo, `engramma oauth deny --all`. La revoca di un refresh
token (`/revoke`) porta con sé anche gli accessi della stessa
famiglia; revocare un accesso tocca solo lui.

Il server MCP è un *resource server* OAuth 2.1 (metadati RFC 9728) ed
Engramma stesso è il *server di autorizzazione*: client pubblici con
PKCE S256, client conosciuti tramite il loro documento CIMD (o, con
`--allow-dcr`, registrazione dinamica RFC 7591 — deprecata dalla
specifica e spenta di base). Il **codice d'autorizzazione** nasce solo
quando tu, sul computer di Engramma, digiti il codice che il tuo
browser mostra (lo schema del flusso dispositivo, RFC 8628 § 5.4). I
token sono opachi, vivono un'ora, e di ogni segreto Engramma conserva
solo il digest. `oauth.db` sta nella radice dei dati, staccato dalle
celle: non viaggia con la sincronizzazione e la revoca è immediata e
locale. Il disegno completo sta in `docs/DESIGN.md` § 8.6.
