// SPDX-License-Identifier: Apache-2.0

//! v0.6.4 (D2): the class of the author — the human hand beats the
//! agent even when older, the agent's loser is born in quarantine and
//! sits in the proposals queue, derived; the cap of three visible
//! copies per note. Red against the v0.6.3 rule (the total order alone
//! decided).

mod common;

use common::*;
use common::K;
use engramma_core::cell::ops::Op;
use engramma_core::{Cell, NotePatch};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use ulid::Ulid;

/// D2: on two concurrent tips the HUMAN wins even when older, and the
/// agent's loser is born IN QUARANTINE, in the proposals queue, with a
/// reason that says «conflitto» — derived like the copy itself, without
/// an operation.
#[test]
fn the_human_hand_wins_even_older_and_the_agent_loser_is_quarantined() {
    let init = shared_init("classe");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(4, "alpha\nbeta\ngamma");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "user").unwrap(/* t */);
    let put: Vec<Op> = a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
    b.ingest_op(&put[0]).unwrap(/* t */);

    // the human writes FIRST (older HLC), the agent AFTER: the human
    // still wins
    wa.store(K + 2, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch { body: Some("la mano umana, più vecchia".into()), ..Default::default() },
        "user",
    )
    .unwrap(/* t */);
    wb.store(K + 9, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("il bot, più nuovo".into()), ..Default::default() },
        "homun:bot-7",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    for cell in [&a, &b] {
        let body: Vec<String> = cell
            .debug_column(&format!("SELECT body FROM notes WHERE id = '{id}'"))
            .unwrap(/* t */);
        assert_eq!(
            body[0], "la mano umana, più vecchia",
            "the human tip wins although older"
        );
        let copies = open_copies(cell, &id);
        assert_eq!(copies.len(), 1, "the agent's loser became one copy");
        assert_eq!(copies[0].2, "quarantined", "the agent's copy is born in quarantine");
        assert!(copies[0].1.contains("il bot, più nuovo"));
        // the queue holds it, derived, with the reason that says conflitto
        let queue: Vec<String> = cell
            .debug_column(&format!(
                "SELECT mode || '' || state || '' || reason FROM proposals
                 WHERE note_id = '{}'",
                copies[0].0
            ))
            .unwrap(/* t */);
        assert_eq!(
            queue,
            vec!["newpendingconflitto".to_string()],
            "the quarantined copy sits in the queue, derived"
        );
    }
    // a HUMAN loser instead produces the ACTIVE copy, tagged, as ever
    let (c, wc) = cell_with("c");
    c.ingest_op(&init).unwrap(/* t */);
    let note2 = base_note(5, "altro corpo");
    let id2 = note2.id;
    wc.store(K + 1, Ordering::SeqCst);
    c.put_note(&note2, "user").unwrap(/* t */);
    let put2: Vec<Op> = c.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
    let (d, wd) = cell_with("d");
    d.ingest_op(&init).unwrap(/* t */);
    d.ingest_op(&put2[0]).unwrap(/* t */);
    wd.store(K + 2, Ordering::SeqCst);
    d.patch_note(
        &id2,
        &NotePatch { body: Some("il corpo del bot su id2".into()), ..Default::default() },
        "mcp:claude",
    )
    .unwrap(/* t */);
    wc.store(K + 9, Ordering::SeqCst);
    c.patch_note(
        &id2,
        &NotePatch { body: Some("la mano umana su id2".into()), ..Default::default() },
        "user",
    )
    .unwrap(/* t */);
    for op in &union(&c, &d) {
        c.ingest_op(op).unwrap(/* t */);
        d.ingest_op(op).unwrap(/* t */);
    }
    // (agent older here: the human wins by class AND by order)
    assert_eq!(
        open_copies(&c, &id2).len(),
        1,
        "the agent's loser became one copy here too"
    );

    // and the reverse: the HUMAN loses only to a NEWER human — the copy
    // is ACTIVE, not quarantined, and never in the queue
    let (e, we) = cell_with("e");
    e.ingest_op(&init).unwrap(/* t */);
    let note3 = base_note(6, "terzo corpo");
    let id3 = note3.id;
    we.store(K + 1, Ordering::SeqCst);
    e.put_note(&note3, "user").unwrap(/* t */);
    let put3: Vec<Op> = e.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();
    let (f, wf) = cell_with("f");
    f.ingest_op(&init).unwrap(/* t */);
    f.ingest_op(&put3[0]).unwrap(/* t */);
    we.store(K + 2, Ordering::SeqCst);
    e.patch_note(
        &id3,
        &NotePatch { body: Some("la mano umana vecchia su id3".into()), ..Default::default() },
        "user",
    )
    .unwrap(/* t */);
    wf.store(K + 3, Ordering::SeqCst);
    f.patch_note(
        &id3,
        &NotePatch { body: Some("la mano umana nuova su id3".into()), ..Default::default() },
        "user@mcp:etichetta",
    )
    .unwrap(/* t */);
    for op in &union(&e, &f) {
        e.ingest_op(op).unwrap(/* t */);
        f.ingest_op(op).unwrap(/* t */);
    }
    for cell in [&e, &f] {
        let copies = open_copies(cell, &id3);
        assert_eq!(copies.len(), 1, "one copy for the older human hand");
        assert_eq!(copies[0].2, "active", "a human loser's copy is active, as ever");
        let queue: Vec<String> = cell
            .debug_column(&format!(
                "SELECT CAST(COUNT(*) AS TEXT) FROM proposals WHERE note_id = '{}'",
                copies[0].0
            ))
            .unwrap(/* t */);
        assert_eq!(queue, vec!["0".to_string()], "a human's copy never enters the queue");
    }
}

