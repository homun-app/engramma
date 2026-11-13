// SPDX-License-Identifier: Apache-2.0

//! v0.6 (P0): the two text indices. `notes_fts` holds the visible notes,
//! `notes_fts_hidden` the quarantined ones, tombstones neither — and every
//! passage of state moves the row inside the operation's own transaction,
//! so the split can never lag behind the state it serves. The v14
//! migration moves the rows of an existing cell; `verify` checks both
//! sides against the statuses.

use engramma_core::format::{At, Note, NoteType};
use engramma_core::search::Query;
use engramma_core::{Cell, Confidence, Status};
use time::OffsetDateTime;
use ulid::Ulid;

fn now() -> At {
    At(OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */))
}

fn note(id: u32, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{id:08}", "0".repeat(16)))
            .unwrap(/* test-only: valid Crockford */),
        note_type: NoteType::Fact,
        title: None,
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: String::new(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: now(),
        updated_at: now(),
        extensions: vec![],
    }
}

/// An external note: born quarantined, invisible to the default view.
fn hidden_note(id: u32, body: &str) -> Note {
    let mut n = note(id, body);
    n.confidence = Confidence::External;
    n.status = Status::Quarantined;
    n.source = "import:web".into();
    n
}

fn one(cell: &Cell, sql: &str) -> i64 {
    cell.debug_column(sql)
        .unwrap(/* test-only */)
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1)
}

/// (rows in the base index, rows in the hidden index).
fn idx_sizes(cell: &Cell) -> (i64, i64) {
    (
        one(cell, "SELECT CAST(COUNT(*) AS TEXT) FROM notes_fts_docsize"),
        one(
            cell,
            "SELECT CAST(COUNT(*) AS TEXT) FROM notes_fts_hidden_docsize",
        ),
    )
}

/// The index term of a word (`body_norm` is stemmed and the index holds
/// stems: `flat()` lists the forms raw-first, stem-last, so a MATCH must
/// speak its last one).
fn term(word: &str) -> String {
    engramma_core::text::query_terms(word, None, &[])
        .flat()
        .last()
        .cloned()
        .unwrap_or_else(|| word.to_string())
}

/// Every passage of state moves the FTS row in the SAME transaction as the
/// operation: the base index answers the default view with exactly the
/// visible notes, the hidden index holds exactly the quarantined ones, and
/// a tombstone lives in neither.
#[test]
fn status_passages_move_the_row_inside_the_operation() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("p.sqlite"), "p", "t").unwrap(/* test-only */);

    let visible = note(1, "Fabio lavora a Milano");
    cell.put_note(&visible, "t").unwrap(/* test-only */);
    assert_eq!(idx_sizes(&cell), (1, 0));
    assert!(cell.verify().unwrap(/* test-only */).ok());

    let hidden = hidden_note(2, "Zebedeo compra il tonno");
    let hidden_id = hidden.id;
    cell.put_note(&hidden, "t").unwrap(/* test-only */);
    assert_eq!(idx_sizes(&cell), (1, 1));
    assert!(cell.verify().unwrap(/* test-only */).ok());
    // the hidden body is searchable ONLY through the owner's view
    let zebedeo = format!("'{}'", term("Zebedeo"));
    assert_eq!(
        one(
            &cell,
            &format!(
                "SELECT CAST(COUNT(*) AS TEXT) FROM notes_fts WHERE notes_fts MATCH {zebedeo}"
            )
        ),
        0
    );
    assert_eq!(
        one(
            &cell,
            &format!(
                "SELECT CAST(COUNT(*) AS TEXT) FROM notes_fts_hidden WHERE notes_fts_hidden MATCH {zebedeo}"
            )
        ),
        1
    );

    // confirm: the row moves from the hidden index to the base one
    cell.confirm(&hidden_id, "t").unwrap(/* test-only */);
    assert_eq!(idx_sizes(&cell), (2, 0));
    assert_eq!(
        one(
            &cell,
            &format!(
                "SELECT CAST(COUNT(*) AS TEXT) FROM notes_fts WHERE notes_fts MATCH {zebedeo}"
            )
        ),
        1
    );
    assert!(cell.verify().unwrap(/* test-only */).ok());

    // delete: the tombstone leaves both
    cell.delete_note(&hidden_id, "t").unwrap(/* test-only */);
    assert_eq!(idx_sizes(&cell), (1, 0));
    assert!(cell.verify().unwrap(/* test-only */).ok());
}

