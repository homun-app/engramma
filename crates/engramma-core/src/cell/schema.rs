// SPDX-License-Identifier: Apache-2.0

//! Cell schema and numbered migrations (DESIGN §4.2, without `notes_vec`).
//!
//! Every migration runs inside a transaction; `meta.schema_version` stores the
//! number of applied migrations. Since v3 every FTS column has a twin column
//! in `notes` (`tags_text`), so the index is verified with an integrity-check
//! instead of merely deduced from the replay.

/// Current schema version (number of entries in [`MIGRATIONS`]).
pub const SCHEMA_VERSION: i64 = 24;

/// v0.6.13 (M1/M2): the tables of one note's ANCHOR — the derived rows
/// a reconstruction DELETES and the whole-object photograph restores.
/// Enumerated from the schema (the test `anchor_tables` walks
/// `sqlite_master` and holds this list honest: a new table with a note
/// column must land here, or in [`NON_ANCHOR_NOTE_TABLES`] with its
/// reason). The note row itself anchors through its version
/// photographs; the queue, the registry and the new side's `supersedes`
/// rows through `object_anchors`; the tags and the declared edges are
/// functions of the note row, rebuilt by the reindex over the anchor's
/// content.
pub const ANCHOR_NOTE_TABLES: &[&str] = &[
    "notes",         // the note row (its photograph is note_versions)
    "note_versions", // the version photographs — the frozen history (v0.6.12, L1)
    "note_tags",     // a function of the note row (the reindex rebuilds them)
    "edges",         // the DECLARED rows (id LIKE 'd%') — a function of the note row;
    // the manual rows are objects of their own (materialize_edge)
    "supersedes", // the rows where the note is the NEW side
    "proposals",  // the queue row
    "decisions",  // the registry rows of the note's proposals
];

/// v0.6.13 (M2): the tables that hold a NOTE COLUMN and are NOT part of
/// one note's anchor — each with its reason. The pair with
/// [`ANCHOR_NOTE_TABLES`] is the single hand-written list: the schema
/// test reads `sqlite_master` and fails when a table with a note column
/// is in neither, so the next new table cannot open the same hole a
/// third time.
pub const NON_ANCHOR_NOTE_TABLES: &[(&str, &str)] = &[
    (
        "ops",
        "the log itself — the source the anchor serves, never derived state",
    ),
    (
        "op_objects",
        "the log's object index — ids only, alive under any redaction",
    ),
    (
        "op_payloads",
        "the log's payload store — redacted by the cut, censored by the purge",
    ),
    (
        "entities",
        "a global derivation over the edges and the titles, re-derived by the \
         reindex, the death-clean and the purge",
    ),
    (
        "conflict_copies",
        "the conflict derivation's own memory — rebuilt by every materialization",
    ),
    (
        "note_heads",
        "the visible head — written by the conflict derivation at every pass",
    ),
    (
        "note_vectors",
        "embeddings recomputed from the notes; a tombstone deletes them",
    ),
    (
        "pending_objects",
        "LOCAL bookkeeping of this copy (what waits), never replicated state",
    ),
    (
        "object_anchors",
        "the anchor store itself — derived, written by the photographs, taken \
         by the purge with the note it belongs to",
    ),
    (
        "vault_files",
        "the markdown view's reconciliation state — it describes a folder the \
         cell does not own (outside the log, stripped from the snapshots)",
    ),
    (
        "vault_conflicts",
        "the view's `.conflitto` copies on disk — outside the log",
    ),
];

/// Ordered migrations; `MIGRATIONS[i]` brings a cell to version i + 1.
/// (`meta` is created by the bootstrap in [`migrate`], not by a migration.)
pub const MIGRATIONS: [&str; 24] = [
    r#"
CREATE TABLE notes     (id TEXT PRIMARY KEY, type TEXT, title TEXT, body TEXT,
                        frontmatter_json TEXT,
                        scope TEXT, source TEXT, confidence TEXT, status TEXT,
                        valid_from TEXT, valid_to TEXT,
                        created_at TEXT, updated_at TEXT, version INTEGER,
                        body_norm TEXT);
CREATE TABLE note_versions (note_id TEXT, version INTEGER, op_id TEXT, snapshot_json TEXT,
                            PRIMARY KEY (note_id, version));
CREATE TABLE tags      (name TEXT PRIMARY KEY, parent TEXT);
CREATE TABLE note_tags (note_id TEXT, tag TEXT, PRIMARY KEY (note_id, tag));
CREATE TABLE entities  (id TEXT PRIMARY KEY, name TEXT UNIQUE COLLATE NOCASE, type TEXT, note_id TEXT);
CREATE TABLE edges     (id TEXT PRIMARY KEY, src TEXT, rel TEXT, dst TEXT,
                        note_id TEXT,
                        valid_from TEXT, valid_to TEXT, confidence TEXT, status TEXT);
CREATE TABLE supersedes (new_id TEXT, old_id TEXT, PRIMARY KEY (new_id, old_id));
CREATE TABLE ops       (seq INTEGER PRIMARY KEY, op_id TEXT UNIQUE, hlc TEXT, actor TEXT,
                        kind TEXT, note_id TEXT, payload_json TEXT, prev_hash TEXT, hash TEXT);
CREATE VIRTUAL TABLE notes_fts USING fts5(title, body_norm, tags, content='notes', content_rowid='rowid',
                                          tokenize='unicode61 remove_diacritics 2');
CREATE INDEX idx_edges_src ON edges(src);
CREATE INDEX idx_edges_dst ON edges(dst);
CREATE INDEX idx_edges_rel ON edges(rel);
CREATE INDEX idx_notes_status_valid ON notes(status, valid_to);
CREATE INDEX idx_notes_type ON notes(type);
CREATE INDEX idx_note_tags_tag ON note_tags(tag);
"#,
    r#"
-- v2: reindexing a note deletes its derived edges by note id; without this
-- index every write scans the whole edge table (measured: the 100k-note
-- benchmark collapsed to ~190 notes/s).
CREATE INDEX idx_edges_note ON edges(note_id);
"#,
    r#"
-- v3: the FTS `tags` column gains a twin column in `notes`. Without it the
-- index could not be verified (rebuild/integrity-check would read a
-- nonexistent notes.tags): delete/insert discipline was trusted, not checked.
ALTER TABLE notes ADD COLUMN tags_text TEXT NOT NULL DEFAULT '';
DROP TABLE notes_fts;
CREATE VIRTUAL TABLE notes_fts USING fts5(title, body_norm, tags_text, content='notes', content_rowid='rowid',
                                          tokenize='unicode61 remove_diacritics 2');
"#,
    r#"
-- v4 (DESIGN §6.2): derived embedding storage. These tables are NOT part of
-- the log, the replay or the export: vectors are recomputed from the notes
-- and never enter `ops`, so the determinism of §4.3 does not depend on
-- floating point across platforms or tract versions. Every row carries the
-- model and the hashes of the text it embeds.
CREATE TABLE embedding_models (model TEXT PRIMARY KEY, dim INTEGER, state TEXT,  -- building|ready|retired
                               created_at TEXT);
CREATE TABLE note_vectors (note_id TEXT, chunk INTEGER, model TEXT, content_hash TEXT,
                           note_hash TEXT,        -- blake3(model || title\n\nbody): staleness without a tokenizer
                           vec BLOB,              -- f32 little-endian, L2-normalised
                           PRIMARY KEY (model, note_id, chunk));
CREATE INDEX idx_note_vectors_note ON note_vectors(note_id);
"#,
    r#"
-- v5 (DESIGN §4.3, log v2): forgettable payloads. The chain covers a salted
-- digest, not the payload; the body lives apart and can be deleted (purge,
-- compaction) without breaking the chain. The columns of `ops` are now the
-- envelope the hash covers. The Rust side of this migration verifies the v1
-- chain, rewrites every row with fresh salts and records the v1 head.
ALTER TABLE ops RENAME COLUMN payload_json TO payload_digest;
CREATE TABLE op_payloads (op_id TEXT PRIMARY KEY,
                          salt BLOB NOT NULL,    -- 16 bytes, generated at op creation
                          body TEXT NOT NULL);   -- canonical JSON of the payload body
"#,
    r#"
-- v6 (DESIGN §4.3): compaction snapshots. `VACUUM INTO` copies the cell at a
-- seq; the payloads up to that seq are then forgotten and `verify` replays
-- only the tail on a copy of the snapshot.
CREATE TABLE snapshots (seq INTEGER PRIMARY KEY,
                        head_hash TEXT NOT NULL,
                        state_digest TEXT NOT NULL,
                        file TEXT NOT NULL,          -- file name, next to the cell
                        created_at TEXT NOT NULL);
"#,
    r#"
-- v7 (DESIGN §4.1): the markdown view's reconciliation state — what the last
-- scan saw on disk, per note. Derived data, outside the log: it describes a
-- folder the cell does not own.
CREATE TABLE vault_files (path TEXT PRIMARY KEY,       -- relative to the vault folder
                          note_id TEXT NOT NULL,
                          file_hash TEXT NOT NULL,      -- blake3 of the file bytes
                          note_version INTEGER NOT NULL,
                          mtime INTEGER NOT NULL,
                          size INTEGER NOT NULL);
"#,
    r#"
-- v8 (v0.3.1, P0.3): a purge reaches the compaction snapshots too — they are
-- copies made before it. The row records WHY its state digest changed (the
-- op_ids of the purges applied to the file): `verify` accepts a redacted
-- digest when the tail replay, those purges included, reproduces the live
-- state (spec §6.4). The head hash stays as the historical record of where
-- the snapshot was taken; after a redaction it no longer matches the file's
-- internal head (the censored supersedes rehash it).
ALTER TABLE snapshots ADD COLUMN redacted_by TEXT NOT NULL DEFAULT '[]';
"#,
    r#"
-- v9 (v0.3.1, P1.6): one file per note. The view's export phase once wrote a
-- second file for a note imported in the same sync, and edits bounced
-- between the two copies. The Rust side of this migration drops the older
-- duplicate rows (first by rowid wins), then the unique index makes the
-- invariant structural.
DELETE FROM vault_files WHERE rowid NOT IN (
    SELECT MIN(rowid) FROM vault_files GROUP BY note_id
);
CREATE UNIQUE INDEX idx_vault_files_note ON vault_files(note_id);
"#,
    r#"
-- v10 (v0.3.2, P0.1): the compaction checkpoint. By default a compaction no
-- longer keeps a second copy of the cell: the row records the head, the
-- digest of the logical state at the head and `redacted_by`, and `file` is
-- NULL — a checkpoint without a file (the live cell IS the durable state,
-- as in rqlite, which uses the SQLite file itself as its snapshot). A file
-- snapshot stays available with `--keep-snapshot`. `state_digest` becomes
-- nullable too: a purge redacts a checkpoint's digest (it described a state
-- that contained the purged note). SQLite cannot drop a NOT NULL constraint
-- in place, hence the table rebuild.
CREATE TABLE snapshots_v10 (seq INTEGER PRIMARY KEY,
                             head_hash TEXT NOT NULL,
                             state_digest TEXT,
                             file TEXT,
                             redacted_by TEXT NOT NULL DEFAULT '[]',
                             created_at TEXT NOT NULL);
