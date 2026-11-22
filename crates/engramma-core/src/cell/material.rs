// SPDX-License-Identifier: Apache-2.0

//! v0.6 (P1): per-object materialization. The state of a copy is a function
//! of the SET of operations it holds, never of the order they arrived in —
//! so an operation that lands BEFORE the newest one already applied to its
//! object forces that object to REMATERIALIZE: its state rows go, and every
//! operation concerning it replays in the TOTAL ORDER
//! `(hlc.physical_ms, hlc.counter, hlc.node, op_id)`. The node ids are
//! fixed-width (12 hex chars), so `ORDER BY hlc, op_id` in SQL walks exactly
//! this order.
//!
//! An operation that arrives last of its objects (the common case) applies
//! directly, as ever — no replay, no extra cost. An operation on an object
//! that does not exist yet (a patch before its put, across a partition) is
//! STORED and waits: the object is PENDING (`pending_objects`, local
//! bookkeeping like `meta.hlc`), and the next arrival rematerializes it.

use rusqlite::OptionalExtension;
use ulid::Ulid;

use crate::error::CellError;

use super::ops::{self, Op, OpKind, Scope};

/// The compaction cut: the newest checkpoint's seq (0 when the cell never
/// compacted). Below the cut the payloads are gone — the state there is
/// frozen and stable (P1.6 will make it the frontier), so materialization
/// only ever replays ABOVE it, starting from the state the cut left.
fn cut_seq(tx: &rusqlite::Transaction<'_>) -> Result<i64, CellError> {
    let cut: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM snapshots", [], |r| {
        r.get(0)
    })?;
    Ok(cut)
}

/// A held half — purge-dominated, or beaten by a lapide — still consumes
/// its version slot: the row's number is the note's history, and the
/// numbering must not drift with the set (the write bumped the counter
/// where it first applied; a `restore` above may name that number).
fn consume_version_slot(tx: &rusqlite::Transaction<'_>, id: &Ulid) -> Result<(), CellError> {
    tx.execute(
        "UPDATE notes SET version = version + 1 WHERE id = ?1",
        [id.to_string()],
    )?;
    Ok(())
}

/// Did this object already hold operations at or below the cut — was it
/// BORN before the freeze? A note whose whole life sits above the cut
/// did not exist at the cut: there is no state to anchor on, and the
/// replay starts from zero (v0.6.12, L1; anchoring on a mid-life
/// photograph made the version counter run away at every
/// rematerialization).
fn note_exists_at(
    tx: &rusqlite::Transaction<'_>,
    object: &str,
    cut: i64,
) -> Result<bool, CellError> {
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         WHERE oo.object = ?1 AND o.seq <= ?2",
        rusqlite::params![object, cut],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}
/// Every stored operation concerning one object ABOVE the compaction cut, in
/// total order.
pub(crate) fn ops_concerning(
    tx: &rusqlite::Transaction<'_>,
    object: &str,
) -> Result<Vec<Op>, CellError> {
    let cut = cut_seq(tx)?;
    let mut stmt = tx.prepare(
        "SELECT o.seq, o.op_id, o.hlc, o.actor, o.kind, o.note_id, o.base, o.content_hash,
                o.payload_digest, o.payload_state, p.salt, p.body, o.prev_hash, o.hash, o.device, o.sig
         FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         LEFT JOIN op_payloads p ON p.op_id = o.op_id
         WHERE oo.object = ?1 AND o.seq > ?2
         ORDER BY o.hlc, o.op_id",
    )?;
    let out = stmt
        .query_map(rusqlite::params![object, cut], |r| {
            Ok(ops::op_from_row(ops::OpRow {
                seq: r.get(0)?,
                op_id: &r.get::<_, String>(1)?,
                hlc: &r.get::<_, String>(2)?,
                actor: &r.get::<_, String>(3)?,
                kind: &r.get::<_, String>(4)?,
                note_id: r.get::<_, Option<String>>(5)?.as_deref(),
                base: r.get::<_, Option<String>>(6)?.as_deref(),
                content_hash: &r.get::<_, String>(7)?,
                payload_digest: &r.get::<_, String>(8)?,
                payload_state: &r.get::<_, String>(9)?,
                salt: r.get::<_, Option<Vec<u8>>>(10)?.as_deref(),
                body_json: r.get::<_, Option<String>>(11)?.as_deref(),
                prev_hash: &r.get::<_, String>(12)?,
                hash: &r.get::<_, String>(13)?,
                device: r.get::<_, Option<String>>(14)?.as_deref(),
                sig: r.get::<_, Option<String>>(15)?.as_deref(),
            }))
        })?
        .collect::<Result<Result<Vec<_>, _>, _>>()??;
    Ok(out)
}

