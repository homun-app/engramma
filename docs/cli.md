# `engramma` CLI reference (v0.1 + v0.2)

Common options (global):

| Option | Meaning |
|---|---|
| `--root DIR` | data root (default `~/.engramma`, or `$ENGRAMMA_ROOT`); cells live in `<root>/cells/<name>.sqlite` |
| `--cell NAME` | cell to operate on (default `default`); names are `[a-z0-9_-]+` |
| `--json` | stable JSON output (schemas below) |

**Exit codes**: `0` success, `2` usage error, `3` note not found, `4`
internal error. Note ids accept a unique prefix or suffix (≥ 4 characters).
Short ids shown by the CLI and used in export file names are the **last 8**
characters of the ULID — its random part: the first 8 are a timestamp and
collide in bulk imports.

## Commands

### `init`
Creates `<root>/cells/` and the `default` cell.
JSON: `{"root": "<path>"}`.

### `cell list|create|drop`
`list` JSON: `{"cells": ["default", ...]}`; `create`/`drop` JSON: `{"ok": "<name>"}`.

### `add FILE`
`FILE` is the body Markdown (`-` = stdin). Required: `--source SRC`,
`--confidence stated|inferred|external`. Options: `--type` (default `fact`),
`--title`, `--tag` (repeatable), `--valid-from DATE`, `--lang it|en|fr|de|es|pt`.
An `external` note is created `quarantined`.
JSON: `{"id": "<ulid>"}`.

### `get ID`
Human: the canonical note file (quarantined and deleted notes get a
`⚠ stato: …` warning line first — the CLI may show them, the MCP server may
not). JSON: the complete note object:
`{id, type, title, body, tags, lang, source, confidence, status, valid_from,
valid_to, supersedes, created_at, updated_at, x}`.

### `edit ID`
Opens the note in `$EDITOR`; on save the difference is applied as a
`patch_note` operation. The `id` and `created_at` cannot change. Clearing the
`lang` field is not expressible in v0.1 (omit it instead). JSON:
`{"id": ..., "patched": true}` or nothing changed → exit 0.

### `rm ID` / `confirm ID`
`rm` is a logical delete (tombstone; `verify` and the log keep it).
`confirm` promotes a quarantined note to active, recording the actor.
JSON: `{"ok": "<id>"}`.

### `supersede OLD FILE`
Closes `OLD` (validity end = the new note's `valid_from`, or today) and adds
the new note from `FILE`. Same required flags as `add`. JSON:
`{"closed": OLD, "new": NEW}`.

### `search TEXT`
Options: `--k` (default 10), `--hops 0-2` (default 1), `--as-of YYYY-MM-DD`,
`--tag` (repeatable, children included), `--type` (repeatable),
`--include-quarantined`, `--lang`.
JSON:

```json
{ "query": "...",
  "hits": [
    { "score": 0.94,
      "explain": { "matched_terms": ["trasloc"], "bm25_rank": 1,
                    "via": "[[Fabio]] -lives_in-> [[Milano]]",
                    "confidence": "stated", "valid_from": "2026-09-01",
                    "valid_to": null, "source": "user" },
      "note": { "id": "...", "type": "fact", "title": "...", "body": "...",
                 "tags": [], "source": "user", "confidence": "stated",
                 "status": "active", "valid_from": "2026-09-01",
                 "valid_to": null, "created_at": "...", "updated_at": "..." } }
  ] }
```

Human output prints score, matched terms, BM25 rank, graph path, validity and
source for each hit. Quarantined and tombstoned notes never appear; superseded
notes appear only for `--as_of` questions they answer.

### `neighbors ENTITY [--depth N] [--rel R ...] [--include-quarantined]`
Edges declared by quarantined or deleted notes are excluded (manual `link`
edges always stay); `--include-quarantined` brings the quarantined ones back.
JSON: `{entity, neighbors: [{depth, entity, rel, direction, note_id,
valid_from, valid_to}]}`.

### `path A B [--include-quarantined]`
Shortest graph path (≤ 4 steps), same edge visibility rules as `neighbors`.
JSON: `{from, to, steps: [{from, rel, to}]}` or `steps: null`.

### `tags [PREFIX] [--include-quarantined]` / `entities [PREFIX]`
A tag prefix includes its children; quarantined and deleted notes do not count
unless `--include-quarantined` (deleted never). JSON: `{tags: [{name, parent,
note_count}]}` / `{entities: [{name, id, note_id}]}`.

### `timeline ENTITY [--include-quarantined]`
Notes about an entity ordered by validity (oldest first), with each note's
explain fields.

### `history ID` / `restore ID VERSION`
`history` JSON: `{id, versions: [{version, op_id, note}]}`. `restore` is
itself an operation in the log. JSON: `{"ok": "<id>"}`.

### `import DIR [--obsidian]`
Engramma folder (with `engramma.json`) or, with `--obsidian`, any Markdown
vault: hidden entries (`.obsidian/`, `.trash/`) and non-Markdown files are
skipped, symlinks never followed, notes without an Engramma header get one
(`source: import:obsidian/<relative path>`). JSON:
`{imported, skipped: [..], errors: [..], warnings: [..]}`.

### `export DIR`
Writes `<dir>/<type>/<slug>-<id8>.md` plus `engramma.json`. Tombstones are
never exported; superseded notes are.

### `verify`
Recomputes the hash chain, rebuilds the whole state by replaying the log in
memory, compares every table and runs the FTS integrity-check (the index
must hold exactly the non-tombstoned notes). After a compaction the `replay`
field (and a human line) says what was actually checked: `full` (no
compaction), `from_snapshot` (the tail replays on the snapshot file) or
`chain_only` — a checkpoint without a file, where verify checks the whole
chain, every surviving payload's digest, the derived indexes (the `d…`
edges re-derived from the notes, no vector for a note that no longer lives)
and the state digest while the head has not moved past the checkpoint.
JSON: `{ops, replay, hash_chain_ok, replay_ok, fts_ok, mismatch, coverage,
ok}` — `coverage` lists the vector coverage per model (`{model, state,
covered, living}`; embeddings are derived data, their absence is never a
verification failure). The human output prints one `copertura vettori` line
per model.