INSERT INTO snapshots_v10 (seq, head_hash, state_digest, file, redacted_by, created_at)
     SELECT seq, head_hash, state_digest, file, redacted_by, created_at FROM snapshots;
DROP TABLE snapshots;
ALTER TABLE snapshots_v10 RENAME TO snapshots;
"#,
    r#"
-- v11 (v0.3.2, P1.6): the cell remembers its views. `vault_roots`: the
-- absolute canonical path of every folder bound with `vault init`/`sync` —
-- a purge must reach them all without being told again. `vault_conflicts`:
-- every `.conflitto` copy the engine creates — the scanner ignores them, so
-- nothing else would ever clean them. Derived data, outside the log (they
-- describe folders the cell does not own), stripped from the compaction
-- snapshots like `vault_files`.
CREATE TABLE vault_roots (root TEXT PRIMARY KEY,
                          bound_at TEXT NOT NULL);
CREATE TABLE vault_conflicts (path TEXT PRIMARY KEY,  -- relative to the root
                              note_id TEXT NOT NULL,
                              created_at TEXT NOT NULL);
CREATE INDEX idx_vault_conflicts_note ON vault_conflicts(note_id);
"#,
    r#"
-- v12 (v0.4, P0.1): a view has an identity, not a path. `vault_roots` keyed
-- views by their absolute path — a moved folder looked like a view nobody
-- could reach, forever, and every purge after it exited incomplete. The
-- identity is a ULID in the view's `engramma.json`; the cell binds rows of
-- `vault_views` to it, and a sync that finds a known id at a new path
-- UPDATES the root instead of adding a view. Files and conflict copies
-- belong to a view now (their paths are relative to ITS root), so a purge
-- knows exactly which unreachable folder still holds bytes of a note — and
-- `vault unbind` can detach a view with everything it owns. The Rust side
-- of this migration mints the ids and adopts the legacy rows (see
-- `migrate_views_v12`).
CREATE TABLE vault_views (view_id TEXT PRIMARY KEY,   -- ULID, from the manifest
                          root TEXT NOT NULL,          -- canonical absolute path
                          bound_at TEXT NOT NULL,
                          last_seen_at TEXT NOT NULL);
CREATE TABLE vault_files_v12 (view_id TEXT NOT NULL,
                              path TEXT NOT NULL,     -- relative to the view root
                              note_id TEXT NOT NULL,
                              file_hash TEXT NOT NULL,
                              note_version INTEGER NOT NULL,
                              mtime INTEGER NOT NULL,
                              size INTEGER NOT NULL,
                              PRIMARY KEY (view_id, path));
CREATE UNIQUE INDEX idx_vault_files_note_v12 ON vault_files_v12(note_id);
CREATE INDEX idx_vault_files_view ON vault_files_v12(view_id);
CREATE TABLE vault_conflicts_v12 (view_id TEXT NOT NULL,
                                  path TEXT NOT NULL,
                                  note_id TEXT NOT NULL,
                                  created_at TEXT NOT NULL,
                                  PRIMARY KEY (view_id, path));
CREATE INDEX idx_vault_conflicts_note_v12 ON vault_conflicts_v12(note_id);
"#,
    r#"
-- v13 (v0.5, P1): the proposals queue and the decisions registry. Both are
-- DERIVED tables, maintained by `apply` of the new operations
-- (propose/decide/revert_decision) and therefore rebuilt by every replay —
-- the state digest covers them, so `verify` checks the new operations'
-- derived state like any other table. The proposal NOTE lives in `notes`
-- from birth, in quarantine; these rows carry the queue metadata. A purge
-- deletes both rows with the note (the reason and the reviewer metadata are
-- payload-body content: the right to be forgotten must reach them). No
-- Rust-side backfill: no older binary knows these op kinds, so no existing
-- cell can hold rows to rebuild. `state_digest` widened with these tables:
-- compaction rows recorded under the old definition are adjusted once at
-- open (see `upgrade_snapshot_digests` in cell/mod.rs).
CREATE TABLE proposals (note_id TEXT PRIMARY KEY,   -- the proposed note
                        mode TEXT NOT NULL,         -- new | supersede
                        target TEXT,                -- superseded note, when mode = supersede
                        proposer TEXT NOT NULL,     -- the propose op's actor
                        client TEXT,                -- DECLARED clientInfo: registered, never an identity
                        reason TEXT,                -- the assistant's stated motivation
                        state TEXT NOT NULL,        -- pending | accepted | rejected | reverted
                        proposed_at TEXT NOT NULL,
                        decision_op TEXT,           -- the deciding op, when decided
                        decided_by TEXT,
                        decided_at TEXT,
                        decision_reason TEXT,
                        review_json TEXT,           -- reviewer name, model, prompt hash, confidence
                        auto_failed INTEGER NOT NULL DEFAULT 0);  -- review ran and FAILED (P2)
CREATE INDEX idx_proposals_state ON proposals(state);
CREATE TABLE decisions (op_id TEXT PRIMARY KEY,     -- the decide op
                        proposal TEXT NOT NULL,     -- the proposed note
                        decision TEXT NOT NULL,     -- accept | reject
                        actor TEXT NOT NULL,
                        decided_at TEXT NOT NULL,
                        reason TEXT,
                        review_json TEXT,
                        reverted_by TEXT);          -- the revert_decision op, when cancelled
CREATE INDEX idx_decisions_proposal ON decisions(proposal);
CREATE INDEX idx_decisions_actor ON decisions(actor);
"#,
    r#"
-- v14 (v0.6, P0): the quarantined notes leave the ONE text index. FTS5
-- computes bm25's IDF over the whole index it answers from: with the hidden
-- rows inside, a quarantined note matching a question moved the IDF of its
-- terms and with it every visible score (measured by the reviewer: a second
-- result from 1e-06 to 0.4156 — a fingerprint of a hidden note, readable by
-- any client repeating the question). From here: `notes_fts` holds exactly
-- the visible notes (`active`, `superseded`), `notes_fts_hidden` exactly the
-- quarantined ones, tombstones neither. The owner's `include_quarantined`
-- (JS contract of v0.4) queries both and fuses by rank; the default view
-- answers from `notes_fts` alone, whose statistics no hidden row can bend.
-- The migration self-heals the base index too (delete-all + selective
-- insert: the discipline `reindex_note` keeps, applied once by hand).
CREATE VIRTUAL TABLE notes_fts_hidden USING fts5(title, body_norm, tags_text, content='notes', content_rowid='rowid',
                                                 tokenize='unicode61 remove_diacritics 2');
INSERT INTO notes_fts_hidden(rowid, title, body_norm, tags_text)
     SELECT rowid, COALESCE(title, ''), COALESCE(body_norm, ''), COALESCE(tags_text, '')
     FROM notes WHERE status = 'quarantined';
INSERT INTO notes_fts(notes_fts) VALUES('delete-all');
INSERT INTO notes_fts(rowid, title, body_norm, tags_text)
     SELECT rowid, COALESCE(title, ''), COALESCE(body_norm, ''), COALESCE(tags_text, '')
     FROM notes WHERE status IN ('active', 'superseded');
