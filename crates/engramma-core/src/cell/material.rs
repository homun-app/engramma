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
                o.payload_digest, p.salt, p.body, o.prev_hash, o.hash, o.device, o.sig
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
                salt: r.get::<_, Option<Vec<u8>>>(9)?.as_deref(),
                body_json: r.get::<_, Option<String>>(10)?.as_deref(),
                prev_hash: &r.get::<_, String>(11)?,
                hash: &r.get::<_, String>(12)?,
                device: r.get::<_, Option<String>>(13)?.as_deref(),
                sig: r.get::<_, Option<String>>(14)?.as_deref(),
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
    for object in &objects {
        if is_pending(tx, object)?
            || !is_newest_of(tx, object, op)?
            || breaks_the_chain(tx, object, op)?
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
                base: op.base.clone(),
                op_id: op.op_id,
                hlc_ms: op.hlc.physical_ms,
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
                        base: op.base.clone(),
                        op_id: op.op_id,
                        hlc_ms: op.hlc.physical_ms,
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
                    base: op.base.clone(),
                    op_id: op.op_id,
                    hlc_ms: op.hlc.physical_ms,
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
                base: op.base.clone(),
                op_id: op.op_id,
                hlc_ms: op.hlc.physical_ms,
            });
        }
    }
    if let Some(since) = pending_since {
        mark_pending(tx, object, &since)?;
    } else {
        clear_pending(tx, object)?;
    }
    // THE CONFLICT SET, derived once from the FINAL winner: the last body
    // write in total order wins (the replay applied it last), and the
    // losers are exactly the body writes it did not see — those after its
    // `base`. Derived from the whole set at the end of the replay, the
    // set is a function of the SET by construction: every copy replays
    // the same total order, computes the same winner and the same losers
    // — however the operations arrived.
    let losers = final_losers(&events);
    reconcile_conflict_copies(tx, id, &losers)?;
    restore_hlc_watermark(tx, watermark, None)?;
    Ok(())
}

/// The losers of the whole replay: the body writes after the WINNER's
/// `base`, the winner being the LAST body write in total order.
fn final_losers(events: &[ReplayEvent]) -> Vec<LoserRecord> {
    let Some(winner_pos) = events.iter().rposition(|e| e.body.is_some()) else {
        return Vec::new();
    };
    let winner_base = events[winner_pos].base.clone();
    // A base that is NOT among the events means "written before the sync
    // era, or censored by a purge" — sequential by declaration, never a
    // concurrent past: no losers. (Starting from 0 here manufactured
    // phantom conflict copies: a purge's censor REWRITES a supersede's
    // content_hash, so a patch written before the censor points at a hash
    // that no longer exists — the phantom's tell.)
    let start = match &winner_base {
        Some(b) => events
            .iter()
            .position(|e| &e.hash == b)
            .map_or(winner_pos, |p| (p + 1).min(winner_pos)),
        None => 0,
    };
    events[start..winner_pos]
        .iter()
        .filter_map(|e| {
            e.body.as_ref().map(|(state, _)| LoserRecord {
                op_id: e.op_id,
                hlc_ms: e.hlc_ms,
                content_hash: e.hash.clone(),
                state: state.clone(),
            })
        })
        .collect()
}

/// One applied operation of the replay, with what the conflict rules need:
/// its wire identity and the note state a BODY write produced (at its
/// version).
struct ReplayEvent {
    hash: String,
    body: Option<(crate::format::Note, i64)>,
    base: Option<String>,
    op_id: Ulid,
    hlc_ms: u64,
}

/// A body write a later concurrent write did not see: its state becomes a
/// conflict copy.
struct LoserRecord {
    op_id: Ulid,
    hlc_ms: u64,
    content_hash: String,
    state: crate::format::Note,
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
    let Some(nid) = object.strip_prefix("note:") else {
        return Ok(false);
    };
    let Ok(nid) = Ulid::from_string(nid) else {
        return Ok(false);
    };
    if !matches!(
        op.kind,
        OpKind::PutNote | OpKind::PatchNote | OpKind::DeleteNote
    ) || op.body.is_none()
    {
        return Ok(false);
    }
    let Some(base) = op.base.as_deref() else {
        return Ok(false);
    };
    let cut = cut_seq(tx)?;
    let hlc = op.hlc.to_string();
    let op_id = op.op_id.to_string();
    let head: Option<String> = tx
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
    let _ = nid;
    Ok(head.as_deref() != Some(base))
}

