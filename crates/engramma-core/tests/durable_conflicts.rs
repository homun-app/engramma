// SPDX-License-Identifier: Apache-2.0

//! v0.6.4 (D1): the conflict copy is DURABLE and only the body creates
//! conflicts. The tests speak of the OBSERVABLE state — the copies'
//! notes rows, the queue, the original's body — so they run red against
//! the v0.6.3 rule and stay green under the new one, whatever the
//! arrival order.

mod common;

use common::*;
use engramma_core::cell::ops::Op;
use engramma_core::NotePatch;
use engramma_core::Cell;
use std::sync::atomic::Ordering;
use std::sync::Arc;
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
    let put: Vec<Op> = a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
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
    assert_eq!(open_copies(&a, &id).len(), 1, "the agent's copy is open on A");
    assert_eq!(open_copies(&b, &id).len(), 1, "the agent's copy is open on B");

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
    let put: Vec<Op> = a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
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
            row[0],
            "il titolo della mano umanaalpha\nX\ngamma",
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
    let put: Vec<Op> = a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
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
    assert_eq!(open_copies(&a, &id).len(), 1, "the copy is open before the merge");

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
