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
- **MCP**: read-only access for Claude and other MCP clients, scoped to the
  cells you choose.

**Status: v0.1 — the format is unstable.** Text + graph + time search is in;
semantic (embedding) search arrives in v0.2, the bidirectional Markdown view
in v0.3. See `docs/DESIGN.md` for the full roadmap.

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

**Claude Code**:

```sh
claude mcp add engramma -- /path/to/engramma mcp --root ~/.engramma --cells famiglia
```

## Layout

- `crates/engramma-core` — format, cell, change log, search (no network I/O)
- `crates/engramma-cli` — the `engramma` binary
- `crates/engramma-mcp` — the MCP server (stdio, read-only)
- `spec/` — the note format specification, with valid/invalid examples used
  as tests
- `bench/` — the benchmark (`engramma-bench`, deterministic corpus)
- `docs/` — `DESIGN.md` (the project document), `cli.md`, `architecture.md`

## License

Apache-2.0 (see `LICENSE`).