/// Does this operation write the BODY of note `id` (a full replace or a
/// patch carrying one)?
fn is_body_write(op: &Op, id: &Ulid) -> bool {
    if op.body.is_none() {
        return false;
    }
    match op.kind {
        OpKind::PatchNote => op.note_id.is_some_and(|n| n == *id),
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
    let cut = cut_seq(tx)?;
    let hlc = op.hlc.to_string();
    let op_id = op.op_id.to_string();
    let head: Option<String> = tx
        .query_row(
            "SELECT o.content_hash FROM ops o
             JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = ?1 AND o.seq > ?2
               AND (o.hlc < ?3 OR (o.hlc = ?3 AND o.op_id < ?4))
             ORDER BY o.hlc DESC, o.op_id DESC LIMIT 1",
            rusqlite::params![format!("note:{id}"), cut, hlc, op_id],
            |r| r.get(0),
        )
        .optional()?;
    if head.as_deref() == Some(base) {
        Ok(TombstoneOutcome::Reopens)
    } else {
        Ok(TombstoneOutcome::Wins)
    }
}

/// The deterministic id of a conflict copy (DESIGN §4.3):
/// `blake3("engramma/conflict/v1" ‖ content_hash del perdente)` in ULID
/// form, the losing operation's HLC as the time part.
fn conflict_copy_id(loser_hash: &str, loser_hlc_ms: u64) -> Ulid {
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
fn conflict_copy_state(
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
    let note = crate::format::Note::from_json(
        &serde_json::from_str::<serde_json::Value>(&json)
            .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?,
    )
    .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?;
    Ok(Some((original, note)))
}

/// Write (or rewrite) the derived ROWS of one conflict copy: the note —
/// active, tagged `conflitto`, the loser's content — its FTS and tags, and
/// the edge to the original. Pure derivation: no operation of its own, and
/// every copy of the cell produces it identically.
fn restore_conflict_copy(
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

/// Record and write one conflict copy derived from `original`'s replay.
fn write_conflict_copy(
    tx: &rusqlite::Transaction<'_>,
    original: &Ulid,
    loser: &LoserRecord,
) -> Result<Ulid, CellError> {
    let copy_id = conflict_copy_id(&loser.content_hash, loser.hlc_ms);
    // the copy's state: the loser's note, active, tagged
    let mut note = loser.state.clone();
    note.id = copy_id;
    note.status = crate::format::Status::Active;
    if !note.tags.iter().any(|t| t == "conflitto") {
        note.tags.push("conflitto".into());
    }
    // the derivation row first: it is the base the copy's own materialization
    // (and a user's later edits on the copy) replay from
    tx.execute(
        "INSERT INTO conflict_copies (copy_id, original_id, loser_op, state_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(copy_id) DO UPDATE SET original_id = ?2, loser_op = ?3, state_json = ?4",
        rusqlite::params![
            copy_id.to_string(),
            original.to_string(),
            loser.op_id.to_string(),
            note.to_json().to_string()
        ],
    )?;
    restore_conflict_copy(tx, &copy_id, original, &note)?;
    Ok(copy_id)
}

/// Delete every state row of a note (no log rows touched), the
/// rematerialization's delete-first for derived copies.
fn delete_note_state(tx: &rusqlite::Transaction<'_>, id: &Ulid) -> Result<(), CellError> {
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

/// Does this note have operations of its own (a conflict copy the user
/// edited, or a note that was never derived)?
fn note_has_ops(tx: &rusqlite::Transaction<'_>, id: &Ulid) -> Result<bool, CellError> {
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM op_objects WHERE object = ?1",
        [format!("note:{id}")],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// After the replay of the original: the copies the conflict rules derive
/// are written, the copies of conflicts that no longer exist go (unless the
/// user made them theirs with edits), and a copy whose derivation moved
/// rematerializes.
fn reconcile_conflict_copies(
    tx: &rusqlite::Transaction<'_>,
    original: &Ulid,
    losers: &[LoserRecord],
) -> Result<(), CellError> {
    let desired: Vec<Ulid> = losers
        .iter()
        .map(|l| conflict_copy_id(&l.content_hash, l.hlc_ms))
        .collect();
    let existing: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT copy_id FROM conflict_copies WHERE original_id = ?1 ORDER BY copy_id",
        )?;
        let rows = stmt.query_map([original.to_string()], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for copy_id in existing {
        let ulid = Ulid::from_string(&copy_id)
            .map_err(|_| CellError::Db(format!("bad conflict copy id {copy_id}")))?;
        if desired.contains(&ulid) {
            continue;
        }
        if note_has_ops(tx, &ulid)? {
            // the user edited the copy: the note lives on as its own
            continue;
        }
        delete_note_state(tx, &ulid)?;
        tx.execute("DELETE FROM conflict_copies WHERE copy_id = ?1", [copy_id])?;
    }
    let mut written: Vec<Ulid> = Vec::new();
    for loser in losers {
        let copy_id = write_conflict_copy(tx, original, loser)?;
        if !written.contains(&copy_id) && note_has_ops(tx, &copy_id)? && copy_id != *original {
            // the copy carries the user's edits and its base just moved:
            // it rematerializes over the new derivation
            materialize(tx, &format!("note:{copy_id}"))?;
        }
        written.push(copy_id);
        clear_pending(tx, &format!("note:{copy_id}"))?;
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
