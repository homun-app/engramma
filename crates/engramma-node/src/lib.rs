// SPDX-License-Identifier: Apache-2.0

//! Node.js (N-API) binding of the Engramma memory engine — v0.4 (P2.7).
//!
//! The JS contract is PUBLIC (Homun's next phase uses it as-is): two
//! classes, [`Engine`] and `Cell`, every disk- or model-touching method
//! asynchronous on libuv's pool (`AsyncTask`) — the daemon's event loop
//! never blocks. Errors are JS `Error`s whose `code` is stable (`NOT_FOUND`,
//! `QUARANTINED`, `INVALID_NOTE`, `CONFLICT`, `IO`, `MODEL_MISSING`,
//! `COMPACTED`), and whose messages never carry note contents — a missing
//! note and a quarantined one read the same from the outside (the closed
//! oracle of v0.1.1).
//!
//! How the `code` travels: `AsyncTask`'s error type is napi's own, whose
//! `code` is fixed by its status — so the compute side prefixes the message
//! with `[CODE]`, and every task's `reject` rebuilds the error as a real
//! JS object with the right `code` property. napi settles a retained error
//! object verbatim (its own async-work code says so), so what reaches
//! `catch (e)` has `e.code` and `e.message` as documented.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use napi::bindgen_prelude::*;
use napi::{Env, Error, Result, Task};
use napi_derive::napi;
use time::OffsetDateTime;
use ulid::Ulid;

use engramma_core::Cell as CoreCell;
use engramma_core::Status as NoteStatus;
use engramma_core::format::{At, Note, NoteType};
use engramma_core::search::{Mode, Query, Semantic};
use engramma_core::{
    CellError, Confidence, Embedder, NotePatch, PurgeReport, SearchError, Validity, VerifyReport,
};

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// The stable code, carried in the message prefix across the async fence.
fn transport_err(code: &'static str, message: impl std::fmt::Display) -> Error {
    Error::new(napi::Status::GenericFailure, format!("[{code}] {message}"))
}

/// Split a transported error back into `(code, message)`.
fn split_transport(err: &Error) -> (&'static str, String) {
    let reason = err.reason.clone();
    match reason.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((code, rest)) => {
            let known = [
                "NOT_FOUND",
                "QUARANTINED",
                "INVALID_NOTE",
                "CONFLICT",
                "IO",
                "MODEL_MISSING",
                "COMPACTED",
            ];
            let c: &str = code;
            match known.iter().find(|k| **k == c) {
                Some(k) => (k, rest.trim_start().to_string()),
                None => ("IO", reason),
            }
        }
        None => ("IO", reason),
    }
}

/// Rebuild a transported error as a JS `Error` with the stable `code`
/// property. All safe napi APIs: `create_error` makes the object, the
/// property is set, and `Error::from(unknown)` retains it — napi settles a
/// retained value verbatim.
fn recode(env: Env, err: Error) -> Error {
    let (code, message) = split_transport(&err);
    let build = || -> Result<Error> {
        let mut obj = env.create_error(Error::new(napi::Status::GenericFailure, message))?;
        obj.set_named_property("code", code)?;
        let unknown = obj.into_unknown(&env)?;
        Ok(Error::from(unknown))
    };
    build().unwrap_or(err)
}

/// Every task rejects through [`recode`]: one line, and `e.code` is stable.
macro_rules! reject_recode {
    () => {
        fn reject(&mut self, env: Env, err: Error) -> Result<Self::JsValue> {
            Err(recode(env, err))
        }
    };
}

fn not_found(message: impl std::fmt::Display) -> Error {
    transport_err("NOT_FOUND", message)
}

fn invalid(message: impl std::fmt::Display) -> Error {
    transport_err("INVALID_NOTE", message)
}

fn cell_error(e: CellError) -> Error {
    match e {
        CellError::NoteNotFound(_) => not_found("nota inesistente in questa cella"),
        CellError::VersionNotFound(..) => not_found("versione inesistente per questa nota"),
        CellError::Compacted { snapshot_seq } => transport_err(
            "COMPACTED",
            format!("le operazioni sotto seq {snapshot_seq} sono compattate"),
        ),
        CellError::Invalid(m) if m.contains("can only be `quarantined`") => transport_err(
            "CONFLICT",
            "una nota external nasce in quarantena: `confirm` la promuove",
        ),
        CellError::Invalid(m) if m.contains("is a tombstone") => transport_err(
            "CONFLICT",
            "la nota è una lapide: si ripristina dallo storico, una riscrittura \
             non la resuscita",
        ),
        CellError::Invalid(m) if m.contains("a rewrite preserves") => transport_err(
            "CONFLICT",
            "una riscrittura conserva lo stato della nota: lo cambia \
             `setStatus`, `confirm` o `delete`",
        ),
        CellError::Invalid(m) if m.contains("is not quarantined") => transport_err(
            "QUARANTINED",
            "la nota non è in quarantena: niente da confermare",
        ),
        CellError::Invalid(m) if m.contains("empty patch") => invalid("patch vuota"),
        CellError::Invalid(m) => invalid(m),
        CellError::Io(e) => transport_err("IO", e),
        other => transport_err("IO", other),
    }
}

fn search_error(e: SearchError) -> Error {
    match e {
        SearchError::Invalid(m) => invalid(m),
        other => transport_err("IO", other),
    }
}

// ---------------------------------------------------------------------------
// view objects (the contract's TS types)
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct NoteInput {
    /// ULID; a fresh one is minted when absent, an existing one is replaced
    /// (a new version).
    pub id: Option<String>,
    /// Note type (fact, person, place, …); default `note`.
    pub note_type: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Option<Vec<String>>,
    /// Names this entity also answers to (entity notes).
    pub aliases: Option<Vec<String>>,
    /// Who says it — required, e.g. `homun:bot-7`.
    pub source: String,
    /// `stated` | `inferred` | `external` — required. An `external` note is
    /// born quarantined.
    pub confidence: String,
    /// `active` (default) or `quarantined`.
    pub status: Option<String>,
    /// RFC 3339 (or YYYY-MM-DD).
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    /// Free `x-*` extensions, JSON values.
    pub extensions: Option<HashMap<String, serde_json::Value>>,
}

#[napi(object)]
pub struct NoteView {
    pub id: String,
    pub note_type: String,
    pub title: Option<String>,
    pub body: String,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    pub scope: Option<String>,
    pub source: String,
    pub confidence: String,
    pub status: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub version: i64,
    /// v0.4.1 (P1.3): the `x-*` extensions as JSON values (`aliases` has its
    /// own field). What Homun writes in the migration, Homun reads back.
    pub extensions: HashMap<String, serde_json::Value>,
}

#[napi(object)]
pub struct NotePatchInput {
    /// `null` clears the title.
    pub title: Option<Option<String>>,
    pub body: Option<String>,
    pub tags: Option<Vec<String>>,
    pub source: Option<String>,
    pub confidence: Option<String>,
    /// `null` clears the validity bound.
    pub valid_from: Option<Option<String>>,
    pub valid_to: Option<Option<String>>,
    /// v0.4.1 (P1.3): per-key `x-*` extensions — a JSON value sets the key,
    /// `null` removes it. Keys without the `x-` prefix get it, like `put`.
    pub extensions: Option<HashMap<String, Option<serde_json::Value>>>,
}

#[napi(object)]
pub struct SearchInput {
    pub text: String,
    pub k: Option<u32>,
    /// `text` | `vector` | `hybrid` (default; `hybrid` degrades to text when
    /// no model is available).
    pub mode: Option<String>,
    pub graph_hops: Option<u8>,
    /// RFC 3339 instant or YYYY-MM-DD.
    pub as_of: Option<String>,
    pub include_quarantined: Option<bool>,
    /// Attach the per-hit explanation.
    pub explain: Option<bool>,
}

