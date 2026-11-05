# `engramma` CLI reference (v0.1)

Common options (global):

| Option | Meaning |
|---|---|
| `--root DIR` | data root (default `~/.engramma`, or `$ENGRAMMA_ROOT`); cells live in `<root>/cells/<name>.sqlite` |
| `--cell NAME` | cell to operate on (default `default`); names are `[a-z0-9_-]+` |
| `--json` | stable JSON output (schemas below) |

**Exit codes**: `0` success, `2` usage error, `3` note not found, `4`
internal error. Note ids accept a unique prefix (≥ 4 characters).

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
must hold exactly the non-tombstoned notes). JSON: `{ops, hash_chain_ok,
replay_ok, fts_ok, mismatch, ok}`.

### `mcp --cells a,b`
Starts the read-only MCP server on stdio. Without `--cells` it refuses to
start (no cell would be exposed).
