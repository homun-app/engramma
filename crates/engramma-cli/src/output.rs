// SPDX-License-Identifier: Apache-2.0

//! Stable JSON shapes and human-readable rendering for the CLI
//! (documented in `docs/cli.md`).

use engramma_core::format::Note;
use engramma_core::search::Hit;
use engramma_core::{CellError, PortError, SearchError};

/// Everything the CLI can fail with, with distinct exit codes.
#[derive(Debug)]
pub struct CliError {
    /// Exit code: 2 usage, 3 note not found, 4 internal, 5 incomplete
    /// forgetting (a purge left bytes in the WAL).
    pub code: i32,
    /// Human-readable message.
    pub message: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl CliError {
    /// A usage error (clap already prints these as 2).
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    /// A not-found error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: 3,
            message: message.into(),
        }
    }

    /// An internal error.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: 4,
            message: message.into(),
        }
    }

    /// An incomplete forgetting (v0.3.1, P0.2): the purge removed the note
    /// but could not truncate the WAL — a reader was open. The checkpoint
    /// is retried on the next write; the exit code says "not done yet".
    pub fn incomplete_forgetting(message: impl Into<String>) -> Self {
        Self {
            code: 5,
            message: message.into(),
        }
    }
}

impl From<CellError> for CliError {
    fn from(e: CellError) -> Self {
        match e {
            CellError::NoteNotFound(id) => CliError::not_found(format!("note {id} not found")),
            CellError::VersionNotFound(id, v) => {
                CliError::not_found(format!("version {v} of note {id} not found"))
            }
            other => CliError::internal(other.to_string()),
        }
    }
}

impl From<SearchError> for CliError {
    fn from(e: SearchError) -> Self {
        CliError::internal(e.to_string())
    }
}

impl From<PortError> for CliError {
    fn from(e: PortError) -> Self {
        CliError::internal(e.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::internal(e.to_string())
    }
}

impl From<engramma_core::FormatError> for CliError {
    fn from(e: engramma_core::FormatError) -> Self {
        CliError::usage(e.to_string())
    }
}

/// Note JSON as emitted by `--json` (complete note, canonical field order is
/// serde's sorted order).
pub fn note_json(n: &Note) -> serde_json::Value {
    n.to_json()
}

/// Hit JSON: `{score, raw_score, explain{matched_terms, bm25_rank,
/// semantic_rank, graph, cosine, fused_score, lists, winning_chunk, via,
/// confidence, valid_from, valid_to, source}, note}`.
pub fn hit_json(h: &Hit) -> serde_json::Value {
    serde_json::json!({
        "score": h.score,
        "raw_score": h.raw_score,
        "explain": {
            "matched_terms": h.explain.matched_terms,
            "bm25_rank": h.explain.bm25_rank,
            "semantic_rank": h.explain.semantic_rank,
            "graph_rank": h.explain.graph_rank,
            "cosine": h.explain.cosine,
            "fused_score": h.explain.fused_score,
            "lists": h.explain.lists,
            "support": h.explain.support,
            "winning_chunk": h.explain.winning_chunk,
            "via": h.explain.via,
            "graph": h.explain.graph.as_ref().map(|g| serde_json::json!({
                "weight": g.weight,
                "off": g.off,
            })),
            "confidence": h.explain.confidence.as_str(),
            "valid_from": h.explain.valid_from,
            "valid_to": h.explain.valid_to,
            "source": h.explain.source,
        },
        "note": note_json(&h.note),
    })
}

/// Human rendering of one hit.
pub fn hit_human(h: &Hit, index: usize) -> String {
    let mut out = format!(
        "{}. [{}] {} ({})",
        index,
        h.note.status.as_str(),
        h.note.display_title(),
        engramma_core::id::id8(&h.note.id),
    );
    out.push_str(&format!(
        "\n   punteggio {:.4} (grezzo {:.4})",
        h.score, h.raw_score
    ));
    if !h.explain.lists.is_empty() {
        out.push_str(&format!("\n   liste: {}", h.explain.lists.join(", ")));
    }
    if !h.explain.matched_terms.is_empty() {
        out.push_str(&format!(
            "\n   termini: {}",
            h.explain.matched_terms.join(", ")
        ));
    }
    if let Some(r) = h.explain.bm25_rank {
        out.push_str(&format!("\n   rango BM25: {r}"));
    }
    if let Some(r) = h.explain.semantic_rank {
        out.push_str(&format!("\n   rango semantico: {r}"));
    }
    if let Some(r) = h.explain.graph_rank {
        out.push_str(&format!("\n   rango grafo: {r}"));
    }
    if let Some(c) = h.explain.cosine {
        out.push_str(&format!("\n   coseno: {c:.4}"));
    }
    if let Some(sup) = h.explain.support {
        out.push_str(&format!("\n   supporto: {sup}"));
    }
    if let Some(chunk) = h.explain.winning_chunk {
        out.push_str(&format!("\n   blocco vincente: {chunk}"));
    }
    if let Some(via) = &h.explain.via {
        out.push_str(&format!("\n   via: {via}"));
    }
    if let Some(g) = &h.explain.graph {
        match g.off {
            Some(why) => out.push_str(&format!("\n   grafo: {why}")),
            None => out.push_str(&format!("\n   peso grafo: {:.2}", g.weight)),
        }
    }
    out.push_str(&format!(
        "\n   validità: {} → {}",
        h.explain.valid_from.as_deref().unwrap_or("sempre"),
        h.explain.valid_to.as_deref().unwrap_or("ora")
    ));
    out.push_str(&format!("\n   fonte: {}", h.explain.source));
    out
}
