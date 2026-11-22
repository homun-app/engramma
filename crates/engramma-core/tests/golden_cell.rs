// SPDX-License-Identifier: Apache-2.0

//! v0.6.8 (H7): the GOLDEN CELL. Written by this version with the
//! repeatable script `examples/make_golden_cell.rs` (synthetic data,
//! never real), the cell holds notes of every type and state, the
//! quarantine born external and confirmed, a lapide, a supersede, an
//! OPEN conflict copy and a CLOSED one, a purge, a compaction with its
//! snapshot file, a vault view and vectors. This test copies the golden
//! into a temporary folder, opens it with TODAY's engine, runs `verify`
//! and checks the facts it must find — every version to come opens and
//! migrates it without losses (DESIGN § 8: each schema-changing phase
//! adds its own golden and never touches the old ones; this test opens
//! them ALL).

use engramma_core::search::Query;
use engramma_core::{Cell, Status};
use std::path::{Path, PathBuf};
use ulid::Ulid;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Every golden cell of the suite: the v0.6.8 one, and the v0.6.12 one
/// (v0.6.13, M1: compacted by the PREVIOUS engine and frozen before the
/// schema change — the whole-object anchor migration opens it). A new
/// golden lands here by NAME, and this list growing is the visible part
/// of the rule.
const GOLDEN_CELLS: &[&str] = &["v068", "v0612", "v0613"];

/// The deterministic note id of the golden's note `i`
/// (`01` + sixteen zeros + eight hex digits).
fn nid(i: u32) -> Ulid {
    Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), i)).unwrap(/* the golden's own ids */)
}

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

#[test]
fn the_golden_cell_opens_verifies_and_holds_its_facts() {
    for name in GOLDEN_CELLS {
        let dir = test_dir();
        let golden = golden_dir();
        // the live file and every snapshot beside it, under their own
        // names (the cell's rows name them so)
        let mut copied = 0;
        for entry in std::fs::read_dir(&golden).unwrap(/* t */).flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if file_name.starts_with(&format!("{name}.sqlite"))
                || (file_name.starts_with("b-snap-") && file_name.ends_with(".sqlite"))
                || (file_name.starts_with(&format!("{name}-snap-"))
                    && file_name.ends_with(".sqlite"))
            {
                std::fs::copy(entry.path(), dir.path().join(&file_name)).unwrap(/* t */);
                copied += 1;
            }
        }
        assert!(copied >= 2, "the golden holds the cell and its snapshot");
        let cell = Cell::open(&dir.path().join(format!("{name}.sqlite"))).unwrap(/* t */);

        // verify is green, and it says it replayed FROM THE SNAPSHOT
        let report = cell.verify().unwrap(/* t */);
        assert!(
            report.ok(),
            "the golden verifies on today's engine: {:?}",
            report.mismatch
        );
        assert!(
            matches!(
                report.replay,
                engramma_core::cell::Replay::FromSnapshot { .. }
            ),
            "the golden carries a compaction with a snapshot: {:?}",
            report.replay
        );

        match *name {
            "v068" => v068_facts(&cell),
            "v0612" => v0612_facts(&cell),
            "v0613" => v0613_facts(&cell),
            other => panic!("a golden with no facts to check: {other}"),
        }
    }
}

/// The v0.6.8 golden's own facts (every note it holds, its purges, its
/// conflict copies, its searches, its vectors, its view).
fn v068_facts(cell: &Cell) {
        // the notes of every type are there, id-sorted and whole
        let ids: Vec<Ulid> = cell.note_ids_all().unwrap(/* t */);
        for i in 1..=10_u32 {
            assert!(
                ids.contains(&nid(i)),
                "the golden's note {i} is there (v068)"
            );
        }
        // the purge took its note and its family
        assert!(
            !ids.contains(&nid(11)),
            "the purged note is gone from the golden (v068)"
        );
        match cell.get(&nid(11)) {
            Err(engramma_core::CellError::NoteNotFound(_)) => {}
            other => panic!("the purged note answers nothing: {other:?}"),
        }

        // the states: a lapide, a superseded pair, a confirmed external
        assert_eq!(cell.get(&nid(6)).unwrap(/* t */).status, Status::Deleted);
        assert_eq!(cell.get(&nid(5)).unwrap(/* t */).status, Status::Superseded);
        assert_eq!(cell.get(&nid(7)).unwrap(/* t */).status, Status::Active);
        assert_eq!(cell.get(&nid(8)).unwrap(/* t */).status, Status::Active);

        // the open conflict: the winner's body on the note, the loser's
        // on the copy; the closed one stays closed
        assert_eq!(
            cell.get(&nid(9)).unwrap(/* t */).body,
            "prima riga\nmanopola smeraldo\nterza"
        );
        let open: Vec<_> = cell
            .conflicts()
            .unwrap(/* t */)
            .into_iter()
            .filter(|c| c.original_id == nid(9))
            .collect();
        assert_eq!(open.len(), 1, "one open copy on the fact (v068)");
        let copy = open[0].copy_id;
        // the compaction cut took the payloads (conflict's copy_body
        // answers None under it, as documented) — the copy's own notes
        // row carries the loser's text
        assert_eq!(
            cell.get(&copy).unwrap(/* t */).body,
            "prima riga\nocchiello turchino verso [[Namorio]]\nterza",
            "the loser's body lives in its copy (v068)"
        );
        assert!(
            cell.conflicts()
                .unwrap(/* t */)
                .iter()
                .all(|c| c.original_id != nid(10)),
            "the resolved conflict stays closed (v068)"
        );

        // three searches, three answers: the winner, the copy, a person
        let hits = |q: &str| -> Vec<Ulid> {
            cell.search(&Query::text(q).k(10))
                .unwrap(/* t */)
                .into_iter()
                .map(|h| h.note.id)
                .collect()
        };
        assert!(
            hits("manopola smeraldo").contains(&nid(9)),
            "the winner's body answers (v068)"
        );
        assert!(
            hits("occhiello turchino").contains(&copy),
            "the copy's body answers (v068)"
        );
        assert!(
            hits("Giulia").contains(&nid(1)),
            "the person note answers (v068)"
        );
}

