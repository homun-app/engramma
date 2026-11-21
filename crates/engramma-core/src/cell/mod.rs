// SPDX-License-Identifier: Apache-2.0

//! The cell: one SQLite file, its change log and its derived state.
//!
//! Writes go through one process-wide lock per cell file **and** a
//! `BEGIN IMMEDIATE` transaction, so exactly one writer at a time. Readers
//! (search, get) share the same connection in WAL mode.

pub(crate) mod bin;
pub mod conflicts;
pub(crate) mod material;
pub mod ops;
pub mod policy;
pub mod proposals;
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
/// v0.6.4 (D3): how a conflict is resolved (DESIGN §4.3, D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveKeep {
    /// The original stays, the copy closes (a delete on its id).
    Original,
    /// The copy's body becomes the original's, the copy closes.
    Copy,
    /// The given body becomes the original's (the hand merge), the copy
    /// closes.
    Merged,
}

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

/// A registered markdown view (v0.4, P0.1): the folder's IDENTITY is the
/// `view_id` ULID carried in its `engramma.json`, not the path — a moved
/// folder is the same view at a new `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultView {
    /// The ULID from the view's `engramma.json`.
    pub view_id: String,
    /// The canonical absolute path, as last seen.
    pub root: String,
    /// When the view was first bound.
    pub bound_at: String,
    /// When the view was last seen (every `init`/`sync`).
    pub last_seen_at: String,
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

/// (v0.6.11, K1) One note the purge of `id` would REOPEN: the note
/// itself (its id) and its title, for the screen that warns before
/// forgetting — «tornerà valido: …».
#[derive(Debug, Clone)]
pub struct Reopening {
    /// The note that comes back to the state it had before the closing.
    pub id: Ulid,
    /// Its title, when it has one.
    pub title: Option<String>,
}

/// Report of [`Cell::purge`] (v0.3.1, P0.2): what was forgotten, and whether
/// every byte is already gone from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    /// Payload rows forgotten (net of the one the purge op itself writes).
    pub forgotten_payloads: usize,
    /// Whether the file, the WAL and the snapshots already carry none of the
    /// bytes. False means something still holds them — see [`Self::pending`].
    /// v0.6.7 (G3): `scrubbed` is EARNED, not declared — after every
    /// snapshot scrub the purge looks into the file and checks that no
    /// row of the note's family survived (notes, conflict copies, FTS,
    /// proposals); a row found means `false`, and `pending` names the
    /// snapshot. The byte-level proof stays the tests' own.
    pub scrubbed: bool,
    /// What could not be scrubbed yet. `"wal"`: a reader kept the WAL from
    /// being truncated (`PRAGMA wal_checkpoint(TRUNCATE)` answered busy);
    /// the next write and the cell close retry the checkpoint. A file
    /// name (v0.6.7, G3): that snapshot still holds rows of the family —
    /// the scrub left something behind, and the report says which file.
    pub pending: Vec<String>,
}

/// How a compaction keeps its restart point (v0.3.2, P0.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompactOptions {
    /// Keep a snapshot FILE next to the cell — the v0.3.1 behavior (the
    /// logical state, purged together with the cell). Default `false`: the
    /// compaction registers a **checkpoint** (head hash and state digest, no
    /// second copy of the cell on disk — the live cell already IS the
    /// durable state, as in rqlite, which uses the SQLite file itself as
    /// its snapshot).
    pub keep_snapshot: bool,
}

/// What one compaction did (v0.3.2, P0.1).
#[derive(Debug, Clone)]
pub struct CompactReport {
    /// The cut: payloads of ops up to this seq are forgotten.
    pub cut_seq: i64,
    /// The head at compaction time.
    pub head_seq: i64,
    /// The hash chain's head at `head_seq`.
    pub head_hash: String,
    /// Digest of the logical state at the head, after the cut.
    pub state_digest: String,
    /// The snapshot file, when `keep_snapshot` was set.
    pub snapshot: Option<PathBuf>,
    /// Cell + WAL + snapshot files, bytes on disk, before and after.
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// What `verify` could replay (v0.3.2, P0.2): a compaction without a
/// snapshot file makes the from-zero replay impossible and the tail cannot
/// replay without the state at the cut — the report says which of the three
/// worlds it checked instead of implying the full one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replay {
    /// No compaction: the whole log replays from zero.
    Full,
    /// A snapshot file exists: the tail replays on it (from its cut `seq`).
    FromSnapshot { seq: i64 },
    /// A checkpoint without a file: only the hash chain, the surviving
    /// payloads and the derived indexes could be checked, from the cut
    /// `since_seq` on. `ok()` can still be true.
    ChainOnly { since_seq: i64 },
}

/// Report of [`Cell::verify`].
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Number of ops checked.
    pub ops: usize,
    /// What could be replayed (v0.3.2, P0.2).
    pub replay: Replay,
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
    /// v0.6.10 (J2): note ids of the «phantoms» this verify FOUND — a
    /// living row on a purged id, written by the v0.6.7/v0.6.8 engines
    /// (their local write path had no purged-id guard). verify changes
    /// nothing (v0.6.9's verify repaired in place, an irreversible
    /// `secure_delete` the report only announced): the ids go here, the
    /// outcome says the cell needs repair, and [`Cell::repair`] is the
    /// step that moves each phantom's content to a new id and applies
    /// the forgetting rule to the old one. The list is empty on a
    /// healthy cell.
    pub phantom_purged: Vec<String>,
}

impl VerifyReport {
    /// Everything passed. `ChainOnly` can pass: it checked the chain, the
    /// surviving payloads and the derived indexes. A phantom is never a
    /// pass: the cell needs repair, and the repair is explicit (J2).
    pub fn ok(&self) -> bool {
        self.hash_chain_ok
            && self.replay_ok
            && self.fts_ok
            && self.snapshot_ok
            && self.phantom_purged.is_empty()
    }
}

