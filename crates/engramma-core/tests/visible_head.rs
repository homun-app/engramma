// SPDX-License-Identifier: Apache-2.0

//! v0.6.5 (E1): the `base` of a local write is the VISIBLE HEAD of the
//! note — the newest operation on the winner's line, not the newest
//! operation of the note. The two tests are the reviewer's R4 and R1:
//! the text of a loser that the next writer never had in front of them
//! must survive as an open copy.

mod common;

use common::*;
use engramma_core::NotePatch;
use std::sync::atomic::Ordering;

/// The body of a note as a device shows it.
fn body_of(cell: &impl std::ops::Deref<Target = engramma_core::Cell>, id: &ulid::Ulid) -> String {
    cell.debug_column(&format!("SELECT body FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */)[0]
        .clone()
}

/// R4, only human hands: A writes the body L, then ONLY the title (newer
/// than W); B writes the body W in between. After the sync the note shows
/// W and L's copy is open — right. B then appends a line to the body it
/// SEES: its `base` must be the winner W (the visible head), not the
/// title patch that descends from L. L's copy stays open on BOTH
/// devices; «DUE-di-A» is never lost.
#[test]
fn a_header_patch_on_the_losing_branch_does_not_swallow_the_copy() {
    let init = shared_init("e1-r4");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(9, "uno\ndue\ntre");
    let id = note.id;

    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    // a: the body L, then ONLY the title (newer than everything)
    wa.store(K + 2, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch { body: Some("uno\nDUE-di-A\ntre".into()), ..Default::default() },
        "homun:user",
    )
    .unwrap(/* t */);
    wa.store(K + 8, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            title: Some(Some("il titolo ritoccato da A".into())),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    // b: the body W, between the two
    wb.store(K + 5, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("uno\nDUE-di-B\ntre".into()), ..Default::default() },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    // the sync: W won, L's copy is open — as v0.6.4 already had it
    assert_eq!(body_of(&b, &id), "uno\nDUE-di-B\ntre", "W won the body");
    assert_eq!(open_copies(&b, &id).len(), 1, "L's copy is open before the append");

    // b appends a line to the body it SEES (W's)
    let seen = body_of(&b, &id);
    wb.store(K + 20, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some(format!("{seen}\nquattro")), ..Default::default() },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for (name, cell) in [("a", &a), ("b", &b)] {
        let body = body_of(cell, &id);
        assert!(
            body.contains("DUE-di-B") && body.contains("quattro"),
            "{name}: the append landed on the visible body: {body:?}"
        );
        let copies = open_copies(cell, &id);
        assert_eq!(copies.len(), 1, "{name}: L's copy is still open: {copies:?}");
        assert!(
            copies[0].1.contains("DUE-di-A"),
            "{name}: the copy holds L's text: {copies:?}"
        );
    }
}

/// R1, with the classes: the older HUMAN hand wins; the bot then re-reads
/// the note (the human body) and appends its line. With the base at the
/// visible head the bot's write DESCENDS from the human one — it is
/// sequential, it becomes the note's body, it does not land in
/// quarantine — and the copy of the bot's FIRST write stays open.
#[test]
fn the_agent_edits_the_visible_human_body_and_wins_the_line() {
    let init = shared_init("e1-r1");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(4, "alpha\nbeta\ngamma");
    let id = note.id;

    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    // the human hand (older) and the bot (newer), concurrent
    wa.store(K + 2, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch { body: Some("alpha\nUMANO\ngamma".into()), ..Default::default() },
        "homun:user",
    )
    .unwrap(/* t */);
    wb.store(K + 9, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("alpha\nbeta\nBOT".into()), ..Default::default() },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    // the human class wins even older: the note shows the human body,
    // the bot's first write is an open QUARANTINED copy
    assert_eq!(body_of(&b, &id), "alpha\nUMANO\ngamma", "the human hand won");
    let copies = open_copies(&b, &id);
    assert_eq!(copies.len(), 1, "the bot's first write is a copy");
    assert_eq!(copies[0].2, "quarantined", "an agent's copy is born in quarantine");

    // the bot RE-READS the note and appends to what it sees
    let seen = body_of(&b, &id);
    wb.store(K + 20, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some(format!("{seen}\naggiunta del bot")), ..Default::default() },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for (name, cell) in [("a", &a), ("b", &b)] {
        let body = body_of(cell, &id);
        assert!(
            body.contains("UMANO") && body.contains("aggiunta del bot"),
            "{name}: the bot's write on the visible body became the note's body: {body:?}"
        );
        let copies = open_copies(cell, &id);
        assert_eq!(copies.len(), 1, "{name}: no second copy, no quarantine: {copies:?}");
        assert!(
            copies[0].1.contains("alpha\nbeta\nBOT"),
            "{name}: the first bot write keeps its open copy: {copies:?}"
        );
        assert_eq!(copies[0].2, "quarantined", "{name}: and it is the quarantined one");
    }
}