fn is_pending(tx: &rusqlite::Transaction<'_>, object: &str) -> Result<bool, CellError> {
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pending_objects WHERE object = ?1",
        [object],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn mark_pending(
    tx: &rusqlite::Transaction<'_>,
    object: &str,
    since_op: &str,
) -> Result<(), CellError> {
    tx.execute(
        "INSERT INTO pending_objects (object, since_op) VALUES (?1, ?2)
         ON CONFLICT(object) DO UPDATE SET since_op = ?2",
        rusqlite::params![object, since_op],
    )?;
    Ok(())
}

fn clear_pending(tx: &rusqlite::Transaction<'_>, object: &str) -> Result<(), CellError> {
    tx.execute("DELETE FROM pending_objects WHERE object = ?1", [object])?;
    Ok(())
}

/// Is this operation the NEWEST of its object, among the stored ones above
/// the compaction cut? (Below the cut nothing arrives anymore — that is the
/// frontier's promise.)
fn is_newest_of(tx: &rusqlite::Transaction<'_>, object: &str, op: &Op) -> Result<bool, CellError> {
    let cut = cut_seq(tx)?;
    let head: Option<(String, String)> = tx
        .query_row(
            "SELECT o.hlc, o.op_id FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.seq > ?2
             ORDER BY o.hlc DESC, o.op_id DESC LIMIT 1",
            rusqlite::params![object, cut],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(head.is_some_and(|(hlc, op_id)| hlc == op.hlc.to_string() && op_id == op.op_id.to_string()))
}

/// The HLC watermark of the cell (`meta.hlc`) must hold the MAXIMUM ever
/// seen — a replay of older operations writes lower values as it goes, so
/// the arrival path restores the watermark it found.
fn restore_hlc_watermark(
    tx: &rusqlite::Transaction<'_>,
    watermark: Option<String>,
    arrival: Option<&crate::hlc::Hlc>,
) -> Result<(), CellError> {
    // the floor is the MAXIMUM of the pre-arrival watermark and the
    // arriving operation's own HLC: an operation that LOSES every conflict
    // rule (a body write the lapide beat) still ARRIVED, and the clock
    // observed it — the watermark is the maximum ever seen, applied or not
    let floor = {
        let w = watermark
            .as_deref()
            .and_then(|s| crate::hlc::Hlc::parse(s).ok());
        match (w, arrival) {
            (Some(w), Some(a)) => Some(w.max(a.clone())),
            (Some(w), None) => Some(w),
            (None, Some(a)) => Some(a.clone()),
            (None, None) => None,
        }
    };
    let Some(floor) = floor else {
        return Ok(());
    };
    let now: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'hlc'", [], |r| r.get(0))
        .optional()?;
    let parsed_n = now.as_deref().and_then(|s| crate::hlc::Hlc::parse(s).ok());
    if let Some(n) = parsed_n
        && n >= floor
    {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('hlc', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [floor.to_string()],
    )?;
    Ok(())
}

/// One operation ARRIVES at this copy: the log row is stored (idempotence
/// is the caller's — `ingest_op` — business), then every object it touches
/// either takes the fast path (the operation is the newest and the object
/// is not pending: it applies whole, as it always did) or rematerializes
/// from the full set.
pub(crate) fn arrive(tx: &rusqlite::Transaction<'_>, op: &Op) -> Result<(), CellError> {
    ops::store_op(tx, op)?;
    let watermark: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'hlc'", [], |r| r.get(0))
        .optional()?;
    let objects = ops::objects_of(op);
    let touched_a_note = objects.iter().any(|o| o.starts_with("note:"));
    let mut to_materialize: Vec<String> = Vec::new();
    // v0.6.1 (P0): an operation whose base is not in the log waits — but
    // only in the object the base BELONGS to (`op.note_id`'s note: the base
    // is always an operation of that note). The other halves of a
    // two-object operation (the NEW note a supersede creates, the target a
    // decide closes) lean on nothing that is missing: they materialize now,
    // or their object would sleep until an operation that never concerns it
    // came to wake it (found by the convergence fleet: the supersede's new
    // note never materialized on the copy that received it before its base).
    let park_object = match op.note_id {
        Some(n) if base_unknown(tx, op)? => Some(format!("note:{n}")),
        _ => None,
    };
    for object in &objects {
        if is_pending(tx, object)?
            || !is_newest_of(tx, object, op)?
            || breaks_the_chain(tx, object, op)?
            || Some(object) == park_object.as_ref()
        {
            to_materialize.push(object.clone());
        }
    }
    // v0.6.11 (K1): the purge dominates per HALF — the half of a purged
    // object, and the halves that only reference it (an edge row is
    // nothing else). A link or unlink whose edge names a purged note is
    // dominated as a whole: it is recorded, it does not wait.
    let edge_dominated = match objects.iter().find(|o| o.starts_with("edge:")) {
        Some(edge) => half_dominated_by_purge(tx, op, edge)?,
        None => false,
    };
    if edge_dominated {
        // a dominated operation is recorded, it does not wait: the link it
        // names will never materialize, and a pending row that no arrival
        // can wake is a lie (the pathological order — the unlink parks
        // before its link arrives, both after the purge — is closed here:
        // the link's own arrival is dominated too and clears the wait)
        for object in objects.iter().filter(|o| o.starts_with("edge:")) {
            clear_pending(tx, object)?;
        }
    }
    // an unlink whose link has not arrived waits like a patch before its
    // put: the edge object goes pending, and the link's arrival replays
    let mut unlink_waits = false;
    if op.kind == OpKind::Unlink
        && !edge_dominated
        && let Some(edge) = objects.iter().find(|o| o.starts_with("edge:"))
    {
        let edge_id = edge.strip_prefix("edge:").unwrap_or_default();
        let n: i64 = tx.query_row("SELECT COUNT(*) FROM edges WHERE id = ?1", [edge_id], |r| {
            r.get(0)
        })?;
        if n == 0 {
            mark_pending(tx, edge, &op.op_id.to_string())?;
            unlink_waits = true;
        }
    }
    if !unlink_waits {
        if to_materialize.is_empty() {
            // the fast path: every object takes this operation directly, in
            // its own half. A half that needs a note that is not here yet (a
            // patch before its put, across a partition) does not fail the
            // arrival — the object turns PENDING and waits for what it leans
            // on; the halves that merely REFERENCE a missing note (the old
            // side of a supersede, the target side of a decide) already
            // tolerate it. The tombstone rule rides along: a body write on a
            // lapide that it did not see wins nothing; one whose base is the
            // lapide reopens it.
            for object in &objects {
                // the purge's word on this half is asked ONCE: the
                // tombstone guard and the application below share it (a
                // put pays one dominance query, not two — the measured
                // insert rate of K1's first cut paid two, +9% on the
                // v0.6.3 base)
                let dominated = half_dominated_by_purge(tx, op, object)?;
                if let Some(nid) = object.strip_prefix("note:")
                    && let Ok(nid) = Ulid::from_string(nid)
                    && !dominated
                    && is_body_write(op, &nid)
                {
                    match tombstone_rule(tx, op, &nid)? {
                        TombstoneOutcome::Wins => {
                            // v0.6.12 (L1): a write the lapide holds still
                            // records its photograph and consumes its
                            // version slot — the writer's copy applied and
                            // bumped it, and the numbering must not drift
                            // between copies
                            if ops::records_a_version(op.kind)
                                && !note_purged_in_set(tx, &nid)?
                                && let Some(row) = ops::read_note_row(tx, &nid)?
                            {
                                ops::snapshot(tx, &nid, row.version, &op.op_id)?;
                                consume_version_slot(tx, &nid)?;
                            }
                            continue;
                        }
                        TombstoneOutcome::Reopens => ops::reopen_tombstone(tx, &nid)?,
                        TombstoneOutcome::NoTombstone => {}
                    }
                }
                match apply_half(tx, op, object, dominated) {
                    Ok(()) => {}
                    Err(CellError::NoteNotFound(_)) => {
                        mark_pending(tx, object, &op.op_id.to_string())?;
                    }
                    Err(e) => return Err(e),
                }
            }
            // v0.6.4 (D1): the conflict set derives at EVERY body write —
            // the arrival fast path pays the same derivation the replay
            // pays. The v0.6.1–v0.6.3 shortcut emptied the loser set
            // (the arrival is newest, its base the head): with the class
            // of the author a head-chained write can still LOSE to an
            // older human tip, and its copy must be born here too. A
            // write the lapide held never reaches this line.
            for object in &objects {
                if let Some(nid) = object.strip_prefix("note:")
                    && let Ok(nid) = Ulid::from_string(nid)
                    && is_body_write(op, &nid)
                {
                    super::conflicts::after_body_write(tx, &nid, op)?;
                }
            }
        } else {
            // the objects that DID take the fast path receive their HALF of
            // this operation (a two-object operation — a supersede, a
            // decide — splits along the scope); the others replay from the
            // whole set
            for object in &objects {
                if !to_materialize.contains(object) {
                    apply_half(tx, op, object, half_dominated_by_purge(tx, op, object)?)?;
                }
            }
            for object in &to_materialize {
                materialize(tx, object)?;
            }
        }
    }
    // v0.6.4: EVERY arrival for a note re-derives its conflict set —
    // body writes through their own hook (above), everything else
    // through the set-only rederivation: a node that completes the
    // graph moves a tip. And an operation on a COPY's id is a closure
    // witness the ORIGINAL's derivation must see — both, not either
    for object in &objects {
        if let Some(nid) = object.strip_prefix("note:")
            && let Ok(nid) = Ulid::from_string(nid)
            && !is_body_write(op, &nid)
        {
            super::conflicts::rederive_note(tx, &nid)?;
        }
        if let Some(nid) = object.strip_prefix("note:")
            && let Ok(nid) = Ulid::from_string(nid)
        {
            super::conflicts::rederive_original_of(tx, &nid)?;
        }
    }
    // a note that just landed may unblock PENDING edges that cite it (a
    // link whose note arrived after it, across a partition): they retry,
    // once, now — materialize_edge keeps them pending while the note is
    // still missing
    if touched_a_note {
        let pending_edges: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT object FROM pending_objects WHERE object LIKE 'edge:%'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for edge in pending_edges {
            materialize(tx, &edge)?;
        }
    }
    restore_hlc_watermark(tx, watermark, Some(&op.hlc))?;
    Ok(())
}

/// The half of one operation that concerns one object. `dominated` is
/// the purge's word on this half, already asked by the caller (the
/// arrival's fast path needs it before the application too, and a put
/// must not pay the question twice).
fn apply_half(
    tx: &rusqlite::Transaction<'_>,
    op: &Op,
    object: &str,
    dominated: bool,
) -> Result<(), CellError> {
    // v0.6.11 (K1): the purge dominates per HALF. The half of the purged
    // object, and a half that only references the purged one (a closing,
    // an edge row) do not apply; the OTHER object's own content does —
    // without everything that names the purged id, which the apply
    // guards take away.
    if dominated {
        // v0.6.12 (L1): a dominated half still records the photograph of
        // the pre-state it observes — the version row is the note's OWN
        // history, and the copy that receives the write after the purge
        // must hold the same row the writer's earlier application
        // recorded (the set, not the arrival order, decides the rows).
        // The kinds that never photographed anything record nothing here
        // either; a purged note has no row left to photograph.
        if ops::records_a_version(op.kind)
            && let Some(nid) = object.strip_prefix("note:")
            && let Ok(nid) = Ulid::from_string(nid)
            && !note_purged_in_set(tx, &nid)?
            && let Some(row) = ops::read_note_row(tx, &nid)?
        {
            ops::snapshot(tx, &nid, row.version, &op.op_id)?;
            consume_version_slot(tx, &nid)?;
        }
        return Ok(());
    }
    let Some(id) = object.strip_prefix("note:") else {
        // an edge or the policy: the operation's whole state step is that
        // object's half
        return ops::apply_scoped(tx, op, Scope::Full);
    };
    let id = Ulid::from_string(id)
        .map_err(|_| CellError::Invalid(format!("bad object key `{object}`")))?;
    ops::apply_scoped(tx, op, Scope::Note(id))
}

/// Rematerialize one object from the whole set of its operations.
pub(crate) fn materialize(tx: &rusqlite::Transaction<'_>, object: &str) -> Result<(), CellError> {
    if let Some(id) = object.strip_prefix("note:") {
        let id = Ulid::from_string(id)
            .map_err(|_| CellError::Invalid(format!("bad object key `{object}`")))?;
        materialize_note(tx, &id, object)
    } else if object == "policy" {
        materialize_policy(tx)
    } else if let Some(edge_id) = object.strip_prefix("edge:") {
        materialize_edge(tx, edge_id, object)
    } else {
        Err(CellError::Invalid(format!("unknown object `{object}`")))
    }
}

/// Does this operation CREATE the note `id` (so the object materializes even
/// while the row is absent)?
fn op_creates_note(op: &Op, id: &Ulid) -> bool {
    let note_id = op.note_id.is_some_and(|n| n == *id);
    match op.kind {
        OpKind::PutNote | OpKind::Propose => note_id,
        OpKind::Supersede => op
            .body
            .as_ref()
            .and_then(|b| b.get("note"))
            .and_then(|n| n.get("id"))
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Ulid::from_string(s).ok())
            .is_some_and(|n| n == *id),
        _ => false,
    }
}

fn note_exists(tx: &rusqlite::Transaction<'_>, id: &Ulid) -> Result<bool, CellError> {
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM notes WHERE id = ?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Rematerialize one note: the state ABOVE the compaction cut goes (the
/// note row and everything derived from it — versions, tags, its declared
/// edges, its queue rows, its FTS row in whichever index holds it — but NOT
/// the manual edges that cite it: they are objects of their own), the state
/// at the cut comes back from the version snapshot the first above-cut
/// writer took, and every operation concerning it replays in total order,
/// each scoped to this note's half. An operation whose note is not there
/// yet (and does not create it) waits: the object stays pending, and the
/// next arrival replays the whole set again.
fn materialize_note(
    tx: &rusqlite::Transaction<'_>,
    id: &Ulid,
    object: &str,
) -> Result<(), CellError> {
    let all = ops_concerning(tx, object)?;
    // a pure conflict copy has no operations of its own: its whole state is
    // the derivation (a copy the user edited has ops, and replays them over
    // the derived base below)
    if all.is_empty() {
        if let Some((original, note)) = conflict_copy_state(tx, id)? {
            restore_conflict_copy(tx, id, &original, &note)?;
        }
        clear_pending(tx, object)?;
        return Ok(());
    }
    let cut = cut_seq(tx)?;
    let watermark: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'hlc'", [], |r| r.get(0))
        .optional()?;
    // --- the state of the note at the cut. The cut froze what the ≤-cut
    // operations produced (their payloads are gone; they cannot replay),
    // so the anchor is the FIRST above-cut operation in TOTAL order that
    // photographed this note (v0.6.12, L1). Whatever applied that
    // operation — the arrival fast path or a later materialization's
    // replay — the photograph was taken at the note's state with every
    // operation sorting before it applied; the first one above the cut in
    // total order has exactly the ≤-cut set before it, so its row IS the
    // state at the cut. The v0.6.8–v0.6.11 rule (the first arrival whose
    // `base` points below the cut) read the author's promise where the
    // photograph's content is what matters, and missed the halves whose
    // base belongs to the OTHER note — a decide closing a target: its
    // base leans on the proposal, and the target, left without an
    // anchor, DIED in the replay instead of reopening. A note born above
    // the cut (no operations at or below it) photographs nothing at its
    // birth and anchors nothing: replay from zero. With no cut there is
    // no prefix at all and everything replays from zero. The anchor row
    // is frozen with the prefix (rule 4): this function READS it, and
    // the write-back below puts back byte for byte what it read — the
    // anchor cannot age with the cut again (H2's second layer).
    let mut anchor_row: Option<(String, i64, String, String)> = None;
    // v0.6.13 (M1): the derived half of the WHOLE-OBJECT anchor — the
    // queue row, the registry rows and the `supersedes` rows of the new
    // side as they were when the anchor's photograph was taken. The
    // photograph of the note and the photograph of its object are taken
    // by the same operation (`ops::snapshot`), so the same op id answers
    // both; when the photograph is missing (a cell the older engine
    // compacted, before the migration could back-fill it) the LIVE
    // derived rows stand in — read here, before the delete step below
    // takes them away.
    let mut derived_anchor: Option<String> = None;
    let cut_state: Option<(crate::format::Note, i64)> =
        if cut > 0 && note_exists_at(tx, object, cut)? {
            let mut stmt = tx.prepare(
                "SELECT o.op_id FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.seq > ?2
             ORDER BY o.hlc ASC, o.op_id ASC",
            )?;
            let op_ids: Vec<String> = stmt
                .query_map(rusqlite::params![object, cut], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            let mut anchor: Option<(String, i64, String)> = None;
            for op_id in op_ids {
                let snap: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT snapshot_json, version FROM note_versions
                     WHERE op_id = ?1 AND note_id = ?2",
                        rusqlite::params![op_id, id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((json, version)) = snap {
                    anchor = Some((op_id, version, json));
                    break;
                }
            }
            match anchor {
                None => None,
                Some((op_id, version, json)) => {
                    let note = crate::format::Note::from_json(
                        &serde_json::from_str::<serde_json::Value>(&json)
                            .map_err(|e| CellError::Db(format!("corrupt version snapshot: {e}")))?,
                    )
                    .map_err(|e| CellError::Db(format!("corrupt version snapshot: {e}")))?;
                    anchor_row = Some((id.to_string(), version, op_id.clone(), json));
                    derived_anchor = tx
                        .query_row(
                            "SELECT anchor_json FROM object_anchors
                         WHERE note_id = ?1 AND op_id = ?2",
                            rusqlite::params![id.to_string(), op_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    Some((note, version))
                }
            }
        } else {
            None
        };

    // v0.6.12 (L1): the LIVE ROW before the delete step. When the note
    // existed at the cut but NO above-cut operation ever photographed it,
    // the live row IS the state at the cut — every operation that changes
    // a note photographs it first, so a note with above-cut operations
    // and no photograph is a note those operations never touched (a
    // reject-decide, a held write, a pending one): without this fallback
    // the replay started from zero and the note DIED instead of keeping
    // the frozen state (found by the oracle, seed 765420891842479779).
    let live_row: Option<(crate::format::Note, i64)> =
        if cut > 0 && note_exists_at(tx, object, cut)? {
            ops::read_note_row(tx, id)?.map(|r| (r.note, r.version))
        } else {
            None
        };
    // no photograph above the cut: the live row read below IS the state
    // at the cut (the note existed there, and nothing above the cut ever
    // changed it — else there would be a photograph)
    let mut cut_state = cut_state.or(live_row);
    // v0.6.13 (M1): a closing at or below the cut whose closer the set
    // purged is work the anchor must not resurrect — the frozen state
    // goes back to what it was before the closing, exactly like the
    // whole-log replay leaves it (there the dominated closing never
    // applies)
    if let Some((note, _)) = &mut cut_state {
        correct_anchor_for_dominated_closing(tx, id, cut, note)?;
    }
    // the derived rows read BEFORE the delete step: the fallback for a
    // photograph the older engine never took (v0.6.13, M1 — the
    // migration back-fills what it can; what it cannot, the live rows
    // hold, and the idempotent replay of the tail converges over them)
    let derived_live = if cut > 0 && note_exists_at(tx, object, cut)? {
        Some(ops::derived_anchor_json(tx, id)?)
    } else {
        None
    };
    let derived_anchor = derived_anchor.or(derived_live);
    // a purged note keeps nothing: not the note row, not its object
    let purged = note_purged_in_set(tx, id)?;
    // --- delete the current state of the note
    if let Some(row) = ops::read_note_row(tx, id)?
        && let Some(idx) = ops::index_for(row.note.status)
    {
        // the delete must find the row in the index it lives in, exactly
        // as `purge_state` guards: a delete for a rowid that is not
        // there corrupts FTS5's accounting
        let table = idx.table();
        let in_index: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM {table}_docsize WHERE id = ?1"),
            [row.rowid],
            |r| r.get(0),
        )?;
        if in_index > 0 {
            ops::fts_delete_row(
                tx,
                idx,
                row.rowid,
                row.note.title.as_deref().unwrap_or(""),
                &row.body_norm,
                &row.tags_text,
            )?;
        }
    }
    let id_str = id.to_string();
    tx.execute("DELETE FROM notes WHERE id = ?1", [&id_str])?;
    // only the versions the above-cut writers recorded: the ones at or
    // below the cut are the frozen history the cut state came from
    for op in &all {
        tx.execute(
            "DELETE FROM note_versions WHERE note_id = ?1 AND op_id = ?2",
            rusqlite::params![id_str, op.op_id.to_string()],
        )?;
    }
    tx.execute("DELETE FROM note_tags WHERE note_id = ?1", [&id_str])?;
    // the note's DECLARED edges are its own; the manual ones are objects of
    // their own and survive this note's rematerialization
    tx.execute(
        "DELETE FROM edges WHERE note_id = ?1 AND id LIKE 'd%'",
        [&id_str],
    )?;
    tx.execute("DELETE FROM supersedes WHERE new_id = ?1", [&id_str])?;
    tx.execute("DELETE FROM proposals WHERE note_id = ?1", [&id_str])?;
    tx.execute("DELETE FROM decisions WHERE proposal = ?1", [&id_str])?;
    // --- the state at the cut comes back first, at its exact version; a
    // note that is a conflict copy starts from its derivation instead
    if let Some((note, version)) = &cut_state {
        ops::restore_note(tx, note, *version)?;
        ops::reindex_note_row(tx, id)?;
        // v0.6.13 (M1): the object comes back WHOLE — the derived rows
        // the delete step took return at the state the anchor
        // photographed, and the tail replays over them. The tags and
        // the declared edges are functions of the note row (the reindex
        // above rebuilt them); the queue, the registry and the
        // `supersedes` rows of the new side are not.
        if let Some(json) = &derived_anchor
            && !purged
        {
            ops::restore_derived_anchor(tx, id, json)?;
        }
    } else if let Some((original, note)) = conflict_copy_state(tx, id)? {
        restore_conflict_copy(tx, id, &original, &note)?;
    }
    // --- replay in total order, scoped to this note, under the conflict
    // rules (DESIGN §4.3): the last body write in total order wins, every
    // body write it did not see survives as a CONFLICT COPY, a tombstone
    // beats every modification except one whose base it is, and a delete
    // that did not see the intervening writes lands on the state ITS author
    // had applied
    let mut pending_since: Option<String> = None;
    let mut events: Vec<ReplayEvent> = Vec::new();
    // (`purged` was read before the delete step — v0.6.13, M1: the
    // restore of the object refuses it for a purged note, whose every
    // row the right to be forgotten takes)
    // v0.6.4: the states the lapide's rollback erases — a lived copy's
    // birth row still wants its loser's state
    let mut erased_states: std::collections::HashMap<String, crate::format::Note> =
        std::collections::HashMap::new();
    for op in &all {
        // v0.6.11 (K1): the purge dominates the replay per HALF — this
        // note's half of the operation goes when the note itself is
        // purged, or when the half only REFERENCES a purged note (the
        // closing of a supersede's old side, of a decide's target: the
        // v0.6.7 guard (G4) asked «is THIS object purged?» and those
        // closings came back at every rematerialization, seed
        // `14903861504949011020`). The v0.6.10 rule (J1) skipped the
        // WHOLE operation, and the new note of a supersede whose OLD
        // side was purged stopped materializing: another note's own
        // content, which a purge never takes. The ops' payloads are
        // loaded ABOVE, before this loop runs — a purge sorting EARLIER
        // than a body write in the total order redacts the log's copy
        // (`scrub_log`, inside `apply_purge`) but not the in-memory one,
        // and the write would resurrect the note the purge had already
        // taken. DESIGN § 4.3: «X non materializza» — the purge ops
        // themselves keep applying (their scrub IS the state they
        // carry), and so does the revert (its own Purged answer came
        // first).
        if half_dominated_by_purge(tx, op, object)? {
            // v0.6.12 (L1): a dominated half still records the photograph
            // of the pre-state it observed. The version row is the note's
            // OWN history — «mai il contenuto proprio di un'altra nota» —
            // and a `restore` above the cut may name it (the open defect:
            // the closing's photograph went with the dominated half, and
            // the purge failed forever with `VersionNotFound`). Recording
            // here keeps the row a function of the set: the replay
            // re-records on every copy what the first application
            // recorded on the one that saw the write before the purge.
            // The kinds that never photographed anything (a link, a
            // propose) record nothing here either; a purged note keeps
            // nothing at all.
            if ops::records_a_version(op.kind)
                && !purged
                && let Some(row) = ops::read_note_row(tx, id)?
            {
                ops::snapshot(tx, id, row.version, &op.op_id)?;
                // the held half still CONSUMES its version slot: the
                // photograph keeps the number it had where the write
                // first applied, and the `restore` that names it finds
                // the row (the numbering cannot drift with the set)
                consume_version_slot(tx, id)?;
            }
            events.push(ReplayEvent {
                hash: op.content_hash.clone(),
                body: None,
            });
            continue;
        }
        // v0.6.1 (P0): an operation whose base is nowhere in the log has
        // not been ordered yet — the author leaned on a past this copy does
        // not hold. It waits (the object turns pending) and applies when
        // the base lands. The wait binds only the object the base belongs
        // to (`op.note_id`'s note): the OTHER halves of a two-object
        // operation — the new note a supersede creates, the target a decide
        // closes — materialize here regardless. A purged note's ops lean on
        // a note that will never materialize and wait for nothing.
        if !purged && op.note_id.is_some_and(|n| n == *id) && base_unknown(tx, op)? {
            pending_since = Some(op.op_id.to_string());
            events.push(ReplayEvent {
                hash: op.content_hash.clone(),
                body: None,
            });
            continue;
        }
        // a purge always applies, on an empty object too: its scrub IS the
        // state it carries (the bodies it must forget arrived before it)
        if !note_exists(tx, id)? && !op_creates_note(op, id) && op.kind != OpKind::Purge {
            if !purged {
                pending_since = Some(op.op_id.to_string());
            }
            // a purged note's ops lean on a note that will never
            // materialize: recorded, redacted, not waiting
            events.push(ReplayEvent {
                hash: op.content_hash.clone(),
                body: None,
            });
            continue;
        }
        if is_body_write(op, id) {
            match tombstone_rule(tx, op, id)? {
                TombstoneOutcome::Wins => {
                    // the tombstone wins: the write is stored, without
                    // effect. v0.6.12 (L1): the photograph of the pre-state
                    // it observed stays anyway — the version row is the
                    // note's OWN history (the same rule as the dominated
                    // half below: the lapide suppresses the EFFECT, and a
                    // `restore` above may name the version the write
                    // recorded where it first applied)
                    if !purged && let Some(row) = ops::read_note_row(tx, id)? {
                        ops::snapshot(tx, id, row.version, &op.op_id)?;
                        // the held write still consumes its version slot
                        // (the original application bumped it; a
                        // `restore` above may name that number)
                        consume_version_slot(tx, id)?;
                    }
                    events.push(ReplayEvent {
                        hash: op.content_hash.clone(),

                        body: None,
                    });
                    continue;
                }
                TombstoneOutcome::Reopens => ops::reopen_tombstone(tx, id)?,
                TombstoneOutcome::NoTombstone => {}
            }
            match ops::apply_scoped(tx, op, Scope::Note(*id)) {
                Ok(()) => {}
                Err(CellError::NoteNotFound(_)) => {
                    pending_since = Some(op.op_id.to_string());
                }
                Err(e) => return Err(e),
            }
            if let Some(row) = ops::read_note_row(tx, id)? {
                events.push(ReplayEvent {
                    hash: op.content_hash.clone(),

                    body: Some((row.note, row.version)),
                });
            }
        } else {
            if op.kind == OpKind::DeleteNote
                && let Some(base) = op.base.as_deref()
                && let Some(pos) = events.iter().position(|e| e.hash == base)
                && events[pos + 1..].iter().any(|e| e.body.is_some())
            {
                // the delete did not see the writes after its base: they go.
                // The tombstone lands on the state the deleter had applied —
                // last body state at or before the base, at ITS version.
                if let Some((state, version)) =
                    events[..=pos].iter().rev().find_map(|e| e.body.clone())
                {
                    replace_note_state(tx, id, &state, version)?;
                }
                // v0.6.4 (D1): the lapide's rule stays today's — the writes
                // the delete did not see are GONE, and they are gone for the
                // conflict rules too: no tips, no copies. A body the user's
                // delete explicitly discarded unseen must not come back as a
                // "winner" the derivation resurrects over the tombstone.
                // But a copy that LIVED (it has operations of its own) keeps
                // its birth row — and the row's state comes from here, before
                // the erasure (found by the 20 000 hunt, seed
                // 9942741803736828218)
                for e in events.iter_mut().skip(pos + 1) {
                    if let Some((state, _)) = e.body.take() {
                        erased_states.insert(e.hash.clone(), state);
                    }
                }
            }
            match ops::apply_scoped(tx, op, Scope::Note(*id)) {
                Ok(()) => {}
                // the object the operation leans on is not here yet: it waits
                Err(CellError::NoteNotFound(_)) => {
                    pending_since = Some(op.op_id.to_string());
                }
                Err(e) => return Err(e),
            }
            events.push(ReplayEvent {
                hash: op.content_hash.clone(),
                body: None,
            });
        }
    }
    if let Some(since) = pending_since {
        mark_pending(tx, object, &since)?;
    } else {
        clear_pending(tx, object)?;
    }
    // THE CONFLICT SET (v0.6.4, D1): derived once, at the end of the
    // replay, from the SET — the tips the ancestry graph leaves, the
    // winner by (class, hlc, op_id), the durable copies with their
    // closure witnesses and their cap. Every copy of the cell walks the
    // same graph over the same set and lands on the same rows,
    // however the operations arrived.
    let applied: std::collections::HashSet<String> = events
        .iter()
        .filter(|e| e.body.is_some())
        .map(|e| e.hash.clone())
        .collect();
    let mut states: std::collections::HashMap<String, crate::format::Note> = events
        .iter()
        .filter_map(|e| {
            e.body
                .as_ref()
                .map(|(state, _)| (e.hash.clone(), state.clone()))
        })
        .collect();
    states.extend(erased_states);
    super::conflicts::after_replay(tx, id, &applied, &states)?;
    // v0.6.8 (H6): a note that IS a conflict copy and holds a LIVING
    // row keeps its `conflitto` edge, whatever branch of the
    // materialization wrote the row (the restore path always did; the
    // cut-state path rebuilt the row without it, and the checkpoint
    // coherence cried wolf — seed `9012498534642822410` reduced)
    {
        let original: Option<String> = tx
            .query_row(
                "SELECT original_id FROM conflict_copies WHERE copy_id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(original) = original
            && let Ok(original) = Ulid::from_string(&original)
            && let Some(row) = ops::read_note_row(tx, id)?
            && row.note.status != crate::format::Status::Deleted
        {
            write_conflict_edge(tx, id, &original)?;
        }
    }
    // v0.6.10 (J1, the hunt's seeds 4815… and 3695…): a note whose
    // replay left NO ROW (its creation was dominated — or it never had
    // one) cannot leave anything that NAMES it: a `supersedes` row would
    // point at a note that will never materialize (whether it lingered
    // depended on which half materialized last), and its CONFLICT COPIES
    // — derived while it lived — are the derivation of a dead line (the
    // same closure the purge applies to the ids it forgets). The
    // `new_id` rows the pre-clean already took; these are the `old_id`
    // ones and the copies.
    if ops::read_note_row(tx, id)?.is_none() {
        tx.execute("DELETE FROM supersedes WHERE old_id = ?1", [id.to_string()])?;
        purge_conflict_copies(tx, id)?;
        // v0.6.10 (J1, the hunt's seed 9683753651273141986): a death
        // must re-derive everything that NAMES the note — the EDGES
        // (the purge's worklist does it for the co-objects it walks,
        // but a note can also die through a materialization triggered
        // by a late arrival, and that road skipped them) and the
        // entities, with the same prune `purge_state` speaks
        {
            let named = format!("note:{id}");
            let mut stmt = tx.prepare(
                "SELECT DISTINCT 'edge:' || e.id FROM edges e
                 WHERE e.id NOT LIKE 'd%'
                   AND (e.src = ?2 OR e.dst = ?2 OR e.note_id = ?1)",
            )?;
            let edge_objects: Vec<String> = stmt
                .query_map(rusqlite::params![id.to_string(), named], |r| r.get(0))?
                .flatten()
                .collect();
            drop(stmt);
            for edge in edge_objects {
                materialize(tx, &edge)?;
            }
            tx.execute(
                "UPDATE entities SET note_id = NULL WHERE note_id = ?1",
                [id.to_string()],
            )?;
            tx.execute(
                "UPDATE entities SET note_id = (
                     SELECT n.id FROM notes n
                     WHERE n.title IS entities.name COLLATE NOCASE
                       AND n.status != 'deleted'
                     ORDER BY n.updated_at DESC, n.id LIMIT 1
                 )
                 WHERE note_id IS NULL
                   AND EXISTS (SELECT 1 FROM notes n
                               WHERE n.title IS entities.name COLLATE NOCASE
                                 AND n.status != 'deleted')",
                [],
            )?;
            tx.execute(
                "DELETE FROM entities WHERE id NOT IN (
                     SELECT e.id FROM entities e
                     JOIN edges ed ON ed.src = e.name OR ed.dst = e.name
                 )
                 AND NOT EXISTS (
                     SELECT 1 FROM notes n
                     WHERE n.id = entities.note_id
                       AND n.status != 'deleted'
                       AND n.title IS entities.name COLLATE NOCASE
                 )",
                [],
            )?;
        }
    }
    // v0.6.10 (J1): the tags table is DERIVED from note_tags — a
    // materialization that changed what the note carries (or that left
    // no note at all) must not leave orphans behind. The redacted
    // replay never creates them; the live state must not keep them (the
    // hunt's seed 13144595116571598162: the tags of a note whose line
    // died survived every path that deleted its rows)
    tx.execute(
        "DELETE FROM tags WHERE NOT EXISTS (
             SELECT 1 FROM note_tags nt
             WHERE nt.tag = tags.name OR nt.tag LIKE tags.name || '/%'
         )",
        [],
    )?;
    // v0.6.12 (L1, rule 4): **the anchor does not age.** The delete step
    // and the replay above may have taken the anchor row away or
    // replaced it under another op id (a header-only first writer
    // re-records the key under its own); what the entry READ is what the
    // exit writes back, byte for byte. Only the cut ever changes which
    // row is the anchor, and by induction the row always holds the state
    // at the cut it was born against — the aging of H2's second layer
    // (an anchor that «carries the number it reached against the cut of
    // then») cannot be written anymore. A purged note writes nothing,
    // its anchor included.
    if let Some((note_id, version, op_id, json)) = anchor_row
        && !purged
    {
        tx.execute(
            "INSERT INTO note_versions (note_id, version, op_id, snapshot_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(note_id, version) DO UPDATE SET
                 op_id = ?3, snapshot_json = ?4",
            rusqlite::params![note_id, version, op_id, json],
        )?;
    }
    restore_hlc_watermark(tx, watermark, None)?;
    Ok(())
}

/// One applied operation of the replay, with what the conflict rules
/// need: its wire identity and, for a BODY write that applied, the note
/// state it produced (at its version).
struct ReplayEvent {
    hash: String,
    body: Option<(crate::format::Note, i64)>,
}

/// Replace the note's row with a replayed state, at that state's OWN
/// version (the delete rollback: the intervening writes vanish without a
/// trace in the version chain, exactly as they would have if they had
/// never arrived before the delete). The version rows stay: they are the
/// history the replay is re-deriving.
fn replace_note_state(
    tx: &rusqlite::Transaction<'_>,
    id: &Ulid,
    note: &crate::format::Note,
    version: i64,
) -> Result<(), CellError> {
    if let Some(row) = ops::read_note_row(tx, id)?
        && let Some(idx) = ops::index_for(row.note.status)
    {
        let table = idx.table();
        let in_index: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM {table}_docsize WHERE id = ?1"),
            [row.rowid],
            |r| r.get(0),
        )?;
        if in_index > 0 {
            ops::fts_delete_row(
                tx,
                idx,
                row.rowid,
                row.note.title.as_deref().unwrap_or(""),
                &row.body_norm,
                &row.tags_text,
            )?;
        }
    }
    tx.execute("DELETE FROM notes WHERE id = ?1", [id.to_string()])?;
    ops::restore_note(tx, note, version)?;
    ops::reindex_note_row(tx, id)?;
    Ok(())
}

/// Is the operation's `base` KNOWN to this copy — in the log above the cut,
/// or below it as a skeleton? With the identity immutable (v0.6.1, P0), a
/// base that is nowhere in `ops` means exactly one thing: it has not
/// arrived yet. The operation WAITS (`pending_objects`), like a patch
/// before its put — it applies when the base lands, and never before: an
/// operation must never take effect over a past its author saw and this
/// copy does not hold. (Before v0.6.1 the censor rewrote the content_hash
/// of what it censored, so a base could be missing on one copy and present
/// on another, and "absent" had to be read as "sequential" — the rule that
/// made the conflict set depend on the arrival order.)
fn base_unknown(tx: &rusqlite::Transaction<'_>, op: &Op) -> Result<bool, CellError> {
    let Some(base) = op.base.as_deref() else {
        return Ok(false);
    };
    let known: Option<i64> = tx
        .query_row("SELECT seq FROM ops WHERE content_hash = ?1", [base], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(known.is_none())
}

/// The newest operation of one object strictly before `(hlc, op_id)` —
/// above the compaction cut when there is one, and otherwise over the WHOLE
/// log: the below-cut rows are redacted skeletons, but they still carry
/// their HLC and their content hash, which is all ordering ever asks of
/// them. Without the whole-log fallback, a sequential operation whose base
/// sits at or below the cut (the first tail write after a compaction — the
/// bootstrap's own shape) would look "concurrent with nothing" and force a
/// materialization its own anchor cannot serve (v0.6.1, P0).
fn head_hash_before(
    tx: &rusqlite::Transaction<'_>,
    object: &str,
    hlc: &str,
    op_id: &str,
) -> Result<Option<String>, CellError> {
    let cut = cut_seq(tx)?;
    let above: Option<String> = tx
        .query_row(
            "SELECT o.content_hash FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.seq > ?2
               AND (o.hlc < ?3 OR (o.hlc = ?3 AND o.op_id < ?4))
             ORDER BY o.hlc DESC, o.op_id DESC LIMIT 1",
            rusqlite::params![object, cut, hlc, op_id],
            |r| r.get(0),
        )
        .optional()?;
    if above.is_some() {
        return Ok(above);
    }
    let anywhere: Option<String> = tx
        .query_row(
            "SELECT o.content_hash FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1
               AND (o.hlc < ?2 OR (o.hlc = ?2 AND o.op_id < ?3))
             ORDER BY o.hlc DESC, o.op_id DESC LIMIT 1",
            rusqlite::params![object, hlc, op_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(anywhere)
}

/// Does this arrival BREAK THE CHAIN of its note — did its author lean on
/// a state this copy has already moved past? A write or a delete whose
/// `base` is not the newest operation of the note (in total order, before
/// this one) is CONCURRENT with something already applied: the conflict
/// rules must run, and only the materializer knows them. Pre-v0.6
/// operations (no base) are sequential by declaration and never break
/// anything.
fn breaks_the_chain(
    tx: &rusqlite::Transaction<'_>,
    object: &str,
    op: &Op,
) -> Result<bool, CellError> {
    if !matches!(
        op.kind,
        OpKind::PutNote | OpKind::PatchNote | OpKind::DeleteNote
    ) || op.body.is_none()
    {
        return Ok(false);
    }
    let Some(base) = op.base.as_deref() else {
        // a BASE-LESS body write over a note that already lives here is
        // CONCURRENT by construction: its author created the note fresh,
        // and this copy holds someone else's creation of the very same id
        // (the same import on two machines — found by the three-device
        // test). The conflict rules must run: without this, the LAST
        // ARRIVAL won and the copies diverged by delivery order.
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM notes WHERE id = ?1",
            [op.note_id.map(|n| n.to_string()).unwrap_or_default()],
            |r| r.get(0),
        )?;
        return Ok(exists > 0);
    };
    let head = head_hash_before(tx, object, &op.hlc.to_string(), &op.op_id.to_string())?;
    Ok(head.as_deref() != Some(base))
}

/// Does this operation write the BODY of note `id` (a full replace or a
/// patch whose `set` carries one)? A header-only patch — title, tags,
/// state, validity, confidence… — is NEVER a body write (v0.6.4, D1):
/// the header stays last-write-per-field and never creates conflicts.
fn is_body_write(op: &Op, id: &Ulid) -> bool {
    if op.body.is_none() {
        return false;
    }
    match op.kind {
        OpKind::PatchNote => {
            op.note_id.is_some_and(|n| n == *id)
                && op
                    .body
                    .as_ref()
                    .and_then(|b| b.get("set"))
                    .and_then(|set| set.get("body"))
                    .is_some()
        }
        OpKind::PutNote => op
            .body
            .as_ref()
            .and_then(|b| b.get("note"))
            .and_then(|n| n.get("id"))
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Ulid::from_string(s).ok())
            .is_some_and(|n| n == *id),
        _ => false,
    }
}

/// The tombstone rule on a body write: a note deleted at the write's
/// position reopens ONLY for a writer whose `base` is the newest operation
/// of the note at that point — whoever chained onto the state that is
/// deleted now, be it the lapide itself or a later op that left it deleted
/// (a restore of a deleted version). A writer that leaned on anything
/// older did not see what deleted the note, and the lapide wins.
/// `NoTombstone` when the note is not deleted at all.
enum TombstoneOutcome {
    NoTombstone,
    Wins,
    Reopens,
}

fn tombstone_rule(
    tx: &rusqlite::Transaction<'_>,
    op: &Op,
    id: &Ulid,
) -> Result<TombstoneOutcome, CellError> {
    let Some(row) = ops::read_note_row(tx, id)? else {
        return Ok(TombstoneOutcome::NoTombstone);
    };
    if row.note.status != crate::format::Status::Deleted {
        return Ok(TombstoneOutcome::NoTombstone);
    }
    let Some(base) = op.base.as_deref() else {
        return Ok(TombstoneOutcome::Wins);
    };
    // v0.6.5 (E1): the writer saw the deletion iff it chained onto the
    // CLOSED LINE — the lapide, or whoever after her left the note closed
    // (the same head the derivation lends a local write). The note's
    // NEWEST operation is not that: a concurrent loser newer than the
    // lapide never saw it, and demanding its hash kept the note closed
    // for every writer that had (the reviewer's shape, on the lapide).
    // The walk is over the SET (a recursive descent of the lapides
    // before this write), so the answer never depends on the arrival
    // order — the replay asks it at any position
    let head: Option<String> = tx
        .query_row(
            "WITH RECURSIVE closed_line(hash) AS (
                 SELECT o.content_hash FROM ops o
                 JOIN op_objects oo ON oo.op_id = o.op_id
                 WHERE oo.object = ?1 AND o.kind = 'delete_note'
                   AND (o.hlc < ?2 OR (o.hlc = ?2 AND o.op_id < ?3))
                 UNION
                 SELECT o2.content_hash FROM ops o2
                 JOIN op_objects oo2 ON oo2.op_id = o2.op_id
                 JOIN closed_line ON o2.base = closed_line.hash
                 WHERE oo2.object = ?1
             )
             SELECT o.content_hash FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.content_hash IN (SELECT hash FROM closed_line)
               AND (o.hlc < ?2 OR (o.hlc = ?2 AND o.op_id < ?3))
             ORDER BY o.hlc DESC, o.op_id DESC LIMIT 1",
            rusqlite::params![
                format!("note:{id}"),
                op.hlc.to_string(),
                op.op_id.to_string()
            ],
            |r| r.get(0),
        )
        .optional()?;
    if head.as_deref() == Some(base) {
        Ok(TombstoneOutcome::Reopens)
    } else {
        Ok(TombstoneOutcome::Wins)
    }
}

/// v0.6.13 (M1): the NEWEST closing of a note — the `supersede` or
/// `decide` operation that closed it over another note (the closer),
/// with the word of the set on that closer: when the closer is PURGED,
/// the closing was its work, and the right to be forgotten (K1: the
/// closing of a supersede's old side is the NEW note's work) takes it
/// back — a note the anchor froze CLOSED must reopen, exactly like the
/// replay of the whole log would leave it (the no-cut world: the
/// dominated closing never applies). For a `supersede` the note is the
/// OLD side (`note_id`); for a `decide` it is the TARGET (the proposal
/// is `note_id`). The `op_objects` index answers both — the ids only,
/// alive under any redaction, so a closing at or below the cut is
/// found long after its payload is gone.
pub(crate) fn newest_closing_of(
    tx: &rusqlite::Transaction<'_>,
    id: &Ulid,
) -> Result<Option<(String, String, bool)>, CellError> {
    // (op_id, closer object, dominated?)
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT o.op_id,
                    CASE WHEN o.note_id = ?1 THEN oo2.object ELSE o.note_id END
             FROM ops o
             JOIN op_objects oo1 ON oo1.op_id = o.op_id AND oo1.object = ?2
             JOIN op_objects oo2 ON oo2.op_id = o.op_id
                  AND oo2.object != ?2 AND oo2.object LIKE 'note:%'
             WHERE o.kind IN ('supersede', 'decide')
               AND ((o.kind = 'supersede' AND o.note_id = ?1)
                    OR (o.kind = 'decide' AND o.note_id IS NOT NULL AND o.note_id != ?1))
             ORDER BY o.seq DESC LIMIT 1",
            rusqlite::params![id.to_string(), format!("note:{id}")],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((op_id, closer)) = row else {
        return Ok(None);
    };
    let dominated = closer
        .strip_prefix("note:")
        .and_then(|n| Ulid::from_string(n).ok())
        .map(|n| note_purged_in_set(tx, &n))
        .transpose()?
        .unwrap_or(false);
    Ok(Some((op_id, closer, dominated)))
}

/// v0.6.13 (M1): the state a note had BEFORE its newest closing — the
/// photograph the closing operation itself took (a closing always
/// photographs the old side), when the cell still holds it.
fn pre_closing_state(
    tx: &rusqlite::Transaction<'_>,
    id: &Ulid,
    closing_op: &str,
) -> Result<Option<crate::format::Note>, CellError> {
    let json: Option<String> = tx
        .query_row(
            "SELECT snapshot_json FROM note_versions WHERE note_id = ?1 AND op_id = ?2",
            rusqlite::params![id.to_string(), closing_op],
            |r| r.get(0),
        )
        .optional()?;
    let Some(json) = json else {
        return Ok(None);
    };
    crate::format::Note::from_json(
        &serde_json::from_str::<serde_json::Value>(&json)
            .map_err(|e| CellError::Db(format!("corrupt version snapshot: {e}")))?,
    )
    .map(Some)
    .map_err(|e| CellError::Db(format!("corrupt version snapshot: {e}")))
}

/// v0.6.13 (M1): undo, in the anchor's state, a closing at or below the
/// cut whose closer the set purged — the anchor froze the CLOSED state
/// and the tail cannot un-close it (the closing's payload is gone).
/// Status and validity alone: the rest of the object is the note's own,
/// and the version numbering already aligns (a dominated half still
/// consumes its slot, v0.6.12 L1). The correction is the same word the
/// replay of the whole log speaks, where the dominated closing never
/// applies.
fn correct_anchor_for_dominated_closing(
    tx: &rusqlite::Transaction<'_>,
    id: &Ulid,
    cut: i64,
    note: &mut crate::format::Note,
) -> Result<(), CellError> {
    if note.status != crate::format::Status::Superseded {
        return Ok(());
    }
    let Some((op_id, _closer, dominated)) = newest_closing_of(tx, id)? else {
        return Ok(());
    };
    if !dominated {
        return Ok(());
    }
    let seq: Option<i64> = tx
        .query_row("SELECT seq FROM ops WHERE op_id = ?1", [&op_id], |r| {
            r.get(0)
        })
        .optional()?;
    if seq.is_none_or(|s| s > cut) {
        // a dominated closing ABOVE the cut never applied: the anchor
        // cannot embed it, and the tail will skip it on its own
        return Ok(());
    }
    if let Some(before) = pre_closing_state(tx, id, &op_id)? {
        note.status = before.status;
        note.valid_to = before.valid_to;
    }
    Ok(())
}

/// The deterministic id of a conflict copy (DESIGN §4.3):
/// `blake3("engramma/conflict/v1" ‖ content_hash del perdente)` in ULID
/// form, the losing operation's HLC as the time part.
pub(crate) fn conflict_copy_id(loser_hash: &str, loser_hlc_ms: u64) -> Ulid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"engramma/conflict/v1");
    hasher.update(loser_hash.as_bytes());
    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let mut randomness: u128 = 0;
    for b in &bytes[..10] {
        randomness = (randomness << 8) | u128::from(*b);
    }
    Ulid::from_parts(loser_hlc_ms, randomness)
}

/// The derivation record of a conflict copy, when this note IS one.
pub(crate) fn conflict_copy_state(
    tx: &rusqlite::Transaction<'_>,
    id: &Ulid,
) -> Result<Option<(Ulid, crate::format::Note)>, CellError> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT original_id, state_json FROM conflict_copies WHERE copy_id = ?1",
            [id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((original, json)) = row else {
        return Ok(None);
    };
    let original = Ulid::from_string(&original)
        .map_err(|_| CellError::Db(format!("bad conflict original {original}")))?;
    let mut note = crate::format::Note::from_json(
        &serde_json::from_str::<serde_json::Value>(&json)
            .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?,
    )
    .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?;
    // v0.6.4 (D5): the stored state has no body — the loser's payload
    // is its home, and a loser is always a body write with its payload
    // (above the compaction cut, where the tips live)
    if note.body.is_empty()
        && let Some(body) = tx
            .query_row(
                "SELECT json_extract(p.body, '$.set.body') FROM op_payloads p
                 JOIN conflict_copies cc ON cc.loser_op = p.op_id
                 WHERE cc.copy_id = ?1 AND json_extract(p.body, '$.set.body') IS NOT NULL",
                [id.to_string()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
    {
        note.body = body;
    }
    Ok(Some((original, note)))
}

/// The deterministic id of a copy's `conflitto` edge (v0.6.8, H6: one
/// formula in one place — the derivation that writes it and the verify
/// that expects it speak the same word).
pub(crate) fn conflict_edge_id(copy_id: &Ulid) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"engramma/conflict-edge/v1");
    h.update(copy_id.to_string().as_bytes());
    format!("d{}", &h.finalize().to_hex()[..31])
}

/// Write (or rewrite) the `conflitto` edge of a copy toward its
/// original: derived (id `d%`), so the copy's own rematerialization
/// cleans and rebuilds it. The RULE (v0.6.8, H6): the edge exists
/// exactly while its copy holds a LIVING notes row — open and visible,
/// or the user's own (edited) — and goes with the row (a resolved
/// copy's tombstone, an invisible window, a closed derivation lose it).
pub(crate) fn write_conflict_edge(
    tx: &rusqlite::Transaction<'_>,
    copy_id: &Ulid,
    original: &Ulid,
) -> Result<(), CellError> {
    tx.execute(
        "INSERT OR REPLACE INTO edges (id, src, rel, dst, note_id, valid_from, valid_to,
                                       confidence, status)
         VALUES (?1, ?2, 'conflitto', ?3, ?4, NULL, NULL, 'stated', 'active')",
        rusqlite::params![
            conflict_edge_id(copy_id),
            format!("note:{copy_id}"),
            format!("note:{original}"),
            copy_id.to_string()
        ],
    )?;
    Ok(())
}

/// Write (or rewrite) the derived ROWS of one conflict copy: the note —
/// active, tagged `conflitto`, the loser's content — its FTS and tags, and
/// the edge to the original. Pure derivation: no operation of its own, and
/// every copy of the cell produces it identically.
pub(crate) fn restore_conflict_copy(
    tx: &rusqlite::Transaction<'_>,
    copy_id: &Ulid,
    original: &Ulid,
    note: &crate::format::Note,
) -> Result<(), CellError> {
    delete_note_state(tx, copy_id)?;
    ops::restore_note(tx, note, 0)?;
    ops::reindex_note_row(tx, copy_id)?;
    write_conflict_edge(tx, copy_id, original)?;
    Ok(())
}

/// Delete every state row of a note (no log rows touched), the
/// rematerialization's delete-first for derived copies.
pub(crate) fn delete_note_state(
    tx: &rusqlite::Transaction<'_>,
    id: &Ulid,
) -> Result<(), CellError> {
    if let Some(row) = ops::read_note_row(tx, id)?
        && let Some(idx) = ops::index_for(row.note.status)
    {
        let table = idx.table();
        let in_index: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM {table}_docsize WHERE id = ?1"),
            [row.rowid],
            |r| r.get(0),
        )?;
        if in_index > 0 {
            ops::fts_delete_row(
                tx,
                idx,
                row.rowid,
                row.note.title.as_deref().unwrap_or(""),
                &row.body_norm,
                &row.tags_text,
            )?;
        }
    }
    let id_str = id.to_string();
    tx.execute("DELETE FROM notes WHERE id = ?1", [&id_str])?;
    tx.execute("DELETE FROM note_versions WHERE note_id = ?1", [&id_str])?;
    tx.execute("DELETE FROM note_tags WHERE note_id = ?1", [&id_str])?;
    tx.execute(
        "DELETE FROM edges WHERE note_id = ?1 AND id LIKE 'd%'",
        [&id_str],
    )?;
    tx.execute("DELETE FROM supersedes WHERE new_id = ?1", [&id_str])?;
    tx.execute("DELETE FROM proposals WHERE note_id = ?1", [&id_str])?;
    tx.execute("DELETE FROM decisions WHERE proposal = ?1", [&id_str])?;
    // a removed conflict copy leaves its tags orphaned in the global table:
    // the tags are a function of note_tags (the purge's own rule), and a
    // copy that never existed on some copies must not leave its name
    // behind on others
    tx.execute(
        "DELETE FROM tags WHERE NOT EXISTS (
             SELECT 1 FROM note_tags nt
             WHERE nt.tag = tags.name OR nt.tag LIKE tags.name || '/%'
         )",
        [],
    )?;
    Ok(())
}

