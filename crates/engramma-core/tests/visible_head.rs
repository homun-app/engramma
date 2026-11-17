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

/// v0.6.5 (E4): the judge's teeth, on the reviewer's own R4 told as a
/// fleet. The model records what each author read; the judge must find
/// every body write saved — with the visible head the closer leans on
/// the WINNER, the copy of L stays open and carries its text. (Against
/// the v0.6.4 base rule this same judge FAILS: the closer leans on the
/// title patch, the copy closes, and «DUE-di-A» is nowhere — proven on
/// the pre-fix code in the worktree run of the REPORT.)
#[test]
fn the_no_text_lost_judge_passes_the_r4_scenario() {
    use std::collections::HashMap;
    // the whole scenario is `a_header_patch_on_the_losing_branch…`, told
    // once more with the model at the side
    let init = shared_init("e4-r4");
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

    // the model: what each author had in front of them
    let mut writes: HashMap<String, (ulid::Ulid, String, String, (String, String))> =
        HashMap::new();
    let mut reads: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
    let put_hash = {
        let mut it = a
            .ops()
            .unwrap(/* t */)
            .into_iter()
            .filter(|o| o.kind.as_str() == "put_note");
        let put = it.next().unwrap(/* t */);
        let key = (put.hlc.to_string(), put.op_id.to_string());
        writes.insert(
            put.op_id.to_string(),
            (id, "uno\ndue\ntre".into(), put.content_hash.clone(), key.clone()),
        );
        reads.insert(put.op_id.to_string(), vec![(String::new(), None)]);
        put.content_hash.clone()
    };
    let mut patch_of = |needle: &str, read: (String, Option<String>)| {
        let op = a
            .ops()
            .unwrap(/* t */)
            .into_iter()
            .find(|o| {
                o.kind.as_str() == "patch_note"
                    && o.body
                        .as_ref()
                        .and_then(|b| b.get("set"))
                        .and_then(|s| s.get("body"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|t| t.contains(needle))
            })
            .unwrap(/* t */);
        let text = op
            .body
            .as_ref()
            .and_then(|b| b.get("set"))
            .and_then(|s| s.get("body"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let key = (op.hlc.to_string(), op.op_id.to_string());
        reads.insert(op.op_id.to_string(), vec![read]);
        writes.insert(op.op_id.to_string(), (id, text, op.content_hash.clone(), key));
        op.content_hash.clone()
    };
    let l_hash = patch_of("DUE-di-A", ("uno\ndue\ntre".into(), Some(put_hash.clone())));
    let w_hash = patch_of("DUE-di-B", ("uno\ndue\ntre".into(), Some(put_hash.clone())));
    patch_of("quattro", (seen.clone(), Some(w_hash)));
    let _ = l_hash;
    let mut model = FleetModel::default();
    model.writes = writes;
    model.reads = reads;
    let mut saved = vec![Default::default(); 2];
    // the cells themselves, as the fleet would hold them
    let dir_c = tempfile::tempdir().unwrap(/* t */);
    let c = engramma_core::Cell::open_bare(&dir_c.path().join("c.sqlite")).unwrap(/* t */);
    for op in b.ops().unwrap(/* t */).iter().chain(a.ops().unwrap(/* t */).iter()) {
        let _ = c.ingest_op(op);
    }
    let cells = [&*a, &*b, &c];
    let verdict = judge_no_text_lost(&cells, &model, &mut saved, "r4");
    assert!(verdict.is_ok(), "the judge must save every body: {verdict:?}");
}

/// v0.6.5 (E4 hunt): the equal-bodied follow-up. A and B concurrent
/// over the put; B (human) wins; F descends from A with A's SAME body
/// (the author read A and rewrote the same middle line). A's copy
/// closes by descent (seen); F — a loser, unfiltered, unwitnessed —
/// must keep its OPEN copy.
#[test]
fn the_equal_bodied_followup_keeps_its_copy() {
    let init = shared_init("e4-followup");
    let (x, wx) = cell_with("x");
    let (y, wy) = cell_with("y");
    x.ingest_op(&init).unwrap(/* t */);
    y.ingest_op(&init).unwrap(/* t */);
    let note = base_note(12, "nota base\nriga di mezzo\nfine");
    let id = note.id;
    wx.store(K + 1, Ordering::SeqCst);
    x.put_note(&note, "user").unwrap(/* t */);
    for op in &union(&x, &y) {
        y.ingest_op(op).unwrap(/* t */);
    }
    // A (agent, on x) and B (human, on y), concurrent over the put —
    // neither crosses yet
    wx.store(K + 2, Ordering::SeqCst);
    x.patch_note(
        &id,
        &NotePatch {
            body: Some("nota base\nla stessa riga nuova\nfine".into()),
            ..Default::default()
        },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    wy.store(K + 3, Ordering::SeqCst);
    y.patch_note(
        &id,
        &NotePatch {
            body: Some("nota base\nla mano umana\nfine".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    assert_eq!(
        body_of(&x, &id),
        "nota base\nla stessa riga nuova\nfine",
        "x has not seen B"
    );
    // F: A's author still sees A (B has not arrived), reads it, and
    // writes the very same body over the visible head — the read-modify
    // hand that lands where it started
    wx.store(K + 9, Ordering::SeqCst);
    x.patch_note(
        &id,
        &NotePatch {
            body: Some("nota base\nla stessa riga nuova\nfine".into()),
            ..Default::default()
        },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    // everything crosses
    for op in &union(&x, &y) {
        x.ingest_op(op).unwrap(/* t */);
        y.ingest_op(op).unwrap(/* t */);
    }
    for (name, cell) in [("x", &x), ("y", &y)] {
        let b = body_of(cell, &id);
        assert_eq!(b, "nota base\nla mano umana\nfine", "{name}: the human hand won");
        let copies = open_copies(cell, &id);
        assert_eq!(
            copies.len(),
            1,
            "{name}: exactly F's copy stays open (A's closes by descent): {copies:?}"
        );
        assert!(
            copies[0].1.contains("la stessa riga nuova"),
            "{name}: F's text lives in the open copy"
        );
    }
}

/// The same shape, with the OTHER arrival order: F reaches the judge's
/// device before B. The set is the set — F's copy must open either way.
#[test]
fn the_equal_bodied_followup_in_the_other_order() {
    let init = shared_init("e4-followup-b");
    let (x, wx) = cell_with("x");
    let (y, wy) = cell_with("y");
    x.ingest_op(&init).unwrap(/* t */);
    y.ingest_op(&init).unwrap(/* t */);
    let note = base_note(13, "nota base\nriga di mezzo\nfine");
    let id = note.id;
    wx.store(K + 1, Ordering::SeqCst);
    x.put_note(&note, "user").unwrap(/* t */);
    for op in &union(&x, &y) {
        y.ingest_op(op).unwrap(/* t */);
    }
    wx.store(K + 2, Ordering::SeqCst);
    x.patch_note(
        &id,
        &NotePatch { body: Some("nota base\nla stessa riga nuova\nfine".into()), ..Default::default() },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    wy.store(K + 3, Ordering::SeqCst);
    y.patch_note(
        &id,
        &NotePatch { body: Some("nota base\nla mano umana\nfine".into()), ..Default::default() },
        "homun:user",
    )
    .unwrap(/* t */);
    // F on x, while x still shows A only
    wx.store(K + 9, Ordering::SeqCst);
    x.patch_note(
        &id,
        &NotePatch { body: Some("nota base\nla stessa riga nuova\nfine".into()), ..Default::default() },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    // the judge's device: put, A, F first — B LAST
    let (z, _) = cell_with("z");
    z.ingest_op(&init).unwrap(/* t */);
    let mut ops = union(&x, &y);
    ops.sort_by(|a, b| a.op_id.cmp(&b.op_id));
    for op in &ops {
        let body = op
            .body
            .as_ref()
            .and_then(|b| b.get("set"))
            .and_then(|set| set.get("body"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if op.kind.as_str() == "put_note" || body.contains("la stessa riga nuova") {
            z.ingest_op(op).unwrap(/* t */);
        }
    }
    let b_op = ops
        .iter()
        .find(|o| {
            o.body
                .as_ref()
                .and_then(|b| b.get("set"))
                .and_then(|set| set.get("body"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|t| t.contains("la mano umana"))
        })
        .cloned()
        .unwrap(/* t */);
    let before = open_copies(&z, &id);
    z.ingest_op(&b_op).unwrap(/* t */);
    let after = open_copies(&z, &id);
    assert_eq!(
        after.len(),
        1,
        "B's late arrival opens F's copy (before: {before:?}, after: {after:?})"
    );
    assert!(after[0].1.contains("la stessa riga nuova"));
}
