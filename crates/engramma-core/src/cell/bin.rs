// SPDX-License-Identifier: Apache-2.0

//! v0.6.8 (H1): the bin's ONE predicate. A note is in the bin when it
//! is deleted, or when it is a conflict copy of a deleted note — one
//! definition, in ONE place: the [`BIN_VIEW_DDL`] view, created with the
//! schema and spoken by every read path. The v0.6.7 rule lived in four
//! hand-written filters, and `note_ids()`/`list()` and `get()` forgot
//! the copy (the reviewer's proof of the verification): the fifth door
//! would have forgotten the sixth. A view cannot drift from the tables
//! it reads, costs nothing on the write path, and changes no state —
//! the digest, the replay and the ALPN stay where they are.

use crate::error::CellError;

/// The view itself: the bin's membership, a function of `notes` and
/// `conflict_copies` alone. Created unversioned and additive (see
/// [`super::schema::migrate`]) — dropped and re-created at every open,
/// so a stale definition can never outlive a schema change (all tables
/// exist by then; an `ALTER … RENAME` migration revalidates every view
/// in the file, and a view is only ever valid here).
pub(crate) const BIN_VIEW_DDL: &str =
    "DROP VIEW IF EXISTS notes_in_bin;
     CREATE VIEW notes_in_bin AS
     SELECT n.id FROM notes n
     WHERE n.status = 'deleted'
        OR EXISTS (SELECT 1 FROM conflict_copies cc
                   JOIN notes o ON o.id = cc.original_id
                   WHERE cc.copy_id = n.id AND o.status = 'deleted');";

/// The read-path fragment: `alias.id` is NOT a bin's contents. Embedded
/// as a plain string so SQLite treats it as one uncorrelated subquery
/// (an ephemeral index the planner builds once per statement).
pub(crate) fn not_in_bin(note: &str) -> String {
    format!("{note}.id NOT IN (SELECT id FROM notes_in_bin)")
}

/// Is THIS note a bin's contents? (The public `get` asks before
/// answering, and the honest by-id conflict read names the lapide with
/// the same word.)
pub(crate) fn note_in_bin(conn: &rusqlite::Connection, id: &str) -> Result<bool, CellError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM notes_in_bin WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Every note id the bin holds right now (the graph's in-memory index
/// asks once at build time).
pub(crate) fn bin_note_ids(conn: &rusqlite::Connection) -> Result<std::collections::HashSet<String>, CellError> {
    let mut stmt = conn.prepare("SELECT id FROM notes_in_bin")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}
