// SPDX-License-Identifier: Apache-2.0

//! Derived vector storage (schema v4, DESIGN §6.2): what is embedded, when a
//! note is stale, and the blue/green model switch.
//!
//! Nothing here ever enters `ops`, the replay, `compare_states` or the
//! export. Writes never compute embeddings (the insert-rate objective stays
//! untouched): notes become *pending* and [`Cell::embed_pending`] works
//! through them in batches.

use rusqlite::Connection;
use ulid::Ulid;

use crate::embed::{Embedder, TextKind, chunk_note, content_hash, encode_vec, note_hash};
use crate::error::CellError;

/// How a model stands in a cell (blue/green, DESIGN §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelState {
    /// Being filled by `embed_pending`; not yet serving searches.
    Building,
    /// Serving semantic searches.
    Ready,
    /// Replaced; its vectors are gone (or going).
    Retired,
}

impl ModelState {
    /// Parse the stored string.
    ///
    /// # Errors
    /// [`CellError::Db`] on unknown values.
    pub fn parse(s: &str) -> Result<Self, CellError> {
        match s {
            "building" => Ok(ModelState::Building),
            "ready" => Ok(ModelState::Ready),
            "retired" => Ok(ModelState::Retired),
            other => Err(CellError::Db(format!("unknown model state `{other}`"))),
        }
    }

    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ModelState::Building => "building",
            ModelState::Ready => "ready",
            ModelState::Retired => "retired",
        }
    }
}

/// One row of `embedding_models`.
#[derive(Debug, Clone)]
pub struct ModelRow {
    /// Full model id (`<name>@<revision8>`).
    pub model: String,
    /// Vector dimension.
    pub dim: i64,
    /// Lifecycle state.
    pub state: ModelState,
    /// When the row was created.
    pub created_at: String,
}

/// Coverage of one model, as reported by `verify`.
#[derive(Debug, Clone)]
pub struct ModelCoverage {
    /// Full model id.
    pub model: String,
    /// Lifecycle state.
    pub state: ModelState,
    /// Living notes with an up-to-date vector.
    pub covered: usize,
    /// Living notes.
    pub living: usize,
}

/// Outcome of one `embed_pending` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedPendingReport {
    /// Notes embedded in this call.
    pub done: usize,
    /// Notes still waiting after this call.
    pub remaining: usize,
    /// Set when a building model reached 100% and became the ready model.
    pub promoted: Option<String>,
}

/// Notes per embedding batch (one `embed()` call, one transaction).
pub(crate) const BATCH: usize = 32;

/// Test-only counter of note rows read by the pending scans (v0.2.1: the
/// proof that a batch reads its own rows, not the whole cell's).
#[cfg(test)]
pub(crate) static ROWS_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
fn count_row() {
    ROWS_READ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(test))]
fn count_row() {}

pub(crate) struct VectorTx<'a> {
    pub(crate) conn: &'a Connection,
}

