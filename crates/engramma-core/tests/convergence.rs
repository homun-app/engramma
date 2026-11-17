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

/// A copy and the folder that holds its file. Fields drop in declaration
/// order: the cell closes first, then the folder goes with it (a leaked
/// folder per copy was ~15k folders per convergence run).
struct Kept {
    cell: Cell,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for Kept {
    type Target = Cell;
    fn deref(&self) -> &Cell {
        &self.cell
    }
}

/// A fresh copy that ingests `ops` in the given order: a bare file, the
/// shared `init` first, then the rest as it arrives.
fn copy_of(name: &str, ops: &[Op]) -> Kept {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::open_bare(&dir.path().join(format!("{name}.sqlite"))).unwrap(/* test-only */);
    for op in ops {
        assert!(
            cell.ingest_op(op).unwrap(/* test-only */),
            "op {} must be new to copy {name}",
            op.op_id
        );
    }
    Kept { cell, _dir: dir }
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
        // v0.6.4 (D2): `device-*` actors are agents — the loser's copy is
        // born in QUARANTINE, in the queue, not active. The copy itself,
        // its tag, its body and its deterministic id are as v0.6 had them.
        assert!(
            copies[0].contains("quarantined"),
            "an agent's copy is born in quarantine: {copies:?}"
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
    /// v0.6.4: delete an OPEN COPY the device can see — the witness (a)
    /// of a resolution
    CloseCopy,
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
    /// the folders of `cells`: declared last, so they go after the cells
    _dirs: Vec<tempfile::TempDir>,
}

/// v0.6.4: the actor of one write — both classes, so the fleet hunts
/// the human-before-agent rule. Two hands in five are human.
fn actor_of(dev: usize, rng: &mut Rng) -> String {
    match rng.below(5) {
        0 => "user".into(),
        1 => format!("user@mcp:etichetta-{dev}"),
        _ => format!("device-{dev}"),
    }
}

/// The open copies a device SEES (visible ones — those with a notes row).
fn open_copies_of(cell: &Cell) -> Vec<(Ulid, Ulid)> {
    cell.conflicts()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.visible)
        .map(|c| (c.copy_id, c.original_id))
        .collect()
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
                    "nota {i} scritta da {dev}\n{}\nfine",
                    ["gatto", "tonno", "treno", "orto"][rng.below(4) as usize]
                );
                let n = base_note(90 + i, &body);
                self.notes.push(n.id);
                let _ = cell.put_note(&n, &actor_of(dev, rng));
            }
            Write::Patch => {
                // the notes THIS device holds (a patch on a note it
                // does not hold is a usage error that rolls back — the
                // writer must have seen the note), the OLDEST few, so
                // devices that synced COLLIDE on them between
                // deliveries: the fleet needs real concurrency or it
                // hunts nothing (six copies in a thousand fleets,
                // before this)
                let own: Vec<Ulid> = cell
                    .debug_column(
                        "SELECT id FROM notes WHERE status != 'deleted'
                         ORDER BY created_at, id LIMIT 5",
                    )
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|id| Ulid::from_string(&id).ok())
                    .collect();
                let Some(id) = own.get(rng.below(own.len() as u64) as usize).copied() else {
                    return;
                };
                // the hand merge: when this device SEES an open copy,
                // half the patches take the copy's body and append a
                // line of its own — over a middle-line change that
                // CONTAINS the loser, the diff3 closes the copy
                if rng.below(2) == 0 {
                    let seen = open_copies_of(cell);
                    if let Some((copy, _)) = seen.get(rng.below(seen.len() as u64) as usize)
                        && let Ok(note) = cell.get(copy)
                        && let Some(original) =
                            seen.iter().find(|(c, _)| c == copy).map(|(_, o)| *o)
                    {
                        let merged = format!("{}\nfuso da {dev}", note.body);
                        let _ = cell.patch_note(
                            &original,
                            &NotePatch {
                                body: Some(merged),
                                ..Default::default()
                            },
                            &actor_of(dev, rng),
                        );
                        return;
                    }
                }
                // a header-only patch in one write of four: the title
                // moves, the body never does
                if rng.below(4) == 0 {
                    let _ = cell.patch_note(
                        &id,
                        &NotePatch {
                            title: Some(Some(format!("il titolo di {dev} su {id}"))),
                            ..Default::default()
                        },
                        &actor_of(dev, rng),
                    );
                    return;
                }
                // the body write: a MIDDLE-LINE change — the shape a
                // later merge can contain
                let token = ["rosso", "blu", "verde", "giallo"][rng.below(4) as usize];
                let body = format!("prima riga\ncorpo di {dev} su {id}: {token}\nterza riga");
                let _ = cell.patch_note(
                    &id,
                    &NotePatch {
                        body: Some(body),
                        ..Default::default()
                    },
                    &actor_of(dev, rng),
                );
            }
            Write::CloseCopy => {
                let seen = open_copies_of(cell);
                if seen.is_empty() {
                    // no copy in THIS device's view: the dice were spent,
                    // write a patch instead (the case fires where copies
                    // ARE visible, or the fleet never exercises it)
                    self.write(dev, &Write::Patch, rng);
                    return;
                }
                let (copy, _) = seen[rng.below(seen.len() as u64) as usize];
                let _ = cell.delete_note(&copy, &actor_of(dev, rng));
            }
            Write::Delete => {
                if let Some(id) = pick(&self.notes, rng) {
                    let _ = cell.delete_note(&id, &actor_of(dev, rng));
                }
            }
            Write::Supersede => {
                if let Some(old) = pick(&self.notes, rng) {
                    let i = self.notes.len() as u32 + 1;
                    let mut n = base_note(90 + i, &format!("la verità nuova di {dev}"));
                    n.valid_from = Some(engramma_core::format::Validity(now().get().date(), None));
                    let _ = cell.supersede(&old, &n, None, &actor_of(dev, rng));
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
                    if let Ok(id) = cell.link(&edge, &actor_of(dev, rng)) {
                        self.edges.push(id);
                    }
                }
            }
            Write::Unlink => {
                if !self.edges.is_empty() {
                    let idx = rng.below(self.edges.len() as u64) as usize;
                    let id = self.edges[idx].clone();
                    let _ = cell.unlink(&id, &actor_of(dev, rng));
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
                    let _ = cell.purge(&id, "oblio", &actor_of(dev, rng));
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
        let fleet = fleet_run(seed, devices, rounds);
        // every copy holds the same replicated state…
        let digests: Vec<String> = fleet
            .cells
            .iter()
            .map(|c| c.replicated_digest().unwrap(/* test-only */))
            .collect();
        let first = digests[0].clone();
        if !digests.iter().all(|d| *d == first) {
            eprintln!("CASE-FLEET seed {seed} devices {devices} rounds {rounds}");
        }
        prop_assert!(
            digests.iter().all(|d| *d == first),
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
        // the reference shares the fleet's frozen clock: the drift cap
        // reads the INJECTED wall, and a bare copy would read the real one
        // (the fleet base is months ahead of it)
        let reference = {
            let dir = tempfile::tempdir().unwrap(/* test-only */);
            let cell = Cell::open_bare(&dir.path().join("ref.sqlite")).unwrap(/* test-only */);
            cell.inject_clock(engramma_core::hlc::HlcClock::new("ref00000000x", || FLEET_BASE_MS))
                .unwrap(/* test-only */);
            for op in &by_total {
                cell.ingest_op(op).unwrap(/* test-only */);
            }
            Kept { cell, _dir: dir }
        };
        prop_assert_eq!(
            reference.replicated_digest().unwrap(/* test-only */),
            first,
            "the total-order reference disagrees with the fleet"
        );
        // one copy's own log still verifies against its own replay
        let vr = fleet.cells[0].verify().unwrap(/* test-only */);
        prop_assert!(vr.ok(), "a converged copy fails verify: {vr:?}");
    }
}

// ---------------------------------------------------------------------------
// v0.6.1 (P0): the fleet is DETERMINISTIC from its seed — the clock base is
// a constant, op ids and salts come from an injected mint keyed on the seed,
// so a saved case writes byte for byte the same op set on every rerun. (The
// v0.6 test froze the wall clock AT RUN TIME and minted random ULIDs: a
// failing seed reran a DIFFERENT case, and that is how a convergence defect
// passed for green.)
// ---------------------------------------------------------------------------

/// The fleet's frozen clock base (2027-01-15T00:00:00Z): near enough to now
/// to be honest, constant so a saved case reproduces. Every clock in the
/// fleet derives from it, so no real wall time enters any operation.
const FLEET_BASE_MS: u64 = 1_800_000_000_000;

/// One device's deterministic mint stream: op ids and payload salts, keyed
/// on the seed and the device, so the same inputs mint the same operations
/// in the same order, forever.
struct SeedMint {
    seed: u64,
    dev: usize,
    n: u64,
}

impl SeedMint {
    fn next(&mut self) -> (Ulid, [u8; 16]) {
        self.n += 1;
        let h =
            blake3::hash(format!("fleet/{}/mint/{}/{}", self.seed, self.dev, self.n).as_bytes());
        let b = h.as_bytes();
        let mut randomness: u128 = 0;
        for x in &b[..10] {
            randomness = (randomness << 8) | u128::from(*x);
        }
        let op_id = Ulid::from_parts(FLEET_BASE_MS, randomness);
        let mut salt = [0_u8; 16];
        salt.copy_from_slice(&b[10..26]);
        (op_id, salt)
    }
}

/// Run one whole fleet from `(seed, devices, rounds)`: the deterministic
/// twin of the old inline body. A rerun with the same inputs writes the
/// same op set byte for byte (clock base, mints and picks all keyed on the
/// seed).
fn fleet_run(seed: u64, devices: u32, rounds: u32) -> Fleet {
    let mut runner = FleetRunner::new(seed, devices);
    runner.rounds(rounds);
    runner.finish();
    runner.fleet
}

/// v0.6.4: the fleet, one ROUND at a time — the monotonicity property
/// needs to look at the state between rounds. The rng stream is exactly
/// `fleet_run`'s: `new`, `rounds`, `finish` in order.
struct FleetRunner {
    fleet: Fleet,
    rng: Rng,
    n: usize,
    round: u64,
}

impl FleetRunner {
    fn new(seed: u64, devices: u32) -> Self {
        let rng = Rng(seed | 1);
        let n = devices as usize;
        // device 0 is born bare and takes the shared init, minted here — the
        // init's op id, salt and cell id come from the seed stream like every
        // other mint (a Cell::create would mint from the process CSPRNG and
        // break the reproduction)
        let dir0 = tempfile::tempdir().unwrap(/* test-only */);
        let a = Cell::open_bare(&dir0.path().join("fleet0.sqlite")).unwrap(/* test-only */);
        let mut mint0 = SeedMint { seed, dev: 0, n: 0 };
        let (cell_id, _) = mint0.next();
        let (op_id, salt) = mint0.next();
        let node0 = "d0000000000x".to_string();
        a.inject_clock(engramma_core::hlc::HlcClock::new(
            node0.clone(),
            move || FLEET_BASE_MS,
        ))
        .unwrap(/* test-only */);
        let init = engramma_core::cell::ops::make_op(engramma_core::cell::ops::OpParts {
            op_id,
            hlc: engramma_core::hlc::Hlc {
                physical_ms: FLEET_BASE_MS,
                counter: 0,
                node: node0,
            },
            actor: "device-0".into(),
            kind: engramma_core::cell::ops::OpKind::Init,
            note_id: None,
            base: None,
            body: serde_json::json!({ "cell_id": cell_id.to_string(), "name": "conv" }),
            salt: salt.to_vec(),
            device: None,
            sig: None,
        })
        .unwrap(/* test-only */);
        a.ingest_op(&init).unwrap(/* test-only */);
        let mut cells = vec![a];
        let mut dirs = vec![dir0];
        for d in 1..n {
            let dir = tempfile::tempdir().unwrap(/* test-only */);
            let cell = Cell::open_bare(&dir.path().join(format!("fleet{d}.sqlite")))
                .unwrap(/* test-only */);
            dirs.push(dir);
            cells.push(cell);
        }
        // deterministic time and deterministic mints, one of each per device
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};
        for (d, cell) in cells.iter().enumerate() {
            let tick = Arc::new(AtomicU64::new(0));
            let now = Arc::clone(&tick);
            let node = format!("d{d:011}x");
            cell.inject_clock(engramma_core::hlc::HlcClock::new(node, move || {
                FLEET_BASE_MS + now.fetch_add(1, Ordering::SeqCst) * 7 + d as u64 * 3
            }))
            .unwrap(/* test-only */);
            let mint = Arc::new(Mutex::new(SeedMint { seed, dev: d, n: 0 }));
            cell.inject_minter(Arc::new(move || {
                let (id, salt) = mint.lock().map_err(|_| {
                    engramma_core::CellError::Db("fleet mint poisoned".into())
                })?.next();
                Ok((id, salt))
            }))
            .unwrap(/* test-only */);
        }
        let fleet = Fleet {
            cells,
            notes: Vec::new(),
            edges: Vec::new(),
            proposals: Vec::new(),
            _dirs: dirs,
        };
        fleet
            .cells[0]
            .set_policy(serde_json::json!({ "write": "propose" }), "device-0")
            .unwrap(/* test-only */);
        Self {
            fleet,
            rng,
            n,
            round: 0,
        }
    }

    /// One round. v0.6.4: the schedule is PHASED — four write rounds
    /// with no delivery in between (the devices drift apart, concurrent
    /// branches GROW from stale heads), then one full sync round where
    /// every pair delivers everything. The pairwise sprinkling of the
    /// old fleet converged every branch before a conflict could ever be
    /// SEEN: the copies appeared only at the final settle, and the
    /// windows the conflict cases need (a merge, a close) never opened.
    fn one_round(&mut self) {
        let n = self.n;
        self.round += 1;
        if self.round.is_multiple_of(5) {
            for from in 0..n {
                for to in 0..n {
                    if from != to {
                        self.fleet.deliver(from, to, 4, &mut self.rng);
                    }
                }
            }
            return;
        }
        match self.rng.below(3) {
            0 | 1 => {
                // a device writes
                let dev = self.rng.below(n as u64) as usize;
                let w = match self.rng.below(10) {
                    0 | 1 => Write::Put,
                    2 | 3 => Write::Patch,
                    4 => Write::Delete,
                    5 => Write::Supersede,
                    6 => Write::Link,
                    7 | 8 => Write::Unlink,
                    // v0.6.4: two writes in ten close a copy. The close
                    // comes from a device that SEES one (the window is
                    // short — the fleet's own traffic closes copies fast,
                    // each closure a descendant witness at work); when
                    // nobody does, the round spends itself on a patch
                    _ => {
                        let seers: Vec<usize> = (0..n)
                            .filter(|d| !open_copies_of(&self.fleet.cells[*d]).is_empty())
                            .collect();
                        if seers.is_empty() {
                            self.fleet.write(dev, &Write::Patch, &mut self.rng);
                        } else {
                            let d = seers[self.rng.below(seers.len() as u64) as usize];
                            self.fleet.write(d, &Write::CloseCopy, &mut self.rng);
                        }
                        return;
                    }
                };
                self.fleet.write(dev, &w, &mut self.rng);
            }
            // the sync rounds carry the deliveries now; write rounds
            // also sometimes move ONE op across (a straggler)
            _ => {
                let from = self.rng.below(n as u64) as usize;
                let to = self.rng.below(n as u64) as usize;
                if from != to {
                    self.fleet.deliver(from, to, 0, &mut self.rng);
                }
            }
        }
    }

    fn rounds(&mut self, rounds: u32) {
        for _ in 0..rounds {
            self.one_round();
        }
    }

    /// The tail `fleet_run` always ran: the queue, the decision, the
    /// purge, and two settles.
    fn finish(&mut self) {
        let n = self.n;
        let dev = self.rng.below(n as u64) as usize;
        self.fleet.write(dev, &Write::Propose, &mut self.rng);
        self.fleet.settle();
        let dev = self.rng.below(n as u64) as usize;
        self.fleet.write(dev, &Write::Decide, &mut self.rng);
        if self.rng.below(2) == 0 && !self.fleet.notes.is_empty() {
            let dev = self.rng.below(n as u64) as usize;
            self.fleet.write(dev, &Write::Purge, &mut self.rng);
        }
        self.fleet.settle();
    }
}