/// Does the SET hold a purge of this note? The purge dominates: the note
/// does not materialize, its operations arrive redacted, its conflict
/// copies go with it.
pub(crate) fn note_purged_in_set(
    tx: &rusqlite::Transaction<'_>,
    id: &Ulid,
) -> Result<bool, CellError> {
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         WHERE oo.object = ?1 AND o.kind = 'purge'",
        [format!("note:{id}")],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// The note ids a purge of is RELEVANT to for this operation (v0.6.11,
/// K1 — the per-half reading of the v0.6.10 index): the operation's own
/// objects — `note_id`, a supersede's new note, a decide's target, a
/// revert's restored note — plus the objects the `op_objects` index
/// does not carry: a link's endpoints and declared note. The index rows
/// are read for STORED operations and merged over [`ops::objects_of`]:
/// the purge's censor takes the new note's id OUT of a supersede's body
/// (`note: null`), and the index — ids only, alive after any redaction
/// — is what still names it. The `edges` row an unlink names may be
/// gone (the purge took it): its endpoints are then read from the link
/// op of the same edge, whose body the censor never touches (a link is
/// not an operation of the note it cites).
fn purge_relevant_note_ids(
    tx: &rusqlite::Transaction<'_>,
    op: &Op,
) -> Result<Vec<Ulid>, CellError> {
    let mut ids: Vec<Ulid> = Vec::new();
    let push = |id: Ulid, ids: &mut Vec<Ulid>| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };
    if let Some(id) = op.note_id {
        push(id, &mut ids);
    }
    // the stored index: written at apply time from the body the operation
    // CARRIED then, censored bodies included
    {
        let mut stmt = tx.prepare("SELECT object FROM op_objects WHERE op_id = ?1")?;
        let rows: Vec<String> = stmt
            .query_map([op.op_id.to_string()], |r| r.get(0))?
            .flatten()
            .collect();
        for object in rows
            .iter()
            .filter_map(|o| o.strip_prefix("note:"))
            .filter_map(|n| Ulid::from_string(n).ok())
        {
            push(object, &mut ids);
        }
    }
    // the body's own extraction — a fresh (not yet stored) operation has
    // no index rows, and the link/propose objects were never indexed
    let body = op.body.as_ref();
    let id_of = |v: Option<&serde_json::Value>| -> Option<Ulid> {
        v.and_then(serde_json::Value::as_str)
            .and_then(|s| Ulid::from_string(s).ok())
    };
    match op.kind {
        OpKind::Supersede => {
            let new = body
                .and_then(|b| b.get("note"))
                .and_then(|n| n.get("id"))
                .and_then(serde_json::Value::as_str)
                // the censor keeps the id by itself (v0.6.10, J1)
                .or_else(|| {
                    body.and_then(|b| b.get("new_id"))
                        .and_then(serde_json::Value::as_str)
                });
            if let Some(id) = new.and_then(|s| Ulid::from_string(s).ok()) {
                push(id, &mut ids);
            }
        }
        OpKind::Decide => {
            if let Some(id) = id_of(body.and_then(|b| b.get("target"))) {
                push(id, &mut ids);
            }
        }
        OpKind::Link => {
            let edge = body.and_then(|b| b.get("edge"));
            for endpoint in [
                edge.and_then(|e| e.get("src")),
                edge.and_then(|e| e.get("dst")),
            ] {
                if let Some(id) = endpoint
                    .and_then(serde_json::Value::as_str)
                    .and_then(|s| s.strip_prefix("note:"))
                    .and_then(|s| Ulid::from_string(s).ok())
                {
                    push(id, &mut ids);
                }
            }
            if let Some(id) = id_of(edge.and_then(|e| e.get("note_id"))) {
                push(id, &mut ids);
            }
        }
        OpKind::Unlink => {
            let endpoints =
                |edge: Option<&serde_json::Value>| -> Option<(String, String, Option<String>)> {
                    let e = edge?;
                    Some((
                        e.get("src")?.as_str()?.to_string(),
                        e.get("dst")?.as_str()?.to_string(),
                        e.get("note_id")
                            .and_then(serde_json::Value::as_str)
                            .map(String::from),
                    ))
                };
            let row = match body
                .and_then(|b| b.get("edge_id"))
                .and_then(serde_json::Value::as_str)
            {
                Some(edge_id) => tx
                    .query_row(
                        "SELECT src, dst, note_id FROM edges WHERE id = ?1",
                        [edge_id],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )
                    .optional()?
                    // the row is gone (a purge took it): the link op of
                    // the same edge still names its endpoints
                    .or_else(|| {
                        let mut stmt = match tx.prepare(
                            "SELECT p.body FROM ops o
                             JOIN op_objects oo ON oo.op_id = o.op_id
                             JOIN op_payloads p ON p.op_id = o.op_id
                             WHERE oo.object = ?1 AND o.kind = 'link'
                             ORDER BY o.seq DESC LIMIT 1",
                        ) {
                            Ok(stmt) => stmt,
                            Err(_) => return None,
                        };
                        let body: Option<String> = stmt
                            .query_row([format!("edge:{edge_id}")], |r| r.get(0))
                            .optional()
                            .ok()
                            .flatten();
                        body.as_deref()
                            .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
                            .as_ref()
                            .and_then(|b| endpoints(b.get("edge")))
                    }),
                None => None,
            };
            if let Some((src, dst, cited)) = row {
                for endpoint in [src, dst] {
                    if let Some(id) = endpoint
                        .strip_prefix("note:")
                        .and_then(|s| Ulid::from_string(s).ok())
                    {
                        push(id, &mut ids);
                    }
                }
                if let Some(id) = cited.as_deref().and_then(|s| Ulid::from_string(s).ok()) {
                    push(id, &mut ids);
                }
            }
        }
        _ => {}
    }
    Ok(ids)
}