"#,
    r#"
-- v15 (v0.6, P1): an operation's identity and causality travel with it.
-- `content_hash` is blake3 over the canonical CONTENT {op_id, hlc, actor,
-- kind, note_id, base, payload_digest}: the same operation carries the same
-- identity on every copy, whatever each copy's local arrival chain is (the
-- `prev_hash`/`hash` pair stays what it always was — the local chain of
-- arrival, for the audit of one disk). `base` is the content_hash of the
-- last operation of the same note the AUTHOR had applied; two operations
-- with the same base are concurrent. NULL before v0.6: those operations
-- are sequential by declaration. `op_objects` maps every operation to the
-- semantic objects it touches ("note:<ulid>", "edge:<id>", "policy"):
-- written at apply time from the payload, it survives redaction (it holds
-- ids only) and answers "which operations concern this object" without
-- reading a single body — the index the per-object materialization of P1
-- and the frontier of P1.6 walk.
ALTER TABLE ops ADD COLUMN content_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE ops ADD COLUMN base TEXT;
CREATE TABLE op_objects (op_id TEXT NOT NULL,
                         object TEXT NOT NULL,
                         PRIMARY KEY (op_id, object));
CREATE INDEX idx_op_objects_object ON op_objects(object);
"#,
    r#"
-- v16 (v0.6, P1): per-object materialization bookkeeping, LOCAL to this
-- copy (like `meta.hlc` and the vault tables: never part of the replicated
-- digest). An object is PENDING while it holds stored operations its state
-- does not reflect yet (a patch that arrived before the put it leans on,
-- an operation waiting for the purge that dominates it): the next arrival
-- for that object rematerializes it from the whole set, in total order.
CREATE TABLE pending_objects (object TEXT PRIMARY KEY,
                              since_op TEXT NOT NULL);
"#,
    r#"
-- v17 (v0.6, P1): the CONFLICT COPIES. When two concurrent writes touch a
-- note's body, the total order picks a winner and every unseen write
-- survives as a derived note — active, tagged `conflitto`, linked to the
-- original — created identically on every copy without an operation of its
-- own. The table is the derivation's memory: which original, which losing
-- operation, and the note state the loser would have produced. It is
-- DERIVED (rebuilt by the original's materialization), so it never enters
-- the replicated digest: the notes rows it produces do.
CREATE TABLE conflict_copies (copy_id TEXT PRIMARY KEY,
                              original_id TEXT NOT NULL,
                              loser_op TEXT NOT NULL,
                              state_json TEXT NOT NULL);
CREATE INDEX idx_conflict_copies_original ON conflict_copies(original_id);
"#,
    r#"
-- v18 (v0.6, P1.5): the STABLE FRONTIER. A sync session ends with both
-- peers holding the same set of operations, so this copy records, per
-- device, the head it knows that device has seen. The frontier is the
-- MINIMUM over the live devices: everything at or below it is causally
-- stable, and ONLY there may a compaction forget payloads — above it
-- someone could still ask for them. LOCAL bookkeeping (like the vault
-- tables and meta.hlc): never part of the replicated state.
CREATE TABLE device_acks (device TEXT PRIMARY KEY,
                          ack_seq INTEGER NOT NULL,
                          at TEXT NOT NULL);
"#,
    r#"
-- v19 (v0.6, P2): who SIGNED an operation. `device` is the device id (the
-- signing public key, hex) and `sig` its Ed25519 signature over the
-- operation's content_hash — the cryptographic author, distinct from the
-- human-facing `actor`. NULL for everything written before the sync era
-- and outside it: those operations are covered by the `adopt` root (P1.6)
-- or never leave this machine. The chain's envelope does not cover these
-- columns: the signature IS its own proof, verified against the device
-- list, not against the local chain of arrival.
ALTER TABLE ops ADD COLUMN device TEXT;
ALTER TABLE ops ADD COLUMN sig TEXT;
"#,
    r#"
-- v20 (v0.6.1, P0): the payload's STATE, beside the identity that never
-- changes. `payload_state` says which of the three shapes the payload on
-- THIS disk is in — whole | redacted | censored — while `payload_digest`
-- (and therefore `content_hash` and `sig`) keeps, forever, the value it was
-- minted with at birth, over the ORIGINAL payload. The v0.6 censor rewrote
-- the digest and the identity of what it censored: a copy that had seen an
-- operation before a purge and a copy that saw it after held two different
-- identities for it, a `base` pointed at a hash that existed on one copy
-- only, and convergence depended on the arrival order. From v20 on the
-- censor replaces the body (deterministic salt, deterministic kept fields)
-- and marks the row: the identity travels whole, whatever each disk did to
-- the payload it holds. The index answers "does this base live here?" —
-- the question every materialization now asks of each arrival.
ALTER TABLE ops ADD COLUMN payload_state TEXT NOT NULL DEFAULT 'whole';
CREATE INDEX idx_ops_content_hash ON ops(content_hash);
"#,
    r#"
-- v21 (v0.6.4, D1): the conflict copies became DURABLE. The derivation
-- remembers WHO lost (the actor, for the class of D2 and the queue row
-- of the agent's quarantined copy) and WHEN (the loser's hlc, for the
-- cap's window: the three most recent open copies are the visible
-- ones), whether the copy is still OPEN (it closes only with a witness
-- in the set), and whether it is VISIBLE. All of it DERIVED — the
-- migration below rematerializes every derivation from the log — so it
-- never enters the replicated digest: the notes rows it produces do.
ALTER TABLE conflict_copies ADD COLUMN loser_actor TEXT NOT NULL DEFAULT '';
ALTER TABLE conflict_copies ADD COLUMN loser_hlc TEXT NOT NULL DEFAULT '';
ALTER TABLE conflict_copies ADD COLUMN open INTEGER NOT NULL DEFAULT 1;
ALTER TABLE conflict_copies ADD COLUMN visible INTEGER NOT NULL DEFAULT 1;
"#,
    r#"
-- v22 (v0.6.5, E1): the VISIBLE HEAD of a note — the operation a LOCAL
-- write leans on as its `base`. The newest operation of the note in the
-- total order is NOT always the one whose body the author had in front
-- of them: with durable conflict copies the loser stays applied (it
-- lives as a copy) while the note shows the winner, and a header patch
-- written above the loser is newer than the winner without ever seeing
-- it. The head is the newest operation that DESCENDS from the winner of
-- the body (or is the winner) — for a note closed by a lapide, the
-- lapide or whoever after her left it closed. DERIVED from the set (the
-- conflict derivation writes it at every pass), so it never enters the
-- replicated digest; the table itself is created unversioned at
-- bootstrap, before any migration runs, because the v21 rematerialization
-- of an old cell already writes heads through the new derivation. This
-- migration backfills the rows the past owes.
CREATE TABLE IF NOT EXISTS note_heads (note_id TEXT PRIMARY KEY,
                                       content_hash TEXT NOT NULL)
WITHOUT ROWID;
"#,
    r#"
-- v23 (v0.6.13, M1): the WHOLE-OBJECT ANCHOR. The compaction cut froze
-- the version photographs (v0.6.12, L1) but not the OTHER derived rows
-- of a note: `materialize_note` deleted the queue row, the registry
-- rows, the `supersedes` rows of the new side without looking at the
-- cut, and a row born at or below it never came back — an agent's
-- correction cancelled by the user turned `Active` again after a purge
-- of its target, invisible to the queue. From here every note
-- photograph (`ops::snapshot`) also photographs the derived rows of
-- the WHOLE object, under the same operation, and a reconstruction
-- restores them before replaying the tail. DERIVED (a function of the
-- set and the cut), never part of the replicated digest; the purge of
-- a note takes its rows with it (the queue metadata is payload-body
-- content). The Rust side of this migration back-fills the photographs
-- a compacted v0.6.12 cell never took (see `migrate_anchors_v23`).
CREATE TABLE object_anchors (note_id TEXT NOT NULL,
                             op_id TEXT NOT NULL,
                             anchor_json TEXT NOT NULL,
                             PRIMARY KEY (note_id, op_id));
"#,
    r#"
-- v24 (v0.6.14, N1.3 and N2.4): no table, no column — a REPAIR of what the
-- engines before this phase left in the derived state, so that the live
-- cell and its replay agree again. Two steps, in this order (the Rust
-- side is migrate_v24):
-- 1. the cut that went back. The v0.6.13 engine accepted a compaction
--    BELOW the cut already made: the registered cut said less than the
--    payloads actually gone, and the first reconstruction above it
--    replayed operations with no payload — the queue row of an accepted
--    proposal came back pending, its registry row went, and so did the
--    version photographs born between the two cuts. The record goes back
--    to the highest operation whose payload the cut removed, and every
--    object a wrong-cut reconstruction could have touched is rebuilt:
--    from the copy file when the cell has one (the state the compaction
--    itself froze), otherwise from the photographs; what neither can
--    serve is the declared corner, named in meta (cut_repair_corner) for
--    verify to report — never invented.
-- 2. the restore that wrote the state (N2). A restore used to copy the
--    WHOLE version photograph, status and confidence included: a note
--    closed without its closing row. The notes holding a replayable
--    restore in their log rematerialize under the new rule.
-- The marker of the corner is LOCAL bookkeeping, like sync_shared: it
-- never enters a digest.
"#,
];