impl VectorTx<'_> {
    /// Register a model for this cell, `building` (blue/green start).
    /// Re-registering the building model is a no-op; a different building
    /// model must be finished first.
    ///
    /// # Errors
    /// [`CellError::Invalid`] when another model is already building.
    pub fn add_model(&self, model_id: &str, dim: usize) -> Result<(), CellError> {
        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT model, state FROM embedding_models WHERE model = ?1",
                [model_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        match existing {
            Some((m, state)) if m == model_id && state != "retired" => Ok(()),
            _ => {
                let building: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT model FROM embedding_models WHERE state = 'building'",
                        [],
                        |r| r.get(0),
                    )
                    .ok();
                if let Some(other) = building
                    && other != model_id
                {
                    return Err(CellError::Invalid(format!(
                        "`{other}` is already building: finish it (`engramma embed`) or pick it"
                    )));
                }
                self.conn.execute(
                    "INSERT OR REPLACE INTO embedding_models (model, dim, state, created_at)
                     VALUES (?1, ?2, 'building', ?3)",
                    rusqlite::params![model_id, i64::try_from(dim).unwrap_or(0), now_iso(),],
                )?;
                Ok(())
            }
        }
    }

    /// Every registered model, id order.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn models(&self) -> Result<Vec<ModelRow>, CellError> {
        let mut stmt = self
            .conn
            .prepare("SELECT model, dim, state, created_at FROM embedding_models ORDER BY model")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (model, dim, state, created_at) = row?;
            out.push(ModelRow {
                model,
                dim,
                state: ModelState::parse(&state)?,
                created_at,
            });
        }
        Ok(out)
    }

    /// The model semantic search should serve: the `ready` one, if any.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn ready_model(&self) -> Result<Option<ModelRow>, CellError> {
        let mut stmt = self
            .conn
            .prepare("SELECT model, dim, state, created_at FROM embedding_models WHERE state = 'ready' ORDER BY created_at, model LIMIT 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next().map_err(CellError::from)? {
            return Ok(Some(ModelRow {
                model: row.get(0)?,
                dim: row.get(1)?,
                state: ModelState::Ready,
                created_at: row.get(3)?,
            }));
        }
        Ok(None)
    }

    /// The model `embed_pending` fills: the building one, else the ready one.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn embed_target(&self) -> Result<Option<ModelRow>, CellError> {
        let mut stmt = self
            .conn
            .prepare("SELECT model, dim, state, created_at FROM embedding_models WHERE state = 'building' LIMIT 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next().map_err(CellError::from)? {
            return Ok(Some(ModelRow {
                model: row.get(0)?,
                dim: row.get(1)?,
                state: ModelState::Building,
                created_at: row.get(3)?,
            }));
        }
        self.ready_model()
    }

    /// One positional window of living notes after `after_id`, keyset-paged
    /// (`WHERE id > ?last ORDER BY id LIMIT ?take`) with the stored chunk-0
    /// `note_hash` LEFT JOINed in. The hash is computed **only for the rows
    /// of the window** (v0.2.1: the v0.2 scan re-read every note and every
    /// vector row per 32-note batch — O(N²/32), 278 s at 100k notes).
    /// Returns the pending notes of the window, the id of its last row read
    /// (the next cursor, pending or not), and whether the table ended inside
    /// the window.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub(crate) fn pending_window(
        &self,
        model: &str,
        after_id: &str,
        take: usize,
        force: bool,
    ) -> Result<(Vec<PendingNote>, Option<String>, bool), CellError> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.title, n.body, v.note_hash
             FROM notes n
             LEFT JOIN note_vectors v
                    ON v.note_id = n.id AND v.model = ?1 AND v.chunk = 0
             WHERE n.status != 'deleted' AND n.id > ?2
             ORDER BY n.id
             LIMIT ?3",
        )?;
        let take_i64 = i64::try_from(take.max(1)).unwrap_or(i64::MAX);
        let rows = stmt.query_map(rusqlite::params![model, after_id, take_i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut batch = Vec::new();
        let mut cursor: Option<String> = None;
        let mut read = 0_usize;
        for row in rows {
            let (id, title, body, stored) = row?;
            count_row();
            read += 1;
            let fresh =
                stored.as_deref() == Some(note_hash(model, title.as_deref(), &body).as_str());
            cursor = Some(id.clone()); // the last row read, pending or not
            if force || !fresh {
                batch.push(PendingNote { id, title, body });
            }
        }
        drop(stmt);
        let exhausted = read < take;
        Ok((batch, cursor, exhausted))
    }

    /// The exact number of pending notes for `model`: one streaming pass,
    /// hashing every living note once. Called once per `embed_pending` (the
    /// report's `remaining` is this count, decremented per embedded note).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub(crate) fn pending_count(&self, model: &str) -> Result<usize, CellError> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.title, n.body, v.note_hash
             FROM notes n
             LEFT JOIN note_vectors v
                    ON v.note_id = n.id AND v.model = ?1 AND v.chunk = 0
             WHERE n.status != 'deleted'
             ORDER BY n.id",
        )?;
        let rows = stmt.query_map(rusqlite::params![model], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut n = 0_usize;
        for row in rows {
            let (_id, title, body, stored) = row?;
            count_row();
            let fresh =
                stored.as_deref() == Some(note_hash(model, title.as_deref(), &body).as_str());
            if !fresh {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Coverage per model (verify report; not a verification error): a
    /// **count query**, not the pending scan — `covered` means "has an
    /// up-to-date-or-not chunk-0 vector" (presence); staleness is
    /// `embed_pending`'s business and never a verification failure.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn coverage(&self) -> Result<Vec<ModelCoverage>, CellError> {
        let living: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE status != 'deleted'",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize;
        let mut out = Vec::new();
        for m in self.models()? {
            let covered = if m.state == ModelState::Retired {
                0
            } else {
                self.conn.query_row(
                    "SELECT COUNT(*) FROM note_vectors v
                         JOIN notes n ON n.id = v.note_id AND n.status != 'deleted'
                         WHERE v.model = ?1 AND v.chunk = 0",
                    [&m.model],
                    |r| r.get::<_, i64>(0),
                )? as usize
            };
            out.push(ModelCoverage {
                model: m.model,
                state: m.state,
                covered,
                living,
            });
        }
        Ok(out)
    }

    /// Write the vectors of one note (replacing its previous chunks).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn write_note_vectors(
        &self,
        model: &str,
        note_id: &str,
        title: Option<&str>,
        body: &str,
        chunks: &[(String, Vec<f32>)],
    ) -> Result<(), CellError> {
        self.conn.execute(
            "DELETE FROM note_vectors WHERE model = ?1 AND note_id = ?2",
            [model, note_id],
        )?;
        for (i, (chunk_text, vec)) in chunks.iter().enumerate() {
            let chunk: i64 = i64::try_from(i).unwrap_or(0);
            self.conn.execute(
                "INSERT INTO note_vectors (note_id, chunk, model, content_hash, note_hash, vec)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    note_id,
                    chunk,
                    model,
                    content_hash(model, chunk_text),
                    note_hash(model, title, body),
                    encode_vec(vec),
                ],
            )?;
        }
        Ok(())
    }

    /// Blue/green swap: when `model` covers every living note, it becomes the
    /// single `ready` model and the previous one is retired with its vectors
    /// deleted — all in one transaction. Returns the promoted model. The
    /// completeness check is the exact pending count (one pass).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub(crate) fn promote_if_complete(&self, model: &str) -> Result<Option<String>, CellError> {
        if self.pending_count(model)? > 0 {
            return Ok(None);
        }
        let old: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT model FROM embedding_models WHERE state = 'ready'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.flatten().collect()
        };
        for o in &old {
            self.conn.execute(
                "UPDATE embedding_models SET state = 'retired' WHERE model = ?1",
                [o],
            )?;
            self.conn
                .execute("DELETE FROM note_vectors WHERE model = ?1", [o])?;
        }
        self.conn.execute(
            "UPDATE embedding_models SET state = 'ready' WHERE model = ?1",
            [model],
        )?;
        Ok(Some(model.to_string()))
    }
}

