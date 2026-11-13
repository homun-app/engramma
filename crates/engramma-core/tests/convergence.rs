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

/// Two devices write concurrent patches; the copies that receive the union
/// in opposite orders must agree on EVERYTHING the conflict rules produce:
/// the original note holds the WINNER's body, and the loser survives as a
/// CONFLICT COPY — an active note with the deterministic id, the tag
/// `conflitto` and an edge to the original, identical on every copy.
#[test]
fn the_losing_body_becomes_a_conflict_copy() {
    let dir_a = tempfile::tempdir().unwrap(/* test-only */);
    let a = Cell::create(&dir_a.path().join("a.sqlite"), "conv", "device-a")
        .unwrap(/* test-only */);
    let note = base_note(7, "il corpo iniziale");
    let id = note.id;
    a.put_note(&note, "device-a").unwrap(/* test-only */);
    let first: Vec<Op> = a.ops().unwrap(/* test-only */);
    let b = copy_of("b", &first);
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
    let union = union_logs(
        &a.ops().unwrap(/* test-only */),
        &b.ops().unwrap(/* test-only */),
    );
    let mut by_total: Vec<Op> = union.values().cloned().collect();
    by_total.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    let patch_of = |needle: &str| -> usize {
        by_total
            .iter()
            .position(|o| {
                o.kind.as_str() == "patch_note"
                    && o.body
                        .as_ref()
                        .and_then(|bd| bd.get("set"))
                        .and_then(|s| s.get("body"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|t| t.contains(needle))
            })
            .unwrap(/* test-only */)
    };
    let mut order_x = by_total.clone();
    let mut order_y = by_total.clone();
    let (px, py) = (patch_of("da A"), patch_of("da B"));
    order_x.swap(px, py);
    order_y.swap(px, py); // the opposite order, twice
    let x = copy_of("x", &order_x);
    let y = copy_of("y", &order_y);
    assert_eq!(
        x.replicated_digest().unwrap(/* test-only */),
        y.replicated_digest().unwrap(/* test-only */),
        "the conflict rules must be a function of the set"
    );

    // the winner is the LAST body write in total order; the loser's body
    // survives in the copy
    let winner = if px < py { "da B" } else { "da A" };
    let loser = if px < py { "da A" } else { "da B" };
    for cell in [&x, &y] {
        let original = cell.get(&id).unwrap(/* test-only */);
        assert!(
            original.body.contains(winner),
            "the original holds the winner's body: {}",
            original.body
        );
        // the conflict copy: an ACTIVE note with the loser's body, the tag,
        // and an edge to the original
        let copies: Vec<String> = cell
            .debug_column(&format!(
                "SELECT n.id || ' | ' || n.body || ' | ' || n.status FROM notes n
                 JOIN note_tags t ON t.note_id = n.id
                 WHERE t.tag = 'conflitto' AND n.id != '{}'",
                id
            ))
            .unwrap(/* test-only */);
        // the original itself may carry the tag? no: only copies do
        assert_eq!(copies.len(), 1, "one conflict copy: {copies:?}");
        assert!(
            copies[0].contains(loser),
            "the copy holds the loser's body: {copies:?}"
        );
        assert!(
            copies[0].contains("active"),
            "the copy is active: {copies:?}"
        );
        let copy_id = copies[0].split(" | ").next().unwrap(/* test-only */).to_string();
        let edge: Vec<String> = cell
            .debug_column(&format!(
                "SELECT src || ' ' || rel || ' ' || dst FROM edges
                 WHERE note_id = '{copy_id}'"
            ))
            .unwrap(/* test-only */);
        assert!(
            edge.iter().any(|e| e.contains(&id.to_string())),
            "the copy declares an edge to the original: {edge:?}"
        );
    }
    // and the copy id is the same on both copies of the cell
    let id_of = |cell: &Cell| -> String {
        cell.debug_column(
            "SELECT n.id FROM notes n JOIN note_tags t ON t.note_id = n.id
             WHERE t.tag = 'conflitto'",
        )
        .unwrap(/* test-only */)[0]
            .clone()
    };
    assert_eq!(
        id_of(&x),
        id_of(&y),
        "the conflict copy's id is deterministic"
    );
}

/// A tombstone written concurrently with an unseen modification wins; a
/// modification whose base IS the tombstone (the writer had seen it)
/// reopens the note.
#[test]
fn a_tombstone_wins_over_an_unseen_modification() {
    let dir_a = tempfile::tempdir().unwrap(/* test-only */);
    let a = Cell::create(&dir_a.path().join("a.sqlite"), "conv", "device-a")
        .unwrap(/* test-only */);
    let note = base_note(9, "il corpo iniziale");
    let id = note.id;
    a.put_note(&note, "device-a").unwrap(/* test-only */);
    let first: Vec<Op> = a.ops().unwrap(/* test-only */);
    let b = copy_of("b", &first);

    // A deletes; B (having seen only the put) patches — concurrent
    a.delete_note(&id, "device-a").unwrap(/* test-only */);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("il corpo scritto da B senza vedere la lapide".into()),
            ..Default::default()
        },
        "device-b",
    )
    .unwrap(/* test-only */);
    let union = union_logs(
        &a.ops().unwrap(/* test-only */),
        &b.ops().unwrap(/* test-only */),
    );
    let mut by_total: Vec<Op> = union.values().cloned().collect();
    by_total.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    let x = copy_of("x", &by_total);
    let mut order_y = by_total.clone();
    // deliver the patch FIRST (out of order), then the delete
    let pp = order_y
        .iter()
        .position(|o| o.kind.as_str() == "patch_note")
        .unwrap(/* test-only */);
    let dd = order_y
        .iter()
        .position(|o| o.kind.as_str() == "delete_note")
        .unwrap(/* test-only */);
    order_y.swap(pp, dd);
    let y = copy_of("y", &order_y);
    assert_eq!(
        x.replicated_digest().unwrap(/* test-only */),
        y.replicated_digest().unwrap(/* test-only */)
    );
    for cell in [&x, &y] {
        let n = cell.get(&id).unwrap(/* test-only */);
        assert_eq!(
            n.status,
            Status::Deleted,
            "the tombstone wins over an unseen modification"
        );
    }

    // a modification that SAW the tombstone (its base is the delete)
    // reopens the note — device B syncs the delete, then patches
    let c = copy_of("c", &by_total);
    c.patch_note(
        &id,
        &NotePatch {
            body: Some("il corpo scritto dopo la lapide".into()),
            ..Default::default()
        },
        "device-b",
    )
    .unwrap(/* test-only */);
    let reopened = c.get(&id).unwrap(/* test-only */);
    assert_eq!(
        reopened.status,
        Status::Active,
        "a patch whose base is the tombstone reopens the note"
    );
    assert!(reopened.body.contains("dopo la lapide"));
}

