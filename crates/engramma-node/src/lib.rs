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

// v0.4.4 (P1) → v0.4.5 (P2): this cdylib's Rust allocations (engine +
// tract) go through mimalloc ON MACOS ONLY: macOS's allocator keeps the
// freed pages of the work resident; mimalloc holds far less (1149 vs
// 1450 MB at rest, REPORT v0.4.5). On Linux the opposite holds — glibc
// returns them (637 vs 1166 MB at rest) — so there the system allocator
// stays. In a cdylib the `#[global_allocator]` covers this module only:
// Node and Electron keep their own.
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
use engramma_sync::service::SyncService;

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
                // v0.4.2 (P0.2): a wrong ARGUMENT of the call (an unknown
                // status in `list`, a `limit` of zero) — INVALID_NOTE is
                // about the notes, and never fit these
                "INVALID_ARGUMENT",
                "CONFLICT",
                "IO",
                "MODEL_MISSING",
                "COMPACTED",
                // v0.5 (P0): the cell was migrated by a newer binary
                "SCHEMA_TOO_NEW",
                "CLOSED",
                // v0.6.2 (S2): the sync service's own refusals
                "SYNC_BUSY",
                // v0.6.4 (D4): the peer speaks other materialization rules
                "PEER_OUTDATED",
                "PAIRING_EXPIRED",
                "PAIRING_REJECTED",
                "PAIRING_TIMEOUT",
                "NO_MAILBOX",
                "NOT_SHARED",
                "LAST_DEVICE",
                "CANCELLED",
                // v0.6.9 (I1): «forgotten forever» — the id does not
                // come back, not even from an old export
                "NOTE_PURGED",
                // v0.6.15 (F2): the call needs an experimental opt-in it
                // did not get (today: compaction on a cell opened without
                // `experimental: ['compaction']`)
                "EXPERIMENTAL",
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

/// v0.4.2 (P0.2): a wrong ARGUMENT of the call — an unknown `status` in
/// `list`, a `limit` of zero. `INVALID_NOTE` is about the notes and never
/// fit these; the caller can tell a bug of its own from a note it cannot
/// write.
fn invalid_argument(message: impl std::fmt::Display) -> Error {
    transport_err("INVALID_ARGUMENT", message)
}

fn cell_error(e: CellError) -> Error {
    match e {
        CellError::NoteNotFound(_) => not_found("nota inesistente in questa cella"),
        CellError::VersionNotFound(..) => not_found("versione inesistente per questa nota"),
        CellError::Compacted { snapshot_seq } => transport_err(
            "COMPACTED",
            format!("le operazioni sotto seq {snapshot_seq} sono compattate"),
        ),
        // v0.5 (P0): ONLY the new code, no new methods — an old Homun `.node`
        // must be told the cell is newer, not left to write under tables it
        // does not know (REPORT, «Per Homun»).
        CellError::SchemaTooNew { found, supported } => transport_err(
            "SCHEMA_TOO_NEW",
            format!(
                "lo schema della cella è più nuovo di questo binario \
                 (versione {found}, supportata fino a {supported}): aggiorna il binding"
            ),
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
        // v0.6.4 (D3): resolving a conflict that is already closed
        CellError::Conflict(m) => transport_err("CONFLICT", m),
        // v0.6.9 (I1): «forgotten forever» means the id does not come
        // back — not even from an old export. Homun catches the code and
        // tells the user which file to drop or rewrite with a new id (the
        // import report names them as `skippedPurged`: docs/node.md)
        CellError::NotePurged(id) => transport_err(
            "NOTE_PURGED",
            format!(
                "la nota {id} è purgata: quell'id non torna, \
                 serve una nota nuova"
            ),
        ),
        CellError::Invalid(m) if m.contains("empty patch") => invalid("patch vuota"),
        CellError::Invalid(m) => invalid(m),
        CellError::Io(e) => transport_err("IO", e),
        other => transport_err("IO", other),
    }
}

/// The sync service's refusals, in the transport's terms: the code rides
/// the message prefix, `e.code` stays stable (v0.6.2, S2).
fn sync_error(e: engramma_sync::service::ServiceError) -> Error {
    transport_err(e.code(), e.message())
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
    /// v0.4.1 (P1.4): import dates (RFC 3339), for a NEW note only — the
    /// migration needs a note to be born in its past. Never in the future
    /// (5 minutes of tolerance); a rewrite ignores both (the birth date is
    /// the note's own).
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
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
    /// v0.4.1 (P1.5): what a result row needs, without a `get` per hit.
    pub note_type: String,
    pub tags: Vec<String>,
    /// RFC 3339 — when the note last changed.
    pub updated_at: String,
    /// `active` outside `includeQuarantined` (which can surface
    /// `quarantined`).
    pub status: String,
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
    /// v0.6.10 (J2): note ids of the «phantoms» this verify FOUND — a
    /// living row on a purged id, written by the v0.6.7/v0.6.8 engines.
    /// verify changes nothing: `ok` is false, and `repair()` is the
    /// explicit step that moves each phantom's content to a new id.
    /// Empty on a healthy cell.
    pub phantom_purged: Vec<String>,
    /// v0.6.14 (N1.3): the DECLARED CORNERS this cell carries — prose,
    /// never a failure (`ok` does not look at it). Today one kind: the cut
    /// of a compaction went back under the v0.6.13 engine and the
    /// migration had no copy file to rebuild some objects from. Empty on
    /// every other cell.
    pub declared_corners: Vec<String>,
}

/// What one `compact()` did. v0.6.14 (N1): the cut only goes forward — a
/// call asking a cut BELOW the one already made does nothing (it is not
/// an error: Homun's automatic call must not fail when `keepLast` grows)
/// and answers `compacted: false` with the TRUE cut in `cutSeq`.
#[napi(object)]
pub struct CompactView {
    /// Did THIS call compact? `false`: the cut asked was below the one
    /// already made, and nothing moved (v0.6.14).
    pub compacted: bool,
    /// The cut in force after the call (the true cut, also when nothing
    /// moved).
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
    /// v0.4.1 (P1.6): the actor this cell's writes are stamped with, instead
    /// of the engine's (`homun:bot-<id>`). The cell underneath is the same:
    /// two Cell objects with different actors write the same memory.
    pub actor: Option<String>,
    /// v0.6.15 (F2): the experimental features this Cell opts into. Today
    /// one: `'compaction'` — `compact()` refuses with `EXPERIMENTAL` on a
    /// cell opened without it, because the declared defects of the cut
    /// (`crates/engramma-core/tests/open-defects.txt`) are open. Nothing
    /// else reads this list.
    pub experimental: Option<Vec<String>>,
}

/// v0.6.15 (F2): does the open-options list ask for the experimental
/// compaction? An unknown feature name is IGNORED (not an error): the list
/// is a plain opt-in, and the caller that names a feature this binding
/// does not have loses nothing but that feature.
fn wants_compaction(experimental: Option<&Vec<String>>) -> bool {
    experimental.is_some_and(|v| v.iter().any(|s| s == "compaction"))
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
        // v0.4.1 (P1.8): an EXPLICIT `active` on an `external` note is the
        // caller's CONFLICT (the core's guard answers, only `confirm`
        // promotes) — the v0.4 binding quarantined it in silence. Without a
        // status the note is born quarantined, as ever.
        Some("active") if confidence == Confidence::External => NoteStatus::Active,
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
    // v0.4.1 (P1.4): the import dates, when given (the future check lives in
    // the put path, which knows whether the note is new)
    let created_at = match &input.created_at {
        Some(s) => parse_import_at(s, "createdAt")?,
        None => now,
    };
    let updated_at = match &input.updated_at {
        Some(s) => parse_import_at(s, "updatedAt")?,
        None => now,
    };
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
        created_at,
        updated_at,
        extensions,
    })
}

/// v0.4.1 (P1.4): an RFC 3339 instant from the JS side.
fn parse_import_at(s: &str, field: &str) -> Result<At> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(At)
        .map_err(|e| invalid(format!("{field} non è un istante RFC 3339 valido: {e}")))
}

