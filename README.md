# Engramma

An open-source **memory engine** for assistants, written in Rust.

Engramma keeps what an assistant knows about a person, a family or a company as
**readable notes** (Markdown), tied into a **graph**, with **tags**, **time**
(validity), **sources** and **confidence** — and it answers with a **mixed
search** (text + graph + time) that **explains** every result.

- **Embedded**: one SQLite file per *cell* (a memory space). No server, no
  external services; WAL mode allows concurrent readers.
- **A change log, not a folder**: every write is one operation with a hybrid
  logical clock and a hash chain. Replaying the log rebuilds exactly the same
  state (`engramma verify` proves it).
- **Time-aware**: notes carry `valid_from`/`valid_to` (bitemporal), so
  "where did they live in August?" and "what do they eat now?" are different
  questions with different answers. Facts that changed are superseded, not
  overwritten.
- **Quarantine by default**: content scraped or inferred from third parties is
  born `quarantined` and never appears in results until a human confirms it.
- **Mixed search (v0.2)**: text (BM25) and **meaning** (local embeddings)
  fused, with the graph and time on top. Paraphrases with no word in common
  with the note are found; the explanation says which branch carried each
  result.
- **MCP**: read-only access for Claude and other MCP clients, scoped to the
  cells you choose.

**Status: v0.4 — the format is unstable.** The bidirectional Markdown view
(`engramma vault`), the right to be forgotten (`purge`) and the Node.js
binding for in-process hosts (Homun) are in; encrypted peer sync is next
(v0.5). Tested on macOS arm64 and Linux arm64 (Docker); x86_64 and Windows
are not tested yet. See `docs/DESIGN.md` for the full roadmap.

## Install from source

Requires Rust 1.95 (edition 2024):

```sh
git clone <this repository> engramma
cd engramma
cargo build --release
# binary: target/release/engramma
```

Everything is offline-friendly: the dependencies are vendored in `Cargo.lock`
and the tests never touch the network.

## A complete example

```sh
engramma init                          # creates ~/.engramma (or --root DIR)

# add a note (body from a file or stdin; source and confidence are required)
cat > body.md <<'EOF'
Fabio ha traslocato a [[Milano]] con [[Giulia]] a settembre 2026.
EOF
engramma add body.md --source user --confidence stated \
    --title "Fabio abita a Milano" --type fact --tag casa/milano --valid-from 2026-09-01

# search (Italian stemming included: "traslocato" finds "trasloco")
engramma search "traslocato"
engramma search "dove abitava Fabio" --as-of 2026-08-15   # the past
engramma search "dove abita Fabio" --json                 # stable JSON

# the graph
engramma neighbors Giulia
engramma path Fabio "Studio Rossi"
engramma timeline Fabio
engramma tags casa

# semantic search (v0.2): download the model once
engramma model pull granite-embedding-97m-multilingual-r2
engramma model use granite-embedding-97m-multilingual-r2   # blue/green + config
engramma embed                                            # fill the vectors
engramma search "in che città si è trasferita la coppia"   # no shared words
engramma search "in che città si è trasferita la coppia" --mode text   # text only
engramma search "Where does Fabio live?"                  # across languages

# provenance, versions, verification
engramma get <id>            # id prefix works too
engramma history <id>
engramma verify              # hash chain + full replay

# folders
engramma export out/                      # Engramma folder (lossless)
engramma import out/                      # back into a cell, same state
engramma import ~/obsidian-vault --obsidian
```

A quarantined note (from the web, unconfirmed):

```sh
engramma add rumor.md --source import:web --confidence external   # quarantined
engramma search "the rumor topic"      # it never appears
engramma confirm <id>                  # a human vouches for it
```

## MCP (read-only)

`engramma mcp` speaks MCP over stdio and exposes only the cells you name
(when there is just one, tool calls may omit the `cell` argument):

```sh
engramma mcp --cells famiglia,preferenze
```

Tools: `memory_search`, `memory_get`, `memory_neighbors`, `memory_tags`,
`memory_timeline`. Every result carries its explanation (matched terms, graph
path, validity) and its `source`. Quarantined notes never appear.

**Claude Desktop** — `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "engramma": {
      "command": "/path/to/engramma",
      "args": ["mcp", "--root", "/Users/you/.engramma", "--cells", "famiglia"]
    }
  }
}
```

### Ollama or any OpenAI-compatible endpoint

Write `<root>/config.json` (see `docs/cli.md`) and run `engramma embed`:

```json
{ "embedder": { "kind": "ollama", "model": "nomic-embed-text",
                 "url": "http://127.0.0.1:11434", "dim": 768 } }
```

Without a configuration the search stays text + graph, exactly as in v0.1.

**Claude Code**:

```sh
claude mcp add engramma -- /path/to/engramma mcp --root ~/.engramma --cells famiglia
```

## Layout

- `crates/engramma-core` — format, cell, change log, search, vector index
  (no network I/O)
- `crates/engramma-embed` — the embedders: builtin ONNX via tract (pure
  Rust), Ollama, OpenAI-compatible HTTP
- `crates/engramma-vault` — the bidirectional Markdown view (`engramma vault`)
- `crates/engramma-node` — the Node.js (N-API) binding, for Homun and any
  Node/Electron host (`docs/node.md`); its Rust allocations go through
  mimalloc on macOS (v0.4.4; per-platform since v0.4.5)
- `crates/engramma-cli` — the `engramma` binary
- `crates/engramma-mcp` — the MCP server (stdio, read-only)
- `spec/` — the note format specification, with valid/invalid examples used
  as tests
- `bench/` — the benchmark (`engramma-bench`, deterministic corpus) and the
  LongMemEval example (`bench/examples/longmemeval.rs`)
- `scripts/linux.sh` — the workspace checks inside the Linux arm64 Docker
  image, offline
- `docs/` — `DESIGN.md` (the project document), `cli.md`, `architecture.md`,
  `node.md`

## Model licences

The builtin Granite embedding models are Apache-2.0. **EmbeddingGemma** is
supported only via a user-supplied folder with its own `manifest.json`:
its Gemma licence terms do not allow bundling or auto-downloading the
weights, so it is not included and no test runs with it.

Tests that need the real models fail with a clear message when the models
are absent; CI without models sets `ENGRAMMA_SKIP_MODEL_TESTS=1`.

## License

Apache-2.0 (see `LICENSE`).


## v0.3: la vista Obsidian, il diritto all'oblio

- **`engramma vault init <cartella>`** esporta la cella in markdown e
  lega ogni nota al suo file dall'`id` nell'intestazione; da lì in poi
  **`engramma vault watch`** riconcilia ciò che cambiate a mano (anche
  da Obsidian) con ciò che cambia la cella, con le copie di conflitto e
  la guardia contro le cancellazioni di massa. Rinominare o spostare un
  file non perde la nota.
- **`engramma purge <id> --reason …`** dimentica una nota sul serio:
  contenido, versioni, archi e i payload delle sue operazioni (registro
  v2 a digest salati); `secure_delete` + `VACUUM` non lasciano byte nel
  file. Le esportazioni fatte prima restano fuori dal nostro controllo.
- **`engramma compact`** compatta il registro con un'istantanea;
  **`engramma consolidate`** propone quasi duplicati e contraddizioni
  aperte: decide l'applicazione, con l'utente.
