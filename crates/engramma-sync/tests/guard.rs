// SPDX-License-Identifier: Apache-2.0

//! K1 (v0.6.1-ciclo): the write guard's DECISIONS. The cheap poll stays
//! on the file's `data_version`, but the pass is armed by the LOG's
//! marker, minus the rows the serve's own sessions brought:
//!
//! - rows beyond the sessions' work are LOCAL: a pass toward all;
//! - rows that are all the sessions' work: a pass toward the OTHERS,
//!   never back to the sources;
//! - no rows: no pass at all, and the baselines settle.
//!
//! And the ack comes AFTER the pass: a local write that lands mid-pass is
//! handed to another pass (`finish_pass` says so), never swallowed.

use std::time::Duration;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use engramma_sync::serve::{DataVersionWatch, PassDecision};
use engramma_sync::sharedcell::share_cell;
use time::OffsetDateTime;
use ulid::Ulid;

fn note(n: u64) -> Note {
    let t = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* fixed */);
    Note {
        id: Ulid::from_string(&format!("01F7ZQ6VTSATW2BQ5Z9R{:06X}", n)).unwrap(/* t */),
        note_type: NoteType::Fact,
        title: Some(format!("nota {n}")),
        body: format!("il corpo della nota {n}"),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: At(t),
        updated_at: At(t),
        extensions: vec![],
    }
}

/// A root with one shared cell and the guard open on it.
fn scene(tag: &str) -> (tempfile::TempDir, DataVersionWatch, Cell, Option<Cell>) {
    scene_cells(tag, &["memoria"])
}

/// A root with N shared cells: the guard, one handle of the FIRST cell,
/// and one handle of the last (when there are two).
fn scene_cells(
    tag: &str,
    names: &[&str],
) -> (
    tempfile::TempDir,
    DataVersionWatch,
    Cell,
    Option<Cell>,
) {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let root = dir.path().join(tag);
    std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
    for name in names {
        Cell::create(&root.join("cells").join(format!("{name}.sqlite")), name, "cli")
            .unwrap(/* t */);
        share_cell(&root, name).unwrap(/* t */);
    }
    // the guard watches; a SECOND handle writes, as another process would
    let writer = Cell::open(&root.join("cells").join(format!("{}.sqlite", names[0])))
        .unwrap(/* t */);
    let other = names
        .last()
        .filter(|last| **last != names[0])
        .map(|last| {
            Cell::open(&root.join("cells").join(format!("{last}.sqlite"))).unwrap(/* t */)
        });
    let watch = DataVersionWatch::new(&root).unwrap(/* t */);
    (dir, watch, writer, other)
}

#[test]
fn local_rows_are_a_pass_toward_all() {
    let (_dir, mut watch, writer, _) = scene("guard-a");
    writer.put_note(&note(1), "cli").unwrap(/* t */);
    assert!(watch.moved().unwrap(/* t */), "the file's counter moved");
    assert_eq!(
        watch.evaluate().unwrap(/* t */),
        PassDecision::All,
        "a row no session brought is LOCAL work: a pass toward all"
    );
    assert!(watch.busy(), "the guard stays armed until the pass settles");
    // the pass settles: nothing else arrived
    assert!(
        !watch.finish_pass(&[]).unwrap(/* t */),
        "no further local rows: no second pass"
    );
    assert!(!watch.busy());
    assert!(!watch.moved().unwrap(/* t */), "the baselines settled");
}

#[test]
fn session_rows_are_a_pass_toward_the_others() {
    let (_dir, mut watch, writer, _) = scene("guard-b");
    // three rows the sessions brought, fed by device X
    for i in 1..=3_u64 {
        writer.put_note(&note(i), "cli").unwrap(/* t */);
    }
    watch.note_session("dispositivo-x", &[("memoria".into(), 3)]);
    assert_eq!(
        watch.evaluate().unwrap(/* t */),
        PassDecision::Others(vec!["dispositivo-x".into()]),
        "rows that are all the sessions' work go to the OTHERS — never back to the source"
    );
}

#[test]
fn the_sessions_ledger_and_local_rows_mix_into_all() {
    let (_dir, mut watch, writer, _) = scene("guard-c");
    writer.put_note(&note(1), "cli").unwrap(/* t */);
    watch.note_session("dispositivo-x", &[("memoria".into(), 1)]);
    writer.put_note(&note(2), "cli").unwrap(/* t */);
    assert_eq!(
        watch.evaluate().unwrap(/* t */),
        PassDecision::All,
        "one row beyond the sessions' work is a local write: the pass goes to all"
    );
}

#[test]
fn a_file_that_moved_without_rows_settles_in_silence() {
    let (_dir, mut watch, writer, _) = scene("guard-d");
    // the FIRST session ack writes its row (a head never confirmed): the
    // file moves, the LOG does not — and the second ack of the SAME head
    // writes nothing at all
    writer.record_session_ack("dispositivo-x").unwrap(/* t */);
    assert!(watch.moved().unwrap(/* t */));
    assert_eq!(
        watch.evaluate().unwrap(/* t */),
        PassDecision::None,
        "bookkeeping that is not operations never arms a pass"
    );
    assert!(!watch.busy());
    writer.record_session_ack("dispositivo-x").unwrap(/* t */);
    assert!(
        !watch.moved().unwrap(/* t */),
        "a session that exchanged nothing wrote nothing: the counter never moved"
    );
}