/// One chunk with its text and its vector.
pub type ChunkVec = (String, Vec<f32>);

/// The embedding output for one note: its id and its chunks.
pub type EmbeddedNote = (String, Vec<ChunkVec>);

/// A note waiting for its vectors.
#[derive(Debug, Clone)]
pub struct PendingNote {
    /// Note id.
    pub id: String,
    /// Title, when present.
    pub title: Option<String>,
    /// Body.
    pub body: String,
}

fn now_iso() -> String {
    // Wall clock in a derived table: never part of the log, precision is irrelevant.
    crate::format::At(time::OffsetDateTime::now_utc()).to_db_string()
}

/// Embed `notes` with `embedder` in one batch call, chunking each note.
/// Returns `(note_id, chunks)` pairs ready for storage.
pub(crate) fn embed_notes(
    embedder: &dyn Embedder,
    notes: &[PendingNote],
) -> Result<Vec<EmbeddedNote>, CellError> {
    let max_tokens = embedder.max_tokens();
    let approx = |t: &str| embedder.approx_tokens(t);
    let mut texts: Vec<String> = Vec::new();
    let mut spans: Vec<(usize, usize, usize)> = Vec::new(); // (note idx, chunk offset, chunk count)
    for (i, n) in notes.iter().enumerate() {
        let chunks = chunk_note(n.title.as_deref(), &n.body, max_tokens, &approx);
        spans.push((i, texts.len(), chunks.len()));
        texts.extend(chunks);
    }
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let vectors = embedder
        .embed(&refs, TextKind::Document)
        .map_err(|e| CellError::Invalid(format!("embedding with {}: {e}", embedder.model_id())))?;
    if vectors.len() != texts.len() {
        return Err(CellError::Invalid(format!(
            "{} returned {} vectors for {} chunks",
            embedder.model_id(),
            vectors.len(),
            texts.len()
        )));
    }
    let dim = embedder.dim();
    let mut out: Vec<EmbeddedNote> = Vec::with_capacity(notes.len());
    for (i, start, count) in spans {
        let mut chunks = Vec::with_capacity(count);
        for (offset, v) in vectors[start..start + count].iter().enumerate() {
            if v.len() != dim {
                return Err(CellError::Invalid(format!(
                    "{} returned a {}-dim vector (want {dim})",
                    embedder.model_id(),
                    v.len()
                )));
            }
            chunks.push((texts[start + offset].clone(), v.clone()));
        }
        out.push((notes[i].id.clone(), chunks));
    }
    Ok(out)
}

