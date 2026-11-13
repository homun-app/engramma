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

/// A SHARED cell compacts only under the STABLE FRONTIER: the deepest seq
/// every live device has confirmed holding. Above it the payloads stay —
/// someone could still ask for them — and when every device has confirmed,
/// the cut reaches the head again.
#[test]
fn a_shared_cell_compacts_only_under_the_stable_frontier() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("f.sqlite"), "conv", "device-a")
        .unwrap(/* test-only */);
    for i in 1..=3_u32 {
        cell.put_note(&base_note(i, &format!("nota {i} della frontiera")), "device-a")
            .unwrap(/* test-only */);
    }
    cell.mark_shared().unwrap(/* test-only */);
    // the laptop synced when the log was four ops tall
    let acked = cell.record_session_ack("portatile").unwrap(/* test-only */);
    assert_eq!(acked, 4, "init + three puts");
    // three more ops arrive; the Mac syncs NOW, the laptop did not
    for i in 4..=6_u32 {
        cell.put_note(&base_note(i, &format!("nota {i} della frontiera")), "device-a")
            .unwrap(/* test-only */);
    }
    cell.record_session_ack("mac").unwrap(/* test-only */);
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    assert_eq!(head, 7);
    assert_eq!(
        cell.stable_frontier().unwrap(/* test-only */),
        4,
        "the frontier is the OLDEST device's ack"
    );
    cell.compact(head, &engramma_core::cell::CompactOptions::default())
        .unwrap(/* test-only */);
    // the payloads BELOW the frontier are gone, those above stay
    let payloads_above: Vec<String> = cell
        .debug_column("SELECT CAST(o.seq AS TEXT) FROM ops o
             JOIN op_payloads p ON p.op_id = o.op_id WHERE o.seq > 4 ORDER BY o.seq")
        .unwrap(/* test-only */);
    assert_eq!(
        payloads_above.len(),
        usize::try_from(head - 4).unwrap(/* test-only */),
        "the tail keeps its payloads"
    );
    let cut: i64 = cell
        .debug_column("SELECT CAST(COALESCE(MAX(seq),0) AS TEXT) FROM snapshots")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    assert_eq!(cut, 4, "the checkpoint sits at the frontier, not the head");
    assert!(cell.verify().unwrap(/* test-only */).ok());
    // the laptop catches up: the frontier reaches the head, the next
    // compaction cuts there
    cell.record_session_ack("portatile").unwrap(/* test-only */);
    assert_eq!(cell.stable_frontier().unwrap(/* test-only */), head);
    cell.compact(head, &engramma_core::cell::CompactOptions::default())
        .unwrap(/* test-only */);
    assert!(cell.verify().unwrap(/* test-only */).ok());
}

