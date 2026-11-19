// SPDX-License-Identifier: Apache-2.0

//! Typed errors for the whole crate. No panic ever crosses the API.

use std::path::PathBuf;

/// Errors returned by the format parser and serialiser.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FormatError {
    /// The file does not start with a frontmatter delimiter.
    #[error("frontmatter not found: the file must start with a `---` line")]
    NoFrontmatter,
    /// The YAML header is malformed.
    #[error("invalid YAML in frontmatter: {0}")]
    Yaml(String),
    /// The YAML header uses constructs outside the restricted model.
    #[error("unsupported YAML construct in frontmatter: {0}")]
    UnsupportedYaml(String),
    /// A field value is not valid.
    #[error("invalid value for `{field}`: {message}")]
    Field {
        /// Field name.
        field: &'static str,
        /// Human-readable explanation.
        message: String,
    },
    /// A required field is missing.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// Unknown key without the `x-` prefix in strict mode.
    #[error("unknown key `{0}` (only `x-` keys may be extended)")]
    UnknownKey(String),
    /// A relation line in `## Relations` does not follow the spec syntax.
    #[error("invalid relation line `{0}`: {1}")]
    RelationLine(String, String),
}

/// Errors returned by the cell (storage, log, operations).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CellError {
    /// SQLite failure.
    #[error("database error: {0}")]
    Db(String),
    /// The requested note does not exist.
    #[error("note {0} not found")]
    NoteNotFound(String),
    /// The requested entity does not exist.
    #[error("entity {0} not found")]
    EntityNotFound(String),
    /// The requested note version does not exist.
    #[error("version {1} of note {0} not found")]
    VersionNotFound(String, i64),
    /// The operation violates an invariant (e.g. quarantine rules).
    #[error("invalid operation: {0}")]
    Invalid(String),
    /// The cell file is not a cell (bad schema, not a database, …).
    #[error("not an engramma cell: {0}")]
    NotACell(String),
    /// I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The hash chain does not verify.
    #[error("log verification failed at seq {0}: {1}")]
    BadHash(i64, String),
    /// Replaying the log produced a different state.
    #[error("replay mismatch: {0}")]
    ReplayMismatch(String),
    /// The requested changes sit under the latest compaction snapshot: the
    /// payloads are gone, a replica restarts from the snapshot (DESIGN §4.3).
    #[error("ops below seq {snapshot_seq} were compacted: restart from the snapshot")]
    Compacted { snapshot_seq: i64 },
    /// v0.5 (P0): the cell's schema is NEWER than this binary knows. An older
    /// binary must refuse to open (let alone migrate) a cell a newer one
    /// wrote: it would write under tables it does not know and the derived
    /// state would diverge from the log. The cell is left untouched.
    #[error(
        "the cell schema is newer than this binary: schema version {found}, \
             this binary supports up to {supported} — upgrade Engramma first"
    )]
    SchemaTooNew {
        /// Schema version found in the cell.
        found: i64,
        /// Highest schema version this binary understands.
        supported: i64,
    },
    /// v0.5 (P1): the operation would land over a later correction — a
    /// `revert_decision` whose note (or supersede target) was touched by a
    /// later operation. The message names the operations in the way.
    #[error("conflict: {0}")]
    Conflict(String),
    /// v0.6.9 (I1): the write touches an id the set holds a PURGE of.
    /// «Forgotten forever» means the id does not come back: every local
    /// write that has it as its object is refused (the arrival and the
    /// replay keep the v0.6.7 rule — the purge dominates the set, the
    /// operation rests redacted); to write similar content again takes a
    /// NEW note, with a new id. The message carries the id.
    #[error("note {0} is purged: that id never comes back — write a new note")]
    NotePurged(String),
}

/// Errors returned by import/export.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PortError {
    /// I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The folder is not a valid Engramma export.
    #[error("not an engramma export: {0}")]
    NotAnExport(String),
    /// A note file could not be parsed.
    #[error("{path}: {source}")]
    Note {
        /// File that failed.
        path: PathBuf,
        /// Underlying parse failure.
        #[source]
        source: FormatError,
    },
    /// A cell-level failure while importing.
    #[error("cell error: {0}")]
    Cell(#[from] CellError),
}

/// Errors returned by search.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// SQLite failure.
    #[error("database error: {0}")]
    Db(String),
    /// Bad query parameters.
    #[error("invalid query: {0}")]
    Invalid(String),
}

impl From<rusqlite::Error> for CellError {
    fn from(e: rusqlite::Error) -> Self {
        CellError::Db(e.to_string())
    }
}

impl From<rusqlite::Error> for SearchError {
    fn from(e: rusqlite::Error) -> Self {
        SearchError::Db(e.to_string())
    }
}

/// Errors returned by embedding (the trait lives in `engramma-core`, the
/// implementations in `engramma-embed`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EmbedError {
    /// The model files are missing from the local cache (run `engramma model pull`).
    #[error("embedding model missing: {0}")]
    ModelMissing(String),
    /// The model is present but unusable (bad hash, bad config, wrong shapes).
    #[error("bad embedding model: {0}")]
    BadModel(String),
    /// A remote embedder failed (transport, HTTP status, malformed response).
    #[error("embedding endpoint error: {0}")]
    Remote(String),
    /// An embedding came back with the wrong dimension.
    #[error("embedding dimension mismatch: expected {expected}, got {got}")]
    Dimension {
        /// Expected dimension.
        expected: usize,
        /// Received dimension.
        got: usize,
    },
}

/// Result alias for format parsing.
pub type FormatResult<T> = Result<T, FormatError>;