/// Rust-side migration steps, run right after the SQL of version `v`.
fn migrate_rust(tx: &rusqlite::Transaction<'_>, v: i64) -> Result<(), crate::error::CellError> {
    if v == 3 {
        // Backfill `tags_text` deterministically (tags sorted, space-joined —
        // the same value `apply` writes), then rebuild the index from it.
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM notes")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.flatten().collect()
        };
        for id in ids {
            let mut tags: Vec<String> = {
                let mut stmt = tx.prepare("SELECT tag FROM note_tags WHERE note_id = ?1")?;
                let rows = stmt.query_map([&id], |r| r.get::<_, String>(0))?;
                rows.flatten().collect()
            };
            tags.sort();
            tx.execute(
                "UPDATE notes SET tags_text = ?2 WHERE id = ?1",
                rusqlite::params![id, tags.join(" ")],
            )?;
        }
        tx.execute("INSERT INTO notes_fts(notes_fts) VALUES('rebuild')", [])?;
    }
    if v == 5 {
        migrate_log_v2(tx)?;
    }
    if v == 12 {
        migrate_views_v12(tx)?;
    }
    if v == 15 {
        migrate_log_v15(tx)?;
    }
    if v == 20 {
        migrate_log_v20(tx)?;
    }
    if v == 21 {
        migrate_conflicts_v21(tx)?;
    }
    if v == 22 {
        migrate_heads_v22(tx)?;
    }
    if v == 23 {
        migrate_anchors_v23(tx)?;
    }
    if v == 24 {
        migrate_v24(tx)?;
    }
    Ok(())
}

/// The Rust side of migration v24 (v0.6.14): the repair of a cut that
/// went back (N1.3), then the rematerialization of the notes a `restore`
/// wrote the state of (N2.4).
fn migrate_v24(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    repair_backward_cut_v24(tx)?;
    rematerialize_restored_v24(tx)?;
    Ok(())
}

/// v0.6.14 (N1.3): the highest operation whose payload the CUT removed.
/// An operation of a note the set purged is the purge's work, at any seq,
/// and does not count; a `purge` op itself is only ever forgotten by the
/// cut.
fn real_cut_v24(tx: &rusqlite::Transaction<'_>) -> Result<i64, crate::error::CellError> {
    let real: i64 = tx.query_row(
        "SELECT COALESCE(MAX(o.seq), 0) FROM ops o
         WHERE o.payload_state = 'redacted'
           AND NOT (o.kind != 'purge'
                    AND o.note_id IN (SELECT note_id FROM ops WHERE kind = 'purge'))",
        [],
        |r| r.get(0),
    )?;
    Ok(real)
}

/// v0.6.14 (N1.3): repair a cell the v0.6.13 engine compacted with a cut
/// BELOW the one already made.
/// - the record goes back to the true cut (the payloads are gone there);
/// - every object with an operation above the WRONG cut — the only ones a
///   reconstruction under it could have touched — is rebuilt FROM THE COPY
///   FILE when the cell has one: its rows are the state the compaction
///   froze, and the operations above the file's head replay over them.
/// - without a copy file NOTHING is rebuilt. The photographs are no
///   repair here: a reconstruction under the wrong cut re-recorded them
///   over the damaged state itself (found staging the damage by hand — the
///   supersede's photograph of the proposal came back with the queue
///   `pending`, and the anchor's write-back covered it). What the
///   skeletons of the log PROVE lost — an operation whose payload the cut
///   took and whose derived row is not there — is the DECLARED CORNER: the
///   rows stay as the migration found them, the corner is written in
///   `meta.cut_repair_corner`, and `verify` reports it as a known corner.
///   Nothing is ever invented: a decision the registry no longer holds is
///   not rebuilt.
fn repair_backward_cut_v24(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    use rusqlite::OptionalExtension;
    let old = super::registered_cut(tx)?;
    if old == 0 {
        return Ok(()); // never compacted: no cut could go back
    }
    let real = real_cut_v24(tx)?;
    if real <= old {
        return Ok(()); // the cut never went back: nothing to repair
    }
    tx.execute(
        "UPDATE snapshots SET seq = ?2 WHERE seq = ?1",
        rusqlite::params![old, real],
    )?;
    // the copy file of the record, when there is one and it opens
    let (snap, file_head): (Option<rusqlite::Connection>, i64) = {
        let file: Option<String> = tx
            .query_row("SELECT file FROM snapshots WHERE seq = ?1", [real], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten();
        let dir: Option<std::path::PathBuf> = {
            let mut stmt = tx.prepare("PRAGMA database_list")?;
            let rows: Vec<(i64, String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .flatten()
                .collect();
            drop(stmt);
            rows.into_iter()
                .find(|(n, _, _)| *n == 0)
                .and_then(|(_, _, path)| {
                    std::path::PathBuf::from(path)
                        .parent()
                        .map(std::path::Path::to_path_buf)
                })
        };
        match (file, dir) {
            (Some(file), Some(dir)) => {
                let path = dir.join(file);
                match rusqlite::Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                ) {
                    Ok(conn) => {
                        let head: i64 = conn
                            .query_row("SELECT COALESCE(MAX(seq), 0) FROM ops", [], |r| r.get(0))
                            .unwrap_or(0);
                        (Some(conn), head)
                    }
                    Err(_) => (None, 0),
                }
            }
            _ => (None, 0),
        }
    };
    let objects: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT oo.object FROM op_objects oo
             JOIN ops o ON o.op_id = oo.op_id
             WHERE o.seq > ?1",
        )?;
        let rows = stmt.query_map([old], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };
    let mut corner: Vec<String> = Vec::new();
    for object in objects {
        // a purged note keeps nothing: its rows are the purge's word
        if let Some(id) = object.strip_prefix("note:")
            && let Ok(id) = ulid::Ulid::from_string(id)
            && super::material::note_purged_in_set(tx, &id)?
        {
            continue;
        }
        match &snap {
            Some(file) => super::material::rebuild_from_file(tx, &object, file, file_head)?,
            None => {
                for what in lost_rows_v24(tx, &object, real)? {
                    corner.push(format!("{object}: {what}"));
                }
            }
        }
    }
    if !corner.is_empty() {
        let marker = serde_json::json!({ "from": old, "to": real, "unserved": corner });
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('cut_repair_corner', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            [marker.to_string()],
        )?;
    }
    Ok(())
}

/// v0.6.14 (N1.3): what the SKELETONS of the log prove lost for one
/// object of a cell without a copy file. The skeletons survive the cut
/// (kind, ids, the object index); the derived rows are what a
/// reconstruction under the wrong cut took: the queue row of a proposal
/// whose `propose` the cut forgot, the registry row of a decision whose
/// `decide` the cut forgot, the `supersedes` row of a closing the cut
/// forgot, the row of a manual edge whose `link` the cut forgot and that
/// nothing closed. Each is read as «the operation is there, its payload
/// is gone, its row is not» — proof, never a guess.
fn lost_rows_v24(
    tx: &rusqlite::Transaction<'_>,
    object: &str,
    real: i64,
) -> Result<Vec<&'static str>, crate::error::CellError> {
    let mut lost: Vec<&'static str> = Vec::new();
    // the operations of the object whose payload the CUT took (a purge's
    // own removals are explained by the purged note, and skipped with it)
    let forgotten = |kind: &str| -> Result<i64, crate::error::CellError> {
        Ok(tx.query_row(
            "SELECT COUNT(*) FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.kind = ?2 AND o.payload_state = 'redacted' AND o.seq <= ?3",
            rusqlite::params![object, kind, real],
            |r| r.get(0),
        )?)
    };
    if let Some(id) = object.strip_prefix("note:") {
        let row: i64 = tx.query_row(
            "SELECT COUNT(*) FROM proposals WHERE note_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if row == 0 && forgotten("propose")? > 0 {
            lost.push("the queue row");
        }
        let orphan: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ops o
             WHERE o.note_id = ?1 AND o.kind = 'decide' AND o.payload_state = 'redacted'
               AND o.seq <= ?2
               AND NOT EXISTS (SELECT 1 FROM decisions d WHERE d.op_id = o.op_id)",
            rusqlite::params![id, real],
            |r| r.get(0),
        )?;
        if orphan > 0 {
            lost.push("the log row");
        }
        // the note as the NEW side of a supersede the cut forgot: the old
        // side is the operation's own note (the index keeps both)
        let closings: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT o.note_id FROM ops o
                 JOIN op_objects oo ON oo.op_id = o.op_id
                 WHERE oo.object = ?1 AND o.kind = 'supersede'
                   AND o.payload_state = 'redacted' AND o.seq <= ?2 AND o.note_id != ?3",
            )?;
            let rows = stmt.query_map(rusqlite::params![object, real, id], |r| {
                r.get::<_, String>(0)
            })?;
            rows.flatten().collect()
        };
        for old_id in closings {
            let purged: i64 = tx.query_row(
                "SELECT COUNT(*) FROM ops WHERE kind = 'purge' AND note_id = ?1",
                [&old_id],
                |r| r.get(0),
            )?;
            let row: i64 = tx.query_row(
                "SELECT COUNT(*) FROM supersedes WHERE new_id = ?1 AND old_id = ?2",
                rusqlite::params![id, old_id],
                |r| r.get(0),
            )?;
            if purged == 0 && row == 0 {
                lost.push("the supersedes row");
                break;
            }
        }
        // the queue row is there but a decision the cut forgot has no
        // registry row: the row's state is the wrong cut's word
        if row > 0 && orphan > 0 {
            lost.push("the queue row's state");
        }
    } else if let Some(edge_id) = object.strip_prefix("edge:") {
        // a link the cut forgot, closed by nothing above it
        let newest_is_link: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.kind = 'link' AND o.payload_state = 'redacted'
               AND o.seq <= ?2
               AND NOT EXISTS (
                   SELECT 1 FROM ops u JOIN op_objects ou ON ou.op_id = u.op_id
                   WHERE ou.object = ?1 AND u.kind = 'unlink'
                     AND (u.hlc > o.hlc OR (u.hlc = o.hlc AND u.op_id > o.op_id)))",
            rusqlite::params![object, real],
            |r| r.get(0),
        )?;
        let row: i64 =
            tx.query_row("SELECT COUNT(*) FROM edges WHERE id = ?1", [edge_id], |r| {
                r.get(0)
            })?;
        if newest_is_link > 0 && row == 0 {
            lost.push("the edge row");
        }
    }
    Ok(lost)
}

