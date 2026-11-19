// SPDX-License-Identifier: Apache-2.0

//! v0.6.9 (I1): a purged id never comes back. The reviewer's hunt of the
//! v0.6.8 verification found the seed `1207186286826530307` (the property
//! `compaction_keeps_verify_and_state`, ONE cell, no compaction needed):
//! a note is born (op 7), purged (op 13), and a LOCAL `put` with the same
//! id (op 46) recreates it in the live state — while the replay applies
//! the v0.6.7 rule («the purge dominates the set: the note does not
//! materialize») and does not. The live state stops being a function of
//! the operations, and `verify` is red.
//!
//! The guard of v0.6.7 covers the ARRIVAL and the REPLAY, not the LOCAL
//! write. With Homun the path is real: re-importing an old export or a
//! backup that holds the purged note does exactly that `put`.
//!
//! The rule, one for write, arrival and replay: when the set holds a
//! purge of note X, EVERY local write that has X as its object is
//! refused with a typed error (`CellError::NotePurged`, `NOTE_PURGED`
//! in JS, the id in the message). «Forgotten forever» means the id does
//! not come back: to write similar content again takes a NEW note, with
//! a new id.

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use time::OffsetDateTime;
use ulid::Ulid;

fn at(day: u8) -> At {
    At(
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
            .unwrap(/* test-only */),
    )
}

fn note_of(id: u16, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some(format!("titolo {id}")),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: at(1),
        updated_at: at(1),
        extensions: vec![],
    }
}

/// The seed `1207186286826530307` reduced by hand: one note, one purge,
/// one local `put` of the same id. The put must be refused, and the
/// state must stay a function of the operations (`verify` green).
#[test]
fn a_local_put_on_a_purged_id_is_refused_seed_1207() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("oblio-torno.sqlite");
    let cell = Cell::create(&path, "oblio", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "quokkaviolaceo zibellinotremante");
    let id = secret.id;
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    cell.purge(&id, "diritto all'oblio", "tester")
        .unwrap(/* test-only */);

    // the same id, from an old export or a backup: the write the old
    // engines accepted — the state must not accept it
    let again = note_of(1, "quokkaviolaceo zibellinotremante ritorno");
    let written = cell.put_note(&again, "tester");
    assert!(
        written.is_err(),
        "a local put on a purged id recreates the note the user asked to forget"
    );

    // the live state is a function of the operations again
    let report = cell.verify().unwrap(/* test-only */);
    assert!(
        report.ok(),
        "verify after the refused put: {:?}",
        report.mismatch
    );
}

/// A NEW note with a new id still writes: the refusal is about the id,
/// not about writing after a purge.
#[test]
fn a_new_note_after_a_purge_writes_normally() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("oblio-nuovo.sqlite");
    let cell = Cell::create(&path, "oblio", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "quokkaviolaceo zibellinotremante");
    let purged = secret.id;
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    cell.purge(&purged, "diritto all'oblio", "tester")
        .unwrap(/* test-only */);

    let fresh = note_of(2, "passeggiata lungo larghissimo fiume");
    cell.put_note(&fresh, "tester").unwrap(/* test-only */);
    assert_eq!(
        cell.get(&fresh.id).unwrap(/* test-only */).body,
        fresh.body,
        "a new id writes normally after a purge"
    );
    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// The import does not stop on a purged note: the other files enter,
/// that one is NAMED in the report, and `verify` stays green. The
/// scenario is the real one — the export was taken BEFORE the purge,
/// and is re-imported after it.
#[test]
fn an_old_export_skips_the_purged_note_and_names_it() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("oblio-import.sqlite");
    let cell = Cell::create(&path, "oblio", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "quokkaviolaceo zibellinotremante");
    let purged = secret.id;
    let innocent = note_of(2, "passeggiata lungo larghissimo fiume");
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    cell.put_note(&innocent, "tester").unwrap(/* test-only */);
    let out = dir.path().join("export");
    std::fs::create_dir(&out).unwrap(/* test-only */);
    engramma_core::export(&cell, &out).unwrap(/* test-only */);
    cell.purge(&purged, "diritto all'oblio", "tester")
        .unwrap(/* test-only */);

    let summary = engramma_core::import_cell(&cell, &out, "tester")
        .unwrap(/* test-only */);
    assert!(
        summary.errors.is_empty(),
        "the import does not fail on a purged note: {:?}",
        summary.errors
    );
    // today the re-imported purged note COMES BACK in the live state
    // while the replay refuses it: this is the red
    let report = cell.verify().unwrap(/* test-only */);
    assert!(
        report.ok(),
        "verify after re-importing an old export that holds a purged note: {:?}",
        report.mismatch
    );
}