/// The v14 migration on a v13 cell: the quarantined rows that lived in the
/// one index move to their own, the base index is rebuilt on the visible
/// notes only, and `verify` passes on the migrated file.
#[test]
fn a_v13_cell_migrates_its_hidden_rows() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("p.sqlite");
    let cell = Cell::create(&path, "p", "t").unwrap(/* test-only */);
    cell.put_note(&note(1, "Fabio lavora a Milano"), "t")
        .unwrap(/* test-only */);
    cell.put_note(&note(2, "Giulia studio a Roma"), "t")
        .unwrap(/* test-only */);
    cell.put_note(&hidden_note(3, "Zebedeo compra il tonno"), "t")
        .unwrap(/* test-only */);
    cell.put_note(&hidden_note(4, "Aurora compra il tonno"), "t")
        .unwrap(/* test-only */);
    drop(cell);

    // the v13 shape: ONE index holding everything, version 13
    {
        let conn = rusqlite::Connection::open(&path).unwrap(/* test-only */);
        conn.execute_batch(
            "DROP TABLE notes_fts_hidden;
             INSERT INTO notes_fts(rowid, title, body_norm, tags_text)
                  SELECT rowid, COALESCE(title, ''), COALESCE(body_norm, ''), COALESCE(tags_text, '')
                  FROM notes WHERE status = 'quarantined';
             UPDATE meta SET value = '13' WHERE key = 'schema_version';",
        )
        .unwrap(/* test-only */);
    }

    let cell = Cell::open(&path).unwrap(/* test-only */);
    assert_eq!(
        one(&cell, "SELECT value FROM meta WHERE key = 'schema_version'"),
        engramma_core::cell::schema::SCHEMA_VERSION
    );
    // two visible in the base, two hidden in their own index
    assert_eq!(idx_sizes(&cell), (2, 2));
    let tonno = format!("'{}'", term("tonno"));
    assert_eq!(
        one(
            &cell,
            &format!("SELECT CAST(COUNT(*) AS TEXT) FROM notes_fts WHERE notes_fts MATCH {tonno}")
        ),
        0
    );
    assert_eq!(
        one(
            &cell,
            &format!(
                "SELECT CAST(COUNT(*) AS TEXT) FROM notes_fts_hidden WHERE notes_fts_hidden MATCH {tonno}"
            )
        ),
        2
    );
    assert!(cell.verify().unwrap(/* test-only */).ok());

    // and the searches behave: the default view never sees the hidden
    // notes, the owner's view does
    let visible_only: Vec<String> = cell
        .search(&Query::text("tonno").k(10))
        .unwrap(/* test-only */)
        .into_iter()
        .map(|h| h.note.id.to_string())
        .collect();
    assert!(visible_only.is_empty(), "{visible_only:?}");
    let mut q = Query::text("tonno").k(10);
    q.filters.include_quarantined = true;
    let owners: Vec<String> = cell
        .search(&q)
        .unwrap(/* test-only */)
        .into_iter()
        .map(|h| h.note.id.to_string())
        .collect();
    assert_eq!(owners.len(), 2, "{owners:?}");
}

/// The owner's `include_quarantined` (JS contract of v0.4) queries BOTH
/// indices and fuses them by rank: hidden and visible notes compete in one
/// list, ordered deterministically.
#[test]
fn the_owners_view_searches_both_indices() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("p.sqlite"), "p", "t").unwrap(/* test-only */);
    cell.put_note(&note(1, "Fabio porta il tonno a casa"), "t")
        .unwrap(/* test-only */);
    let hidden = hidden_note(2, "Zebedeo compra il tonno");
    cell.put_note(&hidden, "t").unwrap(/* test-only */);

    let default: Vec<String> = cell
        .search(&Query::text("tonno").k(10))
        .unwrap(/* test-only */)
        .into_iter()
        .map(|h| h.note.id.to_string())
        .collect();
    assert_eq!(default.len(), 1, "{default:?}");
    assert!(!default.contains(&hidden.id.to_string()));

    let mut q = Query::text("tonno").k(10);
    q.filters.include_quarantined = true;
    let owners: Vec<String> = cell
        .search(&q)
        .unwrap(/* test-only */)
        .into_iter()
        .map(|h| h.note.id.to_string())
        .collect();
    assert_eq!(owners.len(), 2, "{owners:?}");
    assert!(owners.contains(&hidden.id.to_string()));
}

/// `verify` checks BOTH indices: a row quietly removed from the hidden
/// index (the note untouched) must fail the verification.
#[test]
fn verify_catches_a_rotten_hidden_index() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("p.sqlite");
    let cell = Cell::create(&path, "p", "t").unwrap(/* test-only */);
    cell.put_note(&hidden_note(1, "Zebedeo compra il tonno"), "t")
        .unwrap(/* test-only */);
    assert!(cell.verify().unwrap(/* test-only */).ok());
    {
        // a second connection on the file, as the forgetting tests do: the
        // row quietly leaves the hidden index, the note stays
        let conn = rusqlite::Connection::open(&path).unwrap(/* test-only */);
        let (rowid, title, body_norm, tags_text): (i64, String, String, String) = conn
            .query_row(
                "SELECT n.rowid, COALESCE(n.title, ''), n.body_norm, n.tags_text
                 FROM notes n JOIN notes_fts_hidden f ON f.rowid = n.rowid LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap(/* test-only */);
        conn.execute(
            "INSERT INTO notes_fts_hidden(notes_fts_hidden, rowid, title, body_norm, tags_text)
             VALUES ('delete', ?1, ?2, ?3, ?4)",
            rusqlite::params![rowid, title, body_norm, tags_text],
        )
        .unwrap(/* test-only */);
    }
    let report = cell.verify().unwrap(/* test-only */);
    assert!(
        !report.ok(),
        "a rotten hidden index must fail verification: {report:?}"
    );
    assert!(!report.fts_ok, "{report:?}");
}