/// v0.6.10 (J2): the «phantom» ids — a living notes row on an id the set
/// purged. verify names them; repair moves them. One query, shared.
fn phantom_ids(conn: &Connection) -> Result<Vec<Ulid>, CellError> {
    let mut purged_ids: Vec<Option<String>> = {
        let mut stmt = conn.prepare("SELECT DISTINCT note_id FROM ops WHERE kind = 'purge'")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.flatten().collect()
    };
    purged_ids.sort();
    purged_ids.dedup();
    let mut phantoms: Vec<Ulid> = Vec::new();
    for id in purged_ids.into_iter().flatten() {
        if let Ok(id) = Ulid::from_string(&id) {
            let row: i64 = conn.query_row(
                "SELECT COUNT(*) FROM notes WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )?;
            if row > 0 {
                phantoms.push(id);
            }
        }
    }
    Ok(phantoms)
}

/// What [`Cell::repair`] did (v0.6.10, J2).
#[derive(Debug, Clone, Default)]
pub struct RepairReport {
    /// old id → new id, one pair per phantom whose content moved. The
    /// map goes back to the CALLER (Homun updates its references) and is
    /// saved nowhere: the forgotten id must not stay written anywhere
    /// new.
    pub moved: Vec<(String, String)>,
}

/// One `snapshots` row as verify reads it: (seq, head_hash, state_digest,
/// file, redacted_by) — the digest and the file are nullable (v0.3.2).
type SnapshotRecord = (i64, String, Option<String>, Option<String>, String);

/// The `ChainOnly` coherence checks of a checkpoint row (v0.3.2, P0.2) —
/// everything that can still be verified without a replay:
/// - the derived (`d…`) edges of every note, re-derived from its body with
///   the same deterministic analysis that wrote them, must be exactly the
///   table's rows (manual edges cannot be re-derived — the ops that made
///   them are skeletons now — and stay out of the comparison);
/// - no vector may outlive its note (purges and tombstones delete them);
/// - when the head has not moved past the checkpoint, the digest of the
///   current state must equal the recorded one.
fn verify_checkpoint_coherence(
    conn: &Connection,
    snapshot: Option<&SnapshotRecord>,
) -> Result<(), String> {
    // 1. derived edges: the expected set re-derived from the notes, the
    //    actual set from the table. v0.6.8 (H6): the `conflitto` edge of
    //    a copy IS derived — the expected set counts it for every copy
    //    that holds a LIVING notes row (the same rule the derivation
    //    writes: seed `9012498534642822410`, reduced, answered
    //    «expected N, found N+1» on every checkpoint cell with an open
    //    copy)
    let render = |v: rusqlite::types::Value| -> String {
        match v {
            rusqlite::types::Value::Null => "\u{0}".into(),
            rusqlite::types::Value::Integer(i) => i.to_string(),
            rusqlite::types::Value::Real(f) => f.to_string(),
            rusqlite::types::Value::Text(t) => t,
            rusqlite::types::Value::Blob(b) => format!("{b:x?}"),
        }
    };
    let mut expected: Vec<String> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, frontmatter_json, body FROM notes ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows: Vec<Result<(String, String, String), _>> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?
            .collect();
        for row in rows {
            let (id, frontmatter, body) = row.map_err(|e| e.to_string())?;
            let note = serde_json::from_str::<Value>(&frontmatter)
                .ok()
                .and_then(|v| Note::from_json(&v).ok())
                .map(|mut n| {
                    n.body = body;
                    n
                });
            let Some(note) = note else {
                return Err(format!("note {id}: unreadable frontmatter"));
            };
            let note_id = note.id.to_string();
            for e in ops::derived_edges_of(&note).map_err(|e| e.to_string())? {
                // NULL renders as \0, like `render` below (state_dump's rule)
                let null = "\u{0}".to_string();
                // `render` puts the separator after every column, last one
                // included — expected must end the same way (the trailing
                // \u{1} in the format, not an `expect` on the pushed line)
                expected.push(format!(
                    "{note_id}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}",
                    e.id,
                    e.src,
                    e.rel,
                    e.dst,
                    e.valid_from.unwrap_or_else(|| null.clone()),
                    e.valid_to.unwrap_or(null),
                    e.confidence,
                    e.status,
                ));
            }
        }
        // the `conflitto` edges: one per copy with a LIVING notes row,
        // with the derivation's own deterministic id
        let null = "\u{0}".to_string();
        let mut stmt = conn
            .prepare(
                "SELECT cc.copy_id, cc.original_id FROM conflict_copies cc
                 JOIN notes n ON n.id = cc.copy_id
                 WHERE n.status != 'deleted' ORDER BY cc.copy_id",
            )
            .map_err(|e| e.to_string())?;
        let rows: Vec<Result<(String, String), _>> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect();
        for row in rows {
            let (copy, original) = row.map_err(|e| e.to_string())?;
            let edge_id = material::conflict_edge_id(
                &Ulid::from_string(&copy).map_err(|e| format!("bad copy id {copy}: {e}"))?,
            );
            expected.push(format!(
                "{copy}\u{1}{edge_id}\u{1}note:{copy}\u{1}conflitto\u{1}note:{original}\
                 \u{1}{null}\u{1}{null}\u{1}stated\u{1}active\u{1}",
            ));
        }
    }
    let mut actual: Vec<String> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT note_id, id, src, rel, dst, valid_from, valid_to, confidence, status
                 FROM edges WHERE id LIKE 'd%' ORDER BY note_id, id",
            )
            .map_err(|e| e.to_string())?;
        let ncols = stmt.column_count();
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut line = String::new();
            for i in 0..ncols {
                let v: rusqlite::types::Value = row.get(i).map_err(|e| e.to_string())?;
                line.push_str(&render(v));
                line.push('\u{1}');
            }
            actual.push(line);
        }
    }
    expected.sort_unstable();
    actual.sort_unstable();
    if expected != actual {
        let first = expected
            .iter()
            .zip(actual.iter())
            .find(|(a, b)| a != b)
            .map(|(a, _)| a.clone())
            .unwrap_or_default();
        return Err(format!(
            "the derived edges diverge from the notes (around {first}; \
             expected {}, found {})",
            expected.len(),
            actual.len()
        ));
    }
    // 2. vectors: derived data, but they may not outlive their note
    let dead: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM note_vectors
             WHERE note_id NOT IN (SELECT id FROM notes WHERE status != 'deleted')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if dead > 0 {
        return Err(format!(
            "{dead} vector row(s) belong to a note that is no longer living"
        ));
    }
    // 3. the digest at the head, when the head has not moved
    let Some((cut, head_hash, digest, _file, _redacted)) = snapshot else {
        return Ok(());
    };
    let live_head: Option<String> = conn
        .query_row("SELECT hash FROM ops ORDER BY seq DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .ok();
    if live_head.as_deref() != Some(head_hash.as_str()) {
        return Ok(()); // the head moved: the recorded digest is history
    }
    let Some(digest) = digest else {
        return Ok(()); // redacted by a purge: nothing left to compare
    };
    match state_digest(conn) {
        Ok(live) if live == *digest => Ok(()),
        Ok(_) => Err(format!(
            "the state digest moved from the checkpoint at seq {cut}"
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// Run the FTS5 integrity-check on both text indices (v0.6, P0): each must
/// match what its side of the status split in the content table implies
/// (every FTS column has a twin in `notes` since schema v3).
fn fts_integrity_check(conn: &Connection) -> Result<(), CellError> {
    conn.execute_batch(
        "INSERT INTO notes_fts(notes_fts) VALUES('integrity-check');
         INSERT INTO notes_fts_hidden(notes_fts_hidden) VALUES('integrity-check');",
    )?;
    // SQLite 3.54's integrity-check verifies the index structure but does
    // NOT compare an external-content index with its content table (checked
    // by hand), so compare the indexed rowid sets ourselves: the base index
    // holds exactly the visible notes, the hidden one exactly the
    // quarantined notes, tombstones neither.
    let diverged: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM (SELECT rowid AS id FROM notes
                                        WHERE status IN ('active', 'superseded')
                                        EXCEPT SELECT id FROM notes_fts_docsize))
             + (SELECT COUNT(*) FROM (SELECT id FROM notes_fts_docsize
                                      EXCEPT SELECT rowid FROM notes
                                      WHERE status IN ('active', 'superseded')))
             + (SELECT COUNT(*) FROM (SELECT rowid AS id FROM notes WHERE status = 'quarantined'
                                      EXCEPT SELECT id FROM notes_fts_hidden_docsize))
             + (SELECT COUNT(*) FROM (SELECT id FROM notes_fts_hidden_docsize
                                      EXCEPT SELECT rowid FROM notes
                                      WHERE status = 'quarantined'))",
        [],
        |r| r.get(0),
    )?;
    if diverged > 0 {
        return Err(CellError::Db(format!(
            "the text indices hold {diverged} rowid(s) more or fewer than the notes their status routes them to"
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

/// The stemming-language histogram of the visible notes (v0.5.3, P0) and its
/// freshness bookkeeping, same pattern as the graph: the languages the
/// `body_norm` of the living notes was actually stemmed in, most frequent
/// first, so that an uncertain (short) question expands its stems over the
/// languages the cell really speaks instead of trusting one guess.
pub(crate) struct LangHistState {
    /// `PRAGMA data_version` at build time: another process writing the cell
    /// bumps it, and the next uncertain question rebuilds.
    pub(crate) data_version: i64,
    /// `(language, notes)` sorted by count desc, then by wire name.
    pub(crate) langs: Vec<(crate::format::Lang, usize)>,
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

/// v0.6.3 (C5.3): the write-time signer gate — asked before every local
/// write, it answers with the signer to (re)arm or `None` when nothing
/// changed. The host that knows when sharing may have moved under an open
/// cell (the Node binding: `devices.db` changed) installs one; a cell
/// without a gate pays nothing.
pub type SignerGate =
    std::sync::Arc<dyn Fn() -> Option<std::sync::Arc<dyn ops::Signer>> + Send + Sync>;

pub(crate) struct Inner {
    pub(crate) conn: Connection,
    clock: HlcClock,
    /// The sync era's signer (v0.6, P2): signs every LOCAL operation with
    /// the device key. `None` outside the sync era.
    pub(crate) signer: Option<std::sync::Arc<dyn ops::Signer>>,
    /// v0.6.3 (C5.3): asked before every LOCAL write — the host that
    /// knows when sharing may have changed under an open cell answers
    /// with the signer to (re)arm, or `None` when nothing moved. A cell
    /// without a gate pays nothing.
    pub(crate) signer_gate: Option<SignerGate>,
    /// Lazily built on the first semantic search (opening a cell stays ≤ 50 ms).
    pub(crate) index: Option<IndexState>,
    /// Lazily built on the first graph walk (same reason).
    pub(crate) graph: Option<GraphState>,
    /// Own writes since the graph index was last used.
    pub(crate) graph_dirt: GraphDirt,
    /// The language histogram of the visible notes (v0.5.3, P0), lazily
    /// built by the first UNCERTAIN question.
    pub(crate) lang_hist: Option<LangHistState>,
    /// Own writes since the histogram was last built.
    pub(crate) lang_hist_dirty: bool,
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
    /// Test-only determinism (v0.6.1, P0): when set, every LOCAL mint — an
    /// op id, a payload salt, an edge id — comes from here instead of the
    /// process CSPRNG. The convergence simulator injects one so a saved
    /// seed writes a byte-for-byte reproducible op set (with the injected
    /// clock; random ULIDs were half of why a failing seed did not
    /// reproduce).
    pub(crate) minter: Option<Minter>,
}

/// The injectable source of op ids and payload salts (tests only).
pub type Minter = std::sync::Arc<dyn Fn() -> Result<(Ulid, [u8; 16]), CellError> + Send + Sync>;

/// Mint one id and one salt: the injected minter's when there is one, the
/// process CSPRNG otherwise.
fn mint_ids(inner: &Inner) -> Result<(Ulid, [u8; 16]), CellError> {
    match &inner.minter {
        Some(mint) => mint(),
        None => Ok((crate::id::new_ulid()?, crate::id::random_salt()?)),
    }
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

/// v0.4.3 (P0.1): the ONE recognizer of a compaction snapshot file. Only
/// the exact format `compact` writes — `<cell>-snap-<digits>.sqlite`
/// (`{}-snap-{head_seq}.sqlite`) — is a snapshot **of `<cell>`**: a
/// neighboring CELL called `storia-snap-archivio` is not (the suffix is
/// not all digits), and neither is a `-snap-` in the middle of a longer
/// name. The three places that used to agree on a loose prefix match (the
/// engine binding's `dropCell`, the CLI's `cell drop` and this module's
/// [`Cell`] footprint) all ask this predicate now, so a whole-cell
/// forgetting can only ever take the files of THAT cell.
pub fn snapshot_of(file_name: &str) -> Option<(&str, u64)> {
    let stem = file_name.strip_suffix(".sqlite")?;
    let split = stem.rfind("-snap-")?;
    let (cell, rest) = stem.split_at(split);
    let seq = rest.strip_prefix("-snap-")?;
    if cell.is_empty() || seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((cell, seq.parse().ok()?))
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

/// v0.5 (P1): one-time adjustment of the compaction rows the schema-v13
/// digest widening made stale — the recorded digests were computed WITHOUT
/// the queue and the registry tables, so they can never match the new
/// definition. A file snapshot is migrated in place (the empty tables land
/// in it, as in the live cell) and re-digested under the new definition; a
/// checkpoint (no file) has its digest redacted to NULL, exactly like a
/// purge redaction: unverifiable, therefore not kept. Idempotent and
/// crash-safe: a bookmark in `meta` marks the pass, and every step is a
/// no-op the second time.
fn upgrade_snapshot_digests(conn: &mut Connection, cell_path: &Path) -> Result<(), CellError> {
    let done: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'dump_v13'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if done.is_some() {
        return Ok(());
    }
    let rows: Vec<(i64, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT seq, file FROM snapshots ORDER BY seq")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .flatten()
            .collect()
    };
    for (seq, file) in rows {
        let Some(file) = file else {
            // a checkpoint: its digest described a state under the old
            // definition — nothing to re-read, nothing to keep
            conn.execute(
                "UPDATE snapshots SET state_digest = NULL WHERE seq = ?1",
                [seq],
            )?;
            continue;
        };
        let path = cell_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(&file);
        if !path.exists() {
            continue; // a ghost row: the purge owns its cleanup, an open must not fail
        }
        let mut snap = Connection::open(&path)?;
        snap.busy_timeout(std::time::Duration::from_secs(10))?;
        {
            let tx = snap.transaction_with_behavior(TransactionBehavior::Immediate)?;
            schema::migrate(&tx)?;
            tx.commit()?;
        }
        let digest = state_digest(&snap)?;
        conn.execute(
            "UPDATE snapshots SET state_digest = ?2 WHERE seq = ?1",
            rusqlite::params![seq, digest],
        )?;
    }
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('dump_v13', '1')
         ON CONFLICT(key) DO UPDATE SET value = '1'",
        [],
    )?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
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

    /// Open (or create) the file of a COPY without a history of its own
    /// (v0.6, P1): the schema is applied and the cell waits for its first
    /// INGESTED operation — the shared `init` of the cell it is about to
    /// become a copy of. This is how a device joins a shared cell: no
    /// second `init`, no identity of its own to reconcile.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL or I/O failure.
    pub fn open_bare(path: &Path) -> Result<Cell, CellError> {
        Self::open_raw(path, false)
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

    /// v0.4.3 (P1.2): close this cell's real connection NOW, not when the
    /// last clone goes out of scope. [`Cell`] is cheap to clone and every
    /// clone shares one [`Inner`]: a caller that keeps a clone alive (a JS
    /// object waiting for its GC) would keep the database, its WAL and its
    /// shm open for hours after a whole-cell drop unlinked them. `retire`
    /// swaps in an in-memory placeholder and closes the real connection —
    /// every clone sees the placeholder, and the descriptors go back to the
    /// OS. The drop paths call it between their seal and the file removal;
    /// a retired cell is NOT usable (the placeholder has no tables), which
    /// is the point: nothing but a doomed generation ever reaches for it.
    ///
    /// # Errors
    /// [`CellError::Db`] when the placeholder or the close fails.
    pub fn retire(&self) -> Result<(), CellError> {
        let placeholder = Connection::open_in_memory()?;
        let mut inner = self.lock()?;
        let gone = std::mem::replace(&mut inner.conn, placeholder);
        // the caches go with the connection: the vectors, the graph and the
        // query embeddings of a cell whose files are about to be unlinked,
        // and the pending-scrub bookkeeping (those WAL frames go too)
        inner.index = None;
        inner.graph = None;
        inner.graph_dirt = GraphDirt::default();
        inner.lang_hist = None;
        inner.lang_hist_dirty = false;
        inner.query_cache.clear();
        inner.pending_scrub.clear();
        drop(inner);
        drop(gone); // the real close: SQLite checkpoints and lets the WAL go
        Ok(())
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
        // v0.5 (P1): the state digest widened with the queue and the
        // registry (schema v13). Compaction rows recorded under the OLD
        // definition are adjusted once, here — before any verify can compare
        // against them.
        {
            let cell_lock = per_cell_lock(path);
            let _guard = cell_lock
                .lock()
                .map_err(|_| CellError::Db("cell lock registry poisoned".into()))?;
            upgrade_snapshot_digests(&mut conn, path)?;
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
                signer: None,
                signer_gate: None,
                index: None,
                graph: None,
                graph_dirt: GraphDirt::default(),
                lang_hist: None,
                lang_hist_dirty: false,
                query_cache: std::collections::HashMap::new(),
                query_cache_cap: crate::graph::QUERY_CACHE_CAPACITY,
                pending_scrub: Vec::new(),
                minter: None,
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

    /// This cell's `PRAGMA data_version` — the counter that moves when
    /// ANOTHER connection commits to the file (v0.6.1, C4). The serve
    /// loop's write trigger: read it on a PERSISTENT connection (the
    /// counter is per-connection) and cross-process writes appear.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn data_version(&self) -> Result<i64, CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .map_err(CellError::from)
    }

    /// The ops log's own marker `(count, max rowid)` (v0.6.1-ciclo, K1):
    /// `data_version` moves for ANY commit, this moves only when
    /// OPERATIONS arrive — the serve loop's cheap poll stays on the file,
    /// but the pass is armed by the log, never by the file alone (an
    /// idle session's bookkeeping must not re-light the cycle).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn ops_marker(&self) -> Result<(i64, i64), CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(rowid), 0) FROM ops",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(CellError::from)
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

    /// Does the log already hold an operation with this `content_hash`?
    /// The arrival gate's duplicate check — a point query over the v20
    /// index, never a walk of the log.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn holds_content_hash(&self, content_hash: &str) -> Result<bool, CellError> {
        let inner = self.lock()?;
        let n: i64 = inner.conn.query_row(
            "SELECT COUNT(*) FROM ops WHERE content_hash = ?1",
            [content_hash],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// The operation of one `content_hash` (the wire identity), when this
    /// copy holds it — a point query over the v20 index, never a walk of
    /// the log (the envelope road asks it once per missing id).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure; [`CellError::Invalid`] on a
    /// malformed row.
    pub fn op_of_hash(&self, content_hash: &str) -> Result<Option<ops::Op>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT o.seq, o.op_id, o.hlc, o.actor, o.kind, o.note_id, o.base, o.content_hash,
                    o.payload_digest, o.payload_state, p.salt, p.body, o.prev_hash, o.hash,
                    o.device, o.sig
             FROM ops o LEFT JOIN op_payloads p ON p.op_id = o.op_id
             WHERE o.content_hash = ?1",
        )?;
        let mut rows = stmt.query([content_hash])?;
        let Some(row) = rows.next().map_err(CellError::from)? else {
            return Ok(None);
        };
        Ok(Some(ops::op_from_row(ops::OpRow {
            seq: row.get(0)?,
            op_id: &row.get::<_, String>(1)?,
            hlc: &row.get::<_, String>(2)?,
            actor: &row.get::<_, String>(3)?,
            kind: &row.get::<_, String>(4)?,
            note_id: row.get::<_, Option<String>>(5)?.as_deref(),
            base: row.get::<_, Option<String>>(6)?.as_deref(),
            content_hash: &row.get::<_, String>(7)?,
            payload_digest: &row.get::<_, String>(8)?,
            payload_state: &row.get::<_, String>(9)?,
            salt: row.get::<_, Option<Vec<u8>>>(10)?.as_deref(),
            body_json: row.get::<_, Option<String>>(11)?.as_deref(),
            prev_hash: &row.get::<_, String>(12)?,
            hash: &row.get::<_, String>(13)?,
            device: row.get::<_, Option<String>>(14)?.as_deref(),
            sig: row.get::<_, Option<String>>(15)?.as_deref(),
        })?))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, CellError> {
        self.inner
            .lock()
            .map_err(|_| CellError::Db("cell mutex poisoned".into()))
    }

    /// Arm the sync era's signer (v0.6, P2): every LOCAL operation from
    /// here on is signed with the device key and carries the device id.
    ///
    /// # Errors
    /// [`CellError::Db`] when the cell lock is poisoned.
    pub fn set_signer(&self, signer: std::sync::Arc<dyn ops::Signer>) -> Result<(), CellError> {
        let mut inner = self.lock()?;
        inner.signer = Some(signer);
        Ok(())
    }

    /// Install the write-time signer gate (v0.6.3, C5.3): `execute` asks
    /// it before every local write, so a cell that was open BEFORE the
    /// root started sharing signs its first write after the change — the
    /// arm is decided at the WRITE, not at the open.
    pub fn set_signer_gate(&self, gate: SignerGate) -> Result<(), CellError> {
        let mut inner = self.lock()?;
        inner.signer_gate = Some(gate);
        Ok(())
    }

    /// Replace the HLC clock — tests inject skewed wall clocks to play
    /// multiple processes on the same file, and the convergence simulator
    /// fixes time so a failing case reproduces byte for byte (the HLCs
    /// embed the wall clock: without this, the same seed writes a different
    /// op set on every run, and a divergence hides between runs).
    pub fn inject_clock(&self, clock: HlcClock) -> Result<(), CellError> {
        let mut inner = self.lock()?;
        inner.clock = clock;
        Ok(())
    }

    /// Replace the id/salt mint (tests only, v0.6.1, P0): the injected
    /// minter turns every LOCAL mint deterministic, which — with the
    /// injected clock — makes a saved proptest seed write the SAME op set,
    /// byte for byte, on every rerun. See [`Cell::inject_clock`] for the
    /// other half.
    pub fn inject_minter(&self, minter: Minter) -> Result<(), CellError> {
        let mut inner = self.lock()?;
        inner.minter = Some(minter);
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
        // v0.6.3 (C5.3): the gate answers before the write — under the
        // write lock, so the arm and the write are one step. A cell with
        // no gate (the engines that never sync) pays a null check.
        if let Some(gate) = inner.signer_gate.clone()
            && let Some(signer) = gate()
        {
            inner.signer = Some(signer);
        }
        execute_op(&mut inner, kind, note_id, actor, payload)?;
        Ok(())
    }

    /// An operation arriving from another copy of this cell (v0.6, P1).
    /// Idempotent by `op_id`: an operation already held changes nothing and
    /// answers `false`. The operation keeps the identity it was created
    /// with (`content_hash`, `base`) — the receiver recomputes both from
    /// the content and refuses a row that claims to be what it is not — and
    /// is sealed into THIS copy's local chain of arrival (`prev_hash`,
    /// `hash`, `seq`), which stays what it always was: the audit trail of
    /// one disk. A physical time too far in the future is refused at the
    /// door (the threat model's 5-minute drift cap: no device can write
    /// itself "into the future" and win every conflict).
    ///
    /// # Errors
    /// [`CellError::Invalid`] on a forged identity, a body that does not
    /// digest to its salted digest, or a clock beyond the drift cap.
    pub fn ingest_op(&self, op: &Op) -> Result<bool, CellError> {
        const DRIFT_CAP_MS: u64 = 5 * 60 * 1000;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let mut inner = self.lock()?;
        let wall_ms = inner.clock.wall_ms();
        let tx = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seen: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ops WHERE op_id = ?1",
            [op.op_id.to_string()],
            |r| r.get(0),
        )?;
        if seen > 0 {
            // idempotent: nothing to store, nothing to apply
            tx.rollback()?;
            return Ok(false);
        }
        let expect = ops::content_hash(
            &op.op_id,
            &op.hlc,
            &op.actor,
            op.kind,
            op.note_id,
            op.base.as_deref(),
            &op.payload_digest,
        );
        if expect != op.content_hash {
            return Err(CellError::Invalid(format!(
                "op {}: content_hash disagrees with its content",
                op.op_id
            )));
        }
        // v0.6.1 (P0): the identity covers the ORIGINAL payload digest,
        // whatever state the sender's copy holds the payload in. A `whole`
        // body must still digest to it; a `censored` body is checked
        // against the censor's own deterministic rule (shape and salt) —
        // the identity no longer covers it, the shape bounds what it may
        // say; a `redacted` op carries no body at all.
        match op.payload_state {
            ops::PayloadState::Whole => match (&op.salt, &op.body) {
                (Some(salt), Some(body)) => {
                    if ops::payload_digest(salt, body) != op.payload_digest {
                        return Err(CellError::Invalid(format!(
                            "op {}: the body does not digest to its salted digest",
                            op.op_id
                        )));
                    }
                }
                _ => {
                    return Err(CellError::Invalid(format!(
                        "op {}: a whole operation must carry its body and salt",
                        op.op_id
                    )));
                }
            },
            ops::PayloadState::Redacted => {
                if op.salt.is_some() || op.body.is_some() {
                    return Err(CellError::Invalid(format!(
                        "op {}: a redacted operation carries no body",
                        op.op_id
                    )));
                }
            }
            ops::PayloadState::Censored => {
                let salt_ok = op
                    .salt
                    .as_deref()
                    .is_some_and(|s| s == ops::censor_salt(&op.op_id.to_string()));
                if !salt_ok || !ops::censor_shape_ok(op) {
                    return Err(CellError::Invalid(format!(
                        "op {}: the censored payload is not what the censor's rule produces",
                        op.op_id
                    )));
                }
            }
        }
        if op.hlc.physical_ms > wall_ms.saturating_add(DRIFT_CAP_MS) {
            return Err(CellError::Invalid(format!(
                "op {}: physical time {} ms is beyond the +5 min drift cap",
                op.op_id, op.hlc.physical_ms
            )));
        }
        // the local chain of arrival takes the op as it comes
        let (prev_seq, prev_hash): (i64, String) = tx
            .query_row(
                "SELECT seq, hash FROM ops ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, ops::GENESIS_HASH.to_string()));
        // `apply` writes the op's own HLC into `meta`; the cell clock must
        // hold the MAXIMUM ever seen, never go backwards
        let logged: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = 'hlc'", [], |r| r.get(0))
            .optional()?;
        let mut arrival = op.clone();
        arrival.seq = prev_seq + 1;
        arrival.seal(&prev_hash);
        // v0.6 (P1): the purge dominates — an operation of a note this copy
        // knows is purged never rests here in the clear, and a purge that
        // arrives scrubs what earlier arrivals left. Both run inside THIS
        // transaction, wrapped in secure deletion: the freed pages are
        // zeroed as they free, and the plaintext never reaches the WAL.
        let purge_arrival = op.kind == OpKind::Purge;
        let mut scrubbed_notes: Vec<Ulid> = Vec::new();
        if !purge_arrival {
            for object in ops::objects_of(&arrival) {
                if let Some(nid) = object.strip_prefix("note:")
                    && let Ok(purged) = Ulid::from_string(nid)
                    && material::note_purged_in_set(&tx, &purged)?
                {
                    scrubbed_notes.push(purged);
                }
            }
        }
        let scrubbing = purge_arrival || !scrubbed_notes.is_empty();
        let secure_before: Option<i64> = if scrubbing {
            let before: i64 = tx.query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
            tx.pragma_update(None, "secure_delete", "ON")?;
            Some(before)
        } else {
            None
        };
        // v0.6.6 (F3; v0.6.7, G1): the arriving purge optimizes the hidden
        // index only when THIS copy holds quarantined rows of the family
        // — the same before-the-op count of the local purge (the arrival
        // applies the same tombstones a local purge would) — or when the
        // dirty bit says rows have LEFT the index before now
        let hidden_touched = if purge_arrival {
            match arrival.note_id {
                Some(purged) => {
                    ops::hidden_dirty(&tx)? || ops::hidden_rows_in_family(&tx, &purged)? > 0
                }
                None => false,
            }
        } else {
            false
        };
        // v0.6.7 (G3): the arriving purge scrubs THIS copy's snapshots
        // too — the family names, read before the op applies
        let mut arrival_family: Vec<String> = Vec::new();
        if purge_arrival && let Some(purged) = arrival.note_id {
            arrival_family = ops::family_of(&tx, &purged)?;
        }
        material::arrive(&tx, &arrival)?;
        for purged in &scrubbed_notes {
            ops::scrub_log(&tx, purged)?;
        }
        if let Some(s) = logged
            && let Ok(prev) = Hlc::parse(&s)
            && prev > arrival.hlc
        {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('hlc', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = ?1",
                [s],
            )?;
        }
        tx.commit()?;
        inner.clock.observe(&arrival.hlc);
        // a purge that ARRIVED carries the disk hygiene of a local one: the
        // bodies it scrubs may sit in earlier WAL frames and freed pages of
        // this copy, and the right to be forgotten reaches them (secure
        // deletion around the scrub, an FTS optimize, a VACUUM, a
        // checkpoint — the same dance, the same guarantees)
        if scrubbing && let Some(before) = secure_before {
            inner.conn.pragma_update(None, "secure_delete", before)?;
        }
        if purge_arrival {
            inner
                .conn
                .execute_batch("INSERT INTO notes_fts(notes_fts) VALUES('optimize');")?;
            if hidden_touched {
                ops::hidden_optimize(&inner.conn)?;
            }
            inner.conn.execute_batch("VACUUM;")?;
            // v0.6.7 (G3): every snapshot THIS copy registered is a full
            // copy made before the purge arrived — the same scrub the
            // local purge runs on its own files, closure and digest
            // included (found by the two-device forgetting property on
            // its first run: the receiving device's snapshot kept the
            // family while the live file was clean)
            let left = Self::scrub_snapshots_of(
                &mut inner,
                &self.path,
                arrival.note_id,
                &arrival_family,
                &arrival.op_id,
            )?;
            if !left.is_empty() {
                // an arrival that cannot complete its oblivion does not
                // land: the op stays un-ingested and the next sync
                // retries it
                return Err(CellError::Db(format!(
                    "the arriving purge left rows in {} snapshot(s): {:?}",
                    left.len(),
                    left
                )));
            }
            if !wal_checkpoint(&inner.conn).unwrap_or(false) {
                inner.pending_scrub = vec!["wal"];
            }
        }
        // the derived indexes follow, as for a local write
        inner.lang_hist_dirty = true;
        match arrival.kind {
            OpKind::Link | OpKind::Unlink | OpKind::Purge => inner.graph_dirt.global = true,
            _ => {
                if let Some(id) = arrival.note_id {
                    inner.graph_dirt.notes.push(id);
                }
            }
        }
        Ok(true)
    }

    /// Scrub one compaction snapshot file in place (v0.3.1, P0.3): the same
    /// state purge and log scrub the live cell went through, an FTS
    /// `optimize`, a `VACUUM` — and back comes the snapshot's new logical
    /// digest. The snapshot's log must stay identical to the live log's
    /// prefix, so the censor mirrors what the live scrub did (`censorable`).
    fn scrub_snapshot(
        path: &Path,
        id: &Ulid,
        censorable: &std::collections::HashSet<String>,
    ) -> Result<String, CellError> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        // a stray -wal next to a snapshot would survive the file's VACUUM:
        // fold it in and stay in rollback-journal mode
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        // v0.6.6 (F3; v0.6.7, G1): does this snapshot's HIDDEN index
        // need its merge? The current compact writes both indexes EMPTY
        // by construction (`delete-all` at compact, rebuilt only in the
        // restore's copy); a v0.3.1 snapshot with rows keeps the dance.
        // Counted before the scrub, like the live purge's own count —
        // and joined by the DIRTY BIT: a row that left this snapshot's
        // hidden index in an earlier scrub left its stems in the old
        // segments, and the count of today cannot see it (the same gate
        // as the live purge). The base index merges unconditionally,
        // exactly as the live purge runs it.
        let hidden_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM notes_fts_hidden_docsize", [], |r| {
                r.get(0)
            })?;
        let hidden_dirty = ops::hidden_dirty(&conn)?;
        let secure_before: i64 = conn.query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
        conn.pragma_update(None, "secure_delete", "ON")?;
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ops::purge_state(&tx, id)?;
            ops::scrub_log_snapshot(&tx, id, censorable)?;
            // v0.6.7 (G3): the SAME transitive closure the live purge
            // walks — the copies are the note's own derivation, and the
            // right to be forgotten reaches everything its text
            // produced, in the frozen file no less than in the live
            // one. (The one test-only injection of the module: the env
            // var below fabricates a HALF-DONE scrub, the case the
            // earned `scrubbed` exists to catch — it can never make a
            // purge lie, only make it say the truth.)
            if std::env::var("ENGRAMMA_TEST_SKIP_SNAPSHOT_COPY_SCRUB").is_err() {
                material::purge_conflict_copies(&tx, id)?;
            }
            tx.commit()?;
        }
        // the two indexes join the dance exactly as the live purge runs
        // it: the base one always (its rows carried the stems), the
        // hidden one when it holds rows or its dirty bit is lit
        conn.execute_batch("INSERT INTO notes_fts(notes_fts) VALUES('optimize');")?;
        if hidden_rows > 0 || hidden_dirty {
            ops::hidden_optimize(&conn)?;
        }
        conn.pragma_update(None, "secure_delete", secure_before)?;
        conn.execute_batch("VACUUM;")?;
        state_digest(&conn)
    }

    /// (v0.6.7, G3) Scrub every snapshot THIS cell has registered, for one
    /// purged note: the shared body of the local purge and of a purge that
    /// ARRIVES (a copy's snapshots are full copies made before the purge
    /// reached it — the right to be forgotten crosses devices). Returns
    /// the files that still hold rows of the family after their scrub:
    /// the callers turn them into their own honest answer (`pending` for
    /// the local report, an error for the arrival — an op that cannot
    /// complete its oblivion does not land half-done).
    fn scrub_snapshots_of(
        inner: &mut Inner,
        cell_path: &Path,
        id: Option<Ulid>,
        family: &[String],
        purge_op_id: &Ulid,
    ) -> Result<Vec<String>, CellError> {
        // `censorable`: the supersede payloads the LIVE log still carries
        // after its own scrub — only those are censored inside the
        // snapshot; the ones the compaction cut already forgot are merely
        // deleted there, or the snapshot's ops rows would fork from the
        // live prefix.
        let censorable: std::collections::HashSet<String> = {
            let mut stmt = inner.conn.prepare(
                "SELECT o.op_id FROM ops o JOIN op_payloads p ON p.op_id = o.op_id
                 WHERE o.kind IN ('supersede', 'decide', 'revert_decision')",
            )?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .flatten()
                .collect()
        };
        let rows: Vec<(i64, Option<String>)> = {
            let mut stmt = inner
                .conn
                .prepare("SELECT seq, file FROM snapshots ORDER BY seq DESC")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .flatten()
                .collect()
        };
        let mut left_behind: Vec<String> = Vec::new();
        for (i, (seq, file)) in rows.into_iter().enumerate() {
            // v0.3.2 (P0.3): a checkpoint row has no file to scrub, but its
            // digest described a state that contained the purged note. It is
            // an hash of the whole state — not readable data — yet from here
            // on it is unverifiable and there is no reason to keep it: NULL,
            // with this purge's op_id in `redacted_by`.
            let Some(file) = file else {
                let redacted = Self::redacted_by(&inner.conn, seq, purge_op_id)?;
                inner.conn.execute(
                    "UPDATE snapshots SET state_digest = NULL, redacted_by = ?2
                     WHERE seq = ?1",
                    rusqlite::params![seq, redacted],
                )?;
                continue;
            };
            let snap_path = cell_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(&file);
            if !snap_path.exists() {
                if i > 0 {
                    // a ghost row (crash between the row's removal and the
                    // file's in a past compaction — the newest always exists):
                    // drop it, nothing references it
                    inner
                        .conn
                        .execute("DELETE FROM snapshots WHERE seq = ?1", [seq])?;
                    continue;
                }
                return Err(CellError::Db(format!(
                    "the newest snapshot {} is missing: the purge refuses to forget half of the cell",
                    snap_path.display()
                )));
            }
            let Some(id) = id else {
                continue;
            };
            // v0.6.8 (H3): the family's rowids, read BEFORE the scrub —
            // the FTS rows are counted BY ROWID, never through a join
            // with `notes` (the most likely half-scrub takes the notes
            // row and leaves the index row: a JOIN sees nothing). A
            // file this process cannot even READ is not one the purge
            // may call clean: it is named and left behind, and the scrub
            // does not run half-blind against it.
            let rowids = Self::family_rowids_readonly(&snap_path, family);
            let Ok(rowids) = rowids else {
                if !left_behind.contains(&file) {
                    left_behind.push(file);
                }
                continue;
            };
            let digest = Self::scrub_snapshot(&snap_path, &id, &censorable)?;
            let redacted = Self::redacted_by(&inner.conn, seq, purge_op_id)?;
            inner.conn.execute(
                "UPDATE snapshots SET state_digest = ?2, redacted_by = ?3 WHERE seq = ?1",
                rusqlite::params![seq, digest, redacted],
            )?;
            // v0.6.7 (G3): `scrubbed` is earned, not declared. The scrub
            // above SAYS it took the family out of this file; the purge
            // looks into the file and counts what is left — a note row,
            // a derivation row, an FTS row, a queue row of the family
            // means the scrub left something behind. A row-level check:
            // the bytes stay the tests' business.
            // v0.6.8 (H3): every answer but zero counts — an unopenable
            // file answers `-1`, and `-1` is NOT clean — and the FTS
            // rows are counted by the rowids read before, so an ORPHAN
            // (its notes row gone, its index row left) is seen too.
            if Self::snapshot_holds_family(&snap_path, family, &rowids)? != 0
                && !left_behind.contains(&file)
            {
                left_behind.push(file);
            }
        }
        Ok(left_behind)
    }

    /// (v0.6.8, H3) The `notes` rowids of the family in a file opened
    /// read-only — the numbers the FTS rows are counted by, taken BEFORE
    /// the scrub removes the rows they name.
    fn family_rowids_readonly(path: &Path, family: &[String]) -> Result<Vec<i64>, CellError> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        Self::family_rowids(&conn, family)
    }

    /// (v0.6.8, H3) The `notes` rowids of the family, on any connection.
    fn family_rowids(conn: &Connection, family: &[String]) -> Result<Vec<i64>, CellError> {
        let placeholders = vec!["?"; family.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT rowid FROM notes WHERE id IN ({placeholders}) ORDER BY rowid"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(family), |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// (v0.6.7, G3; v0.6.8, H3) How many rows of the family a file still
    /// holds: notes, conflict copies, FTS rows of the family — counted
    /// BY ROWID, the numbers read before the scrub, so an orphan (its
    /// notes row gone, its index row left) is seen — every ORPHAN
    /// docsize row of the file, whatever family it belongs to, and the
    /// queue rows. ONE function for the two files: the snapshots and the
    /// live cell speak the same count. Read-only on the file it judges;
    /// an unopenable file answers `-1`: a file the purge cannot READ is
    /// not one it may call clean, and `-1 != 0`.
    fn snapshot_holds_family(
        path: &Path,
        family: &[String],
        rowids: &[i64],
    ) -> Result<i64, CellError> {
        let Ok(conn) =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            return Ok(-1);
        };
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        Self::holds_family(&conn, family, rowids)
    }

    /// (v0.6.8, H3) The count itself, on any connection — the live
    /// purge's own check and every snapshot's run the same words.
    fn holds_family(
        conn: &Connection,
        family: &[String],
        rowids: &[i64],
    ) -> Result<i64, CellError> {
        let placeholders = vec!["?"; family.len()].join(",");
        // the FTS rows by rowid: empty only when the family had no notes
        // row here to begin with (a double purge) — then there is no
        // rowid to count, and the orphan arms below still speak
        let rowid_arms = if rowids.is_empty() {
            "0".to_string()
        } else {
            let rid = vec!["?"; rowids.len()].join(",");
            format!(
                "(SELECT COUNT(*) FROM notes_fts_docsize WHERE id IN ({rid}))
                 + (SELECT COUNT(*) FROM notes_fts_hidden_docsize WHERE id IN ({rid}))"
            )
        };
        let sql = format!(
            "SELECT (SELECT COUNT(*) FROM notes WHERE id IN ({placeholders}))
                  + (SELECT COUNT(*) FROM conflict_copies WHERE copy_id IN ({placeholders})
                                                OR original_id IN ({placeholders}))
                  + ({rowid_arms})
                  + (SELECT COUNT(*) FROM notes_fts_docsize d
                     WHERE NOT EXISTS (SELECT 1 FROM notes n WHERE n.rowid = d.id))
                  + (SELECT COUNT(*) FROM notes_fts_hidden_docsize d
                     WHERE NOT EXISTS (SELECT 1 FROM notes n WHERE n.rowid = d.id))
                  + (SELECT COUNT(*) FROM proposals WHERE note_id IN ({placeholders}))"
        );
        let mut all: Vec<&dyn rusqlite::ToSql> =
            Vec::with_capacity(4 * family.len() + 2 * rowids.len());
        for _ in 0..4 {
            for s in family {
                all.push(s);
            }
        }
        for _ in 0..2 {
            for r in rowids {
                all.push(r);
            }
        }
        conn.query_row(&sql, all.as_slice(), |r| r.get(0))
            .map_err(CellError::from)
    }

    /// The `redacted_by` JSON of a snapshot row after one more purge op.
    fn redacted_by(conn: &Connection, seq: i64, purge_op_id: &Ulid) -> Result<String, CellError> {
        let current: String = conn.query_row(
            "SELECT redacted_by FROM snapshots WHERE seq = ?1",
            [seq],
            |r| r.get(0),
        )?;
        let mut ops: Vec<String> = serde_json::from_str(&current)
            .map_err(|e| CellError::Db(format!("bad redacted_by: {e}")))?;
        if !ops.contains(&purge_op_id.to_string()) {
            ops.push(purge_op_id.to_string());
        }
        serde_json::to_string(&ops).map_err(|e| CellError::Db(format!("redacted_by: {e}")))
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
    /// (v0.6.11, K1) What the purge of `id` would take with it, asked
    /// BEFORE the purge: read-only, no op, nothing moves. Today it names
    /// the REOPENINGS — the notes whose closing was this note's work (the
    /// old side of a supersede, the target of an accepted correction:
    /// both carry a `supersedes` row whose closer is `id`). A reopening
    /// is the one surprise the rule allows a purge, and Homun must say it
    /// before the user forgets the datum.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn purge_preview(&self, id: &Ulid) -> Result<Vec<Reopening>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT n.id, n.title FROM supersedes s
             JOIN notes n ON n.id = s.old_id
             WHERE s.new_id = ?1 AND n.status = 'superseded'
             ORDER BY n.id",
        )?;
        let out: Vec<Reopening> = stmt
            .query_map([id.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .flatten()
            .filter_map(|(id, title)| {
                Ulid::from_string(&id).ok().map(|id| Reopening { id, title })
            })
            .collect();
        Ok(out)
    }

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
        // v0.6.6 (F3; v0.6.7, G1): does this purge touch the HIDDEN index
        // at all? An index the cell never wrote to holds none of the
        // purged stems, and its `optimize` — an O(index) merge — is pure
        // cost. Counted BEFORE the op: the FTS delete leaves a tombstone
        // the count could no longer see. But the count sees only TODAY:
        // a row that already left the index leaves its stems in the old
        // segments (the released quarantine, the resolved copy — the
        // reviewer's proof), so the DIRTY BIT joins the gate — one meta
        // row every hidden `delete` lights, `fts_delete` itself.
        let hidden_touched: bool =
            ops::hidden_dirty(&inner.conn)? || ops::hidden_rows_in_family(&inner.conn, id)? > 0;
        // v0.6.7 (G3): the family's ids, read BEFORE the op — the purge
        // takes them all, and the earned `scrubbed` check below needs
        // the names once the rows are gone
        let family: Vec<String> = ops::family_of(&inner.conn, id)?;
        // v0.6.8 (H3): the family's rowids, read BEFORE the op too — the
        // LIVE file's own check below counts the index rows by them,
        // with the same function the snapshots speak
        let family_rowids: Vec<i64> = Self::family_rowids(&inner.conn, &family)?;
        let purge_op_id = execute_op(
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
        // the pages the merge frees are zeroed. The hidden twin joins only
        // when the purge touched it (v0.6.6, F3): a quarantined body's stems
        // live nowhere else, but an untouched index holds nothing to forget.
        // The bit goes out WITH the merge (v0.6.7, G1): optimize fuses all
        // segments, so no root of a deleted row survives it.
        inner
            .conn
            .execute_batch("INSERT INTO notes_fts(notes_fts) VALUES('optimize');")?;
        if hidden_touched {
            ops::hidden_optimize(&inner.conn)?;
        }
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
        let mut pending: Vec<String> = if wal_checkpoint(&inner.conn)? {
            Vec::new()
        } else {
            inner.pending_scrub = vec!["wal"];
            vec!["wal".to_string()]
        };
        // v0.6.8 (H3): the LIVE file gets the same check the snapshots
        // get, with the same function — after the local purge the cell
        // must hold nothing of the family: a note row, a derivation row,
        // an FTS row of the family (by the rowids read before) or an
        // orphan docsize row means the scrub left something behind, and
        // the report says so instead of declaring itself done
        if Self::holds_family(&inner.conn, &family, &family_rowids)? != 0 {
            let live_name = self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.display().to_string());
            if !pending.contains(&live_name) {
                pending.push(live_name);
            }
        }
        // v0.3.1 (P0.3; v0.6.7, G3): every registered snapshot is a full
        // copy made before the purge — it holds the note, its payloads
        // and its FTS entries. The same purge (state + log scrub +
        // closure over the copies + optimize + VACUUM) runs on the file;
        // the row keeps its head hash and records the new digest with
        // `redacted_by` naming this purge op — and what the scrub left
        // behind is NAMED, so `scrubbed` is earned, never declared.
        for file in
            Self::scrub_snapshots_of(&mut inner, &self.path, Some(*id), &family, &purge_op_id)?
        {
            if !pending.contains(&file) {
                pending.push(file);
            }
        }
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

    /// The purged-id rule for callers that check BEFORE their own
    /// validations would answer a lesser error (v0.6.9, I1): a
    /// `supersede` on a purged old id would say `NoteNotFound` — the row
    /// is gone — a `propose` on a purged target the same, before the
    /// write path's guard could say the truth. Callers that already hold
    /// the cell's lock speak [`refuse_purged_ids`] on their connection.
    fn refuse_if_purged(&self, ids: &[Ulid]) -> Result<(), CellError> {
        let inner = self.lock()?;
        refuse_purged_ids(&inner.conn, ids)
    }

    /// Replace `old` with `new`: the old note gets `valid_to` (from the new
    /// note's `valid_from`, or from `override_valid_to`) and `superseded`.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when the old note is missing;
    /// [`CellError::NotePurged`] when either id is one the set forgot.
    pub fn supersede(
        &self,
        old_id: &Ulid,
        new: &Note,
        override_valid_to: Option<Validity>,
        actor: &str,
    ) -> Result<(), CellError> {
        // v0.6.9 (I1): closing a purged id OR creating over one — both are
        // the rule's word, and it comes before the existence checks below
        // would answer `NoteNotFound` for a row the purge already took
        self.refuse_if_purged(&[*old_id, new.id])?;
        // a LOCAL writer closes a note it has in front of them: the old note
        // must exist here (the apply tolerates its absence for the REPLAY of
        // a redacted log, but an operation BORN here without its target
        // would claim a causality its author never had)
        let _ = self.get(old_id)?;
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

    /// v0.6.4 (D3): every OPEN conflict copy of the cell — what a
    /// «conflitti da risolvere» list shows. No text in the answer: the
    /// copy is a note like every other, read with [`Cell::get`].
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn conflicts(&self) -> Result<Vec<conflicts::ConflictInfo>, CellError> {
        let inner = self.lock()?;
        let tx = inner.conn.unchecked_transaction()?;
        let out = conflicts::conflicts(&tx)?;
        Ok(out)
    }

    /// v0.6.5 (E5): one conflict copy IN FULL — the three texts a
    /// choice is about: the original's body, the copy's body and the
    /// body at the fork. The quarantined copies are readable here and
    /// only here (`get` answers `null` for them, as for every hidden
    /// note).
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when the id is no OPEN conflict copy.
    pub fn conflict(&self, copy_id: &Ulid) -> Result<conflicts::ConflictDetail, CellError> {
        let inner = self.lock()?;
        let tx = inner.conn.unchecked_transaction()?;
        conflicts::conflict(&tx, copy_id)
    }

    /// v0.6.4 (D3): resolve one conflict copy. Ordinary operations with
    /// the cell's own actor — nothing new for the log: `original` is a
    /// `delete` on the copy's id (the witness (a) that closes it);
    /// `copy` and `merged` first patch the ORIGINAL's body (the copy's
    /// body, or the one given), then delete the copy. Two concurrent
    /// resolutions on two devices are two concurrent writes: with equal
    /// bodies nothing is born, with different bodies a NEW conflict is
    /// — and it should be.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when the id is no conflict copy at
    /// all; [`CellError::Conflict`] when the copy is already closed.
    pub fn resolve_conflict(
        &self,
        copy_id: &Ulid,
        keep: ResolveKeep,
        body: Option<&str>,
        actor: &str,
    ) -> Result<Ulid, CellError> {
        let original = {
            let inner = self.lock()?;
            let tx = inner.conn.unchecked_transaction()?;
            let row: Option<(String, i64)> = tx
                .query_row(
                    "SELECT original_id, open FROM conflict_copies WHERE copy_id = ?1",
                    [copy_id.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match row {
                None => return Err(CellError::NoteNotFound(copy_id.to_string())),
                Some((_, 0)) => {
                    return Err(CellError::Conflict(
                        "la copia di conflitto è già chiusa".into(),
                    ));
                }
                Some((original, _)) => Ulid::from_string(&original)
                    .map_err(|_| CellError::Db(format!("bad conflict original {original}")))?,
            }
        };
        match keep {
            ResolveKeep::Original => {
                self.delete_note(copy_id, actor)?;
            }
            ResolveKeep::Copy | ResolveKeep::Merged => {
                let body = match (keep, body) {
                    (ResolveKeep::Merged, Some(b)) => b.to_string(),
                    (ResolveKeep::Copy, _) => self.conflict_copy_body(copy_id)?.unwrap_or_default(),
                    (ResolveKeep::Merged, None) => {
                        return Err(CellError::Invalid(
                            "resolveConflict: serve il corpo per `merged`".into(),
                        ));
                    }
                    (ResolveKeep::Original, _) => unreachable!(/* matched above */),
                };
                self.patch_note(
                    &original,
                    &NotePatch {
                        body: Some(body),
                        ..Default::default()
                    },
                    actor,
                )?;
                // the patch itself may have CLOSED the copy: `keep: copy`
                // writes the loser's own body, and a later write with the
                // loser's very text CONTAINS it (D1.6b — the diff3 of two
                // identical changes is clean). The delete is the witness
                // only when the patch did not already close it; deleting a
                // copy that just went is not an error — the conflict is
                // resolved either way.
                if self.conflict_copy_is_open(copy_id)? {
                    match self.delete_note(copy_id, actor) {
                        Ok(()) | Err(CellError::NoteNotFound(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(original)
    }

    /// Is this conflict copy's derivation still open?
    fn conflict_copy_is_open(&self, copy_id: &Ulid) -> Result<bool, CellError> {
        let inner = self.lock()?;
        let open: Option<i64> = inner
            .conn
            .query_row(
                "SELECT open FROM conflict_copies WHERE copy_id = ?1",
                [copy_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(open == Some(1))
    }

    /// The derived body a conflict copy holds (its derivation row).
    fn conflict_copy_body(&self, copy_id: &Ulid) -> Result<Option<String>, CellError> {
        let inner = self.lock()?;
        let json: Option<String> = inner
            .conn
            .query_row(
                "SELECT state_json FROM conflict_copies WHERE copy_id = ?1",
                [copy_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Ok(None);
        };
        let mut note = crate::format::Note::from_json(
            &serde_json::from_str::<serde_json::Value>(&json)
                .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?,
        )
        .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?;
        // the stored state has no body (D5): the loser's payload is its
        // home
        if note.body.is_empty()
            && let Some(body) = inner
                .conn
                .query_row(
                    "SELECT json_extract(p.body, '$.set.body') FROM op_payloads p
                     JOIN conflict_copies cc ON cc.loser_op = p.op_id
                     WHERE cc.copy_id = ?1 AND json_extract(p.body, '$.set.body') IS NOT NULL",
                    [copy_id.to_string()],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
        {
            note.body = body;
        }
        Ok(Some(note.body))
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
        // the edge id is minted here (not inside execute_op): it rides in
        // the payload, so the injected minter must cover it too (v0.6.1, P0)
        let edge_id = {
            let inner = self.lock()?;
            mint_ids(&inner)?.0.to_string()
        };
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

    /// The [`replicated_digest`] of this cell (v0.6, P1): what copies of
    /// one cell compare — the state the operations determine, without the
    /// local tables.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn replicated_digest(&self) -> Result<String, CellError> {
        let inner = self.lock()?;
        replicated_digest(&inner.conn)
    }

    /// The row-by-row dump behind [`Cell::replicated_digest`] — the tool
    /// that finds WHERE two copies diverge (line by line).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn replicated_dump(&self) -> Result<String, CellError> {
        let inner = self.lock()?;
        dump_tables(&inner.conn, &replicated_dump_tables())
    }

    /// (v0.6.3, C1 — tests only) The differential oracle: the whole
    /// replicated state rebuilt from the LOG ALONE. The log is stored
    /// without applying a thing, then every object materializes from its
    /// full set of operations — the from-set reconstruction the
    /// incremental arrival path (own writes included) must equal, whatever
    /// order the operations landed in. Returns the rebuilt `(digest,
    /// dump)` so a failing property can show both sides.
    ///
    /// Covers cells WITHOUT a compaction cut (the convergence fleet's): a
    /// cut freezes the below-cut state in payloads that are gone, and the
    /// reconstruction would need the anchor snapshots — `verify`'s replay
    /// is the oracle for those.
    ///
    /// # Errors
    /// [`CellError::Invalid`] on a compacted cell; [`CellError::Db`] on
    /// SQL failure.
    pub fn rematerialize_for_test(&self) -> Result<(String, String), CellError> {
        let inner = self.lock()?;
        let cut: i64 =
            inner
                .conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM snapshots", [], |r| {
                    r.get(0)
                })?;
        if cut > 0 {
            return Err(CellError::Invalid(
                "the oracle covers cells without a compaction cut".into(),
            ));
        }
        let mut rebuild = Connection::open_in_memory()?;
        configure(&rebuild)?;
        let tx = rebuild.transaction()?;
        schema::migrate(&tx)?;
        for op in ops::read_ops(&inner.conn)? {
            ops::store_op(&tx, &op)?;
        }
        // one materialization per object, to a fixpoint: an original's
        // replay derives its copies' rows, and a copy the user edited then
        // materializes over its derivation — one more pass settles it
        let objects: Vec<String> = {
            let mut stmt = tx.prepare("SELECT DISTINCT object FROM op_objects ORDER BY object")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut last_pending: i64 = -1;
        for _ in 0..=objects.len() {
            for object in &objects {
                material::materialize(&tx, object)?;
            }
            let pending: i64 =
                tx.query_row("SELECT COUNT(*) FROM pending_objects", [], |r| r.get(0))?;
            if pending == 0 || pending == last_pending {
                break;
            }
            last_pending = pending;
        }
        let digest = replicated_digest(&tx)?;
        let dump = dump_tables(&tx, &replicated_dump_tables())?;
        tx.commit()?;
        Ok((digest, dump))
    }

    /// (v0.6.4, tests only) Does the current set still DESERVE a copy
    /// for this loser? (the monotonicity judge's other half)
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn conflict_deserved_for_test(
        &self,
        original: &Ulid,
        loser_op: &Ulid,
    ) -> Result<bool, CellError> {
        let inner = self.lock()?;
        let tx = inner.conn.unchecked_transaction()?;
        conflicts::deserved_for_test(&tx, original, loser_op)
    }

    /// (v0.6.4, tests only) The monotonicity judge: does the SET hold a
    /// closure witness for the conflict copy born of one loser op?
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn conflict_witness_for_test(
        &self,
        original: &Ulid,
        loser_op: &Ulid,
    ) -> Result<bool, CellError> {
        let inner = self.lock()?;
        let tx = inner.conn.unchecked_transaction()?;
        conflicts::witness_for_test(&tx, original, loser_op)
    }

    /// The markdown view's reconciliation state: one view's rows (last
    /// scan), path order. v0.4 (P0.1): files belong to a view.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_files_of_view(&self, view_id: &str) -> Result<Vec<VaultFile>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT path, note_id, file_hash, note_version, mtime, size
             FROM vault_files WHERE view_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([view_id], |r| {
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

    /// The file bound to a note, whichever view holds it — `(view_id, row)`
    /// (one file per note is structural, v0.3.1): the cross-view lookups of
    /// the reconciliation.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_file_of_note(&self, id: &Ulid) -> Result<Option<(String, VaultFile)>, CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .query_row(
                "SELECT view_id, path, note_id, file_hash, note_version, mtime, size
                 FROM vault_files WHERE note_id = ?1",
                [id.to_string()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        VaultFile {
                            path: r.get(1)?,
                            note_id: r.get(2)?,
                            file_hash: r.get(3)?,
                            note_version: r.get(4)?,
                            mtime: r.get(5)?,
                            size: r.get(6)?,
                        },
                    ))
                },
            )
            .optional()
            .map_err(CellError::from)
    }

    /// The ids of the living notes (no tombstones), in id order.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn note_ids(&self) -> Result<Vec<Ulid>, CellError> {
        let inner = self.lock()?;
        // v0.6.8 (H1): the bin's one predicate — deleted notes and the
        // copies that follow a lapide, in one word. `list()` reads this.
        let mut stmt = inner.conn.prepare(&format!(
            "SELECT id FROM notes n WHERE {} ORDER BY id",
            bin::not_in_bin("n")
        ))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(Ulid::from_string(&row?).map_err(|_| CellError::Db("bad note id".into()))?);
        }
        Ok(out)
    }

    /// Every note id, tombstones included (v0.4.2, P0.2): a deleted note
    /// keeps its row — restorable — until a purge forgets it, and `list`
    /// with `status: ['deleted']` must be able to find it (the Node binding
    /// offers the tombstones so the caller can offer «Dimentica per
    /// sempre»). [`Cell::note_ids`] stays the living-only view the sync and
    /// the default list use.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn note_ids_all(&self) -> Result<Vec<Ulid>, CellError> {
        let inner = self.lock()?;
        // v0.6.8 (H1): every id, tombstones included — but a bin's COPY
        // is not a note to list even here: it follows its original, and
        // `list({status: ['deleted']})` shows the tombstone, not the
        // copy (deleted notes are IN the bin view, so the first arm
        // keeps them)
        let mut stmt = inner.conn.prepare(&format!(
            "SELECT id FROM notes n WHERE n.status = 'deleted' OR {} ORDER BY id",
            bin::not_in_bin("n")
        ))?;
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
    /// data, no operation. One row per note is structural (v0.3.1, P0.6): a
    /// write for an already-bound note REBINDS it to the new path (and, v0.4,
    /// to THIS view — that is how a legacy row with no view adopts one).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_file_put(&self, view_id: &str, row: &VaultFile) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        inner.conn.execute(
            "INSERT INTO vault_files (view_id, path, note_id, file_hash, note_version, mtime, size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(view_id, path) DO UPDATE SET note_id = ?3, file_hash = ?4,
                                                     note_version = ?5, mtime = ?6, size = ?7
             ON CONFLICT(note_id) DO UPDATE SET view_id = ?1, path = ?2, file_hash = ?4,
                                               note_version = ?5, mtime = ?6, size = ?7",
            rusqlite::params![
                view_id,
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

    /// The views this cell is bound to (v0.4, P0.1): identities, with the
    /// root each one was last seen at, oldest binding first.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_views(&self) -> Result<Vec<VaultView>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT view_id, root, bound_at, last_seen_at
             FROM vault_views ORDER BY bound_at, view_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(VaultView {
                view_id: r.get(0)?,
                root: r.get(1)?,
                bound_at: r.get(2)?,
                last_seen_at: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// A view by its canonical absolute root, when one is bound there.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_view_by_root(&self, root: &str) -> Result<Option<VaultView>, CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .query_row(
                "SELECT view_id, root, bound_at, last_seen_at
                 FROM vault_views WHERE root = ?1",
                [root],
                |r| {
                    Ok(VaultView {
                        view_id: r.get(0)?,
                        root: r.get(1)?,
                        bound_at: r.get(2)?,
                        last_seen_at: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(CellError::from)
    }

    /// A view by its id.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_view_by_id(&self, view_id: &str) -> Result<Option<VaultView>, CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .query_row(
                "SELECT view_id, root, bound_at, last_seen_at
                 FROM vault_views WHERE view_id = ?1",
                [view_id],
                |r| {
                    Ok(VaultView {
                        view_id: r.get(0)?,
                        root: r.get(1)?,
                        bound_at: r.get(2)?,
                        last_seen_at: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(CellError::from)
    }

    /// Register (or refresh) a view binding (v0.4, P0.1): the id is the
    /// identity — a known id at a NEW path updates the root, it never adds
    /// a second view. Written by every `vault init`/`sync`.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_view_put(&self, view_id: &str, root: &str) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        let now = OffsetDateTime::now_utc()
            .replace_millisecond(0)
            .unwrap_or(OffsetDateTime::now_utc())
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        inner.conn.execute(
            "INSERT INTO vault_views (view_id, root, bound_at, last_seen_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(view_id) DO UPDATE SET root = ?2, last_seen_at = ?3",
            rusqlite::params![view_id, root, now],
        )?;
        Ok(())
    }

    /// Detach a view (v0.4, P0.1): its row, its files and its conflict
    /// copies leave the cell — the FILES stay on disk, they were never the
    /// engine's. Returns `(files, conflicts)` rows removed.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_unbind(&self, view_id: &str) -> Result<(usize, usize), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        let files = inner
            .conn
            .execute("DELETE FROM vault_files WHERE view_id = ?1", [view_id])?;
        let conflicts = inner
            .conn
            .execute("DELETE FROM vault_conflicts WHERE view_id = ?1", [view_id])?;
        inner
            .conn
            .execute("DELETE FROM vault_views WHERE view_id = ?1", [view_id])?;
        Ok((files, conflicts))
    }

    /// The views that still hold something of a note — a file or a conflict
    /// copy (v0.4, P0.1): an unreachable view keeps a purge pending only
    /// when it appears here.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_views_of_note(&self, id: &Ulid) -> Result<Vec<VaultView>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT v.view_id, v.root, v.bound_at, v.last_seen_at
             FROM vault_views v
             WHERE EXISTS (SELECT 1 FROM vault_files f
                           WHERE f.view_id = v.view_id AND f.note_id = ?1)
                OR EXISTS (SELECT 1 FROM vault_conflicts c
                           WHERE c.view_id = v.view_id AND c.note_id = ?1)
             ORDER BY v.bound_at, v.view_id",
        )?;
        let rows = stmt.query_map([id.to_string()], |r| {
            Ok(VaultView {
                view_id: r.get(0)?,
                root: r.get(1)?,
                bound_at: r.get(2)?,
                last_seen_at: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The paths (relative to the view root) of every `.conflitto` copy of
    /// a note in ONE view (v0.4, P0.1).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_conflicts_of(&self, view_id: &str, id: &Ulid) -> Result<Vec<String>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT path FROM vault_conflicts WHERE view_id = ?1 AND note_id = ?2 ORDER BY path",
        )?;
        let rows = stmt.query_map(rusqlite::params![view_id, id.to_string()], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every conflict copy ONE view knows about: `(path, note_id)` pairs
    /// (v0.4, P0.1: the sync's cleaner works per view).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_conflicts_of_view(
        &self,
        view_id: &str,
    ) -> Result<Vec<(String, String)>, CellError> {
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT path, note_id FROM vault_conflicts WHERE view_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([view_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Register a `.conflitto` copy the engine just created (v0.3.2, P1.6):
    /// the scanner ignores it, so this registry is the only thing that lets
    /// a purge take it away. v0.4: it belongs to a view, and `note_id: None`
    /// is the orphan marker of P0.2 (a copy whose base file is not bound).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_conflict_put(
        &self,
        view_id: &str,
        path: &str,
        note_id: Option<&Ulid>,
    ) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        let now = OffsetDateTime::now_utc()
            .replace_millisecond(0)
            .unwrap_or(OffsetDateTime::now_utc())
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        inner.conn.execute(
            "INSERT INTO vault_conflicts (view_id, path, note_id, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(view_id, path) DO UPDATE SET note_id = ?3, created_at = ?4",
            rusqlite::params![
                view_id,
                path,
                note_id.map(Ulid::to_string).unwrap_or_default(),
                now
            ],
        )?;
        Ok(())
    }

    /// Forget one conflict copy's row.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_conflict_delete(&self, view_id: &str, path: &str) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        inner.conn.execute(
            "DELETE FROM vault_conflicts WHERE view_id = ?1 AND path = ?2",
            rusqlite::params![view_id, path],
        )?;
        Ok(())
    }

    /// Forget one path of the markdown view's state (the file is gone).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn vault_file_delete(&self, view_id: &str, path: &str) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        inner.conn.execute(
            "DELETE FROM vault_files WHERE view_id = ?1 AND path = ?2",
            rusqlite::params![view_id, path],
        )?;
        Ok(())
    }

    /// The latest compaction row's cut, when any (the row may be a
    /// checkpoint without a file — the payloads below the cut are gone
    /// either way).
    fn last_cut_seq(&self) -> Result<Option<i64>, CellError> {
        let inner = self.lock()?;
        inner
            .conn
            .query_row(
                "SELECT seq FROM snapshots ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(CellError::from)
    }

    /// Bytes on disk of the cell, its WAL and every snapshot file (v0.3.2:
    /// the honest bill of a compaction is the whole directory footprint).
    fn dir_footprint(&self) -> u64 {
        let mut total = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let mut wal = self.path.clone().into_os_string();
        wal.push("-wal");
        total += std::fs::metadata(Path::new(&wal))
            .map(|m| m.len())
            .unwrap_or(0);
        if let Some(dir) = self.path.parent()
            && let Some(stem) = self
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // v0.4.3 (P0.1): only THIS cell's own snapshots belong in
                // its bill — a neighboring cell named `storia-snap-archivio`
                // used to be counted into storia's footprint
                if snapshot_of(&name).is_some_and(|(cell, _)| cell == stem) {
                    total += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        total
    }

    /// Compact the log (DESIGN §4.3, v0.3): forget the payloads of every op
    /// up to `up_to_seq` (must be within the live log); the skeletons and
    /// the chain stay whole. v0.3.2 (P0.1): by default the compaction
    /// registers a **checkpoint** — head hash and [`state_digest`] of the
    /// logical state at the head, no file (a second copy of the cell served
    /// only the full audit, and the logical state *is* almost the whole
    /// cell). With `keep_snapshot` the v0.3.1 behavior: `VACUUM INTO` a
    /// snapshot of the cell **exactly as it stands**, stripped to its
    /// logical state (payloads up to the cut, vectors, model registry,
    /// markdown-view bindings and FTS content go), registered with its head
    /// hash and digest. Only the LAST row stays, with or without a file:
    /// the latest compaction replaces the previous one.
    ///
    /// # Errors
    /// [`CellError::Invalid`] when `up_to_seq` is outside the live log;
    /// [`CellError::Db`] on SQL or I/O failures (a failed compaction leaves
    /// the previous row and its file alone).
    /// The stable frontier of this cell (v0.6, P1.5): the deepest seq every
    /// live device has confirmed holding — everything at or below it is
    /// causally stable, and only there may a compaction forget payloads. An
    /// unshared cell is its own only device: the frontier is the head.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn stable_frontier(&self) -> Result<i64, CellError> {
        let inner = self.lock()?;
        stable_frontier(&inner.conn)
    }

    /// Record that a sync session with `device` ended with both sides
    /// holding the same set: this copy's head, as it stands now, is
    /// confirmed there (v0.6, P1.5). A session that exchanged NOTHING
    /// writes NOTHING (v0.6.1-ciclo, K1): the row moves only when the
    /// head it confirms moved — the timestamp alone must never dirty the
    /// cell, or every idle pass would bump `data_version` and re-light
    /// the serve cycle for a sync that changed nothing.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn record_session_ack(&self, device: &str) -> Result<i64, CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let mut inner = self.lock()?;
        let tx = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM ops", [], |r| r.get(0))?;
        let held: Option<i64> = tx
            .query_row(
                "SELECT ack_seq FROM device_acks WHERE device = ?1",
                [device],
                |r| r.get(0),
            )
            .optional()?;
        if held == Some(head) {
            // nothing changed: no commit, no dirty page, no data_version
            return Ok(head);
        }
        let at = OffsetDateTime::now_utc()
            .replace_millisecond(0)
            .unwrap_or(OffsetDateTime::now_utc())
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        tx.execute(
            "INSERT INTO device_acks (device, ack_seq, at) VALUES (?1, ?2, ?3)
             ON CONFLICT(device) DO UPDATE SET ack_seq = ?2, at = ?3",
            rusqlite::params![device, head, at],
        )?;
        tx.commit()?;
        Ok(head)
    }

    /// Adopt the log as it stands (v0.6, P1.6): the operation records the
    /// Merkle root of the wire identities of everything written before the
    /// sync era, so a copy that joins later accepts the unsigned operations
    /// only under the root that covers them. Called by `sync share`.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn adopt_log(&self, actor: &str) -> Result<Ulid, CellError> {
        let mut hashes: Vec<String> = {
            let inner = self.lock()?;
            let mut stmt = inner.conn.prepare("SELECT content_hash FROM ops")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let count = i64::try_from(hashes.len()).unwrap_or(0);
        let root = ops::merkle_root(&mut hashes);
        let payload = serde_json::json!({ "root": root, "count": count });
        self.execute(OpKind::Adopt, None, actor, payload)?;
        // the adopt itself is not covered by its own root: the sharing
        // device signs IT (P2), and it lands above the covered set
        let inner = self.lock()?;
        let op_id: Option<String> = inner
            .conn
            .query_row(
                "SELECT op_id FROM ops WHERE kind = 'adopt' ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        op_id
            .and_then(|s| Ulid::from_string(&s).ok())
            .ok_or_else(|| CellError::Db("the adopt operation did not land".into()))
    }

    /// This cell is SHARED (v0.6, P1.5): its compactions hold at the stable
    /// frontier of the device list, not at its own head.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn mark_shared(&self) -> Result<(), CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        inner.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('sync_shared', '1')
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            [],
        )?;
        Ok(())
    }

    pub fn compact(
        &self,
        up_to_seq: i64,
        opts: &CompactOptions,
    ) -> Result<CompactReport, CellError> {
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
        // v0.6 (P1.5): a SHARED cell compacts only under the stable
        // frontier — the deepest seq every live device has confirmed. Above
        // it the payloads stay: someone could still ask for them. An
        // unshared cell is its own only device: its frontier is the head.
        let mut up_to_seq = up_to_seq;
        let frontier = stable_frontier(&inner.conn)?;
        if up_to_seq > frontier {
            up_to_seq = frontier;
        }
        if up_to_seq < 1 {
            return Err(CellError::Invalid(
                "nothing is stable yet: no device has confirmed any operation".into(),
            ));
        }
        let head_hash: String = inner
            .conn
            .query_row("SELECT hash FROM ops WHERE seq = ?1", [head_seq], |r| {
                r.get(0)
            })
            .unwrap_or_default();
        let bytes_before = self.dir_footprint();
        let file_name = format!(
            "{}-snap-{head_seq}.sqlite",
            self.path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "cell".into())
                .trim_end_matches(".sqlite")
        );
        let parent = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        // The file snapshot, when asked for: written and stripped BEFORE the
        // transaction, so a failure here leaves the cell untouched.
        let file_snapshot: Option<(PathBuf, String)> = if opts.keep_snapshot {
            let target = parent.join(&file_name);
            // VACUUM INTO refuses an existing file: a re-compact at the same
            // head replaces its own snapshot
            if target.exists() {
                std::fs::remove_file(&target)?;
            }
            inner.conn.execute_batch(&format!(
                "VACUUM INTO '{}'",
                target.display().to_string().replace('\'', "''")
            ))?;
            // v0.3.1 (P0.4): strip the file to its logical state, then
            // digest what stays. The payloads up to the cut go first: their
            // content must not outlive the compaction in the one file that
            // never gets a purge otherwise.
            let digest = {
                let mut snap_conn = Connection::open(&target)?;
                snap_conn.pragma_update(None, "journal_mode", "DELETE")?;
                let secure_before: i64 =
                    snap_conn.query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
                snap_conn.pragma_update(None, "secure_delete", "ON")?;
                {
                    let tx = snap_conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    tx.execute(
                        "DELETE FROM op_payloads WHERE op_id IN (
                             SELECT op_id FROM ops WHERE seq <= ?1)",
                        [up_to_seq],
                    )?;
                    tx.execute(
                        "UPDATE ops SET payload_state = 'redacted' WHERE seq <= ?1",
                        [up_to_seq],
                    )?;
                    tx.execute("DELETE FROM note_vectors", [])?;
                    tx.execute("DELETE FROM embedding_models", [])?;
                    tx.execute("DELETE FROM vault_files", [])?;
                    tx.execute("DELETE FROM vault_views", [])?;
                    tx.execute("DELETE FROM vault_conflicts", [])?;
                    tx.execute("DELETE FROM snapshots", [])?;
                    // v0.6: local bookkeeping never travels with the state
                    tx.execute("DELETE FROM device_acks", [])?;
                    tx.execute("DELETE FROM pending_objects", [])?;
                    // external-content FTS5: `delete-all` empties both
                    // indices (their segments carry the stems); verify
                    // rebuilds them from the content table before the tail
                    // replay. v0.6.7 (G1): the hidden index's dirty bit
                    // goes out with them — the segments that carried the
                    // roots are gone, a lit bit would be a lie
                    tx.execute_batch(
                        "INSERT INTO notes_fts(notes_fts) VALUES('delete-all');
                         INSERT INTO notes_fts_hidden(notes_fts_hidden) VALUES('delete-all');
                         DELETE FROM meta WHERE key = 'fts_hidden_dirty';",
                    )?;
                    tx.commit()?;
                }
                snap_conn.pragma_update(None, "secure_delete", secure_before)?;
                snap_conn.execute_batch("VACUUM;")?;
                state_digest(&snap_conn)?
            };
            Some((target, digest))
        } else {
            None
        };
        let created = OffsetDateTime::now_utc()
            .replace_millisecond(0)
            .unwrap_or(OffsetDateTime::now_utc())
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        // Only the last row stays (P0.4). The rows that exist NOW are read
        // before the new one lands (the write lock is held): at most one by
        // invariant, more only after a failed or crashed compaction. Their
        // files must not outlive them.
        let previous: Vec<(i64, Option<String>)> = {
            let mut stmt = inner
                .conn
                .prepare("SELECT seq, file FROM snapshots ORDER BY seq")?;
            stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .flatten()
            .collect()
        };
        let tx = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM op_payloads WHERE op_id IN (
                 SELECT op_id FROM ops WHERE seq <= ?1)",
            [up_to_seq],
        )?;
        // v0.6.1 (P0): the cut rows turn `redacted` — their skeletons stay
        // (the `base` of a later arrival still finds them), their payloads
        // go, and the state says which is which.
        tx.execute(
            "UPDATE ops SET payload_state = 'redacted' WHERE seq <= ?1",
            [up_to_seq],
        )?;
        // For a checkpoint the digest is the LIVE state's after the cut (the
        // same bytes the file snapshot would carry: the live cell minus the
        // forgotten payloads); for a file snapshot it is the stripped file's
        // — equal by construction to the live one (property-tested).
        let digest = match &file_snapshot {
            Some((_, d)) => d.clone(),
            None => state_digest(&tx)?,
        };
        let new_file: Option<&str> = file_snapshot.as_ref().map(|_| file_name.as_str());
        tx.execute(
            "INSERT INTO snapshots (seq, head_hash, state_digest, file, redacted_by, created_at)
             VALUES (?1, ?2, ?3, ?4, '[]', ?5)
             ON CONFLICT(seq) DO UPDATE SET head_hash = ?2, state_digest = ?3,
                                            file = ?4, redacted_by = '[]', created_at = ?5",
            rusqlite::params![up_to_seq, head_hash, digest, new_file, created],
        )?;
        // The old FILE goes before its row (v0.3.2, P0.4): a row deleted
        // first would orphan the file on a crash or a failed removal, and a
        // file no row points at escapes every later purge — purge walks the
        // ROWS. A removal that fails is a compaction error: the transaction
        // unwinds, the old row stays with its file, and the next purge
        // scrubs it. A crash inside this window leaves a row WITH its file,
        // never a row without one.
        for (seq, old_file) in &previous {
            let Some(old_file) = old_file else { continue };
            if Some(old_file.as_str()) == new_file {
                continue; // the file this very compaction just wrote
            }
            let old = parent.join(old_file);
            match std::fs::remove_file(&old) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(CellError::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "cannot remove the old snapshot {} (seq {seq}): {e}",
                            old.display()
                        ),
                    )));
                }
            }
        }
        let new_file_owned: Option<String> = new_file.map(str::to_string);
        for (seq, old_file) in &previous {
            // The previous row survives when it IS the new row — a re-compact
            // at the same cut with the same file rewrites it in place (found
            // by the compaction property test: deleting by identity left the
            // cell with no row at all, and a from-zero replay of a redacted
            // log diverged). By (seq, file), not seq alone: a re-compact at
            // the same cut with a new head reuses the seq, and the fresh row
            // must survive.
            if *seq == up_to_seq && *old_file == new_file_owned {
                continue;
            }
            tx.execute(
                "DELETE FROM snapshots WHERE seq = ?1 AND file IS ?2",
                rusqlite::params![seq, old_file],
            )?;
        }
        tx.commit()?;
        // the forgotten payloads are the point: give the space back to the OS
        inner
            .conn
            .execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
        let bytes_after = self.dir_footprint();
        Ok(CompactReport {
            cut_seq: up_to_seq,
            head_seq,
            head_hash,
            state_digest: digest,
            snapshot: file_snapshot.map(|(p, _)| p),
            bytes_before,
            bytes_after,
        })
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
        // v0.6.8 (H1): the visible resolution speaks the bin's one
        // predicate — a copy that follows a lapide is not there for the
        // surfaces that must not reveal hidden notes
        let sql = if visible_only {
            format!(
                "SELECT id FROM notes n
                   WHERE (n.id LIKE ?1 || '%' OR n.id LIKE '%' || ?1)
                     AND n.status NOT IN ('quarantined', 'deleted')
                     AND {} ORDER BY id",
                bin::not_in_bin("n")
            )
        } else {
            "SELECT id FROM notes WHERE id LIKE ?1 || '%' OR id LIKE '%' || ?1 ORDER BY id"
                .to_string()
        };
        let mut stmt = inner.conn.prepare(&sql)?;
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

    /// Read a note (with its body). A bin's COPY answers
    /// [`CellError::NoteNotFound`] — the same word as a missing note
    /// (v0.6.8, H1: the binding maps it to `null`, as it already does
    /// for a tombstone). A tombstone itself still reads here: restore
    /// and the tombstone listings want it, and the JS `get` hides it.
    /// v0.6.9 (I2): the bin's copy arm is the OPEN copy only — a copy
    /// the user took is their note and answers. The old fast guard
    /// (`conflitto` tag first, predicate only then) died with it: the
    /// tag is the USER's to change, and the reviewer's re-tagged copy
    /// walked past the guard into a wrong `Active`. The probe now rides
    /// in the SAME single read, as the copy arm of [`bin::not_in_bin`]
    /// verbatim — the same predicate by construction (the SQL texts are
    /// the same words), and the contract test walks the agreement.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] when missing or in the bin.
    pub fn get(&self, id: &Ulid) -> Result<Note, CellError> {
        let inner = self.lock()?;
        let tx = inner.conn.unchecked_transaction()?;
        let id_str = id.to_string();
        // ONE plain read — the same columns the old two-query read
        // fetched, plus the bin's copy arm as a computed column: no
        // second round trip, no tag to trust
        // v0.6.9 (I2): the statement is CACHED — a bulk reader walks ten
        // thousand ids through this read, and paying the planner per call
        // cost the list its whole budget (measured: 42,7 → 73,7 ms; the
        // cache took it home)
        let mut read_note = tx.prepare_cached(
            "SELECT n.frontmatter_json, n.body,
                    EXISTS(SELECT 1 FROM conflict_copies cc
                           JOIN notes o ON o.id = cc.original_id
                           WHERE cc.copy_id = n.id
                             AND o.status = 'deleted' AND cc.open = 1)
             FROM notes n WHERE n.id = ?1",
        )?;
        let row: (String, String, bool) = read_note
            .query_row([&id_str], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|_| CellError::NoteNotFound(id_str.clone()))?;
        let mut note = Note::from_json(
            &serde_json::from_str::<Value>(&row.0)
                .map_err(|e| CellError::Db(format!("corrupt frontmatter_json: {e}")))?,
        )
        .map_err(|e| CellError::Db(format!("corrupt frontmatter_json: {e}")))?;
        note.body = row.1;
        // a tombstone reads here (restore and the tombstone listings
        // want it); an OPEN copy whose original holds a lapide does not
        if row.2 {
            return Err(CellError::NoteNotFound(id_str));
        }
        Ok(note)
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
        if let Some(cut) = self.last_cut_seq()?
            && seq < cut
        {
            return Err(CellError::Compacted { snapshot_seq: cut });
        }
        let inner = self.lock()?;
        let mut stmt = inner.conn.prepare(
            "SELECT o.seq, o.op_id, o.hlc, o.actor, o.kind, o.note_id, o.base, o.content_hash,
                    o.payload_digest, o.payload_state, p.salt, p.body, o.prev_hash, o.hash, o.device, o.sig
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
    /// v0.3.2 (P0.2): the report says WHAT could be replayed
    /// ([`Replay`]). A compaction without a snapshot file (a checkpoint)
    /// makes the from-zero replay impossible and the tail cannot replay
    /// without the state at the cut: there `verify` still checks the whole
    /// hash chain, that every surviving payload matches its digest, the
    /// coherence of the derived indexes (FTS, vectors, edges) and — when the
    /// head has not moved past the checkpoint — that the current state's
    /// digest is the recorded one.
    ///
    /// # Errors
    /// [`CellError::Db`] only on infrastructure failures.
    pub fn verify(&self) -> Result<VerifyReport, CellError> {
        let mut inner = self.lock()?;
        // v0.6.10 (J2): verify is READ-ONLY, by construction — the whole
        // audit runs inside ONE deferred transaction that ALWAYS rolls
        // back (it rolls back on drop, on every return path). The
        // `PRAGMA query_only` route cannot host it: the FTS5
        // `integrity-check` is a write command by construction (it opens
        // a write transaction even when it finds nothing), and the
        // choice leaves a bonus — every read of the audit sees one
        // stable snapshot, and the audit cannot tear under a concurrent
        // writer. The phantoms (a living row on a purged id, the
        // v0.6.7/v0.6.8 engines' local-write hole) are NAMED, not
        // repaired: the phantom is content the USER wrote after the
        // purge (a reimport, a backup), the rule forgot the ID, not the
        // TEXT — and a diagnosis must never destroy. `Cell::repair` is
        // the explicit step that moves the content and applies the rule.
        let audit = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let phantom_purged: Vec<String> =
            phantom_ids(&audit)?.iter().map(|i| i.to_string()).collect();
        let source_ops = match ops::read_ops(&audit) {
            Ok(ops) => ops,
            Err(e) => {
                return Ok(VerifyReport {
                    ops: 0,
                    replay: Replay::Full,
                    hash_chain_ok: false,
                    replay_ok: false,
                    fts_ok: false,
                    snapshot_ok: false,
                    mismatch: Some(format!("unreadable log: {e}")),
                    coverage: Vec::new(),
                    phantom_purged,
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
            // v0.6.1 (P0): the payload check follows the payload's STATE.
            // `whole` must still digest to the ORIGINAL digest (a changed
            // byte, with the salt, breaks here); `censored` must be exactly
            // what the censor's deterministic rule produces (shape and
            // salt) — the identity no longer covers the body, so the shape
            // is what bounds it; `redacted` carries nothing to check.
            let payload_ok = match op.payload_state {
                ops::PayloadState::Whole => match (&op.salt, &op.body) {
                    (Some(salt), Some(body)) => {
                        ops::payload_digest(salt, body) == op.payload_digest
                    }
                    _ => false,
                },
                ops::PayloadState::Redacted => op.salt.is_none() && op.body.is_none(),
                ops::PayloadState::Censored => {
                    op.salt.as_deref() == Some(&ops::censor_salt(&op.op_id.to_string())[..])
                        && ops::censor_shape_ok(op)
                }
            };
            if !payload_ok {
                chain_ok = false;
                break;
            }
            prev = op.hash.clone();
        }
        // Compaction (v0.3): the payloads up to the snapshot are forgotten,
        // so a from-zero replay is impossible — restore the snapshot into
        // memory (checking its recorded head hash and state digest) and
        // replay only the tail. v0.3.1 (P0.3): a purge may have REDACTED the
        // snapshot after the fact — its recorded digest is the redacted one
        // and its head hash stays as history (the in-file censor rehashes the
        // tail), so the snapshot's position is found by its last op_id, and a
        // redacted snapshot is accepted exactly when the tail replay, that
        // purge included, reproduces the live state (the comparison below).
        // v0.3.2 (P0.1/P0.2): a checkpoint row has no file — nothing to
        // restore, the tail cannot replay without the state at the cut.
        let mut snapshot_ok = true;
        let mut replay = Replay::Full;
        let mut rebuild_conn = Connection::open_in_memory()?;
        configure(&rebuild_conn)?;
        let mut tail: &[Op] = &source_ops;
        let mut chain_only = false;
        // (read inline: the cell lock is already held)
        let snapshot: Option<SnapshotRecord> = audit
            .query_row(
                "SELECT seq, head_hash, state_digest, file, redacted_by FROM snapshots
                 ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        match snapshot.as_ref() {
            Some((snap_seq, head_hash, digest, Some(file), redacted_by)) => {
                replay = Replay::FromSnapshot { seq: *snap_seq };
                let snap_path = self
                    .path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(file);
                // what the file says: its logical digest and its own last op
                let checks = Connection::open(&snap_path).and_then(|snap| {
                    // read-only by construction here too (J2): the
                    // snapshot is a full copy of the cell, and a write
                    // through this handle would mutate it silently
                    snap.pragma_update(None, "query_only", "ON")?;
                    let d = state_digest(&snap)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    let last: Option<(String, String)> = snap
                        .query_row(
                            "SELECT op_id, hash FROM ops ORDER BY seq DESC LIMIT 1",
                            [],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    Ok((snap, d, last))
                });
                let was_redacted = redacted_by != "[]" && !redacted_by.is_empty();
                match checks {
                    Ok((snap_conn, d, last)) if Some(d.as_str()) == digest.as_deref() => {
                        // Where this snapshot sits in the live log: by its
                        // last op id. A redaction rehashes the snapshot's log
                        // tail, so the recorded head hash only has to match
                        // when the snapshot was never redacted (it stays as
                        // history).
                        let position = last.as_ref().and_then(|(last_op_id, last_hash)| {
                            let seq = source_ops
                                .iter()
                                .find(|o| o.op_id.to_string() == *last_op_id)
                                .map(|o| o.seq)?;
                            (was_redacted || last_hash == head_hash).then_some(seq)
                        });
                        if let Some(snap_head_seq) = position {
                            let tx = rebuild_conn.transaction()?;
                            tx.execute_batch("DROP TABLE IF EXISTS snapshots;")?;
                            tx.commit()?;
                            {
                                let backup =
                                    rusqlite::backup::Backup::new(&snap_conn, &mut rebuild_conn)
                                        .map_err(|e| {
                                            CellError::Db(format!("snapshot restore: {e}"))
                                        })?;
                                backup
                                    .run_to_completion(
                                        128,
                                        std::time::Duration::from_millis(0),
                                        None,
                                    )
                                    .map_err(|e| CellError::Db(format!("snapshot restore: {e}")))?;
                            }
                            // the snapshot may come from an older schema (a
                            // v0.3 cell opened after the upgrade): bring the
                            // restored copy to the current one — `meta` is
                            // compared
                            {
                                let tx = rebuild_conn.transaction()?;
                                schema::migrate(&tx)?;
                                tx.commit()?;
                            }
                            // v0.3.1 (P0.4): the snapshot carries no FTS
                            // content (stripped at compact — it is derived,
                            // and its segments hold the stems). Rebuild both
                            // indices from the content table BEFORE the tail
                            // replay: the tail's FTS deletes must find their
                            // rows. Not the `rebuild` command: it indexes
                            // every content row, tombstones included, while
                            // each live index holds exactly the notes its
                            // side of the status split owns (v0.6, P0) — so
                            // the same discipline: empty, then insert each
                            // side's rows only.
                            {
                                let tx = rebuild_conn.transaction()?;
                                tx.execute_batch(
                                    "INSERT INTO notes_fts(notes_fts) VALUES('delete-all');
                                     INSERT INTO notes_fts(rowid, title, body_norm, tags_text)
                                     SELECT rowid, COALESCE(title, ''), body_norm, tags_text
                                     FROM notes WHERE status IN ('active', 'superseded');
                                     INSERT INTO notes_fts_hidden(notes_fts_hidden) VALUES('delete-all');
                                     INSERT INTO notes_fts_hidden(rowid, title, body_norm, tags_text)
                                     SELECT rowid, COALESCE(title, ''), body_norm, tags_text
                                     FROM notes WHERE status = 'quarantined';
                                     DELETE FROM meta WHERE key = 'fts_hidden_dirty';",
                                )?;
                                tx.commit()?;
                            }
                            // v0.3 snapshots are full copies taken BEFORE the
                            // forgetting: apply the same compaction cut to
                            // the restored copy (idempotent for the logical
                            // snapshots of v0.3.1, whose payloads are already
                            // cut).
                            {
                                let tx = rebuild_conn.transaction()?;
                                tx.execute(
                                    "DELETE FROM op_payloads WHERE op_id IN (
                                         SELECT op_id FROM ops WHERE seq <= ?1)",
                                    [*snap_seq],
                                )?;
                                tx.execute(
                                    "UPDATE ops SET payload_state = 'redacted'
                                     WHERE seq <= ?1",
                                    [*snap_seq],
                                )?;
                                // v0.6.1 (P0): the rebuilt copy must know the
                                // CUT the live cell knows — its `snapshots`
                                // row. The strip empties the table in the
                                // file, and without the row a rematerialization
                                // inside the tail replay would replay from
                                // zero over redacted skeletons and diverge
                                // from the live cell's own materialization
                                // (which anchors at the cut state). Mirror
                                // the live row, verbatim.
                                tx.execute(
                                    "INSERT INTO snapshots
                                         (seq, head_hash, state_digest, file, redacted_by, created_at)
                                     VALUES (?1, ?2, ?3, NULL, ?4, '')",
                                    rusqlite::params![
                                        *snap_seq,
                                        head_hash,
                                        digest,
                                        redacted_by,
                                    ],
                                )?;
                                tx.commit()?;
                            }
                            let from = source_ops
                                .iter()
                                .position(|o| o.seq > snap_head_seq)
                                .unwrap_or(source_ops.len());
                            tail = &source_ops[from..];
                        } else {
                            snapshot_ok = false;
                        }
                    }
                    _ => snapshot_ok = false,
                }
            }
            // v0.3.2 (P0.1): the checkpoint — a row without a file. The
            // from-zero replay is impossible and the tail has no state to
            // replay on: chain-only. The chain loop above already checked
            // every surviving payload against its digest; below, the FTS
            // coherence runs on the live cell and `ChainOnly` coherence of
            // the derived indexes replaces the state comparison.
            Some((snap_seq, ..)) => {
                replay = Replay::ChainOnly {
                    since_seq: *snap_seq,
                };
                chain_only = true;
            }
            None => {
                let tx = rebuild_conn.transaction()?;
                schema::migrate(&tx)?;
                tx.commit()?;
            }
        }
        let mut mismatch: Option<String> =
            (!snapshot_ok).then(|| "the compaction snapshot does not match its record".to_string());
        // Replay: the tail on the snapshot, or everything from zero.
        let mut replay_err: Option<String> = None;
        if !chain_only {
            let tx = rebuild_conn.transaction()?;
            for op in tail {
                if let Err(e) = material::arrive(&tx, op) {
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
                replay,
                hash_chain_ok: chain_ok,
                replay_ok: false,
                fts_ok: false,
                snapshot_ok,
                mismatch: Some(mismatch.unwrap_or_else(|| format!("replay failed: {e}"))),
                coverage: Vec::new(),
                phantom_purged,
            });
        }
        if chain_only {
            // ChainOnly (P0.2): everything that can still be checked without
            // a replay — the derived edges re-derived from the notes, the
            // vectors' liveness, the digest at the head.
            if let Err(e) = verify_checkpoint_coherence(&audit, snapshot.as_ref()) {
                mismatch = mismatch.or(Some(e));
            }
        } else if mismatch.is_none() {
            mismatch = compare_states(&audit, &rebuild_conn)?;
        }
        // The FTS index is not part of the logical replay: check it against
        // its content table, on the live cell and on the rebuild alike (a
        // checkpoint has no rebuild: the live check is all there is).
        let fts_check = if chain_only {
            fts_integrity_check(&audit)
        } else {
            fts_integrity_check(&audit).and_then(|()| fts_integrity_check(&rebuild_conn))
        };
        let fts_ok = match fts_check {
            Ok(()) => true,
            Err(e) => {
                mismatch = Some(mismatch.unwrap_or_else(|| format!("fts index: {e}")));
                false
            }
        };
        // Vector coverage is reported, not verified: embeddings are derived
        // data, recomputable from the notes at any time.
        let coverage = vectors::VectorTx { conn: &audit }
            .coverage()
            .unwrap_or_default();
        Ok(VerifyReport {
            ops: source_ops.len(),
            replay,
            hash_chain_ok: chain_ok,
            replay_ok: mismatch.is_none(),
            fts_ok,
            snapshot_ok,
            mismatch,
            coverage,
            phantom_purged,
        })
    }

    /// v0.6.10 (J2): the EXPLICIT repair of what verify found. A phantom
    /// — a living row on a purged id, written by the v0.6.7/v0.6.8
    /// engines — is content the user wrote after the purge (a
    /// reimportation, a backup): the rule forgot the ID, not the TEXT.
    /// For each phantom, in its own transaction (an interruption leaves
    /// every note either as it was or repaired, never halfway):
    /// - the content of the living row (body, title, tags, type, fields,
    ///   source) is written to a note with a NEW id, with a normal `put`
    ///   — the actor declares the repair, `engramma:repair`, the AGENT
    ///   class: the engine's hand, like every `mcp:` write — the human
    ///   authority is the command that ran the repair, not this write;
    /// - then the forgetting rule applies to the old id: the state rows,
    ///   the closure over the conflict copies, the log scrub, under
    ///   `secure_delete` — the same steps the purge itself takes, minus
    ///   the op (the purge is already in the log).
    ///
    /// The J1 leftover of a v0.6.9 cell — a row born from a half that
    /// no longer replays (the resurrected `supersedes` of a decide over
    /// a purged proposal) — is settled by the same call: the
    /// reconciliation re-derives the co-objects of the dominated
    /// operations, and the state is a function of the operations again.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure; [`CellError::Invalid`] on a
    /// phantom row that cannot be read back as a note.
    pub fn repair(&self) -> Result<RepairReport, CellError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let inner = self.lock()?;
        // v0.6.8 (H3)'s discipline: the test-only injection stops the
        // walk after N phantoms — the interruption is part of the
        // contract, and the test proves the relaunch ends clean. It can
        // never change what a full repair does.
        let stop_after: Option<usize> = std::env::var("ENGRAMMA_TEST_REPAIR_STOP_AFTER")
            .ok()
            .and_then(|v| v.parse().ok());
        let mut inner_guard = inner;
        let mut moved: Vec<(String, String)> = Vec::new();
        let mut repaired = 0_usize;
        loop {
            let inner = &mut *inner_guard;
            let Some(id) = phantom_ids(&inner.conn)?.into_iter().next() else {
                break;
            };
            // the ids are minted BEFORE the transaction opens (the same
            // order `execute_op` walks: the minter never leans on the
            // transaction, and the borrow stays split by field)
            let (op_id, salt) = mint_ids(inner)?;
            let new_id = mint_ids(inner)?.0;
            let secure_before: i64 = inner
                .conn
                .query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
            inner.conn.pragma_update(None, "secure_delete", "ON")?;
            let tx = inner
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(row) = ops::read_note_row(&tx, &id)? else {
                // nothing to move (a concurrent writer took it): the
                // rule still applies, for idempotence's sake
                ops::purge_state(&tx, &id)?;
                tx.commit()?;
                inner
                    .conn
                    .pragma_update(None, "secure_delete", secure_before)?;
                continue;
            };
            // the content as the state shows it — the COLUMN names the
            // phantom, whatever the row's own header claims
            let mut note = row.note;
            note.id = new_id;
            {
                let mut stmt = tx.prepare("SELECT tag FROM note_tags WHERE note_id = ?1")?;
                note.tags = stmt
                    .query_map([id.to_string()], |r| r.get(0))?
                    .flatten()
                    .collect();
            }
            let logged: Option<String> = tx
                .prepare_cached("SELECT value FROM meta WHERE key = 'hlc'")?
                .query_row([], |r| r.get(0))
                .optional()?;
            if let Some(slogged) = logged
                && let Ok(h) = Hlc::parse(&slogged)
            {
                inner.clock.observe(&h);
            }
            let mut hlc = inner.clock.tick();
            if let Some(signer) = &inner.signer {
                hlc.node = signer.device_id();
            }
            let (prev_seq, prev_hash): (i64, String) = tx
                .prepare_cached("SELECT seq, hash FROM ops ORDER BY seq DESC LIMIT 1")?
                .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap_or((0, ops::GENESIS_HASH.to_string()));
            let mut op = ops::make_op(ops::OpParts {
                op_id,
                hlc,
                actor: "engramma:repair".to_string(),
                kind: OpKind::PutNote,
                note_id: Some(new_id),
                base: None,
                body: serde_json::json!({ "note": note.to_json() }),
                salt: salt.to_vec(),
                device: inner.signer.as_ref().map(|s| s.device_id()),
                sig: None,
            })?;
            if let Some(signer) = &inner.signer {
                op.sig = Some(signer.sign(op.content_hash.as_bytes()));
            }
            op.seq = prev_seq + 1;
            op.seal(&prev_hash);
            ops::apply(&tx, &op)?;
            // the forgetting rule to the old id, as the purge applies it
            ops::purge_state(&tx, &id)?;
            ops::scrub_log(&tx, &id)?;
            material::purge_conflict_copies(&tx, &id)?;
            tx.commit()?;
            inner
                .conn
                .pragma_update(None, "secure_delete", secure_before)?;
            if let Some(st) = &mut inner.index {
                st.idx.remove(&id.to_string());
            }
            moved.push((id.to_string(), new_id.to_string()));
            repaired += 1;
            if stop_after == Some(repaired) {
                // interrupted: the reconciliation stays for the relaunch
                // — it re-applies the rule to the purged ids, and a
                // phantom still waiting must not lose its content to a
                // walk that stopped (every note either as it was or
                // repaired, never halfway — the interruption's own law)
                let inner = &mut *inner_guard;
                inner.graph_dirt.global = true;
                inner.lang_hist_dirty = true;
                return Ok(RepairReport { moved });
            }
        }
        // the J1 reconciliation: the cells the v0.6.9 engine wrote may
        // hold rows born from halves that no longer replay — the
        // co-objects of the dominated operations re-derive, and the
        // stale rows go (idempotent: repairing a healthy cell walks
        // empty worklists)
        {
            let inner = &mut *inner_guard;
            let secure_before: i64 = inner
                .conn
                .query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
            inner.conn.pragma_update(None, "secure_delete", "ON")?;
            let tx = inner
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let purged: Vec<Option<String>> = {
                let mut stmt =
                    tx.prepare("SELECT DISTINCT note_id FROM ops WHERE kind = 'purge'")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.flatten().collect()
            };
            for id in purged.into_iter().flatten() {
                if let Ok(id) = Ulid::from_string(&id) {
                    // the rule, again, on every purged id: the state
                    // rows (a leftover `supersedes` row belongs to the
                    // purged id by `new_id`, and no co-object's
                    // materialization would take it), then the
                    // co-objects' re-derivation — both idempotent
                    ops::purge_state(&tx, &id)?;
                    ops::scrub_log(&tx, &id)?;
                    material::purge_conflict_copies(&tx, &id)?;
                    ops::dominated_comaterialize(&tx, &id)?;
                }
            }
            tx.commit()?;
            inner
                .conn
                .pragma_update(None, "secure_delete", secure_before)?;
        }
        {
            let inner = &mut *inner_guard;
            inner.graph_dirt.global = true;
            inner.lang_hist_dirty = true;
        }
        Ok(RepairReport { moved })
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
/// v0.6.9 (I1): every note id a LOCAL write holds as its OBJECT — the
/// set the purged-id rule refuses. It mirrors [`ops::objects_of`]'s
/// payload extractions (the supersede's NEW note, the decide's target,
/// the revert's restored note) and adds what `objects_of` does not
/// carry: an edge's `note:` endpoints and the note it cites — the
/// rule's «link e unlink da X o verso X». An unlink names its edge by
/// id only, so its endpoints are read from the row.
fn local_write_objects(
    tx: &rusqlite::Transaction<'_>,
    kind: OpKind,
    note_id: Option<Ulid>,
    payload: &Value,
) -> Result<Vec<Ulid>, CellError> {
    let mut ids: Vec<Ulid> = Vec::new();
    let push = |id: Ulid, ids: &mut Vec<Ulid>| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };
    if let Some(id) = note_id {
        push(id, &mut ids);
    }
    let id_of = |v: Option<&Value>| -> Option<Ulid> {
        v.and_then(Value::as_str)
            .and_then(|s| Ulid::from_string(s).ok())
    };
    match kind {
        // the supersede CREATES the new note as much as it closes the
        // old one: both are its objects
        OpKind::Supersede => {
            let new = payload.get("note").and_then(|n| n.get("id"));
            if let Some(id) = id_of(new) {
                push(id, &mut ids);
            }
        }
        // v0.6.11 (K1): a decide is NOT refused over a purged TARGET —
        // the target's half (the closing) is the dominated one, and the
        // proposal's half lands: the decision accepts the text as a NEW
        // note, or rejects it, with nothing to close (the target's
        // existence, when it is not purged, is `decide_locked`'s own
        // word). Every other two-object write keeps the rule: the
        // supersede needs its old note (and closes it), a propose needs
        // a living target, a link needs its endpoints.
        OpKind::Propose => {
            if payload.get("mode").and_then(Value::as_str) == Some("supersede")
                && let Some(id) = id_of(payload.get("target"))
            {
                push(id, &mut ids);
            }
        }
        OpKind::RevertDecision => {
            let restored = payload.get("restore_target").and_then(|t| t.get("id"));
            if let Some(id) = id_of(restored) {
                push(id, &mut ids);
            }
        }
        OpKind::Link => {
            let edge = payload.get("edge");
            for endpoint in [
                edge.and_then(|e| e.get("src")),
                edge.and_then(|e| e.get("dst")),
            ] {
                if let Some(id) = endpoint
                    .and_then(Value::as_str)
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
            // the edge is named by id: its endpoints decide the refusal
            if let Some(edge_id) = payload.get("edge_id").and_then(Value::as_str) {
                let row: Option<(Option<String>, Option<String>, Option<String>)> = tx
                    .query_row(
                        "SELECT src, dst, note_id FROM edges WHERE id = ?1",
                        [edge_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                if let Some((src, dst, cited)) = row {
                    for endpoint in [src, dst] {
                        if let Some(id) = endpoint
                            .as_deref()
                            .and_then(|s| s.strip_prefix("note:"))
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
        }
        _ => {}
    }
    Ok(ids)
}

/// v0.6.9 (I1): the purged-id rule on a connection the caller already
/// holds (propose and decide sit inside the cell's lock when they ask).
fn refuse_purged_ids(conn: &Connection, ids: &[Ulid]) -> Result<(), CellError> {
    for id in ids {
        if ops::note_purged(conn, id)? {
            return Err(CellError::NotePurged(id.to_string()));
        }
    }
    Ok(())
}

/// v0.6.9 (I1): the purged-id rule on the LOCAL write path. The guard
/// of v0.6.7 covers the ARRIVAL and the REPLAY (the purge dominates the
/// set: the note does not materialize) — not the local write, and the
/// reviewer's seed `1207186286826530307` showed the hole: a `put` with
/// a purged id recreated the note in the live state while the replay
/// refused it, and the state stopped being a function of the
/// operations. One rule for write, arrival and replay now: the id
/// never comes back, and the writer is told so, by name (to rewrite
/// similar content takes a NEW note). The purge itself is exempt —
/// forgetting an already-forgotten id stays possible, as it is for a
/// tombstone — and so is the `revert_decision` audit op:
/// `classify_revert` has ALREADY refused every revert that could
/// return purged content (its answer is `RevertOutcome::Purged`), and
/// the op that remains writes bookkeeping (the decisions registry),
/// never a note — v0.5 had this rule before v0.6.9 did.
fn refuse_purged_objects(
    tx: &rusqlite::Transaction<'_>,
    kind: OpKind,
    note_id: Option<Ulid>,
    payload: &Value,
) -> Result<(), CellError> {
    if matches!(kind, OpKind::Purge | OpKind::RevertDecision) {
        return Ok(());
    }
    for id in local_write_objects(tx, kind, note_id, payload)? {
        if ops::note_purged(tx, &id)? {
            return Err(CellError::NotePurged(id.to_string()));
        }
    }
    Ok(())
}

fn execute_op(
    inner: &mut Inner,
    kind: OpKind,
    note_id: Option<Ulid>,
    actor: &str,
    payload: Value,
) -> Result<Ulid, CellError> {
    let (op_id, salt) = mint_ids(inner)?;
    let tx = inner
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    // v0.6.9 (I1): before anything is minted or leaned on, the purged-id
    // rule has its word — every note this write holds as its object must
    // be one the set did not forget
    refuse_purged_objects(&tx, kind, note_id, &payload)?;
    // Inside the write transaction: align with the newest HLC in the log
    // before ticking. Another process may have written since we opened
    // (the daemon and the CLI share a cell); without this, an operation
    // could receive an HLC lower than its predecessor's in the log.
    let logged: Option<String> = tx
        .prepare_cached("SELECT value FROM meta WHERE key = 'hlc'")?
        .query_row([], |r| r.get(0))
        .optional()?;
    if let Some(s) = logged
        && let Ok(h) = Hlc::parse(&s)
    {
        inner.clock.observe(&h);
    }
    let mut hlc = inner.clock.tick();
    // the author enters the identity (v0.6.1, R1): when the signer is
    // armed, the HLC node of a LOCAL operation is the DEVICE id — the
    // clock keeps its physical part and counter, only the node changes.
    // The gate demands `device == hlc.node` on every signed operation;
    // the machine id would name the wrong author forever.
    if let Some(signer) = &inner.signer {
        hlc.node = signer.device_id();
    }
    let (prev_seq, prev_hash): (i64, String) = tx
        .prepare_cached("SELECT seq, hash FROM ops ORDER BY seq DESC LIMIT 1")?
        .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap_or((0, ops::GENESIS_HASH.to_string()));
    // v0.6 (P1): an operation that modifies a note carries `base` — the
    // wire identity of the last operation of that note this author had
    // applied. Two operations with the same base are concurrent.
    // v0.6.5 (E1): what the author had applied is the VISIBLE HEAD — the
    // newest operation on the winner's line, the text that was on the
    // screen — not the newest operation of the note (a header patch above
    // a losing branch never saw the winner, and leaning on it made the
    // loser's copy close without its text living anywhere: the reviewer's
    // R4 and R1). The derivation writes the head (schema 22); the write
    // reads it with one indexed probe.
    let base = if let Some(id) = note_id {
        // a note that does not exist yet has nothing to lean on: the
        // indexed probe skips the join for every fresh note (the common
        // write, and the whole cost of a bulk import — the join alone cost
        // 13% of the insert rate at 10k)
        let exists: i64 = tx
            .prepare_cached("SELECT COUNT(*) FROM notes WHERE id = ?1")?
            .query_row([id.to_string()], |r| r.get(0))?;
        if exists == 0 {
            None
        } else {
            let head: Option<String> = tx
                .prepare_cached("SELECT content_hash FROM note_heads WHERE note_id = ?1")?
                .query_row([id.to_string()], |r| r.get(0))
                .optional()?;
            match head {
                Some(h) => Some(h),
                // no derivation row: the note holds ONE operation (the
                // derivation first writes a head at the second) and the
                // newest operation IS the visible head — today's rule,
                // exact there, and indexed all the same
                None => tx
                    .prepare_cached(
                        "SELECT o.content_hash FROM ops o
                         JOIN op_objects oo ON oo.op_id = o.op_id
                         WHERE oo.object = ?1
                         ORDER BY o.hlc DESC, o.op_id DESC LIMIT 1",
                    )?
                    .query_row([format!("note:{id}")], |r| r.get(0))
                    .optional()?,
            }
        }
    } else {
        None
    };
    // The salt is minted at creation, like the op id and the HLC: `apply`
    // stays deterministic and the digest can be recomputed forever.
    let mut op = ops::make_op(ops::OpParts {
        op_id,
        hlc,
        actor: actor.to_string(),
        kind,
        note_id,
        base,
        body: payload,
        salt: salt.to_vec(),
        device: inner.signer.as_ref().map(|s| s.device_id()),
        sig: None,
    })?;
    if let Some(signer) = &inner.signer {
        op.sig = Some(signer.sign(op.content_hash.as_bytes()));
    }
    op.seq = prev_seq + 1;
    op.seal(&prev_hash);
    ops::apply(&tx, &op)?;
    // v0.6.3 (C1): the conflict set re-derives on a LOCAL write too — the
    // same reconciliation the arrival fast path pays, under the same
    // premise (newest of its note, base at the head). Without this, a
    // conflict copy left by an earlier, smaller set survived on the
    // writing device forever while every receiving copy dropped it: the
    // convergence witnesses of v0.6.2's review.
    material::reconcile_after_local_write(&tx, &op)?;
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
    // The language histogram (v0.5.3, P0) is cheaper to invalidate than to
    // patch: any write may have changed a note's language or status.
    inner.lang_hist_dirty = true;
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
    Ok(op_id)
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
    dump_tables(conn, &state_dump_tables(false))
}

/// The tables of the REPLICATED dump (v0.6, P1): the logical state the
/// operations determine — no arrival chain (`ops`, `op_payloads`: each copy
/// has its own), no local bookkeeping (`meta.hlc` rides with the arrivals).
/// Copies that hold the same SET of operations produce the same digest;
/// this is the comparison the convergence property (and, later, the sync
/// session) speaks.
fn replicated_dump_tables() -> Vec<(&'static str, &'static str)> {
    let mut tables = state_dump_tables(true);
    tables.retain(|(name, _)| !matches!(*name, "ops" | "op_payloads"));
    tables
}

fn dump_tables(conn: &Connection, tables: &[(&str, &str)]) -> Result<String, CellError> {
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

/// The table list of a logical dump. `replicated` drops the keys of `meta`
/// that ride with the arrivals (`hlc`) — everything else a copy must agree
/// on, `cell_id` and the policy included.
fn state_dump_tables(replicated: bool) -> Vec<(&'static str, &'static str)> {
    // `sync_shared` is LOCAL bookkeeping written without an operation (a
    // device decides which cells IT syncs): it never rides with the state,
    // or a copy that joined would fail verify against its own replay
    // (v0.6.1, found by the two-device loopback session). So is
    // `fts_hidden_dirty` (v0.6.7, G1): it remembers THIS file's physical
    // index history — which deletes reached which segments — and two
    // copies holding the same op set may differ in it without diverging.
    let meta_sql = if replicated {
        "SELECT key, value FROM meta
         WHERE key NOT IN ('log_v1_head', 'log_v2_from_seq', 'dump_v13', 'hlc',
                           'sync_shared', 'fts_hidden_dirty') ORDER BY key"
    } else {
        "SELECT key, value FROM meta
         WHERE key NOT IN ('log_v1_head', 'log_v2_from_seq', 'dump_v13',
                           'sync_shared', 'fts_hidden_dirty') ORDER BY key"
    };
    vec![
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
        // v0.5 (P1): the queue and the registry are derived tables the ops
        // themselves maintain — the digest covers them, so a diverging
        // replay of a propose/decide/revert is a verification failure, not
        // a silent drift
        (
            "proposals",
            "SELECT note_id, mode, target, proposer, client, reason, state, proposed_at,
                    decision_op, decided_by, decided_at, decision_reason, review_json,
                    auto_failed
             FROM proposals ORDER BY note_id",
        ),
        (
            "decisions",
            "SELECT op_id, proposal, decision, actor, decided_at, reason, review_json,
                    reverted_by FROM decisions ORDER BY op_id",
        ),
        // The migration bookmarks are not replayable state: no op writes
        // them, so a from-zero rebuild never has them (`dump_v13` marks the
        // v13 digest-widening pass over the compaction rows, v0.5 P1).
        ("meta", meta_sql),
    ]
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

/// The stable frontier (v0.6, P1.5): the minimum ack over the live devices
/// of a SHARED cell; the head of an unshared one (a cell that shares
/// nothing confirms everything to itself).
fn stable_frontier(conn: &Connection) -> Result<i64, CellError> {
    let head: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM ops", [], |r| r.get(0))?;
    let shared: i64 = conn.query_row(
        "SELECT COUNT(*) FROM meta WHERE key = 'sync_shared' AND value = '1'",
        [],
        |r| r.get(0),
    )?;
    if shared == 0 {
        return Ok(head);
    }
    let min_ack: Option<i64> = conn
        .query_row("SELECT MIN(ack_seq) FROM device_acks", [], |r| r.get(0))
        .optional()?;
    Ok(min_ack.unwrap_or(0))
}

/// blake3 of the canonical dump of the REPLICATED tables (v0.6, P1): the
/// digest copies compare — the same set of operations must produce the
/// same value on every copy, arrival order notwithstanding.
///
/// # Errors
/// [`CellError::Db`] on SQL failure.
pub fn replicated_digest(conn: &Connection) -> Result<String, CellError> {
    Ok(
        blake3::hash(dump_tables(conn, &replicated_dump_tables())?.as_bytes())
            .to_hex()
            .to_string(),
    )
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

    /// v0.4.3 (P1.2): retire closes the real connection while clones of the
    /// handle are still alive — the WAL and the shm go back to the file
    /// system NOW, not at the last clone's drop (a JS object can wait for
    /// its GC for hours).
    #[test]
    fn retire_closes_the_connection_under_live_clones() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("r.sqlite");
        let cell = Cell::create(&path, "t", "t").unwrap(/* test-only */);
        for i in 1..=3_u16 {
            cell.put_note(&note(i, "una nota che produce wal"), "t")
                .unwrap(/* test-only */);
        }
        let mut wal = path.clone().into_os_string();
        wal.push("-wal");
        let wal = PathBuf::from(wal);
        assert!(wal.exists(), "the fixture must leave a WAL behind");
        let mut shm = path.clone().into_os_string();
        shm.push("-shm");
        // a clone keeps Inner alive past the retire — like a Cell object in
        // JS that the GC has not collected yet
        let clone = cell.clone();
        cell.retire().unwrap(/* test-only */);
        assert!(!wal.exists(), "the WAL stayed open under a live clone");
        assert!(
            !PathBuf::from(shm).exists(),
            "the shm stayed open under a live clone"
        );
        drop(clone);
    }

    /// v0.4.3 (P0.1): the one snapshot recognizer — only the exact compact
    /// format, never a neighboring cell whose name merely looks like one.
    #[test]
    fn snapshot_of_recognizes_only_the_compact_format() {
        assert_eq!(snapshot_of("storia-snap-2.sqlite"), Some(("storia", 2)));
        // the LAST `-snap-` splits: a cell may be called `a-snap-b`
        assert_eq!(snapshot_of("a-snap-b-snap-3.sqlite"), Some(("a-snap-b", 3)));
        // a neighboring CELL, not a snapshot (the suffix is not all digits)
        assert_eq!(snapshot_of("storia-snap-archivio.sqlite"), None);
        assert_eq!(snapshot_of("storia-snap-x7.sqlite"), None);
        // not the sidecars, not the plain cell, not an empty cell name
        assert_eq!(snapshot_of("storia-snap-2.sqlite-wal"), None);
        assert_eq!(snapshot_of("storia-snap-2.sqlite-shm"), None);
        assert_eq!(snapshot_of("storia.sqlite"), None);
        assert_eq!(snapshot_of("-snap-7.sqlite"), None);
        assert_eq!(snapshot_of("storia-snap-.sqlite"), None);
        assert_eq!(snapshot_of("storia-snap-99999999999999999999.sqlite"), None);
    }

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
        // a full replace no longer REFUSES: v0.4.1 (P0.1) a rewrite preserves
        // the state, and a preserved `superseded` keeps the closing the old
        // version had — the invariant survives the rewrite
        let mut again = old.clone();
        again.status = Status::Superseded;
        again.valid_to = None;
        cell.put_note(&again, "tester").unwrap(/* test-only */);
        // the note keeps its state and its closed validity
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
        // forget the FIRST put: its effect is fully replaced by the second.
        // v0.6.1 (P0): forgetting marks the row `redacted` — the state and
        // the payload must agree.
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
            inner
                .conn
                .execute(
                    "UPDATE ops SET payload_state = 'redacted' WHERE op_id = (
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
    /// the NEW note (content out, id kept, closing of the old note replayable
    /// in the incremental order), and the chain is recomputed over the
    /// censored digests. v0.6.10 (J1): the purge dominates the WHOLE
    /// operation — the old note's closing goes with the new note's content.
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
        // content is gone, its ID stays (J1: the index of a receiving
        // copy needs it), the closing of `old` stays replayable for the
        // incremental order only
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
        assert_eq!(
            sup.body.as_ref().and_then(|b| b.get("new_id")),
            Some(&serde_json::Value::String(new.id.to_string())),
            "the new note's id survives the censor"
        );
        let vr = cell.verify().unwrap(/* test-only */);
        assert!(vr.ok(), "{vr:?}");
        // v0.6.10 (J1): the dominated supersede closes nothing — the old
        // note is open again, its validity with it
        let back = cell.get(&old.id).unwrap(/* test-only */);
        assert_eq!(back.status, Status::Active);
        assert!(back.valid_to.is_none());
        // and the new note is gone
        assert!(matches!(cell.get(&new.id), Err(CellError::NoteNotFound(_))));
    }

    /// v0.3 (P3): compact, write more, verify — the chain is whole, the
    /// payloads up to the snapshot are gone, the tail replays on it.
    /// v0.3.2 (P0.1): the default compaction is a checkpoint without a file.
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
        let report = cell
            .compact(head - 1, &CompactOptions { keep_snapshot: true })
            .unwrap(/* test-only */);
        let snap = report
            .snapshot
            .expect(/* test-only */ "keep_snapshot writes a file");
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
        let vr = cell.verify().unwrap(/* test-only */);
        assert!(vr.ok(), "verify after compaction: {:?}", vr.mismatch);
        assert_eq!(vr.replay, Replay::FromSnapshot { seq: head - 1 });
        // compacting outside the live log is refused
        assert!(cell.compact(head + 5, &CompactOptions::default()).is_err());
        assert!(cell.compact(0, &CompactOptions::default()).is_err());
    }

    /// v0.3.2 (P0.1): the checkpoint compaction keeps `changes_since`
    /// compacted below the cut too, and verify says `ChainOnly`.
    #[test]
    fn checkpoint_compact_compacts_changes_since() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("k.sqlite");
        let cell = Cell::create(&path, "t", "t").unwrap(/* test-only */);
        for i in 1..=4_u16 {
            cell.put_note(&note(i, &format!("nota {i} [[Roma]]")), "t")
                .unwrap(/* test-only */);
        }
        let head: i64 = cell
            .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
            .unwrap(/* test-only */)[0]
            .parse()
            .unwrap(/* test-only */);
        let report = cell
            .compact(head - 1, &CompactOptions::default())
            .unwrap(/* test-only */);
        assert!(report.snapshot.is_none());
        let err = cell.changes_since(head - 2).unwrap_err(/* t */);
        assert!(matches!(err, CellError::Compacted { snapshot_seq } if snapshot_seq == head - 1));
        assert!(cell.changes_since(head - 1).is_ok());
        let vr = cell.verify().unwrap(/* test-only */);
        assert!(vr.ok(), "{vr:?}");
        assert_eq!(
            vr.replay,
            Replay::ChainOnly {
                since_seq: head - 1
            }
        );
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
        let snap = cell
            .compact(head, &CompactOptions { keep_snapshot: true })
            .unwrap(/* test-only */)
            .snapshot
            .expect(/* test-only */ "keep_snapshot writes a file");
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
