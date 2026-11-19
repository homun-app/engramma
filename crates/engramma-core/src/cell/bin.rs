// SPDX-License-Identifier: Apache-2.0

//! v0.6.8 (H1): the bin's ONE predicate. A note is in the bin when it
//! is deleted, or when it is a conflict copy of a deleted note — one
//! definition, in ONE place: this module. Every read path speaks
//! [`not_in_bin`] (or [`original_holds_lapide`] for the original's own
//! side of the same rule); the v0.6.7 rule lived in four hand-written
//! filters, and `note_ids()`/`list()` and `get()` forgot the copy (the
//! reviewer's proof of the verification): the fifth door would have
//! forgotten the sixth.
//!
//! v0.6.9 (I2) narrowed the copy arm: the bin holds the copies NOBODY
//! took. A copy follows its original's lapide while it is still OPEN;
//! a copy the user TOOK — modified with their own hand, the witness
//! (a) of DESIGN § 4.3, which closes it — is the user's note and lives
//! its own life, lapide or no lapide (the reviewer's second proof: a
//! taken, re-tagged copy was hidden in `note_ids()` and shown by
//! `get()`, which asked the predicate only behind the user-writable
//! `conflitto` tag — no shortcut survives that).
//!
//! The FORM was chosen by the measure, as the spec asked, and two
//! candidates lost before this one won. A SQL VIEW (`notes_in_bin`)
//! read through `NOT IN (SELECT …)`: the planner materialized the
//! whole view — a scan of `notes` with an `EXISTS` probe per row — for
//! EVERY query, and the bench answered p50 0,56 → 4,34 ms on the
//! search and 50,8 → 96,6 ms on `list()`. A CORRELATED `NOT EXISTS`
//! per candidate row: one probe per row, but `visible_note_count` and
//! `note_ids` walk every row — p50 1,70 ms, list 95,4 ms, still over.
//! The winner drives the SMALL table: the bin's copies are a handful
//! of rows, `NOT IN` over them materializes a tiny set once per
//! statement and every candidate row probes it — the numbers went
//! home (the pairs of the v0.6.8 REPORT). One definition, two
//! projections; a note row and an original ask the same rule in the
//! shape each can speak.

use crate::error::CellError;

/// The read-path fragment for a NOTES row `note`: not deleted, and not
/// an OPEN conflict copy whose original holds a lapide (v0.6.9, I2: a
/// taken copy — closed by the witness (a) — is the user's note and
/// stays). The copies' ids come from the SMALL table
/// (`conflict_copies` joined to its deleted originals — a handful of
/// rows), so `NOT IN` builds a tiny ephemeral set once per statement:
/// a full-table read (the counts, the lists) pays one probe per row
/// into a set that is usually empty.
pub(crate) fn not_in_bin(note: &str) -> String {
    format!(
        "{note}.status != 'deleted' AND {note}.id NOT IN (
             SELECT cc.copy_id FROM conflict_copies cc
             JOIN notes o ON o.id = cc.original_id
             WHERE o.status = 'deleted' AND cc.open = 1)"
    )
}

/// The same predicate from the ORIGINAL's side (the copies' listings
/// ask it this way): does this original hold an active lapide? The
/// copy's own half of the question — is the copy still OPEN, or did
/// the user take it? — is the caller's to add, because the listings
/// that ask it already read the copy's row.
pub(crate) fn original_holds_lapide(original: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM notes o
                 WHERE o.id = {original} AND o.status = 'deleted')"
    )
}

/// Is THIS note a bin's contents? (The public `get` asks before
/// answering, and the honest by-id conflict read names the lapide with
/// the same word.) Two indexed probes, no scans. v0.6.9 (I2): the copy
/// arm asks `open = 1` too — same rule, same answer as [`not_in_bin`]
/// by construction (the two SQL texts ARE the same predicate).
pub(crate) fn note_in_bin(conn: &rusqlite::Connection, id: &str) -> Result<bool, CellError> {
    let n: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM notes WHERE id = ?1 AND status = 'deleted')
              + (SELECT COUNT(*) FROM conflict_copies cc
                 JOIN notes o ON o.id = cc.original_id
                 WHERE cc.copy_id = ?1 AND o.status = 'deleted' AND cc.open = 1)",
        [id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Every note id the bin holds right now (the graph's in-memory index
/// asks once at build time): the deleted notes, and the OPEN copies
/// whose original holds a lapide — the copies nobody took (v0.6.9, I2).
pub(crate) fn bin_note_ids(
    conn: &rusqlite::Connection,
) -> Result<std::collections::HashSet<String>, CellError> {
    let mut out = std::collections::HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM notes WHERE status = 'deleted'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            out.insert(row?);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT cc.copy_id FROM conflict_copies cc
             JOIN notes o ON o.id = cc.original_id
             WHERE o.status = 'deleted' AND cc.open = 1",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            out.insert(row?);
        }
    }
    Ok(out)
}
