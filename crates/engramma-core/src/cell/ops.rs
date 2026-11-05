// SPDX-License-Identifier: Apache-2.0

//! The change log: operation model, canonical JSON, deterministic `apply`.
//!
//! `apply(state, op)` reads no clock, no randomness, no I/O: every ID, HLC and
//! timestamp is generated when the operation is **created** and travels inside
//! the payload (DESIGN §4.3). The hash chain is
//! `hash = blake3(prev_hash || payload_json_canonical)` with ASCII-hex
//! encoding and 64 `0`s as the genesis `prev_hash`.

use rusqlite::{Connection, Transaction};
use serde_json::Value;
use ulid::Ulid;

use crate::error::CellError;
use crate::format::body;
use crate::format::{Note, Status, Validity};
use crate::text;

/// Operation kinds (one row in `ops` each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// Cell creation: writes `cell_id` and `name` into `meta`.
    Init,
    /// Create or fully replace a note.
    PutNote,
    /// Partial update of a note.
    PatchNote,
    /// Change status (never to `active` for `external`, never superseded/deleted).
    SetStatus,
    /// Replace a note with a newer one (closes the old validity).
    Supersede,
    /// Logical delete (tombstone).
    DeleteNote,
    /// Restore a previous version.
    Restore,
    /// Add a manual edge.
    Link,
    /// Remove an edge by id.
    Unlink,
    /// Promote a quarantined note to `active`.
    Confirm,
}

impl OpKind {
    /// Wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            OpKind::Init => "init",
            OpKind::PutNote => "put_note",
            OpKind::PatchNote => "patch_note",
            OpKind::SetStatus => "set_status",
            OpKind::Supersede => "supersede",
            OpKind::DeleteNote => "delete_note",
            OpKind::Restore => "restore",
            OpKind::Link => "link",
            OpKind::Unlink => "unlink",
            OpKind::Confirm => "confirm",
        }
    }

    /// Parse the wire name.
    ///
    /// # Errors
    /// [`CellError::Invalid`] for unknown kinds.
    pub fn parse(s: &str) -> Result<Self, CellError> {
        Ok(match s {
            "init" => OpKind::Init,
            "put_note" => OpKind::PutNote,
            "patch_note" => OpKind::PatchNote,
            "set_status" => OpKind::SetStatus,
            "supersede" => OpKind::Supersede,
            "delete_note" => OpKind::DeleteNote,
            "restore" => OpKind::Restore,
            "link" => OpKind::Link,
            "unlink" => OpKind::Unlink,
            "confirm" => OpKind::Confirm,
            other => return Err(CellError::Invalid(format!("unknown op kind `{other}`"))),
        })
    }
}

/// One row of the change log.
#[derive(Debug, Clone)]
pub struct Op {
    /// Sequence number (insert order).
    pub seq: i64,
    /// Operation ULID.
    pub op_id: Ulid,
    /// Hybrid logical clock value.
    pub hlc: crate::hlc::Hlc,
    /// Who wrote it.
    pub actor: String,
    /// Kind.
    pub kind: OpKind,
    /// Note involved, when applicable.
    pub note_id: Option<Ulid>,
    /// Canonical JSON payload (keys sorted at every level, no whitespace).
    pub payload: Value,
    /// Previous op hash (genesis: 64 `0`s).
    pub prev_hash: String,
    /// This op hash.
    pub hash: String,
}

/// Genesis `prev_hash`.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Canonical payload string (serde_json maps are BTreeMaps: keys sorted at
/// every level, no spaces).
pub fn canonical_json(v: &Value) -> String {
    v.to_string()
}

/// Compute an op hash.
pub fn op_hash(prev_hash: &str, payload_json: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(payload_json.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Build the payload skeleton common to every op: the envelope travels inside
/// the hashed payload, so the hash binds `op_id`, `hlc`, `actor`, `kind` and
/// `note_id` — the columns of `ops` are an index over it.
fn envelope(
    op_id: &Ulid,
    hlc: &crate::hlc::Hlc,
    actor: &str,
    kind: OpKind,
    note_id: Option<Ulid>,
) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("op_id".into(), op_id.to_string().into());
    m.insert("hlc".into(), hlc.to_string().into());
    m.insert("actor".into(), actor.into());
    m.insert("kind".into(), kind.as_str().into());
    m.insert(
        "note_id".into(),
        note_id
            .map(|u| Value::String(u.to_string()))
            .unwrap_or(Value::Null),
    );
    Value::Object(m)
}

/// The columns of an op row must agree with the payload envelope on every
/// field. The chain hash covers the payload only: without this check a
/// tampered column (say, `kind`) would leave the chain — and possibly the
/// replay — looking valid.
fn check_envelope(op: &Op) -> Result<(), CellError> {
    let mismatch = |field: &'static str| {
        CellError::Invalid(format!(
            "op {}: the `{field}` column disagrees with the payload envelope",
            op.op_id
        ))
    };
    let get = |k: &str| op.payload.get(k);
    if get("op_id").and_then(Value::as_str) != Some(op.op_id.to_string().as_str()) {
        return Err(mismatch("op_id"));
    }
    if get("hlc").and_then(Value::as_str) != Some(op.hlc.to_string().as_str()) {
        return Err(mismatch("hlc"));
    }
    if get("actor").and_then(Value::as_str) != Some(op.actor.as_str()) {
        return Err(mismatch("actor"));
    }
    if get("kind").and_then(Value::as_str) != Some(op.kind.as_str()) {
        return Err(mismatch("kind"));
    }
    let column = op.note_id.map(|u| u.to_string());
    let envelope_ok = match get("note_id") {
        Some(Value::Null) => column.is_none(),
        Some(Value::String(s)) => column.as_deref() == Some(s.as_str()),
        _ => false,
    };
    if !envelope_ok {
        return Err(mismatch("note_id"));
    }
    Ok(())
}