// ---------------------------------------------------------------------------
// v0.6.3 (C1): the conflict copy is a function of the SET — the reviewer's
// witnesses, the minimal sequence behind them, and the differential oracle
// (every copy equals its own reconstruction from the log).
// ---------------------------------------------------------------------------

/// The reviewer's witnesses (v0.6.3, C1), deterministic: two fleets the
/// convergence property found diverging — same op COUNT on both copies, but
/// one holds a conflict copy the other does not derive. They must converge,
/// and every copy must equal its own reconstruction from the log.
#[test]
fn the_witness_fleets_converge() {
    for &(seed, devices, rounds) in &[
        (7348206116156780080_u64, 2_u32, 19_u32),
        (7729712691107302376_u64, 2_u32, 25_u32),
    ] {
        let fleet = fleet_run(seed, devices, rounds);
        let digests: Vec<String> = fleet
            .cells
            .iter()
            .map(|c| c.replicated_digest().unwrap(/* t */))
            .collect();
        assert!(
            digests.iter().all(|d| *d == digests[0]),
            "seed {seed} did not converge: {digests:?}"
        );
        for cell in &fleet.cells {
            let (rebuilt, _) = cell.rematerialize_for_test().unwrap(/* t */);
            assert_eq!(
                cell.replicated_digest().unwrap(/* t */),
                rebuilt,
                "seed {seed}: a copy disagrees with its own reconstruction"
            );
        }
    }
}

