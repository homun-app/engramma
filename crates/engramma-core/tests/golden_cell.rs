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

/// Every golden cell of the suite: the v0.6.8 one (and the ones the
/// future phases add — a new golden lands here by NAME, and this list
/// growing is the visible part of the rule).
const GOLDEN_CELLS: &[&str] = &["v068"];

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

        // the notes of every type are there, id-sorted and whole
        let ids: Vec<Ulid> = cell.note_ids_all().unwrap(/* t */);
        for i in 1..=10_u32 {
            assert!(
                ids.contains(&nid(i)),
                "the golden's note {i} is there ({name})"
            );
        }
        // the purge took its note and its family
        assert!(
            !ids.contains(&nid(11)),
            "the purged note is gone from the golden ({name})"
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
        assert_eq!(open.len(), 1, "one open copy on the fact ({name})");
        let copy = open[0].copy_id;
        // the compaction cut took the payloads (conflict's copy_body
        // answers None under it, as documented) — the copy's own notes
        // row carries the loser's text
        assert_eq!(
            cell.get(&copy).unwrap(/* t */).body,
            "prima riga\nocchiello turchino verso [[Namorio]]\nterza",
            "the loser's body lives in its copy ({name})"
        );
        assert!(
            cell.conflicts()
                .unwrap(/* t */)
                .iter()
                .all(|c| c.original_id != nid(10)),
            "the resolved conflict stays closed ({name})"
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
            "the winner's body answers ({name})"
        );
        assert!(
            hits("occhiello turchino").contains(&copy),
            "the copy's body answers ({name})"
        );
        assert!(
            hits("Giulia").contains(&nid(1)),
            "the person note answers ({name})"
        );

        // the vectors are there (derived, but part of the golden)
        let vector_rows: i64 = cell
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM note_vectors")
            .unwrap(/* t */)[0]
            .parse()
            .unwrap(/* t */);
        assert!(vector_rows >= 9, "the golden holds vectors ({name})");

        // the vault view, with its one bound file
        let views = cell.vault_views().unwrap(/* t */);
        assert_eq!(views.len(), 1, "one view bound ({name})");
        assert_eq!(
            cell.vault_files_of_view(&views[0].view_id)
                .unwrap(/* t */)
                .len(),
            1,
            "one file bound to the view ({name})"
        );
    }
}
