// SPDX-License-Identifier: Apache-2.0

//! The cell: one SQLite file, its change log and its derived state.
//!
//! Writes go through one process-wide lock per cell file **and** a
//! `BEGIN IMMEDIATE` transaction, so exactly one writer at a time. Readers
//! (search, get) share the same connection in WAL mode.

pub mod ops;
pub mod schema;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::error::CellError;
use crate::format::{At, Note, Status, Validity};
use crate::hlc::{Hlc, HlcClock};
use ops::{Op, OpKind};

/// A manual edge as created by [`Cell::link`].
#[derive(Debug, Clone)]
pub struct EdgeInput {
    /// Source endpoint: an entity name or `note:<id>`.
    pub src: String,
    /// Relation name.
    pub rel: String,
    /// Destination endpoint.
    pub dst: String,
    /// Declaring note, if any.
    pub note_id: Option<Ulid>,
    /// Edge validity start.
    pub valid_from: Option<Validity>,
    /// Edge validity end.
    pub valid_to: Option<Validity>,
    /// Edge confidence.
    pub confidence: crate::format::Confidence,
}

/// Partial update for [`Cell::patch_note`].
#[derive(Debug, Clone, Default)]
pub struct NotePatch {
    /// New title (`None` = leave; `Some(None)` = clear).
    pub title: Option<Option<String>>,
    /// New body.
    pub body: Option<String>,
    /// New frontmatter tags.
    pub tags: Option<Vec<String>>,
    /// New scope.
    pub scope: Option<Option<String>>,
    /// New language override.
    pub lang: Option<crate::format::Lang>,
    /// New source.
    pub source: Option<String>,
    /// New confidence.
    pub confidence: Option<crate::format::Confidence>,
    /// New `valid_from`.
    pub valid_from: Option<Option<Validity>>,
    /// New `valid_to`.
    pub valid_to: Option<Option<Validity>>,
    /// New `x-*` extensions.
    pub extensions: Option<Vec<(String, crate::format::yaml::YamlValue)>>,
}

impl NotePatch {
    /// Canonical `set` object for the op payload (empty when nothing changes).
    fn to_set(&self) -> Value {
        let mut m = serde_json::Map::new();
        if let Some(t) = &self.title {
            m.insert(
                "title".into(),
                t.clone().map(Value::String).unwrap_or(Value::Null),
            );
        }
        if let Some(b) = &self.body {
            m.insert("body".into(), Value::String(b.clone()));
        }
        if let Some(t) = &self.tags {
            m.insert(
                "tags".into(),
                Value::Array(t.iter().map(|x| Value::String(x.clone())).collect()),
            );
        }
        if let Some(s) = &self.scope {
            m.insert(
                "scope".into(),
                s.clone().map(Value::String).unwrap_or(Value::Null),
            );
        }
        if let Some(l) = self.lang {
            m.insert("lang".into(), Value::String(l.as_str().into()));
        }
        if let Some(s) = &self.source {
            m.insert("source".into(), Value::String(s.clone()));
        }
        if let Some(c) = self.confidence {
            m.insert("confidence".into(), Value::String(c.as_str().into()));
        }
        if let Some(v) = &self.valid_from {
            m.insert(
                "valid_from".into(),
                v.as_ref()
                    .map(|x| Value::String(x.to_db_string()))
                    .unwrap_or(Value::Null),
            );
        }
        if let Some(v) = &self.valid_to {
            m.insert(
                "valid_to".into(),
                v.as_ref()
                    .map(|x| Value::String(x.to_db_string()))
                    .unwrap_or(Value::Null),
            );
        }
        if let Some(x) = &self.extensions {
            m.insert(
                "x".into(),
                Value::Array(
                    x.iter()
                        .map(|(k, v)| Value::Array(vec![Value::String(k.clone()), v.to_json()]))
                        .collect(),
                ),
            );
        }
        Value::Object(m)
    }
}

/// A version entry of a note's history.
#[derive(Debug, Clone)]
pub struct VersionEntry {
    /// Version number (1 = state after the first put).
    pub version: i64,
    /// Operation that produced this snapshot.
    pub op_id: Ulid,
    /// The note as it was before that operation.
    pub note: Note,
}

/// Report of [`Cell::verify`].
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Number of ops checked.
    pub ops: usize,
    /// Hash chain valid.
    pub hash_chain_ok: bool,
    /// Replay produced the same tables.
    pub replay_ok: bool,
    /// First mismatch, when `replay_ok` is false.
    pub mismatch: Option<String>,
}

impl VerifyReport {
    /// Everything passed.
    pub fn ok(&self) -> bool {
        self.hash_chain_ok && self.replay_ok
    }
}