/// v0.6.11 (K1): does a purge in the set dominate THIS half — the part
/// of `op` that concerns `object` (`note:<ulid>` or `edge:<id>`)? The
/// v0.6.10 rule (J1) skipped the WHOLE operation when the set held a
/// purge of any of its objects, and the purge of an OLD note took away
/// the NEW note the supersede had created — content of another note,
/// which a purge must never touch (the reviewer's two probes). The rule
/// today splits along the halves:
/// - the half of the purged object never applies: the object is gone;
/// - a half that only REFERENCES the purged one never applies: the
///   closing of a supersede's OLD note is the NEW note's work, the
///   closing of a decide's TARGET is the accepted proposal's work, an
///   edge row is a reference to everything it names. This is the
///   transitivity of v0.6.10, kept for the references alone — it is
///   what holds the v0.6.7 defect closed (a decide whose proposal was
///   purged must not close its target back: seed
///   `14903861504949011020`);
/// - the OTHER object's own content APPLIES, without everything that
///   names the purged id — the guards in the apply functions already
///   speak it (a dead target closes nothing, no `supersedes` row names
///   a missing note, a link whose endpoint is gone waits or parks).
///
/// The exceptions are the [`OpKind::Purge`] itself — its scrub IS the
/// state it carries — and [`OpKind::RevertDecision`], whose
/// `classify_revert` has already refused every revert that could return
/// purged content (its answer is `RevertOutcome::Purged`). One function
/// for the arrival, the replay and the rematerialization; the local
/// write path refuses what this one would skip, by the same object
/// list.
pub(crate) fn half_dominated_by_purge(
    tx: &rusqlite::Transaction<'_>,
    op: &Op,
    object: &str,
) -> Result<bool, CellError> {
    if matches!(op.kind, OpKind::Purge | OpKind::RevertDecision) {
        return Ok(false);
    }
    let relevant = purge_relevant_note_ids(tx, op)?;
    if let Some(nid) = object.strip_prefix("note:") {
        let Ok(id) = Ulid::from_string(nid) else {
            return Ok(false);
        };
        for x in &relevant {
            if !note_purged_in_set(tx, x)? {
                continue;
            }
            // the half of the purged object, or a half that is pure
            // reference to it: both go with the purge
            if *x == id || half_references(op, &id, x) {
                return Ok(true);
            }
        }
        Ok(false)
    } else if object.starts_with("edge:") {
        // the edge's whole state step IS the reference: a link that names
        // a purged note (an endpoint, the cited note) must not recreate
        // the row the purge's state step deleted, and a dominated unlink
        // does not wait — the edge it names will never materialize again
        for x in &relevant {
            if note_purged_in_set(tx, x)? {
                return Ok(true);
            }
        }
        Ok(false)
    } else {
        Ok(false)
    }
}

