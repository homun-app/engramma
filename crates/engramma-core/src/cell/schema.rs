// SPDX-License-Identifier: Apache-2.0

//! Cell schema and numbered migrations (DESIGN §4.2, without `notes_vec`).
//!
//! Every migration runs inside a transaction; `meta.schema_version` stores the
//! number of applied migrations. Since v3 every FTS column has a twin column
//! in `notes` (`tags_text`), so the index is verified with an integrity-check
//! instead of merely deduced from the replay.

/// Current schema version (number of entries in [`MIGRATIONS`]).
pub const SCHEMA_VERSION: i64 = 13;

/// Ordered migrations; `MIGRATIONS[i]` brings a cell to version `i + 1`.
/// (`meta` is created by the bootstrap in [`migrate`], not by a migration.)
pub const MIGRATIONS: [&str; 13] = [
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
    // migrations: on a fresh cell `notes_fts` only exists after v1.
    tx.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts_vocab USING fts5vocab(notes_fts, 'row');",
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