/// Assemble a complete [`Op`] (without seq/hash) from a payload.
///
/// # Errors
/// [`CellError::Invalid`] when the payload does not carry the envelope fields.
pub fn make_op(
    op_id: Ulid,
    hlc: crate::hlc::Hlc,
    actor: &str,
    kind: OpKind,
    note_id: Option<Ulid>,
    mut payload: Value,
) -> Result<Op, CellError> {
    let Value::Object(fields) = &mut payload else {
        return Err(CellError::Invalid("op payload must be an object".into()));
    };
    let mut env = match envelope(&op_id, &hlc, actor, kind, note_id) {
        Value::Object(m) => m,
        _ => unreachable!("envelope is always an object"),
    };
    for (k, v) in fields.iter() {
        if matches!(k.as_str(), "op_id" | "hlc" | "actor" | "kind" | "note_id") {
            return Err(CellError::Invalid(format!(
                "op payload must not override the envelope field `{k}`"
            )));
        }
        env.insert(k.clone(), v.clone());
    }
    let op = Op {
        seq: 0,
        op_id,
        hlc,
        actor: actor.to_string(),
        kind,
        note_id,
        payload: Value::Object(env),
        prev_hash: String::new(),
        hash: String::new(),
    };
    check_envelope(&op)?;
    Ok(op)
}

/// The raw fields of a stored `ops` row (one per column).
#[allow(missing_docs)]
pub struct OpRow<'a> {
    pub seq: i64,
    pub op_id: &'a str,
    pub hlc: &'a str,
    pub actor: &'a str,
    pub kind: &'a str,
    pub note_id: Option<&'a str>,
    pub payload_json: &'a str,
    pub prev_hash: &'a str,
    pub hash: &'a str,
}

/// Rebuild an [`Op`] from a stored `ops` row.
///
/// # Errors
/// [`CellError::Invalid`] on malformed rows — including columns that disagree
/// with the payload envelope.
pub fn op_from_row(r: OpRow<'_>) -> Result<Op, CellError> {
    let op = Op {
        seq: r.seq,
        op_id: Ulid::from_string(r.op_id)
            .map_err(|_| CellError::Invalid(format!("bad op_id `{}`", r.op_id)))?,
        hlc: crate::hlc::Hlc::parse(r.hlc)?,
        actor: r.actor.to_string(),
        kind: OpKind::parse(r.kind)?,
        note_id: r
            .note_id
            .map(Ulid::from_string)
            .transpose()
            .map_err(|_| CellError::Invalid(format!("bad note_id in op {}", r.op_id)))?,
        payload: serde_json::from_str(r.payload_json)
            .map_err(|e| CellError::Invalid(format!("bad payload in op {}: {e}", r.op_id)))?,
        prev_hash: r.prev_hash.to_string(),
        hash: r.hash.to_string(),
    };
    check_envelope(&op)?;
    Ok(op)
}

// ---------------------------------------------------------------------------
// apply: the deterministic state transition
// ---------------------------------------------------------------------------

/// Apply one operation inside an open transaction. Deterministic: the payload
/// carries everything; nothing is read from the environment. The row's
/// columns must agree with the payload envelope — `apply` follows the
/// envelope, the columns are only an index.
///
/// # Errors
/// [`CellError`] on invariant violations (missing note, illegal transition,
/// malformed payload) or SQL failures.
pub fn apply(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    check_envelope(op)?;
    // The row in `ops` and the state change live in the same transaction, and
    // replay inserts them identically (explicit seq keeps the numbering).
    tx.execute(
        "INSERT INTO ops (seq, op_id, hlc, actor, kind, note_id, payload_json, prev_hash, hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            op.seq,
            op.op_id.to_string(),
            op.hlc.to_string(),
            op.actor,
            op.kind.as_str(),
            op.note_id.map(|u| u.to_string()),
            canonical_json(&op.payload),
            op.prev_hash,
            op.hash,
        ],
    )?;
    match op.kind {
        OpKind::Init => apply_init(tx, op)?,
        OpKind::PutNote => apply_put(tx, op, &note_from_payload(op, "note")?)?,
        OpKind::PatchNote => apply_patch(tx, op)?,
        OpKind::SetStatus => apply_set_status(tx, op)?,
        OpKind::Supersede => apply_supersede(tx, op)?,
        OpKind::DeleteNote => apply_delete(tx, op)?,
        OpKind::Restore => apply_restore(tx, op)?,
        OpKind::Link => apply_link(tx, op)?,
        OpKind::Unlink => apply_unlink(tx, op)?,
        OpKind::Confirm => apply_confirm(tx, op)?,
    }
    // The cell HLC always advances to the op's HLC (ops apply in order).
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('hlc', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [op.hlc.to_string()],
    )?;
    Ok(())
}