/// v0.6.14 (N2.4): the notes whose log holds a `restore` the cell can
/// still replay (above the cut, or anywhere in a cell without one) — the
/// old rule wrote the WHOLE version photograph, the new one the content
/// alone, so their derived state changes here: they rematerialize, and
/// the live cell agrees with its replay again. A restore frozen under the
/// cut has no payload left: its effect is part of the frozen state, and
/// stays as the engine of then wrote it.
fn rematerialize_restored_v24(
    tx: &rusqlite::Transaction<'_>,
) -> Result<(), crate::error::CellError> {
    let cut = super::registered_cut(tx)?;
    let objects: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT oo.object FROM op_objects oo
             JOIN ops o ON o.op_id = oo.op_id
             WHERE o.kind = 'restore' AND o.seq > ?1 AND oo.object LIKE 'note:%'",
        )?;
        let rows = stmt.query_map([cut], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };
    for object in objects {
        if let Some(id) = object.strip_prefix("note:")
            && let Ok(id) = ulid::Ulid::from_string(id)
            && super::material::note_purged_in_set(tx, &id)?
        {
            continue;
        }
        super::material::materialize(tx, &object)?;
    }
    Ok(())
}

/// The Rust side of migration v23 (v0.6.13, M1): back-fill the
/// WHOLE-OBJECT anchors a compacted cell never took. The engine of
/// v0.6.12 photographed the note row alone; a cell it compacted holds
/// no `object_anchors` rows, and the derived rows born at or below its
/// cut would still be lost by the first reconstruction. For every note
/// with operations above the cut and an anchor photograph, the derived
/// state AT THE CUT is rebuilt and stored under the same operation:
/// - the `supersedes` rows of the new side and the `decisions` rows:
///   the live rows minus the effects of the above-cut operations (their
///   payloads are whole — the effects are computable);
/// - the queue row: its birth fields are immutable (the live row holds
///   them), its state and its decision fields come from the walk of the
///   decide/revert skeletons at or below the cut and the anchor
///   photograph's word on the note's status. Where the walk cannot say
///   it with certainty (a status change at or below the cut hides the
///   flavor of the last decision), NO anchor is written and the
///   reconstruction falls back to the live rows — the declared corner,
///   no worse than the engine the cell came from.
///
/// On a cell with a snapshot FILE the file itself is the cut state (a
/// full copy taken at the compaction): its rows are read directly and
/// the walk is short. The migration is idempotent — an anchor that
/// already exists stays.
fn migrate_anchors_v23(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    use rusqlite::OptionalExtension;
    let cut = super::registered_cut(tx)?;
    if cut == 0 {
        return Ok(()); // no prefix: every reconstruction replays from zero
    }
    // the snapshot file of the cut, when there is one: its derived
    // tables ARE the state at the cut (a copy made by the compaction
    // itself, before any tail operation applied)
    let snap_file: Option<String> = tx
        .query_row("SELECT file FROM snapshots WHERE seq = ?1", [cut], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten();
    let cell_dir: Option<std::path::PathBuf> = {
        let mut stmt = tx.prepare("PRAGMA database_list")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .flatten()
            .collect();
        drop(stmt);
        rows.into_iter()
            .find(|(n, _, _)| *n == 0)
            .and_then(|(_, _, path)| {
                let p = std::path::PathBuf::from(&path);
                p.parent().map(std::path::Path::to_path_buf)
            })
    };
    let snap_conn = match (&snap_file, &cell_dir) {
        (Some(file), Some(dir)) => {
            let path = dir.join(file);
            if path.exists() {
                rusqlite::Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .ok()
            } else {
                None
            }
        }
        _ => None,
    };
    // every note with operations above the cut: the only ones whose
    // reconstruction will ever ask for an anchor
    let objects: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT oo.object FROM op_objects oo
             JOIN ops o ON o.op_id = oo.op_id
             WHERE o.seq > ?1 AND oo.object LIKE 'note:%'",
        )?;
        let rows = stmt.query_map([cut], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };
    for object in objects {
        let Some(id) = object.strip_prefix("note:") else {
            continue;
        };
        let Ok(id) = ulid::Ulid::from_string(id) else {
            continue;
        };
        // a purged note keeps nothing (and the purge of it takes the
        // rows this would write)
        let purged: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.kind = 'purge'",
            [&object],
            |r| r.get(0),
        )?;
        if purged > 0 {
            continue;
        }
        // the anchor operation: the first above-cut operation in TOTAL
        // order that photographed the note (the same walk
        // `materialize_note` runs)
        let op_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT o.op_id FROM ops o
                 JOIN op_objects oo ON oo.op_id = o.op_id
                 WHERE oo.object = ?1 AND o.seq > ?2
                 ORDER BY o.hlc ASC, o.op_id ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![object, cut], |r| r.get::<_, String>(0))?;
            rows.flatten().collect()
        };
        let mut anchor_op: Option<String> = None;
        let mut anchor_photo: Option<String> = None;
        for op_id in op_ids {
            let snap: Option<String> = tx
                .query_row(
                    "SELECT snapshot_json FROM note_versions
                     WHERE op_id = ?1 AND note_id = ?2",
                    rusqlite::params![op_id, id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            if snap.is_some() {
                anchor_op = Some(op_id);
                anchor_photo = snap;
                break;
            }
        }
        let (Some(anchor_op), Some(anchor_photo)) = (&anchor_op, &anchor_photo) else {
            continue; // no photograph above the cut: the live rows are
            // the anchor's fallback, as in v0.6.12
        };
        let have: i64 = tx.query_row(
            "SELECT COUNT(*) FROM object_anchors WHERE note_id = ?1 AND op_id = ?2",
            rusqlite::params![id.to_string(), anchor_op],
            |r| r.get(0),
        )?;
        if have > 0 {
            continue;
        }
        let anchor_status: Option<String> = serde_json::from_str::<serde_json::Value>(anchor_photo)
            .ok()
            .and_then(|v| {
                v.get("status")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        let json = match anchor_cut_state(tx, &id, cut, anchor_status, snap_conn.as_ref()) {
            Ok(Some(json)) => json,
            Ok(None) => continue, // the uncertain corner: no anchor, the
            // live rows stand in
            Err(e) => return Err(e),
        };
        tx.execute(
            "INSERT INTO object_anchors (note_id, op_id, anchor_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(note_id, op_id) DO NOTHING",
            rusqlite::params![id.to_string(), anchor_op, json],
        )?;
    }
    Ok(())
}

/// The derived state of one note at the cut, as the v23 migration
/// computes it: `None` in the declared uncertain corner. With a
/// snapshot connection the rows come from the file (the exact state at
/// the cut — a copy the compaction itself took); without one, the live
/// rows roll the above-cut effects back — in memory, for the JSON this
/// builds: the migration never writes a derived row.
fn anchor_cut_state(
    tx: &rusqlite::Transaction<'_>,
    id: &ulid::Ulid,
    cut: i64,
    anchor_status: Option<String>,
    snap: Option<&rusqlite::Connection>,
) -> Result<Option<String>, crate::error::CellError> {
    use rusqlite::OptionalExtension;
    let id_s = id.to_string();
    let proposal_json = "SELECT json_object(
                     'mode', mode, 'target', target, 'proposer', proposer, 'client', client,
                     'reason', reason, 'state', state, 'proposed_at', proposed_at,
                     'decision_op', decision_op, 'decided_by', decided_by,
                     'decided_at', decided_at, 'decision_reason', decision_reason,
                     'review_json', review_json, 'auto_failed', auto_failed)
             FROM proposals WHERE note_id = ?1";
    // --- the snapshot file speaks first: its rows ARE the cut state
    if let Some(snap) = snap {
        let tables: i64 = snap.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('proposals', 'decisions', 'supersedes')",
            [],
            |r| r.get(0),
        )?;
        if tables == 3 {
            let proposal: Option<String> = snap
                .query_row(proposal_json, [&id_s], |r| r.get(0))
                .optional()?;
            let mut stmt = snap.prepare(
                "SELECT json_array(op_id, proposal, decision, actor, decided_at, reason,
                                   review_json, reverted_by)
                 FROM decisions WHERE proposal = ?1 ORDER BY op_id",
            )?;
            let decisions: Vec<serde_json::Value> = stmt
                .query_map([&id_s], |r| r.get::<_, String>(0))?
                .flatten()
                .filter_map(|d| serde_json::from_str(&d).ok())
                .collect();
            drop(stmt);
            let mut stmt = snap.prepare(
                "SELECT json_array(new_id, old_id) FROM supersedes
                 WHERE new_id = ?1 ORDER BY old_id",
            )?;
            let supersedes: Vec<serde_json::Value> = stmt
                .query_map([&id_s], |r| r.get::<_, String>(0))?
                .flatten()
                .filter_map(|d| serde_json::from_str(&d).ok())
                .collect();
            return Ok(Some(
                serde_json::json!({
                    "proposals": proposal,
                    "decisions": decisions,
                    "supersedes": supersedes,
                })
                .to_string(),
            ));
        }
    }
    // --- no file (a checkpoint): roll the above-cut effects back over
    // the live rows. The above-cut payloads are whole, so every effect
    // is computable; the queue row's DECIDED fields of a decision at or
    // below the cut come from that decision's registry row.
    //
    // supersedes (new_id = id): the live rows minus what the above-cut
    // operations inserted, plus what their reverts deleted
    let mut cut_supersedes: Vec<String> = {
        let mut stmt = tx.prepare("SELECT old_id FROM supersedes WHERE new_id = ?1")?;
        let rows = stmt.query_map([&id_s], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };
    // decisions (proposal = id): the live rows born at or below the
    // cut, with the marks the above-cut reverts left undone (in memory)
    let mut cut_decisions: Vec<(String, Option<String>)> = {
        let mut stmt = tx.prepare(
            "SELECT d.op_id, d.reverted_by FROM decisions d
             JOIN ops o ON o.op_id = d.op_id
             WHERE d.proposal = ?1 AND o.seq <= ?2
             ORDER BY d.op_id",
        )?;
        stmt.query_map(rusqlite::params![id_s, cut], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .flatten()
        .collect()
    };
    // the above-cut operations of the note, payloads whole — found
    // through the object index: a supersede whose NEW side is this note
    // carries the OTHER note as its `note_id`
    let above: Vec<(String, String, Option<String>)> = {
        let mut stmt = tx.prepare(
            "SELECT o.op_id, o.kind, p.body FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id AND oo.object = ?3
             LEFT JOIN op_payloads p ON p.op_id = o.op_id
             WHERE o.seq > ?2 AND o.kind IN ('supersede', 'decide', 'revert_decision')",
        )?;
        stmt.query_map(rusqlite::params![id_s, cut, format!("note:{id_s}")], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .flatten()
        .collect()
    };
    let above_reverts: Vec<String> = above
        .iter()
        .filter(|(_, k, _)| k == "revert_decision")
        .map(|(op, _, _)| op.clone())
        .collect();
    for (_, reverted_by) in cut_decisions.iter_mut() {
        if let Some(rv) = reverted_by.as_deref()
            && above_reverts.iter().any(|r| r == rv)
        {
            *reverted_by = None;
        }
    }
    for (op_id, kind, body) in &above {
        let body: Option<serde_json::Value> = body
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| crate::error::CellError::Db(format!("bad payload of {op_id}: {e}")))?;
        match kind.as_str() {
            "supersede" => {
                // a supersede WHOSE NEW SIDE is this note inserted
                // (id, old) — the op's own `note_id` names the old side,
                // and the censored shape keeps the ids only
                let old = body
                    .as_ref()
                    .and_then(|b| b.get("old_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        tx.query_row("SELECT note_id FROM ops WHERE op_id = ?1", [op_id], |r| {
                            r.get::<_, String>(0)
                        })
                        .optional()
                        .ok()
                        .flatten()
                    });
                if let Some(old) = old {
                    cut_supersedes.retain(|o| o != &old);
                }
            }
            "decide" => {
                if body
                    .as_ref()
                    .and_then(|b| b.get("decision"))
                    .and_then(serde_json::Value::as_str)
                    == Some("accept")
                    && let Some(t) = body
                        .as_ref()
                        .and_then(|b| b.get("target"))
                        .and_then(serde_json::Value::as_str)
                {
                    cut_supersedes.retain(|o| o != t);
                }
                cut_decisions.retain(|(op, _)| op != op_id);
            }
            "revert_decision" => {
                if let Some(t) = body
                    .as_ref()
                    .and_then(|b| b.get("restore_target"))
                    .and_then(|v| v.get("id"))
                    .and_then(serde_json::Value::as_str)
                    && !cut_supersedes.contains(&t.to_string())
                {
                    cut_supersedes.push(t.to_string());
                }
            }
            _ => {}
        }
    }
    cut_supersedes.sort();
    // the registry rows at the cut, with their full fields (the live
    // rows hold them — a ≤-cut decision's row is never rewritten,
    // except by the reverts whose marks were undone above)
    let mut decisions_json: Vec<serde_json::Value> = Vec::new();
    for (op_id, reverted_by) in &cut_decisions {
        let row: Option<String> = tx
            .query_row(
                "SELECT json_array(?1, ?2, decision, actor, decided_at, reason, review_json, ?3)
                 FROM decisions WHERE op_id = ?1",
                rusqlite::params![op_id, id_s, reverted_by],
                |r| r.get(0),
            )
            .optional()?;
        let Some(row) = row else {
            return Ok(None); // the live cell lost the row: not said honestly
        };
        decisions_json.push(
            serde_json::from_str(&row)
                .map_err(|e| crate::error::CellError::Db(format!("bad decisions row: {e}")))?,
        );
    }
    // the queue row: the walk of the decide/revert skeletons at or
    // below the cut, with the anchor photograph's word on the status
    let walk: Vec<(String, String, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT op_id, kind, seq FROM ops
             WHERE note_id = ?1 AND kind IN ('decide', 'revert_decision') AND seq <= ?2
             ORDER BY seq",
        )?;
        stmt.query_map(rusqlite::params![id_s, cut], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .flatten()
        .collect()
    };
    let queue: Option<(String, Option<String>)> = if walk.is_empty() {
        // never decided at or below the cut: the row, if any, was born
        // pending and nothing at or below the cut touched it
        Some(("pending".into(), None))
    } else {
        let (last_op, last_kind, last_seq) = walk.last().cloned().unwrap(/* not empty */);
        if last_kind == "revert_decision" {
            Some(("reverted".into(), None))
        } else {
            // the last event is a decision: its flavor from the anchor
            // photograph, when nothing at or below the cut muddied the
            // status the decision left
            let mudded: i64 = tx.query_row(
                "SELECT COUNT(*) FROM ops
                 WHERE note_id = ?1 AND seq > ?2 AND seq <= ?3
                   AND kind IN ('delete_note', 'set_status', 'put_note', 'patch_note',
                                'supersede', 'confirm', 'restore')",
                rusqlite::params![id_s, last_seq, cut],
                |r| r.get(0),
            )?;
            match (anchor_status.as_deref(), mudded) {
                (Some("active"), 0) => Some(("accepted".into(), Some(last_op))),
                (Some("deleted"), 0) => Some(("rejected".into(), Some(last_op))),
                _ => None, // the declared uncertain corner
            }
        }
    };
    // assemble: the live queue row with the walk's word on state and
    // decided fields (the standing decision's registry row holds them)
    let live_proposal: Option<String> = tx
        .query_row(proposal_json, [&id_s], |r| r.get(0))
        .optional()?;
    let proposals_value = match (&queue, &live_proposal) {
        (None, _) => serde_json::Value::Null,
        (_, None) => serde_json::Value::Null,
        (Some((state, standing)), Some(json)) => {
            let mut p: serde_json::Value = serde_json::from_str(json)
                .map_err(|e| crate::error::CellError::Db(format!("bad proposals row: {e}")))?;
            p["state"] = serde_json::Value::String(state.clone());
            match standing {
                Some(op) => {
                    let decided: Option<serde_json::Value> = tx
                        .query_row(
                            "SELECT json_object('decided_by', actor, 'decided_at', decided_at,
                                                'decision_reason', reason,
                                                'review_json', review_json)
                             FROM decisions WHERE op_id = ?1",
                            [op],
                            |r| r.get::<_, String>(0),
                        )
                        .optional()?
                        .and_then(|s| serde_json::from_str(&s).ok());
                    let Some(d) = decided else {
                        return Ok(None); // the fields cannot be said honestly
                    };
                    p["decision_op"] = serde_json::Value::String(op.clone());
                    for k in ["decided_by", "decided_at", "decision_reason", "review_json"] {
                        p[k] = d.get(k).cloned().unwrap_or(serde_json::Value::Null);
                    }
                }
                None => {
                    p["decision_op"] = serde_json::Value::Null;
                    for k in ["decided_by", "decided_at", "decision_reason", "review_json"] {
                        p[k] = serde_json::Value::Null;
                    }
                }
            }
            serde_json::Value::String(p.to_string())
        }
    };
    // an anchor that says «no queue row» while the walk found a
    // standing decision cannot be honest — the row the reconstruction
    // would restore disagrees with the note the anchor photographs
    if live_proposal.is_none() && queue.as_ref().is_some_and(|(_, s)| s.is_some()) {
        return Ok(None);
    }
    Ok(Some(
        serde_json::json!({
            "proposals": proposals_value,
            "decisions": decisions_json,
            "supersedes": cut_supersedes
                .iter()
                .map(|old| serde_json::json!([id_s, old]))
                .collect::<Vec<_>>(),
        })
        .to_string(),
    ))
}