### `search TEXT` (v0.2)
Options: `--mode text|vector|hybrid` (default: `hybrid` when the cell has a
`ready` embedding model matching the configured embedder, else `text`),
`--fusion rrf|convex-0.3|convex-0.5|convex-0.7` (a `convex-mm-`/`convex-th-`
prefix picks the cosine normalisation: min-max over the candidates, or the
theoretical −1..1 limits),
`--tau` (abstention floor for vector-only results, default 0.30),
`--strict` (drop `support: "weak"` results: vector-only with a thin
cosine margin — near in meaning, unconfirmed by any discriminative text
match), plus the v0.1 options (`--k`, `--hops`, `--as-of`, `--tag`,
`--type`, `--include-quarantined`, `--lang`).

JSON adds `mode`, `embed_ms`, `search_ms`, and per hit: `raw_score` (before
the display normalisation — the top hit is not always "1.0 of something")
and the new explain fields (`support` is `"strong"`/`"weak"`):

```json
{ "query": "...", "mode": "hybrid", "embed_ms": 8.2, "search_ms": 3.1,
  "hits": [
    { "score": 1.0, "raw_score": 0.612,
      "explain": { "matched_terms": ["milan"], "bm25_rank": 1,
                    "semantic_rank": 2, "cosine": 0.8268,
                    "fused_score": 0.93, "lists": ["text", "vector"],
                    "winning_chunk": 0, "via": null,
                    "confidence": "stated", "valid_from": "2026-09-01",
                    "valid_to": null, "source": "user" },
      "note": { ... } } ] }
```

Human output prints lists, ranks, cosine and the winning chunk.

### `model list|pull ID|use ID` (v0.2)
`list` shows the known models (builtin + user folders with their own
`manifest.json`), their size, whether they are downloaded, and their state
in this cell (`building|ready|retired|—`). `pull` downloads the pinned
revision and verifies each file's sha256 before renaming it into place (a
failed check leaves no partial file). `use` registers the model in the
cell as `building` — the blue/green switch — and writes `<root>/config.json`
for the builtin kind. Remote embedders are configured by writing the file
by hand:

```json
{ "embedder": { "kind": "ollama", "model": "nomic-embed-text",
                 "url": "http://127.0.0.1:11434", "dim": 768 } }
{ "embedder": { "kind": "http", "model": "text-embedding-3-small",
                 "url": "https://api.example.com", "token_env": "MY_TOKEN",
                 "dim": 1536 } }
```

JSON `list`: `{model_root, models: [{id, model_id, dim, max_tokens, bytes,
downloaded, state}]}`; `use`: `{model, state: "building", config}`.

