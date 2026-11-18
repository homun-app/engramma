// SPDX-License-Identifier: Apache-2.0

//! v0.6.8 (H2): `verify` tells the truth on SHARED COMPACTED cells. In a
//! shared cell the compaction cuts at the stable frontier — below the
//! head — and a body write that arrives late, offline-written and
//! ordered UNDER the cut by the total order, is the rule, not the
//! rarity. The live state follows the right winner (the per-object
//! materialization); the snapshot replay of `verify` used to keep the
//! old one and cry corruption (the v0.6.7 open defect, seed
//! `9012498534905088620`).

mod common;

use common::*;
use engramma_core::NotePatch;
use std::sync::atomic::Ordering;

/// The spec's own shape: B writes offline, A compacts with a snapshot at
/// its frontier, B's write arrives — `verify` on A must be green.
#[test]
fn verify_is_green_after_a_late_write_under_the_cut() {
    let init = shared_init("h2");
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
    // B writes OFFLINE: A will not see this until after its compaction
    wb.store(K + 3, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nocchiello turchino\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    // A writes its own, LATER: the total order puts B's under it
    wa.store(K + 5, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("prima riga\nmanopola smeraldo\nterza riga".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    // A compacts WITH the snapshot at the STABLE FRONTIER — everything
    // B has confirmed (the put): A's own later write stays ABOVE the
    // cut, and B's offline write (older in the total order, newer by
    // arrival) will land above the cut too, as the frontier promises
    // the frontier here is plain: everything B has seen (the put and
    // the init) — the seq of the LAST op B's log held at the split
    let frontier: i64 = a
        .debug_column(
            "SELECT CAST(MAX(o.seq) AS TEXT) FROM ops o
             WHERE o.hlc <= (SELECT MAX(hlc) FROM ops WHERE kind = 'put_note')",
        )
        .unwrap(/* t */)[0]
        .parse()
        .unwrap_or(0);
    let head: i64 = a
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* t */)[0]
        .parse()
        .unwrap(/* t */);
    assert!(frontier < head, "the frontier sits below A's head");
    let compacted = a
        .compact(
            frontier,
            &engramma_core::cell::CompactOptions { keep_snapshot: true },
        )
        .unwrap(/* t */);
    assert!(compacted.snapshot.is_some(), "the snapshot file stays");
    // B's write arrives, LATE and UNDER the cut in the total order
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
    }
    // the live state follows the right winner: A's later human hand
    let live = a.get(&id).unwrap(/* t */);
    assert_eq!(
        live.body, "prima riga\nmanopola smeraldo\nterza riga",
        "the live body is the winner's"
    );
    assert_eq!(
        a.conflicts().unwrap(/* t */).len(),
        1,
        "the late write lost, its copy is open"
    );
    // and verify must say the same: green, from the snapshot
    let report = a.verify().unwrap(/* t */);
    assert!(
        report.ok(),
        "verify diverges on a shared compacted cell: {:?}",
        report.mismatch
    );
    assert!(matches!(
        report.replay,
        engramma_core::cell::Replay::FromSnapshot { .. }
    ));
}