/// The MINIMAL arrival sequence behind the witnesses (v0.6.3, C1), built by
/// hand so every clock and base is exact. Six body writes on one note, in
/// total order P < Z < H < E < T < W with Z and H concurrent (same base P):
///
/// - the writer's copy receives T BEFORE H: T parks (its base is not here),
///   and H's arrival replays {P, Z, H, E, T} — the winner is T (base H) and
///   E sits between H and T, so E becomes a conflict copy. Correct for THAT
///   set.
/// - then the writer writes W ITSELF (`patch_note`, base T: the newest op
///   in its log): in the final set the winner is W and its losers are
///   EMPTY — the copy of E must GO. The local write path never re-derived
///   the conflict set, and the copy stayed forever on the writing device
///   while every copy that received W as an arrival dropped it: the
///   divergence of the witnesses.
#[test]
fn a_local_write_rederives_the_conflict_set() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    const K: u64 = 1_800_500_000_000;
    let note = base_note(51, "il corpo iniziale");
    let id = note.id;

    // a cell with a hand-set wall clock and a deterministic mint
    let cell_with = |name: &str| -> (Kept, Arc<AtomicU64>) {
        let wall = Arc::new(AtomicU64::new(K));
        let dir = tempfile::tempdir().unwrap(/* t */);
        let cell = Cell::open_bare(&dir.path().join(format!("{name}.sqlite"))).unwrap(/* t */);
        let w = Arc::clone(&wall);
        cell.inject_clock(engramma_core::hlc::HlcClock::new(
            format!("{name}000000000x"),
            move || w.load(Ordering::SeqCst),
        ))
        .unwrap(/* t */);
        let n = Arc::new(Mutex::new(0_u64));
        let tag = name.to_string();
        cell.inject_minter(Arc::new(move || {
            let mut g = n.lock().map_err(|_| engramma_core::CellError::Db("mint".into()))?;
            *g += 1;
            let h = blake3::hash(format!("min/{tag}/{}", *g).as_bytes());
            let b = h.as_bytes();
            let mut randomness: u128 = 0;
            for x in &b[..10] {
                randomness = (randomness << 8) | u128::from(*x);
            }
            let op_id = Ulid::from_parts(K, randomness);
            let mut salt = [0_u8; 16];
            salt.copy_from_slice(&b[10..26]);
            Ok((op_id, salt))
        }))
        .unwrap(/* t */);
        (Kept { cell, _dir: dir }, wall)
    };

    // one shared init, hand-minted (a Cell::create would mint from the
    // process CSPRNG)
    let init = {
        let h = blake3::hash(b"min/c1/init");
        let b = h.as_bytes();
        let mut randomness: u128 = 0;
        for x in &b[..10] {
            randomness = (randomness << 8) | u128::from(*x);
        }
        let cell_id = Ulid::from_parts(K, randomness);
        let mut salt = [0_u8; 16];
        salt.copy_from_slice(&b[10..26]);
        engramma_core::cell::ops::make_op(engramma_core::cell::ops::OpParts {
            op_id: cell_id,
            hlc: engramma_core::hlc::Hlc {
                physical_ms: K - 1,
                counter: 0,
                node: "b00000000000x".into(),
            },
            actor: "device-b".into(),
            kind: engramma_core::cell::ops::OpKind::Init,
            note_id: None,
            base: None,
            body: serde_json::json!({ "cell_id": cell_id.to_string(), "name": "conv" }),
            salt: salt.to_vec(),
            device: None,
            sig: None,
        })
        .unwrap(/* t */)
    };

    let (b, wb) = cell_with("b");
    let (a, wa) = cell_with("a");
    b.ingest_op(&init).unwrap(/* t */);
    a.ingest_op(&init).unwrap(/* t */);

    let copies_of = |cell: &Cell| -> usize {
        cell.debug_column(
            "SELECT CAST(COUNT(*) AS TEXT) FROM notes n JOIN note_tags t ON t.note_id = n.id
             WHERE t.tag = 'conflitto'",
        )
        .unwrap(/* t */)[0]
            .parse()
            .unwrap(/* t */)
    };

    // P: B creates the note; A receives it
    wb.store(K + 1, Ordering::SeqCst);
    b.put_note(&note, "device-b").unwrap(/* t */);
    let ops_b0: Vec<Op> = b.ops().unwrap(/* t */);
    let p = ops_b0
        .iter()
        .find(|o| o.kind.as_str() == "put_note")
        .unwrap(/* t */)
        .clone();
    a.ingest_op(&p).unwrap(/* t */);

    // Z (B, base P), then H (A, base P): concurrent
    wb.store(K + 2, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("il corpo di B in Z".into()), ..Default::default() },
        "device-b",
    )
    .unwrap(/* t */);
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch { body: Some("il corpo di A in H".into()), ..Default::default() },
        "device-a",
    )
    .unwrap(/* t */);

    // E (B, base Z), then T (A, base H)
    wb.store(K + 4, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("il corpo di B in E".into()), ..Default::default() },
        "device-b",
    )
    .unwrap(/* t */);
    wa.store(K + 5, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch { body: Some("il corpo di A in T".into()), ..Default::default() },
        "device-a",
    )
    .unwrap(/* t */);

    // B receives T first (it parks: H is not here), then H — the replay of
    // {P, Z, H, E, T} makes E the loser of T
    let ops_a: Vec<Op> = a.ops().unwrap(/* t */);
    let patches_a: Vec<&Op> = ops_a
        .iter()
        .filter(|o| o.kind.as_str() == "patch_note")
        .collect();
    assert_eq!(patches_a.len(), 2, "A wrote H and T");
    let (h_op, t_op) = if patches_a[0].hlc < patches_a[1].hlc {
        (patches_a[0], patches_a[1])
    } else {
        (patches_a[1], patches_a[0])
    };
    b.ingest_op(t_op).unwrap(/* t */);
    b.ingest_op(h_op).unwrap(/* t */);
    assert_eq!(
        copies_of(&b),
        1,
        "H's arrival replayed {{P, Z, H, E, T}}: E is T's loser, the copy is due"
    );

    // W: B writes it ITSELF (base T, the newest op in its log). v0.6.4
    // (D1): W's tips are {W, E} — E is NOT W's ancestor (W descends T, T
    // descends H, H is concurrent with E) — the winner is W (same class,
    // newest), the loser E keeps its DURABLE copy: W does not contain
    // E's body (two different rewrites of the same line), and nothing
    // touched the copy's id. Under v0.6.3 this expectation was 0: the
    // copy closed because the winner moved — the ephemeral-copy defect
    // this phase fixes (the reviewer's witnesses, sequence 2).
    wb.store(K + 6, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch { body: Some("il corpo di B in W".into()), ..Default::default() },
        "device-b",
    )
    .unwrap(/* t */);
    assert_eq!(
        copies_of(&b),
        1,
        "the local write re-derives the conflict set: E is still a loser, and its copy is durable"
    );

    // fresh copies of the whole set: in total order, and in B's arrival
    // order (W as an ARRIVAL this time) — all three must agree
    let mut union = std::collections::BTreeMap::new();
    for op in b.ops().unwrap(/* t */).iter().chain(a.ops().unwrap(/* t */).iter()) {
        union.insert(op.op_id.to_string(), op.clone());
    }
    let mut by_total: Vec<Op> = union.into_values().collect();
    by_total.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    let (c, _) = cell_with("c");
    let mut arrival_order: Vec<Op> = vec![init.clone()];
    for op in b.ops().unwrap(/* t */) {
        if op.op_id != init.op_id {
            arrival_order.push(op);
        }
    }
    let (d, _) = cell_with("d");
    for op in &by_total {
        c.ingest_op(op).unwrap(/* t */);
    }
    // D receives them in B's own arrival order — W as an ARRIVAL, the path
    // that always re-derived
    for op in &arrival_order {
        d.ingest_op(op).unwrap(/* t */);
    }
    let (db, dc, dd) = (
        b.replicated_digest().unwrap(/* t */),
        c.replicated_digest().unwrap(/* t */),
        d.replicated_digest().unwrap(/* t */),
    );
    assert_eq!(db, dc, "the writer's copy equals the total-order reference");
    assert_eq!(
        dc, dd,
        "the arrival-order copy equals the total-order reference"
    );
    assert_eq!(copies_of(&c), 1, "the final set keeps E's durable copy");
    let (rebuilt, _) = b.rematerialize_for_test().unwrap(/* t */);
    assert_eq!(
        db, rebuilt,
        "the writer's copy equals its own reconstruction"
    );
}