fn payload_str(op: &Op, key: &str) -> Result<String, CellError> {
    op.payload
        .get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| CellError::Invalid(format!("op {} misses `{key}`", op.op_id)))
}

fn note_from_payload(op: &Op, key: &str) -> Result<Note, CellError> {
    let n = Note::from_json(op.payload.get(key).unwrap_or(&Value::Null))
        .map_err(|e| CellError::Invalid(format!("op {} carries a bad note: {e}", op.op_id)))?;
    Ok(n)
}

fn apply_init(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let cell_id = payload_str(op, "cell_id")?;
    let name = payload_str(op, "name")?;
    for (k, v) in [("cell_id", cell_id), ("cell_name", name)] {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            rusqlite::params![k, v],
        )?;
    }
    Ok(())
}

/// Full column set of `notes` (everything but computed FTS columns).
const NOTE_COLS: &str = "id, type, title, body, frontmatter_json, scope, source, confidence, status, \
     valid_from, valid_to, created_at, updated_at, version, body_norm";

/// Header-only canonical JSON for `frontmatter_json` (the body lives in the
/// `notes.body` column; the placeholder keeps the JSON model complete).
fn header_json(note: &Note) -> String {
    let full = note.to_json();
    let mut m = match full {
        Value::Object(m) => m,
        _ => unreachable!("Note::to_json returns an object"),
    };
    m.insert("body".into(), Value::String(String::new()));
    Value::Object(m).to_string()
}

fn valid_to_db(v: &Option<Validity>) -> Option<String> {
    v.as_ref().map(|x| x.to_db_string())
}

/// A stored note row, with what the derived structures need.
struct NoteRow {
    rowid: i64,
    note: Note,
    version: i64,
    /// `body_norm` as last written (FTS delete values must match the insert).
    body_norm: String,
    /// Sorted space-joined tags at read time (same function the FTS insert
    /// uses, so external-content deletes stay exact).
    fts_tags: String,
}

fn read_note(tx: &Transaction<'_>, id: &Ulid) -> Result<Option<NoteRow>, CellError> {
    let sql = format!("SELECT rowid, {NOTE_COLS} FROM notes WHERE id = ?1");
    let mut stmt = tx.prepare(&sql)?;
    let mut rows = stmt.query([id.to_string()])?;
    let Some(row) = rows.next().map_err(CellError::from)? else {
        return Ok(None);
    };
    let frontmatter_json: String = row.get(5)?;
    let mut note = Note::from_json(
        &serde_json::from_str::<Value>(&frontmatter_json)
            .map_err(|e| CellError::Db(format!("corrupt frontmatter_json: {e}")))?,
    )
    .map_err(|e| CellError::Db(format!("corrupt frontmatter_json: {e}")))?;
    // The header JSON carries a body placeholder: use the column.
    note.body = row.get(4)?;
    let rowid: i64 = row.get(0)?;
    let version: i64 = row.get(14)?;
    let body_norm: String = row.get(15).unwrap_or_default();
    let mut tags: Vec<String> = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT tag FROM note_tags WHERE note_id = ?1 ORDER BY tag")?;
        let rows = stmt.query_map([id.to_string()], |r| r.get::<_, String>(0))?;
        for t in rows {
            tags.push(t?);
        }
    }
    Ok(Some(NoteRow {
        rowid,
        note,
        version,
        body_norm,
        fts_tags: tags.join(" "),
    }))
}

/// Deterministic FTS `tags` column value: tags sorted, space-joined.
fn fts_tags_value(tags: &[String]) -> String {
    let mut sorted = tags.to_vec();
    sorted.sort();
    sorted.join(" ")
}

/// Remove a note from the FTS index (values must match the insert exactly).
fn fts_delete(
    tx: &Transaction<'_>,
    rowid: i64,
    title: &str,
    body_norm: &str,
    tags: &str,
) -> Result<(), CellError> {
    tx.execute(
        "INSERT INTO notes_fts(notes_fts, rowid, title, body_norm, tags)
         VALUES ('delete', ?1, ?2, ?3, ?4)",
        rusqlite::params![rowid, title, body_norm, tags],
    )?;
    Ok(())
}