/// D1.3/D1.7: five concurrent branches — the human wins by class, four
/// agent losers open copies, and the CAP keeps three of them visible:
/// the fourth stays open and invisible, and appears when a visible one
/// closes. The choice is a function of the set, so two copies of the
/// cell that received everything in different orders agree on every id.
#[test]
fn five_branches_winner_by_class_four_copies_and_the_cap_of_three() {
    let init = shared_init("tetto");
    let (a, wa) = cell_with("a");
    a.ingest_op(&init).unwrap(/* t */);
    let note = base_note(7, "alpha\nbeta\ngamma");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "user").unwrap(/* t */);
    let put: Vec<Op> = a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();

    // five devices, five concurrent body writes from the same base: the
    // human is the OLDEST, four agents follow
    let mut devices: Vec<Kept> = Vec::new();
    let mut orders: Vec<(Arc<AtomicU64>, u64)> = vec![(Arc::clone(&wa), K + 2)];
    for i in 0..4 {
        let (cell, wall) = cell_with(&format!("b{i}"));
        cell.ingest_op(&init).unwrap(/* t */);
        cell.ingest_op(&put[0]).unwrap(/* t */);
        wall.store(K + 10 + i as u64, Ordering::SeqCst);
        orders.push((wall, K + 10 + i as u64));
        devices.push(cell);
    }
    a.ingest_op(&put[0]).unwrap(/* t */);
    let bodies = [
        ("user", "la mano umana, la più vecchia di tutte"),
        ("homun:bot-1", "il ramo del bot 1"),
        ("homun:bot-2", "il ramo del bot 2"),
        ("homun:bot-3", "il ramo del bot 3"),
        ("homun:story", "il ramo della storia"),
    ];
    let mut writers: Vec<&Cell> = vec![&*a];
    writers.extend(devices.iter().map(|k| &**k));
    for (i, (actor, body)) in bodies.iter().enumerate() {
        // every writer starts from the put alone: re-open a fresh view of
        // the same cells by writing through the public API
        let wall = &orders[i].0;
        let at = orders[i].1;
        wall.store(at, Ordering::SeqCst);
        let cell = writers[i];
        cell.patch_note(
            &id,
            &NotePatch { body: Some(body.to_string()), ..Default::default() },
            actor,
        )
        .unwrap(/* t */);
    }

    // two fresh copies of the cell receive the WHOLE set, in opposite
    // orders: they must agree on everything the rule derives
    let mut all: BTreeMap<String, Op> = BTreeMap::new();
    for cell in &writers {
        for op in cell.ops().unwrap(/* t */) {
            all.entry(op.op_id.to_string()).or_insert_with(|| op.clone());
        }
    }
    let mut by_total: Vec<Op> = all.into_values().collect();
    by_total.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    let (x, _) = cell_with("x");
    let (y, _) = cell_with("y");
    for op in &by_total {
        x.ingest_op(op).unwrap(/* t */);
    }
    let mut reversed: Vec<Op> = by_total.clone();
    reversed.reverse();
    for op in &reversed {
        y.ingest_op(op).unwrap(/* t */);
    }
    assert_eq!(
        x.replicated_digest().unwrap(/* t */),
        y.replicated_digest().unwrap(/* t */),
        "the conflict derivation is a function of the set"
    );

    let body: Vec<String> = x
        .debug_column(&format!("SELECT body FROM notes WHERE id = '{id}'"))
        .unwrap(/* t */);
    assert_eq!(
        body[0], "la mano umana, la più vecchia di tutte",
        "the human tip wins although the oldest"
    );
    // four agent losers: four OPEN copies, but the cap keeps three
    // VISIBLE as notes rows
    let visible = open_copies(&x, &id);
    assert_eq!(visible.len(), 3, "the cap: at most three visible copies");
    assert!(
        visible.iter().all(|(_, _, s)| *s == "quarantined"),
        "the agent's copies are quarantined: {visible:?}"
    );
    // the fourth loser stays open, without a notes row — the derivation
    // row holds it, and conflicts() will list it (D3)
    let hidden: Vec<String> = x
        .debug_column(&format!(
            "SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE original_id = '{id}'"
        ))
        .unwrap(/* t */);
    assert_eq!(hidden, vec!["4".to_string()], "four derivations, three visible");

    // a visible one closes (the delete that resolves it): the fourth
    // becomes visible
    let closed = &visible[0].0;
    x.delete_note(
        &Ulid::from_string(closed).unwrap(/* t */),
        "user",
    )
    .unwrap(/* t */);
    let after = open_copies(&x, &id);
    assert_eq!(after.len(), 3, "the freed window shows the fourth copy");
    assert!(
        after.iter().all(|(i, _, _)| i != closed),
        "the resolved copy is gone from the list"
    );
    // the same closure reaches the other order's copy of the cell — as
    // an ARRIVAL, the same operation the writer wrote
    for op in &x.ops().unwrap(/* t */) {
        y.ingest_op(op).unwrap(/* t */);
    }
    assert_eq!(
        x.replicated_digest().unwrap(/* t */),
        y.replicated_digest().unwrap(/* t */),
        "the two orders agree after the resolution too"
    );
}