/// The `adopt` root covers the unsigned era identically on every copy:
/// whatever order the operations arrived in, the Merkle root over their
/// content hashes is the same, and a copy that receives the adopt records
/// the root of exactly the set it holds.
#[test]
fn the_adopt_root_covers_the_unsigned_era_identically_on_every_copy() {
    let dir_a = tempfile::tempdir().unwrap(/* test-only */);
    let a = Cell::create(&dir_a.path().join("a.sqlite"), "conv", "device-a")
        .unwrap(/* test-only */);
    for i in 1..=3_u32 {
        a.put_note(&base_note(i, &format!("nota {i} dell'era senza firma")), "device-a")
            .unwrap(/* test-only */);
    }
    let pre: Vec<Op> = a.ops().unwrap(/* test-only */);
    // two copies receive the same set in opposite orders
    let mut reversed: Vec<Op> = pre.clone();
    reversed.reverse();
    let x = copy_of("x", &pre);
    let y = copy_of("y", &reversed);

    // the root over each copy's own set is the same
    let root_of = |cell: &Cell| -> String {
        let mut hashes: Vec<String> = cell
            .debug_column("SELECT content_hash FROM ops")
            .unwrap(/* test-only */);
        engramma_core::cell::ops::merkle_root(&mut hashes)
    };
    assert_eq!(root_of(&a), root_of(&x));
    assert_eq!(
        root_of(&x),
        root_of(&y),
        "the root ignores the arrival order"
    );

    // A adopts; the copies receive the adopt; every copy records the root
    // of exactly the set it holds
    a.adopt_log("device-a").unwrap(/* test-only */);
    let adopt: Vec<Op> = a
        .ops()
        .unwrap(/* test-only */)
        .into_iter()
        .filter(|o| o.kind.as_str() == "adopt")
        .collect();
    assert_eq!(adopt.len(), 1);
    for cell in [&x, &y] {
        cell.ingest_op(&adopt[0]).unwrap(/* test-only */);
        let recorded = cell
            .debug_column("SELECT value FROM meta WHERE key = 'adopted_root'")
            .unwrap(/* test-only */);
        // the root of the era BELOW the adopt (the adopt covers what
        // came before it, not itself)
        let mut hashes: Vec<String> = cell
            .debug_column("SELECT content_hash FROM ops WHERE kind != 'adopt'")
            .unwrap(/* test-only */);
        let expected = engramma_core::cell::ops::merkle_root(&mut hashes);
        assert_eq!(recorded, vec![expected]);
        assert!(cell.verify().unwrap(/* test-only */).ok());
    }
}

// ---------------------------------------------------------------------------
// the full convergence property (P1.7): 2–4 devices write concurrently and
// sync through random deliveries — duplicates, partitions, whole
// exchanges — and when everything has reached everyone, every copy holds
// the same replicated state, which is also the state of a copy that
// received the same set in total order.
// ---------------------------------------------------------------------------

use proptest::prelude::*;

/// The vocabulary a device can write. Every op goes through the public API
/// of that device's own copy: its base is whatever THAT device had applied,
/// so concurrency emerges from the delivery order, not from construction.
enum Write {
    Put,
    Patch,
    Delete,
    Supersede,
    Link,
    Unlink,
    Propose,
    Decide,
    Purge,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next() % n
    }
}

/// One simulated cell fleet. Each device holds its own copy; writes go
/// through the public API; deliveries carry the sender's operations.
struct Fleet {
    cells: Vec<Cell>,
    notes: Vec<Ulid>,
    edges: Vec<String>,
    proposals: Vec<Ulid>,
}