// v0.6.3 (C1): the differential oracle — for every fleet, every copy's
// incremental state equals the state rebuilt from the LOG ALONE (one
// materialization per object, the from-set reconstruction). The digest
// first; on divergence the property shows the live dump against the
// rebuilt one.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn every_copy_equals_its_reconstruction(
        seed in any::<u64>(),
        devices in 2_u32..=4,
        rounds in 12_u32..=28,
    ) {
        let fleet = fleet_run(seed, devices, rounds);
        for cell in &fleet.cells {
            let live = cell.replicated_digest().unwrap(/* test-only */);
            let (rebuilt, rebuilt_dump) = cell.rematerialize_for_test().unwrap(/* test-only */);
            if live != rebuilt {
                eprintln!(
                    "CASE seed {seed} devices {devices} rounds {rounds}"
                );
            }
            prop_assert_eq!(
                live, rebuilt,
                "the incremental state diverged from the reconstruction\n\
                 --- live ---\n{}\n--- rebuilt ---\n{}",
                cell.replicated_dump().unwrap(/* test-only */),
                rebuilt_dump
            );
        }
    }
}

/// The determinism the v0.6 test only claimed (v0.6.1, P0): the same
/// `(seed, devices, rounds)` writes, byte for byte, the same set of
/// operations — the serialized log of two independent runs is equal. A seed
/// saved by a failing run reproduces exactly the case that failed.
#[test]
fn a_saved_seed_reproduces_the_same_op_set_byte_for_byte() {
    for &(seed, devices, rounds) in &[
        (15951218505961005304_u64, 3_u32, 21_u32),
        (42, 2, 12),
        (0xBEED, 4, 28),
    ] {
        let dump = |_tag: &str| -> String {
            let fleet = fleet_run(seed, devices, rounds);
            let mut out = String::new();
            for (d, cell) in fleet.cells.iter().enumerate() {
                out.push_str(&format!("== device {d} ==\n"));
                for op in cell.ops().unwrap(/* test-only */) {
                    out.push_str(&serde_json::to_string(&op.payload()).unwrap(/* t */));
                    out.push('\n');
                    out.push_str(&op.content_hash);
                    out.push('\n');
                }
            }
            out
        };
        let first = dump("run-1");
        let second = dump("run-2");
        assert_eq!(
            first, second,
            "the same seed must write byte for byte the same op set"
        );
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

/// v0.6.1 (P0, found by the three-device CLI test): devices CREATE the
/// same note id concurrently (the same import on two machines) — the puts
/// carry no base (the note does not exist for any author). Every copy
/// must converge, whatever order the puts arrive in, for two, three and
/// four concurrent creations.
#[test]
fn concurrent_creates_of_the_same_note_converge() {
    for n in [2_u32, 3, 4] {
        concurrent_creates_case(n);
    }
}

fn concurrent_creates_case(authors: u32) {
    let dir0 = tempfile::tempdir().unwrap(/* t */);
    let seed = Cell::create(&dir0.path().join("seed.sqlite"), "conv", "device-0")
        .unwrap(/* t */);
    let note = base_note(41, "il corpo di 0");
    let id = note.id;
    seed.put_note(&note, "device-0").unwrap(/* t */);
    let first: Vec<Op> = seed.ops().unwrap(/* t */);
    // each author writes its own concurrent creation of the SAME id from
    // the same base set
    let mut all: Vec<Op> = first.clone();
    for k in 1..=authors {
        let dir = tempfile::tempdir().unwrap(/* t */);
        let cell = Cell::open_bare(&dir.path().join(format!("c{k}.sqlite")))
            .unwrap(/* t */);
        for op in &first {
            cell.ingest_op(op).unwrap(/* t */);
        }
        let mut own = base_note(41, &format!("il corpo concorrente di {k}"));
        own.id = id;
        cell.put_note(&own, &format!("device-{k}")).unwrap(/* t */);
        all.extend(
            cell.ops()
                .unwrap(/* t */)
                .into_iter()
                .filter(|o| !first.iter().any(|f| f.op_id == o.op_id)),
        );
    }
    // total order and its reverse must land on the same state
    let mut x_ops = all.clone();
    x_ops.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    let x = copy_of("x", &x_ops);
    let mut y_ops = x_ops.clone();
    y_ops.reverse();
    let y = copy_of("y", &y_ops);
    let dx = x.replicated_digest().unwrap(/* t */);
    let dy = y.replicated_digest().unwrap(/* t */);
    assert_eq!(
        dx, dy,
        "{authors} concurrent creates of the same note converge in any order"
    );
    assert!(x.verify().unwrap(/* t */).ok());
}

// ---------------------------------------------------------------------------
// v0.6.4 (D1.6): the MONOTONITY property — a copy open at round r and
// closed at round r+1 has a closure witness in the round r+1 set. Never
// a closure without one.
// ---------------------------------------------------------------------------

/// The open copies of one device, with the loser op the derivation
/// names (the witness check's key).
fn open_copies_with_loser(cell: &Cell) -> Vec<(Ulid, Ulid, Ulid)> {
    let rows = cell
        .debug_column(
            "SELECT copy_id || ' ' || original_id || ' ' || loser_op FROM conflict_copies
             WHERE open = 1",
        )
        .unwrap(/* test-only */);
    rows.into_iter()
        .filter_map(|row| {
            let mut it = row.split(' ');
            let (Some(c), Some(o), Some(l)) = (it.next(), it.next(), it.next()) else {
                return None;
            };
            Some((
                Ulid::from_string(c).unwrap(/* test-only */),
                Ulid::from_string(o).unwrap(/* test-only */),
                Ulid::from_string(l).unwrap(/* test-only */),
            ))
        })
        .collect()
}

/// The purge dominates with a rule of its own (D1.6 has no say there):
/// a purged original takes its copies without a witness.
fn original_purged(cell: &Cell, original: &Ulid) -> bool {
    cell.debug_column(&format!(
        "SELECT CAST(COUNT(*) AS TEXT) FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         WHERE oo.object = 'note:{original}' AND o.kind = 'purge'"
    ))
    .unwrap_or_default()
    .first()
    .is_some_and(|n| n != "0")
}

/// The lapide closes without a D1.6 witness too — its rule is today's
/// own, kept by this phase (REPORT, «prima della correzione», punto 1):
/// a body write that did not see the delete wins nothing and leaves the
/// tip set; a write the rollback erased is gone; and a tip the lapide
/// held can hand the winnership to a former loser, whose copy closes
/// with it. Wherever the original holds a delete, the conflict set
/// follows the lapide, not the witnesses — the property hunts the
/// derivation on the notes the lapide never touched.
/// The original's derivation row is GONE. Only the purge ever removes
/// the row of a copy that carries operations (every other path keeps it
/// as the note's replay base): its absence says the purge took it — and
/// the purge reaches the copies of copies, transitively.
fn original_row_gone(cell: &Cell, original: &Ulid) -> bool {
    cell.debug_column(&format!(
        "SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE original_id = '{original}'"
    ))
    .unwrap_or_default()
    .first()
    .is_some_and(|n| n == "0")
        && cell
            .debug_column(&format!(
                "SELECT CAST(COUNT(*) AS TEXT) FROM ops o
                 JOIN op_objects oo ON oo.op_id = o.op_id
                 WHERE oo.object = 'note:{original}'"
            ))
            .unwrap_or_default()
            .first()
            .is_some_and(|n| n != "0")
}

fn lapide_active(cell: &Cell, original: &Ulid) -> bool {
    cell.debug_column(&format!(
        "SELECT CAST(COUNT(*) AS TEXT) FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         WHERE oo.object = 'note:{original}' AND o.kind = 'delete_note'"
    ))
    .unwrap_or_default()
    .first()
    .is_some_and(|n| n != "0")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn no_copy_closes_without_a_witness(
        seed in any::<u64>(),
        devices in 2_u32..=4,
        rounds in 12_u32..=28,
    ) {
        let mut runner = FleetRunner::new(seed, devices);
        let mut prev: Vec<Vec<(Ulid, Ulid, Ulid)>> =
            runner.fleet.cells.iter().map(open_copies_with_loser).collect();
        // v0.6.5 (E2): the reopen ledger — for every copy that closed,
        // whether its close had a WITNESS and whether the set still
        // DESERVED it. A copy open at round r+1 that was closed at r is
        // a failure unless it closed undeserved (the winner-following
        // rebirth: the filters and the winner's ancestry are the one
        // closure and reopening that follow the winner — DESIGN § 4.3).
        // A WITNESSED close never reopens: the witness is an existence,
        // and with E2's fork it is monotone in the set.
        let mut closed_at: Vec<std::collections::HashMap<Ulid, (bool, bool)>> =
            runner.fleet.cells.iter().map(|_| std::collections::HashMap::new()).collect();
        for _ in 0..rounds {
            runner.one_round();
            let now: Vec<Vec<(Ulid, Ulid, Ulid)>> =
                runner.fleet.cells.iter().map(open_copies_with_loser).collect();
            for (dev, (was, is)) in prev.iter().zip(&now).enumerate() {
                let held: std::collections::HashSet<Ulid> =
                    is.iter().map(|(c, _, _)| *c).collect();
                for (copy, original, loser) in was {
                    if held.contains(copy) {
                        continue;
                    }
                    // admitted without a D1.6 witness: the purge and the
                    // lapide (their own rules), and a set that no longer
                    // DESERVES the copy (a graph an out-of-order arrival
                    // completed made the loser the winner, or an ancestor
                    // of it — the text lives in the note itself)
                    let witnessed = runner.fleet.cells[dev]
                        .conflict_witness_for_test(original, loser)
                        .unwrap_or(false);
                    let deserved = runner.fleet.cells[dev]
                        .conflict_deserved_for_test(original, loser)
                        .unwrap_or(true);
                    prop_assert!(
                        witnessed
                            || original_purged(&runner.fleet.cells[dev], original)
                            || lapide_active(&runner.fleet.cells[dev], original)
                            || !deserved,
                        "seed {seed}: copy {copy} closed without a witness"
                    );
                    closed_at[dev].insert(*copy, (witnessed, deserved));
                }
                // absent → present (v0.6.5, E2): a closed copy that is
                // open again — only a winner-following rebirth may come
                // back; a witnessed close never does
                for (copy, _, _) in is {
                    if let Some((witnessed_close, deserved_close)) =
                        closed_at[dev].remove(copy)
                    {
                        prop_assert!(
                            !witnessed_close && !deserved_close,
                            "seed {seed}: copy {copy} reopened — it closed \
                             witnessed ({witnessed_close}) or deserved ({deserved_close})"
                        );
                    }
                }
            }
            prev = now;
        }
        // the tail (propose, decide, maybe purge, settles) closes copies
        // too — the purge by its own dominance, the rest by witness
        runner.finish();
        let now: Vec<Vec<(Ulid, Ulid, Ulid)>> =
            runner.fleet.cells.iter().map(open_copies_with_loser).collect();
        for (dev, (was, is)) in prev.iter().zip(&now).enumerate() {
            let held: std::collections::HashSet<Ulid> = is.iter().map(|(c, _, _)| *c).collect();
            for (copy, original, loser) in was {
                if held.contains(copy) {
                    continue;
                }
                prop_assert!(
                    runner.fleet.cells[dev]
                        .conflict_witness_for_test(original, loser)
                        .unwrap_or(false)
                        || original_purged(&runner.fleet.cells[dev], original)
                        || original_row_gone(&runner.fleet.cells[dev], original)
                        || lapide_active(&runner.fleet.cells[dev], original)
                        || !runner.fleet.cells[dev]
                            .conflict_deserved_for_test(original, loser)
                            .unwrap_or(true),
                    "seed {seed}: copy {copy} closed in the tail without a witness"
                );
            }
        }
    }
}

/// v0.6.4 (measurement, `--ignored`): the generator's case frequencies
/// over 1000 fleets — both actor classes, header-only patches, hand
/// merges, deletes on copy ids. The numbers go in the REPORT.
#[test]
#[ignore = "a measurement, not an assertion"]
fn generator_frequencies_1000_fleets() {
    let mut ops = 0_u64;
    let mut human = 0_u64;
    let mut agent = 0_u64;
    let mut header_only = 0_u64;
    let mut hand_merges = 0_u64;
    let mut body_writes = 0_u64;
    let mut copy_deletes = 0_u64;
    let mut open_copies = 0_u64;
    let mut quarantined_copies = 0_u64;
    for seed in 0..1000_u64 {
        let fleet = fleet_run(seed, 3, 20);
        for cell in &fleet.cells {
            let rows = cell
                .debug_column(
                    "SELECT o.kind || ' ' || o.actor || ' ' || COALESCE(json_extract(p.body, '$.set.body'), '') FROM ops o
                     LEFT JOIN op_payloads p ON p.op_id = o.op_id",
                )
                .unwrap_or_default();
            for row in &rows {
                let mut it = row.splitn(3, ' ');
                let (kind, actor, body) = (
                    it.next().unwrap_or_default(),
                    it.next().unwrap_or_default(),
                    it.next().unwrap_or_default(),
                );
                ops += 1;
                match engramma_core::cell::conflicts::classify_actor(actor) {
                    engramma_core::cell::conflicts::ActorClass::Human => human += 1,
                    engramma_core::cell::conflicts::ActorClass::Agent => agent += 1,
                }
                if kind == "patch_note" && body.is_empty() {
                    header_only += 1;
                }
                if kind == "patch_note" && body.contains("fuso da ") {
                    hand_merges += 1;
                }
                if matches!(kind, "put_note" | "patch_note") && !body.is_empty() {
                    body_writes += 1;
                }
            }
            copy_deletes += cell
                .debug_column(
                    "SELECT CAST(COUNT(*) AS TEXT) FROM ops o
                     JOIN op_objects oo ON oo.op_id = o.op_id
                     JOIN conflict_copies cc ON cc.copy_id = substr(oo.object, 6)
                     WHERE o.kind = 'delete_note'",
                )
                .unwrap_or_default()
                .first()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            open_copies += cell
                .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE open = 1")
                .unwrap_or_default()
                .first()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            quarantined_copies += cell
                .debug_column(
                    "SELECT CAST(COUNT(*) AS TEXT) FROM notes n JOIN conflict_copies cc ON cc.copy_id = n.id
                     WHERE cc.open = 1 AND n.status = 'quarantined'",
                )
                .unwrap_or_default()
                .first()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    eprintln!(
        "frequenze/1000 flotte: operazioni {ops}, umane {human} ({:.1}%), agente {agent} ({:.1}%), \
         scritture corpo {body_writes}, patch sola intestazione {header_only}, fusioni a mano {hand_merges}, \
         delete su copie {copy_deletes}, copie aperte a fine corsa {open_copies}, in quarantena {quarantined_copies}",
        100.0 * human as f64 / ops as f64,
        100.0 * agent as f64 / ops as f64,
    );
}