/// D1.5: two losers with the SAME body yield ONE copy — the later
/// equal-bodied write contains the earlier (the diff3 of two identical
/// changes is clean), so the earlier closes and the copy that stays is
/// the later one's.
#[test]
fn two_losers_with_equal_bodies_yield_one_copy() {
    let init = shared_init("uguali");
    let (a, wa) = cell_with("a");
    a.ingest_op(&init).unwrap(/* t */);
    let note = base_note(8, "alpha\nbeta\ngamma");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "user").unwrap(/* t */);
    let put: Vec<Op> = a.ops().unwrap(/* t */).into_iter().filter(|o| o.kind.as_str() == "put_note").collect();

    let mk = |name: &str, wall_ms: u64, actor: &str, body: &str| -> Kept {
        let (cell, wall) = cell_with(name);
        cell.ingest_op(&init).unwrap(/* t */);
        cell.ingest_op(&put[0]).unwrap(/* t */);
        wall.store(wall_ms, Ordering::SeqCst);
        cell.patch_note(
            &id,
            &NotePatch { body: Some(body.into()), ..Default::default() },
            actor,
        )
        .unwrap(/* t */);
        cell
    };
    let l1 = mk("l1", K + 2, "homun:bot-1", "lo stesso testo del ramo");
    let l2 = mk("l2", K + 3, "homun:bot-2", "lo stesso testo del ramo");
    let w = mk("w", K + 9, "user", "il corpo della mano umana");

    let mut all: BTreeMap<String, Op> = BTreeMap::new();
    for cell in [&l1, &l2, &w] {
        for op in cell.ops().unwrap(/* t */) {
            all.entry(op.op_id.to_string()).or_insert_with(|| op.clone());
        }
    }
    let mut by_total: Vec<Op> = all.into_values().collect();
    by_total.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    let (x, _) = cell_with("x");
    let mut rev = by_total.clone();
    rev.reverse();
    let (y, _) = cell_with("y");
    for op in &by_total {
        x.ingest_op(op).unwrap(/* t */);
    }
    for op in &rev {
        y.ingest_op(op).unwrap(/* t */);
    }
    assert_eq!(
        x.replicated_digest().unwrap(/* t */),
        y.replicated_digest().unwrap(/* t */),
        "one copy, whatever the arrival order"
    );
    let copies = open_copies(&x, &id);
    assert_eq!(copies.len(), 1, "two equal bodies, one copy: {copies:?}");
    assert!(
        copies[0].1.contains("lo stesso testo del ramo"),
        "the surviving copy holds the shared body"
    );
}
