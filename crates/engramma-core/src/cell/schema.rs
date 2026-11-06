// SPDX-License-Identifier: Apache-2.0

//! Cell schema and numbered migrations (DESIGN §4.2, without `notes_vec`).
//!
//! Every migration runs inside a transaction; `meta.schema_version` stores the
//! number of applied migrations. Since v3 every FTS column has a twin column
//! in `notes` (`tags_text`), so the index is verified with an integrity-check
//! instead of merely deduced from the replay.

/// Current schema version (number of entries in [`MIGRATIONS`]).
pub const SCHEMA_VERSION: i64 = 4;

/// Ordered migrations; `MIGRATIONS[i]` brings a cell to version `i + 1`.
/// (`meta` is created by the bootstrap in [`migrate`], not by a migration.)
pub const MIGRATIONS: [&str; 4] = [
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