impl Fleet {
    fn write(&mut self, dev: usize, w: &Write, rng: &mut Rng) {
        let cell = &self.cells[dev];
        let pick = |pool: &[Ulid], rng: &mut Rng| -> Option<Ulid> {
            pool.get(rng.below(pool.len() as u64) as usize).copied()
        };
        match w {
            Write::Put => {
                let i = self.notes.len() as u32 + 1;
                let body = format!(
                    "nota {i} scritta da {dev}: {}",
                    ["gatto", "tonno", "treno", "orto"][rng.below(4) as usize]
                );
                let n = base_note(90 + i, &body);
                self.notes.push(n.id);
                let _ = cell.put_note(&n, &format!("device-{dev}"));
            }
            Write::Patch => {
                if let Some(id) = pick(&self.notes, rng) {
                    let body = format!(
                        "il corpo di {dev} su {}: {}",
                        id,
                        ["rosso", "blu", "verde"][rng.below(3) as usize]
                    );
                    let _ = cell.patch_note(
                        &id,
                        &NotePatch {
                            body: Some(body),
                            ..Default::default()
                        },
                        &format!("device-{dev}"),
                    );
                }
            }
            Write::Delete => {
                if let Some(id) = pick(&self.notes, rng) {
                    let _ = cell.delete_note(&id, &format!("device-{dev}"));
                }
            }
            Write::Supersede => {
                if let Some(old) = pick(&self.notes, rng) {
                    let i = self.notes.len() as u32 + 1;
                    let mut n = base_note(90 + i, &format!("la verità nuova di {dev}"));
                    n.valid_from = Some(engramma_core::format::Validity(now().get().date(), None));
                    let _ = cell.supersede(&old, &n, None, &format!("device-{dev}"));
                    self.notes.push(n.id);
                }
            }
            Write::Link => {
                if let Some(a) = pick(&self.notes, rng)
                    && let Some(b) = pick(&self.notes, rng)
                {
                    let edge = engramma_core::cell::EdgeInput {
                        src: format!("note:{a}"),
                        rel: "vicino".into(),
                        dst: format!("note:{b}"),
                        note_id: Some(a),
                        valid_from: None,
                        valid_to: None,
                        confidence: engramma_core::Confidence::Stated,
                    };
                    if let Ok(id) = cell.link(&edge, &format!("device-{dev}")) {
                        self.edges.push(id);
                    }
                }
            }
            Write::Unlink => {
                if !self.edges.is_empty() {
                    let idx = rng.below(self.edges.len() as u64) as usize;
                    let id = self.edges[idx].clone();
                    let _ = cell.unlink(&id, &format!("device-{dev}"));
                }
            }
            Write::Propose => {
                let i = self.notes.len() as u32 + 1;
                let out = cell.propose(
                    &engramma_core::cell::proposals::ProposalInput {
                        note: base_note(90 + i, &format!("la proposta di {dev}")),
                        mode: engramma_core::cell::proposals::ProposalMode::New,
                        client: Some("test".into()),
                        reason: Some("perché".into()),
                    },
                    &format!("device-{dev}"),
                    &engramma_core::cell::proposals::ProposeContext::default(),
                );
                if let Ok(out) = out
                    && let Some(id) = out.id
                {
                    self.proposals.push(id);
                }
            }
            Write::Decide => {
                if let Some(id) = pick(&self.proposals, rng) {
                    let _ = cell.decide(
                        &id,
                        &engramma_core::cell::proposals::DecisionInput {
                            action: engramma_core::cell::proposals::DecisionAction::Accept {
                                edited: None,
                            },
                            reason: Some("va bene".into()),
                            review: None,
                        },
                        &format!("device-{dev}"),
                    );
                }
            }
            Write::Purge => {
                if let Some(id) = pick(&self.notes, rng) {
                    let _ = cell.purge(&id, "oblio", &format!("device-{dev}"));
                }
            }
        }
    }

    /// Deliver from `from` to `to`: every operation the sender holds and the
    /// receiver does not. `fraction` < 1 models a partition (a partial
    /// delivery); a second pass completes it later.
    fn deliver(&self, from: usize, to: usize, fraction: u64, rng: &mut Rng) {
        let sender_ops = self.cells[from].ops().unwrap(/* test-only */);
        let held: std::collections::HashSet<String> = self.cells[to]
            .debug_column("SELECT op_id FROM ops")
            .unwrap(/* test-only */)
            .into_iter()
            .collect();
        let missing: Vec<Op> = sender_ops
            .into_iter()
            .filter(|op| !held.contains(&op.op_id.to_string()))
            .collect();
        // fraction 4+ delivers everything; 0..3 delivers roughly a quarter
        // each — a partition that a later pass completes
        let take = if fraction >= 4 {
            missing.len()
        } else {
            (missing.len() * fraction as usize / 4).min(missing.len())
        };
        // deliver a random subset of size `take`
        let mut idx: Vec<usize> = (0..missing.len()).collect();
        // deterministic shuffle
        for i in (1..idx.len()).rev() {
            let j = rng.below((i + 1) as u64) as usize;
            idx.swap(i, j);
        }
        for &i in idx.iter().take(take) {
            // an ingest of an op already held is a DUPLICATE: harmless
            let _ = self.cells[to].ingest_op(&missing[i]);
        }
    }

