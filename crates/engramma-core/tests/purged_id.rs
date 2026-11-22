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

use engramma_core::cell::EdgeInput;
use engramma_core::cell::proposals::{ProposalInput, ProposalMode, ProposeContext};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, CellError, Confidence, NotePatch, Status};
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

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

#[test]
fn a_local_put_on_a_purged_id_is_refused_seed_1207() {
    let dir = test_dir();
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
    match cell.put_note(&again, "tester") {
        Err(CellError::NotePurged(id)) => assert_eq!(id, again.id.to_string()),
        other => panic!(
            "a local put on a purged id must answer NotePurged (the id in \
             the message); got {other:?}"
        ),
    }

    // the live state is a function of the operations again
    let report = cell.verify().unwrap(/* test-only */);
    assert!(
        report.ok(),
        "verify after the refused put: {:?}",
        report.mismatch
    );
}

/// One stage for the per-operation walk: a cell with a purged id and a
/// LIVING control note. The purged id is `1`, the living one is `2`.
fn stage() -> (tempfile::TempDir, Cell, Ulid, Ulid) {
    let dir = test_dir();
    let cell = Cell::create(&dir.path().join("stage.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let secret = note_of(1, "quokkaviolaceo zibellinotremante");
    let purged = secret.id;
    let alive_note = note_of(2, "passeggiata lungo larghissimo fiume");
    let alive = alive_note.id;
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    cell.put_note(&alive_note, "tester").unwrap(/* test-only */);
    cell.purge(&purged, "diritto all'oblio", "tester")
        .unwrap(/* test-only */);
    (dir, cell, purged, alive)
}

/// The rule's word for one id: `NotePurged` with that id in the message.
fn assert_purged(result: Result<(), CellError>, id: &Ulid, what: &str) {
    match result {
        Err(CellError::NotePurged(named)) => {
            assert_eq!(named, id.to_string(), "{what}: the message carries the id")
        }
        other => panic!("{what} on a purged id must answer NotePurged; got {other:?}"),
    }
}

/// Every operation of the list, one refusal each — plus the two the
/// phase asked to ADD when found (a `delete` on the purged id: it has
/// X as its object like the others; a `decide` whose supersede target
/// is X: the decision would close a note the set forgot).
#[test]
fn every_local_write_that_touches_a_purged_id_is_refused() {
    let (_dir, cell, purged, alive) = stage();

    assert_purged(
        cell.patch_note(
            &purged,
            &NotePatch {
                body: Some("ritorno".into()),
                ..Default::default()
            },
            "tester",
        ),
        &purged,
        "patch",
    );
    assert_purged(
        cell.set_status(&purged, Status::Active, "tester"),
        &purged,
        "setStatus",
    );
    assert_purged(cell.restore(&purged, 1, "tester"), &purged, "restore");
    assert_purged(cell.confirm(&purged, "tester"), &purged, "confirm");
    // added to the phase's list: delete has X as its object too
    assert_purged(cell.delete_note(&purged, "tester"), &purged, "delete");

    // supersede: closing X…
    let mut replacement = note_of(3, "nuova nota che rimpiazza");
    assert_purged(
        cell.supersede(&purged, &replacement, None, "tester"),
        &purged,
        "supersede that closes X",
    );
    // …and CREATING X as the new note over a living old one
    replacement.id = purged;
    assert_purged(
        cell.supersede(&alive, &replacement, None, "tester"),
        &purged,
        "supersede that creates X",
    );

    // link: from X, towards X, and citing X
    let edge_from = EdgeInput {
        src: format!("note:{purged}"),
        rel: "rel_x".into(),
        dst: "Beta".into(),
        note_id: None,
        valid_from: None,
        valid_to: None,
        confidence: Confidence::Stated,
    };
    assert_purged(
        cell.link(&edge_from, "tester").map(|_| ()),
        &purged,
        "link from X",
    );
    let edge_to = EdgeInput {
        src: "Alfa".into(),
        rel: "rel_x".into(),
        dst: format!("note:{purged}"),
        note_id: None,
        valid_from: None,
        valid_to: None,
        confidence: Confidence::Stated,
    };
    assert_purged(
        cell.link(&edge_to, "tester").map(|_| ()),
        &purged,
        "link towards X",
    );
    let edge_citing = EdgeInput {
        src: "Alfa".into(),
        rel: "rel_x".into(),
        dst: "Beta".into(),
        note_id: Some(purged),
        valid_from: None,
        valid_to: None,
        confidence: Confidence::Stated,
    };
    assert_purged(
        cell.link(&edge_citing, "tester").map(|_| ()),
        &purged,
        "link that cites X",
    );
    // unlink: the guard reads the edge's endpoints. An edge between two
    // LIVING notes still unlinks (no false refusal)…
    let good_edge = EdgeInput {
        src: "Alfa".into(),
        rel: "rel_x".into(),
        dst: "Beta".into(),
        note_id: Some(alive),
        valid_from: None,
        valid_to: None,
        confidence: Confidence::Stated,
    };
    let edge_id = cell.link(&good_edge, "tester").unwrap(/* control */);
    cell.unlink(&edge_id, "tester")
        .unwrap(/* control: a living edge unlinks */);

    // propose on X (the cell must accept proposals for the refusal to
    // be the rule's word, not the policy's)
    cell.set_policy(serde_json::json!({ "write": "propose" }), "tester")
        .unwrap(/* test-only */);
    let proposal = note_of(4, "proposta che corregge");
    let input = ProposalInput {
        note: proposal,
        mode: ProposalMode::Supersede(purged),
        client: None,
        reason: None,
    };
    assert_purged(
        cell.propose(&input, "tester", &ProposeContext::default())
            .map(|_| ()),
        &purged,
        "propose on X",
    );

    // the control note is untouched by all those refusals
    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
    assert_eq!(
        cell.get(&alive).unwrap(/* test-only */).body,
        "passeggiata lungo larghissimo fiume"
    );
}

/// The purge itself is NOT refused — on the purged id (forgetting an
/// already-forgotten id is possible, as it is for a tombstone) nor on a
/// tombstone. The rule closes the writers, not the forgetting.
#[test]
fn the_purge_still_works_on_a_purged_id_and_on_tombstones() {
    let (_dir, cell, purged, alive) = stage();
    let report = cell.purge(&purged, "di nuovo", "tester").unwrap(/* t */);
    assert!(report.scrubbed, "{report:?}");

    cell.delete_note(&alive, "tester").unwrap(/* t */);
    let report = cell.purge(&alive, "oblio della lapide", "tester")
        .unwrap(/* t */);
    assert!(report.scrubbed, "{report:?}");
    let verify = cell.verify().unwrap(/* t */);
    assert!(verify.ok(), "{:?}", verify.mismatch);
}

/// The `revert_decision` of a decision whose content was purged keeps
/// ITS OWN typed answer (v0.5, `RevertOutcome::Purged`): the revert says
/// «purged, nothing comes back» and writes the audit op — it never
/// resurrects. The guard exempts it (with the purge) because
/// `classify_revert` has already refused every path that could return
/// the content; this test locks the contract.
#[test]
fn the_revert_of_purged_content_keeps_its_own_answer() {
    use engramma_core::cell::proposals::{ProposalInput, ProposalMode, ProposeContext};
    let (_dir, cell, _purged, _alive) = stage();
    cell.set_policy(serde_json::json!({ "write": "propose" }), "tester")
        .unwrap(/* t */);
    let proposal = note_of(4, "Fabio venderebbe la casa.");
    let out = cell
        .propose(
            &ProposalInput {
                note: proposal,
                mode: ProposalMode::New,
                client: None,
                reason: None,
            },
            "tester",
            &ProposeContext::default(),
        )
        .unwrap(/* t */);
    let id = out.id.unwrap(/* t */);
    cell.decide(
        &id,
        &engramma_core::DecisionInput {
            action: engramma_core::DecisionAction::Accept { edited: None },
            reason: None,
            review: None,
        },
        "tester",
    )
    .unwrap(/* t */);
    // the decision row is read BEFORE the purge (the purge takes it with
    // the note — that is part of what forgetting means)
    let decision = cell
        .decisions(&engramma_core::DecisionsFilter::default())
        .unwrap(/* t */)[0]
        .op_id;
    cell.purge(&id, "diritto all'oblio", "tester").unwrap(/* t */);
    let outcome = cell.revert_decision(&decision, "troppo tardi", "tester")
        .unwrap(/* t */);
    assert!(
        matches!(outcome, engramma_core::RevertOutcome::Purged { .. }),
        "the revert answers Purged, not NotePurged: {outcome:?}"
    );
    assert!(cell.get(&id).is_err(), "nothing came back");
    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// v0.6.9 (I1) found them, v0.6.10 (J2) made the step explicit: a cell
/// the v0.6.8 ENGINE wrote — `tests/golden/v068-put-after-purge.sqlite`,
/// built by a scratch example in a worktree on the tag (`fase-v0.6.8`),
/// with a fixed clock and mint: one note (…895), one purge, then the
/// local `put` of the same id the v0.6.8 engine accepted (its own
/// `verify` on it answers `ok=false` — the defect, captured). Today's
/// engine opens it, `verify` FINDS the phantom and says the cell needs
/// repair without touching a byte, and `repair()` moves the content
/// under a new id (the full contract is `tests/repair.rs`; this test
/// keeps the historical fixture at the point that declared it).
#[test]
fn a_v068_cell_with_a_put_after_a_purge_signals_and_repairs_explicitly() {
    let dir = test_dir();
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/v068-put-after-purge.sqlite");
    let path = dir.path().join("v068-fantasma.sqlite");
    std::fs::copy(&golden, &path).unwrap(/* test-only */);
    let phantom =
        Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), 895)).unwrap(/* fixture */);
    let alive = Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), 2)).unwrap(/* fixture */);

    let cell = Cell::open(&path).unwrap(/* test-only */);
    assert!(
        cell.note_ids_all().unwrap(/* t */).contains(&phantom),
        "the fixture holds the phantom row"
    );
    // verify is a diagnosis: it names the phantom, it says the cell
    // needs repair, and it changes nothing
    let report = cell.verify().unwrap(/* test-only */);
    assert!(!report.ok(), "the outcome says the cell needs repair");
    assert_eq!(
        report.phantom_purged,
        vec![phantom.to_string()],
        "the report names the phantom it found"
    );
    assert!(
        cell.note_ids_all().unwrap(/* t */).contains(&phantom),
        "the diagnosis left the state alone"
    );
    // the repair moves the content under a new id, and verify goes green
    let repaired = cell.repair().unwrap(/* test-only */);
    assert_eq!(
        repaired.moved,
        vec![(phantom.to_string(), repaired.moved[0].1.clone())],
        "one phantom, moved"
    );
    let check = cell.verify().unwrap(/* test-only */);
    assert!(check.ok(), "{:?}", check.mismatch);
    // the living control note is untouched through it all
    assert_eq!(
        cell.get(&alive).unwrap(/* t */).body,
        "passeggiata lungo larghissimo fiume"
    );
}

