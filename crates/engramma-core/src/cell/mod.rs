// SPDX-License-Identifier: Apache-2.0

//! The cell: one SQLite file, its change log and its derived state.
//!
//! Writes go through one process-wide lock per cell file **and** a
//! `BEGIN IMMEDIATE` transaction, so exactly one writer at a time. Readers
//! (search, get) share the same connection in WAL mode.

pub mod ops;
pub mod schema;
pub mod vectors;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::embed::Embedder;
use crate::error::CellError;
use crate::format::{At, Note, Status, Validity};
use crate::hlc::{Hlc, HlcClock};
use crate::vector::VectorIndex;
use ops::{Op, OpKind};
pub use vectors::{EmbedPendingReport, ModelCoverage, ModelRow, ModelState};

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

/// One row of the markdown view's reconciliation state (DESIGN §4.1, v0.3).
#[derive(Debug, Clone, PartialEq)]
pub struct VaultFile {
    /// Path relative to the vault folder.
    pub path: String,
    /// The note the file is bound to (by the `id` in its header).
    pub note_id: String,
    /// blake3 of the file bytes as last seen.
    pub file_hash: String,
    /// The note's version as last seen.
    pub note_version: i64,
    /// File mtime in whole seconds.
    pub mtime: i64,
    /// File size in bytes.
    pub size: i64,
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

/// Report of [`Cell::purge`] (v0.3.1, P0.2): what was forgotten, and whether
/// every byte is already gone from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    /// Payload rows forgotten (net of the one the purge op itself writes).
    pub forgotten_payloads: usize,
    /// Whether the file, the WAL and the snapshots already carry none of the
    /// bytes. False means something still holds them — see [`Self::pending`].
    pub scrubbed: bool,
    /// What could not be scrubbed yet. `"wal"`: a reader kept the WAL from
    /// being truncated (`PRAGMA wal_checkpoint(TRUNCATE)` answered busy);
    /// the next write and the cell close retry the checkpoint.
    pub pending: Vec<&'static str>,
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
    /// The FTS index matches its content table (integrity-check).
    pub fts_ok: bool,
    /// The compaction snapshot (when any) matched its recorded head hash and
    /// state digest (v0.3).
    pub snapshot_ok: bool,
    /// First mismatch, when something failed.
    pub mismatch: Option<String>,
    /// Vector coverage per model, reported separately: embeddings are
    /// derived data and their absence is never a verification failure.
    pub coverage: Vec<ModelCoverage>,
}

impl VerifyReport {
    /// Everything passed.
    pub fn ok(&self) -> bool {
        self.hash_chain_ok && self.replay_ok && self.fts_ok && self.snapshot_ok
    }
}

/// Run the FTS5 integrity-check: the index must match what its content table
/// implies (every FTS column has a twin in `notes` since schema v3).
fn fts_integrity_check(conn: &Connection) -> Result<(), CellError> {
    conn.execute(
        "INSERT INTO notes_fts(notes_fts) VALUES('integrity-check')",
        [],
    )?;
    // SQLite 3.54's integrity-check verifies the index structure but does
    // NOT compare an external-content index with its content table (checked
    // by hand), so compare the indexed rowid set ourselves: it must be
    // exactly the non-tombstoned notes.
    let diverged: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM (SELECT rowid AS id FROM notes WHERE status != 'deleted'
                                       EXCEPT SELECT id FROM notes_fts_docsize))
             + (SELECT COUNT(*) FROM (SELECT id FROM notes_fts_docsize
                                      EXCEPT SELECT rowid FROM notes WHERE status != 'deleted'))",
        [],
        |r| r.get(0),
    )?;
    if diverged > 0 {
        return Err(CellError::Db(format!(
            "the index holds {diverged} rowid(s) more or fewer than the living notes"
        )));
    }
    Ok(())
}

/// The in-memory vector index and its freshness bookkeeping.
pub(crate) struct IndexState {
    /// Model the index was built for (the `ready` one at build time).
    pub(crate) model: String,
    /// `PRAGMA data_version` at build time: another process writing the cell
    /// bumps it, and the next semantic search rebuilds.
    pub(crate) data_version: i64,
    /// The int8 index.
    pub(crate) idx: crate::vector::FlatI8,
}

/// The in-memory graph index and its freshness bookkeeping (v0.3).
pub(crate) struct GraphState {
    /// `PRAGMA data_version` at build time: another process writing the cell
    /// bumps it, and the next graph walk rebuilds.
    pub(crate) data_version: i64,
    /// The CSR index.
    pub(crate) index: crate::graph::GraphIndex,
}

/// What this cell's own writes did to the graph since the last use: the
/// touched notes (their declared edges are patched into an overlay) and
/// whether something global happened (manual edges, purge) that only a full
/// rebuild can absorb.
#[derive(Default)]
pub(crate) struct GraphDirt {
    /// Notes whose declared edges may have changed.
    pub(crate) notes: Vec<ulid::Ulid>,
    /// A rebuild-only change happened.
    pub(crate) global: bool,
}

pub(crate) struct Inner {
    pub(crate) conn: Connection,
    clock: HlcClock,
    /// Lazily built on the first semantic search (opening a cell stays ≤ 50 ms).
    pub(crate) index: Option<IndexState>,
    /// Lazily built on the first graph walk (same reason).
    pub(crate) graph: Option<GraphState>,
    /// Own writes since the graph index was last used.
    pub(crate) graph_dirt: GraphDirt,
    /// LRU cache of embedded queries, per model (default 4 096 entries,
    /// v0.3: the v0.2.1 256-slot cache made every "warm" benchmark question
    /// a re-embedding).
    pub(crate) query_cache: std::collections::HashMap<String, crate::search::LruCache>,
    /// Capacity of each per-model query LRU.
    pub(crate) query_cache_cap: usize,
    /// What a purge could not scrub yet (v0.3.1, P0.2): the WAL frames a
    /// reader kept alive. The next write and the cell close retry the
    /// checkpoint (in-memory only: a crashed process leaves the frames to
    /// the next purge or explicit `checkpoint`, declared in the REPORT).
    pub(crate) pending_scrub: Vec<&'static str>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The close-time checkpoint retry (P0.2). Drop cannot report: on
        // failure the frames stay in the WAL until the next write, purge or
        // explicit `checkpoint` — never silently marked scrubbed.
        if !self.pending_scrub.is_empty() {
            let _ = wal_checkpoint(&self.conn);
        }
    }
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
    // v0.2.1 (P0.3): the exact f32 recomputation reads candidate vectors as
    // BLOBs off the pages — with SQLite's default ~2 MB cache every read
    // reached unixRead. A 64 MB page cache plus a 256 MB read-only mmap
    // window keeps the hot pages in memory (the mmap is best-effort: not
    // every VFS supports it, and the engine stays correct without it). See
    // docs/architecture.md.
    conn.pragma_update(None, "cache_size", -65_536_i64)?;
    let _ = conn.pragma_update(None, "mmap_size", 268_435_456_i64);
    Ok(())
}

fn now_ms() -> u64 {
    // One single read: two `now_utc()` calls can straddle a second boundary
    // and put the combined value almost a second in the past.
    u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or_default()
}

