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
    // an unlink whose link has not arrived waits like a patch before its
    // put: the edge object goes pending, and the link's arrival replays
    let mut unlink_waits = false;
    if op.kind == OpKind::Unlink
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
                if let Some(nid) = object.strip_prefix("note:")
                    && let Ok(nid) = Ulid::from_string(nid)
                    && is_body_write(op, &nid)
                {
                    match tombstone_rule(tx, op, &nid)? {
                        TombstoneOutcome::Wins => continue,
                        TombstoneOutcome::Reopens => ops::reopen_tombstone(tx, &nid)?,
                        TombstoneOutcome::NoTombstone => {}
                    }
                }
                match apply_half(tx, op, object) {
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
                    apply_half(tx, op, object)?;
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

/// The half of one operation that concerns one object.
fn apply_half(tx: &rusqlite::Transaction<'_>, op: &Op, object: &str) -> Result<(), CellError> {
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
    // operations produced (their payloads are gone; they cannot replay), so
    // the anchor is the FIRST above-cut arrival whose `base` points BELOW
    // the cut: its author had applied everything at or below the cut — the
    // frontier's own promise — so its pre-state snapshot IS the state at the
    // cut. No such anchor (the note was born above the cut): replay from
    // zero. Pending arrivals above the cut never anchor: their bases point
    // above it, whatever order they landed in.
    let cut_state: Option<(crate::format::Note, i64)> = {
        let mut anchor: Option<(String, i64)> = None;
        let mut stmt = tx.prepare(
            "SELECT o.op_id, o.base FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.seq > ?2
             ORDER BY o.seq ASC",
        )?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map(rusqlite::params![object, cut], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (op_id, base) in rows {
            let Some(base) = base else { continue };
            let base_seq: Option<i64> = tx
                .query_row("SELECT seq FROM ops WHERE content_hash = ?1", [base], |r| {
                    r.get(0)
                })
                .optional()?;
            if base_seq.is_some_and(|s| s <= cut) {
                let snap: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT snapshot_json, version FROM note_versions WHERE op_id = ?1",
                        [&op_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                anchor = snap;
                break;
            }
        }
        match anchor {
            None => None,
            Some((json, version)) => {
                let note = crate::format::Note::from_json(
                    &serde_json::from_str::<serde_json::Value>(&json)
                        .map_err(|e| CellError::Db(format!("corrupt version snapshot: {e}")))?,
                )
                .map_err(|e| CellError::Db(format!("corrupt version snapshot: {e}")))?;
                Some((note, version))
            }
        }
    };
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
    let purged = note_purged_in_set(tx, id)?;
    for op in &all {
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
                    // the tombstone wins: the write is stored, without effect
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
                for e in events.iter_mut().skip(pos + 1) {
                    e.body = None;
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
    let states: std::collections::HashMap<String, crate::format::Note> = events
        .iter()
        .filter_map(|e| {
            e.body
                .as_ref()
                .map(|(state, _)| (e.hash.clone(), state.clone()))
        })
        .collect();
    super::conflicts::after_replay(tx, id, &applied, &states)?;
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
    // the head before this write, above the cut or below it as a skeleton
    // (`head_hash_before`): a writer whose base is the note's newest
    // operation saw the lapide and reopens the note
    let head = head_hash_before(
        tx,
        &format!("note:{id}"),
        &op.hlc.to_string(),
        &op.op_id.to_string(),
    )?;
    if head.as_deref() == Some(base) {
        Ok(TombstoneOutcome::Reopens)
    } else {
        Ok(TombstoneOutcome::Wins)
    }
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
    // the edge to the original: derived (d%), so the copy's own
    // rematerialization cleans and rebuilds it
    let edge_id = {
        let mut h = blake3::Hasher::new();
        h.update(b"engramma/conflict-edge/v1");
        h.update(copy_id.to_string().as_bytes());
        format!("d{}", &h.finalize().to_hex()[..31])
    };
    tx.execute(
        "INSERT OR REPLACE INTO edges (id, src, rel, dst, note_id, valid_from, valid_to,
                                       confidence, status)
         VALUES (?1, ?2, 'conflitto', ?3, ?4, NULL, NULL, 'stated', 'active')",
        rusqlite::params![
            edge_id,
            format!("note:{copy_id}"),
            format!("note:{original}"),
            copy_id.to_string()
        ],
    )?;
    Ok(())
}

/// Delete every state row of a note (no log rows touched), the
/// rematerialization's delete-first for derived copies.
pub(crate) fn delete_note_state(tx: &rusqlite::Transaction<'_>, id: &Ulid) -> Result<(), CellError> {
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

/// The purge of a note takes its CONFLICT COPIES with it: they are the
/// note's own derivation, and the right to be forgotten reaches everything
/// the note's content produced.
pub(crate) fn purge_conflict_copies(
    tx: &rusqlite::Transaction<'_>,
    original: &Ulid,
) -> Result<(), CellError> {
    let copies: Vec<String> = {
        let mut stmt = tx.prepare("SELECT copy_id FROM conflict_copies WHERE original_id = ?1")?;
        let rows = stmt.query_map([original.to_string()], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for copy_id in copies {
        let Ok(cid) = Ulid::from_string(&copy_id) else {
            continue;
        };
        delete_note_state(tx, &cid)?;
        super::vectors::delete_note_vectors(tx, &cid)?;
        tx.execute("DELETE FROM conflict_copies WHERE copy_id = ?1", [copy_id])?;
        clear_pending(tx, &format!("note:{cid}"))?;
    }
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