/// A NEW note with a new id still writes: the refusal is about the id,
/// not about writing after a purge.
#[test]
fn a_new_note_after_a_purge_writes_normally() {
    let dir = test_dir();
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
    let dir = test_dir();
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
    // the skipped note is NAMED: the id list is the report's contract
    // (`skippedPurged` in the CLI's JSON)
    assert_eq!(
        summary.skipped_purged,
        vec![purged.to_string()],
        "the report names the purged id it skipped"
    );
    assert!(
        summary
            .skipped
            .iter()
            .any(|s| s.contains(&purged.to_string())),
        "the skip line names the id: {:?}",
        summary.skipped
    );
    let report = cell.verify().unwrap(/* test-only */);
    assert!(
        report.ok(),
        "verify after re-importing an old export that holds a purged note: {:?}",
        report.mismatch
    );
}

/// The Obsidian import speaks the same word: a file whose header carries
/// a purged id is skipped and named, the others enter.
#[test]
fn an_obsidian_folder_skips_the_purged_note_and_names_it() {
    let dir = test_dir();
    let path = dir.path().join("oblio-obsidian.sqlite");
    let cell = Cell::create(&path, "oblio", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "quokkaviolaceo zibellinotremante");
    let purged = secret.id;
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    cell.purge(&purged, "diritto all'oblio", "tester")
        .unwrap(/* test-only */);

    let vault = dir.path().join("vault");
    std::fs::create_dir(&vault).unwrap(/* test-only */);
    // the old file, with its Engramma header carrying the purged id
    let text = format!(
        "---\nid: \"{purged}\"\ntype: fact\nsource: user\nconfidence: stated\n\
         created_at: 2027-01-16T08:00:00Z\nupdated_at: 2027-01-16T08:00:00Z\n---\n\n\
         quokkaviolaceo zibellinotremante\n"
    );
    std::fs::write(vault.join("vecchia.md"), text).unwrap(/* test-only */);
    std::fs::write(vault.join("nuova.md"), "appunto nuovo del vault\n")
        .unwrap(/* test-only */);

    let summary = engramma_core::import_obsidian(&cell, &vault, "tester")
        .unwrap(/* test-only */);
    assert_eq!(summary.imported, 1, "the other file enters");
    assert!(
        summary.errors.is_empty(),
        "the import does not fail on a purged note: {:?}",
        summary.errors
    );
    assert_eq!(
        summary.skipped_purged,
        vec![purged.to_string()],
        "the report names the purged id it skipped"
    );
    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}
