# Architecture (v0.1 → v0.3)

## v0.3: the in-memory graph, the forgettable log, the markdown view

```
engramma-core
├── graph/      GraphIndex: notes and entities as integer nodes, visible
│               edges as an undirected CSR adjacency, built lazily, patched
│               by this cell's own writes (an overlay over the frozen CSR),
│               rebuilt on PRAGMA data_version change. The walk is an
│               Andersen–Chung–Lang forward push (expand while residual ≥
│               ε·weighted degree; relations weigh 1, mentions 0.25; the
│               declared projection links a relation's note to its
│               endpoints — a relation row does not otherwise touch the
│               note that states it). neighbors/path run on the same index:
│               no per-level SQL.
├── consolidation/  near duplicates (cosine ≥ 0.95 AND entity Jaccard ≥ 0.5)
│               and probable contradictions (same subject, same functional
│               relation, two destinations, overlapping validity). Proposals
│               only — the application decides.
├── cell/
│   ├── ops.rs   log v2: hash = blake3(prev ‖ {op_id, hlc, actor, kind,
│   │            note_id, payload_digest}); payload_digest =
│   │            blake3(salt ‖ canonical body); the body lives in
│   │            op_payloads(op_id, salt, body) and can be FORGOTTEN
│   │            (purge, compaction) without breaking the chain. A redacted
│   │            op replays as a no-op (skeleton + HLC tick). purge:
│   │            the note, its versions, edges, vectors, FTS row, orphan
│   │            entities and tags go; the supersede payloads where the note
│   │            was the new one are CENSORED (note→null, fresh salt, tail
│   │            rehash) so the old note's closure stays replayable;
│   │            secure_delete + VACUUM leave no body byte in the file.
│   └── schema.rs  v5: op_payloads + the v1→v2 migration (v1 chain verified
│                first, bookmarked in meta); v6: snapshots; v7: vault_files;
│                v8: snapshots.redacted_by; v9: one file per note
│                (UNIQUE(note_id) on vault_files); v10: snapshots.file and
│                state_digest nullable (the checkpoint); v11: vault_roots +
│                vault_conflicts (the cell remembers its views); v12:
│                vault_views — a view's identity is the view_id ULID in its
│                engramma.json, not the path (v0.4).
├── cell/       compact(seq, opts) (v0.3.2): by default a CHECKPOINT —
│               head hash and state_digest of the logical state at the
│               head, no file (the live cell already is the durable state;
│               a second copy served only the full audit and was worth
│               almost the whole cell). keep_snapshot keeps the v0.3.1
│               file: VACUUM INTO, stripped to the LOGICAL state (payloads
│               ≤ seq, vectors, model registry, vault bindings, vault
│               roots/conflicts and the FTS content go), one row only; the
│               OLD file is removed BEFORE its row, and a failed removal
│               fails the compaction (a row-less file escapes every purge).
│               verify says what it could check (VerifyReport.replay):
│               Full | FromSnapshot { seq } | ChainOnly { since_seq } —
│               chain-only still checks the whole chain, every surviving
│               payload's digest, the derived edges re-derived from the
│               notes, that no vector outlives its note, and the state
│               digest while the head has not moved. changes_since below
│               the cut is CellError::Compacted. purge reaches the snapshot
│               files (scrub, new digest, redacted_by naming the purge ops,
│               spec §6.4) and REDACTS a checkpoint (digest → NULL: it
│               described a state that contained the purged note).
└── search/     the graph list as a THIRD list of the fusion: PPR scores of
                the notes (seeds out), scaled by the list maximum; convex
                (1−w)·fused + w·g (RRF: weighted rank reciprocal), w = 0.40
                by weighted MRR at 100k (REPORT §v0.3); explain carries the
                path (via) and the pushed-edge count. In TEXT mode the graph
                stays a support (coverage and recency discipline): measured,
                it never displaced a real match. The query LRU is
                configurable (default 4096).
engramma-vault  the markdown view: scan-based reconciliation (sync_once),
                vault_files state, conflict copies (.conflitto-<ts>.md), the
                mass-deletion guard, notify+debouncer as a HINT with a
                periodic full scan (watch).
```

### The logical state under redaction

`entities.note_id` and the raw `entities`/`tags` rows are an index; the
state comparison (and `state_digest`) uses what is derivable: entity
existence as edge endpoints ∪ subjects of living notes. A purge
re-derives the subject claim (only a living note's claim counts) and
prunes tag/entity orphans, so a replay of the redacted log equals the
live state — the property tests with random purges enforce it.


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
                (configurable, 4096/model default, hand-written), Fusion
                (RRF | convex), abstention tau.
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

### v0.2.1: the four profile fixes (P0.3)

The profiling probe (`engramma-bench --fake-embedder`, a deterministic
fake embedder — the engine without the model) attributed the 100k hybrid
latency to four causes. The fixes:

- **`FlatI8::top_int`** iterates `chunks_exact(dim).zip(inv_scales)` with
  `chunks_exact(8)` lanes zipped against the query — no per-element
  indexing — scores once per row through the precomputed reciprocal
  scale, and selects the top-m with `select_nth_unstable_by`.
- **Connection pragmas** (`configure()`): `cache_size = −65536` (64 MB of
  page cache) and `mmap_size = 256 MB`. The exact f32 recomputation reads
  candidate vectors as BLOBs straight off the pages; with SQLite's
  default ~2 MB cache every read reached `unixRead`. The mmap is
  best-effort — a VFS without mmap support refuses the pragma and the
  engine stays correct, only slower.
- **Ubiquitous terms leave the FTS MATCH** (never the coverage nor the
  explain): FTS5 evaluates `bm25()` for every matching row before the
  `LIMIT`, and a term in a fifth of the notes paid a full-corpus ranking
  pass. Document frequencies come from the `notes_fts_vocab` view
  (`fts5vocab`, created with the schema, additive and unversioned like
  `meta`). A term is dropped only when it exceeds 20% of the corpus AND
  matches ≥ 1000 documents (below that the pass is not a cost problem and
  the term is signal — on a 60-note vault a term in a third of the notes
  is not filler), and only while a term that matches something stays.
- **One read per candidate set** (`read_candidates`): ids travel in a
  temp table, one JOIN returns note + `body_norm` + `tags_text`, with the
  `status`/`type` filters in SQL; the bitemporal validity, tags, scope and
  confidence stay in `passes_filters`, where their semantics live. Text
  candidates, vector candidates (chunks of 64) and graph-discovered notes
  all go through it. The temp-table JOINs (here and in the f32
  recomputation) are pinned with `CROSS JOIN` like the graph walk: without
  it the planner drove from the status filter and probed the temp table
  once per active note — 100k probes per query, found profiling the probe.
- **Extras by score, hubs damped** (found measuring the multi-hop
  questions): an entity mentioned by hundreds of notes dragged its whole
  neighbourhood into the graph extras — thousands of "colleague" notes —
  and the cut kept the first 40 by id, a lottery. The inherited extra
  score is now divided by `1 + ln(1 + degree)` of the frontier entity, and
  the cut keeps the strongest 40 by that score.

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