fn fts_insert(
    tx: &Transaction<'_>,
    rowid: i64,
    title: &str,
    body_norm: &str,
    tags: &str,
) -> Result<(), CellError> {
    tx.execute(
        "INSERT INTO notes_fts(rowid, title, body_norm, tags) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![rowid, title, body_norm, tags],
    )?;
    Ok(())
}

/// Save the current state of a note into `note_versions`.
fn snapshot(tx: &Transaction<'_>, id: &Ulid, version: i64, op_id: &Ulid) -> Result<(), CellError> {
    let Some(row) = read_note(tx, id)? else {
        return Ok(());
    };
    tx.execute(
        "INSERT OR REPLACE INTO note_versions (note_id, version, op_id, snapshot_json)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            id.to_string(),
            version,
            op_id.to_string(),
            row.note.to_json().to_string()
        ],
    )?;
    Ok(())
}

/// Write a note row (insert or full update), bumping the version; returns the
/// rowid and the new version. `allow_external_active` is set only by
/// `confirm`, the single operation that can bring an `external` note to
/// `active`.
fn write_note(
    tx: &Transaction<'_>,
    note: &Note,
    existed: bool,
    allow_external_active: bool,
) -> Result<(i64, i64), CellError> {
    if note.confidence == crate::format::Confidence::External
        && !matches!(note.status, Status::Quarantined | Status::Deleted)
        && !(allow_external_active && note.status == Status::Active)
    {
        return Err(CellError::Invalid(
            "an `external` note can only be `quarantined` (only `confirm` promotes it)".into(),
        ));
    }
    let lang = note
        .lang
        .or_else(|| text::detect_lang(&format!("{}\n{}", note.display_title(), note.body)));
    let body_norm = text::normalize_for_index(note.title.as_deref(), &note.body, lang);
    let id = note.id.to_string();
    let header = header_json(note);
    if existed {
        let (rowid, version): (i64, i64) = tx.query_row(
            "SELECT rowid, version FROM notes WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.execute(
            "UPDATE notes SET type=?2, title=?3, body=?4, frontmatter_json=?5, scope=?6,
                    source=?7, confidence=?8, status=?9, valid_from=?10, valid_to=?11,
                    created_at=?12, updated_at=?13, version=?14, body_norm=?15
             WHERE id = ?1",
            rusqlite::params![
                id,
                note.note_type.as_str(),
                note.title,
                note.body,
                header,
                note.scope,
                note.source,
                note.confidence.as_str(),
                note.status.as_str(),
                valid_to_db(&note.valid_from),
                valid_to_db(&note.valid_to),
                note.created_at.to_db_string(),
                note.updated_at.to_db_string(),
                version + 1,
                body_norm,
            ],
        )?;
        Ok((rowid, version + 1))
    } else {
        tx.execute(
            "INSERT INTO notes (id, type, title, body, frontmatter_json, scope, source,
                                confidence, status, valid_from, valid_to, created_at,
                                updated_at, version, body_norm)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1, ?14)",
            rusqlite::params![
                id,
                note.note_type.as_str(),
                note.title,
                note.body,
                header,
                note.scope,
                note.source,
                note.confidence.as_str(),
                note.status.as_str(),
                valid_to_db(&note.valid_from),
                valid_to_db(&note.valid_to),
                note.created_at.to_db_string(),
                note.updated_at.to_db_string(),
                body_norm,
            ],
        )?;
        let rowid: i64 =
            tx.query_row("SELECT rowid FROM notes WHERE id = ?1", [&id], |r| r.get(0))?;
        Ok((rowid, 1))
    }
}

