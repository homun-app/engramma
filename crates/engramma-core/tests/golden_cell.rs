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
/// schema change — the whole-object anchor migration opens it). The
/// v0.6.17 one is the BETA's golden: the cell the public compatibility
/// promise leans on (docs/compatibilita.md). A new golden lands here by
/// NAME, and this list growing is the visible part of the rule.
const GOLDEN_CELLS: &[&str] = &["v068", "v0612", "v0613", "v0613-indietro", "v0614", "v0617"];

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
            "v0613-indietro" => v0613_indietro_facts(&cell),
            "v0614" => v0614_facts(&cell),
            "v0617" => v0617_facts(&cell),
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

/// v0.6.14 (N1): the golden the v0.6.13 engine damaged and froze. Its
/// story: an agent's `new` proposal, the user's accept, a compaction WITH
/// its copy file at the head (seq 4), a second compaction one step BELOW
/// it (registered 3), and the user's supersede of the proposal above the
/// cut. The base left the queue `pending` with no registry row — the
/// `decide` that accepted sat above the registered cut with its payload
/// gone — and called the cell green. The v24 migration must open it, take
/// the record back to the true cut, rebuild the proposal from the copy
/// file, and `verify` must be green with the queue whole; a write above
/// the cut keeps it whole.
fn v0613_indietro_facts(cell: &Cell) {
    let pid = nid(3);
    // the record is back where the payloads are gone
    let cut: Vec<String> = cell
        .debug_column("SELECT CAST(seq AS TEXT) FROM snapshots")
        .unwrap(/* t */);
    assert_eq!(
        cut,
        vec!["4".to_string()],
        "the registered cut is the true one"
    );
    let above: Vec<String> = cell
        .debug_column(
            "SELECT op_id FROM ops WHERE payload_state = 'redacted' AND seq > 4",
        )
        .unwrap(/* t */);
    assert!(above.is_empty(), "no payload is removed above the cut");
    // the user's decision, whole again
    let queue: Vec<String> = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        queue,
        vec!["new accepted".to_string()],
        "the queue says the user accepted it"
    );
    let registry: Vec<String> = cell
        .debug_column(&format!(
            "SELECT decision || ' ' || actor FROM decisions WHERE proposal = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        registry,
        vec!["accept user:golden".to_string()],
        "the registry holds it"
    );
    // and the user's supersede above the cut stands
    assert_eq!(cell.get(&pid).unwrap(/* t */).status, Status::Superseded);
    let closes: Vec<String> = cell
        .debug_column(&format!(
            "SELECT new_id || '→' || old_id FROM supersedes WHERE old_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        closes,
        vec![format!("{}→{pid}", nid(5))],
        "the supersede's row is there"
    );
    // a further write above the cut: the reconstruction keeps the queue
    cell.patch_note(
        &pid,
        &engramma_core::NotePatch {
            title: Some(Some("ritoccata sopra il taglio".into())),
            ..Default::default()
        },
        "user:golden",
    )
    .expect(/* t */ "the write above the cut lands");
    let queue: Vec<String> = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        queue,
        vec!["new accepted".to_string()],
        "the queue stays whole"
    );
    let report = cell.verify().unwrap(/* t */);
    assert!(
        report.ok(),
        "the golden verifies after the write: {:?}",
        report.mismatch
    );
    assert!(
        matches!(
            report.replay,
            engramma_core::cell::Replay::FromSnapshot { seq: 4 }
        ),
        "the replay starts from the copy file at the true cut: {:?}",
        report.replay
    );
    assert!(
        report.declared_corners.is_empty(),
        "a cell with its copy file has no corner: {:?}",
        report.declared_corners
    );
}

