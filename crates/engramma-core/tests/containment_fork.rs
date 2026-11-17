// SPDX-License-Identifier: Apache-2.0

//! v0.6.5 (E2): the containment witness judges from the fork between
//! the LOSER and the WITNESS — never from the winner. A copy a hand
//! merge closed stays closed when a late human tip moves the winner:
//! the witness depends only on the pair and their past.

mod common;

use common::*;
use engramma_core::NotePatch;
use std::sync::atomic::Ordering;

/// The reviewer's R2: L loses to W1; a hand merge W2 contains L and
/// closes its copy; a LATE human tip H (older, human class) then wins
/// the note. The fork the containment judges from must stay
/// `nca(L, W2)` — adding H cannot reopen the copy.
#[test]
fn a_late_winner_does_not_reopen_a_merged_copy() {
    let init = shared_init("e2-r2");
    let (x, wx) = cell_with("x");
    let (y, wy) = cell_with("y");
    let (z, wz) = cell_with("z");
    for c in [&x, &y, &z] {
        c.ingest_op(&init).unwrap(/* t */);
    }
    let note = base_note(7, "0\na\n2\nb\n3");
    let id = note.id;

    wx.store(K + 1, Ordering::SeqCst);
    x.put_note(&note, "homun:bot-1").unwrap(/* t */);
    for op in &union(&x, &y) {
        y.ingest_op(op).unwrap(/* t */);
        z.ingest_op(op).unwrap(/* t */);
    }
    // z (a human hand, OFFLINE) writes over the put
    wz.store(K + 2, Ordering::SeqCst);
    z.patch_note(
        &id,
        &NotePatch { body: Some("0\na\n2\nb\nH".into()), ..Default::default() },
        "homun:user",
    )
    .unwrap(/* t */);
    // x (an agent) writes over the put too — the fork of everything below
    wx.store(K + 3, Ordering::SeqCst);
    x.patch_note(
        &id,
        &NotePatch { body: Some("1\na\n2\nb\n3".into()), ..Default::default() },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    for op in &union(&x, &y) {
        y.ingest_op(op).unwrap(/* t */);
    }
    // L on x, W1 on y (newer): concurrent, the fork is the agent's write
    wx.store(K + 4, Ordering::SeqCst);
    x.patch_note(
        &id,
        &NotePatch { body: Some("1\na\nL\nb\n3".into()), ..Default::default() },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    wy.store(K + 5, Ordering::SeqCst);
    y.patch_note(
        &id,
        &NotePatch { body: Some("W\na\n2\nb\n3".into()), ..Default::default() },
        "homun:bot-2",
    )
    .unwrap(/* t */);
    for op in &union(&x, &y) {
        x.ingest_op(op).unwrap(/* t */);
        y.ingest_op(op).unwrap(/* t */);
    }
    assert_eq!(
        open_copies(&y, &id).len(),
        1,
        "L lost to W1: its copy is open"
    );
    // L's copy id — the one the hand merge is about to close
    let l_copy = open_copies(&y, &id)[0].0.clone();

    // W2 on y merges L by hand: it contains L, the copy closes
    wy.store(K + 6, Ordering::SeqCst);
    y.patch_note(
        &id,
        &NotePatch { body: Some("W\na\nL\nb\n3".into()), ..Default::default() },
        "homun:bot-2",
    )
    .unwrap(/* t */);
    for op in &union(&x, &y) {
        x.ingest_op(op).unwrap(/* t */);
        y.ingest_op(op).unwrap(/* t */);
    }
    let l_row_of = |cell: &engramma_core::Cell| -> Vec<String> {
        cell.debug_column(&format!(
            "SELECT 'open=' || open FROM conflict_copies WHERE copy_id = '{l_copy}'"
        ))
        .unwrap_or_default()
    };
    let closed = |rows: &[String]| rows.is_empty() || rows == ["open=0"];
    assert!(closed(&l_row_of(&y)), "the hand merge closed L's copy on y");
    assert!(closed(&l_row_of(&x)), "and on x");

    // the human tip of z arrives, late: it WINS (the human class) and a
    // NEW loser may take its copy (W2 itself, now beaten: the one
    // closure and reopening that follow the winner, declared in DESIGN
    // § 4.3) — but L's CLOSED copy must stay closed: the witness is a
    // fact about (L, W2) and their past, not about the winner
    for op in &union(&z, &y) {
        y.ingest_op(op).unwrap(/* t */);
        x.ingest_op(op).unwrap(/* t */);
    }
    let body: Vec<String> = y
        .debug_column(&format!("SELECT body FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */);
    assert_eq!(body[0], "0\na\n2\nb\nH", "the late human tip won the body");
    assert!(
        closed(&l_row_of(&y)),
        "a late winner does not reopen the merged copy: {:?}",
        open_copies(&y, &id)
    );
    assert!(
        closed(&l_row_of(&x)),
        "nor on x: {:?}",
        open_copies(&x, &id)
    );
    // and no open copy carries L's own body: its text lives in the note
    // (the merge), not needed again
    for cell in [&y, &x] {
        assert!(
            open_copies(cell, &id)
                .iter()
                .all(|(_, body, _)| *body != "1\na\nL\nb\n3"),
            "L's own body is not reopened as a copy: {:?}",
            open_copies(cell, &id)
        );
    }
}