/// Recompute every derived structure of a note: FTS row, tags, entities,
/// edges. `old` carries the pre-write FTS values (external-content deletes
/// must repeat the exact inserted row); derived (`d…`) edges are rebuilt,
/// manual (`link`) edges stay.
fn reindex_note(tx: &Transaction<'_>, id: &Ulid, old: Option<&NoteRow>) -> Result<(), CellError> {
    let Some(row) = read_note(tx, id)? else {
        return Err(CellError::NoteNotFound(id.to_string()));
    };
    let analysis = body::analyze(&row.note.body)
        .map_err(|e| CellError::Invalid(format!("note {id} body: {e}")))?;

    // --- FTS: drop the old row first (values as inserted), then rebuild.
    if let Some(old) = old {
        // The index may not hold the row (e.g. it was deleted earlier); the
        // failed delete is harmless then, so ignore the error.
        let _ = fts_delete(
            tx,
            old.rowid,
            old.note.title.as_deref().unwrap_or(""),
            &old.body_norm,
            &old.fts_tags,
        );
    }
    // --- tags tables
    tx.execute("DELETE FROM note_tags WHERE note_id = ?1", [id.to_string()])?;
    let mut all_tags = row.note.tags.clone();
    for t in &analysis.tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }
    for tag in &all_tags {
        tx.execute(
            "INSERT INTO note_tags (note_id, tag) VALUES (?1, ?2)",
            rusqlite::params![id.to_string(), tag],
        )?;
        // Ancestor chain: casa/milano/duomo -> casa, casa/milano.
        let mut prefix = String::new();
        let segments: Vec<&str> = tag.split('/').collect();
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let parent = if i == 0 {
                None
            } else {
                Some(segments[..i].join("/"))
            };
            tx.execute(
                "INSERT INTO tags (name, parent) VALUES (?1, ?2)
                 ON CONFLICT(name) DO NOTHING",
                rusqlite::params![prefix, parent],
            )?;
        }
    }
    // --- FTS insert with the new (deterministic) tags value.
    if row.note.status != Status::Deleted {
        fts_insert(
            tx,
            row.rowid,
            row.note.title.as_deref().unwrap_or(""),
            &row.body_norm,
            &fts_tags_value(&all_tags),
        )?;
    }

    // --- entities and edges
    tx.execute(
        "DELETE FROM edges WHERE note_id = ?1 AND id LIKE 'd%'",
        [id.to_string()],
    )?;
    let mut names: Vec<String> = analysis.links.clone();
    for r in &analysis.relations {
        if !names.contains(&r.dst) {
            names.push(r.dst.clone());
        }
    }
    if let Some(subject) = row.note.subject_entity()
        && !names.contains(&subject)
    {
        names.push(subject.clone());
    }
    for name in &names {
        ensure_entity(tx, name)?;
    }
    if let Some(subject) = row.note.subject_entity() {
        tx.execute(
            "UPDATE entities SET note_id = ?1 WHERE name = ?2 COLLATE NOCASE",
            rusqlite::params![id.to_string(), subject],
        )?;
    }
    let vf = valid_to_db(&row.note.valid_from);
    let vt = valid_to_db(&row.note.valid_to);
    // Mention edges: note -> entity.
    for name in &analysis.links {
        tx.execute(
            "INSERT INTO edges (id, src, rel, dst, note_id, valid_from, valid_to, confidence, status)
             VALUES (?1, ?2, 'mentions', ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                derived_edge_id(id, &format!("mentions:{name}")),
                format!("note:{id}"),
                name,
                id.to_string(),
                vf,
                vt,
                row.note.confidence.as_str(),
                row.note.status.as_str(),
            ],
        )?;
    }
    // Relation edges: subject entity (or the note) -> entity.
    for r in &analysis.relations {
        let src = row
            .note
            .subject_entity()
            .unwrap_or_else(|| format!("note:{id}"));
        let rvf = r
            .from
            .as_ref()
            .map(|v| v.to_db_string())
            .or_else(|| vf.clone());
        let rvt =
            r.to.as_ref()
                .map(|v| v.to_db_string())
                .or_else(|| vt.clone());
        tx.execute(
            "INSERT INTO edges (id, src, rel, dst, note_id, valid_from, valid_to, confidence, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                derived_edge_id(id, &format!("{}:{}", r.rel, r.dst)),
                src,
                r.rel,
                r.dst,
                id.to_string(),
                rvf,
                rvt,
                row.note.confidence.as_str(),
                row.note.status.as_str(),
            ],
        )?;
    }
    Ok(())
}

/// Deterministic id for a derived edge.
fn derived_edge_id(note_id: &Ulid, salt: &str) -> String {
    let h = blake3::hash(format!("{}:{salt}", note_id).as_bytes());
    format!("d{}", &h.to_hex()[..24])
}

/// Deterministic id for an entity.
pub fn entity_id(name: &str) -> String {
    let h = blake3::hash(name.to_lowercase().as_bytes());
    format!("e{}", &h.to_hex()[..24])
}

fn ensure_entity(tx: &Transaction<'_>, name: &str) -> Result<(), CellError> {
    tx.execute(
        "INSERT INTO entities (id, name) VALUES (?1, ?2)
         ON CONFLICT(name) DO NOTHING",
        rusqlite::params![entity_id(name), name],
    )?;
    Ok(())
}

fn apply_put(tx: &Transaction<'_>, op: &Op, note: &Note) -> Result<(), CellError> {
    if note.status == Status::Deleted {
        return Err(CellError::Invalid(
            "use delete_note for tombstones, not put".into(),
        ));
    }
    let old = read_note(tx, &note.id)?;
    if let Some(row) = &old {
        snapshot(tx, &note.id, row.version, &op.op_id)?;
    }
    write_note(tx, note, old.is_some(), false)?;
    reindex_note(tx, &note.id, old.as_ref())?;
    for old_id in &note.supersedes {
        tx.execute(
            "INSERT OR IGNORE INTO supersedes (new_id, old_id) VALUES (?1, ?2)",
            rusqlite::params![note.id.to_string(), old_id.to_string()],
        )?;
    }
    Ok(())
}

