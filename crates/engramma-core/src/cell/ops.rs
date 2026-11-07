// SPDX-License-Identifier: Apache-2.0

//! The change log, version 2: forgettable payloads (DESIGN §4.3, v0.3).
//!
//! `apply(state, op)` reads no clock, no randomness, no I/O: every ID, HLC,
//! timestamp and **salt** is generated when the operation is **created** and
//! travels with it. The chain no longer covers the payload itself but its
//! **salted digest** — the SD-JWT technique (RFC 9901): a naked hash of
//! personal data can still be personal data, a salted one whose salt is
//! destroyed cannot be linked back. The payload body lives in `op_payloads`
//! and can be forgotten (`purge`, compaction) without breaking the chain.
//!
//! ```text
//! payload_digest = blake3(salt ‖ canonical JSON of the body)
//! hash           = blake3(prev_hash ‖ canonical JSON of
//!                         {actor, hlc, kind, note_id, op_id, payload_digest})
//! ```
//!
//! A **redacted** operation (body forgotten) applies as a no-op: only its
//! skeleton row and the HLC tick are replayed. This can only happen for
//! notes later removed by `purge` or under a compaction snapshot; in both
//! cases the live state agrees with the redacted replay, and the spec says
//! exactly that.
//!
//! The v1 rule (`hash = blake3(prev_hash ‖ canonical payload with the
//! envelope inside)`) is kept for the v5 migration, which verifies the old
//! chain before rewriting it.

use rusqlite::{Connection, Transaction};
use serde_json::Value;
use ulid::Ulid;

use crate::error::CellError;
use crate::format::body;
use crate::format::{Note, Status, Validity};
use crate::text;

/// Operation kinds (one row in `ops` each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Right to be forgotten (v0.3): remove the note and everything derived
    /// from it, and forget the payloads of its operations.
    Purge,
}

impl OpKind {
    /// Every kind, in declaration order (tests count coverage).
    pub const ALL: [OpKind; 11] = [
        OpKind::Init,
        OpKind::PutNote,
        OpKind::PatchNote,
        OpKind::SetStatus,
        OpKind::Supersede,
        OpKind::DeleteNote,
        OpKind::Restore,
        OpKind::Link,
        OpKind::Unlink,
        OpKind::Confirm,
        OpKind::Purge,
    ];

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
            OpKind::Purge => "purge",
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
            "purge" => OpKind::Purge,
            other => return Err(CellError::Invalid(format!("unknown op kind `{other}`"))),
        })
    }
}

/// One row of the change log. The payload body and its salt are `None` for a
/// redacted operation (forgotten by `purge` or compaction); the skeleton —
/// columns and hashes — is always whole.
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
    /// The payload body — everything but the envelope fields — when present.
    pub body: Option<Value>,
    /// The 16-byte salt of the digest, when the payload lives on.
    pub salt: Option<Vec<u8>>,
    /// `blake3(salt ‖ canonical body)`; the chain covers this, not the body.
    pub payload_digest: String,
    /// Previous op hash (genesis: 64 `0`s).
    pub prev_hash: String,
    /// This op hash.
    pub hash: String,
}

impl Op {
    /// The envelope this op hashes over: the `ops` columns themselves plus
    /// the payload digest, as canonical JSON with sorted keys.
    fn envelope_json(&self) -> String {
        let note_id = self
            .note_id
            .map(|u| Value::String(u.to_string()))
            .unwrap_or(Value::Null);
        serde_json::json!({
            "actor": self.actor,
            "hlc": self.hlc.to_string(),
            "kind": self.kind.as_str(),
            "note_id": note_id,
            "op_id": self.op_id.to_string(),
            "payload_digest": self.payload_digest,
        })
        .to_string()
    }

    /// Seal the op against its predecessor: sets `prev_hash` and `hash`.
    pub fn seal(&mut self, prev_hash: &str) {
        self.prev_hash = prev_hash.to_string();
        self.hash = op_hash(prev_hash, &self.envelope_json());
    }

