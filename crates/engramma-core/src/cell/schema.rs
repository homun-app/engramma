// SPDX-License-Identifier: Apache-2.0

//! Cell schema and numbered migrations (DESIGN §4.2, without `notes_vec`).
//!
//! Every migration runs inside a transaction; `meta.schema_version` stores the
//! number of applied migrations. The FTS table mirrors the schema exactly as
//! written in the design; the `tags` column has no twin column in `notes`
//! (external-content FTS only reads the content table during `rebuild` and
//! `integrity-check`, which we never run — see REPORT).

/// Current schema version (number of entries in [`MIGRATIONS`]).
pub const SCHEMA_VERSION: i64 = 1;

/// Ordered migrations; `MIGRATIONS[i]` brings a cell to version `i + 1`.
/// (`meta` is created by the bootstrap in [`migrate`], not by a migration.)
pub const MIGRATIONS: [&str; 1] = [r#"
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
"#];

/// Apply pending migrations inside `tx` and update `meta.schema_version`.
///
/// # Errors
/// [`CellError::Db`] on SQL failure.
pub fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), crate::error::CellError> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);",
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
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let v = (i + 1) as i64;
        if v > current {
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = ?1",
                [v.to_string()],
            )?;
        }
    }
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