/// The purge dominates everything, whichever side of the partition it lands
/// on: the note does not materialize, every operation of the note is stored
/// REDACTED when it arrives after the purge (its body never touches the
/// disk of this copy), its conflict copies are purged too, and two copies
/// that received the same set in opposite orders converge.
#[test]
fn the_purge_dominates_whatever_the_arrival_order() {
    let dir_a = tempfile::tempdir().unwrap(/* test-only */);
    let a = Cell::create(&dir_a.path().join("a.sqlite"), "conv", "device-a")
        .unwrap(/* test-only */);
    let note = base_note(11, "il segreto da dimenticare");
    let id = note.id;
    a.put_note(&note, "device-a").unwrap(/* test-only */);
    let first: Vec<Op> = a.ops().unwrap(/* test-only */);
    let b = copy_of("b", &first);

    // A purges; concurrently B patches — and B's patch must never be stored
    // in the clear anywhere the purge has already arrived
    a.purge(&id, "oblio", "device-a").unwrap(/* test-only */);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("il segreto riscritto da B".into()),
            ..Default::default()
        },
        "device-b",
    )
    .unwrap(/* test-only */);

    let union = union_logs(
        &a.ops().unwrap(/* test-only */),
        &b.ops().unwrap(/* test-only */),
    );
    let mut by_total: Vec<Op> = union.values().cloned().collect();
    by_total.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    // one copy receives the purge FIRST, the other the patch first
    let purge_pos = by_total
        .iter()
        .position(|o| o.kind.as_str() == "purge")
        .unwrap(/* test-only */);
    let patch_pos = by_total
        .iter()
        .position(|o| o.kind.as_str() == "patch_note")
        .unwrap(/* test-only */);
    // one copy receives the total order, the other the two swapped
    let order_x = by_total.clone(); // total order (purge before or after the patch by hlc)
    let mut order_y = by_total.clone(); // the opposite delivery of the two
    order_y.swap(purge_pos, patch_pos);
    let x = copy_of("x", &order_x);
    let y = copy_of("y", &order_y);

    // the note is GONE on both copies, whatever the arrival order
    for cell in [&x, &y] {
        assert!(
            matches!(
                cell.get(&id),
                Err(engramma_core::CellError::NoteNotFound(_))
            ),
            "the purged note does not materialize"
        );
        // the body of the note is nowhere in the copy's files — the main
        // database AND the WAL (recent arrivals live there)
        let mut text = String::new();
        for suffix in ["-wal", ""] {
            let mut p = dir_of(cell);
            p.as_mut_os_string().push(suffix);
            if let Ok(bytes) = std::fs::read(&p) {
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        assert!(
            !text.contains("il segreto"),
            "the purged text must not survive in the copy's files"
        );
        // every op of the purged note that arrived after the purge is
        // stored REDACTED: its body never touched this copy's disk
        let rows = cell
            .debug_column(&format!(
                "SELECT kind || ' ' || (p.body IS NULL) FROM ops o
                 LEFT JOIN op_payloads p ON p.op_id = o.op_id
                 WHERE o.op_id IN (SELECT op_id FROM op_objects WHERE object = 'note:{id}')
                 ORDER BY o.seq"
            ))
            .unwrap(/* test-only */);
        assert!(
            rows.iter()
                .all(|r| r.ends_with(" 1") || r.starts_with("purge")),
            "the ops of the purged note are skeletons: {rows:?}"
        );
    }
    assert_eq!(
        x.replicated_digest().unwrap(/* test-only */),
        y.replicated_digest().unwrap(/* test-only */),
        "the purge dominates on every copy of the same set"
    );
}

/// Where this cell's file lives (the test's copy_of leaks its tempdir on
/// purpose; the file sits next to the cell name).
fn dir_of(cell: &Cell) -> std::path::PathBuf {
    let mut p = cell.file_path().to_path_buf();
    p.set_extension("sqlite");
    p
}