/// Is the half of `op` concerning `id` a pure REFERENCE to `x` — work the
/// purged note did ON another note, which the right to be forgotten takes
/// with it and whose undoing is the reopening? The supersede's OLD side
/// closes over the NEW note; a decide's accept closes its TARGET over the
/// PROPOSAL. The censored bodies keep every id this reads (the censor's
/// own rule, v0.6.10 J1), so the answer is the same before and after the
/// purge's scrub of the log.
fn half_references(op: &Op, id: &Ulid, x: &Ulid) -> bool {
    let body = op.body.as_ref();
    let id_of = |v: Option<&serde_json::Value>| -> Option<Ulid> {
        v.and_then(serde_json::Value::as_str)
            .and_then(|s| Ulid::from_string(s).ok())
    };
    match op.kind {
        OpKind::Supersede => {
            let new = id_of(body.and_then(|b| b.get("note")).and_then(|n| n.get("id")))
                .or_else(|| id_of(body.and_then(|b| b.get("new_id"))));
            let old = id_of(body.and_then(|b| b.get("old_id")));
            new.is_some_and(|n| n == *x) && old.is_some_and(|o| o == *id)
        }
        OpKind::Decide => {
            // a reject closes nothing: only the accept's target half is
            // the proposal's work
            let accept = body
                .and_then(|b| b.get("decision"))
                .and_then(serde_json::Value::as_str)
                == Some("accept");
            let proposal = id_of(body.and_then(|b| b.get("proposal")));
            let target = id_of(body.and_then(|b| b.get("target")));
            accept && proposal.is_some_and(|p| p == *x) && target.is_some_and(|t| t == *id)
        }
        _ => false,
    }
}

