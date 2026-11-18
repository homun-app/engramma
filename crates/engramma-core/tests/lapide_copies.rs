// SPDX-License-Identifier: Apache-2.0

//! v0.6.7 (G2): the copies follow the lapide. While the original holds an
//! active tombstone its conflict copies are a bin's contents — not in
//! `conflicts()`, not searchable, `conflict(copyId)` still answering by id
//! — and a copy closed with a witness never reopens, whatever a later
//! lapide does to the witness's own application. The tests speak the
//! public API (the reviewer's proof of the v0.6.6 verification), and the
//! seed `3753902730771729170` reduced by hand.

mod common;

use common::*;
use engramma_core::search::Query;
use engramma_core::{Cell, NotePatch};
use std::sync::atomic::Ordering;
use ulid::Ulid;

/// The copy of a HUMAN loser is active and visible; the original then goes
/// to the bin. The reviewer's proof: after the `delete` the copy must not
/// be in `conflicts()`, must not be searchable — while reading it BY ID
/// stays honest (the detail keeps answering).
#[test]
fn the_copies_of_a_deleted_note_are_not_listed_nor_searchable() {
    let init = shared_init("cestino");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(9, "prima riga\nseconda riga\nterza riga");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    // two HUMAN hands, concurrent from the put: A's is EARLIER and loses —
    // its body is the copy's — B's is the winner the note shows
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nocchiello turchino\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    wb.store(K + 5, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nmanopola smeraldo\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let list = a.conflicts().unwrap(/* t */);
    assert_eq!(list.len(), 1, "the conflict is there before the delete");
    let copy = list[0].copy_id;
    assert_eq!(list[0].original_id, id);
    let hits = a.search(&Query::text("occhiello turchino").k(10)).unwrap(/* t */);
    assert!(
        hits.iter().any(|h| h.note.id == copy),
        "before the delete the copy's body is searchable"
    );

    // the bin: the original goes, and its copies follow the lapide
    wa.store(K + 9, Ordering::SeqCst);
    a.delete_note(&id, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let status: Vec<String> = a
        .debug_column(&format!("SELECT status FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */);
    assert_eq!(status[0], "deleted", "the note is in the bin");
    assert!(
        a.conflicts().unwrap(/* t */)
            .iter()
            .all(|c| c.original_id != id),
        "a deleted note's copies are not in conflicts()"
    );
    for cell in [&a, &b] {
        let hits = cell
            .search(&Query::text("occhiello turchino").k(10))
            .unwrap(/* t */);
        assert!(
            hits.iter().all(|h| h.note.id != copy),
            "the copy is not searchable while the original is deleted"
        );
        // include_quarantined is the owner's wider view, not a resurrection
        // of the bin: the copy is ACTIVE, not quarantined, and still hidden
        let wide = Query::text("occhiello turchino").k(10);
        let mut wide = wide;
        wide.filters.include_quarantined = true;
        let hits = cell.search(&wide).unwrap(/* t */);
        assert!(
            hits.iter().all(|h| h.note.id != copy),
            "not even the owner's wide view searches the bin"
        );
    }
    // a read BY ID is honest: the detail still answers
    let detail = a.conflict(&copy).unwrap(/* t */);
    assert_eq!(detail.original_id, id);
}

/// The note REOPENS the way it always could — a modification whose base IS
/// the lapide — and its open copies come back with the rules of always.
#[test]
fn a_reopened_note_brings_its_copies_back() {
    let init = shared_init("riapre");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(11, "prima riga\nseconda riga\nterza riga");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nperno di ottone\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    wb.store(K + 5, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nmanopola smeraldo\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let copy = a.conflicts().unwrap(/* t */)[0].copy_id;
    // the delete, then the reopening patch: base = the lapide (the closed
    // line), written by a device that HAS the delete
    wa.store(K + 9, Ordering::SeqCst);
    a.delete_note(&id, "homun:user").unwrap(/* t */);
    // while the bin lasts, the copy is neither listed nor searchable…
    assert!(
        a.conflicts().unwrap(/* t */)
            .iter()
            .all(|c| c.original_id != id),
        "the bin hides the open copy while the note stays deleted"
    );
    let binned = a
        .search(&Query::text("perno di ottone").k(10))
        .unwrap(/* t */);
    assert!(
        binned.iter().all(|h| h.note.id != copy),
        "the bin's copy answers no search"
    );
    // …and the reopening patch brings it back
    wa.store(K + 15, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nmanopola smeraldo riaperta\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let status: Vec<String> = a
        .debug_column(&format!("SELECT status FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */);
    assert_eq!(status[0], "active", "the patch reopened the note");
    // the open copy is a choice again, with the rules of always
    let listed: Vec<Ulid> = a
        .conflicts()
        .unwrap(/* t */)
        .iter()
        .map(|c| c.copy_id)
        .collect();
    assert!(
        listed.contains(&copy),
        "the reopened note brings its open copy back: {listed:?}"
    );
    let hits = a
        .search(&Query::text("perno di ottone").k(10))
        .unwrap(/* t */);
    assert!(
        hits.iter().any(|h| h.note.id == copy),
        "and the copy is searchable again"
    );
}

/// The seed `3753902730771729170` (the 20 000-case hunt of v0.6.6),
/// reduced by hand: a note with TWO lapides. `put` device-1 → L device-1
/// (the loser) → W device-2 (concurrent) → D1 `user` (base = W) → P2
/// device-1 (base = W too: it did not see D1, and its text contains L) →
/// D2 device-1 (base = P2, last in the total order). The copy of L closes
/// when P2 is in the set; when D1 arrives, P2 becomes a write the lapide
/// beat — and the copy must stay closed anyway: the witness is an
/// existence, and the lapide that beats P2 cannot retract what P2 saw.
#[test]
fn a_witness_the_lapide_beat_still_closes_the_copy_forever() {
    let init = shared_init("3753");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    let (c, wc) = cell_with("c");
    let (o, wo) = cell_with("o");
    for cell in [&a, &b, &c, &o] {
        cell.ingest_op(&init).unwrap(/* t */);
    }
    let note = base_note(71, "prima riga\nseconda riga\nterza riga");
    let id = note.id;

    // 1. the put, device-1
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "device-1").unwrap(/* t */);
    let put: Vec<engramma_core::cell::ops::Op> = a
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|x| x.kind.as_str() == "put_note")
        .collect();
    for cell in [&b, &c, &o] {
        cell.ingest_op(&put[0]).unwrap(/* t */);
    }
    // 2. L, device-1, from the put
    wa.store(K + 2, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nocchiello turchino\nterza riga".into()),
            ..Default::default()
        },
        "device-1",
    )
    .unwrap(/* t */);
    let l = last_op_of(&a, &id).unwrap(/* t */);
    // 3. W, device-2, concurrent (b saw only the put)
    wb.store(K + 3, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nmanopola smeraldo\nterza riga".into()),
            ..Default::default()
        },
        "device-2",
    )
    .unwrap(/* t */);
    let w_op = last_op_of(&b, &id).unwrap(/* t */);
    for cell in [&a, &c, &o] {
        cell.ingest_op(&w_op).unwrap(/* t */);
    }
    // 5. D1, `user` on device-3: its base is W (the head c holds)
    wc.store(K + 4, Ordering::SeqCst);
    c.delete_note(&id, "user").unwrap(/* t */);
    let d1 = last_op_of(&c, &id).unwrap(/* t */);
    // 6. P2, device-1: it holds put, L, W — its head is W, the very base
    // D1 leaned on (it did not see the delete), and its text CONTAINS L
    wa.store(K + 5, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nocchiello turchino\nterza riga\nquarta riga".into()),
            ..Default::default()
        },
        "device-1",
    )
    .unwrap(/* t */);
    let p2 = last_op_of(&a, &id).unwrap(/* t */);
    // 7. D2, device-1, after P2: the last op of the total order
    wa.store(K + 6, Ordering::SeqCst);
    a.delete_note(&id, "device-1").unwrap(/* t */);
    let d2 = last_op_of(&a, &id).unwrap(/* t */);

    // the observer takes the wave the seed served: everything but the two
    // deletes first — the copy of L closes with P2 as its witness…
    let loser_of = |cell: &Cell| {
        cell.debug_column(&format!(
            "SELECT loser_op FROM conflict_copies WHERE original_id = '{id}' AND open = 1"
        ))
        .unwrap(/* t */)
        .into_iter()
        .map(|lop| lop == l.op_id.to_string())
        .next()
        .unwrap_or(false)
    };
    for op in &[put[0].clone(), l.clone(), w_op.clone(), p2.clone()] {
        o.ingest_op(op).unwrap(/* t */);
    }
    let open_rows: Vec<String> = o
        .debug_column(&format!(
            "SELECT CAST(open AS TEXT) FROM conflict_copies WHERE original_id = '{id}'"
        ))
        .unwrap(/* t */);
    assert!(
        open_rows.iter().all(|r| r == "0"),
        "the copy of L is closed: P2 witnessed it ({open_rows:?})"
    );
    assert!(loser_of(&o) || open_rows.is_empty());
    // …then D2 (the note goes to the bin, the copy stays closed)…
    o.ingest_op(&d2).unwrap(/* t */);
    let open_rows: Vec<String> = o
        .debug_column(&format!(
            "SELECT CAST(open AS TEXT) FROM conflict_copies WHERE original_id = '{id}'"
        ))
        .unwrap(/* t */);
    assert!(
        open_rows.iter().all(|r| r == "0"),
        "D2 deletes the note; the closed copy stays closed ({open_rows:?})"
    );
    // …then D1 arrives and beats P2: the witness must survive the lapide
    o.ingest_op(&d1).unwrap(/* t */);
    let open_rows: Vec<String> = o
        .debug_column(&format!(
            "SELECT CAST(open AS TEXT) FROM conflict_copies WHERE original_id = '{id}'"
        ))
        .unwrap(/* t */);
    assert!(
        open_rows.iter().all(|r| r == "0"),
        "the witness does not depend on the lapide: the copy of L does not \
         reopen while the note stays deleted ({open_rows:?})"
    );
    let status: Vec<String> = o
        .debug_column(&format!("SELECT status FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */);
    assert_eq!(status[0], "deleted", "the note keeps its lapide");
    let _ = wo;
}