#[test]
fn a_local_write_mid_pass_is_handed_to_the_next_pass() {
    let (_dir, mut watch, writer, _) = scene("guard-e");
    writer.put_note(&note(1), "cli").unwrap(/* t */);
    assert_eq!(watch.evaluate().unwrap(/* t */), PassDecision::All);
    // the pass runs; WHILE it runs, another local write lands
    std::thread::sleep(Duration::from_millis(10));
    writer.put_note(&note(2), "cli").unwrap(/* t */);
    assert!(
        watch.finish_pass(&[]).unwrap(/* t */),
        "the mid-pass write must reach the next pass, not be swallowed by the settle"
    );
    assert!(
        !watch.busy(),
        "the guard is free: the next pass is the worker's"
    );
}

// ---------------------------------------------------------------------------
// S1 (v0.6.2): the marker's comparison is PER CELL. The v0.6.1 guard
// summed every cell's delta, and the sum let one cell's story decide
// another cell's: a cell whose rows shrink (a compaction forgets the
// oldest) covered a local write in another cell, and a session's ledger
// inflated by a refused envelope (counted as received, landed as nothing)
// read a local write as session work. Each cell owes its own decision.
// ---------------------------------------------------------------------------

/// The spec's own sentence (S1): a PURGE in one cell and a WRITE in
/// another, in the same guard window — the write leaves with its pass.
/// (The purge op ADDS a row, it never removes one: measured, `ops` went
/// 3 → 4 across a purge, and the compaction keeps its rows too. The
/// cross-cell SUM was the hazard the reviewer named; the two divergent
/// shapes — the inflated ledger and the receipt — have their own tests.)
#[test]
fn a_purge_in_one_cell_covers_no_write_in_another() {
    let (_dir, mut watch, purging, writer_b) = scene_cells("guard-s1", &["alfa", "beta"]);
    let writer_b = writer_b.expect(/* two */ "the second cell's handle");
    // one note in alfa, then its purge: the forget op lands in the log
    purging.put_note(&note(1), "cli").unwrap(/* t */);
    let the_note = purging
        .debug_column("SELECT note_id FROM ops WHERE note_id IS NOT NULL LIMIT 1")
        .unwrap(/* t */)
        .first()
        .cloned()
        .unwrap_or_default();
    purging
        .purge(
            &ulid::Ulid::from_string(&the_note).unwrap(/* t */),
            "oblio",
            "cli",
        )
        .unwrap(/* t */);
    // beta takes ONE local write, by another handle, as another process
    writer_b.put_note(&note(2), "cli").unwrap(/* t */);
    assert!(watch.moved().unwrap(/* t */), "a file's counter moved");
    assert_eq!(
        watch.evaluate().unwrap(/* t */),
        PassDecision::All,
        "beta's write is beta's own news: alfa's purge must not cover it"
    );
}

/// A session's ledger inflated in ONE cell (a refused envelope counted as
/// received: the report says rows that never landed) must not read a
/// local write in ANOTHER cell as the session's own work.
#[test]
fn an_inflated_ledger_in_one_cell_covers_no_write_in_another() {
    let (_dir, mut watch, writer_a, writer_b) = scene_cells("guard-s2", &["alfa", "beta"]);
    let writer_b = writer_b.expect(/* two */ "the second cell's handle");
    // two real rows land in alfa; the session's report counted THREE (one
    // envelope was refused at the gate, and the report said received)
    writer_a.put_note(&note(1), "cli").unwrap(/* t */);
    writer_a.put_note(&note(2), "cli").unwrap(/* t */);
    watch.note_session("dispositivo-x", &[("alfa".into(), 3)]);
    // a local write lands in beta
    writer_b.put_note(&note(3), "cli").unwrap(/* t */);
    assert_eq!(
        watch.evaluate().unwrap(/* t */),
        PassDecision::All,
        "beta's write is local work: alfa's inflated ledger must not cover it"
    );
}

/// The pass's own receipt is PER CELL too: what the pass received in
/// alfa settles alfa, and a write that lands in beta mid-pass is NEW.
#[test]
fn the_pass_receipt_is_per_cell() {
    let (_dir, mut watch, writer_a, writer_b) = scene_cells("guard-s3", &["alfa", "beta"]);
    let writer_b = writer_b.expect(/* two */ "the second cell's handle");
    writer_a.put_note(&note(1), "cli").unwrap(/* t */);
    assert_eq!(watch.evaluate().unwrap(/* t */), PassDecision::All);
    // the pass runs; WHILE it runs, a write lands in beta
    std::thread::sleep(Duration::from_millis(10));
    writer_b.put_note(&note(2), "cli").unwrap(/* t */);
    // the pass says it received ONE row — in alfa, from the OTHER cell's
    // point of view the receipt is nothing
    assert!(
        watch.finish_pass(&[("alfa".into(), 1)]).unwrap(/* t */),
        "beta's mid-pass write is new work: the next pass takes it"
    );
}