### `embed [--all]` (v0.2)
Embeds the pending notes with the configured embedder, in batches, and
promotes a `building` model to `ready` when its coverage reaches 100% (the
old model retires and its vectors are deleted, in one transaction).
`--all` re-embeds every living note. JSON: `{embedded, pending, promoted?}`.
`add` and `import` run the pass automatically (`--no-embed` skips it: the
notes stay pending).

### `mcp --cells a,b`
Starts the read-only MCP server on stdio. Without `--cells` it refuses to
start (no cell would be exposed). `memory_search` joins the vector branch
automatically when the cell has a `ready` model matching the embedder in
`<root>/config.json`; without a configuration every search is the v0.1
text+graph one.

### Models: where they live, and licences

`ENGRAMMA_MODEL_DIR` overrides the model root (default
`~/.cache/engramma/models`). The builtin Granite models are Apache-2.0.
EmbeddingGemma is supported only as a user folder with its own manifest
(its Gemma licence forbids bundling or auto-downloading the weights); no
test runs with it.

## v0.3 commands

### `engramma purge <id> --reason <why> [--yes]`

The right to be forgotten: the note, its versions, the edges it declares,
its vectors, its FTS row, the entities left with no references and the
tags no note carries anymore — all gone, plus the **payloads of its
operations** (log v2: the chain covers salted digests, not the bodies).
`secure_delete` zeroes the freed pages, the FTS `optimize` merges the
segments (the **stems** leave the index too) and a `VACUUM` rebuilds the
file: no byte — and no root — of the body survives in the cell. The
purge's own payload keeps only the id and the reason, never content.
`[[Mentions]]` of the note in OTHER notes stay (they are other notes);
the markdown view deletes the note's file at the next `vault sync`.
**Exports made before the purge are outside the engine's control.**

The WAL is truncated after the vacuum (`wal_checkpoint(TRUNCATE)`). When
another process holds a read transaction open, the truncation cannot
happen: the purge says so and exits with code **5** (`scrubbed: false`,
`pending: ["wal"]`); the checkpoint is retried on the next write and at
cell close, and `--json` carries the same fields. A failed `VACUUM` or
PRAGMA is an error, never a silent success.

### `engramma compact [--keep-last N] [--keep-snapshot]`

Registers a **checkpoint** and forgets the payloads up to `head − N`
(default `keep-last` 1000): the row records the head hash and the state
digest of the logical state at the head, **without a file** — the live
cell already is the durable state, and a second copy of it (the v0.3.1
snapshot) only served the full audit. The skeletons and the hash chain
stay whole; the last `keep-last` operations stay replayable;
`changes_since` below the cut answers `Compacted`. `verify` says what it
could check (`rigioco: solo catena da seq N (compattata senza
istantanea)` in `replay`): the whole chain, every surviving payload's
digest, the derived indexes — but no from-zero replay is possible.

With `--keep-snapshot` the v0.3.1 behavior: `VACUUM INTO
<cella>-snap-<seq>.sqlite`, stripped to the **logical state** (payloads
up to the cut, vectors, model registry, vault bindings and FTS content
go). Only the **last** row stays, with or without a file: the latest
compaction replaces the previous one, and a compaction without the flag
deletes the file of the previous one. A later `purge` scrubs a snapshot
file (its row records the redacted digest with `redacted_by` naming the
purge, spec §6.4).

`--json` carries `cut_seq`, `head_seq`, `head_hash`, `state_digest`,
`snapshot` (null for a checkpoint), `bytes_before`/`bytes_after` — the
latter measured over cell + WAL + snapshot files.

### `engramma vault init <dir> | sync [dir] [--allow-deletions] | watch [dir]`

The markdown view: `init` exports the cell into the folder; `sync` runs
one scan-based reconciliation and prints the report (new notes from
files, notes updated from files, files rewritten from the cell,
conflicts — the cell wins and the file's version becomes a
`<nome>.conflitto-<ts>.md` copy — deletions, moves); `watch` reconciles
on filesystem events (500 ms debounce) and every 60 s, Ctrl-C exits. A
file is bound to its note by the `id` in the header, never by its path:
renaming or moving a file moves the note's binding. More than 20 files
(or 10% of the notes) vanishing at once stops the sync with an error —
an unmounted folder — unless `--allow-deletions`.

### `engramma consolidate [--limit N] [--json]`

Near duplicates (cosine ≥ 0.95 on the ready model AND entity Jaccard
≥ 0.5) and probable contradictions (same subject, same functional
relation — `lives_in`, `works_at`, `partner_of` — two destinations,
overlapping validity). Proposals only: the engine never merges. Not
exposed over MCP.
