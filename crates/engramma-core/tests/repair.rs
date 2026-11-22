// SPDX-License-Identifier: Apache-2.0

//! v0.6.10 (J2): `verify` is read-only and the repair of the «phantom»
//! notes (a living row on a purged id, written by the v0.6.7/v0.6.8
//! engines) is an EXPLICIT step. A diagnosis changes no state: verify
//! FINDS the phantoms and says the cell needs repair; `Cell::repair()`
//! moves each phantom's content to a NEW id with a normal `put`, then
//! applies the forgetting rule to the old id — one transaction per
//! phantom, so an interruption leaves every note either as it was or
//! repaired, never halfway. The old id → new id map goes back to the
//! caller and is saved nowhere: the forgotten id must not stay written
//! anywhere new.

use engramma_core::Cell;
use engramma_core::search::Query;
use ulid::Ulid;

/// The v0.6.8 fixture (`tests/golden/v068-put-after-purge.sqlite`): one
/// note (…895) purged, then the local `put` of the same id the v0.6.8
/// engine accepted — the phantom — and a living control note (…2).
fn phantom_stage(name: &str) -> (tempfile::TempDir, Cell, Ulid, Ulid) {
    let dir = test_dir();
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/v068-put-after-purge.sqlite");
    let path = dir.path().join(name);
    std::fs::copy(&golden, &path).unwrap(/* test-only */);
    let phantom = Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), 895))
        .unwrap(/* fixture */);
    let alive = Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), 2))
        .unwrap(/* fixture */);
    let cell = Cell::open(&path).unwrap(/* test-only */);
    (dir, cell, phantom, alive)
}

/// The bytes of the cell file (and the WAL, when one exists) — what
/// «verify touches nothing» is judged on.
fn file_bytes(dir: &std::path::Path, name: &str) -> Vec<Vec<u8>> {
    let mut out = vec![std::fs::read(dir.join(name)).unwrap(/* ours */)];
    let wal = dir.join(format!("{name}-wal"));
    if wal.is_file() {
        out.push(std::fs::read(&wal).unwrap(/* ours */));
    }
    out
}

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

/// v0.6.10 (J2): verify on a cell with a phantom changes NOTHING — the
/// rows, the digest and the pages are identical before and after — and
/// the report NAMES the phantom while the outcome says the cell needs
/// repair. (v0.6.9's verify repaired in place: `purge_state` +
/// `scrub_log` + `purge_conflict_copies` under `secure_delete`,
/// irreversible, announced only by the `phantom_purged` field — a
/// diagnosis must not destroy, and any future defect leaving a row on a
/// purged id must ALARM, not silently lose the user's content.)
#[test]
fn verify_touches_nothing_and_names_the_phantom() {
    let (dir, cell, phantom, _alive) = phantom_stage("verify-lettura.sqlite");

    // the phantom is the live state the v0.6.8 engine left
    assert!(
        cell.note_ids_all().unwrap(/* t */).contains(&phantom),
        "the fixture holds the phantom row"
    );
    // settle the connection first: the comparison judges what verify
    // itself writes, not what opening the cell does
    let _ = cell.note_ids_all().unwrap(/* t */);
    let before = file_bytes(dir.path(), "verify-lettura.sqlite");

    let report = cell.verify().unwrap(/* test-only */);
    assert_eq!(
        report.phantom_purged,
        vec![phantom.to_string()],
        "the report names the phantom it FOUND"
    );
    assert!(
        !report.ok(),
        "a cell with a phantom needs repair: the outcome says so"
    );

    // nothing moved: the phantom is still the live state…
    assert!(
        cell.note_ids_all().unwrap(/* t */).contains(&phantom),
        "verify left the phantom where it was"
    );
    // …and the file is byte for byte the same, pages and digest
    // included (judged with the cell still open: closing it may
    // checkpoint a pre-existing WAL, and the close is not verify's act)
    let after = file_bytes(dir.path(), "verify-lettura.sqlite");
    assert_eq!(
        before, after,
        "verify is read-only: the cell file must not change"
    );
    drop(cell);
}