/// The import dates are for the past: a note the migration brings in was
/// written before now. Five minutes of tolerance, then INVALID_NOTE.
fn reject_future_import_dates(input: &NoteInput) -> Result<()> {
    let limit = OffsetDateTime::now_utc() + time::Duration::minutes(5);
    for (field, v) in [
        ("createdAt", &input.created_at),
        ("updatedAt", &input.updated_at),
    ] {
        if let Some(s) = v {
            let at = parse_import_at(s, field)?;
            if at.0 > limit {
                return Err(invalid(format!("{field} non può essere nel futuro: {s}")));
            }
        }
    }
    Ok(())
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

/// v0.4.2 (P0.1): an explicit `k` is respected AS GIVEN — the old silent
/// `clamp(1, 100)` cut Homun's pages without a word. Zero is a caller's
/// bug, not an empty answer: INVALID_ARGUMENT.
fn parse_k(k: Option<u32>) -> Result<usize> {
    match k {
        None => Ok(10),
        Some(0) => Err(invalid_argument("k deve essere almeno 1")),
        Some(n) => Ok(n as usize),
    }
}

fn build_query(q: &SearchInput) -> Result<Query> {
    if q.text.trim().is_empty() {
        return Err(invalid_argument("query senza testo"));
    }
    let mut query = Query::text(&q.text).k(parse_k(q.k)?);
    // the engine's own range is 0–2: beyond it is the caller's mistake to
    // hear about, never a silent `min(2)`
    let hops = q.graph_hops.unwrap_or(1);
    if hops > 2 {
        return Err(invalid_argument(format!(
            "graphHops {hops} fuori range (0–2)"
        )));
    }
    query.graph_hops = hops;
    if let Some(as_of) = q.as_of.as_deref().filter(|s| !s.trim().is_empty()) {
        let t = OffsetDateTime::parse(as_of, &time::format_description::well_known::Rfc3339)
            .or_else(|_| Validity::parse(as_of).map(|v| v.start()))
            .map_err(|e| {
                invalid_argument(format!("asOf non valida (RFC 3339 o YYYY-MM-DD): {e}"))
            })?;
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
                note_type: h.note.note_type.as_str().to_string(),
                tags: h.note.tags.clone(),
                updated_at: rfc3339(h.note.updated_at),
                status: h.note.status.as_str().to_string(),
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
            return Err(invalid_argument(format!(
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
// the closed gate (v0.4.1, P1.7; a verdict per gate since v0.4.2, P0.3)
// ---------------------------------------------------------------------------

/// What a sealed gate answers: the engine's says `CLOSED`, a dropped cell
/// generation's says `NOT_FOUND`.
#[derive(Clone)]
struct GateVerdict {
    code: &'static str,
    message: String,
}

/// v0.4.1 (P1.7): after `close`, no silent reopening. Every task ENTERS the
/// gate before touching the engine (a closed gate answers `CLOSED`), holds
/// it for the whole compute, and `close` seals the gate and WAITS until the
/// last in-flight compute is done — a late promise in the daemon can never
/// write to a closed engine. v0.4.2 (P0.3): the same machinery seals a CELL
/// generation — `dropCell` seals it with a `NOT_FOUND` verdict, so the
/// `Cell` objects already handed out stop working without an exception in
/// the dark, while a fresh `cell(name)` mints a new, open gate.
#[derive(Clone)]
pub struct Gate {
    closed: Arc<std::sync::atomic::AtomicBool>,
    in_flight: Arc<(Mutex<usize>, std::sync::Condvar)>,
    verdict: Arc<GateVerdict>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::with_verdict(
            "CLOSED",
            "motore chiuso: `Engine.close` è già stato chiamato",
        )
    }
}

impl Gate {
    fn with_verdict(code: &'static str, message: impl Into<String>) -> Self {
        Gate {
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            in_flight: Arc::new((Mutex::new(0), std::sync::Condvar::new())),
            verdict: Arc::new(GateVerdict {
                code,
                message: message.into(),
            }),
        }
    }

    /// A dropped cell generation's gate (v0.4.2, P0.3).
    fn dropped_cell(name: &str) -> Self {
        Self::with_verdict(
            "NOT_FOUND",
            format!("cella «{name}» dimenticata da `dropCell`"),
        )
    }

    /// Enter at CALL time, on the JS thread: the count rises before `close`
    /// can seal, so every promise that already exists when close is called
    /// finishes its work, and close waits for it. The `closed` check runs
    /// INSIDE the mutex `seal_and_wait` waits on — a gate sealed first is
    /// seen first, with no window in between.
    fn enter_now(&self) -> Result<GateGuard> {
        let mut n = self
            .in_flight
            .0
            .lock()
            .map_err(|_| transport_err("IO", "lock del gate avvelenato"))?;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            let v = &self.verdict;
            return Err(transport_err(v.code, v.message.clone()));
        }
        *n += 1;
        Ok(GateGuard {
            inner: Arc::clone(&self.in_flight),
        })
    }

    /// Seal the gate and wait for the in-flight computes to finish.
    fn seal_and_wait(&self) -> Result<()> {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        let (lock, cvar) = &*self.in_flight;
        let mut n = lock
            .lock()
            .map_err(|_| transport_err("IO", "lock del gate avvelenato"))?;
        while *n > 0 {
            n = cvar
                .wait(n)
                .map_err(|_| transport_err("IO", "lock del gate avvelenato"))?;
        }
        Ok(())
    }
}

/// Drops the in-flight count and wakes `seal_and_wait`.
struct GateGuard {
    inner: Arc<(Mutex<usize>, std::sync::Condvar)>,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        if let Ok(mut n) = self.inner.0.lock() {
            *n = n.saturating_sub(1);
        }
        self.inner.1.notify_all();
    }
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

/// The engine's embedder slot (v0.4.2, P1.4): `None` until the model is
/// there (at open or after a pull), the loaded builtin from then on.
type EmbedderSlot = Arc<std::sync::RwLock<Option<Arc<dyn Embedder>>>>;

/// v0.4.2 (P1.4): what the slot holds RIGHT NOW. Read once per task, at
/// compute time — a search that started before an attach keeps its branch.
fn embedder_now(slot: &EmbedderSlot) -> Result<Option<Arc<dyn Embedder>>> {
    Ok(slot
        .read()
        .map_err(|_| transport_err("IO", "lock dell'embedder avvelenato"))?
        .clone())
}

/// Everything an open engine shares: the cell cache, the optional builtin
/// embedder, the actor every write is stamped with.
#[derive(Clone)]
pub struct EngineHandle {
    root: PathBuf,
    model_dir: Option<PathBuf>,
    actor: Arc<String>,
    /// v0.4.2 (P1.4): the SHARED embedder slot. Until v0.4.1 the embedder
    /// bound at open: after a `pullModel` the OPEN engine kept answering
    /// text+graph and `embedPending` said MODEL_MISSING until close/open —
    /// Homun restarted its daemon's engine to work around it. Now the pull
    /// fills this slot while the engine stays open, and every task reads it
    /// WHEN IT RUNS: a search already in flight during the attach finishes
    /// with the branch it had, the next one runs on the vectors.
    embedder: EmbedderSlot,
    /// Whether this engine wants the builtin embedder at all (`embedder:
    /// 'none'` is an explicit choice: the pull downloads, never attaches).
    want_builtin: bool,
    cells: Arc<Mutex<HashMap<String, CoreCell>>>,
    /// v0.4.2 (P0.3): one gate per cell GENERATION. `dropCell` removes the
    /// entry holding this map's lock for the whole drop, so the in-flight
    /// operations of that generation finish first and every Cell object of
    /// it answers `NOT_FOUND` from the seal on; a `cell(name)` that arrives
    /// mid-drop waits on this same lock and then mints a FRESH gate over a
    /// fresh empty cell — there is no third state. v0.4.3 (P2.3): the open
    /// takes gate and cell under this same lock, so no Cell is ever born
    /// with a gate a completed drop had already sealed (born dead).
    trackers: Arc<Mutex<HashMap<String, Arc<Gate>>>>,
    /// v0.6.2 (S2): this engine's sync service, started at the first
    /// `sync()` and shared by every Sync object — one service per root,
    /// one event hub to fan the events out to JS.
    sync: Arc<Mutex<Option<Arc<SyncShared>>>>,
    gate: Gate,
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
        // v0.4.3 (P0.1): the snapshot-shaped name is reserved. A cell called
        // `x-snap-7` would be indistinguishable from a snapshot of `x`: its
        // file would ride with x's dropCell and `cells()` could never list
        // it. `storia-snap-archivio` stays valid (the suffix is not digits)
        if engramma_core::snapshot_of(&format!("{name}.sqlite")).is_some() {
            return Err(invalid_argument(format!(
                "nome cella riservato: {name:?} (il formato `-snap-<cifre>` \
                 è delle istantanee di compattazione)"
            )));
        }
        // v0.4.4 (P2): on Windows `con.sqlite` still resolves to the device:
        // the classic reserved names cannot be cell file names there. The
        // check is gated so macOS and Linux keep accepting them.
        #[cfg(windows)]
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        ) {
            return Err(invalid_argument(format!(
                "nome cella riservato su Windows: {name:?}"
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

    /// The generation gate AND the cell, under the SAME generations lock
    /// (v0.4.2 P0.3 took the gate first and opened the cell after, lock
    /// released in between — v0.4.3 P2.3 closes that window). A dropCell
    /// entering it sealed the very gate the open was about to hand out, and
    /// the returning Cell answered `NOT_FOUND` forever: born dead, a fresh
    /// Cell over a fresh file. Worse, the open could CREATE the file while
    /// a drop was unlinking it, and the drop died on the half-written
    /// database. With the lock held across both, a drop of that name either
    /// finishes BEFORE the open (which then mints a fresh gate over a fresh
    /// cell) or waits for it and takes the generation THIS open created: a
    /// Cell can only die of an oblivion that started after it existed. The
    /// lock is per engine, not per name — a drop of one name briefly waits
    /// an unrelated open; both are rare events in Homun's daemon.
    fn open_tracked_cell(&self, name: &str, create: bool) -> Result<(CoreCell, Arc<Gate>)> {
        let mut map = self
            .trackers
            .lock()
            .map_err(|_| transport_err("IO", "lock delle generazioni avvelenato"))?;
        let tracker = map
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Gate::dropped_cell(name)))
            .clone();
        let cell = self.open_cell(name, create)?;
        drop(map);
        Ok((cell, tracker))
    }
}

/// Every file a cell owns on disk (v0.4.2, P0.3): the database, its WAL and
/// shm, and the compaction snapshots (`<name>-snap-<seq>.sqlite`, written
/// next to the cell by `compact`). A whole-cell forgetting deletes them all.
/// v0.4.3 (P0.1): the snapshots are the EXACT compact format — one
/// predicate in the core — so a neighboring cell called
/// `storia-snap-archivio` never rides with storia's drop.
fn cell_files(path: &std::path::Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut p = path.to_path_buf().into_os_string();
        p.push(suffix);
        out.push(PathBuf::from(p));
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if let Some(dir) = path.parent()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        let mut snaps: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    engramma_core::snapshot_of(n).is_some_and(|(cell, _)| cell == stem)
                })
            })
            .collect();
        snaps.sort();
        out.extend(snaps);
    }
    out
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
            embedder: Arc::new(std::sync::RwLock::new(embedder)),
            want_builtin,
            cells: Arc::new(Mutex::new(HashMap::new())),
            trackers: Arc::new(Mutex::new(HashMap::new())),
            sync: Arc::new(Mutex::new(None)),
            gate: Gate::default(),
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
    actor: Option<String>,
    /// v0.6.15 (F2): whether this Cell may call `compact()` (the explicit
    /// `experimental: ['compaction']` of the open).
    experimental_compaction: bool,
    _open: GateGuard,
}

impl Task for CellOpenTask {
    type Output = (CoreCell, String, EmbedderSlot, String, Gate, Arc<Gate>);
    type JsValue = Cell;

    fn compute(&mut self) -> Result<Self::Output> {
        // v0.4.2 (P0.3): the generation gate comes first, so a dropCell in
        // flight serializes against every open of that name. v0.4.3 (P2.3):
        // gate AND open under the SAME lock — see open_tracked_cell
        let (cell, tracker) = self.handle.open_tracked_cell(&self.name, self.create)?;
        // v0.6.2 (S4): a SHARED cell's local writes carry their device's
        // signature, or the peers' gate refuses them (the CLI arms the
        // signer at every open; the binding arms at open and at share)
        arm_shared_signer(&self.handle.root, &cell, &self.name);
        // v0.6.3 (C5.3): and again at every WRITE, when the registry moved
        // under the open cell — a share by the CLI in another process
        // arms the first write after it. ENGRAMMA_BENCH_NO_SIGNER_GATE
        // skips the gate — a BENCH-ONLY variable (D6.1 of v0.6.4: what
        // the one-stat-per-write gate costs on a root that never synced)
        if std::env::var("ENGRAMMA_BENCH_NO_SIGNER_GATE").is_err() {
            let _ = cell.set_signer_gate(signer_gate_for(&self.handle.root, &self.name));
        }
        let actor = match &self.actor {
            // v0.4.1 (P1.6): an empty actor is the same as none — the
            // engine's own stamps the writes
            Some(a) if !a.trim().is_empty() => a.trim().to_string(),
            _ => (*self.handle.actor).clone(),
        };
        Ok((
            cell,
            self.name.clone(),
            Arc::clone(&self.handle.embedder),
            actor,
            self.handle.gate.clone(),
            tracker,
        ))
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        let compaction = self.experimental_compaction;
        Ok(Cell::from_parts(
            out.0, out.1, out.2, out.3, out.4, out.5, compaction,
        ))
    }

    reject_recode!();
}

pub struct CellsTask {
    root: PathBuf,
    _open: GateGuard,
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
            // v0.4.3 (P0.1): a compaction snapshot is not a cell. A cell can
            // no longer have that name (reserved in cell_path), so a file in
            // that shape is ALWAYS a snapshot — it does not list
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| engramma_core::snapshot_of(n).is_some())
            {
                continue;
            }
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
    _open: GateGuard,
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
    /// v0.4.2 (P1.4): the slot to fill when the pull succeeds and this
    /// engine wants the builtin embedder.
    embedder: EmbedderSlot,
    want_builtin: bool,
    _open: GateGuard,
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
        // v0.4.2 (P1.5): ENGRAMMA_PULL_BASE replaces the download base —
        // Homun's request 1: offline installs and closed-environment
        // mirrors, like the CLI's tests already do with a local server.
        // The pinned revision and the per-file sha256 stay attached: a
        // different base can only deliver the SAME bytes or fail.
        let base = std::env::var("ENGRAMMA_PULL_BASE")
            .ok()
            .filter(|b| !b.trim().is_empty());
        let mut cb = |done: u64, total: u64| {
            if let Some(p) = self.progress.as_mut() {
                p(done, total);
            }
        };
        engramma_embed::pull::pull_progress(
            &dir,
            &spec,
            base.as_deref(),
            std::time::Duration::from_secs(600),
            &mut cb,
        )
        .map_err(|e| transport_err("IO", format!("pull: {e}")))?;
        // v0.4.2 (P1.4): the pull ends with the model VERIFIED — the open
        // engine loads the embedder now, no close/open. Homun used to
        // restart its daemon's engine for this. When the load fails the
        // pull REJECTS (IO) and the slot stays empty: the engine keeps
        // answering text+graph, working, and the next pull retries.
        // `embedder: 'none'` is an explicit choice: the pull downloads, it
        // never attaches.
        if self.want_builtin {
            let loaded =
                engramma_embed::Builtin::load_named(&dir, "granite-embedding-97m-multilingual-r2")
                    .map_err(|e| {
                        transport_err("IO", format!("il modello scaricato non si carica: {e}"))
                    })?;
            *self
                .embedder
                .write()
                .map_err(|_| transport_err("IO", "lock dell'embedder avvelenato"))? =
                Some(Arc::new(loaded) as Arc<dyn Embedder>);
        }
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
    _open: GateGuard,
}

impl Task for EngineSearchTask {
    type Output = Vec<SearchHit>;
    type JsValue = Vec<SearchHit>;