/// v0.6.14: this phase's golden, written by THIS engine (schema 24) — the
/// shapes the phase fixed, frozen so every later version keeps them: the
/// restore that brings the content back and leaves the state, the cut
/// that does not go back, the user's decision that survives the
/// reconstruction over the cut.
fn v0614_facts(cell: &Cell) {
    // the restore brings the CONTENT back and leaves the state
    let closed = cell.get(&nid(2)).expect(/* t */ "the closed note");
    assert_eq!(
        closed.status,
        Status::Superseded,
        "a restore does not reopen a closed note"
    );
    assert_eq!(
        closed.body, "la prima mano [[Roma]]",
        "and the content of version 1 comes back"
    );
    assert!(closed.valid_to.is_some(), "the closing stays");
    let hidden = cell.get(&nid(4)).expect(/* t */ "the external note");
    assert_eq!(
        hidden.status,
        Status::Quarantined,
        "the restore is no exit from the quarantine"
    );
    assert_eq!(
        hidden.body, "letto sul web [[Milano]]",
        "the content of version 2 comes back"
    );
    assert_eq!(
        hidden.confidence,
        engramma_core::Confidence::External,
        "the confidence is the lifecycle's"
    );
    let restores = cell
        .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM ops WHERE kind = 'restore'")
        .unwrap(/* t */);
    assert_eq!(
        restores,
        vec!["2".to_string()],
        "the two restores are in the log"
    );

    // the cut sits where the first compaction put it, and nothing above
    // it is removed
    let cut: i64 = cell.debug_column("SELECT CAST(seq AS TEXT) FROM snapshots").unwrap(/* t */)[0]
        .parse()
        .unwrap(/* t */);
    let above = cell
        .debug_column(&format!(
            "SELECT op_id FROM ops WHERE payload_state = 'redacted' AND seq > {cut}"
        ))
        .unwrap(/* t */);
    assert!(above.is_empty(), "no payload is removed above the cut");
    let again = cell
        .compact(
            cut - 1,
            &engramma_core::cell::CompactOptions {
                keep_snapshot: true,
            },
        )
        .expect(/* t */ "the call lands");
    assert!(
        !again.compacted && again.cut_seq == cut,
        "the cut does not go back on this cell either: {:?}",
        (again.compacted, again.cut_seq)
    );

    // the user's decision, whole after the supersede above the cut
    let pid = nid(5);
    let queue = |cell: &Cell| -> Vec<String> {
        cell.debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */)
    };
    assert_eq!(queue(cell), vec!["new accepted".to_string()], "the queue");
    let registry = cell
        .debug_column(&format!(
            "SELECT decision || ' ' || actor FROM decisions WHERE proposal = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        registry,
        vec!["accept user:golden".to_string()],
        "the registry"
    );
    assert_eq!(
        cell.get(&pid).expect(/* t */ "the proposal").status,
        Status::Superseded
    );
    let closes = cell
        .debug_column(&format!(
            "SELECT new_id || '→' || old_id FROM supersedes WHERE old_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        closes,
        vec![format!("{}→{pid}", nid(6))],
        "the supersede's row"
    );
    let six = cell
        .get(&nid(6))
        .expect(/* t */ "the note that closes the proposal");
    assert_eq!(
        six.title.as_deref(),
        Some("il blu, ritoccato sopra il taglio"),
        "the write above the cut stands"
    );
    assert_eq!(six.status, Status::Active);

    // a further write above the cut: the reconstruction keeps the queue
    cell.patch_note(
        &pid,
        &engramma_core::NotePatch {
            title: Some(Some("ritoccata sopra il taglio".into())),
            ..Default::default()
        },
        "user:golden",
    )
    .expect(/* t */ "the write above the cut lands");
    assert_eq!(
        queue(cell),
        vec!["new accepted".to_string()],
        "the queue stays whole"
    );
    let report = cell.verify().unwrap(/* t */);
    assert!(
        report.ok(),
        "the golden verifies after the write: {:?}",
        report.mismatch
    );
    assert!(
        matches!(
            report.replay,
            engramma_core::cell::Replay::FromSnapshot { seq } if seq == cut
        ),
        "the replay starts from the copy file at the cut: {:?}",
        report.replay
    );
    assert!(report.declared_corners.is_empty(), "no corner on this cell");
}

/// v0.6.17 (F1): the BETA's golden — the cell of the public promise
/// (docs/compatibilita.md), written by the schema-24 engine on two
/// devices. The facts it must keep through every future migration: the
/// notes of every type, the confirmed external, the supersede, the
/// lapide, the open conflict copy and the closed one, the agent's
/// accepted proposal, the purge — and the searches still answering.
fn v0617_facts(cell: &Cell) {
    // the notes are there, id-sorted and whole; the purge took its own
    let ids: Vec<Ulid> = cell.note_ids_all().unwrap(/* t */);
    for i in 1..=10_u32 {
        assert!(
            ids.contains(&nid(i)),
            "the golden's note {i} is there (v0617)"
        );
    }
    assert!(
        ids.contains(&nid(12)),
        "the accepted proposal's note is there (v0617)"
    );
    assert!(
        !ids.contains(&nid(11)),
        "the purged note is gone from the golden (v0617)"
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

    // the open conflict: the winner's body on the note, the loser's on
    // the copy; the closed one stays closed
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
    assert_eq!(open.len(), 1, "one open copy on the fact (v0617)");
    let copy = open[0].copy_id;
    assert_eq!(
        cell.get(&copy).unwrap(/* t */).body,
        "prima riga\nocchiello turchino verso [[Aldo]]\nterza",
        "the loser's body lives in its copy (v0617)"
    );
    assert!(
        cell.conflicts()
                .unwrap(/* t */)
                .iter()
                .all(|c| c.original_id != nid(10)),
        "the resolved conflict stays closed (v0617)"
    );

    // the agent's accepted proposal: the queue, the registry, the note
    assert_eq!(
        cell.get(&nid(12)).unwrap(/* t */).status,
        Status::Active,
        "the accepted correction is active"
    );
    let queue: Vec<String> = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{}'",
            nid(12)
        ))
        .unwrap(/* t */);
    assert_eq!(
        queue,
        vec!["new accepted".to_string()],
        "the queue says the user accepted it (v0617)"
    );
    let registry: Vec<String> = cell
        .debug_column(&format!(
            "SELECT decision || ' ' || actor FROM decisions WHERE proposal = '{}'",
            nid(12)
        ))
        .unwrap(/* t */);
    assert_eq!(
        registry,
        vec!["accept homun:user".to_string()],
        "the registry holds it (v0617)"
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
        "the winner's body answers (v0617)"
    );
    assert!(
        hits("occhiello turchino").contains(&copy),
        "the copy's body answers (v0617)"
    );
    assert!(
        hits("Sofia").contains(&nid(1)),
        "the person note answers (v0617)"
    );
}
