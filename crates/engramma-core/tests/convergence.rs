// SPDX-License-Identifier: Apache-2.0

//! v0.6 (P1): the convergence property — the state of a copy is a function
//! of the SET of operations it holds, never of the order they arrived in.
//! Two devices write concurrent patches from the same base; two fresh
//! copies receive everything, each in a different order; the replicated
//! state digest of the two copies must be the SAME, and the same a copy
//! that received the operations in total order would produce. This is the
//! property every rule of P1 serves; it starts red (today the last
//! operation to ARRIVE wins) and stays in the suite forever.

use engramma_core::cell::ops::Op;
use engramma_core::format::{At, Note, NoteType};
use engramma_core::{Cell, Confidence, NotePatch, Status};
use std::collections::BTreeMap;
use time::OffsetDateTime;
use ulid::Ulid;

fn now() -> At {
    At(OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */))
}

fn base_note(id: u32, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), id))
            .unwrap(/* test-only: valid Crockford */),
        note_type: NoteType::Fact,
        title: None,
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: String::new(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: now(),
        updated_at: now(),
        extensions: vec![],
    }
}

/// A fresh copy that ingests `ops` in the given order: a bare file, the
/// shared `init` first, then the rest as it arrives.
fn copy_of(name: &str, ops: &[Op]) -> Cell {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::open_bare(&dir.path().join(format!("{name}.sqlite"))).unwrap(/* test-only */);
    for op in ops {
        assert!(
            cell.ingest_op(op).unwrap(/* test-only */),
            "op {} must be new to copy {name}",
            op.op_id
        );
    }
    // the tempdir would take the file with it: leak the path's parent so
    // the cell stays alive for the caller (tests are short-lived)
    std::mem::forget(dir);
    cell
}

/// The union of two cells' logs, deduplicated by op_id, keeping the first
/// occurrence (the fields that matter — content_hash, base, payload — are
/// the same on every copy by construction).
fn union_logs(a: &[Op], b: &[Op]) -> BTreeMap<String, Op> {
    let mut out: BTreeMap<String, Op> = BTreeMap::new();
    for op in a.iter().chain(b.iter()) {
        out.entry(op.op_id.to_string())
            .or_insert_with(|| op.clone());
    }
    out
}

/// Two devices write CONCURRENT patches — the same `base`, the note as each
/// of them had applied it — and two fresh copies receive the union, each in
/// a different arrival order. Their replicated state must converge: same
/// digest, whatever order the operations landed in.
#[test]
fn concurrent_patches_converge_on_every_copy() {
    // A creates the cell and writes one note
    let dir_a = tempfile::tempdir().unwrap(/* test-only */);
    let a = Cell::create(&dir_a.path().join("a.sqlite"), "conv", "device-a")
        .unwrap(/* test-only */);
    let note = base_note(1, "il corpo iniziale");
    let id = note.id;
    a.put_note(&note, "device-a").unwrap(/* test-only */);

    // B is a copy of A at this point: same init, same put
    let first: Vec<Op> = a.ops().unwrap(/* test-only */);
    let b = copy_of("b", &first);

    // both write a patch FROM THE SAME BASE (each had applied exactly the
    // put when it wrote)
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("il corpo scritto da A".into()),
            ..Default::default()
        },
        "device-a",
    )
    .unwrap(/* test-only */);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("il corpo scritto da B".into()),
            ..Default::default()
        },
        "device-b",
    )
    .unwrap(/* test-only */);

    // the two concurrent patches carry the same base
    let ops_a = a.ops().unwrap(/* test-only */);
    let ops_b = b.ops().unwrap(/* test-only */);
    let patch_a = ops_a
        .iter()
        .find(|o| o.kind.as_str() == "patch_note")
        .unwrap(/* test-only: A wrote one */);
    let patch_b = ops_b
        .iter()
        .find(|o| o.kind.as_str() == "patch_note")
        .unwrap(/* test-only: B wrote one */);
    assert!(
        patch_a.base.is_some() && patch_a.base == patch_b.base,
        "the two patches must be concurrent (same base): {:?} vs {:?}",
        patch_a.base,
        patch_b.base
    );

    // two fresh copies receive the SAME union in opposite orders
    let union = union_logs(&ops_a, &ops_b);
    assert_eq!(union.len(), 4, "init, put, and two patches");
    let mut order_x: Vec<Op> = union.values().cloned().collect();
    order_x.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    let mut order_y: Vec<Op> = order_x.clone();
    // the two patches swap: everything else keeps its place
    let px = order_x
        .iter()
        .position(|o| o.op_id == patch_a.op_id)
        .unwrap(/* test-only */);
    let py = order_x
        .iter()
        .position(|o| o.op_id == patch_b.op_id)
        .unwrap(/* test-only */);
    order_y.swap(px, py);
    let ids_x: Vec<String> = order_x.iter().map(|o| o.op_id.to_string()).collect();
    let ids_y: Vec<String> = order_y.iter().map(|o| o.op_id.to_string()).collect();
    assert_ne!(ids_x, ids_y, "the two orders must differ");

    let x = copy_of("x", &order_x);
    let y = copy_of("y", &order_y);
    let dx = x.replicated_digest().unwrap(/* test-only */);
    let dy = y.replicated_digest().unwrap(/* test-only */);
    assert_eq!(
        dx, dy,
        "two copies of the same op SET must hold the same replicated state"
    );

    // a duplicate delivery changes nothing: ingest is idempotent by op_id
    for op in &order_x {
        let fresh = x.ingest_op(op).unwrap(/* test-only */);
        assert!(!fresh, "op {} was already held", op.op_id);
    }
    assert_eq!(
        x.replicated_digest().unwrap(/* test-only */),
        dx,
        "a duplicate delivery must not move the state"
    );

    // a partition: a copy that sees one patch BEFORE the put it leans on
    // holds it pending, and converges when the rest arrives
    let patch_of = |who: &str| -> Op {
        union
            .values()
            .find(|o| {
                o.kind.as_str() == "patch_note"
                    && o.body
                        .as_ref()
                        .and_then(|b| b.get("set"))
                        .and_then(|s| s.get("body"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|t| t.contains(who))
            })
            .cloned()
            .unwrap(/* test-only */)
    };
    let put = union
        .values()
        .find(|o| o.kind.as_str() == "put_note")
        .cloned()
        .unwrap(/* test-only */);
    let init = union
        .values()
        .find(|o| o.kind.as_str() == "init")
        .cloned()
        .unwrap(/* test-only */);
    let dir_z = tempfile::tempdir().unwrap(/* test-only */);
    let z = Cell::open_bare(&dir_z.path().join("z.sqlite")).unwrap(/* test-only */);
    std::mem::forget(dir_z);
    z.ingest_op(&init).unwrap(/* test-only */);
    z.ingest_op(&patch_of("da A")).unwrap(/* test-only */);
    // the patch waits: its note is not here yet
    assert!(
        z.debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM notes")
            .unwrap(/* test-only */)
            .first()
            .is_some_and(|n| n == "0"),
        "a patch before its put materializes nothing"
    );
    z.ingest_op(&put).unwrap(/* test-only */);
    z.ingest_op(&patch_of("da B")).unwrap(/* test-only */);
    assert_eq!(
        z.replicated_digest().unwrap(/* test-only */),
        dx,
        "the partitioned copy converges when the rest arrives"
    );
}