#[napi(object)]
pub struct GraphSay {
    /// The weight the graph list weighed with (0 when gated off).
    pub weight: f64,
    /// Why the graph stayed off («grafo spento: nessuna entità nominata»).
    pub off: Option<String>,
    /// First-to-second gap of the fused list.
    pub gap: Option<f64>,
    /// Whether the text and vector lists agree on the first result.
    pub agreement: Option<bool>,
}

#[napi(object)]
pub struct HitExplain {
    pub lists: Vec<String>,
    pub matched_terms: Vec<String>,
    pub bm25_rank: Option<u32>,
    pub semantic_rank: Option<u32>,
    pub graph_rank: Option<u32>,
    pub cosine: Option<f64>,
    pub support: Option<String>,
    pub via: Option<String>,
    pub graph: Option<GraphSay>,
    pub fused_score: Option<f64>,
}

#[napi(object)]
pub struct SearchHit {
    pub id: String,
    pub cell: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    pub support: Option<String>,
    pub explain: Option<HitExplain>,
}

#[napi(object)]
pub struct VersionView {
    pub version: i64,
    pub op_id: String,
    /// The note as it was BEFORE the operation that this version records.
    pub note: NoteView,
}

#[napi(object)]
pub struct NeighborView {
    pub depth: u8,
    pub entity: String,
    pub rel: String,
    pub direction: String,
    pub note_id: Option<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
}

#[napi(object)]
pub struct PurgeView {
    pub forgotten_payloads: i32,
    pub scrubbed: bool,
    /// What still holds bytes (`wal`, a view): the caller finishes the
    /// forgetting (another embedPending/write retries the checkpoint, a
    /// sync reaches the view).
    pub pending: Vec<String>,
    /// v0.4.1 (P0.2): the views that could not be reached (or asked),
    /// one entry per problem — no note contents ride in them.
    pub views: Vec<PurgeProblemView>,
}

#[napi(object)]
pub struct PurgeProblemView {
    /// The view's ULID, `null` when the cell could not even list its views.
    pub view_id: Option<String>,
    pub root: String,
    pub reason: String,
}

#[napi(object)]
pub struct OpView {
    pub seq: i64,
    pub op_id: String,
    pub actor: String,
    pub kind: String,
    pub note_id: Option<String>,
}

#[napi(object)]
pub struct ChangesSinceOk {
    pub head: i64,
    pub ops: Vec<OpView>,
}

#[napi(object)]
pub struct ChangesSinceCompacted {
    pub compacted: bool,
    pub head: i64,
}

#[napi(object)]
pub struct CoverageView {
    pub model: String,
    pub state: String,
    pub covered: i32,
    pub living: i32,
}

#[napi(object)]
pub struct VerifyView {
    pub ops: i32,
    /// `full` | `from_snapshot` | `chain_only`.
    pub replay: String,
    pub replay_since: Option<i64>,
    pub hash_chain_ok: bool,
    pub replay_ok: bool,
    pub fts_ok: bool,
    pub snapshot_ok: bool,
    pub ok: bool,
    pub mismatch: Option<String>,
    pub coverage: Vec<CoverageView>,
}

#[napi(object)]
pub struct CompactView {
    pub cut_seq: i64,
    pub head_seq: i64,
    pub head_hash: String,
    pub state_digest: Option<String>,
    pub snapshot: Option<String>,
    pub bytes_before: f64,
    pub bytes_after: f64,
}

#[napi(object)]
pub struct ModelStatus {
    pub present: bool,
    pub id: Option<String>,
    pub bytes: Option<f64>,
}

#[napi(object)]
pub struct CellOpts {
    pub create: Option<bool>,
}

