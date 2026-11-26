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
- **Mixed search**: text (BM25) and **meaning** (local embeddings) fused, with
  the graph and time on top. Paraphrases with no word in common with the note
  are found; the explanation says which branch carried each result.
- **MCP**: access for Claude and other MCP clients, over stdio or HTTP with
  OAuth 2.1, scoped to the cells you choose; writes follow each cell's policy
  (proposals in quarantine, review, rollback).
- **Sync**: end-to-end encrypted synchronization between one owner's devices
  (`engramma sync`, see `docs/sync.md`).

## Status — a developer preview

**Version `0.6.17-beta.1`.** This is the first public release: a developer
preview of the CLI and of the MCP server, for anyone who wants to give their
memory to Claude or another MCP client. The engine is stable enough to trust
with real notes — the [data promise](#the-data-promise) below is written down,
line by line, each line with the test that proves it.

What is **not** stable yet: the CLI's command surface may still move, the
Node.js binding tracks its consumer's needs (see `docs/node.md`), and search
scores may improve between versions (results can change order; nothing is
lost). See `docs/DESIGN.md` § 12 for the roadmap — organizations are next
(v0.7).

## The data promise

- **Every future version opens a cell written by this one, without losses.**
  Each schema-changing release freezes a *golden cell* and every later version
  must open and verify all of them.
- **An older version refuses a cell written by a newer one, and does not touch
  it**: a readable error, and the file stays byte for byte what it was.
- **Sync refuses mismatched versions** before a single operation moves; you
  update both devices and it resumes.
- **The MCP tool names and arguments are stable** for the whole `0.6.x-beta`
  series; a breaking change is announced in the `CHANGELOG.md`.

The details, the limits (what an export is and is not), and the test behind
each line: [`docs/compatibilita.md`](docs/compatibilita.md).

## Install

### The shell installer (macOS arm64, Linux arm64/x86_64)

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/homun-app/engramma/releases/download/v0.6.17-beta.1/engramma-cli-installer.sh | sh
```

It downloads the right archive for your platform, checks its sha256, and puts
the `engramma` binary in cargo's bin directory (`$HOME/.cargo/bin`, created if
missing).

The URL names the version on purpose: GitHub's `releases/latest` link skips
prereleases, so while only `-beta` versions exist it points at nothing. Newer
versions are on the [releases page](https://github.com/homun-app/engramma/releases).

**macOS, said honestly: the binaries are not signed.** A binary installed with
the `curl` command above does **not** get the quarantine attribute, so
Gatekeeper does not block it — the terminal installer is the recommended
road. A tarball downloaded **from the browser** does get it; if you go that
way, clear it once:

```sh
xattr -d com.apple.quarantine /path/to/engramma
```

Signing and notarization need an Apple Developer certificate as a repository
secret — a decision still open; see the notes at the bottom.

### Build from source

Requires Rust 1.95 (edition 2024):

```sh
git clone https://github.com/homun-app/engramma.git
cd engramma
cargo build --release -p engramma-cli
# binary: target/release/engramma
```

`Cargo.lock` pins every dependency (add `--locked` to build exactly those);
the first build downloads them from crates.io.

### Platforms

| Platform | Status |
|---|---|
| macOS arm64 (Apple Silicon) | **shipped** — the test suite runs on it in CI; the release binary was exercised by hand (`init`, `add`, `search`, `verify`, MCP handshake) |
| Linux arm64 (glibc) | **shipped** — built on a native arm runner; test suite in CI; the binary exercised by hand in Docker |
| Linux x86_64 (glibc) | **shipped** — test suite in CI; the binary exercised by hand in Docker |
| macOS Intel | not shipped — its CI job is exploratory (non-blocking); it enters when it turns green and blocking |
| Windows | not shipped — the engine does not compile there yet (Unix-only APIs in the sync and MCP layers) |

The Linux binaries are built on Ubuntu 22.04 and need **glibc ≥ 2.35**
(Ubuntu 22.04+, Debian 12+, Fedora 36+). RHEL 9 and its rebuilds ship 2.34
and are not covered: build from source there.

## The first minute

Real output from a run (the CLI speaks English since v0.6.19; your notes
stay in your own language):

```console
$ export ENGRAMMA_ROOT=~/engramma-demo
$ engramma init
initialized: /home/you/engramma-demo
$ printf 'Fabio ha traslocato a [[Milano]] con [[Giulia]] a settembre 2026.\n' > body.md
$ engramma add body.md --source user --confidence stated \
    --title "Fabio abita a Milano" --type fact --tag casa/milano --valid-from 2026-09-01
added: 01M8CMRGZGX0EW10R1WRRW65S0
$ engramma search "traslocato"
1. [active] Fabio abita a Milano (WRRW65S0)
   score 1.0000 (raw 0.6500)
   lists: text
   terms: trasloc
   BM25 rank: 1
   support: strong
   graph weight: 0.35 (gap 0.00)
   validity: always → now
   source:
$ engramma verify
log verified: 2 operations, hash chain, reconstruction and FTS index in place
```

`[[Milano]]` and `[[Giulia]]` are entities: the graph grows with your notes.
Semantic search (paraphrases, other languages) needs a local embedding model —
`engramma model pull granite-embedding-97m-multilingual-r2`, then `engramma
embed`; without one, search is text + graph, which is already mixed. The full
command reference is [`docs/cli.md`](docs/cli.md).

More of the surface: time-aware queries (`--as-of`), the Markdown vault view
(`engramma vault`), the right to be forgotten (`engramma purge`), the review
page (`engramma review`), export/import (`engramma export`).

## Give your memory to Claude

`engramma mcp` speaks MCP over stdio and exposes **only the cells you name**
(when there is just one, tool calls may omit the `cell` argument):

```sh
engramma mcp --cells famiglia,preferenze
```

Five tools, all read-only: `memory_search`, `memory_get`, `memory_neighbors`,
`memory_tags`, `memory_timeline`. Every result carries its explanation
(matched terms, graph path, validity) and its `source`. Quarantined notes
never appear.

**Claude Desktop** — `~/Library/Application Support/Claude/claude_desktop_config.json`
(format verified against the current MCP quickstart guide; not exercised on a
live Claude Desktop for this release):

```json
{
  "mcpServers": {
    "engramma": {
      "command": "/absolute/path/to/engramma",
      "args": ["mcp", "--root", "/absolute/path/to/engramma-root", "--cells", "famiglia"]
    }
  }
}
```

With `--write <cells>` (and `--actor <name>`) the assistant may *propose*
facts through a sixth tool, `memory_propose`: proposals sit in a quarantine
queue the search never sees, until you accept or reject them (`engramma
proposals`, `engramma review`). No tool ever confirms, deletes or changes a
policy — those stay yours. See [`docs/mcp.md`](docs/mcp.md).

**Claude Code**:

```sh
claude mcp add engramma -- /absolute/path/to/engramma mcp --root ~/engramma-demo --cells famiglia
```

## Ollama or any OpenAI-compatible endpoint

Write `<root>/config.json` (see `docs/cli.md`) and run `engramma embed`:

```json
{ "embedder": { "kind": "ollama", "model": "nomic-embed-text",
                 "url": "http://127.0.0.1:11434", "dim": 768 } }
```

## Documentation

- [`docs/cli.md`](docs/cli.md) — the full command reference
- [`docs/mcp.md`](docs/mcp.md) — the MCP server (stdio and HTTP)
- [`docs/sync.md`](docs/sync.md) — end-to-end encrypted sync between devices
- [`docs/compatibilita.md`](docs/compatibilita.md) — the data promise, with
  the test that proves each line
- [`docs/DESIGN.md`](docs/DESIGN.md) § 12 — the roadmap
- [`CHANGELOG.md`](CHANGELOG.md) — what changed

## Layout

- `crates/engramma-core` — format, cell, change log, search, vector index
  (no network I/O)
- `crates/engramma-embed` — the embedders: builtin ONNX via tract (pure
  Rust), Ollama, OpenAI-compatible HTTP
- `crates/engramma-vault` — the bidirectional Markdown view (`engramma vault`)
- `crates/engramma-node` — the Node.js (N-API) binding, for in-process hosts
- `crates/engramma-cli` — the `engramma` binary
- `crates/engramma-mcp` — the MCP server
- `crates/engramma-sync` — the end-to-end encrypted sync
- `spec/` — the note format specification, with valid/invalid examples used
  as tests
- `bench/` — the benchmark (`engramma-bench`, deterministic corpus)
- `scripts/linux.sh` — the workspace checks inside the Linux arm64 Docker
  image, offline
- `dist-workspace.toml` — the release configuration (cargo-dist); the
  workflow it generates is `.github/workflows/release.yml`

## Model licences

The builtin Granite embedding models are Apache-2.0. **EmbeddingGemma** is
supported only via a user-supplied folder with its own `manifest.json`:
its Gemma licence terms do not allow bundling or auto-downloading the
weights, so it is not included and no test runs with it.

Tests that need the real models fail with a clear message when the models
are absent; CI without models sets `ENGRAMMA_SKIP_MODEL_TESTS=1`.

## License

Apache-2.0 (see `LICENSE`).

---

*Not in this release, on purpose: Apple codesigning/notarization (needs a
certificate as a repository secret — a user decision), a Homebrew tap,
crates.io publishing, Windows and macOS Intel builds. Each is tracked in the
project's phase notes.*
