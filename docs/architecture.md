# Architecture (v0.1 + v0.2: semantic branch)

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

## v0.2: the semantic branch

```
engramma-core
├── embed/      the Embedder trait (TextKind::Query|Document, model_id, dim,
│               max_tokens, embed → L2-normalised f32) + note chunking
│               (title + "\n\n" + body, paragraphs ≤ max_tokens, one-paragraph
│               overlap) and the content hashes. NO network I/O in core.
├── cell/
│   └── vectors/  derived storage (schema v4): embedding_models
│                 (building|ready|retired) and note_vectors (f32 LE blobs with
│                 model + content_hash + note_hash). Never in ops, the replay,
│                 compare_states or the export.
├── vector/     the VectorIndex trait and FlatI8: per-vector symmetric int8
│               quantisation, integer dot over the whole matrix (8 independent
│               i32 accumulators), top-m candidates; exact f32 recomputation
│               of the candidates against note_vectors (temp table + JOIN).
└── search/     the mixed pipeline (below), the LRU of embedded queries
                (256/model, hand-written), Fusion (RRF | convex), abstention
                tau.
engramma-embed  the embedders: Builtin (ONNX via tract — same vectors as ONNX
                Runtime, pure Rust), Ollama (/api/embed), OpenAI-compatible
                HTTP (/v1/embeddings, bearer from an env var, never logged);
                ModelSpec manifests with pinned revisions and sha256; model
                pull with hash-verified temp files; <root>/config.json.
```

### Derived data

Vectors are recomputed from the notes, never part of the cell's truth:
nothing in `ops`, the replay, `verify`'s state comparison or the export.
Staleness is detected by `note_hash = blake3(model_id || title\n\nbody)`
— no tokenizer needed to know a row is stale, so `verify` reports
coverage per model without an embedder. Deleting a note removes its
vectors inside the delete op itself.

### A semantic search

```
query embedded (TextKind::Query) OUTSIDE the cell lock, through the LRU
  ↓
lock the cell
 1. ensure the index: lazy build on first search (open stays ≤ 50 ms);
    rebuilt when PRAGMA data_version changed (another process wrote);
    updated in place by this cell's own embeds/deletes
 2. BM25 (50) and vectors (50) in parallel:
    int8 scan → top-m (m = max(200, 4k)) → exact f32 of the candidates
    (temp table + JOIN) → best chunk per note → the same filters as text,
    BEFORE fusion; fewer than k survivors double m up to a full scan
 3. fusion: RRF Σ 1/(60+rank) or convex α·cos + (1−α)·bm25_minmax
    (missing from a list = 0), scaled to [0,1]
 4. abstention: vector-only candidates below tau leave before the graph
    walk (no seeding through them, no extras resurfacing)
 5. graph expansion from the fused seeds (same bounded walk as v0.1)
 6. rerank: fused × confidence × graph decay. The v0.1 recency weight is
    NOT applied here — measured (REPORT §v0.2): cosine differences are
    hundredths and a 180-day half-life shuffled them; text-only searches
    keep the full v0.1 formula
 7. explain: semantic_rank, cosine, fused_score, lists [text|vector|graph],
    winning chunk; raw score exposed next to the display normalisation
```

### Blue/green model switch

`model use <id>` registers the model as `building` (and writes
`<root>/config.json` for the builtin kind). `engramma embed` fills the
building model while the `ready` one keeps serving searches — the search
reads only the ready model, so a half-built index is never visible. At
100% coverage one transaction retires the old model (its vectors are
deleted) and promotes the new one; the in-memory index rebuilds on the
next search. Mid-switch searches answer with the old model end to end
(tested).

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