/// The purge of a note takes its CONFLICT COPIES with it: they are the
/// note's own derivation, and the right to be forgotten reaches everything
/// the note's content produced.
pub(crate) fn purge_conflict_copies(
    tx: &rusqlite::Transaction<'_>,
    original: &Ulid,
) -> Result<(), CellError> {
    // v0.6.4: the purge reaches TRANSITIVELY — a copy that lived its own
    // life (edited, resolved) carries copies of ITS conflicts, and the
    // right to be forgotten reaches everything the text produced (found
    // by the 20 000-case hunt, seed 4946564889653177402: the sub-copy
    // lingered in the incremental state while the rebuild, whose base
    // row the purge had taken, could no longer derive it)
    let mut frontier = vec![format!("note:{original}")];
    let mut seen: std::collections::HashSet<String> = frontier.iter().cloned().collect();
    while let Some(object) = frontier.pop() {
        let Some(id) = object.strip_prefix("note:") else {
            continue;
        };
        let Ok(cid) = Ulid::from_string(id) else {
            continue;
        };
        delete_note_state(tx, &cid)?;
        super::vectors::delete_note_vectors(tx, &cid)?;
        // v0.6.5 (E1): a purged note's visible head goes with it — nothing
        // leans on a note that will never materialize again
        tx.execute("DELETE FROM note_heads WHERE note_id = ?1", [id])?;
        let next: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT copy_id FROM conflict_copies WHERE original_id = ?1")?;
            let rows = stmt.query_map([id.to_string()], |r| r.get::<_, String>(0))?;
            rows.flatten().collect()
        };
        for copy_id in next {
            tx.execute("DELETE FROM conflict_copies WHERE copy_id = ?1", [&copy_id])?;
            tx.execute(
                "DELETE FROM proposals WHERE note_id = ?1 AND state = 'pending'",
                [&copy_id],
            )?;
            clear_pending(tx, &format!("note:{copy_id}"))?;
            let object = format!("note:{copy_id}");
            if seen.insert(object.clone()) {
                frontier.push(object);
            }
        }
    }
    // v0.6.10 (J1, the hunt's seed 13144595116571598162): the copies'
    // note_tags went with their rows, and the tags no note carries
    // anymore must go too — the same prune `purge_state` runs for the
    // purged note itself. Without it the live keeps orphan tags the
    // reconstruction never creates (a copy's rows die here, not in a
    // materialization that prunes)
    tx.execute(
        "DELETE FROM tags WHERE NOT EXISTS (
             SELECT 1 FROM note_tags nt
             WHERE nt.tag = tags.name OR nt.tag LIKE tags.name || '/%'
         )",
        [],
    )?;
    Ok(())
}

