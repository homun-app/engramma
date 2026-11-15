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
fn scene(tag: &str) -> (tempfile::TempDir, DataVersionWatch, Cell) {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let root = dir.path().join(tag);
    std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
    let cell = Cell::create(&root.join("cells").join("memoria.sqlite"), "memoria", "cli")
        .unwrap(/* t */);
    drop(cell);
    share_cell(&root, "memoria").unwrap(/* t */);
    // the guard watches; a SECOND handle writes, as another process would
    let writer = Cell::open(&root.join("cells").join("memoria.sqlite")).unwrap(/* t */);
    let watch = DataVersionWatch::new(&root).unwrap(/* t */);
    (dir, watch, writer)
}

#[test]
fn local_rows_are_a_pass_toward_all() {
    let (_dir, mut watch, writer) = scene("guard-a");
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
        !watch.finish_pass(0).unwrap(/* t */),
        "no further local rows: no second pass"
    );
    assert!(!watch.busy());
    assert!(!watch.moved().unwrap(/* t */), "the baselines settled");
}

#[test]
fn session_rows_are_a_pass_toward_the_others() {
    let (_dir, mut watch, writer) = scene("guard-b");
    // three rows the sessions brought, fed by device X
    for i in 1..=3_u64 {
        writer.put_note(&note(i), "cli").unwrap(/* t */);
    }
    watch.note_session("dispositivo-x", 3);
    assert_eq!(
        watch.evaluate().unwrap(/* t */),
        PassDecision::Others(vec!["dispositivo-x".into()]),
        "rows that are all the sessions' work go to the OTHERS — never back to the source"
    );
}

#[test]
fn the_sessions_ledger_and_local_rows_mix_into_all() {
    let (_dir, mut watch, writer) = scene("guard-c");
    writer.put_note(&note(1), "cli").unwrap(/* t */);
    watch.note_session("dispositivo-x", 1);
    writer.put_note(&note(2), "cli").unwrap(/* t */);
    assert_eq!(
        watch.evaluate().unwrap(/* t */),
        PassDecision::All,
        "one row beyond the sessions' work is a local write: the pass goes to all"
    );
}

#[test]
fn a_file_that_moved_without_rows_settles_in_silence() {
    let (_dir, mut watch, writer) = scene("guard-d");
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
    let (_dir, mut watch, writer) = scene("guard-e");
    writer.put_note(&note(1), "cli").unwrap(/* t */);
    assert_eq!(watch.evaluate().unwrap(/* t */), PassDecision::All);
    // the pass runs; WHILE it runs, another local write lands
    std::thread::sleep(Duration::from_millis(10));
    writer.put_note(&note(2), "cli").unwrap(/* t */);
    assert!(
        watch.finish_pass(0).unwrap(/* t */),
        "the mid-pass write must reach the next pass, not be swallowed by the settle"
    );
    assert!(
        !watch.busy(),
        "the guard is free: the next pass is the worker's"
    );
}