fn apply_patch(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = Ulid::from_string(&payload_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let Some(old) = read_note(tx, &id)? else {
        return Err(CellError::NoteNotFound(id.to_string()));
    };
    snapshot(tx, &id, old.version, &op.op_id)?;
    let set = op
        .payload
        .get("set")
        .and_then(Value::as_object)
        .ok_or_else(|| CellError::Invalid(format!("op {} misses `set`", op.op_id)))?;
    let get_s = |k: &str| set.get(k).and_then(Value::as_str);
    let mut note = old.note.clone();
    if let Some(v) = set.get("title") {
        note.title = v.as_str().map(String::from);
    }
    if let Some(v) = get_s("body") {
        note.body = v.to_string();
    }
    if let Some(v) = set.get("tags") {
        note.tags = v
            .as_array()
            .ok_or_else(|| CellError::Invalid("`tags` must be a list".into()))?
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .ok_or_else(|| CellError::Invalid("`tags` must contain strings".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
    }
    if let Some(v) = set.get("scope") {
        note.scope = v.as_str().map(String::from);
    }
    if let Some(v) = get_s("lang") {
        note.lang = Some(
            crate::format::Lang::parse(v)
                .map_err(|e| CellError::Invalid(format!("bad `lang`: {e}")))?,
        );
    }
    if let Some(v) = get_s("source") {
        note.source = v.to_string();
    }
    if let Some(v) = get_s("confidence") {
        note.confidence = crate::format::Confidence::parse(v)
            .map_err(|e| CellError::Invalid(format!("bad `confidence`: {e}")))?;
        if note.confidence == crate::format::Confidence::External {
            note.status = Status::Quarantined;
        }
    }
    if let Some(v) = set.get("valid_from") {
        note.valid_from = v
            .as_str()
            .map(Validity::parse)
            .transpose()
            .map_err(|e| CellError::Invalid(format!("bad `valid_from`: {e}")))?;
    }
    if let Some(v) = set.get("valid_to") {
        note.valid_to = v
            .as_str()
            .map(Validity::parse)
            .transpose()
            .map_err(|e| CellError::Invalid(format!("bad `valid_to`: {e}")))?;
    }
    if let Some(v) = set.get("x") {
        note.extensions =
            v.as_array()
                .ok_or_else(|| CellError::Invalid("`x` must be an array of pairs".into()))?
                .iter()
                .map(|pair| {
                    let arr = pair.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
                        CellError::Invalid("`x` must be [key, value] pairs".into())
                    })?;
                    let k = arr[0]
                        .as_str()
                        .ok_or_else(|| CellError::Invalid("`x` keys must be strings".into()))?;
                    let val = crate::format::yaml::YamlValue::from_json(&arr[1])
                        .map_err(|e| CellError::Invalid(format!("bad `x` value: {e}")))?;
                    Ok((k.to_string(), val))
                })
                .collect::<Result<Vec<_>, CellError>>()?;
    }
    note.updated_at = crate::format::At::parse(&payload_str(op, "updated_at")?)
        .map_err(|e| CellError::Invalid(format!("bad `updated_at`: {e}")))?;
    write_note(tx, &note, true, false)?;
    reindex_note(tx, &id, Some(&old))?;
    Ok(())
}

fn apply_set_status(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = Ulid::from_string(&payload_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let status = Status::parse(&payload_str(op, "status")?)
        .map_err(|e| CellError::Invalid(format!("bad `status`: {e}")))?;
    let updated_at = crate::format::At::parse(&payload_str(op, "updated_at")?)
        .map_err(|e| CellError::Invalid(format!("bad `updated_at`: {e}")))?;
    let Some(old) = read_note(tx, &id)? else {
        return Err(CellError::NoteNotFound(id.to_string()));
    };
    match status {
        Status::Active if old.note.confidence == crate::format::Confidence::External => {
            return Err(CellError::Invalid(
                "an `external` note reaches `active` only through `confirm`".into(),
            ));
        }
        Status::Superseded => {
            return Err(CellError::Invalid(
                "use `supersede` to mark a note superseded".into(),
            ));
        }
        Status::Deleted => {
            return Err(CellError::Invalid(
                "use `delete_note` for tombstones".into(),
            ));
        }
        _ => {}
    }
    snapshot(tx, &id, old.version, &op.op_id)?;
    let mut note = old.note.clone();
    note.status = status;
    note.updated_at = updated_at;
    write_note(tx, &note, true, false)?;
    reindex_note(tx, &id, Some(&old))?;
    Ok(())
}

fn apply_supersede(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let old_id = Ulid::from_string(&payload_str(op, "old_id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad old_id", op.op_id)))?;
    let new = note_from_payload(op, "note")?;
    let valid_to = Validity::parse(&payload_str(op, "valid_to")?)
        .map_err(|e| CellError::Invalid(format!("bad `valid_to`: {e}")))?;
    let old_updated_at = crate::format::At::parse(&payload_str(op, "old_updated_at")?)
        .map_err(|e| CellError::Invalid(format!("bad `old_updated_at`: {e}")))?;
    let Some(old_row) = read_note(tx, &old_id)? else {
        return Err(CellError::NoteNotFound(old_id.to_string()));
    };
    if new.id == old_id {
        return Err(CellError::Invalid("a note cannot supersede itself".into()));
    }
    // 1. the new note
    let new_old = read_note(tx, &new.id)?;
    if let Some(r) = &new_old {
        snapshot(tx, &new.id, r.version, &op.op_id)?;
    }
    write_note(tx, &new, new_old.is_some(), false)?;
    reindex_note(tx, &new.id, new_old.as_ref())?;
    // 2. the old note closes
    snapshot(tx, &old_id, old_row.version, &op.op_id)?;
    let mut old = old_row.note.clone();
    old.status = Status::Superseded;
    old.valid_to = Some(valid_to);
    old.updated_at = old_updated_at;
    write_note(tx, &old, true, false)?;
    reindex_note(tx, &old_id, Some(&old_row))?;
    // 3. the link
    tx.execute(
        "INSERT OR IGNORE INTO supersedes (new_id, old_id) VALUES (?1, ?2)",
        rusqlite::params![new.id.to_string(), old_id.to_string()],
    )?;
    Ok(())
}

fn apply_delete(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = Ulid::from_string(&payload_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let updated_at = crate::format::At::parse(&payload_str(op, "updated_at")?)
        .map_err(|e| CellError::Invalid(format!("bad `updated_at`: {e}")))?;
    let Some(old) = read_note(tx, &id)? else {
        return Err(CellError::NoteNotFound(id.to_string()));
    };
    snapshot(tx, &id, old.version, &op.op_id)?;
    let mut note = old.note.clone();
    note.status = Status::Deleted;
    note.updated_at = updated_at;
    write_note(tx, &note, true, false)?;
    reindex_note(tx, &id, Some(&old))?;
    Ok(())
}

fn apply_restore(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = Ulid::from_string(&payload_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let version: i64 = op
        .payload
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| CellError::Invalid(format!("op {} misses `version`", op.op_id)))?;
    let updated_at = crate::format::At::parse(&payload_str(op, "updated_at")?)
        .map_err(|e| CellError::Invalid(format!("bad `updated_at`: {e}")))?;
    let snap: String = tx
        .query_row(
            "SELECT snapshot_json FROM note_versions WHERE note_id = ?1 AND version = ?2",
            rusqlite::params![id.to_string(), version],
            |r| r.get(0),
        )
        .map_err(|_| CellError::VersionNotFound(id.to_string(), version))?;
    let restored = Note::from_json(
        &serde_json::from_str(&snap)
            .map_err(|e| CellError::Db(format!("corrupt snapshot for {id} v{version}: {e}")))?,
    )
    .map_err(|e| CellError::Db(format!("corrupt snapshot for {id} v{version}: {e}")))?;
    let Some(old) = read_note(tx, &id)? else {
        return Err(CellError::NoteNotFound(id.to_string()));
    };
    snapshot(tx, &id, old.version, &op.op_id)?;
    let mut note = restored;
    note.updated_at = updated_at;
    write_note(tx, &note, true, false)?;
    reindex_note(tx, &id, Some(&old))?;
    Ok(())
}

fn apply_link(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let e = op
        .payload
        .get("edge")
        .ok_or_else(|| CellError::Invalid(format!("op {} misses `edge`", op.op_id)))?;
    let get_s = |k: &str| -> Result<String, CellError> {
        e.get(k)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| CellError::Invalid(format!("edge misses `{k}`")))
    };
    let id = get_s("id")?;
    let src = get_s("src")?;
    let rel = get_s("rel")?;
    let dst = get_s("dst")?;
    // Entity endpoints are created implicitly; note endpoints must exist.
    for endpoint in [&src, &dst] {
        if let Some(note_id) = endpoint.strip_prefix("note:") {
            let n: i64 =
                tx.query_row("SELECT COUNT(*) FROM notes WHERE id = ?1", [note_id], |r| {
                    r.get(0)
                })?;
            if n == 0 {
                return Err(CellError::NoteNotFound(note_id.to_string()));
            }
        } else {
            ensure_entity(tx, endpoint)?;
        }
    }
    tx.execute(
        "INSERT INTO edges (id, src, rel, dst, note_id, valid_from, valid_to, confidence, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active')",
        rusqlite::params![
            id,
            src,
            rel,
            dst,
            e.get("note_id").and_then(Value::as_str),
            e.get("valid_from").and_then(Value::as_str),
            e.get("valid_to").and_then(Value::as_str),
            e.get("confidence")
                .and_then(Value::as_str)
                .unwrap_or("stated"),
        ],
    )?;
    Ok(())
}

fn apply_unlink(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = payload_str(op, "edge_id")?;
    let n = tx.execute("DELETE FROM edges WHERE id = ?1", [&id])?;
    if n == 0 {
        return Err(CellError::Invalid(format!("edge `{id}` not found")));
    }
    Ok(())
}

fn apply_confirm(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = Ulid::from_string(&payload_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let updated_at = crate::format::At::parse(&payload_str(op, "updated_at")?)
        .map_err(|e| CellError::Invalid(format!("bad `updated_at`: {e}")))?;
    let Some(old) = read_note(tx, &id)? else {
        return Err(CellError::NoteNotFound(id.to_string()));
    };
    if old.note.status != Status::Quarantined {
        return Err(CellError::Invalid(format!(
            "note {id} is not quarantined; nothing to confirm"
        )));
    }
    snapshot(tx, &id, old.version, &op.op_id)?;
    let mut note = old.note.clone();
    note.status = Status::Active;
    note.updated_at = updated_at;
    write_note(tx, &note, true, true)?;
    reindex_note(tx, &id, Some(&old))?;
    Ok(())
}

/// Read every op row in sequence order (for replay and verification).
///
/// # Errors
/// [`CellError::Db`] on SQL failure or malformed rows.
pub fn read_ops(conn: &Connection) -> Result<Vec<Op>, CellError> {
    let mut stmt = conn.prepare(
        "SELECT seq, op_id, hlc, actor, kind, note_id, payload_json, prev_hash, hash
         FROM ops ORDER BY seq",
    )?;
    let ops = stmt
        .query_map([], |r| {
            Ok(op_from_row(OpRow {
                seq: r.get(0)?,
                op_id: &r.get::<_, String>(1)?,
                hlc: &r.get::<_, String>(2)?,
                actor: &r.get::<_, String>(3)?,
                kind: &r.get::<_, String>(4)?,
                note_id: r.get::<_, Option<String>>(5)?.as_deref(),
                payload_json: &r.get::<_, String>(6)?,
                prev_hash: &r.get::<_, String>(7)?,
                hash: &r.get::<_, String>(8)?,
            }))
        })?
        .collect::<Result<Result<Vec<_>, _>, _>>()??;
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden vector of spec §6.1: a fixed payload with its expected
    /// hash. Any silent change of the canonical JSON (e.g. a dependency
    /// enabling serde_json's `preserve_order`) breaks this test before it
    /// breaks every cell ever written.
    #[test]
    fn hash_golden_vector() {
        // keys inserted in scrambled order: the canonical form must sort them
        let mut m = serde_json::Map::new();
        m.insert("x".into(), serde_json::json!(1));
        m.insert("kind".into(), serde_json::json!("init"));
        m.insert("note_id".into(), serde_json::Value::Null);
        m.insert(
            "hlc".into(),
            serde_json::json!("01a17720ca0000000007.0123456789ab"),
        );
        m.insert(
            "op_id".into(),
            serde_json::json!("01F7ZQ6VTSATW2BQ5Z9R0P1S8T"),
        );
        m.insert("actor".into(), serde_json::json!("engramma"));
        let canonical = serde_json::Value::Object(m).to_string();
        assert_eq!(
            canonical,
            "{\"actor\":\"engramma\",\"hlc\":\"01a17720ca0000000007.0123456789ab\",\
             \"kind\":\"init\",\"note_id\":null,\"op_id\":\"01F7ZQ6VTSATW2BQ5Z9R0P1S8T\",\"x\":1}"
        );
        assert_eq!(
            op_hash(GENESIS_HASH, &canonical),
            "a217239290ba91e17196ab1812bd01a1e3d05e71f6048387017212636c555d46"
        );
    }

    #[test]
    fn envelope_covers_the_whole_row() {
        let at = crate::format::At::parse("2026-11-05T10:00:00Z").unwrap(/* test-only */);
        let base = crate::Note {
            id: Ulid::from_string("01F7ZQ6VTSATW2BQ5Z9R0P1S8T")
                .unwrap(/* test-only: valid ULID */),
            note_type: crate::format::NoteType::Fact,
            title: None,
            body: "corpo".into(),
            tags: vec![],
            scope: None,
            lang: None,
            source: "test".into(),
            confidence: crate::format::Confidence::Stated,
            status: crate::format::Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: at,
            updated_at: at,
            extensions: vec![],
        };
        let hlc = crate::hlc::Hlc::parse("01a17720ca0000000007.0123456789ab")
            .unwrap(/* test-only */);
        let op = make_op(
            base.id,
            hlc.clone(),
            "engramma",
            OpKind::PutNote,
            Some(base.id),
            serde_json::json!({ "note": base.to_json() }),
        )
        .unwrap(/* test-only */);
        // the envelope carries note_id too
        assert_eq!(
            op.payload.get("note_id").and_then(Value::as_str),
            Some("01F7ZQ6VTSATW2BQ5Z9R0P1S8T")
        );
        check_envelope(&op).unwrap(/* test-only */);
        // a disagreeing column is caught
        let mut tampered = op.clone();
        tampered.kind = OpKind::Confirm;
        assert!(check_envelope(&tampered).is_err());
        let mut tampered_null = op.clone();
        tampered_null.note_id = None;
        assert!(check_envelope(&tampered_null).is_err());
        // payloads cannot smuggle envelope keys
        assert!(
            make_op(
                base.id,
                hlc.clone(),
                "engramma",
                OpKind::Init,
                None,
                serde_json::json!({ "kind": "confirm" }),
            )
            .is_err()
        );
    }
}