/// Tombstone cleanup (called from the delete op): a deleted note has no
/// vectors, in any model.
pub(crate) fn delete_note_vectors(conn: &Connection, note_id: &Ulid) -> Result<(), CellError> {
    conn.execute(
        "DELETE FROM note_vectors WHERE note_id = ?1",
        [note_id.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Cell;
    use crate::embed::{TextKind, l2_normalize};
    use crate::format::{Note, NoteType};
    use time::OffsetDateTime;
    use ulid::Ulid;

    /// A deterministic test embedder: 8-dim vectors hashed from the text.
    struct FakeEmbedder {
        id: String,
        dim: usize,
    }

    impl Embedder for FakeEmbedder {
        fn model_id(&self) -> &str {
            &self.id
        }
        fn dim(&self) -> usize {
            self.dim
        }
        fn max_tokens(&self) -> usize {
            8_192
        }
        fn embed(
            &self,
            texts: &[&str],
            _kind: TextKind,
        ) -> Result<Vec<Vec<f32>>, crate::error::EmbedError> {
            Ok(texts
                .iter()
                .map(|t| {
                    let h = blake3::hash(t.as_bytes());
                    let mut v: Vec<f32> = h.as_bytes()[..8]
                        .iter()
                        .map(|b| f32::from(*b) / 255.0 - 0.5)
                        .collect();
                    l2_normalize(&mut v);
                    v
                })
                .collect())
        }
    }

    fn at() -> crate::format::At {
        crate::format::At(
            OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* test-only */),
        )
    }

    fn note(n: u16, body: &str) -> Note {
        Note {
            id: Ulid::from_string(&format!("01{:024}", n)).unwrap(/* test-only */),
            note_type: NoteType::Fact,
            title: Some(format!("nota {n}")),
            body: body.to_string(),
            tags: vec![],
            scope: None,
            lang: None,
            source: "test".into(),
            confidence: crate::format::Confidence::Stated,
            status: crate::format::Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: at(),
            updated_at: at(),
            extensions: vec![],
        }
    }

    fn cell_with_notes() -> (tempfile::TempDir, Cell, Vec<Note>) {
        let dir = tempfile::Builder::new().prefix("engramma-test-").tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("v.sqlite"), "t", "tester")
            .unwrap(/* test-only */);
        let notes: Vec<Note> = (1..=5)
            .map(|i| note(i, &format!("corpo {i} su [[Milano]]")))
            .collect();
        for n in &notes {
            cell.put_note(n, "t").unwrap(/* test-only */);
        }
        (dir, cell, notes)
    }

    #[test]
    fn embed_pending_fills_promotes_and_reports_coverage() {
        let (_d, cell, notes) = cell_with_notes();
        let e = FakeEmbedder {
            id: "fake@1".into(),
            dim: 8,
        };
        assert!(cell.models().unwrap(/* test-only */).is_empty());
        cell.add_model("fake@1", 8).unwrap(/* test-only */);
        let report = cell.embed_pending(&e, 100).unwrap(/* test-only */);
        assert_eq!(report.done, 5);
        assert_eq!(report.remaining, 0);
        assert_eq!(report.promoted.as_deref(), Some("fake@1"));
        // every note has its 8-float vector (scoped: the lock guard must be
        // gone before any other Cell call)
        for n in &notes {
            let rows = cell
                .debug_column(&format!(
                    "SELECT CAST(LENGTH(vec) AS TEXT) FROM note_vectors \
                     WHERE model = 'fake@1' AND note_id = '{}'",
                    n.id
                ))
                .unwrap(/* test-only */);
            assert_eq!(rows, vec!["32".to_string()], "one 8-float vector");
        }
        let models = cell.models().unwrap(/* test-only */);
        assert_eq!(models[0].state, ModelState::Ready);
        // verify reports coverage and stays ok — vectors are derived data
        let v = cell.verify().unwrap(/* test-only */);
        assert!(v.ok(), "{v:?}");
        assert_eq!(v.coverage.len(), 1);
        assert_eq!(v.coverage[0].covered, 5);
        assert_eq!(v.coverage[0].living, 5);
    }

    #[test]
    fn same_log_with_and_without_vectors_verifies() {
        // The regression the prompt asks for: two cells holding the same
        // logical content, one with vectors and one without — verify ok on both.
        let (_d1, a, notes) = cell_with_notes();
        let e = FakeEmbedder {
            id: "fake@1".into(),
            dim: 8,
        };
        a.add_model("fake@1", 8).unwrap(/* test-only */);
        a.embed_pending(&e, 100).unwrap(/* test-only */);
        assert!(a.verify().unwrap(/* test-only */).ok());

        let d2 = tempfile::Builder::new().prefix("engramma-test-").tempdir().unwrap(/* test-only */);
        let b = Cell::create(&d2.path().join("b.sqlite"), "t", "tester")
            .unwrap(/* test-only */);
        for n in &notes {
            b.put_note(n, "t").unwrap(/* test-only */);
        }
        assert!(b.verify().unwrap(/* test-only */).ok());
        assert!(b.verify().unwrap(/* test-only */).coverage.is_empty());
    }

    #[test]
    fn blue_green_keeps_serving_until_the_swap() {
        let (_d, cell, _notes) = cell_with_notes();
        let a = FakeEmbedder {
            id: "a@1".into(),
            dim: 8,
        };
        let b = FakeEmbedder {
            id: "b@2".into(),
            dim: 8,
        };
        cell.add_model("a@1", 8).unwrap(/* test-only */);
        cell.embed_pending(&a, 100).unwrap(/* test-only */);
        // a second building model while a@1 serves
        cell.add_model("b@2", 8).unwrap(/* test-only */);
        // a third model cannot build at the same time
        assert!(cell.add_model("c@3", 8).is_err());
        // partial embedding: b@2 still building, a@1 still ready
        let report = cell.embed_pending(&b, 3).unwrap(/* test-only */);
        assert_eq!(report.done, 3);
        assert_eq!(report.remaining, 2);
        assert!(report.promoted.is_none(), "no swap before 100%");
        let states: Vec<(String, ModelState)> = cell
            .models()
            .unwrap(/* test-only */)
            .into_iter()
            .map(|m| (m.model.clone(), m.state))
            .collect();
        assert!(states.contains(&("a@1".into(), ModelState::Ready)));
        assert!(states.contains(&("b@2".into(), ModelState::Building)));
        // an embedder for the wrong model is refused
        assert!(cell.embed_pending(&a, 1).is_err());
        // finish: atomic swap — b@2 ready, a@1 retired with its vectors gone
        let report = cell.embed_pending(&b, 100).unwrap(/* test-only */);
        assert_eq!(report.promoted.as_deref(), Some("b@2"));
        let old_rows: i64 = cell
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM note_vectors WHERE model = 'a@1'")
            .unwrap(/* test-only */)
            .first()
            .and_then(|s| s.parse().ok())
            .unwrap_or(-1);
        assert_eq!(old_rows, 0, "retired vectors must be deleted");
        {
            let inner = cell.inner.lock().unwrap(/* test-only */);
            let vx = VectorTx { conn: &inner.conn };
            assert_eq!(
                vx.ready_model().unwrap(/* test-only */).unwrap(/* test-only */).model,
                "b@2"
            );
        }
        assert!(cell.verify().unwrap(/* test-only */).ok());
    }

    #[test]
    fn edited_note_is_pending_again_and_tombstones_lose_vectors() {
        let (_d, cell, notes) = cell_with_notes();
        let e = FakeEmbedder {
            id: "fake@1".into(),
            dim: 8,
        };
        cell.add_model("fake@1", 8).unwrap(/* test-only */);
        cell.embed_pending(&e, 100).unwrap(/* test-only */);
        // an edit makes exactly one note stale
        cell.patch_note(
            &notes[0].id,
            &crate::cell::NotePatch {
                body: Some("corpo del tutto nuovo su [[Torino]]".into()),
                ..Default::default()
            },
            "t",
        )
        .unwrap(/* test-only */);
        let remaining = {
            let inner = cell.inner.lock().unwrap(/* test-only */);
            let vx = VectorTx { conn: &inner.conn };
            vx.pending_count("fake@1").unwrap(/* test-only */)
        };
        assert_eq!(remaining, 1);
        let report = cell.embed_pending(&e, 100).unwrap(/* test-only */);
        assert_eq!(report.done, 1);
        // a tombstone has no vectors
        cell.delete_note(&notes[1].id, "t").unwrap(/* test-only */);
        let rows: i64 = cell
            .debug_column(
                &format!(
                    "SELECT CAST(COUNT(*) AS TEXT) FROM note_vectors WHERE note_id = '{}'",
                    notes[1].id
                ),
            )
            .unwrap(/* test-only */)
            .first()
            .and_then(|s| s.parse().ok())
            .unwrap_or(-1);
        assert_eq!(rows, 0);
        let v = cell.verify().unwrap(/* test-only */);
        assert!(v.ok(), "{v:?}");
        assert_eq!(v.coverage[0].covered, 4);
        assert_eq!(v.coverage[0].living, 4);
    }

    #[test]
    fn embed_all_reembeds_even_fresh_notes() {
        let (_d, cell, _notes) = cell_with_notes();
        let e = FakeEmbedder {
            id: "fake@1".into(),
            dim: 8,
        };
        cell.add_model("fake@1", 8).unwrap(/* test-only */);
        cell.embed_pending(&e, 100).unwrap(/* test-only */);
        let report = cell.embed_all(&e, 100).unwrap(/* test-only */);
        assert_eq!(report.done, 5, "force re-embeds every living note");
    }

    /// The same 300 notes in two identical cells, all pending: a call that
    /// embeds 192 must read only ~128 more note rows than a call that embeds
    /// 64 (its own windows) — the v0.2 scan re-read the whole cell per
    /// 32-note batch, which at these sizes would mean ≥ 2×300 extra rows.
    #[test]
    fn embed_pending_reads_rows_per_window_not_per_cell() {
        let e = FakeEmbedder {
            id: "fake@1".into(),
            dim: 8,
        };
        let measure = |max: usize| -> (usize, u64) {
            let dir = tempfile::Builder::new().prefix("engramma-test-").tempdir().unwrap(/* test-only */);
            let cell = Cell::create(&dir.path().join("rows.sqlite"), "t", "tester")
                .unwrap(/* test-only */);
            let notes: Vec<Note> = (1..=300)
                .map(|i| note(i as u16, &format!("corpo {i}")))
                .collect();
            for n in &notes {
                cell.put_note(n, "t").unwrap(/* test-only */);
            }
            cell.add_model("fake@1", 8).unwrap(/* test-only */);
            ROWS_READ.store(0, std::sync::atomic::Ordering::Relaxed);
            let report = cell.embed_pending(&e, max).unwrap(/* test-only */);
            let rows = ROWS_READ.load(std::sync::atomic::Ordering::Relaxed);
            (report.done, rows)
        };
        let (done64, rows64) = measure(64);
        let (done192, rows192) = measure(192);
        assert_eq!((done64, done192), (64, 192));
        // expected: 300 (initial count) + ~64/192 window rows; the quadratic
        // scan would add ≥ 300 rows per extra batch of 32
        assert!(
            rows192 - rows64 < 200,
            "rows for 192 vs 64 embedded: {rows192} vs {rows64}"
        );
        assert!(rows64 < 500, "one count pass + one window: {rows64}");
    }

    /// Notes edited or added between calls are picked up again (the cursor
    /// is per-call: each call re-walks from the first id), and a tombstone
    /// between calls never re-enters the pending set.
    #[test]
    fn edits_between_calls_are_resumed_not_lost() {
        let (_d, cell, notes) = cell_with_notes();
        let e = FakeEmbedder {
            id: "fake@1".into(),
            dim: 8,
        };
        cell.add_model("fake@1", 8).unwrap(/* test-only */);
        let full = cell.embed_pending(&e, 100).unwrap(/* test-only */);
        assert_eq!((full.done, full.remaining), (5, 0));
        // one note already embedded is edited, one new note arrives
        cell.patch_note(
            &notes[0].id,
            &crate::cell::NotePatch {
                body: Some("un corpo del tutto nuovo".into()),
                ..Default::default()
            },
            "t",
        )
        .unwrap(/* test-only */);
        let extra = note(9, "nuova arrivata");
        cell.put_note(&extra, "t").unwrap(/* test-only */);
        let again = cell.embed_pending(&e, 100).unwrap(/* test-only */);
        assert_eq!((again.done, again.remaining), (2, 0));
        // and a deleted note does not come back
        cell.delete_note(&extra.id, "t").unwrap(/* test-only */);
        let third = cell.embed_pending(&e, 100).unwrap(/* test-only */);
        assert_eq!((third.done, third.remaining), (0, 0));
    }

    /// A partial call (max < pending) reports the true tail; the next call
    /// continues from where the table stands — the keyset cursor must not
    /// skip or repeat notes across calls.
    #[test]
    fn partial_calls_walk_the_whole_table_across_calls() {
        let (_d, cell, _notes) = cell_with_notes();
        let e = FakeEmbedder {
            id: "fake@1".into(),
            dim: 8,
        };
        cell.add_model("fake@1", 8).unwrap(/* test-only */);
        let mut total = 0_usize;
        loop {
            let r = cell.embed_pending(&e, 2).unwrap(/* test-only */);
            total += r.done;
            if r.remaining == 0 || r.done == 0 {
                break;
            }
        }
        assert_eq!(total, 5, "two per call, nothing skipped or doubled");
        let covered = cell
            .debug_column(
                "SELECT CAST(COUNT(*) AS TEXT) FROM note_vectors WHERE model = 'fake@1'",
            )
            .unwrap(/* test-only */)
            .first()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        assert_eq!(covered, 5);
    }
}