pub(crate) struct Inner {
    pub(crate) conn: Connection,
    clock: HlcClock,
}

/// An open cell. Cheap to clone; one process-wide write lock per file.
#[derive(Clone)]
pub struct Cell {
    path: PathBuf,
    pub(crate) inner: Arc<Mutex<Inner>>,
    write_lock: Arc<Mutex<()>>,
}

fn per_cell_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    map.lock()
        .map_err(|_| CellError::Db("cell lock registry poisoned".into()))
        .map(|mut m| m.entry(path.to_path_buf()).or_default().clone())
        .unwrap_or_else(|_| Arc::new(Mutex::new(())))
}

fn configure(conn: &Connection) -> Result<(), CellError> {
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn now_ms() -> u64 {
    // One single read: two `now_utc()` calls can straddle a second boundary
    // and put the combined value almost a second in the past.
    u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or_default()
}

impl Cell {
    /// Create a new cell file (fails when it already exists).
    ///
    /// # Errors
    /// [`CellError::Io`] when the file exists or cannot be created;
    /// [`CellError::Db`] on SQL failure.
    pub fn create(path: &Path, name: &str, actor: &str) -> Result<Cell, CellError> {
        if path.exists() {
            return Err(CellError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("cell file {} already exists", path.display()),
            )));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let cell = Self::open_raw(path, false)?;
        // init op
        let cell_id = crate::id::new_ulid()?;
        let mut payload = serde_json::Map::new();
        payload.insert("cell_id".into(), cell_id.to_string().into());
        payload.insert("name".into(), name.into());
        cell.execute(OpKind::Init, None, actor, Value::Object(payload))?;
        Ok(cell)
    }

    /// Open an existing cell (runs pending migrations, restores the HLC).
    ///
    /// # Errors
    /// [`CellError::NotACell`] when the file is not a cell;
    /// [`CellError::Db`] on SQL failure.
    pub fn open(path: &Path) -> Result<Cell, CellError> {
        if !path.exists() {
            return Err(CellError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("cell file {} not found", path.display()),
            )));
        }
        Self::open_raw(path, true)
    }

    fn open_raw(path: &Path, expect_init: bool) -> Result<Cell, CellError> {
        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            schema::migrate(&tx)?;
            if expect_init {
                // A cell always has its init op; a bare database does not.
                let n: i64 = tx.query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))?;
                if n == 0 {
                    return Err(CellError::NotACell(format!(
                        "{} has no init operation",
                        path.display()
                    )));
                }
            }
            tx.commit()?;
        }
        let mut clock = HlcClock::new(crate::id::node_id().to_string(), now_ms);
        {
            let hlc_s: Option<String> = conn
                .query_row("SELECT value FROM meta WHERE key='hlc'", [], |r| r.get(0))
                .ok();
            if let Some(s) = hlc_s
                && let Ok(h) = Hlc::parse(&s)
            {
                clock.restore(&h);
            }
        }
        Ok(Cell {
            path: path.to_path_buf(),
            inner: Arc::new(Mutex::new(Inner { conn, clock })),
            write_lock: per_cell_lock(path),
        })
    }

    /// The cell file path.
    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// Checkpoint the WAL (bulk loads leave a large -wal behind).
    ///
    /// # Errors
    /// [] on SQL failure.
    pub fn checkpoint(&self) -> Result<(), CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Run a read-only query returning one TEXT column per row. Meant for
    /// tests and tooling that need to compare states.
    ///
    /// # Errors
    /// [`CellError::Db`] when the query is not a single-column SELECT.
    pub fn debug_column(&self, sql: &str) -> Result<Vec<String>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .flatten()
            .collect();
        Ok(rows)
    }

    /// Cell metadata (`cell_id`, `cell_name`, …).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn meta(&self) -> Result<HashMap<String, String>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare("SELECT key, value FROM meta")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, CellError> {
        self.inner
            .lock()
            .map_err(|_| CellError::Db("cell mutex poisoned".into()))
    }

    /// Replace the HLC clock — tests inject skewed wall clocks to play
    /// multiple processes on the same file.
    #[cfg(test)]
    pub(crate) fn inject_clock(&self, clock: HlcClock) -> Result<(), CellError> {
        let mut inner = self.lock()?;
        inner.clock = clock;
        Ok(())
    }

    /// Build and append an operation, then apply it, in one transaction.
    #[allow(clippy::too_many_arguments)]
    fn execute(
        &self,
        kind: OpKind,
        note_id: Option<Ulid>,
        actor: &str,
        payload: Value,
    ) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let mut inner = self.lock()?;
        let op_id = crate::id::new_ulid()?;
        let Inner { conn, clock } = &mut *inner;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Inside the write transaction: align with the newest HLC in the log
        // before ticking. Another process may have written since we opened
        // (the daemon and the CLI share a cell); without this, an operation
        // could receive an HLC lower than its predecessor's in the log.
        let logged: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = 'hlc'", [], |r| r.get(0))
            .optional()?;
        if let Some(s) = logged
            && let Ok(h) = Hlc::parse(&s)
        {
            clock.observe(&h);
        }
        let hlc = clock.tick();
        let (prev_seq, prev_hash): (i64, String) = tx
            .query_row(
                "SELECT seq, hash FROM ops ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, ops::GENESIS_HASH.to_string()));
        let mut op = ops::make_op(op_id, hlc.clone(), actor, kind, note_id, payload)?;
        op.seq = prev_seq + 1;
        op.prev_hash = prev_hash;
        op.hash = ops::op_hash(&op.prev_hash, &ops::canonical_json(&op.payload));
        ops::apply(&tx, &op)?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // writes
    // ------------------------------------------------------------------

    /// Create or fully replace a note. The note must carry a valid `status`
    /// (and `quarantined` for `external`); `deleted` is rejected.
    ///
    /// # Errors
    /// [`CellError::Invalid`] on invariant violations; [`CellError::Db`] on
    /// SQL failure.
    pub fn put_note(&self, note: &Note, actor: &str) -> Result<(), CellError> {
        self.execute(
            OpKind::PutNote,
            Some(note.id),
            actor,
            Value::Object([("note".to_string(), note.to_json())].into_iter().collect()),
        )
    }

    /// Apply a partial update to a note.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when the note does not exist.
    pub fn patch_note(&self, id: &Ulid, patch: &NotePatch, actor: &str) -> Result<(), CellError> {
        let set = patch.to_set();
        if set.as_object().is_some_and(|m| m.is_empty()) {
            return Err(CellError::Invalid("empty patch".into()));
        }
        let updated_at = self.now_at()?;
        let payload = serde_json::json!({
            "id": id.to_string(),
            "set": set,
            "updated_at": updated_at.to_db_string(),
        });
        self.execute(OpKind::PatchNote, Some(*id), actor, payload)
    }

    /// Change a note's status (`quarantined` or `active`, never for
    /// `external` without [`Cell::confirm`]).
    ///
    /// # Errors
    /// [`CellError::Invalid`] on illegal transitions.
    pub fn set_status(&self, id: &Ulid, status: Status, actor: &str) -> Result<(), CellError> {
        let updated_at = self.now_at()?;
        let payload = serde_json::json!({
            "id": id.to_string(),
            "status": status.as_str(),
            "updated_at": updated_at.to_db_string(),
        });
        self.execute(OpKind::SetStatus, Some(*id), actor, payload)
    }

    /// Replace `old` with `new`: the old note gets `valid_to` (from the new
    /// note's `valid_from`, or from `override_valid_to`) and `superseded`.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when the old note is missing.
    pub fn supersede(
        &self,
        old_id: &Ulid,
        new: &Note,
        override_valid_to: Option<Validity>,
        actor: &str,
    ) -> Result<(), CellError> {
        let old_updated_at = self.now_at()?;
        let valid_to = override_valid_to
            .or(new.valid_from)
            .unwrap_or_else(|| Validity(old_updated_at.get().date(), None));
        let mut new_note = new.clone();
        if !new_note.supersedes.contains(old_id) {
            new_note.supersedes.push(*old_id);
        }
        let payload = serde_json::json!({
            "old_id": old_id.to_string(),
            "note": new_note.to_json(),
            "valid_to": valid_to.to_db_string(),
            "old_updated_at": old_updated_at.to_db_string(),
        });
        self.execute(OpKind::Supersede, Some(*old_id), actor, payload)
    }

    /// Logical delete (tombstone).
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when the note does not exist.
    pub fn delete_note(&self, id: &Ulid, actor: &str) -> Result<(), CellError> {
        let updated_at = self.now_at()?;
        let payload = serde_json::json!({
            "id": id.to_string(),
            "updated_at": updated_at.to_db_string(),
        });
        self.execute(OpKind::DeleteNote, Some(*id), actor, payload)
    }

    /// Restore a previous version.
    ///
    /// # Errors
    /// [`CellError::VersionNotFound`] when the version does not exist.
    pub fn restore(&self, id: &Ulid, version: i64, actor: &str) -> Result<(), CellError> {
        let updated_at = self.now_at()?;
        let payload = serde_json::json!({
            "id": id.to_string(),
            "version": version,
            "updated_at": updated_at.to_db_string(),
        });
        self.execute(OpKind::Restore, Some(*id), actor, payload)
    }

    /// Add a manual edge.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] for unknown `note:` endpoints.
    pub fn link(&self, edge: &EdgeInput, actor: &str) -> Result<String, CellError> {
        let edge_id = crate::id::new_ulid()?.to_string();
        let payload = serde_json::json!({
            "edge": {
                "id": edge_id,
                "src": edge.src,
                "rel": edge.rel,
                "dst": edge.dst,
                "note_id": edge.note_id.map(|u| u.to_string()),
                "valid_from": edge.valid_from.as_ref().map(|v| v.to_db_string()),
                "valid_to": edge.valid_to.as_ref().map(|v| v.to_db_string()),
                "confidence": edge.confidence.as_str(),
            }
        });
        self.execute(OpKind::Link, None, actor, payload)?;
        Ok(edge_id)
    }

    /// Remove an edge by id.
    ///
    /// # Errors
    /// [`CellError::Invalid`] when the edge does not exist.
    pub fn unlink(&self, edge_id: &str, actor: &str) -> Result<(), CellError> {
        self.execute(
            OpKind::Unlink,
            None,
            actor,
            serde_json::json!({ "edge_id": edge_id }),
        )
    }

    /// Promote a quarantined note to `active`, recording the actor.
    ///
    /// # Errors
    /// [`CellError::Invalid`] when the note is not quarantined.
    pub fn confirm(&self, id: &Ulid, actor: &str) -> Result<(), CellError> {
        let updated_at = self.now_at()?;
        let payload = serde_json::json!({
            "id": id.to_string(),
            "updated_at": updated_at.to_db_string(),
        });
        self.execute(OpKind::Confirm, Some(*id), actor, payload)
    }

    // ------------------------------------------------------------------
    // reads
    // ------------------------------------------------------------------

    /// Resolve a full ULID or a prefix (≥4 chars) to one note id.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when nothing matches;
    /// [`CellError::Invalid`] when the prefix is ambiguous or malformed.
    pub fn resolve_note_id(&self, input: &str) -> Result<Ulid, CellError> {
        if input.len() == 26 {
            return Ulid::from_string(input)
                .map_err(|_| CellError::Invalid(format!("`{input}` is not a ULID")));
        }
        if input.len() < 4 || !input.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(CellError::Invalid(format!(
                "`{input}` is neither a ULID nor a prefix of at least 4 characters"
            )));
        }
        let inner = self.lock()?;
        let mut stmt = inner
            .conn
            .prepare("SELECT id FROM notes WHERE id LIKE ?1 || '%' ORDER BY id")?;
        let ids: Vec<String> = stmt.query_map([input], |r| r.get(0))?.flatten().collect();
        drop(stmt);
        match ids.first() {
            Some(only) if ids.len() == 1 => {
                Ulid::from_string(only).map_err(|_| CellError::Db("bad id".into()))
            }
            None => Err(CellError::NoteNotFound(format!(
                "no note starts with `{input}`"
            ))),
            Some(_) => Err(CellError::Invalid(format!(
                "`{input}` is ambiguous ({} notes)",
                ids.len()
            ))),
        }
    }

    /// Read a note (with its body).
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when missing.
    pub fn get(&self, id: &Ulid) -> Result<Note, CellError> {
        let inner = self.lock()?;
        let tx = inner.conn.unchecked_transaction()?;
        get_note_tx(&tx, id)
    }

    /// All versions of a note, oldest first.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when the note does not exist.
    pub fn history(&self, id: &Ulid) -> Result<Vec<VersionEntry>, CellError> {
        let inner = self.lock()?;
        let conn = &inner.conn;
        // The note must exist.
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        if n == 0 {
            return Err(CellError::NoteNotFound(id.to_string()));
        }
        let mut stmt = conn.prepare(
            "SELECT version, op_id, snapshot_json FROM note_versions
             WHERE note_id = ?1 ORDER BY version",
        )?;
        let rows = stmt.query_map([id.to_string()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (version, op_id, snap) = row?;
            let note = Note::from_json(
                &serde_json::from_str::<Value>(&snap)
                    .map_err(|e| CellError::Db(format!("corrupt snapshot: {e}")))?,
            )
            .map_err(|e| CellError::Db(format!("corrupt snapshot: {e}")))?;
            out.push(VersionEntry {
                version,
                op_id: Ulid::from_string(&op_id)
                    .map_err(|_| CellError::Db(format!("bad op_id {op_id}")))?,
                note,
            });
        }
        Ok(out)
    }

    /// Operations after `seq` (for replicas and applications).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn changes_since(&self, seq: i64) -> Result<Vec<Op>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT seq, op_id, hlc, actor, kind, note_id, payload_json, prev_hash, hash
             FROM ops WHERE seq > ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([seq], |r| {
            Ok(ops::op_from_row(ops::OpRow {
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
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    /// Every operation, in order (replay source).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn ops(&self) -> Result<Vec<Op>, CellError> {
        let inner = self.lock()?;
        ops::read_ops(&inner.conn)
    }

    // ------------------------------------------------------------------
    // verification
    // ------------------------------------------------------------------

    /// Verify the hash chain, then rebuild the whole state in memory by
    /// replaying the log and compare every table.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn verify(&self) -> Result<VerifyReport, CellError> {
        let inner = self.lock()?;
        let source_ops = ops::read_ops(&inner.conn)?;
        let mut chain_ok = true;
        let mut prev = ops::GENESIS_HASH.to_string();
        for op in &source_ops {
            let expect = ops::op_hash(&prev, &ops::canonical_json(&op.payload));
            if op.prev_hash != prev || op.hash != expect {
                chain_ok = false;
                break;
            }
            prev = op.hash.clone();
        }
        // Rebuild in memory.
        let mut rebuild_conn = Connection::open_in_memory()?;
        configure(&rebuild_conn)?;
        {
            let tx = rebuild_conn.transaction()?;
            schema::migrate(&tx)?;
            for op in &source_ops {
                ops::apply(&tx, op)?;
            }
            tx.commit()?;
        }
        let mismatch = compare_states(&inner.conn, &rebuild_conn)?;
        Ok(VerifyReport {
            ops: source_ops.len(),
            hash_chain_ok: chain_ok,
            replay_ok: mismatch.is_none(),
            mismatch,
        })
    }

    fn now_at(&self) -> Result<At, CellError> {
        let inner = self.lock()?;
        let ms = inner.clock.wall_ms();
        Ok(At(OffsetDateTime::from_unix_timestamp(ms as i64 / 1000)
            .map_err(|e| CellError::Invalid(format!("clock out of range: {e}")))?
            .replace_millisecond((ms % 1000) as u16)
            .map_err(|e| {
                CellError::Invalid(format!("clock out of range: {e}"))
            })?))
    }
}

/// Read a note through any transaction-ish handle.
pub(crate) fn get_note_tx(tx: &rusqlite::Transaction<'_>, id: &Ulid) -> Result<Note, CellError> {
    let frontmatter_json: String = tx
        .query_row(
            "SELECT frontmatter_json FROM notes WHERE id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .map_err(|_| CellError::NoteNotFound(id.to_string()))?;
    let body: String = tx.query_row(
        "SELECT body FROM notes WHERE id = ?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    let mut note = Note::from_json(
        &serde_json::from_str::<Value>(&frontmatter_json)
            .map_err(|e| CellError::Db(format!("corrupt frontmatter_json: {e}")))?,
    )
    .map_err(|e| CellError::Db(format!("corrupt frontmatter_json: {e}")))?;
    note.body = body;
    Ok(note)
}

/// Row-by-row comparison of two cell states (ops included; internal FTS
/// rowids excluded — the index is rebuilt identically by construction).
fn compare_states(a: &Connection, b: &Connection) -> Result<Option<String>, CellError> {
    let tables = [
        (
            "notes",
            "SELECT id, type, title, body, frontmatter_json, scope, source, confidence, status, valid_from, valid_to, created_at, updated_at, version, body_norm FROM notes ORDER BY id",
        ),
        (
            "note_versions",
            "SELECT note_id, version, op_id, snapshot_json FROM note_versions ORDER BY note_id, version",
        ),
        ("tags", "SELECT name, parent FROM tags ORDER BY name"),
        (
            "note_tags",
            "SELECT note_id, tag FROM note_tags ORDER BY note_id, tag",
        ),
        (
            "entities",
            "SELECT id, name, type, note_id FROM entities ORDER BY id",
        ),
        (
            "edges",
            "SELECT id, src, rel, dst, note_id, valid_from, valid_to, confidence, status FROM edges ORDER BY id",
        ),
        (
            "supersedes",
            "SELECT new_id, old_id FROM supersedes ORDER BY new_id, old_id",
        ),
        (
            "ops",
            "SELECT seq, op_id, hlc, actor, kind, note_id, payload_json, prev_hash, hash FROM ops ORDER BY seq",
        ),
        ("meta", "SELECT key, value FROM meta ORDER BY key"),
    ];
    for (name, sql) in tables {
        let render = |v: rusqlite::types::Value| -> String {
            use rusqlite::types::Value as V;
            match v {
                V::Null => "\u{0}".into(),
                V::Integer(i) => i.to_string(),
                V::Real(f) => f.to_string(),
                V::Text(t) => t,
                V::Blob(b) => format!("{:x?}", b),
            }
        };
        let dump = |conn: &Connection, out: &mut String| -> Result<(), CellError> {
            let mut stmt = conn.prepare(sql)?;
            let ncols = stmt.column_count();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next().map_err(CellError::from)? {
                for i in 0..ncols {
                    let v: rusqlite::types::Value = row.get(i)?;
                    out.push_str(&render(v));
                    out.push('\u{1}');
                }
                out.push('\n');
            }
            Ok(())
        };
        let mut sa = String::new();
        dump(a, &mut sa)?;
        let mut sb = String::new();
        dump(b, &mut sb)?;
        if sa != sb {
            // Find the first differing line for the report.
            let la: Vec<&str> = sa.lines().collect();
            let lb: Vec<&str> = sb.lines().collect();
            let first = la
                .iter()
                .zip(lb.iter())
                .enumerate()
                .find(|(i, (x, y))| {
                    let _ = i;
                    x != y
                })
                .map(|(_, (x, _))| x.to_string())
                .unwrap_or_else(|| {
                    if la.len() > lb.len() {
                        la[lb.len()].to_string()
                    } else {
                        lb[la.len()].to_string()
                    }
                });
            return Ok(Some(format!("table {name} differs around: {first}")));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::NoteType;

    fn at(day: u8) -> At {
        At(
            OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
                .unwrap(/* test-only */)
                .replace_nanosecond(0)
                .unwrap(/* test-only */),
        )
    }

    fn note(id: u16, body: &str) -> Note {
        Note {
            id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
            note_type: NoteType::Fact,
            title: Some(format!("nota {id}")),
            body: body.to_string(),
            tags: vec!["casa/milano".into()],
            scope: None,
            lang: None,
            source: "user".into(),
            confidence: crate::format::Confidence::Stated,
            status: Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: at(1),
            updated_at: at(1),
            extensions: vec![],
        }
    }

    fn test_cell() -> (tempfile::TempDir, Cell) {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("t.sqlite"), "test", "tester")
            .unwrap(/* test-only */);
        (dir, cell)
    }

    #[test]
    fn put_then_get_round_trip() {
        let (_d, cell) = test_cell();
        let n = note(1, "Fabio abita a [[Milano]] con #tag/uno\n");
        cell.put_note(&n, "tester").unwrap(/* test-only */);
        let back = cell.get(&n.id).unwrap(/* test-only */);
        assert_eq!(back, n);
        assert!(matches!(
            cell.get(&Ulid::nil()),
            Err(CellError::NoteNotFound(_))
        ));
    }

    #[test]
    fn ops_log_grows_and_hashes_chain() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "prima"), "tester").unwrap(/* test-only */);
        cell.put_note(&note(2, "seconda [[Milano]]"), "tester").unwrap(/* test-only */);
        let ops = cell.ops().unwrap(/* test-only */);
        assert_eq!(ops.len(), 3); // init + 2 puts
        assert_eq!(ops[0].kind, OpKind::Init);
        assert_ne!(ops[0].hash, ops[1].hash);
        assert_eq!(ops[1].prev_hash, ops[0].hash);
        assert_eq!(ops[2].prev_hash, ops[1].hash);
    }

    #[test]
    fn patch_bumps_version_and_keeps_history() {
        let (_d, cell) = test_cell();
        let n = note(1, "corpo iniziale [[Roma]]");
        cell.put_note(&n, "tester").unwrap(/* test-only */);
        cell.patch_note(
            &n.id,
            &NotePatch {
                body: Some("corpo nuovo [[Milano]]".into()),
                ..Default::default()
            },
            "tester",
        )
        .unwrap(/* test-only */);
        let back = cell.get(&n.id).unwrap(/* test-only */);
        assert_eq!(back.body, "corpo nuovo [[Milano]]");
        assert_ne!(back.updated_at, n.updated_at); // bumped at op creation
        let hist = cell.history(&n.id).unwrap(/* test-only */);
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].note.body, "corpo iniziale [[Roma]]");
        cell.restore(&n.id, 1, "tester").unwrap(/* test-only */);
        assert_eq!(
            cell.get(&n.id).unwrap(/* test-only */).body,
            "corpo iniziale [[Roma]]"
        );
    }

    #[test]
    fn supersede_closes_validity() {
        let (_d, cell) = test_cell();
        let old = note(1, "Fabio abita a [[Roma]]");
        let mut new = note(2, "Fabio abita a [[Milano]]");
        new.valid_from = Some(Validity(
            time::Date::from_calendar_date(2026, time::Month::September, 1)
                .unwrap(/* test-only */),
            None,
        ));
        cell.put_note(&old, "tester").unwrap(/* test-only */);
        cell.supersede(&old.id, &new, None, "tester").unwrap(/* test-only */);
        let back = cell.get(&old.id).unwrap(/* test-only */);
        assert_eq!(back.status, Status::Superseded);
        assert_eq!(
            back.valid_to.as_ref().map(|v| v.to_db_string()),
            Some("2026-09-01".to_string())
        );
        let fresh = cell.get(&new.id).unwrap(/* test-only */);
        assert_eq!(fresh.status, Status::Active);
        assert_eq!(fresh.supersedes, vec![old.id]);
    }

    #[test]
    fn delete_is_a_tombstone_and_stays_in_log() {
        let (_d, cell) = test_cell();
        let n = note(1, "da cancellare");
        cell.put_note(&n, "tester").unwrap(/* test-only */);
        cell.delete_note(&n.id, "tester").unwrap(/* test-only */);
        let back = cell.get(&n.id).unwrap(/* test-only */);
        assert_eq!(back.status, Status::Deleted);
        assert!(cell.ops().unwrap(/* test-only */).iter().any(|o| o.kind == OpKind::DeleteNote));
    }

    #[test]
    fn quarantine_flow() {
        let (_d, cell) = test_cell();
        let mut n = note(1, "da internet");
        n.confidence = crate::format::Confidence::External;
        n.source = "import:web".into();
        n.status = Status::Quarantined;
        cell.put_note(&n, "tester").unwrap(/* test-only */);
        // set_status cannot make an external note active
        assert!(cell.set_status(&n.id, Status::Active, "tester").is_err());
        cell.confirm(&n.id, "fabio").unwrap(/* test-only */);
        assert_eq!(
            cell.get(&n.id).unwrap(/* test-only */).status,
            Status::Active
        );
        // confirming again fails
        assert!(cell.confirm(&n.id, "fabio").is_err());
    }

    #[test]
    fn put_rejects_external_active() {
        let (_d, cell) = test_cell();
        let mut n = note(1, "x");
        n.confidence = crate::format::Confidence::External;
        n.status = Status::Active;
        assert!(cell.put_note(&n, "tester").is_err());
    }

    #[test]
    fn entities_and_edges_derive_from_body() {
        let (_d, cell) = test_cell();
        let mut person = note(1, "bio di Giulia");
        person.note_type = NoteType::Person;
        person.title = Some("Giulia".into());
        person.body =
            "Giulia, partner di [[Fabio]].\n\n## Relations\n- works_at:: [[Studio Rossi]]\n".into();
        cell.put_note(&person, "tester").unwrap(/* test-only */);
        let inner = cell.lock().unwrap(/* test-only */);
        let entities: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
            .unwrap(/* test-only */);
        assert_eq!(entities, 3); // Giulia, Fabio, Studio Rossi
        let subject_note: Option<String> = inner
            .conn
            .query_row(
                "SELECT note_id FROM entities WHERE name = 'Giulia'",
                [],
                |r| r.get(0),
            )
            .unwrap(/* test-only */);
        assert_eq!(
            subject_note.as_deref(),
            Some(person.id.to_string().as_str())
        );
        let works_at: i64 = inner
            .conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE src = 'Giulia' AND rel = 'works_at' AND dst = 'Studio Rossi'",
                [],
                |r| r.get(0),
            )
            .unwrap(/* test-only */);
        assert_eq!(works_at, 1);
        let mentions: i64 = inner
            .conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE src = ?1 AND rel = 'mentions' AND dst = 'Fabio'",
                [format!("note:{}", person.id)],
                |r| r.get(0),
            )
            .unwrap(/* test-only */);
        assert_eq!(mentions, 1);
    }

    #[test]
    fn fts_follows_edits() {
        let (_d, cell) = test_cell();
        let mut n = note(1, "Fabio ha traslocato a [[Milano]] con [[Giulia]]");
        n.lang = Some(crate::format::Lang::It);
        cell.put_note(&n, "tester").unwrap(/* test-only */);
        let hits = |cell: &Cell, q: &str| -> i64 {
            let inner = cell.lock().unwrap(/* test-only */);
            inner
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM notes_fts WHERE notes_fts MATCH ?1",
                    [q],
                    |r| r.get(0),
                )
                .unwrap(/* test-only */)
        };
        assert_eq!(hits(&cell, "\"trasloc\""), 1);
        // edit the body: the old token must disappear from the index
        cell.patch_note(
            &n.id,
            &NotePatch {
                body: Some("Fabio vive a [[Milano]]".into()),
                ..Default::default()
            },
            "tester",
        )
        .unwrap(/* test-only */);
        assert_eq!(hits(&cell, "\"trasloc\""), 0);
        assert_eq!(hits(&cell, "\"viv\""), 1);
        // delete removes the note from the index
        cell.delete_note(&n.id, "tester").unwrap(/* test-only */);
        assert_eq!(hits(&cell, "\"viv\""), 0);
    }

    #[test]
    fn manual_link_and_unlink() {
        let (_d, cell) = test_cell();
        let n = note(1, "x");
        cell.put_note(&n, "tester").unwrap(/* test-only */);
        let edge_id = cell
            .link(
                &EdgeInput {
                    src: "Milano".into(),
                    rel: "near".into(),
                    dst: "Monza".into(),
                    note_id: None,
                    valid_from: None,
                    valid_to: None,
                    confidence: crate::format::Confidence::Stated,
                },
                "tester",
            )
            .unwrap(/* test-only */);
        let inner = cell.lock().unwrap(/* test-only */);
        let n: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM edges WHERE rel = 'near'", [], |r| r.get(0))
            .unwrap(/* test-only */);
        assert_eq!(n, 1);
        drop(inner);
        cell.unlink(&edge_id, "tester").unwrap(/* test-only */);
        assert!(cell.unlink(&edge_id, "tester").is_err());
    }

    #[test]
    fn replay_reproduces_state_and_verify_passes() {
        let (_d, cell) = test_cell();
        let n1 = note(1, "Fabio abita a [[Milano]]");
        cell.put_note(&n1, "tester").unwrap(/* test-only */);
        let mut person = note(2, "Giulia");
        person.note_type = NoteType::Person;
        person.title = Some("Giulia".into());
        person.body = "## Relations\n- works_at:: [[Studio Rossi]]\n".into();
        cell.put_note(&person, "tester").unwrap(/* test-only */);
        cell.patch_note(
            &n1.id,
            &NotePatch {
                tags: Some(vec!["lavoro/robot".into()]),
                ..Default::default()
            },
            "tester",
        )
        .unwrap(/* test-only */);
        let mut ext = note(3, "notizia esterna su [[Milano]]");
        ext.confidence = crate::format::Confidence::External;
        ext.status = Status::Quarantined;
        ext.source = "import:web".into();
        cell.put_note(&ext, "tester").unwrap(/* test-only */);
        cell.confirm(&ext.id, "fabio").unwrap(/* test-only */);
        let mut newer = note(4, "Fabio vive a [[Torino]]");
        newer.valid_from = Some(Validity(
            time::Date::from_calendar_date(2026, time::Month::November, 1)
                .unwrap(/* test-only */),
            None,
        ));
        cell.supersede(&n1.id, &newer, None, "tester").unwrap(/* test-only */);
        cell.delete_note(&ext.id, "tester").unwrap(/* test-only */);
        cell.restore(&n1.id, 2, "tester").unwrap(/* test-only */);

        let report = cell.verify().unwrap(/* test-only */);
        assert!(report.ok(), "verify failed: {:?}", report.mismatch);
        assert_eq!(report.ops, 9);
    }

    #[test]
    fn hlc_meta_follows_ops() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "x"), "tester").unwrap(/* test-only */);
        let meta = cell.meta().unwrap(/* test-only */);
        let hlc = meta.get("hlc").cloned().unwrap_or_default();
        assert!(crate::hlc::Hlc::parse(&hlc).is_ok());
    }

    #[test]
    fn hlc_is_strictly_monotonic_across_two_writers() {
        // Two handles on the same file, like the Homun daemon and the CLI:
        // the second one runs with a wall clock one hour ahead. Writes must
        // carry strictly increasing HLCs in `seq` order, whichever wrote.
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("two.sqlite");
        let a = Cell::create(&path, "p", "a").unwrap(/* test-only */);
        let b = Cell::open(&path).unwrap(/* test-only */);
        let ahead = now_ms() + 3_600_000;
        b.inject_clock(crate::hlc::HlcClock::new("b", move || ahead))
            .unwrap(/* test-only */);
        for i in 0..12 {
            let cell = if i % 2 == 0 { &a } else { &b };
            cell.put_note(&note(i + 1, "corpo [[Milano]]"), "w")
                .unwrap(/* test-only */);
        }
        let ops = a.ops().unwrap(/* test-only */);
        for pair in ops.windows(2) {
            assert!(
                pair[1].hlc > pair[0].hlc,
                "seq {} -> {}: {:?} then {:?}",
                pair[0].seq,
                pair[1].seq,
                pair[0].hlc,
                pair[1].hlc
            );
        }
    }
}