/// The Rust side of migration v22 (v0.6.5, E1): backfill the visible
/// head of every note that holds operations. Notes the v21
/// rematerialization already replayed have their row (the derivation
/// wrote it); a note with ONE operation needs no derivation — the head
/// is that operation; the rest rematerialize and the derivation speaks.
fn migrate_heads_v22(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    let objects: Vec<(String, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT oo.object, COUNT(*) FROM op_objects oo
             WHERE oo.object LIKE 'note:%'
             GROUP BY oo.object",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (object, n) in objects {
        let Some(id) = object.strip_prefix("note:") else {
            continue;
        };
        let have: i64 = tx.query_row(
            "SELECT COUNT(*) FROM note_heads WHERE note_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if have > 0 {
            continue;
        }
        if n <= 1 {
            // one operation: it is the head, whatever it is
            let hash: Option<String> = tx
                .query_row(
                    "SELECT o.content_hash FROM ops o
                     JOIN op_objects oo ON oo.op_id = o.op_id
                     WHERE oo.object = ?1
                     ORDER BY o.hlc DESC, o.op_id DESC LIMIT 1",
                    [&object],
                    |r| r.get(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            if let Some(hash) = hash {
                tx.execute(
                    "INSERT INTO note_heads (note_id, content_hash) VALUES (?1, ?2)
                     ON CONFLICT(note_id) DO NOTHING",
                    rusqlite::params![id, hash],
                )?;
            }
        } else {
            super::material::materialize(tx, &object)?;
        }
    }
    Ok(())
}

/// The Rust side of migration v21 (v0.6.4, D1): rematerialize the
/// conflict derivations under the new rule. A copy the user made theirs
/// with edits keeps its row — it is the replay base of a note that now
/// lives on its own — with its new columns backfilled from the loser's
/// own op and `open = 0` (witness (a): an operation on the copy's id).
/// Every other row is pure derivation: it goes, state and all, and the
/// whole conflict set is re-derived from the log for every note with
/// more than one body write — the notes a fresh put ever touches alone
/// have no conflict set to derive.
fn migrate_conflicts_v21(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    // the user-edited copies keep their rows, closed, with the new
    // columns backfilled from the loser op's own log row
    tx.execute(
        "UPDATE conflict_copies SET
             loser_actor = (SELECT o.actor FROM ops o WHERE o.op_id = conflict_copies.loser_op),
             loser_hlc   = (SELECT o.hlc FROM ops o WHERE o.op_id = conflict_copies.loser_op),
             open = 0, visible = 0
         WHERE copy_id IN (SELECT object FROM op_objects
                           WHERE object LIKE 'note:%'
                             AND substr(object, 6) = copy_id)",
        [],
    )?;
    // everything else is derivation: it goes and comes back from the log
    let stale: Vec<String> = {
        let mut stmt = tx.prepare("SELECT copy_id FROM conflict_copies WHERE open = 1")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };
    for copy in stale {
        if let Ok(id) = ulid::Ulid::from_string(&copy) {
            super::material::delete_note_state(tx, &id)?;
            super::vectors::delete_note_vectors(tx, &id)?;
        }
        tx.execute("DELETE FROM conflict_copies WHERE copy_id = ?1", [&copy])?;
    }
    // re-derive: every note with more than one body write
    let originals: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT oo.object FROM op_objects oo
             JOIN ops o ON o.op_id = oo.op_id
             WHERE o.kind IN ('put_note', 'patch_note') AND oo.object LIKE 'note:%'
             GROUP BY oo.object HAVING COUNT(*) > 1",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };
    for object in originals {
        super::material::materialize(tx, &object)?;
    }
    Ok(())
}

/// The Rust side of migration v20 (v0.6.1, P0): backfill the payload state
/// of what older binaries wrote. A row with no payload is `redacted`; the
/// v0.6 censor's replacements — a supersede born with `note: null`, a
/// decide/revert carrying the censor's marker — are `censored` (their
/// digest was already rewritten by the old rule and stays as it is now:
/// stable from here on, which is the invariant that matters). Everything
/// else keeps the default: `whole`.
fn migrate_log_v20(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    tx.execute(
        "UPDATE ops SET payload_state = 'redacted'
         WHERE op_id NOT IN (SELECT op_id FROM op_payloads)",
        [],
    )?;
    tx.execute(
        "UPDATE ops SET payload_state = 'censored'
         WHERE ops.op_id IN (
             SELECT p.op_id FROM op_payloads p JOIN ops o ON o.op_id = p.op_id
             WHERE (o.kind = 'supersede' AND json_extract(p.body, '$.note') IS NULL)
                OR (o.kind IN ('decide', 'revert_decision')
                    AND json_extract(p.body, '$.censored') = 1))",
        [],
    )?;
    Ok(())
}

/// The Rust side of migration v15 (v0.6, P1): give every existing operation
/// its wire identity and its object index. `base` stays NULL — pre-v0.6
/// operations are sequential by declaration. The object set comes from the
/// columns first (the note), then the payload when it still lives (a
/// supersede names its new note, a link its edge), then the derived tables
/// (`supersedes`) for the redacted supersedes; a redacted link/unlink under
/// a compaction cut has no recoverable edge id and is left unindexed — no
/// v0.6 copy has written it, and `sync share` starts the epoch with
/// `adopt` (P1.6).
fn migrate_log_v15(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    use crate::error::CellError;
    struct Row {
        op_id: String,
        hlc: String,
        actor: String,
        kind: String,
        note_id: Option<String>,
        payload_digest: String,
    }
    let rows: Vec<Row> = {
        let mut stmt = tx.prepare(
            "SELECT op_id, hlc, actor, kind, note_id, payload_digest FROM ops ORDER BY seq",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok(Row {
                op_id: r.get(0)?,
                hlc: r.get(1)?,
                actor: r.get(2)?,
                kind: r.get(3)?,
                note_id: r.get(4)?,
                payload_digest: r.get(5)?,
            })
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    // v0.6.20: the era this migration declares sequential, written down
    // where the conflict derivation can read it — LOCAL bookkeeping (like
    // `meta.hlc`), never replicated: a copy that receives this history
    // reads the same era from its `adopt` instead.
    if !rows.is_empty() {
        tx.execute(
            "INSERT OR IGNORE INTO meta (key, value)
             SELECT 'pre_v06_seq', CAST(MAX(seq) AS TEXT) FROM ops",
            [],
        )?;
    }
    for r in &rows {
        let op_id = ulid::Ulid::from_string(&r.op_id)
            .map_err(|_| CellError::Invalid(format!("bad op_id `{}`", r.op_id)))?;
        let hlc = crate::hlc::Hlc::parse(&r.hlc)?;
        let kind = super::ops::OpKind::parse(&r.kind)?;
        let note_id = r
            .note_id
            .as_deref()
            .map(ulid::Ulid::from_string)
            .transpose()
            .map_err(|_| CellError::Invalid(format!("bad note_id in op {}", r.op_id)))?;
        let ch = super::ops::content_hash(
            &op_id,
            &hlc,
            &r.actor,
            kind,
            note_id,
            None,
            &r.payload_digest,
        );
        tx.execute(
            "UPDATE ops SET content_hash = ?2 WHERE op_id = ?1",
            rusqlite::params![r.op_id, ch],
        )?;
        // the object set: the note column, the new-note side of a supersede,
        // the edge id of a link/unlink — payload first, derived tables as
        // the fallback for the redacted ones
        let mut objects: Vec<String> = Vec::new();
        if let Some(n) = &r.note_id {
            objects.push(format!("note:{n}"));
        }
        let body: Option<serde_json::Value> = tx
            .query_row(
                "SELECT p.body FROM op_payloads p WHERE p.op_id = ?1",
                [&r.op_id],
                |row| row.get(0),
            )
            .ok()
            .and_then(|b: String| serde_json::from_str(&b).ok());
        if kind == super::ops::OpKind::Supersede {
            let new_id = body
                .as_ref()
                .and_then(|b| b.get("note"))
                .and_then(|n| n.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let fallback: Vec<String> = {
                let mut stmt =
                    tx.prepare("SELECT new_id FROM supersedes WHERE old_id = ?1 ORDER BY new_id")?;
                let mapped = stmt.query_map([&r.note_id], |row| row.get(0))?;
                mapped.collect::<Result<Vec<_>, _>>()?
            };
            if let Some(new) = new_id {
                objects.push(format!("note:{new}"));
            } else {
                for new in fallback {
                    objects.push(format!("note:{new}"));
                }
            }
        }
        if matches!(kind, super::ops::OpKind::Link | super::ops::OpKind::Unlink)
            && let Some(id) = body
                .as_ref()
                .and_then(|b| b.get("edge"))
                .and_then(|e| e.get("id"))
                .or_else(|| body.as_ref().and_then(|b| b.get("id")))
                .and_then(serde_json::Value::as_str)
        {
            objects.push(format!("edge:{id}"));
        }
        for object in objects {
            tx.execute(
                "INSERT INTO op_objects (op_id, object) VALUES (?1, ?2)
                 ON CONFLICT(op_id, object) DO NOTHING",
                rusqlite::params![r.op_id, object],
            )?;
        }
    }
    Ok(())
}

/// The Rust side of migration v12 (v0.4, P0.1): mint a ULID per registered
/// root (a v0.3.x manifest has no id to adopt — the first sync of such a
/// view finds its root and adopts the id minted here), copy the legacy
/// file and conflict rows onto the OLDEST view (the one-folder reality;
/// every other view re-records what it sees at its next sync), then drop
/// the legacy tables. A cell with files bound but no root at all (the
/// canonicalize failure edge) keeps them under the empty view id: the
/// first sync that binds their paths adopts them via the note conflict
/// clause of `vault_file_put`.
fn migrate_views_v12(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    let roots: Vec<(String, String)> = {
        let mut stmt =
            tx.prepare("SELECT root, bound_at FROM vault_roots ORDER BY bound_at, root")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut first: Option<String> = None;
    for (root, bound_at) in &roots {
        let view_id = crate::id::new_ulid()?.to_string();
        tx.execute(
            "INSERT INTO vault_views (view_id, root, bound_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?3)",
            rusqlite::params![view_id, root, bound_at],
        )?;
        if first.is_none() {
            first = Some(view_id);
        }
    }
    let owner = first.unwrap_or_default();
    tx.execute(
        "INSERT INTO vault_files_v12 (view_id, path, note_id, file_hash, note_version, mtime, size)
         SELECT ?1, path, note_id, file_hash, note_version, mtime, size FROM vault_files",
        [&owner],
    )?;
    tx.execute(
        "INSERT INTO vault_conflicts_v12 (view_id, path, note_id, created_at)
         SELECT ?1, path, note_id, created_at FROM vault_conflicts",
        [&owner],
    )?;
    tx.execute_batch(
        "DROP TABLE vault_files;
         ALTER TABLE vault_files_v12 RENAME TO vault_files;
         DROP TABLE vault_conflicts;
         ALTER TABLE vault_conflicts_v12 RENAME TO vault_conflicts;
         DROP INDEX IF EXISTS idx_vault_files_note_v12;
         DROP INDEX IF EXISTS idx_vault_conflicts_note_v12;
         CREATE UNIQUE INDEX IF NOT EXISTS idx_vault_files_note ON vault_files(note_id);
         CREATE INDEX IF NOT EXISTS idx_vault_conflicts_note ON vault_conflicts(note_id);
         DROP TABLE vault_roots;",
    )?;
    Ok(())
}

/// The Rust side of migration v5: verify the v1 chain, then rewrite every op
/// in the v2 form with fresh salts (the format of the log is declared
/// unstable in 0.x). `ops.payload_digest` still holds the v1 payload JSON
/// when this runs — the SQL migration only renamed the column.
fn migrate_log_v2(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    use super::ops;
    use crate::error::CellError;
    struct V1Row {
        seq: i64,
        op_id: String,
        hlc: String,
        actor: String,
        kind: String,
        note_id: Option<String>,
        payload: String,
        prev_hash: String,
        hash: String,
    }
    let rows: Vec<V1Row> = {
        let mut stmt = tx.prepare(
            "SELECT seq, op_id, hlc, actor, kind, note_id, payload_digest, prev_hash, hash
             FROM ops ORDER BY seq",
        )?;
        stmt.query_map([], |r| {
            Ok(V1Row {
                seq: r.get(0)?,
                op_id: r.get(1)?,
                hlc: r.get(2)?,
                actor: r.get(3)?,
                kind: r.get(4)?,
                note_id: r.get(5)?,
                payload: r.get(6)?,
                prev_hash: r.get(7)?,
                hash: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    // 1. the v1 chain must verify first: a broken log is never rewritten.
    let mut prev = ops::GENESIS_HASH.to_string();
    for r in &rows {
        let expect = ops::op_hash_v1(&prev, &r.payload);
        if r.prev_hash != prev || r.hash != expect {
            return Err(CellError::Invalid(format!(
                "the v1 log chain is broken at seq {} ({}): refusing to migrate to v2",
                r.seq, r.op_id
            )));
        }
        prev = r.hash.clone();
    }
    let v1_head = prev;
    // 2. rewrite every op: body = payload minus the envelope, fresh salt,
    //    salted digest, chain recomputed in the v2 form.
    let mut prev = ops::GENESIS_HASH.to_string();
    for r in &rows {
        let payload: serde_json::Value = serde_json::from_str(&r.payload).map_err(|e| {
            CellError::Invalid(format!("op {}: unreadable v1 payload: {e}", r.op_id))
        })?;
        let mut body = serde_json::Map::new();
        if let serde_json::Value::Object(fields) = &payload {
            for (k, v) in fields {
                if !matches!(k.as_str(), "op_id" | "hlc" | "actor" | "kind" | "note_id") {
                    body.insert(k.clone(), v.clone());
                }
            }
        }
        let salt = crate::id::random_salt()?;
        let body = serde_json::Value::Object(body);
        let digest = ops::payload_digest(&salt, &body);
        let hash = ops::op_hash(
            &prev,
            &serde_json::json!({
                "actor": r.actor,
                "hlc": r.hlc,
                "kind": r.kind,
                "note_id": r.note_id,
                "op_id": r.op_id,
                "payload_digest": digest,
            })
            .to_string(),
        );
        tx.execute(
            "INSERT INTO op_payloads (op_id, salt, body) VALUES (?1, ?2, ?3)
             ON CONFLICT(op_id) DO UPDATE SET salt = ?2, body = ?3",
            rusqlite::params![r.op_id, salt, &body.to_string()],
        )?;
        tx.execute(
            "UPDATE ops SET payload_digest = ?2, prev_hash = ?3, hash = ?4 WHERE seq = ?1",
            rusqlite::params![r.seq, digest, prev, hash],
        )?;
        prev = hash;
    }
    // 3. the bookmark: where the v1 log ended and where the v2 form begins.
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('log_v1_head', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [v1_head],
    )?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('log_v2_from_seq', '1')
         ON CONFLICT(key) DO UPDATE SET value = '1'",
        [],
    )?;
    Ok(())
}

/// Apply pending migrations inside `tx` and update `meta.schema_version`.
///
/// # Errors
/// [`CellError::Db`] on SQL failure.
pub fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);")?;
    // v0.6.5 (E1): the visible heads exist before any migration runs — a
    // migration that rematerializes notes (v21) goes through the new
    // derivation, which writes them. Unversioned and additive, like
    // `meta`: the numbered migration v22 backfills the rows of the past.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS note_heads (note_id TEXT PRIMARY KEY,
                                                content_hash TEXT NOT NULL)
         WITHOUT ROWID;",
    )?;
    let current: i64 = tx
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // v0.5 (P0): a cell a NEWER binary migrated must refuse to open here,
    // before any migration or write. The old behavior — apply nothing (the
    // list is exhausted) and keep going — silently wrote under tables the
    // binary did not know; with derived tables maintained by `apply` the
    // state would diverge from the log. Every surface opens through this
    // function: CLI, MCP, Node, vault.
    if current > SCHEMA_VERSION {
        return Err(crate::error::CellError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let v = (i + 1) as i64;
        if v > current {
            tx.execute_batch(sql)?;
            migrate_rust(tx, v)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = ?1",
                [v.to_string()],
            )?;
        }
    }
    // v0.2.1 (P0.3): per-term document frequency of the text index, read by
    // the search to drop ubiquitous terms from the FTS MATCH. A derived view
    // over `notes_fts` (never part of `ops`, the replay comparison or the
    // export); created unversioned like `meta` — idempotent and additive, so
    // existing cells gain it on their next open. It must follow the
    // migrations: on a fresh cell `notes_fts` only exists after v1. The
    // hidden twin (v0.6, P0) serves the same read on `notes_fts_hidden`.
    tx.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts_vocab USING fts5vocab(notes_fts, 'row');
         CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts_hidden_vocab USING fts5vocab(notes_fts_hidden, 'row');",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_are_idempotent() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap(/* test-only */);
        let tx = conn.transaction().unwrap(/* test-only */);
        migrate(&tx).unwrap(/* test-only */);
        migrate(&tx).unwrap(/* test-only */);
        let v: String = tx
            .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| {
                r.get(0)
            })
            .unwrap(/* test-only */);
        let v: i64 = v.parse().unwrap(/* test-only */);
        assert_eq!(v, SCHEMA_VERSION);
        tx.commit().unwrap(/* test-only */);
    }
}