/// v0.6.10 (J2): the explicit repair of a v0.6.8-written cell. The
/// phantom's content lives under a NEW id (a normal `put`, actor
/// `engramma:repair`), the old id is forgotten by the rule (state rows,
/// closure over the copies, log scrub, `secure_delete`), the map old →
/// new goes back to the caller, and verify is green after. Repairing
/// twice does nothing the second time.
#[test]
fn repair_moves_the_phantom_content_under_a_new_id() {
    let (dir, cell, phantom, alive) = phantom_stage("riparata.sqlite");

    let report = cell.repair().unwrap(/* test-only */);
    assert_eq!(report.moved.len(), 1, "one phantom moved: {report:?}");
    let (old, new) = report.moved[0].clone();
    assert_eq!(old, phantom.to_string());
    let new = Ulid::from_string(&new).unwrap(/* our own put */);
    assert_ne!(new, phantom, "the content lives under a NEW id");

    // the content survives, whole, under the new id…
    let moved = cell.get(&new).unwrap(/* t */);
    assert_eq!(moved.body, "quokkaviolaceo zibellinotremante ritorno");
    assert_eq!(moved.title.as_deref(), Some("la nota che non torna"));
    // …the old id is gone from every read…
    assert!(!cell.note_ids_all().unwrap(/* t */).contains(&phantom));
    assert!(cell.get(&phantom).is_err());
    assert!(
        cell.search(&Query::text("quokkaviolaceo").k(10))
            .unwrap(/* t */)
            .iter()
            .all(|h| h.note.id != phantom),
        "the search answers the NEW id only"
    );
    assert!(
        cell.search(&Query::text("quokkaviolaceo").k(10))
            .unwrap(/* t */)
            .iter()
            .any(|h| h.note.id == new),
        "the moved content is findable"
    );
    // …the living control note is untouched…
    assert_eq!(
        cell.get(&alive).unwrap(/* t */).body,
        "passeggiata lungo larghissimo fiume"
    );
    // …and the repair declared itself in the log
    let actors = cell
        .debug_column(&format!(
            "SELECT actor FROM ops WHERE kind = 'put_note' AND note_id = '{new}'"
        ))
        .unwrap(/* t */);
    assert_eq!(actors, vec!["engramma:repair".to_string()]);

    // the old id is written nowhere it was not already: the ops skeleton
    // keeps it (the purge's own audit row, as for every purge), but no
    // content table does
    for sql in [
        format!("SELECT 1 FROM notes WHERE id = '{phantom}'"),
        format!("SELECT 1 FROM note_versions WHERE note_id = '{phantom}'"),
        format!("SELECT 1 FROM note_tags WHERE note_id = '{phantom}'"),
        format!("SELECT 1 FROM edges WHERE note_id = '{phantom}'"),
        format!("SELECT 1 FROM supersedes WHERE new_id = '{phantom}' OR old_id = '{phantom}'"),
        format!(
            "SELECT 1 FROM op_payloads p JOIN ops o ON o.op_id = p.op_id
                  WHERE o.note_id = '{phantom}' AND p.body IS NOT NULL"
        ),
    ] {
        assert!(
            cell.debug_column(&sql).unwrap(/* t */).is_empty(),
            "the old id lives on: {sql}"
        );
    }

    // verify is green after the repair, and repairing again does nothing
    let check = cell.verify().unwrap(/* t */);
    assert!(check.ok(), "{:?}", check.mismatch);
    assert!(check.phantom_purged.is_empty());
    let again = cell.repair().unwrap(/* t */);
    assert!(again.moved.is_empty(), "the repair is once: {again:?}");

    // closing and reopening keeps the promise (the file on disk is the
    // state, not the memory)
    drop(cell);
    let cell = Cell::open(&dir.path().join("riparata.sqlite")).unwrap(/* t */);
    let check = cell.verify().unwrap(/* t */);
    assert!(check.ok(), "{:?}", check.mismatch);
}

/// v0.6.10 (J2): the interruption — a repair stopped after the first
/// phantom, then relaunched, ends clean. One transaction per phantom:
/// every note is either as it was or repaired, never halfway.
#[test]
fn an_interrupted_resumable_repair_ends_clean() {
    let (dir, cell, phantom, _alive) = phantom_stage("interrotta.sqlite");

    // a SECOND phantom, crafted as the v0.6.8 engine would have left
    // it: a living row on a purged id. The fixture's own purge covers
    // only …895, so the cell gets a purge of its own first (a real one,
    // through the engine: the log stays honest), then the row the old
    // engine would have accepted — a clone of the first phantom, on an
    // id that sorts AFTER it (the repair walks the phantoms in order)
    let second = Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), 999))
        .unwrap(/* fixture id space */);
    {
        use engramma_core::format::{Lang, NoteType};
        let mut fresh = note(999, "secondo fantasma zibellinotremante");
        fresh.note_type = NoteType::Fact;
        fresh.lang = Some(Lang::It);
        cell.put_note(&fresh, "tester").unwrap(/* t */);
        cell.purge(&second, "diritto all'oblio", "tester").unwrap(/* t */);
        let conn = rusqlite::Connection::open(dir.path().join("interrotta.sqlite"))
            .unwrap(/* t */);
        conn.execute(
            "INSERT INTO notes (id, type, title, body, frontmatter_json, scope, source,
                                confidence, status, valid_from, valid_to, created_at,
                                updated_at, version, body_norm)
             SELECT ?1, type, title, body, frontmatter_json, scope, source,
                    confidence, 'active', valid_from, valid_to, created_at,
                    updated_at, 1, body_norm
             FROM notes WHERE id = ?2",
            rusqlite::params![second.to_string(), phantom.to_string()],
        )
        .unwrap(/* t */);
    }
    // reopen: the crafted row is visible
    drop(cell);
    let cell = Cell::open(&dir.path().join("interrotta.sqlite")).unwrap(/* t */);
    let ids = cell.note_ids_all().unwrap(/* t */);
    assert!(
        ids.contains(&phantom) && ids.contains(&second),
        "two phantoms"
    );

    // the interruption: stop after the first phantom (the test-only
    // injection, same discipline as the purge's FTS one)
    let stopped = {
        // SAFETY: the test runner is single-threaded here (the injection
        // is read once, at repair's start)
        unsafe {
            std::env::set_var("ENGRAMMA_TEST_REPAIR_STOP_AFTER", "1");
        }
        let r = cell.repair();
        unsafe {
            std::env::remove_var("ENGRAMMA_TEST_REPAIR_STOP_AFTER");
        }
        r.unwrap(/* t */)
    };
    assert_eq!(
        stopped.moved.len(),
        1,
        "one phantom repaired, then the stop"
    );
    let (old, new) = stopped.moved[0].clone();
    assert_eq!(old, phantom.to_string(), "the phantoms go in order");
    // the FIRST is whole under its new id, the SECOND is exactly as it was
    assert!(
        cell.get(&Ulid::from_string(&new).unwrap(/* ours */))
            .is_ok(),
        "the first phantom's content lives"
    );
    assert!(
        cell.note_ids_all().unwrap(/* t */).contains(&second),
        "the second phantom waits, untouched"
    );

    // the relaunch finishes the work
    let done = cell.repair().unwrap(/* t */);
    assert_eq!(done.moved.len(), 1, "only the waiting one: {done:?}");
    assert_eq!(done.moved[0].0, second.to_string());
    let check = cell.verify().unwrap(/* t */);
    assert!(check.ok(), "{:?}", check.mismatch);
}