#[napi(object)]
pub struct ListFilter {
    pub tags: Option<Vec<String>>,
    pub note_type: Option<String>,
    pub status: Option<Vec<String>>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[napi(object)]
pub struct CompactOpts {
    pub keep_snapshot: Option<bool>,
}

// ---------------------------------------------------------------------------
// small conversions
// ---------------------------------------------------------------------------

fn parse_ulid(s: &str) -> Result<Ulid> {
    Ulid::from_string(s.trim()).map_err(|_| not_found("id non è un ULID valido"))
}

/// Resolve an id (full or 8-char prefix, like the CLI) to a note.
fn resolve_id(cell: &CoreCell, id: &str) -> Result<Ulid> {
    cell.resolve_note_id(id).map_err(cell_error)
}

fn rfc3339(at: At) -> String {
    at.get()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn validity_to_rfc(v: Option<Validity>) -> Option<String> {
    v.map(|x| {
        x.start()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    })
}

fn rfc_to_validity(s: &str) -> Result<Option<Validity>> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    Validity::parse(s)
        .map(Some)
        .map_err(|e| invalid(format!("data non valida (RFC 3339 o YYYY-MM-DD): {e}")))
}

fn parse_confidence(s: &str) -> Result<Confidence> {
    match s {
        "stated" => Ok(Confidence::Stated),
        "inferred" => Ok(Confidence::Inferred),
        "external" => Ok(Confidence::External),
        other => Err(invalid(format!(
            "confidence non valida: {other:?} (stated|inferred|external)"
        ))),
    }
}

fn parse_note_type(s: &str) -> Result<NoteType> {
    NoteType::parse(s).map_err(|e| invalid(format!("tipo non valido: {e}")))
}

fn aliases_of(note: &Note) -> Vec<String> {
    note.extensions
        .iter()
        .filter(|(k, _)| k == "aliases")
        .flat_map(|(_, v)| match v {
            engramma_core::format::yaml::YamlValue::Seq(items) => items
                .iter()
                .filter_map(|i| match i {
                    engramma_core::format::yaml::YamlValue::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

/// v0.4.1 (P1.3): a YAML extension value back to JSON — the exact inverse
/// of `json_to_yaml` below. A sequence of `[key, value]` pairs reads as an
/// object too: that is how a structured `x-*` value rode in the v0.4
/// binding, and those notes must read well as much as the new ones.
fn yaml_to_json(v: &engramma_core::format::yaml::YamlValue) -> serde_json::Value {
    use engramma_core::format::yaml::YamlValue;
    match v {
        YamlValue::Seq(items)
            if !items.is_empty()
                && items.iter().all(|i| {
                    matches!(i, YamlValue::Seq(pair)
                        if pair.len() == 2
                            && matches!(pair[0], YamlValue::Str(_)))
                }) =>
        {
            let mut m = serde_json::Map::new();
            for item in items {
                if let YamlValue::Seq(pair) = item
                    && let YamlValue::Str(k) = &pair[0]
                {
                    m.insert(k.clone(), yaml_to_json(&pair[1]));
                }
            }
            serde_json::Value::Object(m)
        }
        other => other.to_json(),
    }
}

fn note_view(cell: &CoreCell, note: &Note) -> Result<NoteView> {
    let mut extensions = HashMap::with_capacity(note.extensions.len());
    for (k, v) in &note.extensions {
        if k.starts_with("x-") {
            extensions.insert(k.clone(), yaml_to_json(v));
        }
    }
    Ok(NoteView {
        id: note.id.to_string(),
        note_type: note.note_type.as_str().to_string(),
        title: note.title.clone(),
        body: note.body.clone(),
        tags: note.tags.clone(),
        aliases: aliases_of(note),
        scope: note.scope.clone(),
        source: note.source.clone(),
        confidence: note.confidence.as_str().to_string(),
        status: note.status.as_str().to_string(),
        valid_from: validity_to_rfc(note.valid_from),
        valid_to: validity_to_rfc(note.valid_to),
        created_at: rfc3339(note.created_at),
        updated_at: rfc3339(note.updated_at),
        version: cell.note_version(&note.id).unwrap_or(0),
        extensions,
    })
}

fn snippet_of(note: &Note, max: usize) -> String {
    let chars: Vec<char> = note.body.chars().collect();
    if chars.len() <= max {
        return note.body.trim().to_string();
    }
    let cut: String = chars.into_iter().take(max).collect();
    format!("{}…", cut.trim_end())
}

// ---------------------------------------------------------------------------
// the note a put builds (the CLI `add` rules: external is born quarantined)
// ---------------------------------------------------------------------------

fn build_note(input: &NoteInput, id: Ulid) -> Result<Note> {
    if input.source.trim().is_empty() {
        return Err(invalid("source è obbligatoria"));
    }
    let confidence = parse_confidence(&input.confidence)?;
    let note_type = match &input.note_type {
        Some(t) => parse_note_type(t)?,
        None => NoteType::Note,
    };
    let status = match input.status.as_deref() {
        None | Some("") | Some("active") => {
            if confidence == Confidence::External {
                NoteStatus::Quarantined
            } else {
                NoteStatus::Active
            }
        }
        Some("quarantined") => NoteStatus::Quarantined,
        Some(other) => {
            return Err(invalid(format!(
                "status non valida: {other:?} (active|quarantined)"
            )));
        }
    };
    let now = At(OffsetDateTime::now_utc());
    let mut extensions: Vec<(String, engramma_core::format::yaml::YamlValue)> = Vec::new();
    if let Some(aliases) = input.aliases.as_ref().filter(|a| !a.is_empty()) {
        extensions.push((
            "aliases".into(),
            engramma_core::format::yaml::YamlValue::Seq(
                aliases
                    .iter()
                    .map(|a| engramma_core::format::yaml::YamlValue::Str(a.clone()))
                    .collect(),
            ),
        ));
    }
    for (k, v) in input.extensions.iter().flatten() {
        let key = if k.starts_with("x-") {
            k.clone()
        } else {
            format!("x-{k}")
        };
        extensions.push((key, json_to_yaml(v)?));
    }
    Ok(Note {
        id,
        note_type,
        title: input.title.clone().filter(|t| !t.trim().is_empty()),
        body: input.body.clone().unwrap_or_default(),
        tags: input.tags.clone().unwrap_or_default(),
        scope: None,
        lang: None,
        source: input.source.clone(),
        confidence,
        status,
        valid_from: match &input.valid_from {
            Some(s) => rfc_to_validity(s)?,
            None => None,
        },
        valid_to: match &input.valid_to {
            Some(s) => rfc_to_validity(s)?,
            None => None,
        },
        supersedes: vec![],
        created_at: now,
        updated_at: now,
        extensions,
    })
}

fn json_to_yaml(v: &serde_json::Value) -> Result<engramma_core::format::yaml::YamlValue> {
    // v0.4.1 (P1.3): the format's own inverse conversion — objects become
    // `YamlValue::Map` (v0.4 rode them as pair lists, a YAML list in the
    // file; `yaml_to_json` above still reads both forms)
    engramma_core::format::yaml::YamlValue::from_json(v)
        .map_err(|e| invalid(format!("valore di estensione non valido: {e}")))
}

// ---------------------------------------------------------------------------
// search plumbing (shared by Cell and Engine)
// ---------------------------------------------------------------------------

fn build_query(q: &SearchInput) -> Result<Query> {
    if q.text.trim().is_empty() {
        return Err(invalid("query senza testo"));
    }
    let mut query = Query::text(&q.text).k(q.k.unwrap_or(10).clamp(1, 100) as usize);
    query.graph_hops = q.graph_hops.unwrap_or(1).min(2);
    if let Some(as_of) = q.as_of.as_deref().filter(|s| !s.trim().is_empty()) {
        let t = OffsetDateTime::parse(as_of, &time::format_description::well_known::Rfc3339)
            .or_else(|_| Validity::parse(as_of).map(|v| v.start()))
            .map_err(|e| invalid(format!("asOf non valida (RFC 3339 o YYYY-MM-DD): {e}")))?;
        query.filters.as_of = Some(t);
    }
    if q.include_quarantined.unwrap_or(false) {
        query.filters.include_quarantined = true;
    }
    Ok(query)
}

fn hits_to_js(
    cell: &CoreCell,
    cell_name: &str,
    hits: Vec<engramma_core::search::Hit>,
    want_explain: bool,
) -> Vec<SearchHit> {
    hits.into_iter()
        .map(|h| {
            let explain = want_explain.then(|| HitExplain {
                lists: h.explain.lists.iter().map(|s| s.to_string()).collect(),
                matched_terms: h.explain.matched_terms.clone(),
                bm25_rank: h.explain.bm25_rank.and_then(|r| u32::try_from(r).ok()),
                semantic_rank: h.explain.semantic_rank.and_then(|r| u32::try_from(r).ok()),
                graph_rank: h.explain.graph_rank.and_then(|r| u32::try_from(r).ok()),
                cosine: h.explain.cosine,
                support: h.explain.support.map(str::to_string),
                via: h.explain.via.clone(),
                graph: h.explain.graph.as_ref().map(|g| GraphSay {
                    weight: g.weight,
                    off: g.off.map(str::to_string),
                    gap: g.gap,
                    agreement: g.agreement,
                }),
                fused_score: h.explain.fused_score,
            });
            let _ = cell;
            SearchHit {
                id: h.note.id.to_string(),
                cell: cell_name.to_string(),
                title: h.note.display_title().to_string(),
                snippet: snippet_of(&h.note, 180),
                score: h.score,
                support: h.explain.support.map(str::to_string),
                explain,
            }
        })
        .collect()
}

fn search_cell(
    cell: &CoreCell,
    cell_name: &str,
    input: &SearchInput,
    embedder: Option<&Arc<dyn Embedder>>,
) -> Result<Vec<SearchHit>> {
    let mode = match input.mode.as_deref().unwrap_or("hybrid") {
        "text" => Mode::Text,
        "vector" | "embedding" => Mode::Vector,
        "hybrid" => Mode::Hybrid,
        other => {
            return Err(invalid(format!(
                "mode non valida: {other:?} (text|vector|hybrid)"
            )));
        }
    };
    let query = build_query(input)?;
    let now = OffsetDateTime::now_utc();
    let hits: Vec<engramma_core::search::Hit> = match (&mode, embedder) {
        (Mode::Text, _) => cell.search_with_now(&query, now).map_err(search_error)?,
        (_, Some(emb)) => {
            let model = emb.model_id().to_string();
            let known = cell
                .models()
                .map_err(cell_error)?
                .iter()
                .any(|m| m.model == model);
            if !known {
                cell.add_model(&model, emb.dim()).map_err(cell_error)?;
            }
            let sem = Semantic::new(Arc::clone(emb)).with_mode(mode);
            cell.search_semantic_with_now(&query, now, &sem)
                .map_err(search_error)?
                .hits
        }
        (Mode::Vector, None) => {
            return Err(transport_err(
                "MODEL_MISSING",
                "ricerca vettoriale senza modello: `pullModel` prima",
            ));
        }
        (Mode::Hybrid, None) => {
            // no model: hybrid degrades to text+graph, as documented
            cell.search_with_now(&query, now).map_err(search_error)?
        }
    };
    let want_explain = input.explain.unwrap_or(false);
    Ok(hits_to_js(cell, cell_name, hits, want_explain))
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct EngineOptions {
    /// Data folder (cells live in root/cells).
    pub root: String,
    /// The models folder; the builtin default location when absent.
    pub model_dir: Option<String>,
    /// `'builtin'` (Granite 97M via tract, when the model is there) or
    /// `'none'` (text+graph only). Default `'builtin'`.
    pub embedder: Option<String>,
    /// Who writes: `homun`, `homun:bot-<id>`, …
    pub actor: String,
}

/// Everything an open engine shares: the cell cache, the optional builtin
/// embedder, the actor every write is stamped with.
#[derive(Clone)]
pub struct EngineHandle {
    root: PathBuf,
    model_dir: Option<PathBuf>,
    actor: Arc<String>,
    embedder: Option<Arc<dyn Embedder>>,
    cells: Arc<Mutex<HashMap<String, CoreCell>>>,
}

impl EngineHandle {
    fn cell_path(&self, name: &str) -> Result<PathBuf> {
        let clean = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if name.is_empty() || !clean {
            return Err(invalid(format!(
                "nome cella non valido: {name:?} (a-z 0-9 - _)"
            )));
        }
        Ok(self.root.join("cells").join(format!("{name}.sqlite")))
    }

    fn open_cell(&self, name: &str, create: bool) -> Result<CoreCell> {
        if let Some(cell) = self
            .cells
            .lock()
            .map_err(|_| transport_err("IO", "lock delle celle avvelenato"))?
            .get(name)
        {
            return Ok(cell.clone());
        }
        let path = self.cell_path(name)?;
        let cell = if create {
            if path.exists() {
                CoreCell::open(&path).map_err(cell_error)?
            } else {
                std::fs::create_dir_all(path.parent().unwrap_or(&self.root))
                    .map_err(|e| transport_err("IO", e))?;
                CoreCell::create(&path, name, &self.actor).map_err(cell_error)?
            }
        } else {
            if !path.exists() {
                return Err(not_found(format!("cella {name} inesistente")));
            }
            CoreCell::open(&path).map_err(cell_error)?
        };
        self.cells
            .lock()
            .map_err(|_| transport_err("IO", "lock delle celle avvelenato"))?
            .insert(name.to_string(), cell.clone());
        Ok(cell)
    }
}

fn model_status_of(model_dir: Option<&std::path::Path>) -> ModelStatus {
    let dir = model_dir
        .map(PathBuf::from)
        .unwrap_or_else(engramma_embed::spec::model_root);
    let spec = engramma_embed::spec::ModelSpec::granite_97m();
    let present = spec.files.iter().all(|f| spec.file_path(&dir, f).is_file());
    if !present {
        return ModelStatus {
            present: false,
            id: None,
            bytes: None,
        };
    }
    let bytes: u64 = spec
        .files
        .iter()
        .map(|f| {
            std::fs::metadata(spec.file_path(&dir, f))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum();
    ModelStatus {
        present: true,
        id: Some(spec.model_id()),
        bytes: Some(bytes as f64),
    }
}

/// The engine: one per process, one model in RAM, cells on demand.
#[napi]
pub struct Engine {
    handle: EngineHandle,
}

pub struct OpenTask {
    opts: EngineOptions,
}

impl Task for OpenTask {
    type Output = EngineHandle;
    type JsValue = Engine;

    fn compute(&mut self) -> Result<Self::Output> {
        let root = PathBuf::from(&self.opts.root);
        std::fs::create_dir_all(root.join("cells"))
            .map_err(|e| transport_err("IO", format!("{}: {e}", root.display())))?;
        if self.opts.actor.trim().is_empty() {
            return Err(invalid("actor è obbligatorio (per esempio \"homun\")"));
        }
        let model_dir = self.opts.model_dir.as_ref().map(PathBuf::from);
        let want_builtin = self.opts.embedder.as_deref().unwrap_or("builtin") == "builtin";
        let embedder: Option<Arc<dyn Embedder>> = if want_builtin {
            let dir = model_dir
                .clone()
                .unwrap_or_else(engramma_embed::spec::model_root);
            engramma_embed::Builtin::load_named(&dir, "granite-embedding-97m-multilingual-r2")
                .ok()
                .map(|b| Arc::new(b) as Arc<dyn Embedder>)
        } else {
            None
        };
        Ok(EngineHandle {
            root,
            model_dir,
            actor: Arc::new(self.opts.actor.clone()),
            embedder,
            cells: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn resolve(&mut self, _env: Env, handle: EngineHandle) -> Result<Self::JsValue> {
        Ok(Engine { handle })
    }

    reject_recode!();
}

pub struct CellOpenTask {
    handle: EngineHandle,
    name: String,
    create: bool,
}

impl Task for CellOpenTask {
    type Output = (CoreCell, String, Option<Arc<dyn Embedder>>, String);
    type JsValue = Cell;

    fn compute(&mut self) -> Result<Self::Output> {
        let cell = self.handle.open_cell(&self.name, self.create)?;
        Ok((
            cell,
            self.name.clone(),
            self.handle.embedder.clone(),
            (*self.handle.actor).clone(),
        ))
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(Cell::from_parts(out.0, out.1, out.2, out.3))
    }

    reject_recode!();
}

pub struct CellsTask {
    root: PathBuf,
}

impl Task for CellsTask {
    type Output = Vec<String>;
    type JsValue = Vec<String>;

    fn compute(&mut self) -> Result<Self::Output> {
        let mut names = Vec::new();
        let entries =
            std::fs::read_dir(self.root.join("cells")).map_err(|e| transport_err("IO", e))?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "sqlite")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                names.push(stem.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    fn resolve(&mut self, _env: Env, out: Vec<String>) -> Result<Vec<String>> {
        Ok(out)
    }

    reject_recode!();
}

pub struct ModelStatusTask {
    model_dir: Option<PathBuf>,
}

impl Task for ModelStatusTask {
    type Output = ModelStatus;
    type JsValue = ModelStatus;

    fn compute(&mut self) -> Result<Self::Output> {
        Ok(model_status_of(self.model_dir.as_deref()))
    }

    fn resolve(&mut self, _env: Env, out: ModelStatus) -> Result<ModelStatus> {
        Ok(out)
    }

    reject_recode!();
}

pub struct PullModelTask {
    model_dir: Option<PathBuf>,
    #[allow(clippy::type_complexity)]
    progress: Option<Box<dyn FnMut(u64, u64) + Send>>,
}

impl Task for PullModelTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        let dir = self
            .model_dir
            .clone()
            .unwrap_or_else(engramma_embed::spec::model_root);
        let spec = engramma_embed::spec::ModelSpec::granite_97m();
        let mut cb = |done: u64, total: u64| {
            if let Some(p) = self.progress.as_mut() {
                p(done, total);
            }
        };
        engramma_embed::pull::pull_progress(
            &dir,
            &spec,
            None,
            std::time::Duration::from_secs(600),
            &mut cb,
        )
        .map_err(|e| transport_err("IO", format!("pull: {e}")))?;
        Ok(())
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

pub struct EngineSearchTask {
    handle: EngineHandle,
    cells: Vec<String>,
    q: SearchInput,
}

impl Task for EngineSearchTask {
    type Output = Vec<SearchHit>;
    type JsValue = Vec<SearchHit>;

    fn compute(&mut self) -> Result<Self::Output> {
        if self.cells.is_empty() {
            return Err(invalid("serve almeno una cella"));
        }
        let k = self.q.k.unwrap_or(10).clamp(1, 100) as usize;
        let mut fused: HashMap<String, (f64, SearchHit)> = HashMap::new();
        for name in &self.cells {
            let cell = self.handle.open_cell(name, false)?;
            let hits = search_cell(&cell, name, &self.q, self.handle.embedder.as_ref())?;
            for (rank, hit) in hits.into_iter().take(k).enumerate() {
                // RRF, k = 60: the scores of different cells do not compare
                // (documented as approximate)
                let score = 1.0 / (60.0 + (rank + 1) as f64);
                let e = fused.entry(hit.id.clone()).or_insert((0.0, hit));
                e.0 += score;
            }
        }
        let mut out: Vec<(f64, SearchHit)> = fused.into_values().collect();
        out.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.id.cmp(&b.1.id))
        });
        Ok(out
            .into_iter()
            .take(k)
            .map(|(rrf, mut hit)| {
                hit.score = rrf;
                hit
            })
            .collect())
    }

    fn resolve(&mut self, _env: Env, out: Vec<SearchHit>) -> Result<Vec<SearchHit>> {
        Ok(out)
    }

    reject_recode!();
}

pub struct CloseTask {
    cells: Arc<Mutex<HashMap<String, CoreCell>>>,
}

impl Task for CloseTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        self.cells
            .lock()
            .map_err(|_| transport_err("IO", "lock delle celle avvelenato"))?
            .clear();
        Ok(())
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

#[napi]
impl Engine {
    /// Open (or create) an engine at `root`. One per process: the model
    /// stays in RAM (~400 MB with the 97M).
    #[napi(ts_return_type = "Promise<Engine>")]
    pub fn open(opts: EngineOptions) -> AsyncTask<OpenTask> {
        AsyncTask::new(OpenTask { opts })
    }

    /// A cell by name (`create` defaults to true).
    #[napi(ts_return_type = "Promise<Cell>")]
    pub fn cell(&self, name: String, opts: Option<CellOpts>) -> AsyncTask<CellOpenTask> {
        AsyncTask::new(CellOpenTask {
            handle: self.handle.clone(),
            name,
            create: opts.and_then(|o| o.create).unwrap_or(true),
        })
    }

    /// The names of the cells that exist under root/cells.
    #[napi(ts_return_type = "Promise<string[]>")]
    pub fn cells(&self) -> AsyncTask<CellsTask> {
        AsyncTask::new(CellsTask {
            root: self.handle.root.clone(),
        })
    }

    /// Is the builtin model there? (id and bytes when it is)
    #[napi(ts_return_type = "Promise<ModelStatus>")]
    pub fn model_status(&self) -> AsyncTask<ModelStatusTask> {
        AsyncTask::new(ModelStatusTask {
            model_dir: self.handle.model_dir.clone(),
        })
    }

    /// Download the builtin model (pinned revision, sha256 verified per
    /// chunk). Already-present files verify and skip: offline it is a no-op.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn pull_model(
        &self,
        #[napi(ts_arg_type = "(done: number, total: number) => void")] on_progress: Option<
            Function<'static, (f64, f64)>,
        >,
    ) -> Result<AsyncTask<PullModelTask>> {
        let progress: Option<Box<dyn FnMut(u64, u64) + Send>> = match on_progress {
            None => None,
            Some(f) => {
                // numbers are 'static: the callback hands the tuple straight
                // through, no JS value is built on the Rust side
                let tsfn = f
                    .build_threadsafe_function::<(f64, f64)>()
                    .build_callback(|ctx| Ok(ctx.value))?;
                Some(Box::new(move |done: u64, total: u64| {
                    tsfn.call(
                        (done as f64, total as f64),
                        napi::threadsafe_function::ThreadsafeFunctionCallMode::Blocking,
                    );
                }) as Box<dyn FnMut(u64, u64) + Send>)
            }
        };
        Ok(AsyncTask::new(PullModelTask {
            model_dir: self.handle.model_dir.clone(),
            progress,
        }))
    }

    /// Search ACROSS cells: each cell's ranking, fused with RRF (k = 60).
    /// Approximate by declaration — the scores of different cells do not
    /// compare.
    #[napi(ts_return_type = "Promise<SearchHit[]>")]
    pub fn search(&self, cells: Vec<String>, q: SearchInput) -> AsyncTask<EngineSearchTask> {
        AsyncTask::new(EngineSearchTask {
            handle: self.handle.clone(),
            cells,
            q,
        })
    }

    /// Release the engine's cell handles. Outstanding Cell objects keep
    /// working until the GC drops them.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn close(&self) -> AsyncTask<CloseTask> {
        AsyncTask::new(CloseTask {
            cells: self.handle.cells.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------------

/// One cell of memory. Usable from many promises in parallel: writes on
/// the same cell serialize inside the engine (the cell's own lock).
#[napi(js_name = "Cell")]
pub struct Cell {
    cell: CoreCell,
    name: String,
    embedder: Option<Arc<dyn Embedder>>,
    actor: String,
}

impl Cell {
    fn from_parts(
        cell: CoreCell,
        name: String,
        embedder: Option<Arc<dyn Embedder>>,
        actor: String,
    ) -> Self {
        Cell {
            cell,
            name,
            embedder,
            actor,
        }
    }
}

pub struct PutTask {
    cell: CoreCell,
    actor: String,
    input: NoteInput,
}

impl Task for PutTask {
    type Output = Note;
    type JsValue = NoteView;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = match &self.input.id {
            Some(s) => parse_ulid(s)?,
            None => engramma_core::id::new_ulid().map_err(cell_error)?,
        };
        let mut note = build_note(&self.input, id)?;
        // v0.4.1 (P0.1): a rewrite PRESERVES the note's state. The caller
        // that does not ask for a status gets the preserved one (the core
        // recomputes and enforces it); an explicit request that disagrees
        // with the preserved state is the caller's CONFLICT to hear about.
        if self.input.id.is_some()
            && let Some(old) = self.cell.get(&note.id).ok()
            && old.status != NoteStatus::Deleted
            && self.input.status.is_none()
        {
            note.status =
                if old.status == NoteStatus::Active && note.confidence == Confidence::External {
                    NoteStatus::Quarantined
                } else {
                    old.status
                };
            if note.status == NoteStatus::Superseded && note.valid_to.is_none() {
                note.valid_to = old.valid_to;
            }
        }
        self.cell.put_note(&note, &self.actor).map_err(cell_error)?;
        // the view comes from the cell, the single source of truth (the
        // state a rewrite preserves, the dates, the version)
        self.cell.get(&note.id).map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, note: Note) -> Result<NoteView> {
        note_view(&self.cell, &note)
    }

    reject_recode!();
}

pub struct PatchTask {
    cell: CoreCell,
    actor: String,
    id: String,
    patch: NotePatchInput,
}

impl Task for PatchTask {
    type Output = Note;
    type JsValue = NoteView;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = resolve_id(&self.cell, &self.id)?;
        // v0.4.1 (P1.3): extensions patch PER KEY — a JSON value sets it,
        // `null` removes it, everything the note already carries stays. The
        // core's patch replaces the whole list, so the merge happens here.
        let extensions = match &self.patch.extensions {
            None => None,
            Some(ops) => {
                let old = self.cell.get(&id).map_err(cell_error)?;
                let mut list = old.extensions.clone();
                for (k, v) in ops {
                    let key = if k.starts_with("x-") {
                        k.clone()
                    } else {
                        format!("x-{k}")
                    };
                    match v {
                        None => list.retain(|(ek, _)| *ek != key),
                        Some(val) => {
                            let yv = json_to_yaml(val)?;
                            match list.iter_mut().find(|(ek, _)| *ek == key) {
                                Some(slot) => slot.1 = yv,
                                None => list.push((key, yv)),
                            }
                        }
                    }
                }
                Some(list)
            }
        };
        let patch = NotePatch {
            title: self.patch.title.clone(),
            body: self.patch.body.clone(),
            tags: self.patch.tags.clone(),
            scope: None,
            lang: None,
            source: self.patch.source.clone(),
            confidence: self
                .patch
                .confidence
                .as_deref()
                .map(parse_confidence)
                .transpose()?,
            valid_from: match &self.patch.valid_from {
                None => None,
                Some(None) => Some(None),
                Some(Some(s)) => Some(rfc_to_validity(s)?),
            },
            valid_to: match &self.patch.valid_to {
                None => None,
                Some(None) => Some(None),
                Some(Some(s)) => Some(rfc_to_validity(s)?),
            },
            extensions,
        };
        self.cell
            .patch_note(&id, &patch, &self.actor)
            .map_err(cell_error)?;
        self.cell.get(&id).map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, note: Note) -> Result<NoteView> {
        note_view(&self.cell, &note)
    }

    reject_recode!();
}

pub struct GetTask {
    cell: CoreCell,
    id: String,
}

impl Task for GetTask {
    type Output = Option<Note>;
    type JsValue = Option<NoteView>;

    fn compute(&mut self) -> Result<Self::Output> {
        // the short (8) form resolves like the CLI; quarantined and deleted
        // notes answer null — the same oracle as search
        // a note that does not exist, is quarantined or is deleted answers
        // null — the closed oracle: nothing about the cell can be read
        // from a get
        let Ok(id) = self.cell.resolve_note_id(&self.id) else {
            return Ok(None);
        };
        match self.cell.get(&id) {
            Ok(note) if matches!(note.status, NoteStatus::Quarantined | NoteStatus::Deleted) => {
                Ok(None)
            }
            Ok(note) => Ok(Some(note)),
            Err(_) => Ok(None),
        }
    }

    fn resolve(&mut self, _env: Env, note: Option<Note>) -> Result<Option<NoteView>> {
        match note {
            Some(n) => Ok(Some(note_view(&self.cell, &n)?)),
            None => Ok(None),
        }
    }

    reject_recode!();
}

pub struct ListTask {
    cell: CoreCell,
    tags: Option<Vec<String>>,
    note_type: Option<String>,
    statuses: Option<Vec<String>>,
    limit: Option<u32>,
    offset: Option<u32>,
}

impl Task for ListTask {
    type Output = Vec<Note>;
    type JsValue = Vec<NoteView>;

    fn compute(&mut self) -> Result<Self::Output> {
        let want_type = self.note_type.as_deref().map(parse_note_type).transpose()?;
        let wanted: Option<Vec<NoteStatus>> = self.statuses.as_ref().map(|ss| {
            ss.iter()
                .filter_map(|s| match s.as_str() {
                    "active" => Some(NoteStatus::Active),
                    "quarantined" => Some(NoteStatus::Quarantined),
                    "superseded" => Some(NoteStatus::Superseded),
                    _ => None,
                })
                .collect()
        });
        let offset = self.offset.unwrap_or(0) as usize;
        let limit = self.limit.unwrap_or(100).clamp(1, 1000) as usize;
        let ids = self.cell.note_ids().map_err(cell_error)?;
        let mut out: Vec<Note> = Vec::new();
        for id in ids {
            if out.len() >= offset + limit {
                break;
            }
            let Ok(note) = self.cell.get(&id) else {
                continue;
            };
            if let Some(t) = &want_type
                && note.note_type != *t
            {
                continue;
            }
            if let Some(want) = &wanted
                && !want.contains(&note.status)
            {
                continue;
            }
            if let Some(tags) = &self.tags
                && !tags.iter().any(|t| {
                    note.tags
                        .iter()
                        .any(|nt| nt == t || nt.starts_with(&format!("{t}/")))
                })
            {
                continue;
            }
            out.push(note);
        }
        Ok(out.into_iter().skip(offset).take(limit).collect())
    }

    fn resolve(&mut self, _env: Env, notes: Vec<Note>) -> Result<Vec<NoteView>> {
        notes.iter().map(|n| note_view(&self.cell, n)).collect()
    }

    reject_recode!();
}

pub struct SearchTask {
    cell: CoreCell,
    name: String,
    embedder: Option<Arc<dyn Embedder>>,
    q: SearchInput,
}

impl Task for SearchTask {
    type Output = Vec<SearchHit>;
    type JsValue = Vec<SearchHit>;

    fn compute(&mut self) -> Result<Self::Output> {
        search_cell(&self.cell, &self.name, &self.q, self.embedder.as_ref())
    }

    fn resolve(&mut self, _env: Env, hits: Vec<SearchHit>) -> Result<Vec<SearchHit>> {
        Ok(hits)
    }

    reject_recode!();
}

pub struct HistoryTask {
    cell: CoreCell,
    id: String,
}

impl Task for HistoryTask {
    type Output = Vec<VersionView>;
    type JsValue = Vec<VersionView>;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = resolve_id(&self.cell, &self.id)?;
        let versions = self.cell.history(&id).map_err(cell_error)?;
        versions
            .iter()
            .map(|v| {
                Ok(VersionView {
                    version: v.version,
                    op_id: v.op_id.to_string(),
                    note: note_view(&self.cell, &v.note)?,
                })
            })
            .collect()
    }

    fn resolve(&mut self, _env: Env, out: Vec<VersionView>) -> Result<Vec<VersionView>> {
        Ok(out)
    }

    reject_recode!();
}

pub struct NeighborsTask {
    cell: CoreCell,
    id: String,
    depth: u8,
}

impl Task for NeighborsTask {
    type Output = Vec<NeighborView>;
    type JsValue = Vec<NeighborView>;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = resolve_id(&self.cell, &self.id)?;
        let ns = self
            .cell
            .neighbors(&format!("note:{id}"), self.depth.min(4), &[], false)
            .map_err(search_error)?;
        Ok(ns
            .into_iter()
            .map(|n| NeighborView {
                depth: n.depth,
                entity: n.entity,
                rel: n.rel,
                direction: n.direction,
                note_id: n.note_id,
                valid_from: n.valid_from,
                valid_to: n.valid_to,
            })
            .collect())
    }

    fn resolve(&mut self, _env: Env, out: Vec<NeighborView>) -> Result<Vec<NeighborView>> {
        Ok(out)
    }

    reject_recode!();
}

pub struct SetStatusTask {
    cell: CoreCell,
    actor: String,
    id: String,
    status: String,
}

impl Task for SetStatusTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        let id = resolve_id(&self.cell, &self.id)?;
        let status = match self.status.as_str() {
            "active" => NoteStatus::Active,
            "quarantined" => NoteStatus::Quarantined,
            "superseded" => NoteStatus::Superseded,
            other => {
                return Err(invalid(format!(
                    "status non valida: {other:?} (active|quarantined|superseded)"
                )));
            }
        };
        self.cell
            .set_status(&id, status, &self.actor)
            .map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

pub struct ConfirmTask {
    cell: CoreCell,
    actor: String,
    id: String,
}

impl Task for ConfirmTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        let id = resolve_id(&self.cell, &self.id)?;
        self.cell.confirm(&id, &self.actor).map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

pub struct DeleteTask {
    cell: CoreCell,
    actor: String,
    id: String,
}

impl Task for DeleteTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        let id = resolve_id(&self.cell, &self.id)?;
        self.cell.delete_note(&id, &self.actor).map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

pub struct PurgeTask {
    cell: CoreCell,
    actor: String,
    id: String,
    reason: String,
}

impl Task for PurgeTask {
    type Output = (PurgeReport, Vec<engramma_vault::ViewProblem>);

    type JsValue = PurgeView;

    fn compute(&mut self) -> Result<Self::Output> {
        if self.reason.trim().is_empty() {
            return Err(invalid("reason è obbligatoria"));
        }
        let id = resolve_id(&self.cell, &self.id)?;
        // v0.4.1 (P0.2): the SAME purge the CLI runs — the views included.
        // Before, the binding forgot only the cell and answered `scrubbed:
        // true` while the note's file stayed in a bound view until the next
        // sync.
        engramma_vault::purge_everywhere(&self.cell, &id, &self.reason, &self.actor).map_err(|e| {
            match e {
                engramma_vault::VaultError::Cell(c) => cell_error(c),
                other => transport_err("IO", format!("purge: {other}")),
            }
        })
    }

    fn resolve(&mut self, _env: Env, (r, problems): Self::Output) -> Result<PurgeView> {
        Ok(PurgeView {
            forgotten_payloads: i32::try_from(r.forgotten_payloads).unwrap_or(i32::MAX),
            scrubbed: r.scrubbed,
            pending: r.pending.iter().map(|s| s.to_string()).collect(),
            views: problems
                .into_iter()
                .map(|p| PurgeProblemView {
                    view_id: p.view_id,
                    root: p.root,
                    reason: p.reason,
                })
                .collect(),
        })
    }

    reject_recode!();
}

pub struct EmbedPendingTask {
    cell: CoreCell,
    embedder: Option<Arc<dyn Embedder>>,
    limit: Option<u32>,
}

impl Task for EmbedPendingTask {
    type Output = i64;
    type JsValue = f64;

    fn compute(&mut self) -> Result<Self::Output> {
        let Some(embedder) = self.embedder.as_ref() else {
            return Err(transport_err(
                "MODEL_MISSING",
                "embedPending senza modello: `pullModel` prima",
            ));
        };
        let model = embedder.model_id().to_string();
        let known = self
            .cell
            .models()
            .map_err(cell_error)?
            .iter()
            .any(|m| m.model == model);
        if !known {
            self.cell
                .add_model(&model, embedder.dim())
                .map_err(cell_error)?;
        }
        let limit = self.limit.unwrap_or(512).clamp(1, 8192) as usize;
        let report = self
            .cell
            .embed_pending(embedder.as_ref(), limit)
            .map_err(cell_error)?;
        Ok(i64::try_from(report.done).unwrap_or(0))
    }

    fn resolve(&mut self, _env: Env, done: i64) -> Result<f64> {
        Ok(done as f64)
    }

    reject_recode!();
}

pub struct ChangesSinceTask {
    cell: CoreCell,
    seq: i64,
}

impl Task for ChangesSinceTask {
    type Output = std::result::Result<(i64, Vec<OpView>), i64>;
    type JsValue = Either<ChangesSinceOk, ChangesSinceCompacted>;

    fn compute(&mut self) -> Result<Self::Output> {
        match self.cell.changes_since(self.seq) {
            Ok(ops) => {
                let head = ops.last().map(|o| o.seq).unwrap_or(self.seq);
                let views = ops
                    .into_iter()
                    .map(|op| OpView {
                        seq: op.seq,
                        op_id: op.op_id.to_string(),
                        actor: op.actor,
                        kind: op.kind.as_str().to_string(),
                        note_id: op.note_id.map(|n| n.to_string()),
                    })
                    .collect();
                Ok(Ok((head, views)))
            }
            Err(CellError::Compacted { .. }) => {
                let head = self
                    .cell
                    .debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM ops")
                    .ok()
                    .and_then(|v| v.first().and_then(|s| s.parse().ok()))
                    .unwrap_or(self.seq);
                Ok(Err(head))
            }
            Err(e) => Err(cell_error(e)),
        }
    }

    fn resolve(
        &mut self,
        _env: Env,
        out: std::result::Result<(i64, Vec<OpView>), i64>,
    ) -> Result<Self::JsValue> {
        Ok(match out {
            Ok((head, ops)) => Either::A(ChangesSinceOk { head, ops }),
            Err(head) => Either::B(ChangesSinceCompacted {
                compacted: true,
                head,
            }),
        })
    }

    reject_recode!();
}

pub struct VerifyTask {
    cell: CoreCell,
}

impl Task for VerifyTask {
    type Output = VerifyReport;
    type JsValue = VerifyView;

    fn compute(&mut self) -> Result<Self::Output> {
        self.cell.verify().map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, r: VerifyReport) -> Result<VerifyView> {
        Ok(VerifyView {
            ops: i32::try_from(r.ops).unwrap_or(i32::MAX),
            replay: match r.replay {
                engramma_core::cell::Replay::Full => "full".into(),
                engramma_core::cell::Replay::FromSnapshot { .. } => "from_snapshot".into(),
                engramma_core::cell::Replay::ChainOnly { .. } => "chain_only".into(),
            },
            replay_since: match r.replay {
                engramma_core::cell::Replay::FromSnapshot { seq }
                | engramma_core::cell::Replay::ChainOnly { since_seq: seq } => Some(seq),
                engramma_core::cell::Replay::Full => None,
            },
            hash_chain_ok: r.hash_chain_ok,
            replay_ok: r.replay_ok,
            fts_ok: r.fts_ok,
            snapshot_ok: r.snapshot_ok,
            ok: r.ok(),
            mismatch: r.mismatch,
            coverage: r
                .coverage
                .into_iter()
                .map(|c| CoverageView {
                    model: c.model,
                    state: format!("{:?}", c.state),
                    covered: i32::try_from(c.covered).unwrap_or(i32::MAX),
                    living: i32::try_from(c.living).unwrap_or(i32::MAX),
                })
                .collect(),
        })
    }

    reject_recode!();
}

pub struct CompactTask {
    cell: CoreCell,
    keep_last: i64,
    keep_snapshot: bool,
}

impl Task for CompactTask {
    type Output = engramma_core::cell::CompactReport;
    type JsValue = CompactView;

    fn compute(&mut self) -> Result<Self::Output> {
        let head: i64 = self
            .cell
            .debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM ops")
            .map_err(cell_error)?
            .first()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| transport_err("IO", "testa del registro illeggibile"))?;
        let cut = (head - self.keep_last).max(1);
        let opts = engramma_core::cell::CompactOptions {
            keep_snapshot: self.keep_snapshot,
        };
        self.cell.compact(cut, &opts).map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, r: engramma_core::cell::CompactReport) -> Result<CompactView> {
        Ok(CompactView {
            cut_seq: r.cut_seq,
            head_seq: r.head_seq,
            head_hash: r.head_hash,
            state_digest: Some(r.state_digest),
            snapshot: r.snapshot.map(|p| p.display().to_string()),
            bytes_before: r.bytes_before as f64,
            bytes_after: r.bytes_after as f64,
        })
    }

    reject_recode!();
}

#[napi]
impl Cell {
    /// The cell's name.
    #[napi(getter)]
    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// Write a note (a fresh id when absent; a new version when present).
    /// `source` and `confidence` are required; an `external` note is born
    /// quarantined.
    #[napi(ts_return_type = "Promise<NoteView>")]
    pub fn put(&self, note: NoteInput) -> AsyncTask<PutTask> {
        AsyncTask::new(PutTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            input: note,
        })
    }

    /// Change some fields of a note.
    #[napi(ts_return_type = "Promise<NoteView>")]
    pub fn patch(&self, id: String, patch: NotePatchInput) -> AsyncTask<PatchTask> {
        AsyncTask::new(PatchTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            patch,
        })
    }

    /// Read a note by id (the short 8-char form works, like the CLI).
    /// Quarantined and deleted notes answer `null` — the same oracle as
    /// search.
    #[napi(ts_return_type = "Promise<NoteView | null>")]
    pub fn get(&self, id: String) -> AsyncTask<GetTask> {
        AsyncTask::new(GetTask {
            cell: self.cell.clone(),
            id,
        })
    }

    /// The notes of this cell, filtered. Default limit 100.
    #[napi(ts_return_type = "Promise<NoteView[]>")]
    pub fn list(&self, f: Option<ListFilter>) -> AsyncTask<ListTask> {
        AsyncTask::new(ListTask {
            cell: self.cell.clone(),
            tags: f.as_ref().and_then(|f| f.tags.clone()),
            note_type: f.as_ref().and_then(|f| f.note_type.clone()),
            statuses: f.as_ref().and_then(|f| f.status.clone()),
            limit: f.as_ref().and_then(|f| f.limit),
            offset: f.as_ref().and_then(|f| f.offset),
        })
    }

    /// Search this cell.
    #[napi(ts_return_type = "Promise<SearchHit[]>")]
    pub fn search(&self, q: SearchInput) -> AsyncTask<SearchTask> {
        AsyncTask::new(SearchTask {
            cell: self.cell.clone(),
            name: self.name.clone(),
            embedder: self.embedder.clone(),
            q,
        })
    }

    /// The note's versions, oldest first (each is the state BEFORE the
    /// operation that version records).
    #[napi(ts_return_type = "Promise<VersionView[]>")]
    pub fn history(&self, id: String) -> AsyncTask<HistoryTask> {
        AsyncTask::new(HistoryTask {
            cell: self.cell.clone(),
            id,
        })
    }

    /// The note's graph neighborhood (BFS, default depth 1, max 4).
    #[napi(ts_return_type = "Promise<NeighborView[]>")]
    pub fn neighbors(&self, id: String, depth: Option<u8>) -> AsyncTask<NeighborsTask> {
        AsyncTask::new(NeighborsTask {
            cell: self.cell.clone(),
            id,
            depth: depth.unwrap_or(1),
        })
    }

    /// Change a note's status (`active` | `quarantined` | `superseded`).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_status(&self, id: String, status: String) -> AsyncTask<SetStatusTask> {
        AsyncTask::new(SetStatusTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            status,
        })
    }

    /// Promote a quarantined note to active.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn confirm(&self, id: String) -> AsyncTask<ConfirmTask> {
        AsyncTask::new(ConfirmTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
        })
    }

    /// A reversible tombstone.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn delete(&self, id: String) -> AsyncTask<DeleteTask> {
        AsyncTask::new(DeleteTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
        })
    }

    /// The right to be forgotten — irreversible. The view says whether
    /// every byte is already gone (`scrubbed: true`) or something is
    /// `pending` (the WAL, a view): an incomplete forgetting the caller
    /// must finish.
    #[napi(ts_return_type = "Promise<PurgeView>")]
    pub fn purge(&self, id: String, reason: String) -> AsyncTask<PurgeTask> {
        AsyncTask::new(PurgeTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            reason,
        })
    }

    /// Embed the notes that are still pending; returns how many it did.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn embed_pending(&self, limit: Option<u32>) -> AsyncTask<EmbedPendingTask> {
        AsyncTask::new(EmbedPendingTask {
            cell: self.cell.clone(),
            embedder: self.embedder.clone(),
            limit,
        })
    }

    /// The ops after `seq`, or `{compacted: true, head}` when they sit
    /// below the compaction cut.
    #[napi(
        ts_return_type = "Promise<{ head: number; ops: OpView[] } | { compacted: true; head: number }>"
    )]
    pub fn changes_since(&self, seq: i64) -> AsyncTask<ChangesSinceTask> {
        AsyncTask::new(ChangesSinceTask {
            cell: self.cell.clone(),
            seq,
        })
    }

    /// Verify the hash chain, the replay and the derived indexes.
    #[napi(ts_return_type = "Promise<VerifyView>")]
    pub fn verify(&self) -> AsyncTask<VerifyTask> {
        AsyncTask::new(VerifyTask {
            cell: self.cell.clone(),
        })
    }

    /// Forget the payloads of the oldest operations, keeping the last
    /// `keepLast` replayable.
    #[napi(ts_return_type = "Promise<CompactView>")]
    pub fn compact(&self, keep_last: i64, opts: Option<CompactOpts>) -> AsyncTask<CompactTask> {
        AsyncTask::new(CompactTask {
            cell: self.cell.clone(),
            keep_last: keep_last.max(0),
            keep_snapshot: opts.and_then(|o| o.keep_snapshot).unwrap_or(false),
        })
    }

    /// The views bound to this cell (diagnostics: names and roots).
    #[napi(ts_return_type = "Promise<VaultView[]>")]
    pub fn vault_views(&self) -> AsyncTask<VaultViewsTask> {
        AsyncTask::new(VaultViewsTask {
            cell: self.cell.clone(),
        })
    }

    /// Detach a markdown view from this cell (v0.4, P0.1): the cell forgets
    /// it and its bindings — the files stay on disk. Accepts the view id
    /// (even its first 8 characters) or the folder.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn vault_unbind(&self, dir_or_view_id: String) -> AsyncTask<VaultUnbindTask> {
        AsyncTask::new(VaultUnbindTask {
            cell: self.cell.clone(),
            arg: dir_or_view_id,
        })
    }
}

