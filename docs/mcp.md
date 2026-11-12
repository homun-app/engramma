# Engramma via MCP — leggere e proporre (v0.5)

Engramma espone la memoria a Claude Desktop, Claude Code e a ogni client
MCP su stdio; dalla v0.5.2 anche **su HTTP**, per i client che si
collegano solo a server remoti (ChatGPT, claude.ai): vedi
`docs/http.md`. Dalla v0.5 un assistente può anche **proporre** un
fatto: non scrive mai — la proposta finisce in una coda che la ricerca
non vede, finché non la decidi tu (o un revisore che hai configurato
tu).

| | stdio | HTTP (v0.5.2) |
|---|---|---|
| chi si collega | client locali (Claude Desktop, Claude Code) | client remoti (ChatGPT, claude.ai, app), via tunnel |
| comando | `engramma mcp --cells a --write a --actor x` | `engramma mcp --cells a --http 127.0.0.1:8787 --public-url https://… [--write a]` |
| chi è il proponente | `--actor` dalla riga di comando | l'etichetta del grant (`mcp:<etichetta>`, scelta con `engramma oauth approve`) |
| accesso | chi ha la tua shell è te | solo con un token OAuth approvato da te, per grant |
| celle visibili | `--cells` | `--cells` ∩ grant (il token vede solo le sue) |
| strumenti extra | — | `search`/`fetch` con `--compat chatgpt` |

## 1. Sola lettura

```
engramma mcp --cells famiglia,preferenze
```

Cinque strumenti: `memory_search`, `memory_get`, `memory_neighbors`,
`memory_tags`, `memory_timeline`. Nessuno scrive.

### Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "engramma": {
      "command": "/percorso/di/engramma",
      "args": ["mcp", "--cells", "famiglia"]
    }
  }
}
```

### Claude Code

```sh
claude mcp add engramma -- /percorso/di/engramma mcp --cells famiglia
```

## 2. In scrittura (le proposte)

```
engramma mcp --cells famiglia --write famiglia --actor claude-desktop
```

- `--write` nomina le celle che accettano proposte (un sottoinsieme di
  `--cells`) e **richiede `--actor`**: è l'identità con cui le proposte
  firmano (`mcp:claude-desktop`). Viene dalla configurazione, non da ciò
  che il client dichiara di essere.
- Lo strumento in più è uno solo: `memory_propose`. Nessuno strumento
  conferma, scarta, cancella o cambia la politica — quelle sono operazioni
  tue.
- La **politica della cella** vale comunque: `engramma policy show` dice
  com'è. Con `write: off` (il predefinito) la cella rifiuta le proposte
  anche se sta in `--write`; apri con `engramma policy set <cella>
  write=propose` (o `confirm`, o `auto` — vedi `docs/DESIGN.md` § 8.5).

Configurazione di Claude Desktop in scrittura:

```json
{
  "mcpServers": {
    "engramma": {
      "command": "/percorso/di/engramma",
      "args": ["mcp", "--cells", "famiglia", "--write", "famiglia",
               "--actor", "claude-desktop"]
    }
  }
}
```

## 3. Cosa vede l'assistente, cosa vedi tu

L'assistente chiama `memory_propose` e riceve subito:

- `status: "pending"` — la proposta è in coda, la ricerca non la vede;
- `status: "duplicate"` — la memoria ha già quel testo (normalizzato:
  maiuscole, spazi, punteggiatura); niente è stato scritto;
- `status: "accepted"` / `"rejected"` — solo se la politica è `auto`
  (un revisore ha deciso) oppure `confirm` e tu hai risposto al modulo.

Con la politica `confirm` e un client che supporta i moduli (Claude
Desktop), la proposta arriva a **te**: un modulo con il testo — modificabile
— e i tasti **accetta** / **rifiuta**. Accetti: la nota diventa attiva
(con le tue correzioni, se ne hai fatte). Rifiuti: la proposta muore con
la lapide (recuperabile). Annulli: resta in coda. La decisione è firmata
`user@mcp:claude-desktop`.

Dove decidi, se non dal modulo:

```
engramma proposals                 # la coda
engramma proposals show <id>
engramma proposals accept <id> [--edit]   # --edit passa da $EDITOR
engramma proposals reject <id> [--reason "perché"]
engramma review                    # la pagina locale (127.0.0.1)
```

## 4. Se ci ripensi

Ogni decisione — tua, del modulo o del revisore — è annullabile:

```
engramma decisions                        # chi, quando, cosa, perché
engramma revert <id-decisione>            # la proposta torna in coda
engramma revert --by reviewer:x --yes     # in blocco (prima l'anteprima)
```

Il revert non riscrive la storia: è un'operazione nuova. Se dopo la
decisione qualcuno (tu incluso) ha corretto la nota, il revert si rifiuta
e ti dice quali operazioni si mettono in mezzo.

## 5. Che cosa NON è sicurezza

Le `instructions` del server chiedono all'assistente di proporti solo fatti
stabili, di chiedere prima, di non proporre mai ciò che legge dal web. È
cortesia, non un confine. Il confine è la coda, la politica per cella e il
revert. E ricorda: **chi ha la tua shell è te** — un agente con accesso
alla tua macchina può lanciare la CLI; non facciamo finta del contrario.