/// v0.6.13 (M1): the v0.6.12 golden's facts. The cell was compacted by
/// the PREVIOUS engine — the queue row and the registry row of its
/// proposal are born at or below the cut, and the migration built their
/// whole-object anchor at open. The purge of the proposal's target must
/// SUCCEED (the v0.6.12 engine lost the queue row in the same
/// reconstruction), and the queue must still be there, reverted: the
/// story froze the user accepting the agent's correction ON the cut and
/// cancelling it above.
fn v0612_facts(cell: &Cell) {
    // the story's shape: A closed by the accepted correction, the
    // cancel above the cut — the v0.6.12 quirk stays as the artifact
    // froze it (the redacted decide payload leaves `restore_target`
    // null, so the cancel reopens nothing)
    let pid = nid(3);
    assert_eq!(
        cell.get(&pid).unwrap(/* t */).status,
        Status::Quarantined,
        "the cancelled proposal is back in the queue"
    );
    let queue: Vec<String> = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        queue,
        vec!["supersede reverted".to_string()],
        "the queue row the cut froze, with the cancel's word on it"
    );
    let decisions: Vec<String> = cell
        .debug_column(&format!(
            "SELECT CAST(COUNT(*) AS TEXT) FROM decisions WHERE proposal = '{pid}' \
             AND reverted_by IS NOT NULL"
        ))
        .unwrap(/* t */);
    assert_eq!(
        decisions.first().map(String::as_str),
        Some("1"),
        "the registry row, with the cancel's mark"
    );
    // the anchors the migration built: the proposal has operations
    // above the cut (the cancel), so its whole-object anchor exists
    let anchors: Vec<String> = cell
        .debug_column(&format!(
            "SELECT CAST(COUNT(*) AS TEXT) FROM object_anchors WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert!(
        anchors.first().is_some_and(|n| n != "0"),
        "the migration back-filled the whole-object anchor of the proposal"
    );
    // and the purge of the tied target succeeds, leaving the queue
    cell.purge(&nid(1), "dato superato", "user:golden")
        .expect(/* t */ "the purge of the proposal's target succeeds");
    assert_eq!(
        cell.get(&pid).unwrap(/* t */).status,
        Status::Quarantined,
        "the proposal stays in the queue after the purge"
    );
    let queue: Vec<String> = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        queue,
        vec!["supersede reverted".to_string()],
        "the queue row survives the purge (the anchor restored it whole)"
    );
    assert!(
        cell.verify().unwrap(/* t */).ok(),
        "the v0.6.12 golden verifies after the purge"
    );
}

/// v0.6.13 (M1): this phase's golden. The agent's correction of the
/// note 12 was proposed and ACCEPTED before the cut: the queue row and
/// the registry row live at or below it, with the whole-object anchors
/// beside them. The purge of the target must succeed and keep the
/// queue whole — the reconstruction it forces reads the anchors this
/// very engine wrote.
fn v0613_facts(cell: &Cell) {
    let pid = nid(13);
    assert_eq!(
        cell.get(&pid).unwrap(/* t */).status,
        Status::Active,
        "the accepted correction is active"
    );
    assert_eq!(
        cell.get(&nid(12)).unwrap(/* t */).status,
        Status::Superseded,
        "the target closed by the acceptance"
    );
    let queue: Vec<String> = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(queue, vec!["supersede accepted".to_string()]);
    let anchors: Vec<String> = cell
        .debug_column(&format!(
            "SELECT CAST(COUNT(*) AS TEXT) FROM object_anchors WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert!(
        anchors.first().is_some_and(|n| n != "0"),
        "the whole-object anchors are part of the golden"
    );
    // the purge of the target: it succeeds, and the queue stays whole
    cell.purge(&nid(12), "dato superato", "homun:user")
        .expect(/* t */ "the purge of the correction's target succeeds");
    let queue: Vec<String> = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        queue,
        vec!["supersede accepted".to_string()],
        "the queue row survives the purge (the anchors restored it whole)"
    );
    assert_eq!(
        cell.get(&pid).unwrap(/* t */).status,
        Status::Active,
        "the accepted text stands"
    );
    assert!(
        cell.verify().unwrap(/* t */).ok(),
        "the v0613 golden verifies after the purge"
    );
}
