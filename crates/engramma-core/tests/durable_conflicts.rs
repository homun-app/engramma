// SPDX-License-Identifier: Apache-2.0

//! v0.6.4 (D1): the conflict copy is DURABLE and only the body creates
//! conflicts. The tests speak of the OBSERVABLE state — the copies'
//! notes rows, the queue, the original's body — so they run red against
//! the v0.6.3 rule and stay green under the new one, whatever the
//! arrival order.

mod common;

use common::*;
use engramma_core::cell::ops::Op;
use engramma_core::{Cell, NotePatch, ResolveKeep};
use std::sync::atomic::Ordering;
use ulid::Ulid;

/// D1: a title patch on the original does NOT close the copy. The copy
/// is durable: it closes only with a witness (an operation on its id, or
/// a body write that contains it) — and a header write is neither.
#[test]
fn a_title_patch_on_the_original_does_not_close_the_copy() {
    let init = shared_init("titolo");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(1, "alpha\nbeta\ngamma");
    let id = note.id;

    // B (an agent) changes the body; A (a human, later) changes it too —
    // concurrent, both saw the put
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "user").unwrap(/* t */);
    let put: Vec<Op> =
        a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
    b.ingest_op(&put[0]).unwrap(/* t */);
    wb.store(K + 2, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("alpha\nX\ngamma".into()), ..Default::default() },
        "homun:bot-7",
    )
    .unwrap(/* t */);
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch { body: Some("il corpo della mano umana".into()), ..Default::default() },
        "user",
    )
    .unwrap(/* t */);
    // the two settle: the human body won, the agent's copy is open
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    assert_eq!(
        open_copies(&a, &id).len(),
        1,
        "the agent's copy is open on A"
    );
    assert_eq!(
        open_copies(&b, &id).len(),
        1,
        "the agent's copy is open on B"
    );

    // A changes ONLY the title of the original: the copy must stay
    wa.store(K + 4, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            title: Some(Some("il titolo ritoccato".into())),
            ..Default::default()
        },
        "user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for cell in [&a, &b] {
        let copies = open_copies(cell, &id);
        assert_eq!(copies.len(), 1, "a title patch does not close the copy");
        assert!(
            copies[0].1.contains("alpha\nX\ngamma"),
            "the copy still holds the agent's body: {:?}",
            copies
        );
        let title: Vec<String> = cell
            .debug_column(&format!(
                "SELECT COALESCE(title, '') FROM notes WHERE id = '{id}'"
            ))
            .unwrap(/* t */);
        assert_eq!(title[0], "il titolo ritoccato", "the title patch did land");
    }
}

/// D1: a concurrent TITLE does not create a spurious copy. Only the body
/// creates conflicts; the header is last-write-per-field and never
/// produces a copy. The original keeps the agent's body and the human's
/// (later) title.
#[test]
fn a_concurrent_title_does_not_create_a_spurious_copy() {
    let init = shared_init("spurio");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(2, "alpha\nbeta\ngamma");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "user").unwrap(/* t */);
    let put: Vec<Op> =
        a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
    b.ingest_op(&put[0]).unwrap(/* t */);

    // B (agent) changes the body, A (human, LATER in the total order)
    // changes only the title — concurrent
    wb.store(K + 2, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("alpha\nX\ngamma".into()), ..Default::default() },
        "homun:bot-7",
    )
    .unwrap(/* t */);
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            title: Some(Some("il titolo della mano umana".into())),
            ..Default::default()
        },
        "user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for cell in [&a, &b] {
        assert!(
            open_copies(cell, &id).is_empty(),
            "a concurrent title creates no copy: {:?}",
            open_copies(cell, &id)
        );
        let row: Vec<String> = cell
            .debug_column(&format!(
                "SELECT COALESCE(title, '') || '' || body FROM notes WHERE id = '{id}'"
            ))
            .unwrap(/* t */);
        assert_eq!(
            row[0], "il titolo della mano umanaalpha\nX\ngamma",
            "the human's title (later) and the agent's body: no copy, no loss"
        );
    }
}