/// v0.6.4 (D1): a LOCAL body write pays the same derivation the
/// arrival path pays (`after_body_write`) — and through it, when the
/// note needs it, the full replay. The v0.6.3 premise (a local write is
/// newest, base at the head, losers empty) died with the class of the
/// author: a head-chained write can LOSE to an older human tip, and
/// losing means its copy is born on the writing device itself. A
/// header-only patch derives nothing: the header never creates or
/// closes conflicts (D1.1).
pub(crate) fn reconcile_after_local_write(
    tx: &rusqlite::Transaction<'_>,
    op: &Op,
) -> Result<(), CellError> {
    for object in ops::objects_of(op) {
        let Some(nid) = object.strip_prefix("note:") else {
            continue;
        };
        let Ok(nid) = Ulid::from_string(nid) else {
            continue;
        };
        if is_body_write(op, &nid) {
            super::conflicts::after_body_write(tx, &nid, op)?;
        } else {
            super::conflicts::rederive_note(tx, &nid)?;
        }
        super::conflicts::rederive_original_of(tx, &nid)?;
    }
    Ok(())
}

/// The policy object: `init` and `set_policy` in total order — last write
/// wins, the whole history of the meta keys replays.
fn materialize_policy(tx: &rusqlite::Transaction<'_>) -> Result<(), CellError> {
    let all = ops_concerning(tx, "policy")?;
    let watermark: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'hlc'", [], |r| r.get(0))
        .optional()?;
    tx.execute_batch("DELETE FROM meta WHERE key IN ('cell_id', 'cell_name', 'policy');")?;
    for op in &all {
        ops::apply_scoped(tx, op, Scope::Full)?;
    }
    restore_hlc_watermark(tx, watermark, None)?;
    Ok(())
}