#[napi(object)]
pub struct VaultView {
    pub view_id: String,
    pub root: String,
    pub bound_at: String,
    pub last_seen_at: String,
}

pub struct VaultViewsTask {
    cell: CoreCell,
}

impl Task for VaultViewsTask {
    type Output = Vec<VaultView>;
    type JsValue = Vec<VaultView>;

    fn compute(&mut self) -> Result<Self::Output> {
        Ok(self
            .cell
            .vault_views()
            .map_err(cell_error)?
            .into_iter()
            .map(|v| VaultView {
                view_id: v.view_id,
                root: v.root,
                bound_at: v.bound_at,
                last_seen_at: v.last_seen_at,
            })
            .collect())
    }

    fn resolve(&mut self, _env: Env, out: Vec<VaultView>) -> Result<Vec<VaultView>> {
        Ok(out)
    }

    reject_recode!();
}

pub struct VaultUnbindTask {
    cell: CoreCell,
    arg: String,
}

impl Task for VaultUnbindTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        let arg = self.arg.trim();
        let by_path = std::fs::canonicalize(arg).ok().and_then(|c| {
            self.cell
                .vault_view_by_root(&c.to_string_lossy())
                .ok()
                .flatten()
        });
        let view = match by_path {
            Some(v) => Some(v),
            None => {
                let hits: Vec<_> = self
                    .cell
                    .vault_views()
                    .map_err(cell_error)?
                    .into_iter()
                    .filter(|v| v.view_id == arg || (arg.len() >= 8 && v.view_id.starts_with(arg)))
                    .collect();
                match hits.len() {
                    1 => Some(hits[0].clone()),
                    0 => None,
                    n => {
                        return Err(invalid(format!(
                            "«{arg}» indica {n} viste: serve l'id completo"
                        )));
                    }
                }
            }
        };
        let Some(view) = view else {
            return Err(not_found(format!("nessuna vista legata a «{arg}»")));
        };
        self.cell.vault_unbind(&view.view_id).map_err(cell_error)?;
        Ok(())
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

/// The engine version, so the host can log what it loaded.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