/// `PRAGMA wal_checkpoint(TRUNCATE)`: copies every WAL frame into the file
/// and truncates the WAL. Returns false when it answered busy — a reader
/// holds an older snapshot and the frames (a purge's included) stay. The
/// busy timeout is shortened around the call: with the cell's 10 s timeout a
/// lingering reader would stall a purge for ten whole seconds before the
/// "busy" that makes it `pending`.
fn wal_checkpoint(conn: &Connection) -> Result<bool, CellError> {
    conn.pragma_update(None, "busy_timeout", 250_i64)?;
    let answer = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    });
    conn.pragma_update(None, "busy_timeout", 10_000_i64)?;
    let (busy, _log, _checkpointed) = answer?;
    Ok(busy == 0)
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
            inner: Arc::new(Mutex::new(Inner {
                conn,
                clock,
                index: None,
                graph: None,
                graph_dirt: GraphDirt::default(),
                query_cache: std::collections::HashMap::new(),
                query_cache_cap: crate::graph::QUERY_CACHE_CAPACITY,
                pending_scrub: Vec::new(),
            })),
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

    /// Capacity of the embedded-query LRU (one per model). The default is
    /// [`crate::graph::QUERY_CACHE_CAPACITY`]; benchmarks sizing a
    /// truly-warm cache set it to their question count. Clears the caches.
    ///
    /// # Errors
    /// [`CellError::Db`] when the cell lock is poisoned.
    pub fn set_query_cache_capacity(&self, cap: usize) -> Result<(), CellError> {
        let mut inner = self.lock()?;
        inner.query_cache.clear();
        inner.query_cache_cap = cap.max(1);
        Ok(())
    }

    /// Build (or patch) the in-memory graph index now and report how long it
    /// took with its stats — `(nodes, live edges, ~heap bytes)`. The first
    /// graph search does this lazily; the benchmark calls it explicitly.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn build_graph(&self) -> Result<(f64, (usize, usize, usize)), CellError> {
        let mut inner = self.lock()?;
        let started = std::time::Instant::now();
        crate::search::ensure_graph(&mut inner).map_err(|e| CellError::Db(e.to_string()))?;
        let stats = inner
            .graph
            .as_ref()
            .map(|g| g.index.stats())
            .unwrap_or((0, 0, 0));
        Ok((started.elapsed().as_secs_f64() * 1000.0, stats))
    }

    /// Run a **read-only** query returning one TEXT column per row. Meant for
    /// tests and tooling that need to compare states; not part of the public
    /// contract (statements that are not read-only are refused).
    ///
    /// # Errors
    /// [`CellError::Invalid`] when the statement is not read-only or does not
    /// return exactly one column; [`CellError::Db`] on SQL failure.
    #[doc(hidden)]
    pub fn debug_column(&self, sql: &str) -> Result<Vec<String>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(sql)?;
        if !stmt.readonly() {
            return Err(CellError::Invalid(format!(
                "debug_column is read-only: `{sql}` is not"
            )));
        }
        if stmt.column_count() != 1 {
            return Err(CellError::Invalid(format!(
                "debug_column wants exactly one column: `{sql}` returns {}",
                stmt.column_count()
            )));
        }
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
    /// The caller holds the per-cell write lock.
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
        execute_op(&mut inner, kind, note_id, actor, payload)?;
        Ok(())
    }

    /// Right to be forgotten (DESIGN §4.4, v0.3): the note, its versions,
    /// its edges, vectors, FTS row and orphaned entities go, and the
    /// payloads of its operations are forgotten — the chain stays whole
    /// (log v2). `secure_delete` zeroes the freed pages, the FTS `optimize`
    /// merges the segments and the `VACUUM` rebuilds the file, so neither
    /// the words nor their STEMS survive. The WAL is then truncated
    /// (`wal_checkpoint(TRUNCATE)`): when a reader keeps it open the report
    /// says so (`scrubbed: false`, `pending: ["wal"]`) and the checkpoint is
    /// retried on the next write and at close — a purge never lies "done".
    /// A failed `VACUUM`, PRAGMA or checkpoint statement is an error, not a
    /// shrug.
    ///
    /// The markdown view (v0.3) deletes the file of a purged note too;
    /// exports made before the purge are outside the engine's control (the
    /// CLI says so). `[[Mentions]]` of this note in other notes remain:
    /// they are other notes.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL or I/O failures.
    pub fn purge(&self, id: &Ulid, reason: &str, actor: &str) -> Result<PurgeReport, CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let mut inner = self.lock()?;
        let payloads_before: i64 =
            inner
                .conn
                .query_row("SELECT COUNT(*) FROM op_payloads", [], |r| r.get(0))?;
        // secure_delete answers 0 (off), 1 (on) or 2 (fast): keep the number,
        // restore the number.
        let secure_before: i64 = inner
            .conn
            .query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
        inner.conn.pragma_update(None, "secure_delete", "ON")?;
        execute_op(
            &mut inner,
            OpKind::Purge,
            Some(*id),
            actor,
            serde_json::json!({ "reason": reason }),
        )?;
        // v0.3.1 (P0.1): the FTS5 `delete` only writes a tombstone into a NEW
        // segment; the terms stay in the old ones until a merge. `optimize`
        // fuses the segments and discards the deleted entries, so the STEMS of
        // the purged body leave the index too (O(index): measured on the 100k
        // bench, REPORT §v0.3.1). It runs inside the `secure_delete` window:
        // the pages the merge frees are zeroed.
        inner
            .conn
            .execute("INSERT INTO notes_fts(notes_fts) VALUES('optimize')", [])?;
        // the in-memory vector index drops the note with its vectors
        if let Some(st) = &mut inner.index {
            st.idx.remove(&id.to_string());
        }
        inner
            .conn
            .pragma_update(None, "secure_delete", secure_before)?;
        let payloads_after: i64 =
            inner
                .conn
                .query_row("SELECT COUNT(*) FROM op_payloads", [], |r| r.get(0))?;
        // v0.3.1 (P0.2): a failed VACUUM is an error — without it the freed
        // pages stay in the file and the purge must not report success.
        inner.conn.execute_batch("VACUUM;")?;
        // secure_delete does not cover the WAL: only a checkpoint copies the
        // frames over and truncates. `TRUNCATE` answers busy when a reader
        // holds an older snapshot: that is `pending`, not done.
        let pending = if wal_checkpoint(&inner.conn)? {
            Vec::new()
        } else {
            inner.pending_scrub = vec!["wal"];
            vec!["wal"]
        };
        // the purge itself wrote one payload: the forgotten rows are the drop
        // net of it
        let forgotten = usize::try_from((payloads_before + 1 - payloads_after).max(0)).unwrap_or(0);
        Ok(PurgeReport {
            forgotten_payloads: forgotten,
            scrubbed: pending.is_empty(),
            pending,
        })
    }

    /// The forgetting state as it stands: `(scrubbed, pending)`. A purge that
    /// could not truncate the WAL (a reader was open) stays pending until the
    /// next write or the cell close completes the checkpoint.
    ///
    /// # Errors
    /// [`CellError::Db`] when the cell lock is poisoned.
    pub fn scrub_state(&self) -> Result<(bool, Vec<&'static str>), CellError> {
        let inner = self.lock()?;
        Ok((inner.pending_scrub.is_empty(), inner.pending_scrub.clone()))
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
    // derived vectors (schema v4): blue/green models, pending embedding
    // ------------------------------------------------------------------

    /// Register an embedding model for this cell, in `building` state (the
    /// blue/green start, DESIGN §7): `embed_pending` fills it while the
    /// `ready` model keeps serving searches.
    ///
    /// # Errors
    /// [`CellError::Invalid`] when another model is already building.
    pub fn add_model(&self, model_id: &str, dim: usize) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let mut inner = self.lock()?;
        let tx = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        vectors::VectorTx { conn: &tx }.add_model(model_id, dim)?;
        tx.commit()?;
        Ok(())
    }

    /// The registered embedding models.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn models(&self) -> Result<Vec<ModelRow>, CellError> {
        let inner = self.lock()?;
        vectors::VectorTx { conn: &inner.conn }.models()
    }

    /// Embed pending notes in batches with `embedder`, at most `max` notes in
    /// this call. Embedding happens **outside** the cell lock and outside
    /// every write transaction: writes stay fast, readers stay served. When a
    /// `building` model reaches 100% coverage it is promoted to `ready` in
    /// one transaction (the previous ready model retires and its vectors go).
    ///
    /// # Errors
    /// [`CellError::Invalid`] when the embedder is not the cell's target
    /// model, or the embedding call fails; [`CellError::Db`] on SQL failure.
    pub fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        max: usize,
    ) -> Result<EmbedPendingReport, CellError> {
        self.embed_pending_inner(embedder, max, false)
    }

    /// Like [`Cell::embed_pending`], but re-embeds every living note even
    /// when its hash is current (`engramma embed --all`).
    ///
    /// # Errors
    /// As [`Cell::embed_pending`].
    pub fn embed_all(
        &self,
        embedder: &dyn Embedder,
        max: usize,
    ) -> Result<EmbedPendingReport, CellError> {
        self.embed_pending_inner(embedder, max, true)
    }

    fn embed_pending_inner(
        &self,
        embedder: &dyn Embedder,
        max: usize,
        force: bool,
    ) -> Result<EmbedPendingReport, CellError> {
        if max == 0 {
            return Ok(EmbedPendingReport {
                done: 0,
                remaining: 0,
                promoted: None,
            });
        }
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        // Target model, the initial pending count (counted ONCE, then only
        // decremented — v0.2.1) and the op watermark for the mid-run delta:
        // notes another process writes while this call runs must be picked
        // up, not lost.
        let (target, r0, start_seq) = {
            let inner = self.lock()?;
            let vx = vectors::VectorTx { conn: &inner.conn };
            let Some(target) = vx.embed_target()? else {
                return Ok(EmbedPendingReport {
                    done: 0,
                    remaining: 0,
                    promoted: None,
                });
            };
            if target.model != embedder.model_id() {
                return Err(CellError::Invalid(format!(
                    "the cell wants `{}` but this embedder is `{}` (run `engramma model use {}`)",
                    target.model,
                    embedder.model_id(),
                    embedder.model_id()
                )));
            }
            let r0 = if force {
                // with force every living note is pending: no hashes needed
                inner.conn.query_row(
                    "SELECT COUNT(*) FROM notes WHERE status != 'deleted'",
                    [],
                    |r| r.get::<_, i64>(0),
                )? as usize
            } else {
                vx.pending_count(&target.model)?
            };
            let seq: i64 = inner
                .conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM ops", [], |r| r.get(0))
                .unwrap_or(0);
            (target, r0, seq)
        };
        let mut done = 0_usize;
        let mut promoted: Option<String> = None;
        let mut remaining = r0;
        if r0 > 0 {
            // Windows walk the table by key (`id > ?cursor`), hashing only
            // the rows of each window; embedding stays outside every lock.
            let mut cursor = String::new();
            'walk: loop {
                if done >= max {
                    break;
                }
                let take = vectors::BATCH.min(max - done);
                let (batch, last_id, exhausted) = {
                    let inner = self.lock()?;
                    vectors::VectorTx { conn: &inner.conn }.pending_window(
                        &target.model,
                        &cursor,
                        take,
                        force,
                    )?
                };
                if let Some(id) = last_id {
                    cursor = id;
                }
                if !batch.is_empty() {
                    let embedded = vectors::embed_notes(embedder, &batch)?;
                    {
                        let mut inner = self.lock()?;
                        let tx = inner.conn.transaction()?;
                        let vx = vectors::VectorTx { conn: &tx };
                        for (id, chunks) in &embedded {
                            let note = batch
                                .iter()
                                .find(|n| n.id == *id)
                                .ok_or_else(|| CellError::Db("batch mismatch".into()))?;
                            vx.write_note_vectors(
                                embedder.model_id(),
                                id,
                                note.title.as_deref(),
                                &note.body,
                                chunks,
                            )?;
                        }
                        tx.commit()?;
                    }
                    // This cell's own writes update the in-memory index in
                    // place (another process's writes: `data_version`).
                    {
                        let mut inner = self.lock()?;
                        if let Some(st) = &mut inner.index
                            && st.model == embedder.model_id()
                        {
                            for (id, chunks) in &embedded {
                                if chunks.is_empty() {
                                    st.idx.remove(id);
                                } else {
                                    let rows: Vec<(i64, Vec<f32>)> = chunks
                                        .iter()
                                        .enumerate()
                                        .map(|(i, (_, v))| {
                                            (i64::try_from(i).unwrap_or(0), v.clone())
                                        })
                                        .collect();
                                    st.idx.upsert(id, &rows);
                                }
                            }
                        }
                    }
                    done += embedded.len();
                    remaining = remaining.saturating_sub(embedded.len());
                }
                if exhausted {
                    break 'walk;
                }
            }
        }
        // Notes touched by another writer while this call ran: re-check their
        // staleness and count the newly-pending ones (they may sit behind the
        // cursor — they are reported, and the next call re-walks from the
        // start and embeds them).
        {
            let inner = self.lock()?;
            let touched: Vec<String> = {
                let mut stmt = inner.conn.prepare(
                    "SELECT DISTINCT note_id FROM ops WHERE seq > ?1 AND note_id IS NOT NULL",
                )?;
                let rows = stmt.query_map([start_seq], |r| r.get::<_, String>(0))?;
                rows.flatten().collect()
            };
            if !touched.is_empty() {
                let mut stmt = inner.conn.prepare(
                    "SELECT n.id, n.title, n.body, v.note_hash
                     FROM notes n
                     LEFT JOIN note_vectors v
                            ON v.note_id = n.id AND v.model = ?1 AND v.chunk = 0
                     WHERE n.status != 'deleted' AND n.id = ?2",
                )?;
                for id in &touched {
                    let row = stmt.query_row(rusqlite::params![embedder.model_id(), id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    });
                    if let Ok((_, title, body, stored)) = row {
                        let fresh = stored.as_deref()
                            == Some(
                                crate::embed::note_hash(
                                    embedder.model_id(),
                                    title.as_deref(),
                                    &body,
                                )
                                .as_str(),
                            );
                        if !fresh {
                            remaining += 1;
                        }
                    }
                }
            }
        }
        // Blue/green: a building model with full coverage becomes ready, the
        // old ready model retires with its vectors — one transaction. The
        // exact recount inside runs only at completion (`remaining == 0`),
        // never as a per-call tax.
        if remaining == 0 && target.state == ModelState::Building {
            let mut inner = self.lock()?;
            let swapped = {
                let vx = vectors::VectorTx { conn: &inner.conn };
                match vx.embed_target() {
                    Ok(Some(t)) if t.model == target.model => vx.promote_if_complete(&t.model)?,
                    Ok(_) => None,
                    Err(e) => return Err(e),
                }
            };
            if let Some(p) = swapped {
                // the ready model changed: the index rebuilds on the next
                // semantic search (never a mixed index: searches served
                // before this point used the old model end to end)
                inner.index = None;
                promoted = Some(p);
            }
        }
        Ok(EmbedPendingReport {
            done,
            remaining,
            promoted,
        })
    }

    /// The [`state_digest`] of this cell as it stands.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn state_digest(&self) -> Result<String, CellError> {
        let inner = self.lock()?;
        state_digest(&inner.conn)
    }

    /// The markdown view's reconciliation state (last scan).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_files(&self) -> Result<Vec<VaultFile>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT path, note_id, file_hash, note_version, mtime, size
             FROM vault_files ORDER BY path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(VaultFile {
                path: r.get(0)?,
                note_id: r.get(1)?,
                file_hash: r.get(2)?,
                note_version: r.get(3)?,
                mtime: r.get(4)?,
                size: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The ids of the living notes (no tombstones), in id order.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn note_ids(&self) -> Result<Vec<Ulid>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner
            .conn
            .prepare("SELECT id FROM notes WHERE status != 'deleted' ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(Ulid::from_string(&row?).map_err(|_| CellError::Db("bad note id".into()))?);
        }
        Ok(out)
    }

    /// The `version` column of a note (how many writes it took).
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when the note does not exist.
    pub fn note_version(&self, id: &Ulid) -> Result<i64, CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .query_row(
                "SELECT version FROM notes WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .map_err(|_| CellError::NoteNotFound(id.to_string()))
    }

    /// Record (or replace) one row of the markdown view's state — derived
    /// data, no operation.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_file_put(&self, row: &VaultFile) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        inner.conn.execute(
            "INSERT INTO vault_files (path, note_id, file_hash, note_version, mtime, size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET note_id = ?2, file_hash = ?3,
                                             note_version = ?4, mtime = ?5, size = ?6",
            rusqlite::params![
                row.path,
                row.note_id,
                row.file_hash,
                row.note_version,
                row.mtime,
                row.size,
            ],
        )?;
        Ok(())
    }

    /// Forget one path of the markdown view's state (the file is gone).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_file_delete(&self, path: &str) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        inner
            .conn
            .execute("DELETE FROM vault_files WHERE path = ?1", [path])?;
        Ok(())
    }

    /// The latest compaction snapshot, when any.
    fn last_snapshot(&self) -> Result<Option<(i64, String, String, String)>, CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .query_row(
                "SELECT seq, head_hash, state_digest, file FROM snapshots ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(CellError::from)
    }

    /// Compact the log (DESIGN §4.3, v0.3): `VACUUM INTO` a snapshot of the
    /// cell **exactly as it stands** (`up_to_seq` must be the current head —
    /// the point of compacting is now), register it with its head hash and
    /// its [`state_digest`], then forget the payloads of every op up to
    /// `seq`: the skeletons and the chain stay whole. Returns the snapshot
    /// path.
    ///
    /// # Errors
    /// [`CellError::Invalid`] when `up_to_seq` is not the live head;
    /// [`CellError::Db`] on SQL or I/O failures.
    pub fn compact(&self, up_to_seq: i64) -> Result<PathBuf, CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let mut inner = self.lock()?;
        let head_seq: i64 = inner
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM ops", [], |r| r.get(0))
            .unwrap_or(0);
        if head_seq == 0 || up_to_seq < 1 || up_to_seq > head_seq {
            return Err(CellError::Invalid(format!(
                "compact wants a seq within the live log (1..={head_seq}), got {up_to_seq}"
            )));
        }
        let head_hash: String = inner
            .conn
            .query_row("SELECT hash FROM ops WHERE seq = ?1", [head_seq], |r| {
                r.get(0)
            })
            .unwrap_or_default();
        let digest = state_digest(&inner.conn)?;
        let file = format!(
            "{}-snap-{head_seq}.sqlite",
            self.path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "cell".into())
                .trim_end_matches(".sqlite")
        );
        let target = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(&file);
        // VACUUM INTO refuses an existing file: a re-compact at the same head
        // replaces its own snapshot
        if target.exists() {
            std::fs::remove_file(&target)?;
        }
        inner.conn.execute_batch(&format!(
            "VACUUM INTO '{}'",
            target.display().to_string().replace('\'', "''")
        ))?;
        let created = OffsetDateTime::now_utc()
            .replace_millisecond(0)
            .unwrap_or(OffsetDateTime::now_utc())
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let tx = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO snapshots (seq, head_hash, state_digest, file, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(seq) DO UPDATE SET head_hash = ?2, state_digest = ?3,
                                            file = ?4, created_at = ?5",
            rusqlite::params![up_to_seq, head_hash, digest, file, created],
        )?;
        tx.execute(
            "DELETE FROM op_payloads WHERE op_id IN (
                 SELECT op_id FROM ops WHERE seq <= ?1)",
            [up_to_seq],
        )?;
        tx.commit()?;
        // the forgotten payloads are the point: give the space back to the OS
        inner
            .conn
            .execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(target)
    }

    // ------------------------------------------------------------------
    // reads
    // ------------------------------------------------------------------

    /// Resolve a full ULID, a unique prefix or a unique **suffix** (≥4 chars)
    /// to one note id. Suffixes are the natural handle: the last 8 characters
    /// are the random part, which is what the CLI shows.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when nothing matches;
    /// [`CellError::Invalid`] when the fragment is ambiguous or malformed.
    pub fn resolve_note_id(&self, input: &str) -> Result<Ulid, CellError> {
        self.resolve_note_id_where(input, false)
    }

    /// Like [`Cell::resolve_note_id`], but quarantined and deleted notes do
    /// not exist for the resolution: they neither match nor make a fragment
    /// ambiguous. Meant for surfaces that must not reveal hidden notes (MCP).
    ///
    /// # Errors
    /// As [`Cell::resolve_note_id`].
    pub fn resolve_visible_note_id(&self, input: &str) -> Result<Ulid, CellError> {
        self.resolve_note_id_where(input, true)
    }

    fn resolve_note_id_where(&self, input: &str, visible_only: bool) -> Result<Ulid, CellError> {
        if input.len() == 26 {
            return Ulid::from_string(input)
                .map_err(|_| CellError::Invalid(format!("`{input}` is not a ULID")));
        }
        if input.len() < 4 || !input.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(CellError::Invalid(format!(
                "`{input}` is neither a ULID nor a prefix/suffix of at least 4 characters"
            )));
        }
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(if visible_only {
            "SELECT id FROM notes WHERE (id LIKE ?1 || '%' OR id LIKE '%' || ?1)
               AND status NOT IN ('quarantined', 'deleted') ORDER BY id"
        } else {
            "SELECT id FROM notes WHERE id LIKE ?1 || '%' OR id LIKE '%' || ?1 ORDER BY id"
        })?;
        let ids: Vec<String> = stmt.query_map([input], |r| r.get(0))?.flatten().collect();
        drop(stmt);
        match ids.first() {
            Some(only) if ids.len() == 1 => {
                Ulid::from_string(only).map_err(|_| CellError::Db("bad id".into()))
            }
            None => Err(CellError::NoteNotFound(format!(
                "no note starts or ends with `{input}`"
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
        if let Some((snap_seq, _, _, _)) = self.last_snapshot()?
            && seq < snap_seq
        {
            return Err(CellError::Compacted {
                snapshot_seq: snap_seq,
            });
        }
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT o.seq, o.op_id, o.hlc, o.actor, o.kind, o.note_id, o.payload_digest,
                    p.salt, p.body, o.prev_hash, o.hash
             FROM ops o LEFT JOIN op_payloads p ON p.op_id = o.op_id
             WHERE o.seq > ?1 ORDER BY o.seq",
        )?;
        let rows = stmt.query_map([seq], |r| {
            Ok(ops::op_from_row(ops::OpRow {
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
    /// replaying the log and compare every table. Corruption (unreadable
    /// rows, columns disagreeing with the hashed envelope, a broken chain, a
    /// diverging replay) is reported in the report, not as an error.
    ///
    /// # Errors
    /// [`CellError::Db`] only on infrastructure failures.
    pub fn verify(&self) -> Result<VerifyReport, CellError> {
        let inner = self.lock()?;
        let source_ops = match ops::read_ops(&inner.conn) {
            Ok(ops) => ops,
            Err(e) => {
                return Ok(VerifyReport {
                    ops: 0,
                    hash_chain_ok: false,
                    replay_ok: false,
                    fts_ok: false,
                    snapshot_ok: false,
                    mismatch: Some(format!("unreadable log: {e}")),
                    coverage: Vec::new(),
                });
            }
        };
        let mut chain_ok = true;
        let mut prev = ops::GENESIS_HASH.to_string();
        for op in &source_ops {
            let mut sealed = op.clone();
            sealed.seal(&prev);
            if op.prev_hash != prev || op.hash != sealed.hash {
                chain_ok = false;
                break;
            }
            // When the payload lives on, its salted digest must still match
            // the body: this is what catches a changed byte (with the salt)
            // under an otherwise intact chain.
            if let (Some(salt), Some(body)) = (&op.salt, &op.body)
                && ops::payload_digest(salt, body) != op.payload_digest
            {
                chain_ok = false;
                break;
            }
            prev = op.hash.clone();
        }
        // Compaction (v0.3): the payloads up to the snapshot are forgotten,
        // so a from-zero replay is impossible — restore the snapshot into
        // memory (checking its recorded head hash and state digest) and
        // replay only the tail.
        let mut snapshot_ok = true;
        let mut rebuild_conn = Connection::open_in_memory()?;
        configure(&rebuild_conn)?;
        let mut tail: &[Op] = &source_ops;
        // (read inline: the cell lock is already held)
        let snapshot: Option<(i64, String, String, String)> = inner
            .conn
            .query_row(
                "SELECT seq, head_hash, state_digest, file FROM snapshots
                 ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((snap_seq, head_hash, digest, file)) = snapshot.as_ref() {
            let snap_path = self
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(file);
            let checks = Connection::open(&snap_path).and_then(|snap| {
                let d = state_digest(&snap)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                Ok((snap, d))
            });
            // the snapshot was taken at the op whose hash it records; the
            // payloads up to `snap_seq` (≤ that head) were forgotten after it
            let snap_head = source_ops
                .iter()
                .find(|o| &o.hash == head_hash)
                .map(|o| o.seq);
            match (snap_head, checks) {
                (Some(snap_head_seq), Ok((snap_conn, d))) if &d == digest => {
                    let tx = rebuild_conn.transaction()?;
                    tx.execute_batch("DROP TABLE IF EXISTS snapshots;")?;
                    tx.commit()?;
                    {
                        let backup =
                            rusqlite::backup::Backup::new(&snap_conn, &mut rebuild_conn)
                                .map_err(|e| CellError::Db(format!("snapshot restore: {e}")))?;
                        backup
                            .run_to_completion(128, std::time::Duration::from_millis(0), None)
                            .map_err(|e| CellError::Db(format!("snapshot restore: {e}")))?;
                    }
                    // the snapshot was taken BEFORE the forgetting: apply the
                    // same compaction cut to the restored copy, or the
                    // comparison sees payload rows the live cell dropped
                    {
                        let tx = rebuild_conn.transaction()?;
                        tx.execute(
                            "DELETE FROM op_payloads WHERE op_id IN (
                                 SELECT op_id FROM ops WHERE seq <= ?1)",
                            [*snap_seq],
                        )?;
                        tx.commit()?;
                    }
                    let from = source_ops
                        .iter()
                        .position(|o| o.seq > snap_head_seq)
                        .unwrap_or(source_ops.len());
                    tail = &source_ops[from..];
                }
                _ => snapshot_ok = false,
            }
        } else {
            let tx = rebuild_conn.transaction()?;
            schema::migrate(&tx)?;
            tx.commit()?;
        }
        let mut mismatch: Option<String> =
            (!snapshot_ok).then(|| "the compaction snapshot does not match its record".to_string());
        // Replay: the tail on the snapshot, or everything from zero.
        let mut replay_err: Option<String> = None;
        {
            let tx = rebuild_conn.transaction()?;
            for op in tail {
                if let Err(e) = ops::apply(&tx, op) {
                    replay_err = Some(e.to_string());
                    break;
                }
            }
            if replay_err.is_none() {
                tx.commit()?;
            }
        }
        if let Some(e) = replay_err {
            return Ok(VerifyReport {
                ops: source_ops.len(),
                hash_chain_ok: chain_ok,
                replay_ok: false,
                fts_ok: false,
                snapshot_ok,
                mismatch: Some(mismatch.unwrap_or_else(|| format!("replay failed: {e}"))),
                coverage: Vec::new(),
            });
        }
        if mismatch.is_none() {
            mismatch = compare_states(&inner.conn, &rebuild_conn)?;
        }
        // The FTS index is not part of the logical replay: check it against
        // its content table, on the live cell and on the rebuild alike.
        let fts_ok = match fts_integrity_check(&inner.conn)
            .and_then(|()| fts_integrity_check(&rebuild_conn))
        {
            Ok(()) => true,
            Err(e) => {
                mismatch = Some(mismatch.unwrap_or_else(|| format!("fts index: {e}")));
                false
            }
        };
        // Vector coverage is reported, not verified: embeddings are derived
        // data, recomputable from the notes at any time.
        let coverage = vectors::VectorTx { conn: &inner.conn }
            .coverage()
            .unwrap_or_default();
        Ok(VerifyReport {
            ops: source_ops.len(),
            hash_chain_ok: chain_ok,
            replay_ok: mismatch.is_none(),
            fts_ok,
            snapshot_ok,
            mismatch,
            coverage,
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

/// Build, seal and apply one operation inside one immediate transaction.
/// The caller holds the write lock and passes the locked `Inner`.
fn execute_op(
    inner: &mut Inner,
    kind: OpKind,
    note_id: Option<Ulid>,
    actor: &str,
    payload: Value,
) -> Result<(), CellError> {
    let op_id = crate::id::new_ulid()?;
    let tx = inner
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        inner.clock.observe(&h);
    }
    let hlc = inner.clock.tick();
    let (prev_seq, prev_hash): (i64, String) = tx
        .query_row(
            "SELECT seq, hash FROM ops ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, ops::GENESIS_HASH.to_string()));
    // The salt is minted at creation, like the op id and the HLC: `apply`
    // stays deterministic and the digest can be recomputed forever.
    let salt = crate::id::random_salt()?;
    let mut op = ops::make_op(op_id, hlc, actor, kind, note_id, payload, salt.to_vec())?;
    op.seq = prev_seq + 1;
    op.seal(&prev_hash);
    ops::apply(&tx, &op)?;
    tx.commit()?;
    // A tombstone leaves the index immediately (its vectors were deleted
    // inside the op). The guard is the one the caller holds.
    if kind == OpKind::DeleteNote
        && let Some(id) = note_id
        && let Some(st) = &mut inner.index
    {
        st.idx.remove(&id.to_string());
    }
    // The graph index follows the same writes (v0.3): manual edges and
    // anything that reshapes entities need a rebuild, a note write only
    // patches that note's declared edges into the overlay.
    match kind {
        OpKind::Link | OpKind::Unlink | OpKind::Purge => inner.graph_dirt.global = true,
        _ => {
            if let Some(id) = note_id {
                inner.graph_dirt.notes.push(id);
            }
            if kind == OpKind::Supersede
                && let Some(new_id) = op
                    .body
                    .as_ref()
                    .and_then(|b| b.get("note"))
                    .and_then(|n| n.get("id"))
                    .and_then(Value::as_str)
                    .and_then(|s| Ulid::from_string(s).ok())
            {
                inner.graph_dirt.notes.push(new_id);
            }
        }
    }
    // v0.3.1 (P0.2): a pending purge scrub retries its checkpoint on every
    // write. Best effort by design: a busy checkpoint must not fail an
    // unrelated write — it simply stays pending.
    if !inner.pending_scrub.is_empty() && wal_checkpoint(&inner.conn).unwrap_or(false) {
        inner.pending_scrub.clear();
    }
    Ok(())
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

/// The canonical dump of the logical tables, row by row (the comparison
/// basis of [`compare_states`] and the content of [`state_digest`]).
fn state_dump(conn: &Connection) -> Result<String, CellError> {
    let tables = [
        (
            "notes",
            "SELECT id, type, title, body, frontmatter_json, scope, source, confidence, status, valid_from, valid_to, created_at, updated_at, version, body_norm, tags_text FROM notes ORDER BY id",
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
        // The logical entity set is DERIVED: the edge endpoints plus the
        // subjects of the alive notes. The raw `entities` table is an index
        // over exactly that (claims are last-writer-wins by title, purges
        // prune orphans), and its history can legitimately differ between
        // the live cell and a replay of a redacted log — the derived set
        // cannot (v0.3, found by the purge property tests).
        (
            "entities",
            "SELECT DISTINCT lower(name) FROM (
                 SELECT src AS name FROM edges
                 UNION SELECT dst FROM edges
                 UNION SELECT trim(title) FROM notes
                 WHERE status != 'deleted' AND title IS NOT NULL
                   AND type IN ('person', 'place', 'project', 'event', 'preference')
             ) ORDER BY 1",
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
            "SELECT seq, op_id, hlc, actor, kind, note_id, payload_digest, prev_hash, hash FROM ops ORDER BY seq",
        ),
        (
            "op_payloads",
            "SELECT op_id, salt, body FROM op_payloads ORDER BY op_id",
        ),
        // The migration bookmarks are not replayable state: no op writes
        // them, so a from-zero rebuild never has them.
        (
            "meta",
            "SELECT key, value FROM meta
             WHERE key NOT IN ('log_v1_head', 'log_v2_from_seq') ORDER BY key",
        ),
    ];
    let render = |v: rusqlite::types::Value| -> String {
        use rusqlite::types::Value as V;
        match v {
            V::Null => "\u{0}".into(),
            V::Integer(i) => i.to_string(),
            V::Real(f) => f.to_string(),
            V::Text(t) => t,
            V::Blob(b) => format!("{b:x?}"),
        }
    };
    let mut out = String::new();
    for (name, sql) in tables {
        out.push_str(name);
        out.push('\n');
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
    }
    Ok(out)
}

/// blake3 of the canonical dump of the logical tables — the digest a
/// compaction snapshot records and `verify` re-checks (DESIGN §4.3, v0.3).
///
/// # Errors
/// [`CellError::Db`] on SQL failure.
pub fn state_digest(conn: &Connection) -> Result<String, CellError> {
    Ok(blake3::hash(state_dump(conn)?.as_bytes())
        .to_hex()
        .to_string())
}

/// Row-by-row comparison of two cell states.
fn compare_states(a: &Connection, b: &Connection) -> Result<Option<String>, CellError> {
    let sa = state_dump(a)?;
    let sb = state_dump(b)?;
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
        return Ok(Some(format!("state differs around: {first}")));
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
    fn superseded_requires_valid_to_through_every_operation() {
        // v0.1.1 leftover: the format and the import reject a `superseded`
        // note without `valid_to`, but the cell API could still produce one
        // (a patch clearing `valid_to`, a `put` with the status already set).
        let (_d, cell) = test_cell();
        let mut old = note(1, "Fabio abita a [[Roma]]");
        old.valid_from = Some(Validity(
            time::Date::from_calendar_date(2020, time::Month::January, 1)
                .unwrap(/* test-only */),
            None,
        ));
        cell.put_note(&old, "tester").unwrap(/* test-only */);
        let mut new = note(2, "Fabio abita a [[Milano]]");
        new.valid_from = Some(Validity(
            time::Date::from_calendar_date(2026, time::Month::September, 1)
                .unwrap(/* test-only */),
            None,
        ));
        cell.supersede(&old.id, &new, None, "tester")
            .unwrap(/* test-only */);
        // a patch clearing valid_to on the superseded note must be refused
        let err = cell.patch_note(
            &old.id,
            &NotePatch {
                valid_to: Some(None),
                ..Default::default()
            },
            "tester",
        );
        assert!(err.is_err(), "superseded without valid_to came back");
        // and so must a full replace
        let mut again = old.clone();
        again.status = Status::Superseded;
        again.valid_to = None;
        assert!(cell.put_note(&again, "tester").is_err());
        // the note is untouched: still superseded with a closed validity
        let back = cell.get(&old.id).unwrap(/* test-only */);
        assert_eq!(back.status, Status::Superseded);
        assert!(back.valid_to.is_some());
        assert!(cell.verify().unwrap(/* test-only */).ok());
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

    #[test]
    fn tampering_a_column_breaks_verify() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "prima"), "t")
            .unwrap(/* test-only */);
        cell.put_note(&note(2, "seconda"), "t")
            .unwrap(/* test-only */);
        // Swap the note_id COLUMNS of the two puts: the payloads (hashed)
        // stay untouched, the replay is unaffected — only the agreement
        // between columns and envelope can catch it.
        {
            let inner = cell.lock().unwrap(/* test-only */);
            inner
                .conn
                .execute(
                    "UPDATE ops SET note_id = CASE seq
                        WHEN 2 THEN (SELECT note_id FROM ops WHERE seq = 3)
                        WHEN 3 THEN (SELECT note_id FROM ops WHERE seq = 2)
                     END WHERE seq IN (2, 3)",
                    [],
                )
                .unwrap(/* test-only */);
        }
        let report = cell.verify().unwrap(/* test-only */);
        assert!(
            !report.ok(),
            "a swapped column must fail verification: {report:?}"
        );
        // v2: the columns ARE the hashed envelope, so the swap breaks the
        // chain itself (in v1 it only disagreed with the payload envelope).
        assert!(
            !report.hash_chain_ok,
            "the chain must catch a tampered column: {report:?}"
        );
    }

    /// Rebuild a full state from an op slice, in memory.
    fn rebuild_state(ops: &[Op]) -> Result<Connection, CellError> {
        let mut conn = Connection::open_in_memory()?;
        configure(&conn)?;
        let tx = conn.transaction()?;
        schema::migrate(&tx)?;
        for op in ops {
            ops::apply(&tx, op)?;
        }
        tx.commit()?;
        Ok(conn)
    }

    /// A compact op mix for the replay tests (put/patch/supersede/delete and
    /// manual edges), targets drawn at random.
    fn replay_mix(cell: &Cell, seed: u64, n: u16) {
        let mut s = seed | 1;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut notes: Vec<Ulid> = Vec::new();
        let draw = |pool: &[Ulid], rnd: u64| -> Option<Ulid> {
            if pool.is_empty() {
                None
            } else {
                pool.get((rnd % pool.len() as u64) as usize).copied()
            }
        };
        for i in 0..n {
            let n1 = note(i + 1, "corpo [[Milano]]");
            let n2 = note(i + 500, "sostituto [[Roma]]");
            match next() % 5 {
                0 | 1 => {
                    if cell.put_note(&n1, "t").is_ok() {
                        notes.push(n1.id);
                    }
                }
                2 => {
                    if let Some(id) = draw(&notes, next()) {
                        let patch = NotePatch {
                            body: Some(format!("nuovo {}", next() % 50)),
                            ..Default::default()
                        };
                        let _ = cell.patch_note(&id, &patch, "t");
                    }
                }
                3 => {
                    if let Some(id) = draw(&notes, next()) {
                        let _ = cell.delete_note(&id, "t");
                    }
                }
                _ => {
                    if let Some(id) = draw(&notes, next())
                        && cell.supersede(&id, &n2, None, "t").is_ok()
                    {
                        notes.push(n2.id);
                    }
                }
            }
        }
    }

    #[test]
    fn incremental_replay_matches_full_replay() {
        // The replication shape (§9): a replica holds ops[..k], receives
        // changes_since(k) and ends in exactly the same state as a full
        // replay — for random k and the boundaries.
        for seed in 1..=6u64 {
            let dir = tempfile::tempdir().unwrap(/* test-only */);
            let path = dir.path().join(format!("inc-{seed}.sqlite"));
            let cell = Cell::create(&path, "inc", "t").unwrap(/* test-only */);
            replay_mix(&cell, seed, 36);
            let all = cell.ops().unwrap(/* test-only */);
            assert!(!all.is_empty());
            let full = rebuild_state(&all).unwrap(/* test-only */);
            {
                let inner = cell.lock().unwrap(/* test-only */);
                let diff = compare_states(&inner.conn, &full).unwrap(/* t */);
                assert!(
                    diff.is_none(),
                    "full replay diverged (seed {seed}): {diff:?}"
                );
            }
            // one random k per seed plus the boundaries
            let s = (seed * 2) | 1;
            let random_k = ((s * 2_654_435_761) as usize) % (all.len() + 1);
            let mut ks = vec![0, all.len() / 2, all.len().saturating_sub(1), all.len()];
            ks.push(random_k);
            ks.sort_unstable();
            ks.dedup();
            for k in ks {
                let k = k.min(all.len());
                let tail = cell.changes_since(k as i64).unwrap(/* test-only */);
                let mut combined = all[..k].to_vec();
                combined.extend(tail);
                let inc = rebuild_state(&combined).unwrap(/* test-only */);
                let diff = compare_states(&full, &inc).unwrap(/* test-only */);
                assert!(
                    diff.is_none(),
                    "incremental replay from k={k} diverged (seed {seed}): {diff:?}"
                );
            }
        }
    }

    /// v0.3 (P1): forgetting a payload leaves the chain whole — the skeleton
    /// hashes cover the digest, not the body. (A full `verify` after a bare
    /// forget would rightly diverge: the redacted replay skips the state
    /// change. It passes end to end with `purge`, which removes the note too.)
    #[test]
    fn forgetting_a_payload_keeps_the_chain() {
        let (_d, cell) = test_cell();
        let n1 = note(1, "prima versione");
        cell.put_note(&n1, "t").unwrap(/* test-only */);
        let mut n2 = note(1, "seconda versione [[Milano]]");
        n2.title = Some("v2".into());
        cell.put_note(&n2, "t").unwrap(/* test-only */);
        // forget the FIRST put: its effect is fully replaced by the second
        {
            let inner = cell.lock().unwrap(/* test-only */);
            inner
                .conn
                .execute(
                    "DELETE FROM op_payloads WHERE op_id = (
                         SELECT op_id FROM ops WHERE kind = 'put_note' ORDER BY seq LIMIT 1)",
                    [],
                )
                .unwrap(/* test-only */);
        }
        let report = cell.verify().unwrap(/* test-only */);
        assert!(report.hash_chain_ok, "chain must survive a forgotten body");
        // and the redacted op reads back with no body
        let ops = cell.ops().unwrap(/* test-only */);
        assert!(
            ops.iter()
                .any(|o| o.body.is_none() && o.salt.is_none() && o.kind == OpKind::PutNote)
        );
    }

    /// v0.3 (P1): a changed byte of a live payload breaks the digest check.
    #[test]
    fn tampering_the_payload_body_breaks_verify() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "corpo segreto [[Milano]]"), "t")
            .unwrap(/* test-only */);
        {
            let inner = cell.lock().unwrap(/* test-only */);
            inner
                .conn
                .execute(
                    "UPDATE op_payloads SET body = replace(body, 'segreto', 'pubblico')",
                    [],
                )
                .unwrap(/* test-only */);
        }
        let report = cell.verify().unwrap(/* test-only */);
        assert!(!report.ok(), "a changed body byte must fail: {report:?}");
        assert!(!report.hash_chain_ok, "{report:?}");
    }

    /// v0.3 (P1): changing only the salt breaks the digest too — that is the
    /// point of the salt.
    #[test]
    fn tampering_the_salt_breaks_verify() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "corpo segreto [[Milano]]"), "t")
            .unwrap(/* test-only */);
        {
            let inner = cell.lock().unwrap(/* test-only */);
            inner
                .conn
                .execute("UPDATE op_payloads SET salt = x'00000000000000000000000000000000'", [])
                .unwrap(/* test-only */);
        }
        let report = cell.verify().unwrap(/* test-only */);
        assert!(!report.ok(), "a changed salt must fail: {report:?}");
        assert!(!report.hash_chain_ok, "{report:?}");
    }

    /// v0.3 (P1): a v0.2.1 cell (log v1) migrates to v2 on open — same state,
    /// verify ok, bookmark written, every payload re-salted. The v1 file is
    /// built by copying the state of a real cell and rewriting its log in the
    /// frozen v1 form (envelope inside the payload, v1 hashes).
    #[test]
    fn migration_v1_to_v2_preserves_state_and_verifies() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let n = note(1, "nota vecchia [[Roma]]");
        // 1. a real (v2) cell with one note — the reference state
        let v2path = dir.path().join("ref.sqlite");
        let cell = Cell::create(&v2path, "famiglia", "v1-writer").unwrap(/* test-only */);
        cell.put_note(&n, "v1-writer").unwrap(/* test-only */);
        let reference_ops = cell.ops().unwrap(/* test-only */);
        // 2. the v0.2.1 file: schema 1–4, same state, log in v1 form
        let v1path = dir.path().join("v1.sqlite");
        {
            let mut conn = Connection::open(&v1path).unwrap(/* test-only */);
            conn.execute(
                "ATTACH DATABASE ?1 AS src",
                [v2path.to_string_lossy().to_string()],
            )
            .unwrap(/* test-only */);
            let tx = conn.transaction().unwrap(/* test-only */);
            tx.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
            )
            .unwrap(/* test-only */);
            for sql in schema::MIGRATIONS.iter().take(4) {
                tx.execute_batch(sql).unwrap(/* test-only */);
            }
            for table in [
                "notes",
                "note_versions",
                "tags",
                "note_tags",
                "entities",
                "edges",
                "supersedes",
            ] {
                tx.execute(&format!("INSERT INTO {table} SELECT * FROM src.{table}"), [])
                    .unwrap(/* test-only */);
            }
            tx.execute(
                "INSERT INTO meta SELECT * FROM src.meta WHERE key != 'schema_version'",
                [],
            )
            .unwrap(/* test-only */);
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '4')",
                [],
            )
            .unwrap(/* test-only */);
            // the FTS index is not a table to copy: rebuild it from notes
            tx.execute("INSERT INTO notes_fts(notes_fts) VALUES('rebuild')", [])
                .unwrap(/* test-only */);
            let mut prev = ops::GENESIS_HASH.to_string();
            for op in &reference_ops {
                let payload = op.payload().to_string();
                let hash = ops::op_hash_v1(&prev, &payload);
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
                        payload,
                        prev,
                        hash,
                    ],
                )
                .unwrap(/* test-only */);
                prev = hash;
            }
            tx.commit().unwrap(/* test-only */);
        }
        // 3. open: the v5 migration runs
        let migrated = Cell::open(&v1path).unwrap(/* test-only */);
        let report = migrated.verify().unwrap(/* test-only */);
        assert!(report.ok(), "verify after migration: {:?}", report.mismatch);
        assert_eq!(migrated.get(&n.id).unwrap(/* test-only */), n);
        let meta = migrated.meta().unwrap(/* test-only */);
        assert_eq!(meta.get("cell_name").map(String::as_str), Some("famiglia"));
        assert!(
            meta.get("log_v1_head").is_some_and(|h| h.len() == 64),
            "{meta:?}"
        );
        assert_eq!(meta.get("log_v2_from_seq").map(String::as_str), Some("1"));
        for op in migrated.ops().unwrap(/* test-only */) {
            assert!(op.salt.is_some() && op.body.is_some(), "{op:?}");
        }
        // 4. a broken v1 chain refuses to migrate
        let broken = dir.path().join("v1-broken.sqlite");
        std::fs::copy(&v1path, &broken).unwrap(/* test-only */);
        {
            let conn = Connection::open(&broken).unwrap(/* test-only */);
            conn.execute(
                "UPDATE ops SET payload_json = replace(payload_json, 'famiglia', 'x') WHERE seq = 1",
                [],
            )
            .unwrap(/* test-only */);
            conn.execute(
                "UPDATE meta SET value = '4' WHERE key = 'schema_version'",
                [],
            )
            .unwrap(/* test-only */);
        }
        let err = Cell::open(&broken);
        assert!(
            err.as_ref()
                .is_err_and(|e| e.to_string().contains("broken")),
            "a broken v1 chain must refuse the migration: {}",
            err.err()
                .map_or_else(|| "opened!".into(), |e| e.to_string())
        );
    }

    /// v0.3 (P2): purge — no byte of the body survives in the file, the
    /// chain verifies, the replay agrees, and the note is unfindable.
    #[test]
    fn purge_forgets_the_note_end_to_end() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("purge.sqlite");
        let cell = Cell::create(&path, "t", "t").unwrap(/* test-only */);
        let secret = "il codice segreto di Sofia e la parola d'ordine zanzara";
        let mut n = note(1, secret);
        n.body = format!("Sofia scrive: {secret}. Vive a [[Roma]].");
        cell.put_note(&n, "t").unwrap(/* test-only */);
        let other = note(2, "una nota che resta [[Roma]]");
        cell.put_note(&other, "t").unwrap(/* test-only */);
        let forgotten = cell.purge(&n.id, "diritto all'oblio", "cli-user")
            .unwrap(/* test-only */);
        assert!(
            forgotten.forgotten_payloads >= 1,
            "at least the creating put is forgotten"
        );
        // 1. no byte of the body in the raw file (VACUUM ran inside purge)
        cell.checkpoint().unwrap(/* test-only */);
        drop(cell);
        let raw = std::fs::read(&path).unwrap(/* test-only */);
        for needle in ["zanzara", "codice segreto"] {
            assert!(
                !raw.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "`{needle}` is still in the cell file"
            );
        }
        // 2. verify: chain whole, replay equal (the redacted creating ops
        //    skip their state change; the purge removed the live note too)
        let cell = Cell::open(&path).unwrap(/* test-only */);
        let report = cell.verify().unwrap(/* test-only */);
        assert!(report.ok(), "verify after purge: {:?}", report.mismatch);
        // 3. the note is gone, the other one stays
        assert!(matches!(cell.get(&n.id), Err(CellError::NoteNotFound(_))));
        assert_eq!(cell.get(&other.id).unwrap(/* test-only */).body, other.body);
        // history: not found (the row is gone, not tombstoned)
        assert!(matches!(
            cell.history(&n.id),
            Err(CellError::NoteNotFound(_))
        ));
        // 4. the payloads of its ops are skeletons, the chain covers them
        let ops = cell.ops().unwrap(/* test-only */);
        assert!(
            ops.iter()
                .filter(|o| o.note_id == Some(n.id) && o.kind != OpKind::Purge)
                .all(|o| o.body.is_none())
        );
        assert!(
            ops.iter()
                .any(|o| o.kind == OpKind::Purge && o.body.is_some())
        );
        // a second purge is an idempotent no-op
        cell.purge(&n.id, "di nuovo", "cli-user").unwrap(/* t */);
        assert!(cell.verify().unwrap(/* test-only */).ok());
    }

    /// v0.3 (P2): a purge censors the supersede payloads where the note was
    /// the NEW note (content out, closing of the old note replayable), and
    /// the chain is recomputed over the censored digests.
    #[test]
    fn purge_censors_supersede_payloads_and_rehashes() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("sup.sqlite");
        let cell = Cell::create(&path, "t", "t").unwrap(/* test-only */);
        let mut old = note(1, "Sofia abitava a [[Roma]]");
        old.valid_from = Some(Validity(
            time::Date::from_calendar_date(2020, time::Month::January, 1)
                .unwrap(/* test-only */),
            None,
        ));
        cell.put_note(&old, "t").unwrap(/* test-only */);
        let mut new = note(2, "Sofia vive a [[Roma]]");
        new.valid_from = Some(Validity(
            time::Date::from_calendar_date(2026, time::Month::September, 1)
                .unwrap(/* test-only */),
            None,
        ));
        cell.supersede(&old.id, &new, None, "t").unwrap(/* t */);
        cell.purge(&new.id, "oblio", "t").unwrap(/* test-only */);
        let ops = cell.ops().unwrap(/* test-only */);
        // the supersede op (note_id = old) had `new` in its payload: the
        // content is gone, the closing of `old` stays replayable
        let sup = ops
            .iter()
            .find(|o| o.kind == OpKind::Supersede)
            .expect(/* test-only */ "the supersede op is still there");
        assert!(sup.body.is_some());
        assert_eq!(
            sup.body.as_ref().and_then(|b| b.get("note")),
            Some(&serde_json::Value::Null),
            "the new note's content must be censored out"
        );
        assert!(cell.verify().unwrap(/* test-only */).ok());
        // the old note stays closed: the censored supersede still replays it
        let back = cell.get(&old.id).unwrap(/* test-only */);
        assert_eq!(back.status, Status::Superseded);
        assert!(back.valid_to.is_some());
        // and the new note is gone
        assert!(matches!(cell.get(&new.id), Err(CellError::NoteNotFound(_))));
    }

    /// v0.3 (P3): compact, write more, verify — the chain is whole, the
    /// payloads up to the snapshot are gone, the tail replays on it.
    #[test]
    fn compact_then_write_then_verify() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("c.sqlite");
        let cell = Cell::create(&path, "t", "t").unwrap(/* test-only */);
        for i in 1..=5_u16 {
            cell.put_note(&note(i, &format!("nota {i} [[Milano]]")), "t")
                .unwrap(/* test-only */);
        }
        let head: i64 = cell
            .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
            .unwrap(/* test-only */)[0]
            .parse()
            .unwrap(/* test-only */);
        // compact up to one before the head: the last op stays replayable
        let snap = cell.compact(head - 1).unwrap(/* test-only */);
        assert!(snap.is_file(), "{}", snap.display());
        let ops = cell.ops().unwrap(/* test-only */);
        assert!(ops.iter().all(|o| o.seq > head - 1 || o.body.is_none()));
        assert!(
            ops.iter()
                .find(|o| o.seq == head)
                .is_some_and(|o| o.body.is_some())
        );
        // changes_since under the cut is compacted, from the cut is fine
        let err = cell.changes_since(head - 2).unwrap_err(/* t */);
        assert!(matches!(err, CellError::Compacted { snapshot_seq } if snapshot_seq == head - 1));
        assert!(cell.changes_since(head - 1).is_ok());
        // more writes after the compaction
        cell.put_note(&note(9, "dopo la compattazione [[Roma]]"), "t")
            .unwrap(/* test-only */);
        let report = cell.verify().unwrap(/* test-only */);
        assert!(
            report.ok(),
            "verify after compaction: {:?}",
            report.mismatch
        );
        // compacting outside the live log is refused
        assert!(cell.compact(head + 5).is_err());
        assert!(cell.compact(0).is_err());
    }

    /// v0.3 (P3): a tampered snapshot fails verification (the recorded
    /// state digest no longer matches the file).
    #[test]
    fn tampered_snapshot_fails_verify() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("c.sqlite");
        let cell = Cell::create(&path, "t", "t").unwrap(/* test-only */);
        cell.put_note(&note(1, "da compattare [[Milano]]"), "t")
            .unwrap(/* test-only */);
        let head = cell
            .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
            .unwrap(/* test-only */)[0]
            .parse::<i64>()
            .unwrap(/* test-only */);
        let snap = cell.compact(head).unwrap(/* test-only */);
        drop(cell);
        {
            let conn = Connection::open(&snap).unwrap(/* test-only */);
            conn.execute("UPDATE notes SET title = 'manomesso' WHERE id LIKE '%'", [])
                .unwrap(/* test-only */);
        }
        let cell = Cell::open(&path).unwrap(/* test-only */);
        let report = cell.verify().unwrap(/* test-only */);
        assert!(
            !report.ok() && !report.snapshot_ok,
            "a tampered snapshot must fail: {report:?}"
        );
    }

    #[test]
    fn tampering_the_kind_column_breaks_verify() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "prima"), "t")
            .unwrap(/* test-only */);
        {
            let inner = cell.lock().unwrap(/* test-only */);
            inner
                .conn
                .execute("UPDATE ops SET kind = 'confirm' WHERE seq = 2", [])
                .unwrap(/* test-only */);
        }
        let report = cell.verify().unwrap(/* test-only */);
        assert!(
            !report.ok() && report.mismatch.is_some(),
            "a tampered kind must fail verification: {report:?}"
        );
    }

    #[test]
    fn tags_text_is_the_deterministic_fts_twin() {
        let (_d, cell) = test_cell();
        let mut n = note(1, "ordinamento [[Milano]]");
        n.tags = vec!["beta".into(), "alfa".into()];
        n.body = "corpo #gamma".into();
        cell.put_note(&n, "t").unwrap(/* test-only */);
        let id = n.id.to_string();
        let tags_text = cell
            .debug_column(&format!("SELECT tags_text FROM notes WHERE id = '{id}'"))
            .unwrap(/* test-only */);
        assert_eq!(
            tags_text,
            vec!["alfa beta gamma".to_string()],
            "tags_text must be sorted and space-joined"
        );
    }

    #[test]
    fn resolve_accepts_prefix_and_suffix() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "prima [[Milano]]"), "t")
            .unwrap(/* test-only */);
        let full = format!("01{:024}", 1);
        // unique suffix (the random part — what the CLI displays)
        let resolved = cell.resolve_note_id("00000001").unwrap(/* test-only */);
        assert_eq!(resolved.to_string(), full);
        // prefix still works when unique
        assert_eq!(
            cell.resolve_note_id("0100").unwrap(/* test-only */).to_string(),
            full
        );
        // too short (the minimum is 4 characters)
        assert!(cell.resolve_note_id("001").is_err());
        // two notes sharing a suffix → ambiguity is an error
        let mut other = note(2, "seconda");
        // same random suffix, different ULID
        other.id = Ulid::from_string("01BBBBBBBBBBBBBBBB00000001")
            .unwrap(/* test-only */);
        cell.put_note(&other, "t").unwrap(/* test-only */);
        let err = cell.resolve_note_id("00000001").unwrap_err(/* test-only */);
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn debug_column_refuses_writes() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "x"), "t")
            .unwrap(/* test-only */);
        assert!(
            cell.debug_column("SELECT id FROM notes WHERE status != 'deleted'")
                .is_ok()
        );
        assert!(cell.debug_column("DELETE FROM notes").is_err());
        assert!(
            cell.debug_column("INSERT INTO meta (key, value) VALUES ('k', 'v')")
                .is_err()
        );
        // and nothing ran behind the refusal
        let n = cell
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM notes")
            .unwrap(/* test-only */);
        assert_eq!(n, vec!["1".to_string()]);
        let k = cell
            .debug_column("SELECT value FROM meta WHERE key = 'k'")
            .unwrap(/* test-only */);
        assert!(k.is_empty());
    }

    #[test]
    fn fts_index_is_really_verified() {
        let (_d, cell) = test_cell();
        cell.put_note(&note(1, "indicizzami [[Roma]]"), "t")
            .unwrap(/* test-only */);
        assert!(cell.verify().unwrap(/* test-only */).ok());
        // remove the document from the FTS index without touching the note:
        // the replay still matches, only the index is rotten
        {
            let inner = cell.lock().unwrap(/* test-only */);
            let (rowid, title, body_norm, tags_text): (i64, String, String, String) = inner
                .conn
                .query_row(
                    "SELECT n.rowid, n.title, n.body_norm, n.tags_text
                     FROM notes n JOIN notes_fts f ON f.rowid = n.rowid LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap(/* test-only */);
            inner
                .conn
                .execute(
                    "INSERT INTO notes_fts(notes_fts, rowid, title, body_norm, tags_text)
                     VALUES ('delete', ?1, ?2, ?3, ?4)",
                    rusqlite::params![rowid, title, body_norm, tags_text],
                )
                .unwrap(/* test-only */);
        }
        let report = cell.verify().unwrap(/* test-only */);
        assert!(
            !report.ok(),
            "a rotten FTS index must fail verification: {report:?}"
        );
        assert!(!report.fts_ok, "{report:?}");
    }
}