    fn compute(&mut self) -> Result<Self::Output> {
        if self.cells.is_empty() {
            return Err(invalid_argument("serve almeno una cella"));
        }
        let k = parse_k(self.q.k)?;
        // v0.4.2 (P1.4): read ONCE — the slot can fill mid-flight (a pull
        // that lands), and this search runs with the branch it started with
        let embedder = embedder_now(&self.handle.embedder)?;
        let mut fused: HashMap<String, (f64, SearchHit)> = HashMap::new();
        for name in &self.cells {
            let cell = self.handle.open_cell(name, false)?;
            let hits = search_cell(&cell, name, &self.q, embedder.as_ref())?;
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
    gate: Gate,
    /// v0.6.2 (S4): the sync service of this engine, if one was started —
    /// `close` stops it (a daemon that exits with the service on must
    /// not hang on a thread of ours).
    sync: Arc<Mutex<Option<Arc<SyncShared>>>>,
}

impl Task for CloseTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        // v0.6.2 (S4): the service goes FIRST — its own shutdown is
        // bounded, and after it no thread of ours holds the root
        if let Some(shared) = self
            .sync
            .lock()
            .map_err(|_| transport_err("IO", "lock della sync avvelenato"))?
            .take()
        {
            shared.stop();
        }
        // v0.4.1 (P1.7): seal first, then WAIT — the computes already in
        // flight finish before this close resolves, and no later task can
        // enter. Closing twice is fine: the gate is already sealed.
        self.gate.seal_and_wait()?;
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

/// v0.4.2 (P0.3): what one whole-cell drop says — shaped like [`PurgeView`],
/// for the same reason: an oblivion that cannot reach a view is a RETURN
/// VALUE, never an exception.
#[napi(object)]
pub struct DropView {
    /// The notes the cell carried — all of them, tombstones included.
    pub notes: i32,
    pub scrubbed: bool,
    /// What still holds bytes (`vault`): the caller finishes the forgetting.
    pub pending: Vec<String>,
    /// The views that could not be reached (or asked), one entry per
    /// problem — no note contents ride in them.
    pub views: Vec<PurgeProblemView>,
}

pub struct DropCellTask {
    handle: EngineHandle,
    name: String,
    reason: String,
    _open: GateGuard,
}

impl Task for DropCellTask {
    type Output = engramma_vault::DropReport;
    type JsValue = DropView;

    fn compute(&mut self) -> Result<Self::Output> {
        if self.reason.trim().is_empty() {
            return Err(invalid_argument("reason è obbligatoria"));
        }
        // v0.6.2 (S2): a SHARED cell does not go quietly — the service
        // would keep syncing a deleted file. Say it, refuse it. (One stat
        // first: a root without devices.db never shared anything, and the
        // drop path must stay as fast as it was for the engines that
        // never sync.)
        if self.handle.root.join("devices.db").exists()
            && engramma_sync::service::cell_is_shared(&self.handle.root, &self.name)
                .unwrap_or(false)
        {
            return Err(transport_err(
                "CONFLICT",
                "la cella è condivisa: `unshare` prima di `dropCell`",
            ));
        }
        let path = self.handle.cell_path(&self.name)?;
        if !path.exists() {
            return Err(not_found(format!("cella {} inesistente", self.name)));
        }
        // The generations map stays LOCKED for the whole drop. The entry
        // goes first and its gate is SEALED: the operations of that cell
        // already in flight finish (the seal waits for them), and every
        // Cell object of that generation answers NOT_FOUND from here on. A
        // `cell(name)` that arrives now waits on this same lock and then
        // mints a FRESH generation over a fresh cell — never a ghost over a
        // deleted file.
        let mut generations = self
            .handle
            .trackers
            .lock()
            .map_err(|_| transport_err("IO", "lock delle generazioni avvelenato"))?;
        if let Some(tracker) = generations.remove(&self.name) {
            tracker.seal_and_wait()?;
        }
        // the cell (from the cache when there, from disk otherwise), then
        // OUT of the cache: the engine must not hand the doomed cell out
        let cell = self.handle.open_cell(&self.name, false)?;
        self.handle
            .cells
            .lock()
            .map_err(|_| transport_err("IO", "lock delle celle avvelenato"))?
            .remove(&self.name);
        // every view loses the files it holds for this cell's notes, now —
        // an unreachable view is a problem in the report, never a throw
        let report = engramma_vault::drop_cell_everywhere(&cell).map_err(|e| match e {
            engramma_vault::VaultError::Cell(c) => cell_error(c),
            other => transport_err("IO", format!("drop: {other}")),
        })?;
        // the files: the cell, its WAL, its shm, every snapshot. The handle
        // goes first (an open connection would keep writing a -wal nobody
        // can reach anymore); other processes' handles are out of scope —
        // one engine per process is the documented shape. v0.4.3 (P1.2):
        // RETIRE, not just drop — the clones of this handle (a Cell object
        // still alive in JS, for as long as the GC takes) share one Inner,
        // and the unlinked .sqlite/-wal/-shm would stay open under them.
        // retire closes the real connection for every clone at once
        cell.retire().map_err(cell_error)?;
        drop(cell);
        for file in cell_files(&path) {
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(transport_err("IO", format!("{}: {e}", file.display()))),
            }
        }
        drop(generations);
        Ok(report)
    }

    fn resolve(&mut self, _env: Env, r: Self::Output) -> Result<DropView> {
        Ok(DropView {
            notes: i32::try_from(r.notes).unwrap_or(i32::MAX),
            scrubbed: r.pending.is_empty(),
            pending: r.pending.iter().map(|s| s.to_string()).collect(),
            views: r
                .problems
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
    pub fn cell(
        &self,
        env: Env,
        name: String,
        opts: Option<CellOpts>,
    ) -> Result<AsyncTask<CellOpenTask>> {
        Ok(AsyncTask::new(CellOpenTask {
            handle: self.handle.clone(),
            name,
            create: opts.as_ref().and_then(|o| o.create).unwrap_or(true),
            actor: opts.as_ref().and_then(|o| o.actor.clone()),
            experimental_compaction: wants_compaction(
                opts.as_ref().and_then(|o| o.experimental.as_ref()),
            ),
            _open: self.handle.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    /// The names of the cells that exist under root/cells.
    #[napi(ts_return_type = "Promise<string[]>")]
    pub fn cells(&self, env: Env) -> Result<AsyncTask<CellsTask>> {
        Ok(AsyncTask::new(CellsTask {
            root: self.handle.root.clone(),
            _open: self.handle.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    /// Is the builtin model there? (id and bytes when it is)
    #[napi(ts_return_type = "Promise<ModelStatus>")]
    pub fn model_status(&self, env: Env) -> Result<AsyncTask<ModelStatusTask>> {
        Ok(AsyncTask::new(ModelStatusTask {
            model_dir: self.handle.model_dir.clone(),
            _open: self.handle.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    /// Download the builtin model (pinned revision, sha256 verified per
    /// chunk). Already-present files verify and skip: offline it is a no-op.
    /// v0.4.2 (P1.4): when the model is verified, the OPEN engine loads the
    /// embedder and uses it from then on — `modelStatus().present`, hybrid
    /// search on the vectors, `embedPending` working — without any
    /// close/open. A load failure rejects with `IO` and the engine keeps
    /// answering text+graph. An engine opened with `embedder: 'none'`
    /// downloads but never attaches (the explicit choice stands).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn pull_model(
        &self,
        env: Env,
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
            embedder: Arc::clone(&self.handle.embedder),
            want_builtin: self.handle.want_builtin,
            _open: self.handle.gate.enter_now().map_err(|e| recode(env, e))?,
            progress,
        }))
    }

    /// Search ACROSS cells: each cell's ranking, fused with RRF (k = 60).
    /// Approximate by declaration — the scores of different cells do not
    /// compare.
    #[napi(ts_return_type = "Promise<SearchHit[]>")]
    pub fn search(
        &self,
        env: Env,
        cells: Vec<String>,
        q: SearchInput,
    ) -> Result<AsyncTask<EngineSearchTask>> {
        Ok(AsyncTask::new(EngineSearchTask {
            handle: self.handle.clone(),
            cells,
            q,
            _open: self.handle.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    /// v0.4.2 (P0.3): forget a WHOLE cell — its notes, its files (WAL and
    /// snapshots included) and the note files of its bound views. The
    /// `Cell` objects already handed out answer `NOT_FOUND`; a new
    /// `cell(name)` creates a fresh empty one. Operations in flight on that
    /// cell finish first, like `close`. The `reason` is the caller's audit
    /// trail (the engine keeps no registry: nothing of the cell stays
    /// behind to write one into).
    #[napi(ts_return_type = "Promise<DropView>")]
    pub fn drop_cell(
        &self,
        env: Env,
        name: String,
        reason: String,
    ) -> Result<AsyncTask<DropCellTask>> {
        Ok(AsyncTask::new(DropCellTask {
            handle: self.handle.clone(),
            name,
            reason,
            _open: self.handle.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    /// Release the engine's cell handles. Outstanding Cell objects keep
    /// working until the GC drops them.
    /// The sync service of this root: one per Engine, started at the
    /// first call and shared by every Sync object (v0.6.2, S2).
    #[napi(ts_return_type = "Promise<Sync>")]
    pub fn sync(&self, env: Env, opts: Option<SyncOpts>) -> Result<AsyncTask<SyncOpenTask>> {
        Ok(AsyncTask::new(SyncOpenTask {
            handle: self.handle.clone(),
            opts: opts.unwrap_or(SyncOpts {
                device_name: None,
                relay: None,
            }),
            _open: self.handle.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn close(&self) -> AsyncTask<CloseTask> {
        AsyncTask::new(CloseTask {
            cells: self.handle.cells.clone(),
            gate: self.handle.gate.clone(),
            sync: Arc::clone(&self.handle.sync),
        })
    }
}

// ---------------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------------

/// One cell of memory. Usable from many promises in parallel: writes on
/// the same cell serialize inside the engine (the cell's own lock).
/// One open conflict copy (v0.6.4, D3): who lost, when, in quarantine
/// or not. No text — the copy is a note, read it with `get`.
#[napi(object)]
pub struct ConflictEntry {
    /// The copy's note id.
    pub copy_id: String,
    /// The original the copy lost against.
    pub original_id: String,
    /// Who wrote the losing body.
    pub loser_actor: String,
    /// `human` | `agent`.
    pub loser_class: String,
    /// The loser's HLC, RFC 3339.
    pub loser_at: String,
    /// An agent's copy is born in quarantine, in the queue.
    pub quarantined: bool,
    /// Inside the cap's window of three. The others stay open and appear
    /// when a visible one closes.
    pub visible: bool,
}

/// One open conflict copy WITH ITS TEXTS (v0.6.5, E5): what a choice is
/// about. The quarantined copies are readable here and only here — `get`
/// answers `null` for them, as for every hidden note.
#[napi(object)]
pub struct ConflictDetailEntry {
    /// The copy's note id.
    pub copy_id: String,
    /// The original the copy lost against.
    pub original_id: String,
    /// Who wrote the losing body.
    pub loser_actor: String,
    /// `human` | `agent`.
    pub loser_class: String,
    /// The loser's HLC, RFC 3339.
    pub loser_at: String,
    /// An agent's copy is born in quarantine, in the queue.
    pub quarantined: bool,
    /// Inside the cap's window of three.
    pub visible: bool,
    /// v0.6.7 (G2): the original holds an active lapide — the copy is the
    /// bin's content (not in `conflicts()`, not searchable); this read by
    /// id says so.
    pub original_deleted: bool,
    /// The ORIGINAL's body as it stands (the left column).
    pub original_body: Option<String>,
    /// The LOSER's body — the copy's own text (the right column).
    pub copy_body: Option<String>,
    /// The body at the fork between the loser and the winner (the middle
    /// column); `null` under the compaction cut.
    pub fork_body: Option<String>,
}

#[napi(js_name = "Cell")]
pub struct Cell {
    cell: CoreCell,
    name: String,
    /// v0.4.2 (P1.4): the ENGINE's embedder slot, read when a task runs —
    /// a cell opened before a `pullModel` embeds and searches on the
    /// vectors from the moment the model lands.
    embedder: EmbedderSlot,
    actor: String,
    gate: Gate,
    /// v0.4.2 (P0.3): this cell's GENERATION gate — sealed (NOT_FOUND) by
    /// `dropCell`, while a fresh `cell(name)` mints a new one. The Cell
    /// objects of a dropped generation stop working without an exception
    /// in the dark.
    tracker: Arc<Gate>,
    /// v0.6.15 (F2): whether `compact()` may run — the cell was opened with
    /// `experimental: ['compaction']`. The refusal is the binding's, not
    /// the engine's: the engine stays as it is.
    experimental_compaction: bool,
}

/// The two permits every Cell task holds (v0.4.2, P0.3): the engine's gate
/// (`CLOSED` after close) and this cell generation's gate (`NOT_FOUND`
/// after dropCell). Both taken at CALL time, on the JS thread — an
/// operation already in flight when the drop arrives finishes first.
struct CellPermits {
    _engine: GateGuard,
    _generation: GateGuard,
}

impl Cell {
    fn from_parts(
        cell: CoreCell,
        name: String,
        embedder: EmbedderSlot,
        actor: String,
        gate: Gate,
        tracker: Arc<Gate>,
        experimental_compaction: bool,
    ) -> Self {
        Cell {
            cell,
            name,
            embedder,
            actor,
            gate,
            tracker,
            experimental_compaction,
        }
    }

    fn enter(&self, env: Env) -> Result<CellPermits> {
        let engine = self.gate.enter_now().map_err(|e| recode(env, e))?;
        let generation = self.tracker.enter_now().map_err(|e| recode(env, e))?;
        Ok(CellPermits {
            _engine: engine,
            _generation: generation,
        })
    }
}

pub struct PutTask {
    cell: CoreCell,
    actor: String,
    input: NoteInput,
    _open: CellPermits,
}

impl Task for PutTask {
    type Output = Note;
    type JsValue = NoteView;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = match &self.input.id {
            Some(s) => parse_ulid(s)?,
            None => engramma_core::id::new_ulid().map_err(cell_error)?,
        };
        let existing = self.cell.get(&id).ok();
        // v0.4.1 (P1.4): the import dates are for a NEW note (never in the
        // future); a rewrite ignores both — the birth date is the note's own
        if existing.is_none() {
            reject_future_import_dates(&self.input)?;
        }
        let mut note = build_note(&self.input, id)?;
        // v0.4.1 (P0.1): a rewrite PRESERVES the note's state. The caller
        // that does not ask for a status gets the preserved one (the core
        // recomputes and enforces it); an explicit request that disagrees
        // with the preserved state is the caller's CONFLICT to hear about.
        if let Some(old) = &existing
            && old.status != NoteStatus::Deleted
        {
            if self.input.status.is_none() {
                note.status = if old.status == NoteStatus::Active
                    && note.confidence == Confidence::External
                {
                    NoteStatus::Quarantined
                } else {
                    old.status
                };
                if note.status == NoteStatus::Superseded && note.valid_to.is_none() {
                    note.valid_to = old.valid_to;
                }
            }
            note.created_at = old.created_at;
            note.updated_at = At(OffsetDateTime::now_utc());
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
    _open: CellPermits,
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
    _open: CellPermits,
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
    _open: CellPermits,
}

impl Task for ListTask {
    type Output = Vec<Note>;
    type JsValue = Vec<NoteView>;

    fn compute(&mut self) -> Result<Self::Output> {
        let want_type = self.note_type.as_deref().map(parse_note_type).transpose()?;
        // v0.4.2 (P0.2): a status the engine does not know is the caller's
        // mistake to hear about — the old filter_map dropped it and the
        // empty filter let NOTHING through: `list({ status: ['deleted'] })`
        // answered [] with no error, and a deleted note kept its text in
        // the history forever with no way to find it. `deleted` is a first
        // class status here: a tombstone lists as its last version (the row
        // keeps the content until a purge forgets it), which is what the
        // user sees to decide restore or forget. `get` stays a closed
        // oracle: null for quarantined and tombstoned, as ever.
        let wanted: Option<Vec<NoteStatus>> = match self.statuses.as_ref() {
            None => None,
            Some(ss) => Some(
                ss.iter()
                    .map(|s| match s.as_str() {
                        "active" => Ok(NoteStatus::Active),
                        "quarantined" => Ok(NoteStatus::Quarantined),
                        "superseded" => Ok(NoteStatus::Superseded),
                        "deleted" => Ok(NoteStatus::Deleted),
                        other => Err(invalid_argument(format!(
                            "status non valida: {other:?} \
                             (active|quarantined|superseded|deleted)"
                        ))),
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        let offset = self.offset.unwrap_or(0) as usize;
        // v0.4.2 (P0.1): an explicit limit is respected AS GIVEN — the old
        // silent `clamp(1, 1000)` answered Homun's `limit: 100_000` with a
        // thousand notes and the log said «dimenticato». This is a local
        // process, the ids are already in memory: no ceiling at all. Zero is
        // a caller's bug: INVALID_ARGUMENT, not an empty page.
        let limit = match self.limit {
            Some(0) => return Err(invalid_argument("limit deve essere almeno 1")),
            Some(n) => n as usize,
            None => 100,
        };
        // the tombstones sit below note_ids' living-only view: ask for every
        // row when the caller asked for a status at all (the filter above
        // keeps the living-only answer when `deleted` is not among them)
        let ids = if self.statuses.is_some() {
            self.cell.note_ids_all().map_err(cell_error)?
        } else {
            self.cell.note_ids().map_err(cell_error)?
        };
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
    embedder: EmbedderSlot,
    q: SearchInput,
    _open: CellPermits,
}

impl Task for SearchTask {
    type Output = Vec<SearchHit>;
    type JsValue = Vec<SearchHit>;

    fn compute(&mut self) -> Result<Self::Output> {
        // v0.4.2 (P1.4): read ONCE, at compute time — the cell may have been
        // opened before the pull that fills the slot
        let embedder = embedder_now(&self.embedder)?;
        search_cell(&self.cell, &self.name, &self.q, embedder.as_ref())
    }

    fn resolve(&mut self, _env: Env, hits: Vec<SearchHit>) -> Result<Vec<SearchHit>> {
        Ok(hits)
    }

    reject_recode!();
}

pub struct HistoryTask {
    cell: CoreCell,
    id: String,
    _open: CellPermits,
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
    _open: CellPermits,
}

impl Task for NeighborsTask {
    type Output = Vec<NeighborView>;
    type JsValue = Vec<NeighborView>;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = resolve_id(&self.cell, &self.id)?;
        // v0.4.2 (P0.1): the engine's range is 1–4 — beyond it (or zero) is
        // the caller's mistake, never a silent `min(4)`
        if self.depth == 0 || self.depth > 4 {
            return Err(invalid_argument(format!(
                "depth {} fuori range (1–4)",
                self.depth
            )));
        }
        let ns = self
            .cell
            .neighbors(&format!("note:{id}"), self.depth, &[], false)
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
    _open: CellPermits,
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
            // v0.4.2 (P0.2): a wrong ARGUMENT of the call, not a note
            // problem — deleting is `delete`
            other => {
                return Err(invalid_argument(format!(
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
    _open: CellPermits,
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
    _open: CellPermits,
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

/// v0.6.13 (M1): what `propose` is asked to store. The note is born
/// quarantined whatever its `status` says; `mode` is `new` or
/// `supersede` (with `target`).
#[napi(object)]
pub struct ProposeInput {
    /// The note (same shape `put` takes).
    pub note: NoteInput,
    /// `new` (a fresh fact) or `supersede` (a correction of `target`).
    pub mode: String,
    /// The superseded note, when `mode` is `supersede`.
    pub target: Option<String>,
    /// The client the assistant DECLARED (`clientInfo`): registered,
    /// never an identity.
    pub client: Option<String>,
    /// The assistant's stated motivation.
    pub reason: Option<String>,
}

/// What `propose` did: `pending` (queued), `duplicate` (refused), or
/// `accepted`/`rejected` (a policy reviewer decided inside the call —
/// not on this surface today, the field is the contract's own).
#[napi(object)]
pub struct ProposalOutcome {
    /// `pending` | `duplicate` | `accepted` | `rejected`.
    pub status: String,
    /// The proposal's note id, when the proposal was written.
    pub id: Option<String>,
    /// The existing twin, on `duplicate`.
    pub existing: Option<String>,
    /// Who decided, when a reviewer did.
    pub decided_by: Option<String>,
    /// Why, when a reviewer did.
    pub decision_reason: Option<String>,
}

/// What `decide` is asked to do.
#[napi(object)]
pub struct DecideInput {
    /// `accept` (activate; `edited` may fix the text while accepting)
    /// or `reject` (tombstone — the text stays, the decision can be
    /// undone from the CLI).
    pub action: String,
    /// The text the human fixed while accepting, when any.
    pub edited: Option<NoteInput>,
    /// The motivation, kept with the decision.
    pub reason: Option<String>,
}

/// One row of the proposals queue.
#[napi(object)]
pub struct ProposalRow {
    /// The proposed note.
    pub id: String,
    /// `new` or `supersede`.
    pub mode: String,
    /// The superseded note, when a correction.
    pub target: Option<String>,
    /// Who proposed (the propose op's actor).
    pub proposer: String,
    /// The declared client, when any.
    pub client: Option<String>,
    /// The stated motivation, when any.
    pub reason: Option<String>,
    /// `pending` | `accepted` | `rejected` | `reverted`.
    pub state: String,
    /// When the proposal was made.
    pub proposed_at: String,
    /// Who decided, when decided.
    pub decided_by: Option<String>,
    /// When, when decided.
    pub decided_at: Option<String>,
    /// Why, when decided.
    pub decision_reason: Option<String>,
    /// An automatic review was attempted and failed: the proposal
    /// waits for a human.
    pub auto_failed: bool,
    /// The note as it stands.
    pub note: NoteView,
}

pub struct ProposeTask {
    cell: CoreCell,
    actor: String,
    input: ProposeInput,
    _open: CellPermits,
}

impl Task for ProposeTask {
    type Output = engramma_core::cell::proposals::ProposalOutcome;
    type JsValue = ProposalOutcome;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = match &self.input.note.id {
            Some(s) => parse_ulid(s)?,
            None => engramma_core::id::new_ulid().map_err(cell_error)?,
        };
        let note = build_note(&self.input.note, id)?;
        let mode = match (self.input.mode.as_str(), &self.input.target) {
            ("supersede", Some(t)) => {
                engramma_core::cell::proposals::ProposalMode::Supersede(resolve_id(&self.cell, t)?)
            }
            _ => engramma_core::cell::proposals::ProposalMode::New,
        };
        self.cell
            .propose(
                &engramma_core::cell::proposals::ProposalInput {
                    note,
                    mode,
                    client: self.input.client.clone(),
                    reason: self.input.reason.clone(),
                },
                &self.actor,
                &engramma_core::cell::proposals::ProposeContext::default(),
            )
            .map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(ProposalOutcome {
            status: match out.status {
                engramma_core::cell::proposals::ProposalStatus::Pending => "pending".into(),
                engramma_core::cell::proposals::ProposalStatus::Duplicate => "duplicate".into(),
                engramma_core::cell::proposals::ProposalStatus::Accepted => "accepted".into(),
                engramma_core::cell::proposals::ProposalStatus::Rejected => "rejected".into(),
            },
            id: out.id.map(|i| i.to_string()),
            existing: out.existing.map(|i| i.to_string()),
            decided_by: out.decided_by,
            decision_reason: out.decision_reason,
        })
    }

    reject_recode!();
}

pub struct DecideTask {
    cell: CoreCell,
    actor: String,
    id: String,
    action: DecideInput,
    _open: CellPermits,
}

impl Task for DecideTask {
    type Output = Ulid;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = resolve_id(&self.cell, &self.id)?;
        let action = match self.action.action.as_str() {
            "reject" => engramma_core::cell::proposals::DecisionAction::Reject,
            _ => {
                let edited = match &self.action.edited {
                    Some(e) => {
                        let eid = match &e.id {
                            Some(s) => parse_ulid(s)?,
                            None => id,
                        };
                        Some(build_note(e, eid)?)
                    }
                    None => None,
                };
                engramma_core::cell::proposals::DecisionAction::Accept { edited }
            }
        };
        self.cell
            .decide(
                &id,
                &engramma_core::cell::proposals::DecisionInput {
                    action,
                    reason: self.action.reason.clone(),
                    review: None,
                },
                &self.actor,
            )
            .map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(out.to_string())
    }

    reject_recode!();
}

pub struct ProposalsTask {
    cell: CoreCell,
    _open: CellPermits,
}

impl Task for ProposalsTask {
    type Output = Vec<engramma_core::cell::proposals::ProposalRow>;
    type JsValue = Vec<ProposalRow>;

    fn compute(&mut self) -> Result<Self::Output> {
        self.cell
            .proposals(&engramma_core::cell::proposals::ProposalsFilter::default())
            .map_err(cell_error)
    }

    fn resolve(&mut self, env: Env, out: Self::Output) -> Result<Self::JsValue> {
        let mut rows = Vec::with_capacity(out.len());
        for row in out {
            let note = note_view(&self.cell, &row.note)?;
            rows.push(ProposalRow {
                id: row.id.to_string(),
                mode: match row.mode {
                    engramma_core::cell::proposals::ProposalMode::New => "new".into(),
                    engramma_core::cell::proposals::ProposalMode::Supersede(_) => {
                        "supersede".into()
                    }
                },
                target: row.target.map(|t| t.to_string()),
                proposer: row.proposer,
                client: row.client,
                reason: row.reason,
                state: row.state.as_str().to_string(),
                proposed_at: row.proposed_at,
                decided_by: row.decided_by,
                decided_at: row.decided_at,
                decision_reason: row.decision_reason,
                auto_failed: row.auto_failed,
                note,
            });
        }
        let _ = env;
        Ok(rows)
    }

    reject_recode!();
}

pub struct PurgePreviewTask {
    cell: CoreCell,
    id: String,
    _open: CellPermits,
}

impl Task for PurgePreviewTask {
    type Output = Vec<engramma_core::cell::Reopening>;
    type JsValue = PurgePreview;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = parse_ulid(&self.id)?;
        self.cell.purge_preview(&id).map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, rows: Self::Output) -> Result<Self::JsValue> {
        Ok(PurgePreview {
            reopens: rows
                .into_iter()
                .map(|r| ReopenEntry {
                    id: r.id.to_string(),
                    title: r.title,
                })
                .collect(),
        })
    }
}

/// v0.6.11 (K1): the read-only preview of a purge.
#[napi(object)]
pub struct PurgePreview {
    /// The notes that would REOPEN — back to the state they had before
    /// the closing — when this note is forgotten.
    pub reopens: Vec<ReopenEntry>,
}

/// One reopening of [`PurgePreview`].
#[napi(object)]
pub struct ReopenEntry {
    /// The note that comes back.
    pub id: String,
    /// Its title, when it has one.
    pub title: Option<String>,
}

pub struct ConflictsTask {
    cell: CoreCell,
    _open: CellPermits,
}

impl Task for ConflictsTask {
    type Output = Vec<engramma_core::cell::conflicts::ConflictInfo>;
    type JsValue = Vec<ConflictEntry>;

    fn compute(&mut self) -> Result<Self::Output> {
        self.cell.conflicts().map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, rows: Self::Output) -> Result<Self::JsValue> {
        Ok(rows
            .into_iter()
            .map(|r| ConflictEntry {
                copy_id: r.copy_id.to_string(),
                original_id: r.original_id.to_string(),
                loser_actor: r.loser_actor.clone(),
                loser_class: match r.loser_class {
                    engramma_core::cell::conflicts::ActorClass::Human => "human".into(),
                    engramma_core::cell::conflicts::ActorClass::Agent => "agent".into(),
                },
                loser_at: r.loser_at.clone(),
                quarantined: r.quarantined,
                visible: r.visible,
            })
            .collect())
    }

    reject_recode!();
}

/// `conflict(copyId)`: one copy in full (v0.6.5, E5).
pub struct ConflictTask {
    cell: CoreCell,
    copy_id: String,
    _open: CellPermits,
}

impl Task for ConflictTask {
    type Output = engramma_core::cell::conflicts::ConflictDetail;
    type JsValue = ConflictDetailEntry;

    fn compute(&mut self) -> Result<Self::Output> {
        let id = parse_ulid(&self.copy_id)?;
        self.cell.conflict(&id).map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, d: Self::Output) -> Result<Self::JsValue> {
        Ok(ConflictDetailEntry {
            copy_id: d.copy_id.to_string(),
            original_id: d.original_id.to_string(),
            loser_actor: d.loser_actor.clone(),
            loser_class: match d.loser_class {
                engramma_core::cell::conflicts::ActorClass::Human => "human".into(),
                engramma_core::cell::conflicts::ActorClass::Agent => "agent".into(),
            },
            loser_at: d.loser_at.clone(),
            quarantined: d.quarantined,
            visible: d.visible,
            original_deleted: d.original_deleted,
            original_body: d.original_body,
            copy_body: d.copy_body,
            fork_body: d.fork_body,
        })
    }

    reject_recode!();
}

/// `keep`: `original` (the delete), `copy` (the loser's body), `merged`
/// (the body given).
#[napi(object)]
pub struct ResolveInput {
    pub keep: String,
    pub body: Option<String>,
}

pub struct ResolveConflictTask {
    cell: CoreCell,
    actor: String,
    copy_id: String,
    keep: String,
    body: Option<String>,
    _open: CellPermits,
}

impl Task for ResolveConflictTask {
    type Output = Ulid;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        let copy_id = parse_ulid(&self.copy_id)?;
        let keep = match self.keep.as_str() {
            "original" => engramma_core::ResolveKeep::Original,
            "copy" => engramma_core::ResolveKeep::Copy,
            "merged" => engramma_core::ResolveKeep::Merged,
            other => {
                return Err(invalid_argument(format!(
                    "`keep` vale original | copy | merged, non `{other}`"
                )));
            }
        };
        self.cell
            .resolve_conflict(&copy_id, keep, self.body.as_deref(), &self.actor)
            .map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, out: Ulid) -> Result<Self::JsValue> {
        Ok(out.to_string())
    }

    reject_recode!();
}

pub struct PurgeTask {
    cell: CoreCell,
    actor: String,
    id: String,
    reason: String,
    _open: CellPermits,
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
    embedder: EmbedderSlot,
    limit: Option<u32>,
    _open: CellPermits,
}

impl Task for EmbedPendingTask {
    type Output = i64;
    type JsValue = f64;

    fn compute(&mut self) -> Result<Self::Output> {
        // v0.4.2 (P1.4): the slot, read when this runs — the pull may have
        // landed after the cell was opened (or after this was called)
        let Some(embedder) = embedder_now(&self.embedder)? else {
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
        // v0.4.2 (P0.1): an explicit limit is respected AS GIVEN, no silent
        // `clamp(1, 8192)`; zero is a caller's bug
        let limit = match self.limit {
            Some(0) => return Err(invalid_argument("limit deve essere almeno 1")),
            Some(n) => n as usize,
            None => 512,
        };
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
    _open: CellPermits,
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
    _open: CellPermits,
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
            phantom_purged: r.phantom_purged,
            declared_corners: r.declared_corners,
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

/// What `repair()` did (v0.6.10, J2): the map old id → new id, back to
/// the caller, saved nowhere — Homun updates its references with it.
#[napi(object)]
pub struct RepairView {
    /// `[{ old, new }]`, one pair per phantom whose content moved.
    pub moved: Vec<RepairMove>,
}

#[napi(object)]
pub struct RepairMove {
    /// the purged id the phantom lived on.
    pub old: String,
    /// the fresh id the content lives under now.
    pub new: String,
}

pub struct RepairTask {
    cell: CoreCell,
    _open: CellPermits,
}

impl Task for RepairTask {
    type Output = engramma_core::cell::RepairReport;
    type JsValue = RepairView;

    fn compute(&mut self) -> Result<Self::Output> {
        self.cell.repair().map_err(cell_error)
    }

    fn resolve(&mut self, _env: Env, r: engramma_core::cell::RepairReport) -> Result<RepairView> {
        Ok(RepairView {
            moved: r
                .moved
                .into_iter()
                .map(|(old, new)| RepairMove { old, new })
                .collect(),
        })
    }

    reject_recode!();
}

pub struct CompactTask {
    cell: CoreCell,
    keep_last: i64,
    keep_snapshot: bool,
    _open: CellPermits,
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
            compacted: r.compacted,
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
    pub fn put(&self, env: Env, note: NoteInput) -> Result<AsyncTask<PutTask>> {
        Ok(AsyncTask::new(PutTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            input: note,
            _open: self.enter(env)?,
        }))
    }

    /// Change some fields of a note.
    #[napi(ts_return_type = "Promise<NoteView>")]
    pub fn patch(
        &self,
        env: Env,
        id: String,
        patch: NotePatchInput,
    ) -> Result<AsyncTask<PatchTask>> {
        Ok(AsyncTask::new(PatchTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            patch,
            _open: self.enter(env)?,
        }))
    }

    /// Read a note by id (the short 8-char form works, like the CLI).
    /// Quarantined and deleted notes answer `null` — the same oracle as
    /// search.
    #[napi(ts_return_type = "Promise<NoteView | null>")]
    pub fn get(&self, env: Env, id: String) -> Result<AsyncTask<GetTask>> {
        Ok(AsyncTask::new(GetTask {
            cell: self.cell.clone(),
            id,
            _open: self.enter(env)?,
        }))
    }

    /// v0.6.4 (D3): every open conflict copy of the cell — the
    /// «conflitti da risolvere» list. No text in the answer: the copy is
    /// a note, read it with `get`.
    #[napi(ts_return_type = "Promise<ConflictEntry[]>")]
    pub fn conflicts(&self, env: Env) -> Result<AsyncTask<ConflictsTask>> {
        Ok(AsyncTask::new(ConflictsTask {
            cell: self.cell.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// v0.6.5 (E5): one conflict copy in full — the original's body, the
    /// copy's body and the fork's, for the screen that asks the user to
    /// choose. The quarantined copies are readable here and only here
    /// (`get` answers `null` for them).
    #[napi(ts_return_type = "Promise<ConflictDetailEntry>")]
    pub fn conflict(&self, env: Env, copy_id: String) -> Result<AsyncTask<ConflictTask>> {
        Ok(AsyncTask::new(ConflictTask {
            cell: self.cell.clone(),
            copy_id,
            _open: self.enter(env)?,
        }))
    }

    /// v0.6.4 (D3): resolve one conflict. Ordinary operations with the
    /// cell's actor: `original` is a delete on the copy, `copy` and
    /// `merged` first patch the original's body. Two concurrent
    /// resolutions with equal bodies create nothing new.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn resolve_conflict(
        &self,
        env: Env,
        copy_id: String,
        input: ResolveInput,
    ) -> Result<AsyncTask<ResolveConflictTask>> {
        Ok(AsyncTask::new(ResolveConflictTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            copy_id,
            keep: input.keep,
            body: input.body,
            _open: self.enter(env)?,
        }))
    }

    /// The notes of this cell, filtered. Default limit 100.
    #[napi(ts_return_type = "Promise<NoteView[]>")]
    pub fn list(&self, env: Env, f: Option<ListFilter>) -> Result<AsyncTask<ListTask>> {
        Ok(AsyncTask::new(ListTask {
            cell: self.cell.clone(),
            tags: f.as_ref().and_then(|f| f.tags.clone()),
            note_type: f.as_ref().and_then(|f| f.note_type.clone()),
            statuses: f.as_ref().and_then(|f| f.status.clone()),
            limit: f.as_ref().and_then(|f| f.limit),
            offset: f.as_ref().and_then(|f| f.offset),
            _open: self.enter(env)?,
        }))
    }

    /// Search this cell.
    #[napi(ts_return_type = "Promise<SearchHit[]>")]
    pub fn search(&self, env: Env, q: SearchInput) -> Result<AsyncTask<SearchTask>> {
        Ok(AsyncTask::new(SearchTask {
            cell: self.cell.clone(),
            name: self.name.clone(),
            embedder: Arc::clone(&self.embedder),
            q,
            _open: self.enter(env)?,
        }))
    }

    /// The note's versions, oldest first (each is the state BEFORE the
    /// operation that version records).
    #[napi(ts_return_type = "Promise<VersionView[]>")]
    pub fn history(&self, env: Env, id: String) -> Result<AsyncTask<HistoryTask>> {
        Ok(AsyncTask::new(HistoryTask {
            cell: self.cell.clone(),
            id,
            _open: self.enter(env)?,
        }))
    }

    /// The note's graph neighborhood (BFS, default depth 1, max 4).
    #[napi(ts_return_type = "Promise<NeighborView[]>")]
    pub fn neighbors(
        &self,
        env: Env,
        id: String,
        depth: Option<u8>,
    ) -> Result<AsyncTask<NeighborsTask>> {
        Ok(AsyncTask::new(NeighborsTask {
            cell: self.cell.clone(),
            id,
            depth: depth.unwrap_or(1),
            _open: self.enter(env)?,
        }))
    }

    /// Change a note's status (`active` | `quarantined` | `superseded`).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_status(
        &self,
        env: Env,
        id: String,
        status: String,
    ) -> Result<AsyncTask<SetStatusTask>> {
        Ok(AsyncTask::new(SetStatusTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            status,
            _open: self.enter(env)?,
        }))
    }

    /// Promote a quarantined note to active.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn confirm(&self, env: Env, id: String) -> Result<AsyncTask<ConfirmTask>> {
        Ok(AsyncTask::new(ConfirmTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            _open: self.enter(env)?,
        }))
    }

    /// A reversible tombstone.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn delete(&self, env: Env, id: String) -> Result<AsyncTask<DeleteTask>> {
        Ok(AsyncTask::new(DeleteTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            _open: self.enter(env)?,
        }))
    }

    /// The right to be forgotten — irreversible. The view says whether
    /// every byte is already gone (`scrubbed: true`) or something is
    /// `pending` (the WAL, a view): an incomplete forgetting the caller
    /// must finish.
    #[napi(ts_return_type = "Promise<PurgeView>")]
    pub fn purge(&self, env: Env, id: String, reason: String) -> Result<AsyncTask<PurgeTask>> {
        Ok(AsyncTask::new(PurgeTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            reason,
            _open: self.enter(env)?,
        }))
    }

    /// v0.6.11 (K1): what the purge of this note would take with it,
    /// asked BEFORE the purge — read-only, nothing moves. Today it names
    /// the REOPENINGS: the notes whose closing was this note's work (the
    /// old side of a supersede, the target of an accepted correction).
    /// Homun warns the user before forgetting: «tornerà valido: …».
    #[napi(ts_return_type = "Promise<PurgePreview>")]
    pub fn purge_preview(&self, env: Env, id: String) -> Result<AsyncTask<PurgePreviewTask>> {
        Ok(AsyncTask::new(PurgePreviewTask {
            cell: self.cell.clone(),
            id,
            _open: self.enter(env)?,
        }))
    }

    /// v0.6.13 (M1): propose a fact — the note is born quarantined, in
    /// the queue, invisible to the search, until a decision says
    /// otherwise. The actor is the cell's own (an agent for Homun);
    /// `revertDecision` stays out of the binding (declared by the
    /// phase: it lives in the CLI's review).
    #[napi(ts_return_type = "Promise<ProposalOutcome>")]
    pub fn propose(&self, env: Env, input: ProposeInput) -> Result<AsyncTask<ProposeTask>> {
        Ok(AsyncTask::new(ProposeTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            input,
            _open: self.enter(env)?,
        }))
    }

    /// v0.6.13 (M1): decide a queued proposal — `accept` activates the
    /// note (and closes the supersede target), `reject` tombstones it.
    /// Returns the decision op's id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn decide(
        &self,
        env: Env,
        id: String,
        action: DecideInput,
    ) -> Result<AsyncTask<DecideTask>> {
        Ok(AsyncTask::new(DecideTask {
            cell: self.cell.clone(),
            actor: self.actor.clone(),
            id,
            action,
            _open: self.enter(env)?,
        }))
    }

    /// v0.6.13 (M1): the proposals queue (every state, oldest first).
    /// The rows the agent's conflict copies derive stay hidden from the
    /// LISTING while their original holds a lapide (the bin's own
    /// rule), but their rows are here.
    #[napi(ts_return_type = "Promise<ProposalRow[]>")]
    pub fn proposals(&self, env: Env) -> Result<AsyncTask<ProposalsTask>> {
        Ok(AsyncTask::new(ProposalsTask {
            cell: self.cell.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// Embed the notes that are still pending; returns how many it did.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn embed_pending(
        &self,
        env: Env,
        limit: Option<u32>,
    ) -> Result<AsyncTask<EmbedPendingTask>> {
        Ok(AsyncTask::new(EmbedPendingTask {
            cell: self.cell.clone(),
            embedder: Arc::clone(&self.embedder),
            limit,
            _open: self.enter(env)?,
        }))
    }

    /// The ops after `seq`, or `{compacted: true, head}` when they sit
    /// below the compaction cut.
    #[napi(
        ts_return_type = "Promise<{ head: number; ops: OpView[] } | { compacted: true; head: number }>"
    )]
    pub fn changes_since(&self, env: Env, seq: i64) -> Result<AsyncTask<ChangesSinceTask>> {
        Ok(AsyncTask::new(ChangesSinceTask {
            cell: self.cell.clone(),
            seq,
            _open: self.enter(env)?,
        }))
    }

    /// Verify the hash chain, the replay and the derived indexes.
    #[napi(ts_return_type = "Promise<VerifyView>")]
    pub fn verify(&self, env: Env) -> Result<AsyncTask<VerifyTask>> {
        Ok(AsyncTask::new(VerifyTask {
            cell: self.cell.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// Repair what verify found: each phantom's content moves to a NEW
    /// id (a normal put, actor `engramma:repair`), the old purged ids
    /// are forgotten by the rule. The map old → new goes back to the
    /// caller and is saved nowhere.
    #[napi(ts_return_type = "Promise<RepairView>")]
    pub fn repair(&self, env: Env) -> Result<AsyncTask<RepairTask>> {
        Ok(AsyncTask::new(RepairTask {
            cell: self.cell.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// Forget the payloads of the oldest operations, keeping the last
    /// `keepLast` replayable.
    ///
    /// **v0.6.15 (F2): EXPERIMENTAL — an explicit opt-in.** The call works
    /// only on a cell opened with `{ experimental: ['compaction'] }`;
    /// without it, it refuses with code `EXPERIMENTAL` and the cell does
    /// not change. The reason is declared, not fixed: the defects of the
    /// cut listed in `crates/engramma-core/tests/open-defects.txt` are
    /// open, and compaction is an optimization Homun keeps off — not a
    /// function of its memory.
    ///
    /// v0.6.14 (N1): **the cut only goes forward.** A `keepLast` so large
    /// that the cut would fall BELOW the one already made changes
    /// nothing: the call answers `compacted: false` with the true cut.
    /// Raising `keepLast` does no harm — the cut never goes back (before
    /// this version it did, and the first reconstruction above it lost
    /// the queue row of an accepted proposal).
    #[napi(ts_return_type = "Promise<CompactView>")]
    pub fn compact(
        &self,
        env: Env,
        keep_last: i64,
        opts: Option<CompactOpts>,
    ) -> Result<AsyncTask<CompactTask>> {
        // the gate first: a closed or dropped cell answers CLOSED /
        // NOT_FOUND even without the opt-in
        let permits = self.enter(env)?;
        if !self.experimental_compaction {
            return Err(recode(
                env,
                transport_err(
                    "EXPERIMENTAL",
                    "la compattazione è sperimentale: i difetti dichiarati del taglio \
                 (crates/engramma-core/tests/open-defects.txt) sono aperti — \
                 apri la cella con { experimental: ['compaction'] } per usarla lo stesso",
                ),
            ));
        }
        Ok(AsyncTask::new(CompactTask {
            cell: self.cell.clone(),
            keep_last: keep_last.max(0),
            keep_snapshot: opts.and_then(|o| o.keep_snapshot).unwrap_or(false),
            _open: permits,
        }))
    }

    /// The views bound to this cell (diagnostics: names and roots).
    #[napi(ts_return_type = "Promise<VaultView[]>")]
    pub fn vault_views(&self, env: Env) -> Result<AsyncTask<VaultViewsTask>> {
        Ok(AsyncTask::new(VaultViewsTask {
            cell: self.cell.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// Detach a markdown view from this cell (v0.4, P0.1): the cell forgets
    /// it and its bindings — the files stay on disk. Accepts the view id
    /// (even its first 8 characters) or the folder.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn vault_unbind(
        &self,
        env: Env,
        dir_or_view_id: String,
    ) -> Result<AsyncTask<VaultUnbindTask>> {
        Ok(AsyncTask::new(VaultUnbindTask {
            cell: self.cell.clone(),
            arg: dir_or_view_id,
            _open: self.enter(env)?,
        }))
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
    _open: CellPermits,
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
    _open: CellPermits,
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
                // v0.4.1 (P2.9): the short id is the LAST 8 characters (the
                // random part, like the notes'); the full id always works,
                // an ambiguous short id lists its candidates
                let hits: Vec<_> = self
                    .cell
                    .vault_views()
                    .map_err(cell_error)?
                    .into_iter()
                    .filter(|v| v.view_id == arg || (arg.len() == 8 && v.view_id.ends_with(arg)))
                    .collect();
                match hits.len() {
                    1 => Some(hits[0].clone()),
                    0 => None,
                    n => {
                        let list = hits
                            .iter()
                            .map(|v| format!("{} ({})", v.root, &v.view_id[v.view_id.len() - 8..]))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(invalid(format!("«{arg}» indica {n} viste: {list}")));
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

// ---------------------------------------------------------------------------
// the sync service (v0.6.2, S2–S4): the JS contract over the library's
// own SyncService — one per Engine, on the service's own thread; every
// call answers within its bound and the event loop never blocks
// ---------------------------------------------------------------------------

/// `sync()`'s options.
#[napi(object)]
pub struct SyncOpts {
    /// This device's name in the list, when it is the first (and the
    /// name a `join` presents). Default `portatile`.
    pub device_name: Option<String>,
    /// `'default'` (n0's public relays), `'none'` (direct connections
    /// only) or `{ url }` for one's own — like the CLI's flags; the
    /// choice is saved in the root as today.
    #[napi(ts_type = "'default' | 'none' | { url: string }")]
    pub relay: Option<serde_json::Value>,
}

/// The relay choice as the service takes it.
fn parse_relay(v: Option<serde_json::Value>) -> Result<Option<engramma_sync::relay::RelayChoice>> {
    let Some(v) = v else {
        return Ok(None);
    };
    match &v {
        serde_json::Value::String(s) if s == "default" => {
            Ok(Some(engramma_sync::relay::RelayChoice::Default))
        }
        serde_json::Value::String(s) if s == "none" => {
            Ok(Some(engramma_sync::relay::RelayChoice::Disabled))
        }
        serde_json::Value::Object(map)
            if map.len() == 1 && map.contains_key("url") && map["url"].is_string() =>
        {
            Ok(Some(engramma_sync::relay::RelayChoice::Own(
                map["url"].as_str().unwrap_or_default().to_string(),
            )))
        }
        _ => Err(invalid_argument(
            "relay non valida: 'default' | 'none' | { url }",
        )),
    }
}

/// One device of the list.
#[napi(object)]
pub struct SyncDeviceView {
    /// The name its owner chose.
    pub name: String,
    /// The device id, hex (the signing public key).
    pub device: String,
    /// The first 8 characters of the device id.
    pub short_id: String,
    /// Is it THIS device?
    pub is_self: bool,
    /// The last session the address book saw (RFC 3339).
    pub last_contact: Option<String>,
    /// What the book holds: `diretta` or `relay`.
    pub via: Option<String>,
    /// Live, or removed (the row stays for the history).
    pub live: bool,
}

/// One shared cell of the status.
#[napi(object)]
pub struct SyncCellView {
    pub name: String,
    pub shared: bool,
    /// The cell's current key epoch.
    pub epoch: f64,
    /// How many live devices hold it.
    pub devices: f64,
    /// The last confirmed session (RFC 3339).
    pub last_session: Option<String>,
    /// Operations written but not yet confirmed anywhere.
    pub pending: f64,
    /// Operations that still cannot leave (R4).
    pub unsent: f64,
}

/// What `status` answers.
#[napi(object)]
pub struct SyncStatusView {
    pub device: SyncDeviceView,
    pub serving: bool,
    /// `"default"`, `"none"`, the relay's URL, or null before a choice.
    pub relay: Option<String>,
    pub cells: Vec<SyncCellView>,
}

/// What `share` says.
#[napi(object)]
pub struct ShareView {
    pub cell: String,
    /// The adopt operation's id (the signed root of the pre-sync era).
    pub adopt: String,
    pub epoch: f64,
    /// The first 8 characters of the epoch key.
    pub key: String,
    /// How many OTHER live devices received a wrapped key.
    pub wraps: f64,
}

/// One rotated cell of a removal.
#[napi(object)]
pub struct RotatedView {
    pub cell: String,
    pub epoch: f64,
}

/// What `remove` says.
#[napi(object)]
pub struct RemoveView {
    pub removed: String,
    pub accepted_root: String,
    pub accepted_count: f64,
    pub rotated: Vec<RotatedView>,
}

/// One device's row of a pass.
#[napi(object)]
pub struct PassDeviceView {
    pub name: String,
    pub device: String,
    pub reached: bool,
    pub why_not: Option<String>,
    /// `diretta` or `relay` when a session ran.
    pub via: Option<String>,
}

/// One cell's row of a pass.
#[napi(object)]
pub struct PassCellView {
    pub cell: String,
    pub sent: f64,
    pub received: f64,
    pub refused: f64,
    pub conflicts: f64,
    pub unsent: f64,
    pub head: f64,
    pub frontier: f64,
}

/// What `runOnce` says: an unreachable device is a row, never an error.
#[napi(object)]
pub struct PassView {
    pub devices: Vec<PassDeviceView>,
    pub cells: Vec<PassCellView>,
}

/// What `start` publishes.
#[napi(object)]
pub struct ServeInfoView {
    pub node: String,
    pub addrs: Vec<String>,
    pub relay: Option<String>,
    pub relay_warning: Option<String>,
}

/// The pairing's ticket/code side, ready when `invite`/`code` resolves.
#[napi(object)]
pub struct PairingInfoView {
    /// The ticket, textual form (`invite` only).
    pub ticket: Option<String>,
    /// The ticket in the QR's alphanumeric clothes (`invite` only).
    pub ticket_qr: Option<String>,
    /// The short code (`code` only).
    pub code: Option<String>,
    /// When the ticket (the window) ends, RFC 3339.
    pub expires_at: String,
    /// The relay URL the ticket carries, when there is one.
    pub relay: Option<String>,
    pub relay_warning: Option<String>,
    /// How many SAS rounds this pairing shows (1 for the ticket flows,
    /// 2 for the short code: the meeting's and the ticket's).
    pub rounds: f64,
}

/// One cell a pairing delivered, or received.
#[napi(object)]
pub struct CellArrivedView {
    pub name: String,
    pub ops: f64,
}

/// The other device, briefly.
#[napi(object)]
pub struct DeviceBriefView {
    pub name: String,
    pub short_id: String,
}

/// What `done` says: the other device and the cells.
#[napi(object)]
pub struct PairingResultView {
    pub peer: Option<DeviceBriefView>,
    pub cells: Vec<CellArrivedView>,
    pub registry_rows: f64,
}

/// The listeners of the event hub.
type Listener = Box<dyn Fn(serde_json::Value) + Send + std::marker::Sync>;

/// The fan-out the pump thread feeds: JS callbacks come and go, the hub
/// stays (one per service, shared by every Sync object).
#[derive(Default)]
pub struct EventHub {
    next: std::sync::atomic::AtomicU64,
    listeners: Mutex<Vec<(u64, Listener)>>,
}

impl EventHub {
    fn add(&self, l: Listener) -> u64 {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.listeners
            .lock()
            .map(|mut ls| ls.push((id, l)))
            .map_err(|_| transport_err("IO", "lock degli eventi avvelenato"))
            .ok();
        id
    }

    fn remove(&self, id: u64) {
        if let Ok(mut ls) = self.listeners.lock() {
            ls.retain(|(i, _)| *i != id);
        }
    }

    /// v0.6.3 (C2): the end of the service empties the hub — every
    /// listener goes, and with it the tsfn it holds (its release is the
    /// Drop). A listener that will never fire again must not hold
    /// anything, least of all the process's event loop.
    fn clear(&self) {
        if let Ok(mut ls) = self.listeners.lock() {
            ls.clear();
        }
    }

    fn emit(&self, event: serde_json::Value) {
        if let Ok(ls) = self.listeners.lock() {
            for (_, l) in ls.iter() {
                l(event.clone());
            }
        }
    }
}

/// The engine's own share of the service: the handle, the hub, the pump
/// thread that drains the service's events into JS.
pub struct SyncShared {
    service: Arc<SyncService>,
    hub: Arc<EventHub>,
}

impl SyncShared {
    fn start(root: &std::path::Path) -> Result<Self> {
        let service = SyncService::start(root).map_err(sync_error)?;
        let hub = Arc::new(EventHub::default());
        if let Some(events) = service.take_events() {
            // the pump: a plain thread, a blocking read, the hub's fan-out
            // — the event loop never blocks, and the thread dies with the
            // service (the channel closes when the worker ends)
            let hub = Arc::clone(&hub);
            std::thread::Builder::new()
                .name("engramma-sync-events".into())
                .spawn(move || {
                    while let Ok(event) = events.recv() {
                        hub.emit(event_json(event));
                    }
                    // v0.6.3 (C2): the channel closed — the service is
                    // over (a stop, a close, a removal that stopped it by
                    // itself) — the listeners go with it
                    hub.clear();
                })
                .map_err(|e| transport_err("IO", e))?;
        }
        Ok(Self { service, hub })
    }

    /// The end of the service: the continuous loop stops, the worker
    /// thread ends, the pump follows the closed channel.
    fn stop(&self) {
        self.service.shutdown();
        // v0.6.3 (C2): close() empties the hub itself — the pump follows
        // when the channel closes, and a listener that slipped in between
        // would hear nothing anyway (the service is over)
        self.hub.clear();
    }
}

/// One service event, as JS reads it (a union on `type`).
fn event_json(event: engramma_sync::service::ServiceEvent) -> serde_json::Value {
    use engramma_sync::service::ServiceEvent;
    use serde_json::json;
    match event {
        ServiceEvent::Pass { peers, cells } => json!({
            "type": "pass",
            "devices": peers.iter().map(|p| json!({
                "name": p.name, "device": p.device, "reached": p.reached,
                "whyNot": p.why_not, "via": p.via,
            })).collect::<Vec<_>>(),
            "cells": cells.iter().map(|c| json!({
                "cell": c.name, "sent": c.sent, "received": c.received,
                "refused": c.refused, "conflicts": c.conflicts,
                "unsent": c.unsent, "head": c.head, "frontier": c.frontier,
            })).collect::<Vec<_>>(),
        }),
        ServiceEvent::Arrived { cell, count } => {
            json!({ "type": "arrived", "cell": cell, "count": count })
        }
        ServiceEvent::Device { name, short, added } => {
            json!({ "type": "device", "name": name, "short": short, "added": added })
        }
        ServiceEvent::RemovedSelf => json!({ "type": "removed" }),
        ServiceEvent::Unsent { cell, count } => {
            json!({ "type": "unsent", "cell": cell, "count": count })
        }
        ServiceEvent::Error { code, message } => {
            json!({ "type": "error", "code": code, "message": message })
        }
    }
}

/// The views the service's own answers map onto.
fn devices_view(v: Vec<engramma_sync::service::DeviceView>) -> Vec<SyncDeviceView> {
    v.into_iter()
        .map(|d| SyncDeviceView {
            name: d.name,
            device: d.device,
            short_id: d.short_id,
            is_self: d.is_self,
            last_contact: d.last_contact,
            via: d.via,
            live: d.live,
        })
        .collect()
}

fn status_view(v: engramma_sync::service::StatusView) -> SyncStatusView {
    SyncStatusView {
        device: SyncDeviceView {
            name: v.device.name,
            device: v.device.device,
            short_id: v.device.short_id,
            is_self: v.device.is_self,
            last_contact: v.device.last_contact,
            via: v.device.via,
            live: v.device.live,
        },
        serving: v.serving,
        relay: v.relay,
        cells: v
            .cells
            .into_iter()
            .map(|c| SyncCellView {
                name: c.name,
                shared: c.shared,
                epoch: c.epoch as f64,
                devices: c.devices as f64,
                last_session: c.last_session,
                pending: c.pending as f64,
                unsent: c.unsent as f64,
            })
            .collect(),
    }
}

pub struct SyncOpenTask {
    handle: EngineHandle,
    opts: SyncOpts,
    _open: GateGuard,
}

impl Task for SyncOpenTask {
    type Output = (
        Arc<SyncService>,
        Arc<EventHub>,
        String,
        Option<engramma_sync::relay::RelayChoice>,
    );
    type JsValue = Sync;

    fn compute(&mut self) -> Result<Self::Output> {
        // parse the relay ONCE: every call the service takes reuses it —
        // and an explicit choice SAVES in the root right here, as the
        // CLI's flags always did
        let flag = parse_relay(self.opts.relay.clone())?;
        engramma_sync::relay::resolve(&self.handle.root, flag.as_ref())
            .map_err(|e| transport_err("IO", e))?;
        let name = self
            .opts
            .device_name
            .clone()
            .unwrap_or_else(|| "portatile".into());
        let mut slot = self
            .handle
            .sync
            .lock()
            .map_err(|_| transport_err("IO", "lock della sync avvelenato"))?;
        let shared = match slot.as_ref() {
            Some(shared) => Arc::clone(shared),
            None => {
                let shared = Arc::new(SyncShared::start(&self.handle.root)?);
                *slot = Some(Arc::clone(&shared));
                shared
            }
        };
        Ok((
            Arc::clone(&shared.service),
            Arc::clone(&shared.hub),
            name,
            flag,
        ))
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(Sync {
            service: out.0,
            handle: self.handle.clone(),
            hub: out.1,
            device_name: out.2,
            relay: out.3,
            gate: self.handle.gate.clone(),
        })
    }

    reject_recode!();
}

/// The sync service of one root, as JS drives it (v0.6.2): every method
/// is async on libuv's pool, the service runs on its own thread, the
/// events fan out from the hub. After `Engine.close` every method
/// answers `CLOSED`.
#[napi(js_name = "Sync")]
pub struct Sync {
    service: Arc<SyncService>,
    handle: EngineHandle,
    hub: Arc<EventHub>,
    device_name: String,
    relay: Option<engramma_sync::relay::RelayChoice>,
    gate: Gate,
}

impl Sync {
    fn enter(&self, env: Env) -> Result<GateGuard> {
        self.gate.enter_now().map_err(|e| recode(env, e))
    }

    fn engine_handle(&self) -> EngineHandle {
        self.handle.clone()
    }
}

/// The service's refusals cross the async fence with their code.
fn io_sync(e: engramma_sync::service::ServiceError) -> Error {
    sync_error(e)
}

/// Arm the device's operation signer on a cell of a SHARED root, as the
/// CLI does at every open: from the share on, every LOCAL write carries
/// its device's signature, or the peers' arrival gate refuses it.
fn arm_shared_signer(root: &std::path::Path, cell: &CoreCell, name: &str) {
    // a root without devices.db never shared anything: one stat, no
    // SQLite — the open path must stay cheap for the engines that never
    // sync (the 200-open race of v0.4.3 P2.3 runs right through here)
    if !root.join("devices.db").exists() {
        return;
    }
    let shared = engramma_sync::service::cell_is_shared(root, name).unwrap_or(false);
    if !shared {
        return;
    }
    if let Ok(dk) = engramma_sync::device::DeviceKey::load_or_create(root) {
        let _ = cell.set_signer(Arc::new(engramma_sync::arrival::DeviceSigner::new(&dk)));
    }
}

/// v0.6.3 (C5.3): the stamp of `devices.db` — modified time and length,
/// the cheapest thing that MOVES when the registry changes (a share, a
/// join, a device added — by this Engine or by the CLI in another
/// process while ours holds the cell open).
fn devices_db_stamp(root: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(root.join("devices.db")).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// v0.6.3 (C5.3): the write-time signer gate of one cell. Every local
/// write pays ONE stat of `devices.db`: a root without it (the engines
/// that never sync — the insert bench's roots) stops there, at a failed
/// stat. When the stamp moves, the first write after the change (re)arms
/// the signer — the arm is decided at the WRITE, not at the open, so a
/// cell shared by ANOTHER process under an open Engine signs too.
fn signer_gate_for(root: &std::path::Path, name: &str) -> engramma_core::cell::SignerGate {
    let root = root.to_path_buf();
    let name = name.to_string();
    let last = Arc::new(std::sync::Mutex::new(devices_db_stamp(&root)));
    Arc::new(move || {
        let now = devices_db_stamp(&root);
        {
            let mut last = last
                .lock()
                .map_err(|_| engramma_core::CellError::Db("gate".into()))
                .ok()?;
            if now == *last {
                return None;
            }
            *last = now;
        }
        // the stamp moved: arm only if THIS cell is shared now
        if !engramma_sync::service::cell_is_shared(&root, &name).unwrap_or(false) {
            return None;
        }
        let dk = engramma_sync::device::DeviceKey::load_or_create(&root).ok()?;
        Some(Arc::new(engramma_sync::arrival::DeviceSigner::new(&dk))
            as Arc<dyn engramma_core::cell::ops::Signer>)
    })
}

pub struct SyncStatusTask {
    service: Arc<SyncService>,
    _open: GateGuard,
}

impl Task for SyncStatusTask {
    type Output = engramma_sync::service::StatusView;
    type JsValue = SyncStatusView;

    fn compute(&mut self) -> Result<Self::Output> {
        self.service.status().map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(status_view(out))
    }

    reject_recode!();
}

pub struct SyncDevicesTask {
    service: Arc<SyncService>,
    _open: GateGuard,
}

impl Task for SyncDevicesTask {
    type Output = Vec<engramma_sync::service::DeviceView>;
    type JsValue = Vec<SyncDeviceView>;

    fn compute(&mut self) -> Result<Self::Output> {
        self.service.devices().map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(devices_view(out))
    }

    reject_recode!();
}

pub struct SyncRemoveTask {
    service: Arc<SyncService>,
    name: String,
    _open: GateGuard,
}

impl Task for SyncRemoveTask {
    type Output = engramma_sync::service::RemoveView;
    type JsValue = RemoveView;

    fn compute(&mut self) -> Result<Self::Output> {
        self.service.remove(&self.name).map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(RemoveView {
            removed: out.removed,
            accepted_root: out.accepted_root,
            accepted_count: out.accepted_count as f64,
            rotated: out
                .rotated
                .into_iter()
                .map(|(cell, epoch)| RotatedView {
                    cell,
                    epoch: epoch as f64,
                })
                .collect(),
        })
    }

    reject_recode!();
}

pub struct SyncShareTask {
    service: Arc<SyncService>,
    handle: EngineHandle,
    cell: String,
    _open: GateGuard,
}

impl SyncShareTask {
    fn handle_shared_cell(&self) -> Result<CoreCell> {
        self.handle.open_cell(&self.cell, false)
    }
}

impl Task for SyncShareTask {
    type Output = engramma_sync::service::ShareView;
    type JsValue = ShareView;

    fn compute(&mut self) -> Result<Self::Output> {
        let out = self.service.share(&self.cell).map_err(io_sync)?;
        // the ENGINE's own handles of this cell start signing now too
        if let Ok(cell) = self.handle_shared_cell() {
            arm_shared_signer(&self.handle.root, &cell, &self.cell);
        }
        Ok(out)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(ShareView {
            cell: out.cell,
            adopt: out.adopt,
            epoch: out.epoch as f64,
            key: out.key,
            wraps: out.wraps as f64,
        })
    }

    reject_recode!();
}

pub struct SyncUnshareTask {
    service: Arc<SyncService>,
    cell: String,
    _open: GateGuard,
}

impl Task for SyncUnshareTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        self.service.unshare(&self.cell).map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

pub struct SyncRunOnceTask {
    service: Arc<SyncService>,
    relay: Option<engramma_sync::relay::RelayChoice>,
    _open: GateGuard,
}

impl Task for SyncRunOnceTask {
    type Output = engramma_sync::serve::PassReport;
    type JsValue = PassView;

    fn compute(&mut self) -> Result<Self::Output> {
        self.service.run_once(self.relay.clone()).map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(PassView {
            devices: out
                .peers
                .into_iter()
                .map(|p| PassDeviceView {
                    name: p.name,
                    device: p.device,
                    reached: p.reached,
                    why_not: p.why_not,
                    via: p.via.map(str::to_string),
                })
                .collect(),
            cells: out
                .cells
                .into_iter()
                .map(|c| PassCellView {
                    cell: c.name,
                    sent: c.sent as f64,
                    received: c.received as f64,
                    refused: c.refused as f64,
                    conflicts: c.conflicts as f64,
                    unsent: c.unsent as f64,
                    head: c.head as f64,
                    frontier: c.frontier as f64,
                })
                .collect(),
        })
    }

    reject_recode!();
}

pub struct SyncServeStartTask {
    service: Arc<SyncService>,
    relay: Option<engramma_sync::relay::RelayChoice>,
    _open: GateGuard,
}

impl Task for SyncServeStartTask {
    type Output = engramma_sync::service::ServeInfo;
    type JsValue = ServeInfoView;

    fn compute(&mut self) -> Result<Self::Output> {
        self.service
            .serve_start(self.relay.clone())
            .map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(ServeInfoView {
            node: out.node,
            addrs: out.addrs,
            relay: out.relay,
            relay_warning: out.relay_warning,
        })
    }

    reject_recode!();
}

pub struct SyncServeStopTask {
    service: Arc<SyncService>,
    _open: GateGuard,
}

impl Task for SyncServeStopTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        self.service.serve_stop().map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

/// A pairing in flight: the staged handle, its info ready at resolve.
#[napi(js_name = "Pairing")]
pub struct Pairing {
    handle: engramma_sync::service::PairingHandle,
    info: PairingInfoView,
    gate: Gate,
}

pub struct PairingSasTask {
    handle: engramma_sync::service::PairingHandle,
    _open: GateGuard,
}

impl Task for PairingSasTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        self.handle
            .sas(std::time::Duration::from_secs(11 * 60))
            .map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, out: String) -> Result<String> {
        Ok(out)
    }

    reject_recode!();
}

pub struct PairingConfirmTask {
    handle: engramma_sync::service::PairingHandle,
    same: bool,
    _open: GateGuard,
}

impl Task for PairingConfirmTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        self.handle.confirm(self.same).map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, _out: ()) -> Result<()> {
        Ok(())
    }

    reject_recode!();
}

pub struct PairingDoneTask {
    handle: engramma_sync::service::PairingHandle,
    _open: GateGuard,
}

impl Task for PairingDoneTask {
    type Output = engramma_sync::service::PairingView;
    type JsValue = PairingResultView;

    fn compute(&mut self) -> Result<Self::Output> {
        self.handle
            .done(std::time::Duration::from_secs(3 * 60))
            .map_err(io_sync)
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(PairingResultView {
            peer: out.peer.map(|p| DeviceBriefView {
                name: p.name,
                short_id: p.short_id,
            }),
            cells: out
                .cells
                .into_iter()
                .map(|c| CellArrivedView {
                    name: c.name,
                    ops: c.ops as f64,
                })
                .collect(),
            registry_rows: out.registry_rows as f64,
        })
    }

    reject_recode!();
}

/// The staged pairing tasks' shared shape (invite, code, join): bind,
/// mint (or meet), resolve with the info ready.
enum PairingRoad {
    Invite,
    Code,
    Join(String),
}

pub struct PairingStartTask {
    service: Arc<SyncService>,
    road: PairingRoad,
    name: String,
    relay: Option<engramma_sync::relay::RelayChoice>,
    gate: Gate,
    _open: GateGuard,
}

impl Task for PairingStartTask {
    type Output = (
        engramma_sync::service::PairingHandle,
        engramma_sync::service::PairingInfo,
        u32,
    );
    type JsValue = Pairing;

    fn compute(&mut self) -> Result<Self::Output> {
        let (handle, rounds) = match &self.road {
            PairingRoad::Invite => (
                self.service
                    .invite(&self.name, self.relay.clone())
                    .map_err(io_sync)?,
                1,
            ),
            PairingRoad::Code => (
                self.service
                    .code(&self.name, self.relay.clone())
                    .map_err(io_sync)?,
                2,
            ),
            PairingRoad::Join(text) => (
                self.service
                    .join(text, &self.name, self.relay.clone())
                    .map_err(io_sync)?,
                if text.starts_with("engramma-invite-v1") {
                    1
                } else {
                    2
                },
            ),
        };
        // resolve when the ticket (the code) is ready: the relay wait
        // included, never after
        let info = handle
            .info(std::time::Duration::from_secs(60))
            .map_err(io_sync)?;
        Ok((handle, info, rounds))
    }

    fn resolve(&mut self, _env: Env, (handle, info, rounds): Self::Output) -> Result<Pairing> {
        Ok(Pairing {
            handle,
            info: PairingInfoView {
                ticket: info.ticket,
                ticket_qr: info.ticket_qr,
                code: info.code,
                expires_at: info.expires_at,
                relay: info.relay,
                relay_warning: info.relay_warning,
                rounds: rounds as f64,
            },
            gate: self.gate.clone(),
        })
    }

    reject_recode!();
}

#[napi]
impl Sync {
    /// This device, the service, the relay, the shared cells.
    #[napi(ts_return_type = "Promise<SyncStatusView>")]
    pub fn status(&self, env: Env) -> Result<AsyncTask<SyncStatusTask>> {
        Ok(AsyncTask::new(SyncStatusTask {
            service: Arc::clone(&self.service),
            _open: self.enter(env)?,
        }))
    }

    /// The devices: name, short id, is-this, last contact, what the book
    /// holds, live or removed.
    #[napi(ts_return_type = "Promise<SyncDeviceView[]>")]
    pub fn devices(&self, env: Env) -> Result<AsyncTask<SyncDevicesTask>> {
        Ok(AsyncTask::new(SyncDevicesTask {
            service: Arc::clone(&self.service),
            _open: self.enter(env)?,
        }))
    }

    /// Invite with a ticket (ten minutes, one use): resolves when the
    /// ticket is ready; `sas()`, `confirm()`, `done()` drive the rest.
    #[napi(ts_return_type = "Promise<Pairing>")]
    pub fn invite(&self, env: Env) -> Result<AsyncTask<PairingStartTask>> {
        Ok(AsyncTask::new(PairingStartTask {
            service: Arc::clone(&self.service),
            road: PairingRoad::Invite,
            name: self.device_name.clone(),
            relay: self.relay.clone(),
            gate: self.gate.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// Invite with the short code (the blind mailbox must be configured:
    /// `NO_MAILBOX` otherwise). TWO SAS rounds: the meeting's and the
    /// ticket's.
    #[napi(ts_return_type = "Promise<Pairing>")]
    pub fn code(&self, env: Env) -> Result<AsyncTask<PairingStartTask>> {
        Ok(AsyncTask::new(PairingStartTask {
            service: Arc::clone(&self.service),
            road: PairingRoad::Code,
            name: self.device_name.clone(),
            relay: self.relay.clone(),
            gate: self.gate.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// Enter with a ticket (both forms) or a short code: this device's
    /// name is the one `sync()` was given.
    #[napi(ts_return_type = "Promise<Pairing>")]
    pub fn join(
        &self,
        env: Env,
        #[napi(ts_arg_type = "string")] ticket_or_code: String,
    ) -> Result<AsyncTask<PairingStartTask>> {
        Ok(AsyncTask::new(PairingStartTask {
            service: Arc::clone(&self.service),
            road: PairingRoad::Join(ticket_or_code),
            name: self.device_name.clone(),
            relay: self.relay.clone(),
            gate: self.gate.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// Remove a device and rotate every shared cell's key; the removed
    /// device hears it from this one and stops its own service.
    #[napi(ts_return_type = "Promise<RemoveView>")]
    pub fn remove(
        &self,
        env: Env,
        #[napi(ts_arg_type = "string")] name: String,
    ) -> Result<AsyncTask<SyncRemoveTask>> {
        Ok(AsyncTask::new(SyncRemoveTask {
            service: Arc::clone(&self.service),
            name,
            _open: self.enter(env)?,
        }))
    }

    /// Share a cell (the adopt, epoch 0, the wrapped keys).
    #[napi(ts_return_type = "Promise<ShareView>")]
    pub fn share(
        &self,
        env: Env,
        #[napi(ts_arg_type = "string")] cell: String,
    ) -> Result<AsyncTask<SyncShareTask>> {
        Ok(AsyncTask::new(SyncShareTask {
            service: Arc::clone(&self.service),
            handle: self.engine_handle(),
            cell,
            _open: self.enter(env)?,
        }))
    }

    /// Stop sharing a cell from here (`NOT_SHARED` when it is not).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn unshare(
        &self,
        env: Env,
        #[napi(ts_arg_type = "string")] cell: String,
    ) -> Result<AsyncTask<SyncUnshareTask>> {
        Ok(AsyncTask::new(SyncUnshareTask {
            service: Arc::clone(&self.service),
            cell,
            _open: self.enter(env)?,
        }))
    }

    /// One pass with every reachable device and the blind node: an
    /// unreachable device is a row of the report, never an error. With
    /// the continuous service on, the pass rides its endpoint and its
    /// one-pass-at-a-time lock.
    #[napi(ts_return_type = "Promise<PassView>")]
    pub fn run_once(&self, env: Env) -> Result<AsyncTask<SyncRunOnceTask>> {
        Ok(AsyncTask::new(SyncRunOnceTask {
            service: Arc::clone(&self.service),
            relay: self.relay.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// The continuous service: accept, sync at every local write and
    /// every five minutes. A second one on the same root — this process
    /// or another — is `SYNC_BUSY`. Starting twice changes nothing.
    #[napi(ts_return_type = "Promise<ServeInfoView>")]
    pub fn start(&self, env: Env) -> Result<AsyncTask<SyncServeStartTask>> {
        Ok(AsyncTask::new(SyncServeStartTask {
            service: Arc::clone(&self.service),
            relay: self.relay.clone(),
            _open: self.enter(env)?,
        }))
    }

    /// Stop the continuous service: the session in flight is
    /// interrupted, the convergence holds by construction, the lock goes.
    /// Stopping a stopped service changes nothing. Bounded at 5 s.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn stop(&self, env: Env) -> Result<AsyncTask<SyncServeStopTask>> {
        Ok(AsyncTask::new(SyncServeStopTask {
            service: Arc::clone(&self.service),
            _open: self.enter(env)?,
        }))
    }

    /// The events of the service: `pass`, `arrived`, `device`,
    /// `removed`, `unsent`, `error` — nothing here carries note text, a
    /// ticket or a short code. Returns the function that unsubscribes.
    ///
    /// v0.6.3 (C2): a listener alone does not keep the process alive —
    /// the `EventEmitter`'s own semantics. What keeps it alive is the
    /// open engine (and its running service); `close()` empties the hub
    /// and releases every callback, and the unsubscribe of a closed
    /// engine does not throw.
    #[napi(ts_return_type = "() => void")]
    pub fn on_event(
        &self,
        env: Env,
        #[napi(ts_arg_type = "(e: SyncEvent) => void")] cb: Function<'static, (serde_json::Value,)>,
    ) -> Result<napi::sys::napi_value> {
        let tsfn = cb
            .build_threadsafe_function::<(serde_json::Value,)>()
            // v0.6.3 (C2): an UNREFERENCED tsfn — the listener alone must
            // not hold the event loop; the engine does, while it is open
            .weak::<true>()
            .build_callback(|ctx| Ok(ctx.value.0))?;
        let listener: Listener = Box::new(move |event: serde_json::Value| {
            tsfn.call(
                (event,),
                napi::threadsafe_function::ThreadsafeFunctionCallMode::Blocking,
            );
        });
        let id = self.hub.add(listener);
        let hub = Arc::clone(&self.hub);
        let off: napi::bindgen_prelude::Function<(), ()> =
            env.create_function_from_closure("unsubscribe", move |_ctx| {
                hub.remove(id);
                Ok(())
            })?;
        // the raw handle crosses back as the JS function itself (a
        // `Function<'_>` cannot be returned: it borrows the Env — the
        // raw `napi_value` is the one owned shape `ToNapiValue` takes)
        use napi::bindgen_prelude::JsValue;
        Ok(off.raw())
    }
}

#[napi]
impl Pairing {
    /// The ticket, textual form (`invite` only).
    #[napi(getter)]
    pub fn ticket(&self) -> Option<String> {
        self.info.ticket.clone()
    }

    /// The ticket in the QR's alphanumeric clothes (`invite` only):
    /// uppercase, digits and the few symbols the QR mode allows — a
    /// smaller, more readable QR than the byte mode.
    #[napi(getter)]
    pub fn ticket_qr(&self) -> Option<String> {
        self.info.ticket_qr.clone()
    }

    /// The short code (`code` only).
    #[napi(getter)]
    pub fn code(&self) -> Option<String> {
        self.info.code.clone()
    }

    /// When the ticket (the window) ends, RFC 3339.
    #[napi(getter)]
    pub fn expires_at(&self) -> String {
        self.info.expires_at.clone()
    }

    /// The relay URL the ticket carries, or null.
    #[napi(getter)]
    pub fn relay(&self) -> Option<String> {
        self.info.relay.clone()
    }

    /// The relay never connected: said, never silent.
    #[napi(getter)]
    pub fn relay_warning(&self) -> Option<String> {
        self.info.relay_warning.clone()
    }

    /// How many SAS rounds this pairing shows (1 for the ticket flows,
    /// 2 for the short code).
    #[napi(getter)]
    pub fn rounds(&self) -> f64 {
        self.info.rounds
    }

    /// The six digits of the next round, when both sides hold them (with
    /// the short code there are two: ask again after each `confirm`).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn sas(&self, env: Env) -> Result<AsyncTask<PairingSasTask>> {
        Ok(AsyncTask::new(PairingSasTask {
            handle: self.handle.clone(),
            _open: self.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    /// The human's answer to the digits on the screen. Before `sas()`
    /// hands them out it is `INVALID_ARGUMENT` — the comparison cannot
    /// be skipped, and there is no confirming by default.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn confirm(
        &self,
        env: Env,
        #[napi(ts_arg_type = "boolean")] same: bool,
    ) -> Result<AsyncTask<PairingConfirmTask>> {
        Ok(AsyncTask::new(PairingConfirmTask {
            handle: self.handle.clone(),
            same,
            _open: self.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    /// The end of the flow: the other device and the cells — or the
    /// refusal (`PAIRING_REJECTED`, `PAIRING_TIMEOUT`, `PAIRING_EXPIRED`,
    /// `CANCELLED`).
    #[napi(ts_return_type = "Promise<PairingResultView>")]
    pub fn done(&self, env: Env) -> Result<AsyncTask<PairingDoneTask>> {
        Ok(AsyncTask::new(PairingDoneTask {
            handle: self.handle.clone(),
            _open: self.gate.enter_now().map_err(|e| recode(env, e))?,
        }))
    }

    /// Close the pairing endpoint, now: the other side hears a clean
    /// refusal once its human answers, never a hang.
    #[napi(ts_return_type = "void")]
    pub fn cancel(&self) {
        self.handle.cancel();
    }
}

/// The engine version, so the host can log what it loaded.
///
/// v0.6.3 (C4): the parity trick of v0.6.2 (one address-taken libSystem
/// function to keep the linker's indirect-symbol ledger even) is gone —
/// the mis-alignment was `strip -S` of Xcode 27.2 mangling the LINKEDIT
/// string pool, and the artifact now builds with `strip = "none"` (see
/// the workspace profile); the build loads it right after linking, so a
/// future toolchain that breaks it again fails the BUILD, not Homun.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// v0.6.10 (J3): what the ADDON was built from — the commit (with the
/// `-dirty` mark, baked by the build script) and the cell schema it
/// supports. The JS bench compares this with the CLI's own `--version`
/// line and refuses to run when they disagree: a test that passes or
/// falls according to which binary lies around measures nothing.
#[napi(object)]
pub struct BuildInfo {
    /// the crate version.
    pub version: String,
    /// the commit the addon was built from (`…-dirty` on a dirty tree).
    pub commit: String,
    /// the cell schema this build supports.
    pub schema: i64,
}

#[napi]
pub fn build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        commit: env!("ENGRAMMA_BUILD_COMMIT").to_string(),
        schema: engramma_core::cell::schema::SCHEMA_VERSION,
    }
}
