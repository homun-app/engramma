// SPDX-License-Identifier: Apache-2.0

//! Cell schema and numbered migrations (DESIGN §4.2, without `notes_vec`).
//!
//! Every migration runs inside a transaction; `meta.schema_version` stores the
//! number of applied migrations. Since v3 every FTS column has a twin column
//! in `notes` (`tags_text`), so the index is verified with an integrity-check
//! instead of merely deduced from the replay.

/// Current schema version (number of entries in [`MIGRATIONS`]).
pub const SCHEMA_VERSION: i64 = 22;

/// Ordered migrations; `MIGRATIONS[i]` brings a cell to version i + 1.
/// (`meta` is created by the bootstrap in [`migrate`], not by a migration.)
pub const MIGRATIONS: [&str; 22] = [
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
    Ok(())
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
