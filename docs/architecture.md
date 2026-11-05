# Architecture (v0.1)

## Modules

```
engramma-core
├── format/     the note format (spec/format-v0.md): frontmatter (saphyr),
│               canonical serialiser, body analysis (pulldown-cmark events):
│               [[links]], #tags, ## Relations
├── text/       body_norm: lowercase → hand-written Latin diacritics table →
│               Snowball stems (rust-stemmers) in the note language (whatlang)
├── id/         ULID generation (monotonic per process)
├── hlc/        hybrid logical clock: (physical ms, counter, node)
├── cell/       the SQLite cell: schema + numbered migrations (schema.rs),
│               operations and their deterministic apply() (ops.rs)
├── search/     BM25 + graph expansion + filters + weighted rerank, explain,
│               neighbors/path/tags/entities/timeline
└── port/       Engramma folder export/import, Obsidian import
engramma-cli    the `engramma` binary (clap)
engramma-mcp    read-only MCP server on stdio (rmcp)
```

## A write

```
Cell::put_note(note, actor)
  1. generate op_id (ULID) and HLC tick          ← now, before the transaction
  2. lock (process lock per cell file) + BEGIN IMMEDIATE
  3. read last (seq, hash) from ops
  4. payload_json = canonical JSON (keys sorted, no spaces)
     hash = blake3(prev_hash || payload_json)
  5. INSERT row into ops  ─┐ same transaction
  6. apply(tx, op):        ┘ deterministic: reads no clock, no randomness
       - snapshot the old note into note_versions
       - write the notes row (body_norm recomputed from body + lang)
       - reindex: FTS delete+insert with exact values, tags (with parents),
         entities (case-insensitive, subject from person/place/project/event
         titles), edges: mentions + typed relations, deterministic ids
  7. meta.hlc = op.hlc
  8. COMMIT
```

`apply()` is the whole state transition. Replay (`verify`) opens an in-memory
cell and calls `apply()` for every stored op: the tables must match row by
row, the ops table included.

## A search

```
Query { text, k, filters { types, tags (children), scope, as_of,
        min_confidence, include_quarantined, include_superseded },
        graph_hops 0–2, weights { stated 1.0, inferred 0.8, external 0.5,
        recency half-life 180 days, hop decay 0.6 } }

1. terms = query_terms(text, lang)   # stopworded (it/en), stemmed, plus an
                                     # adjacency phrase for multi-word queries
2. FTS5 BM25 (title ×10, body ×3, tags ×2) → top 50 candidates
3. RRF fusion computed (one list in v0.1; the semantic list joins in v0.2)
4. graph expansion: entities of the top 10 hits → walk valid edges up to 2
   note-hops; discovered notes keep a decaying share of their seed's score
5. filters: quarantined (unless asked), tombstones (never), superseded (only
   if they were the truth at as_of), validity window vs as_of/now (events
   never expire), types/tags/scope/min_confidence
6. rerank: score = bm25_norm × confidence × recency × graph_decay;
   zero-signal results withheld ("sopra soglia")
7. explain per hit: matched terms, BM25 rank, graph path, confidence,
   validity, source
```

## Deliberate v0.1 boundaries

- FTS is external-content (`content='notes'`) exactly as in DESIGN §4.2; the
  `tags` column has no twin in `notes`, so FTS `rebuild`/`integrity-check`
  are never used — deletes/inserts are always paired with exact values, and
  `verify` compares the logical tables instead.
- Edges derived from note bodies have deterministic ids (`d` + blake3
  prefix); manual edges (`link`) carry ULIDs, so reindexing a note never
  touches them.
- IDs, HLC and timestamps are generated when the operation is created and
  travel in the payload; `apply` is a pure function of (state, op).