/// The edge object: its link and unlink operations in total order — the
/// edge exists iff its newest operation is the link. An unlink whose link
/// has not arrived yet waits (pending), exactly like a patch before its
/// put.
fn materialize_edge(
    tx: &rusqlite::Transaction<'_>,
    edge_id: &str,
    object: &str,
) -> Result<(), CellError> {
    let all = ops_concerning(tx, object)?;
    let watermark: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'hlc'", [], |r| r.get(0))
        .optional()?;
    tx.execute("DELETE FROM edges WHERE id = ?1", [edge_id])?;
    let mut pending_since: Option<String> = None;
    for op in &all {
        // v0.6.11 (K1): the purge dominates per half here too — the
        // edge's whole state step is a reference: a link whose endpoint
        // (or cited note) is purged does not recreate the edge row the
        // purge's state step deleted, and a dominated unlink does not
        // wait
        if half_dominated_by_purge(tx, op, object)? {
            continue;
        }
        match op.kind {
            OpKind::Link => {
                match ops::apply_scoped(tx, op, Scope::Full) {
                    Ok(()) => {}
                    // a cited note has not arrived yet: the edge waits
                    Err(CellError::NoteNotFound(_)) => {
                        pending_since = Some(op.op_id.to_string());
                    }
                    Err(e) => return Err(e),
                }
            }
            OpKind::Unlink => {
                let n: i64 =
                    tx.query_row("SELECT COUNT(*) FROM edges WHERE id = ?1", [edge_id], |r| {
                        r.get(0)
                    })?;
                if n == 0 {
                    pending_since = Some(op.op_id.to_string());
                } else {
                    ops::apply_scoped(tx, op, Scope::Full)?;
                }
            }
            _ => {}
        }
    }
    if let Some(since) = pending_since {
        mark_pending(tx, object, &since)?;
    } else {
        clear_pending(tx, object)?;
    }
    restore_hlc_watermark(tx, watermark, None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The total order `ORDER BY hlc, op_id` speaks: the HLC's own structure
    /// (physical, counter, node — fixed-width node ids compare the same as
    /// strings) with the op id as the final tiebreak. The SQL and this
    /// tuple must never disagree.
    #[test]
    fn the_sql_order_walks_the_total_order() {
        let mk = |ms: u64, counter: u32, node: &str, id: u64| {
            let op_id = Ulid::from_string(&format!("01{}{id:08}", "0".repeat(16)))
                .unwrap(/* test-only */);
            let hlc = crate::hlc::Hlc {
                physical_ms: ms,
                counter,
                node: node.into(),
            };
            (hlc.to_string(), op_id.to_string())
        };
        let a = mk(1, 0, "aaaaaaaaaaaa", 1);
        let b = mk(1, 0, "aaaaaaaaaaaa", 2);
        let c = mk(1, 1, "000000000000", 1);
        let d = mk(0, 9, "ffffffffffff", 9);
        let mut all = [a.clone(), b.clone(), c, d.clone()];
        all.sort();
        // the string sort the SQL does == the structural order
        assert_eq!(all[0], d, "physical first");
        assert!(
            all[1].0.ends_with("aaaaaaaaaaaa")
                && all[2].0.ends_with("aaaaaaaaaaaa")
                && all[3].0.ends_with("000000000000"),
            "counter before node: {:?}",
            all
        );
        assert!(a < b, "same hlc: the op id breaks the tie");
    }
}