    /// The merged payload (envelope + body) — the reader's view, v1 shape.
    #[must_use]
    pub fn payload(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("op_id".into(), self.op_id.to_string().into());
        m.insert("hlc".into(), self.hlc.to_string().into());
        m.insert("actor".into(), self.actor.clone().into());
        m.insert("kind".into(), self.kind.as_str().into());
        m.insert(
            "note_id".into(),
            self.note_id
                .map(|u| Value::String(u.to_string()))
                .unwrap_or(Value::Null),
        );
        if let Some(body) = &self.body
            && let Value::Object(fields) = body
        {
            for (k, v) in fields {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }
}

/// Genesis `prev_hash`.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Canonical JSON (serde_json maps are BTreeMaps: keys sorted at every
/// level, no spaces).
pub fn canonical_json(v: &Value) -> String {
    v.to_string()
}

/// The salted payload digest (SD-JWT technique, RFC 9901).
pub fn payload_digest(salt: &[u8], body: &Value) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(salt);
    hasher.update(canonical_json(body).as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Compute a v2 op hash over the predecessor and the envelope.
pub fn op_hash(prev_hash: &str, envelope_json: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(envelope_json.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// The v1 rule (`hash = blake3(prev_hash ‖ canonical payload)`), kept for the
/// v5 migration: it must verify the old chain before rewriting it.
pub fn op_hash_v1(prev_hash: &str, payload_json: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(payload_json.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Build an operation from its parts. The salt is generated by the caller at
/// creation time (like the op id and the HLC) so `apply` stays deterministic.
///
/// # Errors
/// [`CellError::Invalid`] when the body is not an object or smuggles an
/// envelope field.
pub fn make_op(
    op_id: Ulid,
    hlc: crate::hlc::Hlc,
    actor: &str,
    kind: OpKind,
    note_id: Option<Ulid>,
    body: Value,
    salt: Vec<u8>,
) -> Result<Op, CellError> {
    let Value::Object(fields) = &body else {
        return Err(CellError::Invalid(
            "op payload body must be an object".into(),
        ));
    };
    for k in fields.keys() {
        if matches!(
            k.as_str(),
            "op_id" | "hlc" | "actor" | "kind" | "note_id" | "payload_digest"
        ) {
            return Err(CellError::Invalid(format!(
                "op payload body must not carry the envelope field `{k}`"
            )));
        }
    }
    let digest = payload_digest(&salt, &body);
    Ok(Op {
        seq: 0,
        op_id,
        hlc,
        actor: actor.to_string(),
        kind,
        note_id,
        body: Some(body),
        salt: Some(salt),
        payload_digest: digest,
        prev_hash: String::new(),
        hash: String::new(),
    })
}

/// The raw fields of a stored `ops` row (one per column) with its joined
/// payload, when present.
#[allow(missing_docs)]
pub struct OpRow<'a> {
    pub seq: i64,
    pub op_id: &'a str,
    pub hlc: &'a str,
    pub actor: &'a str,
    pub kind: &'a str,
    pub note_id: Option<&'a str>,
    pub payload_digest: &'a str,
    pub salt: Option<&'a [u8]>,
    pub body_json: Option<&'a str>,
    pub prev_hash: &'a str,
    pub hash: &'a str,
}

/// Rebuild an [`Op`] from a stored row.
///
/// # Errors
/// [`CellError::Invalid`] on malformed rows or a body/salt disagreement
/// (both must be present, or both forgotten).
pub fn op_from_row(r: OpRow<'_>) -> Result<Op, CellError> {
    if r.salt.is_some() != r.body_json.is_some() {
        return Err(CellError::Invalid(format!(
            "op {}: payload body and salt disagree (one forgotten, one present)",
            r.op_id
        )));
    }
    let body = r
        .body_json
        .map(|b| {
            serde_json::from_str::<Value>(b)
                .map_err(|e| CellError::Invalid(format!("bad payload in op {}: {e}", r.op_id)))
        })
        .transpose()?;
    Ok(Op {
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
        body,
        salt: r.salt.map(<[u8]>::to_vec),
        payload_digest: r.payload_digest.to_string(),
        prev_hash: r.prev_hash.to_string(),
        hash: r.hash.to_string(),
    })
}

// ---------------------------------------------------------------------------
// apply: the deterministic state transition
// ---------------------------------------------------------------------------

/// Apply one operation inside an open transaction. Deterministic: the payload
/// carries everything; nothing is read from the environment. A **redacted**
/// op (no body) writes only its skeleton row and the HLC tick — its state
/// change is gone with the payload, which is only ever correct for purged
/// notes or ops under a compaction snapshot.
///
/// # Errors
/// [`CellError`] on invariant violations (missing note, illegal transition,
/// malformed payload) or SQL failures.
pub fn apply(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    tx.execute(
        "INSERT INTO ops (seq, op_id, hlc, actor, kind, note_id, payload_digest, prev_hash, hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            op.seq,
            op.op_id.to_string(),
            op.hlc.to_string(),
            op.actor,
            op.kind.as_str(),
            op.note_id.map(|u| u.to_string()),
            op.payload_digest,
            op.prev_hash,
            op.hash,
        ],
    )?;
    if let (Some(salt), Some(body)) = (&op.salt, &op.body) {
        tx.execute(
            "INSERT INTO op_payloads (op_id, salt, body) VALUES (?1, ?2, ?3)
             ON CONFLICT(op_id) DO UPDATE SET salt = ?2, body = ?3",
            rusqlite::params![op.op_id.to_string(), salt, canonical_json(body)],
        )?;
    }
    if op.body.is_some() {
        match op.kind {
            OpKind::Init => apply_init(tx, op)?,
            OpKind::PutNote => apply_put(tx, op, &note_from_body(op, "note")?)?,
            OpKind::PatchNote => apply_patch(tx, op)?,
            OpKind::SetStatus => apply_set_status(tx, op)?,
            OpKind::Supersede => apply_supersede(tx, op)?,
            OpKind::DeleteNote => apply_delete(tx, op)?,
            OpKind::Restore => apply_restore(tx, op)?,
            OpKind::Link => apply_link(tx, op)?,
            OpKind::Unlink => apply_unlink(tx, op)?,
            OpKind::Confirm => apply_confirm(tx, op)?,
            OpKind::Purge => apply_purge(tx, op)?,
        }
    }
    // The cell HLC always advances to the op's HLC (ops apply in order).
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('hlc', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [op.hlc.to_string()],
    )?;
    Ok(())
}

/// Read a required string field of the op body.
fn body_str(op: &Op, key: &str) -> Result<String, CellError> {
    op.body
        .as_ref()
        .and_then(|b| b.get(key))
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| CellError::Invalid(format!("op {} misses `{key}`", op.op_id)))
}

fn note_from_body(op: &Op, key: &str) -> Result<Note, CellError> {
    let n = Note::from_json(
        op.body
            .as_ref()
            .and_then(|b| b.get(key))
            .unwrap_or(&Value::Null),
    )
    .map_err(|e| CellError::Invalid(format!("op {} carries a bad note: {e}", op.op_id)))?;
    Ok(n)
}

fn apply_init(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let cell_id = body_str(op, "cell_id")?;
    let name = body_str(op, "name")?;
    for (k, v) in [("cell_id", cell_id), ("cell_name", name)] {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            rusqlite::params![k, v],
        )?;
    }
    Ok(())
}

/// Full column set of `notes` (FTS twin columns included).
const NOTE_COLS: &str = "id, type, title, body, frontmatter_json, scope, source, confidence, status, \
     valid_from, valid_to, created_at, updated_at, version, body_norm, tags_text";

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
    /// `tags_text` as last written: the exact value the FTS index holds for
    /// this row (external-content deletes must repeat it byte for byte).
    tags_text: String,
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
    let tags_text: String = row.get(16).unwrap_or_default();
    Ok(Some(NoteRow {
        rowid,
        note,
        version,
        body_norm,
        tags_text,
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
    tags_text: &str,
) -> Result<(), CellError> {
    tx.execute(
        "INSERT INTO notes_fts(notes_fts, rowid, title, body_norm, tags_text)
         VALUES ('delete', ?1, ?2, ?3, ?4)",
        rusqlite::params![rowid, title, body_norm, tags_text],
    )?;
    Ok(())
}

fn fts_insert(
    tx: &Transaction<'_>,
    rowid: i64,
    title: &str,
    body_norm: &str,
    tags_text: &str,
) -> Result<(), CellError> {
    tx.execute(
        "INSERT INTO notes_fts(rowid, title, body_norm, tags_text) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![rowid, title, body_norm, tags_text],
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
    // The cell-level twin of the format rule: a superseded note always has a
    // closed validity. The format rejects it on parse and the import closes
    // it in two phases, but a patch (or a full replace) could still produce
    // one through the API.
    if note.status == Status::Superseded && note.valid_to.is_none() {
        return Err(CellError::Invalid(
            "a `superseded` note must have `valid_to` (use `supersede`)".into(),
        ));
    }
    // Manual edges that cite this note follow its status in the same
    // operation (deterministic, replayed like the rest): a quarantined or
    // deleted note must not keep its manual edges visible.
    tx.execute(
        "UPDATE edges SET status = ?2 WHERE note_id = ?1 AND id NOT LIKE 'd%'",
        rusqlite::params![note.id.to_string(), note.status.as_str()],
    )?;
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
    // The index holds exactly the non-tombstoned notes: a delete for a
    // rowid that is not there corrupts FTS5's internal accounting (the
    // "database disk image is malformed" error it raises is not a no-op —
    // one delete later the row refuses to leave the index), so tombstones
    // never issue one and the error is no longer swallowed.
    if let Some(old) = old
        && old.note.status != Status::Deleted
    {
        fts_delete(
            tx,
            old.rowid,
            old.note.title.as_deref().unwrap_or(""),
            &old.body_norm,
            &old.tags_text,
        )?;
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
    // --- tags_text twin + FTS insert with the same deterministic value.
    let tags_text = fts_tags_value(&all_tags);
    tx.execute(
        "UPDATE notes SET tags_text = ?2 WHERE id = ?1",
        rusqlite::params![id.to_string(), tags_text],
    )?;
    if row.note.status != Status::Deleted {
        fts_insert(
            tx,
            row.rowid,
            row.note.title.as_deref().unwrap_or(""),
            &row.body_norm,
            &tags_text,
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
    // the derived edges, from the one source of truth (v0.3.2, P0.2: the
    // same function verify re-derives them with, chain-only)
    for e in derived_edges_of(&row.note)? {
        tx.execute(
            "INSERT INTO edges (id, src, rel, dst, note_id, valid_from, valid_to, confidence, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                e.id,
                e.src,
                e.rel,
                e.dst,
                id.to_string(),
                e.valid_from,
                e.valid_to,
                e.confidence,
                e.status,
            ],
        )?;
    }
    Ok(())
}

/// One derived (declared) edge of a note, exactly as `reindex_note` writes
/// it (v0.3.2, P0.2): the single source of truth for the `edges` rows a
/// note declares, so that `verify` can re-derive them without a replay.
pub(crate) struct DerivedEdge {
    /// Deterministic id (`d…`).
    pub id: String,
    /// Source endpoint.
    pub src: String,
    /// Relation.
    pub rel: String,
    /// Destination endpoint.
    pub dst: String,
    /// Validity start as stored.
    pub valid_from: Option<String>,
    /// Validity end as stored.
    pub valid_to: Option<String>,
    /// The note's confidence.
    pub confidence: String,
    /// The note's status.
    pub status: String,
}

/// The derived edges of a note: its mentions and its relations, with the
/// note's validity, confidence and status (the same deterministic analysis
/// `reindex_note` applies).
pub(crate) fn derived_edges_of(note: &Note) -> Result<Vec<DerivedEdge>, CellError> {
    let analysis = body::analyze(&note.body)
        .map_err(|e| CellError::Invalid(format!("note {} body: {e}", note.id)))?;
    let vf = valid_to_db(&note.valid_from);
    let vt = valid_to_db(&note.valid_to);
    let mut out = Vec::new();
    // Mention edges: note -> entity.
    for name in &analysis.links {
        out.push(DerivedEdge {
            id: derived_edge_id(&note.id, &format!("mentions:{name}")),
            src: format!("note:{}", note.id),
            rel: "mentions".into(),
            dst: name.clone(),
            valid_from: vf.clone(),
            valid_to: vt.clone(),
            confidence: note.confidence.as_str().into(),
            status: note.status.as_str().into(),
        });
    }
    // Relation edges: subject entity (or the note) -> entity.
    for r in &analysis.relations {
        let src = note
            .subject_entity()
            .unwrap_or_else(|| format!("note:{}", note.id));
        let rvf = r
            .from
            .as_ref()
            .map(|v| v.to_db_string())
            .or_else(|| vf.clone());
        let rvt =
            r.to.as_ref()
                .map(|v| v.to_db_string())
                .or_else(|| vt.clone());
        out.push(DerivedEdge {
            id: derived_edge_id(&note.id, &format!("{}:{}", r.rel, r.dst)),
            src,
            rel: r.rel.clone(),
            dst: r.dst.clone(),
            valid_from: rvf,
            valid_to: rvt,
            confidence: note.confidence.as_str().into(),
            status: note.status.as_str().into(),
        });
    }
    Ok(out)
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
    let id = Ulid::from_string(&body_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let Some(old) = read_note(tx, &id)? else {
        return Err(CellError::NoteNotFound(id.to_string()));
    };
    snapshot(tx, &id, old.version, &op.op_id)?;
    let set = op
        .body
        .as_ref()
        .and_then(|b| b.get("set"))
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
    note.updated_at = crate::format::At::parse(&body_str(op, "updated_at")?)
        .map_err(|e| CellError::Invalid(format!("bad `updated_at`: {e}")))?;
    write_note(tx, &note, true, false)?;
    reindex_note(tx, &id, Some(&old))?;
    Ok(())
}

fn apply_set_status(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = Ulid::from_string(&body_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let status = Status::parse(&body_str(op, "status")?)
        .map_err(|e| CellError::Invalid(format!("bad `status`: {e}")))?;
    let updated_at = crate::format::At::parse(&body_str(op, "updated_at")?)
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
    let old_id = Ulid::from_string(&body_str(op, "old_id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad old_id", op.op_id)))?;
    // A `purge` of the new note censors this body: the new note's content is
    // gone, the closing of the old note's validity stays replayable.
    if op
        .body
        .as_ref()
        .is_some_and(|b| b.get("note") == Some(&Value::Null))
    {
        let valid_to = Validity::parse(&body_str(op, "valid_to")?)
            .map_err(|e| CellError::Invalid(format!("bad `valid_to`: {e}")))?;
        let old_updated_at = crate::format::At::parse(&body_str(op, "old_updated_at")?)
            .map_err(|e| CellError::Invalid(format!("bad `old_updated_at`: {e}")))?;
        let Some(old_row) = read_note(tx, &old_id)? else {
            return Ok(()); // the old note is gone too (both purged)
        };
        snapshot(tx, &old_id, old_row.version, &op.op_id)?;
        let mut old = old_row.note.clone();
        old.status = Status::Superseded;
        old.valid_to = Some(valid_to);
        old.updated_at = old_updated_at;
        write_note(tx, &old, true, false)?;
        reindex_note(tx, &old_id, Some(&old_row))?;
        return Ok(());
    }
    let new = note_from_body(op, "note")?;
    let valid_to = Validity::parse(&body_str(op, "valid_to")?)
        .map_err(|e| CellError::Invalid(format!("bad `valid_to`: {e}")))?;
    let old_updated_at = crate::format::At::parse(&body_str(op, "old_updated_at")?)
        .map_err(|e| CellError::Invalid(format!("bad `old_updated_at`: {e}")))?;
    // The old note may be absent on a replay of a redacted log (its ops were
    // forgotten with its purge): the new note still exists in the live state,
    // so create it — without the closure and without the supersedes row,
    // which the purge removed in the live state too.
    let Some(old_row) = read_note(tx, &old_id)? else {
        let new_old = read_note(tx, &new.id)?;
        if let Some(r) = &new_old {
            snapshot(tx, &new.id, r.version, &op.op_id)?;
        }
        write_note(tx, &new, new_old.is_some(), false)?;
        reindex_note(tx, &new.id, new_old.as_ref())?;
        return Ok(());
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
    let id = Ulid::from_string(&body_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let updated_at = crate::format::At::parse(&body_str(op, "updated_at")?)
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
    // A tombstone has no vectors, in any model (derived data: the delete is
    // the single authority, no later sweep needed).
    super::vectors::delete_note_vectors(tx, &id)?;
    Ok(())
}

fn apply_restore(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = Ulid::from_string(&body_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let version: i64 = op
        .body
        .as_ref()
        .and_then(|b| b.get("version"))
        .and_then(Value::as_i64)
        .ok_or_else(|| CellError::Invalid(format!("op {} misses `version`", op.op_id)))?;
    let updated_at = crate::format::At::parse(&body_str(op, "updated_at")?)
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
        .body
        .as_ref()
        .and_then(|b| b.get("edge"))
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
    // A manual edge is born with the status of the note it cites (when it
    // cites one): visibility must be coherent from the first moment, not only
    // after the note's next write.
    let cited = e.get("note_id").and_then(Value::as_str);
    let status = match cited {
        Some(note_id) => tx
            .query_row("SELECT status FROM notes WHERE id = ?1", [note_id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap_or_else(|_| "active".to_string()),
        None => "active".to_string(),
    };
    tx.execute(
        "INSERT INTO edges (id, src, rel, dst, note_id, valid_from, valid_to, confidence, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            id,
            src,
            rel,
            dst,
            cited,
            e.get("valid_from").and_then(Value::as_str),
            e.get("valid_to").and_then(Value::as_str),
            e.get("confidence")
                .and_then(Value::as_str)
                .unwrap_or("stated"),
            status,
        ],
    )?;
    Ok(())
}

fn apply_unlink(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = body_str(op, "edge_id")?;
    let n = tx.execute("DELETE FROM edges WHERE id = ?1", [&id])?;
    if n == 0 {
        return Err(CellError::Invalid(format!("edge `{id}` not found")));
    }
    Ok(())
}

/// The right to be forgotten (DESIGN §4.4). Removes the note, its versions,
/// the edges it declares, its vectors and FTS row, the entities left with no
/// references, and **forgets the payloads** of every operation about the
/// note — including the `supersede` ops where it appeared as the new note.
/// The purge's own payload stays (it holds only the id and a reason).
///
/// A purge of a note that is not there is a no-op: on the live cell it makes
/// `purge` idempotent, and on a replay of a redacted log (the creating ops
/// are already forgotten) it is what keeps the replay equal to the live
/// state. The `[[Mentions]]` of the purged note in OTHER notes stay: they are
/// other notes (documented in the CLI help).
fn apply_purge(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let Some(id) = op.note_id else {
        return Err(CellError::Invalid(format!(
            "op {}: purge needs `note_id`",
            op.op_id
        )));
    };
    purge_state(tx, &id)?;
    scrub_log(tx, &id)?;
    Ok(())
}

/// The state side of the right to be forgotten: everything the note touched —
/// rows, FTS entry, vectors, orphaned entities and tags. Also the purge step
/// for a **compaction snapshot** (v0.3.1, P0.3): there no op row is appended,
/// the frozen log keeps its shape and only the state changes.
pub(crate) fn purge_state(tx: &Transaction<'_>, id: &Ulid) -> Result<(), CellError> {
    let id_str = id.to_string();
    // everything the note touched, when it is still there
    if let Some(row) = read_note(tx, id)?
        && row.note.status != Status::Deleted
    {
        // The FTS row is removed only when the index really holds it: a
        // compaction snapshot's index is empty by construction (P0.4:
        // `delete-all` at compact, rebuilt at verify), and a `delete` for a
        // rowid that is not there corrupts FTS5's accounting.
        let in_index: i64 = tx.query_row(
            "SELECT COUNT(*) FROM notes_fts_docsize WHERE id = ?1",
            [row.rowid],
            |r| r.get(0),
        )?;
        if in_index > 0 {
            fts_delete(
                tx,
                row.rowid,
                row.note.title.as_deref().unwrap_or(""),
                &row.body_norm,
                &row.tags_text,
            )?;
        }
    }
    tx.execute("DELETE FROM notes WHERE id = ?1", [&id_str])?;
    tx.execute("DELETE FROM note_versions WHERE note_id = ?1", [&id_str])?;
    tx.execute("DELETE FROM note_tags WHERE note_id = ?1", [&id_str])?;
    // tags no note carries anymore (directly or through a descendant): the
    // redacted replay never creates them, so the live state must not keep
    // them either
    tx.execute(
        "DELETE FROM tags WHERE NOT EXISTS (
             SELECT 1 FROM note_tags nt
             WHERE nt.tag = tags.name OR nt.tag LIKE tags.name || '/%'
         )",
        [],
    )?;
    tx.execute(
        "DELETE FROM supersedes WHERE new_id = ?1 OR old_id = ?1",
        [&id_str],
    )?;
    // the edges the note declares (derived and manual alike)
    tx.execute("DELETE FROM edges WHERE note_id = ?1", [&id_str])?;
    // The purged note's subject entity: unhook it, then RE-DERIVE the claim
    // (an alive note with the same title) and only then delete the entities
    // left with no claim and no references. The re-derivation is what keeps
    // a replay of the redacted log equal: there, the last surviving claim
    // still points at the alive note — without it the purge would delete an
    // entity the replay keeps (found by the compaction property tests).
    tx.execute(
        "UPDATE entities SET note_id = NULL WHERE note_id = ?1",
        [&id_str],
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
    // delete the entities with no references at all — where "reference"
    // means an edge, or the claim of an ALIVE note with that title: a claim
    // held by a tombstone is stale (the redacted replay keeps its claim on
    // the tombstone, the live side unhooks at the purge — only an
    // alive-note claim is identical on both sides)
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
    super::vectors::delete_note_vectors(tx, id)?;
    Ok(())
}

/// The log side of the purge on the LIVE cell: forget the payloads of the
/// note's operations and **censor** the `supersede` ops where it was the NEW
/// note (deterministic salt, recomputed chain tail).
pub(crate) fn scrub_log(tx: &Transaction<'_>, id: &Ulid) -> Result<(), CellError> {
    forget_note_payloads(tx, id)?;
    censor_supersedes(tx, id, &CensorFilter::Live)
}

/// The log side of the purge on a **compaction snapshot** (v0.3.1, P0.3):
/// the snapshot's log must end up identical to the live log's prefix. A
/// snapshot is a copy taken at compaction time: it may still carry payloads
/// the live log forgot with the compaction cut — for those the live `ops`
/// row kept the ORIGINAL digest, so the snapshot only deletes the body. The
/// payloads the live log still carries (`censorable`, their op ids) are
/// censored exactly like the live purge did: deterministic salt, same
/// digest, same rehash — the tail replay stays equal.
pub(crate) fn scrub_log_snapshot(
    tx: &Transaction<'_>,
    id: &Ulid,
    censorable: &std::collections::HashSet<String>,
) -> Result<(), CellError> {
    forget_note_payloads(tx, id)?;
    censor_supersedes(tx, id, &CensorFilter::Snapshot(censorable))
}

/// Which supersede payloads a censor may rewrite (see [`scrub_log_snapshot`]).
enum CensorFilter<'a> {
    /// The live log: every match is censored.
    Live,
    /// A snapshot: only op ids the live log still carries a payload for.
    Snapshot(&'a std::collections::HashSet<String>),
}

impl CensorFilter<'_> {
    fn may_censor(&self, op_id: &str) -> bool {
        match self {
            CensorFilter::Live => true,
            CensorFilter::Snapshot(set) => set.contains(op_id),
        }
    }
}

/// Forget the payloads of every op about the note (but never the `supersede`
/// ops: they carry the OTHER note's content — forgetting them would erase
/// the surviving note from every replay; they are censored instead).
fn forget_note_payloads(tx: &Transaction<'_>, id: &Ulid) -> Result<(), CellError> {
    let mut stmt = tx.prepare(
        "DELETE FROM op_payloads WHERE op_id IN (
             SELECT op_id FROM ops
             WHERE note_id = ?1 AND kind NOT IN ('purge', 'supersede'))",
    )?;
    stmt.execute([id.to_string()])?;
    Ok(())
}

/// Censor the `supersede` payloads where the note was the NEW note: the
/// content goes, the closing of the OLD note's validity stays replayable —
/// forgetting the whole body would leave the old note un-superseded in a
/// replay of the redacted log.
fn censor_supersedes(
    tx: &Transaction<'_>,
    id: &Ulid,
    filter: &CensorFilter<'_>,
) -> Result<(), CellError> {
    let id_str = id.to_string();
    let mut stmt = tx.prepare(
        "SELECT o.op_id, p.body FROM ops o JOIN op_payloads p ON p.op_id = o.op_id
         WHERE o.kind = 'supersede'",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .flatten()
        .collect();
    drop(stmt);
    let mut censored: Vec<(i64, String, String)> = Vec::new(); // (seq, op_id, body)
    let mut plain_deleted: Vec<String> = Vec::new();
    for (op_id, body) in rows {
        let parsed = serde_json::from_str::<Value>(&body).ok();
        let new_id = parsed
            .as_ref()
            .and_then(|b| b.get("note"))
            .and_then(|n| n.get("id"))
            .and_then(Value::as_str)
            .map(String::from);
        if new_id.as_deref() != Some(id_str.as_str()) {
            continue;
        }
        if !filter.may_censor(&op_id) {
            // the live log already forgot this payload (compaction cut): its
            // ops row keeps the original digest, so here the body only goes
            plain_deleted.push(op_id);
            continue;
        }
        let mut kept = serde_json::Map::new();
        for k in ["old_id", "valid_to", "old_updated_at"] {
            if let Some(v) = parsed.as_ref().and_then(|b| b.get(k)) {
                kept.insert(k.into(), v.clone());
            }
        }
        kept.insert("note".into(), Value::Null);
        let seq: i64 = tx
            .query_row("SELECT seq FROM ops WHERE op_id = ?1", [&op_id], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        censored.push((seq, op_id, Value::Object(kept).to_string()));
    }
    // the rehash of the tail depends on the order the censored ops are
    // applied in: seq order, deterministically, on the live log and on the
    // snapshot alike
    censored.sort_by_key(|(seq, _, _)| *seq);
    for (seq, op_id, censored_body) in &censored {
        // The censor's salt is DETERMINISTIC (blake3 over the op id): the
        // live purge and the in-snapshot purge censor the same original body
        // and must land on the same digest, hence on the same chain — a
        // fresh random salt would fork the two logs (found by the
        // in-snapshot censor test). The censored body carries none of the
        // purged note's content, so the salt has nothing personal left to
        // decouple.
        let salt: [u8; 16] = {
            let h = blake3::hash(format!("censor:{op_id}").as_bytes());
            let mut s = [0_u8; 16];
            s.copy_from_slice(&h.as_bytes()[..16]);
            s
        };
        let digest = payload_digest(
            &salt,
            &serde_json::from_str::<Value>(censored_body)
                .map_err(|e| CellError::Db(format!("censored body: {e}")))?,
        );
        tx.execute(
            "UPDATE op_payloads SET salt = ?2, body = ?3 WHERE op_id = ?1",
            rusqlite::params![op_id, salt, censored_body],
        )?;
        tx.execute(
            "UPDATE ops SET payload_digest = ?2 WHERE op_id = ?1",
            rusqlite::params![op_id, digest],
        )?;
        rehash_tail(tx, *seq)?;
    }
    for op_id in &plain_deleted {
        tx.execute("DELETE FROM op_payloads WHERE op_id = ?1", [op_id])?;
    }
    Ok(())
}

/// Recompute the chain from `from_seq` to the head: a censored op changes its
/// payload digest, and every later hash covers it as `prev_hash`.
fn rehash_tail(tx: &Transaction<'_>, from_seq: i64) -> Result<(), CellError> {
    let prev: String = if from_seq <= 1 {
        GENESIS_HASH.to_string()
    } else {
        tx.query_row("SELECT hash FROM ops WHERE seq = ?1", [from_seq - 1], |r| {
            r.get(0)
        })
        .unwrap_or_else(|_| GENESIS_HASH.to_string())
    };
    /// One ops row as read for the rehash: the envelope columns.
    type OpsRow = (i64, String, String, String, String, Option<String>, String);
    let rows: Vec<OpsRow> = {
        let mut stmt = tx.prepare(
            "SELECT seq, op_id, hlc, actor, kind, note_id, payload_digest
             FROM ops WHERE seq >= ?1 ORDER BY seq",
        )?;
        stmt.query_map([from_seq], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut prev = prev;
    for (seq, op_id, hlc, actor, kind, note_id, digest) in rows {
        let envelope = serde_json::json!({
            "actor": actor,
            "hlc": hlc,
            "kind": kind,
            "note_id": note_id,
            "op_id": op_id,
            "payload_digest": digest,
        })
        .to_string();
        let hash = op_hash(&prev, &envelope);
        tx.execute(
            "UPDATE ops SET prev_hash = ?2, hash = ?3 WHERE seq = ?1",
            rusqlite::params![seq, prev, hash],
        )?;
        prev = hash;
    }
    Ok(())
}

fn apply_confirm(tx: &Transaction<'_>, op: &Op) -> Result<(), CellError> {
    let id = Ulid::from_string(&body_str(op, "id")?)
        .map_err(|_| CellError::Invalid(format!("op {}: bad id", op.op_id)))?;
    let updated_at = crate::format::At::parse(&body_str(op, "updated_at")?)
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

/// Read every op row in sequence order (for replay and verification), with
/// the joined payload when it still exists.
///
/// # Errors
/// [`CellError::Db`] on SQL failure or malformed rows.
pub fn read_ops(conn: &Connection) -> Result<Vec<Op>, CellError> {
    let mut stmt = conn.prepare(
        "SELECT o.seq, o.op_id, o.hlc, o.actor, o.kind, o.note_id, o.payload_digest,
                p.salt, p.body, o.prev_hash, o.hash
         FROM ops o LEFT JOIN op_payloads p ON p.op_id = o.op_id
         ORDER BY o.seq",
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
                payload_digest: &r.get::<_, String>(6)?,
                salt: r.get::<_, Option<Vec<u8>>>(7)?.as_deref(),
                body_json: r.get::<_, Option<String>>(8)?.as_deref(),
                prev_hash: &r.get::<_, String>(9)?,
                hash: &r.get::<_, String>(10)?,
            }))
        })?
        .collect::<Result<Result<Vec<_>, _>, _>>()??;
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v2 golden vector: a fixed salt, a known body, the digest and the
    /// chain hash that must come out. Any silent change of the canonical JSON
    /// or of the envelope shape breaks this before it breaks a cell.
    #[test]
    fn hash_golden_vector_v2() {
        let salt = [7_u8; 16];
        let body: Value = serde_json::json!({ "note": { "body": "corpo", "x": 1 } });
        let digest = payload_digest(&salt, &body);
        assert_eq!(
            digest,
            "aeff683a134c22f54b920c36db80d1f730083f862962decd73876ac533fb34d3"
        );
        // keys sorted, no spaces: the envelope is the row, not the payload
        let mut op = make_op(
            Ulid::from_string("01F7ZQ6VTSATW2BQ5Z9R0P1S8T").unwrap(/* test-only */),
            crate::hlc::Hlc::parse("01a17720ca0000000007.0123456789ab").unwrap(/* t */),
            "engramma",
            OpKind::PutNote,
            Some(Ulid::from_string("01F7ZQ6VTSATW2BQ5Z9R0P1S8T").unwrap(/* t */)),
            body,
            salt.to_vec(),
        )
        .unwrap(/* test-only */);
        op.seal(GENESIS_HASH);
        assert_eq!(
            op.envelope_json(),
            "{\"actor\":\"engramma\",\"hlc\":\"01a17720ca0000000007.0123456789ab\",\
             \"kind\":\"put_note\",\"note_id\":\"01F7ZQ6VTSATW2BQ5Z9R0P1S8T\",\
             \"op_id\":\"01F7ZQ6VTSATW2BQ5Z9R0P1S8T\",\
             \"payload_digest\":\"aeff683a134c22f54b920c36db80d1f730083f862962decd73876ac533fb34d3\"}"
        );
        assert_eq!(
            op.hash,
            "13c81593af51e2faf2f301fdf1a2c85172048bc25c5cec2bb12411e974849418"
        );
    }

    /// The v1 rule stays frozen for the migration.
    #[test]
    fn hash_golden_vector_v1() {
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
            op_hash_v1(GENESIS_HASH, &canonical),
            "a217239290ba91e17196ab1812bd01a1e3d05e71f6048387017212636c555d46"
        );
    }

    #[test]
    fn body_cannot_smuggle_envelope_keys() {
        let hlc = crate::hlc::Hlc::parse("01a17720ca0000000007.0123456789ab").unwrap(/* t */);
        assert!(
            make_op(
                Ulid::nil(),
                hlc.clone(),
                "engramma",
                OpKind::Init,
                None,
                serde_json::json!({ "kind": "confirm" }),
                vec![0; 16],
            )
            .is_err()
        );
        assert!(
            make_op(
                Ulid::nil(),
                hlc.clone(),
                "engramma",
                OpKind::Init,
                None,
                serde_json::json!([]),
                vec![0; 16]
            )
            .is_err()
        );
    }
}