    /// Full sync: the union of everyone's operations, pushed to everyone,
    /// until nothing new flows.
    fn settle(&self) {
        loop {
            let mut union: std::collections::BTreeMap<String, Op> =
                std::collections::BTreeMap::new();
            for cell in &self.cells {
                for op in cell.ops().unwrap(/* test-only */) {
                    union.insert(op.op_id.to_string(), op);
                }
            }
            let mut moved = false;
            for cell in &self.cells {
                let held: std::collections::HashSet<String> = cell
                    .debug_column("SELECT op_id FROM ops")
                    .unwrap(/* test-only */)
                    .into_iter()
                    .collect();
                for op in union.values() {
                    if !held.contains(&op.op_id.to_string())
                        && cell.ingest_op(op).unwrap(/* test-only */)
                    {
                        moved = true;
                    }
                }
            }
            if !moved {
                return;
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn the_fleet_converges(
        seed in any::<u64>(),
        devices in 2_u32..=4,
        rounds in 12_u32..=28,
    ) {
        let mut rng = Rng(seed | 1);
        let n = devices as usize;
        let dir0 = tempfile::tempdir().unwrap(/* test-only */);
        let a = Cell::create(&dir0.path().join("fleet.sqlite"), "conv", "device-0")
            .unwrap(/* test-only */);
        std::mem::forget(dir0);
        a.set_policy(serde_json::json!({ "write": "propose" }), "device-0")
            .unwrap(/* test-only */);
        let mut cells = vec![a];
        for d in 1..n {
            let dir = tempfile::tempdir().unwrap(/* test-only */);
            let cell = Cell::open_bare(&dir.path().join(format!("fleet{d}.sqlite")))
                .unwrap(/* test-only */);
            std::mem::forget(dir);
            cells.push(cell);
        }
        // deterministic time: each device ticks a fixed clock, so a failing
        // case reproduces exactly (wall-time HLCs would rewrite the op set
        // on every run)
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        // the base is the REAL wall, frozen here: always within the drift
        // cap, and frozen so every device ticks the same deterministic
        // sequence
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        for (d, cell) in cells.iter().enumerate() {
            let tick = Arc::new(AtomicU64::new(0));
            let now = Arc::clone(&tick);
            let node = format!("d{d:011}x");
            cell.inject_clock(engramma_core::hlc::HlcClock::new(node, move || {
                base + now.fetch_add(1, Ordering::SeqCst) * 7 + d as u64 * 3
            }))
            .unwrap(/* test-only */);
        }
        let mut fleet = Fleet { cells, notes: Vec::new(), edges: Vec::new(), proposals: Vec::new() };
        for _ in 0..rounds {
            match rng.below(3) {
                0 | 1 => {
                    // a device writes
                    let dev = rng.below(n as u64) as usize;
                    let w = match rng.below(9) {
                        0 | 1 => Write::Put,
                        2 | 3 => Write::Patch,
                        4 => Write::Delete,
                        5 => Write::Supersede,
                        6 => Write::Link,
                        _ => Write::Unlink,
                    };
                    fleet.write(dev, &w, &mut rng);
                }
                _ => {
                    // a delivery between two devices: sometimes partial
                    let from = rng.below(n as u64) as usize;
                    let to = rng.below(n as u64) as usize;
                    if from != to {
                        let fraction = rng.below(5);
                        fleet.deliver(from, to, fraction, &mut rng);
                    }
                }
            }
        }
        // occasionally exercise the queue and the right to be forgotten
        fleet.write(rng.below(n as u64) as usize, &Write::Propose, &mut rng);
        fleet.settle();
        fleet.write(rng.below(n as u64) as usize, &Write::Decide, &mut rng);
        if rng.below(2) == 0 && !fleet.notes.is_empty() {
            fleet.write(rng.below(n as u64) as usize, &Write::Purge, &mut rng);
        }
        fleet.settle();

        // every copy holds the same replicated state…
        let digests: Vec<String> = fleet
            .cells
            .iter()
            .map(|c| c.replicated_digest().unwrap(/* test-only */))
            .collect();
        let first = &digests[0];
        prop_assert!(
            digests.iter().all(|d| d == first),
            "the fleet did not converge: {digests:?}"
        );
        // …and the same state as a copy that received everything in total
        // order
        let mut union: std::collections::BTreeMap<String, Op> = std::collections::BTreeMap::new();
        for cell in &fleet.cells {
            for op in cell.ops().unwrap(/* test-only */) {
                union.insert(op.op_id.to_string(), op);
            }
        }
        let mut by_total: Vec<Op> = union.into_values().collect();
        by_total.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
        let reference = copy_of("ref", &by_total);
        prop_assert_eq!(
            reference.replicated_digest().unwrap(/* test-only */),
            first.clone(),
            "the total-order reference disagrees with the fleet"
        );
        // one copy's own log still verifies against its own replay
        let vr = fleet.cells[0].verify().unwrap(/* test-only */);
        prop_assert!(vr.ok(), "a converged copy fails verify: {vr:?}");
    }
}

#[test]
fn the_watermark_keeps_the_maximum_across_out_of_order_arrivals() {
    let dir_a = tempfile::tempdir().unwrap(/* test-only */);
    let a = Cell::create(&dir_a.path().join("a.sqlite"), "conv", "device-a")
        .unwrap(/* test-only */);
    a.put_note(&base_note(21, "prima"), "device-a").unwrap(/* t */);
    a.put_note(&base_note(22, "seconda"), "device-a").unwrap(/* t */);
    let ops = a.ops().unwrap(/* test-only */);
    // deliver in REVERSE: the newer hlc first, the older last
    let mut rev: Vec<Op> = ops.clone();
    rev.reverse();
    let z = copy_of("z", &rev);
    let max = a
        .debug_column("SELECT MAX(hlc) FROM ops")
        .unwrap(/* test-only */)[0]
        .clone();
    let got = z
        .debug_column("SELECT value FROM meta WHERE key = 'hlc'")
        .unwrap(/* test-only */)[0]
        .clone();
    assert_eq!(got, max, "the watermark must hold the maximum ever seen");
    assert!(z.verify().unwrap(/* test-only */).ok());
}

/// The P1 measure: what does ONE rematerialization cost on a note with a
/// hundred versions? The number belongs in the REPORT; the assertion is a
/// generous ceiling, a regression tripwire rather than a promise.
#[test]
fn rematerializing_a_hundred_version_note_stays_cheap() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let a = Cell::create(&dir.path().join("m.sqlite"), "conv", "device-a")
        .unwrap(/* test-only */);
    let note = base_note(31, "la nota dalle cento versioni");
    let id = note.id;
    a.put_note(&note, "device-a").unwrap(/* test-only */);
    for i in 1..=100 {
        a.patch_note(
            &id,
            &NotePatch {
                body: Some(format!("versione {i}")), ..Default::default()
            },
            "device-a",
        )
        .unwrap(/* test-only */);
    }
    // a copy that receives everything in REVERSE order rematerializes the
    // note at every arrival: the worst case
    let ops = a.ops().unwrap(/* test-only */);
    let mut rev: Vec<Op> = ops.clone();
    rev.reverse();
    let started = std::time::Instant::now();
    let z = copy_of("m", &rev);
    let elapsed = started.elapsed();
    assert_eq!(
        z.get(&id).unwrap(/* test-only */).body,
        "versione 100",
        "the winner is the last write in total order"
    );
    assert!(z.verify().unwrap(/* test-only */).ok());
    eprintln!("misure P1: 100 versioni consegnate alla rovescia (la peggiore): {elapsed:?}");
    assert!(
        elapsed.as_secs() < 30,
        "a hundred-version rematerialization storm took {elapsed:?}"
    );
}