/// v0.6.10 (J1's own leftover): a cell written by the v0.6.9 engine may
/// hold a row born from a half that no longer replays — the resurrected
/// `supersedes` row of a decide over a purged proposal. verify goes red
/// on it, and the SAME explicit repair settles it: the reconciliation
/// re-derives the co-objects of the dominated operations, and the state
/// becomes a function of the operations again.
#[test]
fn repair_settles_the_j1_leftover_of_an_old_cell() {
    let dir = test_dir();
    let path = dir.path().join("residuo.sqlite");
    let cell = Cell::create(&path, "residuo", "tester").unwrap(/* t */);
    let target = note(1, "la nota che il mondo vede");
    let mut proposal = note(2, "la rettifica che la corregge");
    proposal.status = engramma_core::Status::Quarantined;
    cell.put_note(&target, "tester").unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "tester")
        .unwrap(/* t */);
    cell.propose(
        &engramma_core::cell::proposals::ProposalInput {
            note: proposal,
            mode: engramma_core::cell::proposals::ProposalMode::Supersede(target.id),
            client: None,
            reason: None,
        },
        "mcp:tester",
        &engramma_core::cell::proposals::ProposeContext::default(),
    )
    .unwrap(/* t */);
    cell.decide(
        &proposal_id(2),
        &engramma_core::cell::proposals::DecisionInput {
            action: engramma_core::cell::proposals::DecisionAction::Accept { edited: None },
            reason: Some("accettata".into()),
            review: None,
        },
        "tester",
    )
    .unwrap(/* t */);
    cell.purge(&proposal_id(2), "diritto all'oblio", "tester")
        .unwrap(/* t */);
    let check = cell.verify().unwrap(/* t */);
    assert!(check.ok(), "the fixed engine writes no leftover: {check:?}");

    // the v0.6.9 engine's residue, crafted: the supersedes row the old
    // materialization used to resurrect
    {
        let conn = rusqlite::Connection::open(&path).unwrap(/* t */);
        conn.execute(
            "INSERT INTO supersedes (new_id, old_id) VALUES (?1, ?2)",
            rusqlite::params![proposal_id(2).to_string(), target.id.to_string()],
        )
        .unwrap(/* t */);
    }
    drop(cell);
    let cell = Cell::open(&path).unwrap(/* t */);
    let red = cell.verify().unwrap(/* t */);
    assert!(
        !red.ok(),
        "the leftover diverges the replay: {:?}",
        red.mismatch
    );

    // the repair settles it: the row that no half would write goes, the
    // state returns to be a function of the operations
    let report = cell.repair().unwrap(/* t */);
    assert!(report.moved.is_empty(), "no phantom here: {report:?}");
    let green = cell.verify().unwrap(/* t */);
    assert!(green.ok(), "{:?}", green.mismatch);
}

fn at(day: u8) -> engramma_core::format::At {
    engramma_core::format::At(
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
            .unwrap(/* test-only */),
    )
}

fn note(id: u16, body: &str) -> engramma_core::format::Note {
    use engramma_core::format::{Lang, Note, NoteType};
    Note {
        id: proposal_id(id),
        note_type: NoteType::Fact,
        title: Some(format!("titolo {id}")),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: engramma_core::Confidence::Stated,
        status: engramma_core::Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: at(1),
        updated_at: at(1),
        extensions: vec![],
    }
}

fn proposal_id(n: u16) -> Ulid {
    Ulid::from_string(&format!("01{:024}", n)).unwrap(/* test-only */)
}