/// D1.6(b): a hand merge closes the copy on every device — the winner
/// CONTAINS the loser's change, the rigorous diff3 from the fork sees
/// it — and deleting the merged phrase LATER does not reopen it: the
/// witness is an existence in the set.
#[test]
fn a_hand_merge_closes_the_copy_and_a_later_delete_does_not_reopen_it() {
    let init = shared_init("fusione");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(3, "alpha\nbeta\ngamma");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "user").unwrap(/* t */);
    let put: Vec<Op> =
        a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
    b.ingest_op(&put[0]).unwrap(/* t */);

    // the agent's branch: change the middle line; the human's concurrent
    // branch: something else entirely — the human wins, the agent's copy
    // opens
    wb.store(K + 2, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("alpha\nX\ngamma".into()), ..Default::default() },
        "homun:bot-7",
    )
    .unwrap(/* t */);
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch { body: Some("tre righe del tutto diverse".into()), ..Default::default() },
        "user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    assert_eq!(
        open_copies(&a, &id).len(),
        1,
        "the copy is open before the merge"
    );

    // the human merges BY HAND: from the fork's own body, the same
    // middle-line change plus a new tail — the diff3 contains the loser
    wa.store(K + 4, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("alpha\nX\ngamma\ndelta".into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for cell in [&a, &b] {
        assert!(
            open_copies(cell, &id).is_empty(),
            "the hand merge closed the copy everywhere: {:?}",
            open_copies(cell, &id)
        );
        let body: Vec<String> = cell
            .debug_column(&format!("SELECT body FROM notes WHERE id = '{id}'"))
            .unwrap(/* t */);
        assert_eq!(body[0], "alpha\nX\ngamma\ndelta");
    }

    // the merged phrase goes away: the copy does NOT come back
    wa.store(K + 5, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch { body: Some("alpha\ngamma".into()), ..Default::default() },
        "user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for cell in [&a, &b] {
        assert!(
            open_copies(cell, &id).is_empty(),
            "the witness is an existence: deleting the phrase does not reopen the copy"
        );
    }
}

/// D3: `resolveConflict` in its three ways — `original` (the delete on
/// the copy's id), `copy` (the copy's body becomes the original's),
/// `merged` (the body given by hand) — and the two errors of the
/// contract: an unknown id is `NOT_FOUND`, a closed copy is
/// `CONFLICT`. Two concurrent resolutions with EQUAL bodies create
/// nothing new: the equal-body filter holds, and the second delete
/// lands on the copy's tombstone.
#[test]
fn resolve_conflict_in_the_three_ways_and_concurrent_equal_resolutions() {
    let mk = |tag: &str| -> (Kept, Kept, Ulid) {
        let init = shared_init(tag);
        let (a, wa) = cell_with("a");
        let (b, wb) = cell_with("b");
        a.ingest_op(&init).unwrap(/* t */);
        b.ingest_op(&init).unwrap(/* t */);
        let note = base_note(61, "alpha\nbeta\ngamma");
        let id = note.id;
        wa.store(K + 1, Ordering::SeqCst);
        a.put_note(&note, "user").unwrap(/* t */);
        let put: Vec<Op> = a
            .ops()
            .unwrap(/* t */)
            .into_iter()
            .filter(|o| o.kind.as_str() == "put_note")
            .collect();
        b.ingest_op(&put[0]).unwrap(/* t */);
        // the agent's body loses to the human's: the copy opens
        wb.store(K + 2, Ordering::SeqCst);
        b.patch_note(
            &id,
            &NotePatch {
                body: Some("alpha\nX\ngamma".into()),
                ..Default::default()
            },
            "homun:bot-7",
        )
        .unwrap(/* t */);
        wa.store(K + 3, Ordering::SeqCst);
        a.patch_note(
            &id,
            &NotePatch {
                body: Some("il corpo della mano umana".into()),
                ..Default::default()
            },
            "user",
        )
        .unwrap(/* t */);
        for op in &union(&a, &b) {
            a.ingest_op(op).unwrap(/* t */);
            b.ingest_op(op).unwrap(/* t */);
        }
        (a, b, id)
    };
    let open_ids = |cell: &Cell, id: &Ulid| -> Vec<String> {
        open_copies(cell, id)
            .into_iter()
            .map(|(i, _, _)| i)
            .collect()
    };

    // original: the copy closes, the original keeps its body
    let (a, _b, id) = mk("res-orig");
    let list = a.conflicts().unwrap(/* t */);
    assert_eq!(list.len(), 1);
    let copy = list[0].copy_id;
    assert_eq!(list[0].original_id, id);
    assert_eq!(list[0].loser_actor, "homun:bot-7");
    assert!(list[0].quarantined, "an agent's copy is in quarantine");
    assert!(list[0].visible, "one copy: visible");
    a.resolve_conflict(&copy, ResolveKeep::Original, None, "user").unwrap(/* t */);
    assert!(open_ids(&a, &id).is_empty(), "original: the copy closed");
    assert!(
        a.conflicts().unwrap(/* t */).is_empty(),
        "the list is empty after the resolution"
    );
    let body: Vec<String> = a
        .debug_column(&format!("SELECT body FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */);
    assert_eq!(body[0], "il corpo della mano umana");
    // resolving again: the copy is closed — CONFLICT
    match a.resolve_conflict(&copy, ResolveKeep::Original, None, "user") {
        Err(engramma_core::CellError::Conflict(_)) => {}
        other => panic!("a closed copy is a CONFLICT, got {other:?}"),
    }
    // an id that is no conflict copy at all — NOT_FOUND
    let stranger = Ulid::from_string("01AAAAAAAAAAAAAAAAAAAAAAAA").unwrap(/* t */);
    match a.resolve_conflict(&stranger, ResolveKeep::Original, None, "user") {
        Err(engramma_core::CellError::NoteNotFound(_)) => {}
        other => panic!("an unknown id is NOT_FOUND, got {other:?}"),
    }

    // copy: the copy's body becomes the original's
    let (a, b, id) = mk("res-copy");
    let copy = a.conflicts().unwrap(/* t */)[0].copy_id;
    a.resolve_conflict(&copy, ResolveKeep::Copy, None, "user").unwrap(/* t */);
    let body: Vec<String> = a
        .debug_column(&format!("SELECT body FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */);
    assert_eq!(
        body[0], "alpha\nX\ngamma",
        "copy: the loser's body won by hand"
    );
    assert!(open_ids(&a, &id).is_empty());
    // the same resolution, CONCURRENTLY, on the other device: equal
    // bodies — nothing new is born
    b.resolve_conflict(&copy, ResolveKeep::Copy, None, "user").unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for cell in [&a, &b] {
        assert!(
            cell.conflicts().unwrap(/* t */).is_empty(),
            "two equal concurrent resolutions create nothing"
        );
        assert_eq!(
            cell.replicated_digest().unwrap(/* t */),
            a.replicated_digest().unwrap(/* t */),
            "the two devices agree"
        );
    }

    // merged: the body given by hand
    let (a, _b, id) = mk("res-merged");
    let copy = a.conflicts().unwrap(/* t */)[0].copy_id;
    a.resolve_conflict(
        &copy,
        ResolveKeep::Merged,
        Some("alpha\nX e la frase della mano umana\ngamma"),
        "user",
    )
    .unwrap(/* t */);
    let body: Vec<String> = a
        .debug_column(&format!("SELECT body FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */);
    assert_eq!(body[0], "alpha\nX e la frase della mano umana\ngamma");
    assert!(open_ids(&a, &id).is_empty());
    // merged without a body is a usage error
    let (a2, _b2, _id2) = mk("res-merged-err");
    let copy2 = a2.conflicts().unwrap(/* t */)[0].copy_id;
    assert!(
        a2.resolve_conflict(&copy2, ResolveKeep::Merged, None, "user")
            .is_err()
    );
}

/// A note born from `propose`, patched concurrently (two baseless
/// patches — the note never had a body write before): the copy must
/// derive identically from the set. Found by the 20 000-case hunt.
#[test]
fn a_proposal_note_patch_concurrently_derives_the_copy() {
    let init = shared_init("prop");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    a.set_policy(serde_json::json!({ "write": "propose" }), "device-0")
        .unwrap(/* t */);
    // A proposes: the note is born quarantined, in the queue
    let out = a
        .propose(
            &engramma_core::cell::proposals::ProposalInput {
                note: base_note(81, "prima riga\nil corpo proposto\nterza riga"),
                mode: engramma_core::cell::proposals::ProposalMode::New,
                client: Some("test".into()),
                reason: Some("perché".into()),
            },
            "device-0",
            &engramma_core::cell::proposals::ProposeContext::default(),
        )
        .unwrap(/* t */);
    let c0 = out.id.expect(/* mode new */ "the proposal note");
    let ops_a: Vec<Op> =
        a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "propose").collect();
    b.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&ops_a[0]).unwrap(/* t */);

    // both patch the proposal note CONCURRENTLY (baseless: no op of that
    // note existed before)
    wa.store(K + 2, Ordering::SeqCst);
    a.patch_note(
        &c0,
        &NotePatch { body: Some("prima riga\nla mano di A\nterza riga".into()), ..Default::default() },
        "user",
    )
    .unwrap(/* t */);
    wb.store(K + 9, Ordering::SeqCst);
    b.patch_note(
        &c0,
        &NotePatch { body: Some("prima riga\nla mano di B\nterza riga".into()), ..Default::default() },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for cell in [&a, &b] {
        let copies = open_copies(cell, &c0);
        assert_eq!(copies.len(), 1, "one copy on the proposal note: {copies:?}");
        assert_eq!(copies[0].2, "quarantined", "the agent's copy");
        let (live, _) = (cell.replicated_digest().unwrap(/* t */), ());
        let (rebuilt, _) = cell.rematerialize_for_test().unwrap(/* t */);
        assert_eq!(live, rebuilt, "the oracle agrees");
    }
}

// ---------------------------------------------------------------------------
// v0.6.7 (G1): the hidden index's roots outlive the row — checked on the
// file's raw bytes, the same needle rule as tests/forgetting.rs (each test
// binary is its own crate: the helpers are local).
// ---------------------------------------------------------------------------

/// Every root of a body's terms, as the engine's normalisation produces
/// them (the same rule `assert_forgotten` applies in forgetting.rs).
fn roots_here(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in body.split(|c: char| !c.is_alphanumeric()) {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric());
        if token.is_empty() {
            continue;
        }
        let lowered = engramma_core::text::strip_diacritics(&token.to_lowercase());
        let stem = engramma_core::text::normalize(&lowered, Some(engramma_core::format::Lang::It));
        for needle in [lowered, stem] {
            if needle.len() >= 5 && !out.contains(&needle) {
                out.push(needle);
            }
        }
    }
    out
}

/// The cell file and its WAL must carry none of the needles.
fn assert_no_roots(cell: &Cell, needles: &[String]) {
    let path = cell.file_path().to_path_buf();
    let wal = path.parent().unwrap(/* our own cell */).join(format!(
        "{}-wal",
        path.file_name().unwrap(/* our own cell */).to_string_lossy()
    ));
    for f in [path, wal] {
        let raw = std::fs::read(&f).unwrap_or_default();
        for n in needles {
            assert!(
                !raw.windows(n.len()).any(|w| w == n.as_bytes()),
                "«{n}» sopravvive in {}: oblio violato",
                f.display()
            );
        }
    }
}

/// v0.6.7 (G1): the third way out of the hidden index — a QUARANTINED
/// conflict copy closed by its resolution. The agent's loser is born in
/// quarantine, its body's stems live in the hidden index only; the
/// resolution deletes the copy's note and its hidden FTS row goes with an
/// FTS `delete` (roots in the old segments). The purge of the original
/// then finds NO hidden row in the family — and must still forget.
#[test]
fn purge_after_a_quarantined_copy_resolution_scrubs_the_hidden_index() {
    let init = shared_init("chiusa-in-nascosto");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(9, "prima riga\nseconda riga\nterza riga");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    // the agent's body loses to the human's: the copy is born QUARANTINED
    let loser_body = "prima riga\nquokkaviolaceo zibellinotremante\nterza riga";
    wb.store(K + 2, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some(loser_body.into()), ..Default::default() },
        "homun:bot-x",
    )
    .unwrap(/* t */);
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nla mano umana vince\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let hidden: Vec<String> = a
        .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM notes_fts_hidden_docsize")
        .unwrap(/* t */);
    assert_eq!(
        hidden[0], "1",
        "the quarantined copy sits in the hidden index"
    );
    let copy = a.conflicts().unwrap(/* t */)[0].copy_id;
    // the resolution closes it: the hidden FTS row leaves with a `delete`
    a.resolve_conflict(&copy, ResolveKeep::Original, None, "user")
        .unwrap(/* t */);
    // and the original goes too — the whole family's stems must leave
    a.purge(&id, "diritto all'oblio", "user").unwrap(/* t */);
    assert_no_roots(&a, &roots_here(loser_body));
}

/// v0.6.7 (G3): the purge takes the conflict copies out of the SNAPSHOT
/// too. The live scrub closes transitively over the copies
/// (`purge_conflict_copies`); the snapshot's scrub passed only the
/// ORIGINAL's `purge_state` — and the copy's notes row, body and all,
/// stayed in the file (the reviewer's proof of the v0.6.6 verification),
/// while the report said `scrubbed: true`. The whole family's bodies
/// must leave every file: the cell, the WAL and the snapshot.
#[test]
fn purge_takes_the_conflict_copies_out_of_the_snapshot() {
    let init = shared_init("istantanea-copia");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(23, "prima riga\nseconda riga\nterza riga");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    // the bot loses to the human hand: its copy is born quarantined
    let loser_body = "zibellinotremante ovoviviparo sussurrando";
    wb.store(K + 5, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some(loser_body.into()), ..Default::default() },
        "homun:bot-x",
    )
    .unwrap(/* t */);
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nscritta a mano\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    assert_eq!(a.conflicts().unwrap(/* t */).len(), 1, "the copy is open");
    // the compaction freezes a snapshot that HOLDS the conflict family
    let head: i64 = a
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* t */)[0]
        .parse()
        .unwrap(/* t */);
    let snap = a
        .compact(
            head,
            &engramma_core::cell::CompactOptions { keep_snapshot: true },
        )
        .unwrap(/* t */)
        .snapshot
        .expect(/* asked for */ "the snapshot file");
    // the purge of the original: the whole family must leave every file
    let report = a.purge(&id, "diritto all'oblio", "homun:user").unwrap(/* t */);
    assert!(report.scrubbed, "the purge says done: {report:?}");
    let needles = roots_here(loser_body);
    for f in [
        a.file_path().to_path_buf(),
        snap.parent().unwrap(/* our own dirs */).join("a.sqlite-wal"),
        snap.clone(),
    ] {
        let raw = std::fs::read(&f).unwrap_or_default();
        for n in &needles {
            assert!(
                !raw.windows(n.len()).any(|w| w == n.as_bytes()),
                "«{n}» sopravvive in {}: oblio violato",
                f.display()
            );
        }
    }
}
