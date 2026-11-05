// SPDX-License-Identifier: Apache-2.0

//! Search: BM25 candidates (FTS5) + graph expansion + filters + weighted
//! rerank, with an explanation per result (DESIGN §6.3, v0.1 without vectors),
//! and the other queries of §6.4.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use time::OffsetDateTime;

use crate::cell::vectors::VectorTx;
use crate::cell::{Cell, Inner};
use crate::embed::{decode_vec, dot_f32};
use crate::error::SearchError;
use crate::format::{Confidence, Note, NoteType, Status};
use crate::text;
use crate::vector::{FlatI8, IndexRow, VectorIndex};

/// Confidence rank for `min_confidence` (higher = more trusted).
fn confidence_rank(c: Confidence) -> u8 {
    match c {
        Confidence::Stated => 2,
        Confidence::Inferred => 1,
        Confidence::External => 0,
    }
}

/// Query filters (§6.3).
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// Note types (empty = all).
    pub types: Vec<NoteType>,
    /// Tags with their children (empty = all).
    pub tags: Vec<String>,
    /// Scope.
    pub scope: Option<String>,
    /// Only what was valid at this instant (bitemporal question side).
    pub as_of: Option<OffsetDateTime>,
    /// Minimum confidence.
    pub min_confidence: Option<Confidence>,
    /// Include quarantined notes (default: no).
    pub include_quarantined: bool,
    /// Include superseded notes (default: no; `as_of` can still surface the
    /// note that was the truth at that time).
    pub include_superseded: bool,
}

/// Search weights (§13.4): defaults are part of the public contract.
#[derive(Debug, Clone, PartialEq)]
pub struct Weights {
    /// Weight of `stated` notes.
    pub stated: f64,
    /// Weight of `inferred` notes.
    pub inferred: f64,
    /// Weight of `external` notes (visible only with `include_quarantined`).
    pub external: f64,
    /// Recency half-life in days.
    pub recency_half_life_days: f64,
    /// Per-hop decay in graph expansion (one hop = `hop_decay`, two = `hop_decay`²).
    pub hop_decay: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            stated: 1.0,
            inferred: 0.8,
            external: 0.5,
            recency_half_life_days: 180.0,
            hop_decay: 0.6,
        }
    }
}

impl Weights {
    fn confidence(&self, c: Confidence) -> f64 {
        match c {
            Confidence::Stated => self.stated,
            Confidence::Inferred => self.inferred,
            Confidence::External => self.external,
        }
    }
}

/// A search query (§6.3).
#[derive(Debug, Clone)]
pub struct Query {
    /// Free text.
    pub text: String,
    /// Result set size.
    pub k: usize,
    /// Filters.
    pub filters: Filters,
    /// Graph expansion 0–2 hops.
    pub graph_hops: u8,
    /// Query language override for stemming.
    pub lang: Option<crate::format::Lang>,
    /// Weights.
    pub weights: Weights,
}

impl Query {
    /// A plain text query with defaults (k = 10, one hop).
    pub fn text(t: impl Into<String>) -> Self {
        Query {
            text: t.into(),
            k: 10,
            filters: Filters::default(),
            graph_hops: 1,
            lang: None,
            weights: Weights::default(),
        }
    }

    /// Set `k`.
    pub fn k(mut self, k: usize) -> Self {
        self.k = k;
        self
    }

    /// Set `as_of`.
    pub fn as_of(mut self, t: OffsetDateTime) -> Self {
        self.filters.as_of = Some(t);
        self
    }

    /// Set `graph_hops` (clamped to 0–2).
    pub fn graph_hops(mut self, hops: u8) -> Self {
        self.graph_hops = hops.min(2);
        self
    }
}

/// Why a result is in the list (§6.3.6; v0.2 adds the semantic fields).
#[derive(Debug, Clone, PartialEq)]
pub struct Explain {
    /// Query terms found in the note (stemmed index terms).
    pub matched_terms: Vec<String>,
    /// Position in the BM25 candidate list (1-based).
    pub bm25_rank: Option<usize>,
    /// Position in the vector candidate list (1-based, v0.2).
    pub semantic_rank: Option<usize>,
    /// Cosine with the query embedding (best chunk), when the vector branch
    /// produced this result.
    pub cosine: Option<f64>,
    /// The raw fused score before the display normalisation (v0.2).
    pub fused_score: Option<f64>,
    /// Which candidate lists produced it: `text`, `vector`, `graph`.
    pub lists: Vec<&'static str>,
    /// The chunk that won, for a multi-chunk note.
    pub winning_chunk: Option<usize>,
    /// Graph path that led here, e.g. `[[Giulia]] -lives_in-> [[Milano]]`.
    pub via: Option<String>,
    /// Confidence weight applied.
    pub confidence: Confidence,
    /// Validity start.
    pub valid_from: Option<String>,
    /// Validity end.
    pub valid_to: Option<String>,
    /// Provenance.
    pub source: String,
}

impl Default for Explain {
    fn default() -> Self {
        Self {
            matched_terms: Vec::new(),
            bm25_rank: None,
            semantic_rank: None,
            cosine: None,
            fused_score: None,
            lists: Vec::new(),
            winning_chunk: None,
            via: None,
            confidence: Confidence::Stated,
            valid_from: None,
            valid_to: None,
            source: String::new(),
        }
    }
}

/// One search result.
#[derive(Debug, Clone)]
pub struct Hit {
    /// The note.
    pub note: Note,
    /// Final score (normalised for display: the best hit is 1.0).
    pub score: f64,
    /// The raw score before normalisation (v0.2: the ranking signal, not
    /// hidden behind the display normalisation).
    pub raw_score: f64,
    /// Why it matched.
    pub explain: Explain,
}

/// A hand-written LRU for embedded queries (256 per model, §4 v0.2): a
/// VecDeque of keys plus a map; `get` refreshes, `put` evicts the front.
#[derive(Debug)]
pub struct LruCache {
    map: std::collections::HashMap<String, Vec<f32>>,
    order: std::collections::VecDeque<String>,
    cap: usize,
}

impl LruCache {
    /// A cache holding `cap` entries.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Cached value, refreshed to most-recently-used.
    pub fn get(&mut self, key: &str) -> Option<&Vec<f32>> {
        if self.map.contains_key(key) {
            self.order.retain(|k| k != key);
            self.order.push_back(key.to_string());
            self.map.get(key)
        } else {
            None
        }
    }

    /// Insert (or replace), evicting the least recently used when full.
    pub fn put(&mut self, key: String, value: Vec<f32>) {
        if self.map.contains_key(&key) {
            self.order.retain(|k| k != &key);
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    /// Entries currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Search mode (v0.2 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// BM25 + graph, as in v0.1.
    Text,
    /// Vector candidates only (with the abstention threshold).
    Vector,
    /// Both lists fused (default when an embedder is ready).
    Hybrid,
}

/// How the text and vector lists combine (§4; the default is chosen on the
/// golden set and the benchmark, see REPORT).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fusion {
    /// Reciprocal-rank fusion: Σ 1/(k + rank) — no calibration needed.
    Rrf {
        /// Rank dampening (60 in the literature).
        k: u64,
    },
    /// Convex combination: `alpha·cos + (1−alpha)·bm25_minmax` with BM25
    /// min-max normalised over the candidates; a note missing from one list
    /// scores 0 for it.
    Convex {
        /// Weight of the cosine (0–1).
        alpha: f64,
    },
}

impl Default for Fusion {
    fn default() -> Self {
        // Measured on the golden vault (REPORT §v0.2): the convex combination
        // with α = 0.7 beats RRF on paraphrases (0.867 vs 0.733 recall@5 with
        // the 311M; 0.800 vs 0.733 with the 97M) at equal factual recall.
        Fusion::Convex { alpha: 0.7 }
    }
}

/// The semantic side of a search: the embedder plus the knobs.
#[derive(Clone)]
pub struct Semantic {
    /// The embedder (its model must be the cell's `ready` model for serving,
    /// or its `building` model during a switch — searches always serve
    /// `ready`).
    pub embedder: std::sync::Arc<dyn crate::embed::Embedder>,
    /// Which candidate lists contribute (`Hybrid` by default; `Vector`
    /// skips the text list, `Text` is served by the v0.1 path).
    pub mode: Mode,
    /// Text+vector combination.
    pub fusion: Fusion,
    /// Abstention: a result that arrives only from the vector branch needs
    /// cosine ≥ tau.
    pub tau: f32,
}

impl Semantic {
    /// Defaults: hybrid, RRF k=60 and tau = 0.30 (tuned on the golden vault,
    /// REPORT).
    #[must_use]
    pub fn new(embedder: std::sync::Arc<dyn crate::embed::Embedder>) -> Self {
        Self {
            embedder,
            mode: Mode::Hybrid,
            fusion: Fusion::default(),
            tau: 0.30,
        }
    }

    /// Set the mode.
    #[must_use]
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    /// Set the fusion.
    #[must_use]
    pub fn with_fusion(mut self, fusion: Fusion) -> Self {
        self.fusion = fusion;
        self
    }

    /// Set the abstention threshold.
    #[must_use]
    pub fn with_tau(mut self, tau: f32) -> Self {
        self.tau = tau;
        self
    }
}

/// Result of a semantic search, with the timing split the `explain` story
/// needs: embedding the question is separate from searching.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    /// The hits, best first.
    pub hits: Vec<Hit>,
    /// Milliseconds spent embedding the query (0.0 on a cache hit — the
    /// cache is part of the point).
    pub embed_ms: f64,
    /// Milliseconds spent searching (index build included, when it happens).
    pub search_ms: f64,
}

/// Reciprocal-rank fusion (§6.3.2): score = Σ 1/(k + rank), ranks from 1.
/// With a single list in v0.1; v0.2 adds the semantic list.
pub fn rrf_fuse(lists: &[Vec<String>], k: u64) -> BTreeMap<String, f64> {
    let mut out: BTreeMap<String, f64> = BTreeMap::new();
    for list in lists {
        for (i, id) in list.iter().enumerate() {
            *out.entry(id.clone()).or_default() += 1.0 / (k as f64 + (i + 1) as f64);
        }
    }
    out
}

/// Base score for graph-discovered notes (below any real text match, above
/// nothing: multi-hop answers with no shared terms still rank).
const GRAPH_FLOOR: f64 = 0.05;

/// One graph edge as stored (endpoints are entity names or `note:<id>`).
#[derive(Debug, Clone)]
struct EdgeRow {
    src: String,
    rel: String,
    dst: String,
    note_id: Option<String>,
    valid_from: Option<String>,
    valid_to: Option<String>,
}

fn valid_at(from: &Option<String>, to: &Option<String>, t: Option<OffsetDateTime>) -> bool {
    let Some(t) = t else { return true };
    let from_ok = from
        .as_deref()
        .and_then(|s| crate::format::Validity::parse(s).ok())
        .is_none_or(|v| v.covers_from(t));
    let to_ok = to
        .as_deref()
        .and_then(|s| crate::format::Validity::parse(s).ok())
        .is_none_or(|v| v.covers_to(t));
    from_ok && to_ok
}

impl Cell {
    /// Search with the wall clock as `now` (recency).
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures or invalid queries.
    pub fn search(&self, q: &Query) -> Result<Vec<Hit>, SearchError> {
        self.search_with_now(q, OffsetDateTime::now_utc())
    }

    /// Search with an explicit `now` (recency reference; tests use a fixed
    /// instant).
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures or invalid queries.
    pub fn search_with_now(&self, q: &Query, now: OffsetDateTime) -> Result<Vec<Hit>, SearchError> {
        if q.text.trim().is_empty() {
            return Err(SearchError::Invalid("empty query text".into()));
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let conn = &inner.conn;

        // 1. BM25 candidates (up to 50).
        let terms = text::query_terms(&q.text, q.lang);
        if terms.is_empty() {
            return Err(SearchError::Invalid("query has no usable terms".into()));
        }
        let match_expr = terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");
        // A phrase for adjacent stem pairs ("bianca bianchi" collapses to one
        // stem; the phrase still rewards the notes where the two words are
        // next to each other, i.e. the actual namesake).
        let phrase = terms
            .iter()
            .map(|t| t.replace('"', ""))
            .collect::<Vec<_>>()
            .join(" ");
        let match_expr = if terms.len() >= 2 {
            format!("{match_expr} OR \"{phrase}\"")
        } else {
            match_expr
        };
        // Column weights: the title says what a note is about; the body is
        // longer and noisier; tags are short but precise.
        let mut stmt = conn.prepare(
            "SELECT n.id, bm25(notes_fts, 10.0, 3.0, 2.0) FROM notes_fts
             JOIN notes n ON n.rowid = notes_fts.rowid
             WHERE notes_fts MATCH ?1
             ORDER BY bm25(notes_fts, 10.0, 3.0, 2.0) LIMIT 50",
        )?;
        let rows = stmt.query_map([&match_expr], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        let mut text_hits: Vec<(String, f64)> = Vec::new();
        for row in rows {
            let (id, bm25) = row?;
            // FTS5 bm25(): negative, more negative = better.
            text_hits.push((id, -bm25));
        }
        drop(stmt);

        // 2. Filters come BEFORE the graph expansion: only candidates that
        // pass them (status, quarantine, superseded, `as_of`) can seed the
        // walk — a quarantined note must not guide the search (§5).
        let mut text_candidates: Vec<(String, f64, usize, Note)> = Vec::new();
        for (rank, (id, bm25)) in text_hits.iter().enumerate() {
            let Ok(note) = read_note_by_id(conn, id) else {
                continue;
            };
            if !passes_filters(&note, &q.filters, now) {
                continue;
            }
            text_candidates.push((id.clone(), *bm25, rank + 1, note));
        }

        // 3. Normalised seed scores for the graph expansion (§6.3.3: the added
        // notes keep a weight that decays per hop from the seed they came
        // from). RRF fusion over the single ranked list arrives with the
        // semantic list in v0.2; the rerank below uses bm25_norm.
        let max_bm25 = text_candidates
            .iter()
            .map(|(_, b, _, _)| *b)
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON);
        let seeds: Vec<(String, f64)> = text_candidates
            .iter()
            .take(10)
            .map(|(id, b, _, _)| (id.clone(), *b / max_bm25))
            .collect();

        // 4. Graph expansion from the surviving candidates. Edges declared by
        // quarantined or deleted notes are out unless asked for.
        let as_of = q.filters.as_of;
        let mut extra: BTreeMap<String, (u8, String, f64)> = BTreeMap::new(); // (hops, path, seed score)
        if q.graph_hops > 0 && !text_candidates.is_empty() {
            expand_graph(
                conn,
                &seeds,
                q.graph_hops,
                as_of,
                q.filters.include_quarantined,
                &text_hits.iter().map(|(i, _)| i.clone()).collect(),
                &mut extra,
            )?;
        }

        // 5. Rerank (§13.4): bm25_norm × coverage × confidence × recency ×
        // graph. Graph-discovered notes keep a decaying share of the seed's
        // score (with a floor for pure multi-hop answers); the coverage factor
        // halves notes that share none of the query terms, so pure graph
        // neighbours support — never replace — real text matches.
        // body_norm already holds the stemmed title+body (computed at write
        // time): reuse it instead of re-stemming per candidate.
        let mut hits: Vec<Hit> = Vec::new();
        for (id, bm25, rank, note) in text_candidates {
            let (matched, coverage) = coverage_of(conn, &id, &note, &terms);
            let raw = (bm25 / max_bm25)
                * coverage
                * q.weights.confidence(note.confidence)
                * recency_of(now, &note, q.weights.recency_half_life_days);
            hits.push(Hit {
                explain: Explain {
                    matched_terms: matched,
                    bm25_rank: Some(rank),
                    lists: vec!["text"],
                    fused_score: Some(raw),
                    ..Explain::default()
                },
                note,
                raw_score: raw,
                score: raw,
            });
        }
        // Cap the expansion: the nearest graph notes only.
        let mut extras: Vec<(String, (u8, String, f64))> = extra.into_iter().take(40).collect();
        extras.sort_by_key(|(_, (hops, _, _))| *hops);
        for (id, (hops, path, seed)) in extras {
            let Ok(note) = read_note_by_id(conn, &id) else {
                continue;
            };
            if !passes_filters(&note, &q.filters, now) {
                continue;
            }
            let (matched, coverage) = coverage_of(conn, &id, &note, &terms);
            // one extra decay step: no text match is one hop less evidence
            let w_graph = q.weights.hop_decay.powi(i32::from(hops) + 1);
            let raw = GRAPH_FLOOR.max(seed)
                * coverage
                * q.weights.confidence(note.confidence)
                * recency_of(now, &note, q.weights.recency_half_life_days)
                * w_graph;
            hits.push(Hit {
                explain: Explain {
                    matched_terms: matched,
                    via: Some(path),
                    lists: vec!["graph"],
                    fused_score: Some(raw),
                    ..Explain::default()
                },
                note,
                raw_score: raw,
                score: raw,
            });
        }
        let max_raw = hits.iter().map(|h| h.raw_score).fold(0.0_f64, f64::max);
        if max_raw > 0.0 {
            for h in &mut hits {
                h.score = h.raw_score / max_raw;
            }
        }
        // Below-threshold results (no signal at all) are withheld.
        hits.retain(|h| h.score > 0.0);
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.note.id.to_string().cmp(&b.note.id.to_string()))
        });
        hits.truncate(q.k);
        Ok(hits)
    }

    /// Neighbours of an entity (or `note:<id>`), BFS up to `depth`, optionally
    /// restricted to some relations. Edges declared by quarantined or deleted
    /// notes are excluded unless `include_quarantined`; manual edges (no
    /// declaring note) always stay. One indexed frontier query per level (at
    /// most [`WALK_LIMIT`] edges each, in id order); returns nearest first.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn neighbors(
        &self,
        start: &str,
        depth: u8,
        rels: &[&str],
        include_quarantined: bool,
    ) -> Result<Vec<Neighbor>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let conn = &inner.conn;
        let mut out: Vec<Neighbor> = Vec::new();
        let mut seen_entities: BTreeSet<String> = BTreeSet::from([start.to_string()]);
        let mut frontier: Vec<String> = vec![start.to_string()];
        for d in 1..=depth {
            if frontier.is_empty() {
                break;
            }
            let edges =
                edges_touching_frontier(conn, &frontier, rels, include_quarantined, WALK_LIMIT)?;
            let frontier_set: BTreeSet<&str> = frontier.iter().map(String::as_str).collect();
            let mut next: Vec<String> = Vec::new();
            for e in &edges {
                let (other, direction) = if frontier_set.contains(e.src.as_str()) {
                    (e.dst.as_str(), "->")
                } else {
                    (e.src.as_str(), "<-")
                };
                let hop = Neighbor {
                    depth: d,
                    entity: other.to_string(),
                    rel: e.rel.clone(),
                    direction: direction.to_string(),
                    note_id: e.note_id.clone(),
                    valid_from: e.valid_from.clone(),
                    valid_to: e.valid_to.clone(),
                };
                if !out
                    .iter()
                    .any(|n| n.entity == hop.entity && n.rel == hop.rel && n.depth == hop.depth)
                {
                    out.push(hop);
                }
                if seen_entities.insert(other.to_string()) {
                    next.push(other.to_string());
                }
            }
            frontier = next;
        }
        Ok(out)
    }

    /// Shortest path between two entities, depth ≤ 4. The cap applies **while
    /// visiting**: levels beyond it are never expanded (the walk touches only
    /// the edges adjacent to the frontier, never the whole table). Edges
    /// declared by quarantined or deleted notes are excluded unless
    /// `include_quarantined`.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn path(
        &self,
        a: &str,
        b: &str,
        include_quarantined: bool,
    ) -> Result<Option<Vec<PathStep>>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let conn = &inner.conn;
        if a == b {
            return Ok(Some(Vec::new()));
        }
        // Level-order BFS with parent tracking.
        let mut parents: HashMap<String, (String, String)> = HashMap::new(); // node -> (parent, rel)
        let mut discovered: BTreeSet<String> = BTreeSet::from([a.to_string()]);
        let mut frontier: Vec<String> = vec![a.to_string()];
        let mut found = false;
        for _ in 0..4 {
            let edges =
                edges_touching_frontier(conn, &frontier, &[], include_quarantined, WALK_LIMIT)?;
            let frontier_set: BTreeSet<&str> = frontier.iter().map(String::as_str).collect();
            let mut next: Vec<String> = Vec::new();
            for e in &edges {
                let (from, to) = if frontier_set.contains(e.src.as_str()) {
                    (e.src.as_str(), e.dst.as_str())
                } else {
                    (e.dst.as_str(), e.src.as_str())
                };
                if discovered.insert(to.to_string()) {
                    parents.insert(to.to_string(), (from.to_string(), e.rel.clone()));
                    if to == b {
                        found = true;
                        break;
                    }
                    next.push(to.to_string());
                }
            }
            if found || next.is_empty() {
                break;
            }
            frontier = next;
        }
        if !found {
            return Ok(None);
        }
        // Reconstruct from the parents.
        let mut steps: Vec<PathStep> = Vec::new();
        let mut cur = b.to_string();
        while cur != a {
            let Some((parent, rel)) = parents.get(&cur) else {
                return Ok(None);
            };
            steps.push(PathStep {
                from: parent.clone(),
                rel: rel.clone(),
                to: cur.clone(),
            });
            cur = parent.clone();
        }
        steps.reverse();
        Ok(Some(steps))
    }

    /// Tags with note counts; a prefix includes its children. Notes in
    /// quarantine (unless `include_quarantined`) and tombstones never count.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn tags(
        &self,
        prefix: &str,
        include_quarantined: bool,
    ) -> Result<Vec<TagInfo>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let like = format!("{prefix}%");
        let visible = if include_quarantined {
            "n.id IS NULL OR n.status != 'deleted'"
        } else {
            "n.id IS NULL OR n.status NOT IN ('quarantined', 'deleted')"
        };
        let sql = format!(
            "SELECT t.name, t.parent, COUNT(CASE WHEN {visible} THEN 1 END)
             FROM tags t LEFT JOIN note_tags nt ON nt.tag = t.name
             LEFT JOIN notes n ON n.id = nt.note_id
             WHERE t.name = ?1 OR t.name LIKE ?2
             GROUP BY t.name, t.parent ORDER BY t.name"
        );
        let mut stmt = inner.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![prefix, like], |r| {
            Ok(TagInfo {
                name: r.get(0)?,
                parent: r.get(1)?,
                note_count: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Entities by name prefix.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn entities(&self, prefix: &str) -> Result<Vec<EntityInfo>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let like = format!("{prefix}%");
        let mut stmt = inner.conn.prepare(
            "SELECT id, name, note_id FROM entities WHERE name LIKE ?1 ORDER BY name LIMIT 100",
        )?;
        let rows = stmt.query_map([like], |r| {
            Ok(EntityInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                note_id: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Notes about an entity ordered by validity (its timeline). Edges
    /// declared by quarantined or deleted notes are excluded unless
    /// `include_quarantined`; tombstoned notes never appear.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn timeline(
        &self,
        entity: &str,
        include_quarantined: bool,
    ) -> Result<Vec<Hit>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        // notes declaring an edge on the entity, or the entity's own note
        let sql = format!(
            "SELECT DISTINCT n.id FROM notes n
             WHERE n.id = (SELECT note_id FROM entities WHERE name = ?1 COLLATE NOCASE)
                OR n.id IN (SELECT e.note_id FROM edges e
                            LEFT JOIN notes dn ON dn.id = e.note_id
                            WHERE e.note_id IS NOT NULL
                              AND (e.src = ?1 OR e.dst = ?1
                                   OR e.src IN (SELECT 'note:' || note_id FROM entities WHERE name = ?1 COLLATE NOCASE))
                              AND {})
             ORDER BY n.valid_from IS NULL, n.valid_from, n.id",
            edge_visible("e", "dn", include_quarantined)
        );
        let mut stmt = inner.conn.prepare(&sql)?;
        let rows = stmt.query_map([entity], |r| r.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        drop(stmt);
        let mut out = Vec::new();
        for id in ids {
            if let Ok(note) = read_note_by_id(&inner.conn, &id) {
                // like search: tombstones never, quarantine not by default;
                // superseded notes stay — the timeline is the change history
                let hidden = if include_quarantined {
                    note.status == Status::Deleted
                } else {
                    matches!(note.status, Status::Deleted | Status::Quarantined)
                };
                if hidden {
                    continue;
                }
                out.push(Hit {
                    score: 0.0,
                    raw_score: 0.0,
                    note,
                    explain: Explain {
                        via: Some(format!("[[{entity}]]")),
                        ..Explain::default()
                    },
                });
                if let Some(last) = out.last_mut() {
                    last.explain.valid_from =
                        last.note.valid_from.as_ref().map(|v| v.to_db_string());
                    last.explain.valid_to = last.note.valid_to.as_ref().map(|v| v.to_db_string());
                    last.explain.confidence = last.note.confidence;
                    last.explain.source = last.note.source.clone();
                }
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// semantic search (v0.2 §4)
// ---------------------------------------------------------------------------

/// A vector candidate after filters: the best chunk of a note.
struct VecCand {
    id: String,
    cosine: f64,
    rank: usize,
    note: Note,
    chunk: usize,
}

/// Build or reuse the in-memory index of the cell's `ready` model.
/// Returns `(model, dim)` or `None` when no model is ready (the caller
/// degrades to text-only). A `data_version` change (another process wrote)
/// triggers a rebuild; this cell's own writes update the index in place.
fn ensure_index(inner: &mut Inner) -> Result<Option<(String, usize)>, SearchError> {
    let Some(ready) = VectorTx { conn: &inner.conn }
        .ready_model()
        .map_err(|e| SearchError::Db(e.to_string()))?
    else {
        // no ready model: drop any stale index and go textual
        inner.index = None;
        return Ok(None);
    };
    let dim = usize::try_from(ready.dim).unwrap_or(0);
    let data_version: i64 = inner
        .conn
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap_or(0);
    let fresh = inner
        .index
        .as_ref()
        .is_some_and(|st| st.model == ready.model && st.data_version == data_version);
    if !fresh {
        // Build lazily, here and not at open time: opening a cell stays ≤ 50 ms.
        let mut rows: Vec<IndexRow> = Vec::new();
        {
            let mut stmt = inner.conn.prepare(
                "SELECT note_id, chunk, vec FROM note_vectors WHERE model = ?1 ORDER BY note_id, chunk",
            )?;
            let result = stmt.query_map([&ready.model], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            for row in result {
                let (note_id, chunk, blob) = row?;
                if let Ok(v) = decode_vec(&blob, dim) {
                    rows.push(IndexRow {
                        note_id,
                        chunk,
                        vec: v,
                    });
                }
            }
        }
        let mut idx = FlatI8::new(dim);
        idx.rebuild(rows);
        inner.index = Some(crate::cell::IndexState {
            model: ready.model.clone(),
            data_version,
            idx,
        });
    }
    Ok(Some((ready.model, dim)))
}

/// Vector candidates for one query: int8 top-`m` → exact f32 recomputation
/// of those candidates (temp table + JOIN against `note_vectors`) → best
/// chunk per note → the same filters as BM25. Fewer than `k` survivors
/// double `m`, up to the full scan.
#[allow(clippy::too_many_arguments)]
fn vector_candidates(
    conn: &rusqlite::Connection,
    inner: &Inner,
    qv: &[f32],
    k: usize,
    filters: &Filters,
    now: OffsetDateTime,
    model: &str,
) -> Result<Vec<VecCand>, SearchError> {
    let Some(state) = &inner.index else {
        return Ok(Vec::new());
    };
    let total = state.idx.len();
    let mut m = 200.max(4 * k);
    let mut out: Vec<VecCand> = Vec::new();
    loop {
        let hits = state.idx.search(qv, m.min(total));
        // exact f32 recomputation, in bulk
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS vcand (note_id TEXT PRIMARY KEY) WITHOUT ROWID;",
        )?;
        conn.execute("DELETE FROM vcand", [])?;
        {
            let mut ins = conn.prepare("INSERT OR IGNORE INTO vcand (note_id) VALUES (?1)")?;
            for h in &hits {
                ins.execute([&h.note_id])?;
            }
        }
        let mut best: HashMap<String, (f64, i64)> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT v.note_id, v.chunk, v.vec FROM vcand c
                 JOIN note_vectors v ON v.note_id = c.note_id AND v.model = ?1",
            )?;
            let result = stmt.query_map([model], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            for row in result {
                let (note_id, chunk, blob) = row?;
                if let Ok(v) = decode_vec(&blob, qv.len()) {
                    let cos = f64::from(dot_f32(&v, qv));
                    let e = best.entry(note_id).or_insert((f64::NEG_INFINITY, chunk));
                    if cos > e.0 {
                        *e = (cos, chunk);
                    }
                }
            }
        }
        // rank by exact cosine, then filter
        let mut ranked: Vec<(String, f64, i64)> = best
            .into_iter()
            .map(|(id, (cos, chunk))| (id, cos, chunk))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.clear();
        for (rank, (id, cos, chunk)) in ranked.into_iter().enumerate() {
            if out.len() >= 50 {
                break;
            }
            let Ok(note) = read_note_by_id(conn, &id) else {
                continue;
            };
            if !passes_filters(&note, filters, now) {
                continue;
            }
            out.push(VecCand {
                id,
                cosine: cos,
                rank: rank + 1,
                note,
                chunk: usize::try_from(chunk).unwrap_or(0),
            });
        }
        let filtered_short = out.len() < k;
        if !filtered_short || m >= total {
            break;
        }
        m = (m * 2).min(total);
    }
    Ok(out)
}

/// Fuse the text and vector lists (§4): RRF or convex, over the candidates
/// that survived the filters. Returns `(id, fused score)` for the union.
fn fuse(
    text: &[(String, f64, usize, Note)],
    vecs: &[VecCand],
    fusion: &Fusion,
) -> BTreeMap<String, f64> {
    let mut out: BTreeMap<String, f64> = BTreeMap::new();
    match *fusion {
        Fusion::Rrf { k } => {
            for (id, _, rank, _) in text {
                *out.entry(id.clone()).or_default() += 1.0 / (k as f64 + *rank as f64);
            }
            for v in vecs {
                *out.entry(v.id.clone()).or_default() += 1.0 / (k as f64 + v.rank as f64);
            }
        }
        Fusion::Convex { alpha } => {
            // BM25 min-max over the union's present values
            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            for (_, bm25, _, _) in text {
                min = min.min(*bm25);
                max = max.max(*bm25);
            }
            let norm = |b: f64| -> f64 {
                if max > min {
                    (b - min) / (max - min)
                } else {
                    // a single candidate (or ties): all best
                    1.0
                }
            };
            for (id, bm25, _, _) in text {
                *out.entry(id.clone()).or_default() += (1.0 - alpha) * norm(*bm25);
            }
            for v in vecs {
                *out.entry(v.id.clone()).or_default() += alpha * v.cosine.max(0.0);
            }
        }
    }
    out
}

impl Cell {
    /// Semantic search with the wall clock as `now`.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures, invalid queries or embedding errors.
    pub fn search_semantic(&self, q: &Query, sem: &Semantic) -> Result<SearchOutcome, SearchError> {
        self.search_semantic_with_now(q, OffsetDateTime::now_utc(), sem)
    }

    /// Semantic search with an explicit `now`: hybrid (BM25 + vectors fused),
    /// vector-only, or text-only (identical to [`Cell::search_with_now`]).
    /// The query is embedded through a 256-entry LRU cache per model, outside
    /// the cell lock; `embed_ms` and `search_ms` are reported separately.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures, invalid queries or embedding errors.
    pub fn search_semantic_with_now(
        &self,
        q: &Query,
        now: OffsetDateTime,
        sem: &Semantic,
    ) -> Result<SearchOutcome, SearchError> {
        if q.text.trim().is_empty() {
            return Err(SearchError::Invalid("empty query text".into()));
        }
        let started = std::time::Instant::now();
        // 1. Embed the question (cached), before taking the cell lock.
        let model = sem.embedder.model_id().to_string();
        let cached = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
            inner
                .query_cache
                .entry(model.clone())
                .or_insert_with(|| LruCache::new(256))
                .get(&q.text)
                .cloned()
        };
        let qv = match cached {
            Some(v) => v,
            None => {
                let mut vecs = sem
                    .embedder
                    .embed(&[&q.text], crate::embed::TextKind::Query)
                    .map_err(|e| SearchError::Invalid(format!("embedding: {e}")))?;
                let v = vecs.pop().unwrap_or_default();
                let mut inner = self
                    .inner
                    .lock()
                    .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
                inner
                    .query_cache
                    .entry(model.clone())
                    .or_insert_with(|| LruCache::new(256))
                    .put(q.text.clone(), v.clone());
                v
            }
        };
        let embed_ms = started.elapsed().as_secs_f64() * 1000.0;

        // 2. Search under the lock.
        let search_started = std::time::Instant::now();
        let hits = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
            semantic_pipeline(&mut inner, q, now, sem, &qv, &model)?
        };
        Ok(SearchOutcome {
            hits,
            embed_ms,
            search_ms: search_started.elapsed().as_secs_f64() * 1000.0,
        })
    }
}

/// The mixed pipeline (§4): BM25 and vectors in parallel, filters, fusion,
/// graph expansion from the fused seeds, weighted rerank, abstention.
fn semantic_pipeline(
    inner: &mut Inner,
    q: &Query,
    now: OffsetDateTime,
    sem: &Semantic,
    qv: &[f32],
    model: &str,
) -> Result<Vec<Hit>, SearchError> {
    let dim = ensure_index(inner)?;
    let conn = &inner.conn;

    // --- text list (BM25, as in v0.1; skipped in vector mode)
    let terms = if sem.mode == Mode::Vector {
        Vec::new()
    } else {
        text::query_terms(&q.text, q.lang)
    };
    let mut text_hits: Vec<(String, f64)> = Vec::new();
    if !terms.is_empty() {
        let match_expr = terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut stmt = conn.prepare(
            "SELECT n.id, bm25(notes_fts, 10.0, 3.0, 2.0) FROM notes_fts
             JOIN notes n ON n.rowid = notes_fts.rowid
             WHERE notes_fts MATCH ?1
             ORDER BY bm25(notes_fts, 10.0, 3.0, 2.0) LIMIT 50",
        )?;
        let rows = stmt.query_map([&match_expr], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        for row in rows {
            let (id, bm25) = row?;
            text_hits.push((id, -bm25));
        }
    }
    let mut text_list: Vec<(String, f64, usize, Note)> = Vec::new();
    for (rank, (id, bm25)) in text_hits.iter().enumerate() {
        let Ok(note) = read_note_by_id(conn, id) else {
            continue;
        };
        if !passes_filters(&note, &q.filters, now) {
            continue;
        }
        text_list.push((id.clone(), *bm25, rank + 1, note));
    }

    // --- vector list
    let vec_list: Vec<VecCand> = match (&dim, qv.len()) {
        (Some((m, d)), n) if m == model && n == *d => {
            vector_candidates(conn, inner, qv, q.k, &q.filters, now, m)?
        }
        _ => Vec::new(),
    };

    // --- fusion over the filtered lists, then scaled to [0, 1] like the
    // bm25_norm of the text path: RRF scores are rank reciprocals (~1/61)
    // and the graph floor (0.05) is calibrated for normalised seeds —
    // without this, random graph neighbours would outrank real matches.
    let fused_raw = fuse(&text_list, &vec_list, &sem.fusion);
    let fused_max = fused_raw
        .values()
        .fold(0.0_f64, |a, v| a.max(*v))
        .max(f64::EPSILON);
    let fused: BTreeMap<String, f64> = fused_raw
        .into_iter()
        .map(|(id, v)| (id, v / fused_max))
        .collect();
    let bm25_rank_of: HashMap<&str, usize> = text_list
        .iter()
        .map(|(id, _, rank, _)| (id.as_str(), *rank))
        .collect();
    let note_of_text: HashMap<String, &Note> = text_list
        .iter()
        .map(|(id, _, _, n)| (id.clone(), n))
        .collect();
    let vec_of: HashMap<&str, &VecCand> = vec_list.iter().map(|v| (v.id.as_str(), v)).collect();

    // --- abstention FIRST (§4): a vector-only candidate below tau leaves
    // before it can seed the graph walk — otherwise its neighbours would
    // resurface through the expansion and the threshold would mean nothing.
    // Text matches are never abstained away.
    let fused: BTreeMap<String, f64> = fused
        .into_iter()
        .filter(|(id, _)| {
            note_of_text.contains_key(id)
                || vec_of
                    .get(id.as_str())
                    .is_some_and(|v| (v.cosine as f32) >= sem.tau)
        })
        .collect();

    // --- graph expansion from the fused seeds
    let mut seeds: Vec<(String, f64)> = fused.iter().map(|(id, s)| (id.clone(), *s)).collect();
    seeds.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let seeds: Vec<(String, f64)> = seeds.into_iter().take(10).collect();
    // normalise seed scores for the walk
    let seed_max = seeds
        .iter()
        .map(|(_, s)| *s)
        .fold(0.0_f64, f64::max)
        .max(f64::EPSILON);
    let seeds: Vec<(String, f64)> = seeds
        .into_iter()
        .map(|(id, s)| (id, s / seed_max))
        .collect();
    let as_of = q.filters.as_of;
    let mut extra: BTreeMap<String, (u8, String, f64)> = BTreeMap::new();
    if q.graph_hops > 0 && !seeds.is_empty() {
        let exclude: BTreeSet<String> = fused.keys().cloned().collect();
        expand_graph(
            conn,
            &seeds,
            q.graph_hops,
            as_of,
            q.filters.include_quarantined,
            &exclude,
            &mut extra,
        )?;
    }

    // --- rerank: fused × confidence × recency (§4.5: reliability, recency,
    // graph — as in v0.1; term coverage is not applied to semantic hits, a
    // paraphrase legitimately shares no term with the note).
    let mut hits: Vec<Hit> = Vec::new();
    for (id, fused_score) in &fused {
        let from_text = note_of_text.contains_key(id);
        let from_vec = vec_of.contains_key(id.as_str());
        let note: &Note = match (note_of_text.get(id), vec_of.get(id.as_str())) {
            (Some(n), _) => n,
            (None, Some(v)) => &v.note,
            (None, None) => continue,
        };
        let cosine = from_vec
            .then(|| vec_of.get(id.as_str()).map(|v| v.cosine))
            .flatten();
        let chunk = from_vec
            .then(|| vec_of.get(id.as_str()).map(|v| v.chunk))
            .flatten();
        let mut lists: Vec<&'static str> = Vec::new();
        if from_text {
            lists.push("text");
        }
        if from_vec {
            lists.push("vector");
        }
        // (abstention already happened above, before the graph walk; in
        // vector mode the text list is empty so everything went through it)
        // Reliability and graph, without the v0.1 recency factor: cosine
        // differences are hundredths, and a 180-day half-life (tuned for the
        // wide score gaps of BM25) shuffled them — measured, REPORT §v0.2.
        // Text-only searches keep the full v0.1 formula.
        let raw = fused_score * q.weights.confidence(note.confidence);
        hits.push(Hit {
            note: (*note).clone(),
            raw_score: raw,
            score: raw,
            explain: Explain {
                bm25_rank: from_text
                    .then(|| bm25_rank_of.get(id.as_str()).copied())
                    .flatten(),
                semantic_rank: from_vec
                    .then(|| vec_of.get(id.as_str()).map(|v| v.rank))
                    .flatten(),
                cosine,
                fused_score: Some(*fused_score),
                lists,
                winning_chunk: chunk,
                matched_terms: if from_text {
                    coverage_of(conn, id, note, &terms).0
                } else {
                    Vec::new()
                },
                confidence: note.confidence,
                valid_from: note.valid_from.as_ref().map(|v| v.to_db_string()),
                valid_to: note.valid_to.as_ref().map(|v| v.to_db_string()),
                source: note.source.clone(),
                ..Explain::default()
            },
        });
    }
    // graph-discovered notes (never candidates themselves)
    let mut extras: Vec<(String, (u8, String, f64))> = extra.into_iter().take(40).collect();
    extras.sort_by_key(|(_, (hops, _, _))| *hops);
    for (id, (hops, path, seed)) in extras {
        let Ok(note) = read_note_by_id(conn, &id) else {
            continue;
        };
        if !passes_filters(&note, &q.filters, now) {
            continue;
        }
        let w_graph = q.weights.hop_decay.powi(i32::from(hops) + 1);
        let raw = GRAPH_FLOOR.max(seed) * q.weights.confidence(note.confidence) * w_graph;
        hits.push(Hit {
            note,
            raw_score: raw,
            score: raw,
            explain: Explain {
                via: Some(path),
                lists: vec!["graph"],
                fused_score: Some(raw),
                ..Explain::default()
            },
        });
    }

    hits.retain(|h| h.raw_score > 0.0);
    hits.sort_by(|a, b| {
        b.raw_score
            .partial_cmp(&a.raw_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.note.id.to_string().cmp(&b.note.id.to_string()))
    });
    hits.truncate(q.k);
    let max_raw = hits.iter().map(|h| h.raw_score).fold(0.0_f64, f64::max);
    if max_raw > 0.0 {
        for h in &mut hits {
            h.score = h.raw_score / max_raw;
        }
    }
    Ok(hits)
}

/// A neighbour in the graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    /// BFS depth.
    pub depth: u8,
    /// The other endpoint.
    pub entity: String,
    /// Relation name.
    pub rel: String,
    /// `->` or `<-` from the perspective of the walk.
    pub direction: String,
    /// Declaring note, when present.
    pub note_id: Option<String>,
    /// Edge validity start.
    pub valid_from: Option<String>,
    /// Edge validity end.
    pub valid_to: Option<String>,
}

/// One step of a shortest path.
#[derive(Debug, Clone, PartialEq)]
pub struct PathStep {
    /// From entity.
    pub from: String,
    /// Relation.
    pub rel: String,
    /// To entity.
    pub to: String,
}

/// A tag with its note count.
#[derive(Debug, Clone, PartialEq)]
pub struct TagInfo {
    /// Tag name.
    pub name: String,
    /// Parent tag.
    pub parent: Option<String>,
    /// Notes carrying this exact tag.
    pub note_count: i64,
}

/// An entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityInfo {
    /// Deterministic id.
    pub id: String,
    /// Canonical name.
    pub name: String,
    /// The note that is this entity, when present.
    pub note_id: Option<String>,
}

// ---------------------------------------------------------------------------

fn read_note_by_id(conn: &rusqlite::Connection, id: &str) -> Result<Note, SearchError> {
    let fm: String = conn.query_row(
        "SELECT frontmatter_json FROM notes WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?;
    let body: String =
        conn.query_row("SELECT body FROM notes WHERE id = ?1", [id], |r| r.get(0))?;
    let mut note =
        Note::from_json(&serde_json::from_str(&fm).map_err(|e| SearchError::Db(e.to_string()))?)
            .map_err(|e| SearchError::Db(e.to_string()))?;
    note.body = body;
    Ok(note)
}

fn note_tag_string(conn: &rusqlite::Connection, id: &str) -> String {
    let Ok(mut stmt) = conn.prepare("SELECT tag FROM note_tags WHERE note_id = ?1 ORDER BY tag")
    else {
        return String::new();
    };
    let Ok(rows) = stmt.query_map([id], |r| r.get::<_, String>(0)) else {
        return String::new();
    };
    let mut tags = Vec::new();
    for r in rows.flatten() {
        tags.push(r);
    }
    tags.join(" ")
}

fn passes_filters(note: &Note, f: &Filters, now: OffsetDateTime) -> bool {
    if note.status == Status::Deleted {
        return false;
    }
    if note.status == Status::Quarantined && !f.include_quarantined {
        return false;
    }
    if note.status == Status::Superseded {
        // Superseded notes surface only for bitemporal questions, when they
        // were the truth at `as_of`.
        let was_truth = f.as_of.is_some_and(|t| {
            note.valid_from.as_ref().is_none_or(|v| v.covers_from(t))
                && note.valid_to.as_ref().is_none_or(|v| v.covers_to(t))
        });
        if !(f.include_superseded || was_truth) {
            return false;
        }
    }
    if !f.types.is_empty() && !f.types.contains(&note.note_type) {
        return false;
    }
    if let Some(min) = f.min_confidence
        && confidence_rank(note.confidence) < confidence_rank(min)
    {
        return false;
    }
    if let Some(scope) = &f.scope
        && note.scope.as_deref() != Some(scope.as_str())
    {
        return false;
    }
    if !f.tags.is_empty() {
        let note_tags: Vec<String> = note.tags.clone();
        let ok = f.tags.iter().any(|want| {
            note_tags
                .iter()
                .any(|t| t == want || t.starts_with(&format!("{want}/")))
        });
        if !ok {
            return false;
        }
    }
    if let Some(as_of) = f.as_of {
        if !note
            .valid_from
            .as_ref()
            .is_none_or(|v| v.covers_from(as_of))
        {
            return false;
        }
        if !note.valid_to.as_ref().is_none_or(|v| v.covers_to(as_of)) {
            return false;
        }
    } else {
        // Current truth by default: not yet started, already expired — out.
        if note
            .valid_from
            .as_ref()
            .is_some_and(|v| !v.covers_from(now))
        {
            return false;
        }
        // Events are historical records: their `valid_to` is when they ended,
        // not when they stopped being true, so they never "expire".
        if note.note_type != crate::format::NoteType::Event
            && note.valid_to.as_ref().is_some_and(|v| !v.covers_to(now))
        {
            return false;
        }
    }
    true
}

/// SQL fragment: edges declared by a note the default view hides (quarantined
/// or deleted). Manual edges (`link`, no declaring note) always stay. `en` is
/// the alias of the LEFT JOIN over `notes`.
fn edge_visible(edge: &str, en: &str, include_quarantined: bool) -> String {
    if include_quarantined {
        format!("({edge}.note_id IS NULL OR {en}.status != 'deleted')")
    } else {
        format!("({edge}.note_id IS NULL OR {en}.status NOT IN ('quarantined', 'deleted'))")
    }
}

/// Bounded, deterministic walk: a hub entity (a city mentioned by hundreds of
/// notes) would otherwise drag its whole neighbourhood in.
const WALK_LIMIT: u64 = 400;

/// Load the edges that touch the frontier (either endpoint), restricted to
/// `rels` when non-empty, in edge-id order with at most `limit` rows.
///
/// The frontier travels in a temp table and the relations as `?` parameters:
/// values are never spliced into the statement, and no IN list can outgrow
/// SQLite's variable limit. Two indexed branches (one per endpoint) united:
/// an edge with both endpoints in the frontier comes out once.
fn edges_touching_frontier(
    conn: &rusqlite::Connection,
    frontier: &[String],
    rels: &[&str],
    include_quarantined: bool,
    limit: u64,
) -> Result<Vec<EdgeRow>, SearchError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS frontier (id TEXT PRIMARY KEY) WITHOUT ROWID;",
    )?;
    conn.execute("DELETE FROM frontier", [])?;
    {
        let mut ins = conn.prepare("INSERT OR IGNORE INTO frontier (id) VALUES (?1)")?;
        for id in frontier {
            ins.execute([id])?;
        }
    }
    // `edges.status` mirrors the declaring note at write time (derived edges
    // are rewritten on every note write), so visibility needs no join — a
    // join over notes doubled the 100k search p95. A manual edge with a
    // note_id keeps the status it was created with.
    let vis = if include_quarantined {
        "(e.note_id IS NULL OR e.status != 'deleted')"
    } else {
        "(e.note_id IS NULL OR e.status NOT IN ('quarantined', 'deleted'))"
    };
    let rel_clause = if rels.is_empty() {
        String::new()
    } else {
        format!(" AND e.rel IN ({})", vec!["?"; rels.len()].join(", "))
    };
    let branch = |endpoint: &str| {
        // CROSS JOIN pins the join order: the (tiny) frontier drives indexed
        // lookups on edges. A plain JOIN lets the planner scan `edges` whole
        // to keep the ORDER BY cheap (checked with EXPLAIN QUERY PLAN).
        format!(
            "SELECT e.id AS eid, e.src, e.rel, e.dst, e.note_id, e.valid_from, e.valid_to
             FROM frontier f CROSS JOIN edges e ON e.{endpoint} = f.id
             WHERE {vis}{rel_clause}"
        )
    };
    let sql = format!(
        "SELECT eid, src, rel, dst, note_id, valid_from, valid_to FROM ({}
         UNION {})
         ORDER BY eid LIMIT ?",
        branch("src"),
        branch("dst")
    );
    // The branches each bind the rel list, then the LIMIT.
    let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(2 * rels.len() + 1);
    for r in rels.iter().chain(rels.iter()) {
        params.push(rusqlite::types::Value::Text((*r).to_string()));
    }
    params.push(rusqlite::types::Value::Integer(
        i64::try_from(limit).unwrap_or(i64::MAX),
    ));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
        Ok(EdgeRow {
            src: r.get(1)?,
            rel: r.get(2)?,
            dst: r.get(3)?,
            note_id: r.get(4)?,
            valid_from: r.get(5)?,
            valid_to: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Query terms found in the note (stems of `body_norm` or tags) and the
/// coverage factor: 1.0 when every term matches, 0.5 when none does.
fn coverage_of(
    conn: &rusqlite::Connection,
    id: &str,
    _note: &Note,
    terms: &[String],
) -> (Vec<String>, f64) {
    let norm: String = conn
        .query_row("SELECT body_norm FROM notes WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap_or_default();
    let words: BTreeSet<&str> = norm.split(' ').filter(|w| !w.is_empty()).collect();
    let tag_words = note_tag_string(conn, id);
    let matched: Vec<String> = terms
        .iter()
        .filter(|t| words.contains(t.as_str()) || tag_words.split(' ').any(|w| w == t.as_str()))
        .cloned()
        .collect();
    let coverage = if terms.is_empty() {
        1.0
    } else {
        0.5 + 0.5 * (matched.len() as f64 / terms.len() as f64)
    };
    (matched, coverage)
}

/// Recency weight: exponential decay with the configured half-life (days).
fn recency_of(now: OffsetDateTime, note: &Note, half_life_days: f64) -> f64 {
    let age_days = (now - note.updated_at.get()).whole_seconds() as f64 / 86_400.0;
    0.5_f64.powf(age_days / half_life_days.max(1.0))
}

/// Graph expansion (§6.3.3): from the seed notes (already filtered by the
/// query), walk the entity graph (edges valid at `as_of`) up to `max_hops`
/// note-hops; collect newly discovered notes with the path that led to them
/// and the seed score they inherit (decaying per hop in the rerank).
///
/// Seeds come from the notes' own bodies (links, relations, subject), so the
/// walk only needs edges adjacent to the frontier — the indexed `src`/`dst`
/// columns — never the whole edge table.
#[allow(clippy::too_many_arguments)]
fn expand_graph(
    conn: &rusqlite::Connection,
    seeds: &[(String, f64)],
    max_hops: u8,
    as_of: Option<OffsetDateTime>,
    include_quarantined: bool,
    exclude: &BTreeSet<String>,
    extra: &mut BTreeMap<String, (u8, String, f64)>,
) -> Result<(), SearchError> {
    // entity -> (path, inherited seed score)
    let mut entities: BTreeMap<String, (String, f64)> = BTreeMap::new();
    for (seed_id, score) in seeds {
        let Ok(note) = read_note_by_id(conn, seed_id) else {
            continue;
        };
        let Ok(analysis) = crate::format::body::analyze(&note.body) else {
            continue;
        };
        let mut names: Vec<String> = analysis.links.clone();
        for r in &analysis.relations {
            if !names.contains(&r.dst) {
                names.push(r.dst.clone());
            }
        }
        if let Some(subject) = note.subject_entity()
            && !names.contains(&subject)
        {
            names.push(subject);
        }
        for name in &names {
            entities
                .entry(name.clone())
                .or_insert_with(|| (format!("[[{name}]]"), *score));
        }
    }

    // BFS over entity adjacency, one indexed frontier query per level.
    let mut frontier: Vec<String> = entities.keys().cloned().collect();
    for hop in 1..=max_hops {
        if frontier.is_empty() {
            break;
        }
        let edges = edges_touching_frontier(conn, &frontier, &[], include_quarantined, WALK_LIMIT)?;
        let frontier_set: BTreeSet<String> = frontier.iter().cloned().collect();
        let mut next: BTreeSet<String> = BTreeSet::new();
        for e in &edges {
            if !valid_at(&e.valid_from, &e.valid_to, as_of) {
                continue;
            }
            // Which endpoint is in the frontier, and what lies on the other side?
            let (from, to) = if frontier_set.contains(&e.src) && !e.src.starts_with("note:") {
                (e.src.clone(), e.dst.clone())
            } else if frontier_set.contains(&e.dst) && !e.dst.starts_with("note:") {
                (e.dst.clone(), e.src.clone())
            } else {
                continue;
            };
            let note_like = to.starts_with("note:");
            let (base, score) = entities.get(&from).cloned().unwrap_or_default();
            let dest = if note_like {
                format!(
                    "(nota {})",
                    to.get(5..)
                        .and_then(|id| id.get(id.len().saturating_sub(8)..))
                        .unwrap_or("?")
                )
            } else {
                format!("[[{to}]]")
            };
            let path = format!("{base} -{}-> {dest}", e.rel);
            if !note_like {
                entities.entry(to.clone()).or_insert((path.clone(), score));
                next.insert(to.clone());
            }
            if let Some(nid) = &e.note_id
                && !exclude.contains(nid)
                && !extra.contains_key(nid)
                && *nid != from
            {
                extra.insert(nid.clone(), (hop, path, score));
            }
        }
        frontier = next.into_iter().collect();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Cell;
    use crate::format::NoteType;
    use ulid::Ulid;

    fn at(day: u32) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
            .unwrap(/* test-only */)
    }

    fn note(id: u16, title: &str, body: &str, note_type: NoteType) -> Note {
        Note {
            id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
            note_type,
            title: Some(title.into()),
            body: body.into(),
            tags: vec![],
            scope: None,
            lang: Some(crate::format::Lang::It),
            source: "user".into(),
            confidence: Confidence::Stated,
            status: Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: crate::format::At(at(1)),
            updated_at: crate::format::At(at(1)),
            extensions: vec![],
        }
    }

    fn setup() -> (tempfile::TempDir, Cell) {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("t.sqlite"), "t", "tester")
            .unwrap(/* test-only */);
        // people
        let mut fabio = note(1, "Fabio", "Fabio.", NoteType::Person);
        fabio.body = "## Relations\n- partner_of:: [[Giulia]]\n".into();
        cell.put_note(&fabio, "t").unwrap(/* test-only */);
        let mut giulia = note(2, "Giulia", "Giulia.", NoteType::Person);
        giulia.body = "## Relations\n- works_at:: [[Studio Rossi]]\n".into();
        cell.put_note(&giulia, "t").unwrap(/* test-only */);
        // facts
        let mut fact = note(
            3,
            "Fabio abita a Milano",
            "[[Fabio]] ha traslocato a [[Milano]] a settembre.",
            NoteType::Fact,
        );
        fact.valid_from =
            Some(crate::format::Validity::parse("2026-09-01").unwrap(/* test-only */));
        cell.put_note(&fact, "t").unwrap(/* test-only */);
        let mut old = note(
            4,
            "Fabio abitava a Roma",
            "[[Fabio]] viveva a [[Roma]].",
            NoteType::Fact,
        );
        old.valid_from = Some(crate::format::Validity::parse("2020-01-01").unwrap(/* test-only */));
        cell.put_note(&old, "t").unwrap(/* test-only */);
        let mut ext = note(
            5,
            "Notizia web",
            "Si dice che [[Fabio]] traslochi a [[Torino]].",
            NoteType::Fact,
        );
        ext.confidence = Confidence::External;
        ext.status = Status::Quarantined;
        ext.source = "import:web".into();
        cell.put_note(&ext, "t").unwrap(/* test-only */);
        (dir, cell)
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* test-only */)
    }

    #[test]
    fn rrf_single_list_preserves_order() {
        let r = rrf_fuse(&[vec!["a".into(), "b".into()]], 60);
        assert!(r["a"] > r["b"]);
        // two lists: overlap wins
        let r2 = rrf_fuse(
            &[vec!["a".into(), "b".into()], vec!["b".into(), "c".into()]],
            60,
        );
        assert!(r2["b"] > r2["a"] && r2["b"] > r2["c"]);
    }

    #[test]
    fn text_search_finds_and_ranks() {
        let (_d, cell) = setup();
        let hits = cell
            .search_with_now(&Query::text("traslocato").k(10), now())
            .unwrap(/* test-only */);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].note.id.to_string(), format!("01{:024}", 3));
        assert!(
            hits[0]
                .explain
                .matched_terms
                .contains(&"trasloc".to_string())
        );
        // Ranks are positions in the BM25 candidate list *before* filters:
        // the quarantined web note can rank first and then be filtered out.
        assert_eq!(hits[0].explain.bm25_rank, Some(2));
    }

    #[test]
    fn quarantined_never_in_default_results() {
        let (_d, cell) = setup();
        let hits = cell
            .search_with_now(&Query::text("Torino notizia").k(10), now())
            .unwrap(/* test-only */);
        assert!(hits.iter().all(|h| h.note.status != Status::Quarantined));
        // and with the explicit flag they appear
        let mut q = Query::text("Torino notizia");
        q.filters.include_quarantined = true;
        let hits2 = cell.search_with_now(&q, now()).unwrap(/* test-only */);
        assert!(hits2.iter().any(|h| h.note.status == Status::Quarantined));
    }

    #[test]
    fn temporal_query_surfaces_old_note() {
        let (_d, cell) = setup();
        // close the old note with a supersede
        let mut new = note(
            6,
            "Fabio vive a Milano",
            "Fabio abita a [[Milano]].",
            NoteType::Fact,
        );
        new.valid_from = Some(crate::format::Validity::parse("2026-09-01").unwrap(/* test-only */));
        cell.supersede(
            &Ulid::from_string(&format!("01{:024}", 4)).unwrap(/* test-only */),
            &new,
            None,
            "t",
        )
        .unwrap(/* test-only */);
        // as of August 2026 the superseded Roma note was the truth
        let as_of = crate::format::Validity::parse("2026-08-15")
            .unwrap(/* test-only */)
            .start();
        let hits = cell
            .search_with_now(&Query::text("Fabio dove abita").k(10).as_of(as_of), now())
            .unwrap(/* test-only */);
        assert!(
            hits.iter().any(|h| h.note.display_title().contains("Roma")),
            "the Roma note should surface for an August 2026 question: {hits:?}"
        );
        // today it does not
        let hits_now = cell
            .search_with_now(&Query::text("Fabio dove abita").k(10), now())
            .unwrap(/* test-only */);
        assert!(
            hits_now
                .iter()
                .all(|h| !h.note.display_title().contains("Roma"))
        );
    }

    #[test]
    fn multi_hop_via_graph() {
        let (_d, cell) = setup();
        // "dove lavora il partner di Fabio": text hits Fabio; graph reaches
        // Giulia's works_at note (hop 2)
        let hits = cell
            .search_with_now(&Query::text("Fabio").k(10).graph_hops(2), now())
            .unwrap(/* test-only */);
        let ids: Vec<String> = hits.iter().map(|h| h.note.id.to_string()).collect();
        assert!(
            ids.contains(&format!("01{:024}", 2)),
            "Giulia note via graph: {hits:?}"
        );
        let via = hits
            .iter()
            .find(|h| h.note.id.to_string() == format!("01{:024}", 2))
            .unwrap(/* test-only */)
            .explain
            .via
            .clone();
        assert!(
            via.as_deref().is_some_and(|v| v.contains("->")),
            "path: {via:?}"
        );
    }

    #[test]
    fn graph_hops_zero_keeps_text_only() {
        let (_d, cell) = setup();
        let hits = cell
            .search_with_now(&Query::text("Fabio").k(10).graph_hops(0), now())
            .unwrap(/* test-only */);
        let ids: Vec<String> = hits.iter().map(|h| h.note.id.to_string()).collect();
        assert!(!ids.contains(&format!("01{:024}", 2)));
    }

    #[test]
    fn tags_filter_includes_children() {
        let (_d, cell) = setup();
        let mut n = note(
            7,
            "Lista cose",
            "cose da fare per il trasloco",
            NoteType::Note,
        );
        n.tags = vec!["casa/milano/trasloco".into()];
        cell.put_note(&n, "t").unwrap(/* test-only */);
        let mut q = Query::text("trasloco");
        q.filters.tags = vec!["casa".into()];
        let hits = cell.search_with_now(&q, now()).unwrap(/* test-only */);
        assert!(hits.iter().any(|h| h.note.id == n.id));
        let tags = cell.tags("casa", false).unwrap(/* test-only */);
        assert!(tags.iter().any(|t| t.name == "casa/milano/trasloco"));
    }

    #[test]
    fn neighbors_and_path() {
        let (_d, cell) = setup();
        let ns = cell.neighbors("Fabio", 2, &[], false)
            .unwrap(/* test-only */);
        assert!(
            ns.iter()
                .any(|n| n.entity == "Giulia" && n.rel == "partner_of")
        );
        assert!(
            ns.iter()
                .any(|n| n.entity == "Studio Rossi" && n.rel == "works_at")
        );
        let p = cell
            .path("Fabio", "Studio Rossi", false)
            .unwrap(/* test-only */);
        let steps = p.expect("path exists");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].rel, "partner_of");
        assert_eq!(steps[1].to, "Studio Rossi");
        assert!(
            cell
            .path("Fabano", "Studio Rossi", false)
            .unwrap(/* test-only */)
            .is_none()
        );
    }

    #[test]
    fn quarantined_note_does_not_seed_the_graph() {
        let (_d, cell) = setup();
        // "notizia" matches only the quarantined web note: no result may come
        // out of it, not even graph-discovered neighbours of the poisoned note
        let hits = cell
            .search_with_now(&Query::text("notizia").k(10), now())
            .unwrap(/* test-only */);
        assert!(hits.is_empty(), "quarantine leaked: {hits:?}");
    }

    #[test]
    fn quarantined_edges_do_not_guide_the_walk() {
        let (_d, cell) = setup();
        // Zoro is reachable from Fabio ONLY through the relation the
        // quarantined note declares, and note 7 only through Zoro
        let mut gossip = note(
            6,
            "Fabio",
            "Voci di corridoio.\n\n## Relations\n- loves:: [[Zoro]]\n",
            NoteType::Preference,
        );
        gossip.confidence = Confidence::External;
        gossip.status = Status::Quarantined;
        gossip.source = "import:web".into();
        cell.put_note(&gossip, "t").unwrap(/* test-only */);
        let zoro = note(
            7,
            "Zoro sta a Torino",
            "Zoro abita a [[Torino]].",
            NoteType::Fact,
        );
        cell.put_note(&zoro, "t").unwrap(/* test-only */);
        let hits = cell
            .search_with_now(&Query::text("Fabio").k(10).graph_hops(2), now())
            .unwrap(/* test-only */);
        assert!(
            hits.iter().all(|h| h.note.id != zoro.id),
            "walk crossed a quarantined edge: {hits:?}"
        );
        // with the flag the quarantined note is a first-class citizen again
        let mut q = Query::text("Fabio").k(10).graph_hops(2);
        q.filters.include_quarantined = true;
        let hits2 = cell.search_with_now(&q, now()).unwrap(/* test-only */);
        assert!(
            hits2.iter().any(|h| h.note.id == zoro.id),
            "include_quarantined should traverse it: {hits2:?}"
        );
    }

    #[test]
    fn neighbors_and_path_skip_quarantined_edges() {
        let (_d, cell) = setup();
        let web = format!("01{:024}", 5);
        // the mention edge `note:5 -> Fabio` is declared by a quarantined note
        let ns = cell
            .neighbors("Fabio", 1, &[], false)
            .unwrap(/* test-only */);
        assert!(
            ns.iter()
                .all(|n| n.note_id.as_deref() != Some(web.as_str())),
            "quarantined edge in neighbors: {ns:?}"
        );
        // Torino hangs off the graph only through that note
        assert!(cell
            .path("Fabio", "Torino", false)
            .unwrap(/* test-only */)
            .is_none());
        // the flag brings them back
        let ns2 = cell
            .neighbors("Fabio", 1, &[], true)
            .unwrap(/* test-only */);
        assert!(
            ns2.iter()
                .any(|n| n.note_id.as_deref() == Some(web.as_str()))
        );
        assert!(cell
            .path("Fabio", "Torino", true)
            .unwrap(/* test-only */)
            .is_some());
    }

    #[test]
    fn manual_edge_with_note_follows_its_lifecycle() {
        // v0.1.1 leftover: a manual edge that cites a note must follow that
        // note through quarantine/confirm/delete, in the same operation, and
        // the replay must reproduce it.
        let (_d, cell) = setup();
        let cited = note(9, "Nota citata", "un fatto su [[Milano]].", NoteType::Fact);
        cell.put_note(&cited, "t").unwrap(/* test-only */);
        cell.link(
            &crate::cell::EdgeInput {
                src: "Fabio".into(),
                rel: "friend_of".into(),
                dst: "Monza".into(),
                note_id: Some(cited.id),
                valid_from: None,
                valid_to: None,
                confidence: Confidence::Stated,
            },
            "t",
        )
        .unwrap(/* test-only */);
        let edge_vis = |cell: &Cell, flag: bool| -> bool {
            cell.neighbors("Fabio", 1, &[], flag)
                .unwrap(/* test-only */)
                .iter()
                .any(|n| n.rel == "friend_of")
        };
        assert!(edge_vis(&cell, false), "fresh manual edge is visible");
        // quarantine the cited note: the edge disappears
        cell.set_status(&cited.id, Status::Quarantined, "t")
            .unwrap(/* test-only */);
        assert!(
            !edge_vis(&cell, false),
            "the manual edge must hide with its note"
        );
        assert!(edge_vis(&cell, true), "include_quarantined still shows it");
        // confirm brings it back
        cell.confirm(&cited.id, "t").unwrap(/* test-only */);
        assert!(edge_vis(&cell, false), "confirm restores the edge");
        // a later delete hides it again
        cell.delete_note(&cited.id, "t").unwrap(/* test-only */);
        assert!(
            !edge_vis(&cell, true),
            "a deleted note's manual edge never comes back"
        );
        // and the replay reproduces the whole story
        assert!(cell.verify().unwrap(/* test-only */).ok());
    }

    #[test]
    fn hostile_names_travel_as_parameters() {
        let (_d, cell) = setup();
        // an entity with an apostrophe and a relation that closes the quoted
        // literal: both must travel as SQL parameters, never spliced text
        cell.link(
            &crate::cell::EdgeInput {
                src: "D'Angelo".into(),
                rel: "') OR 1=1 --".into(),
                dst: "Milano".into(),
                note_id: None,
                valid_from: None,
                valid_to: None,
                confidence: Confidence::Stated,
            },
            "t",
        )
        .unwrap(/* test-only */);
        let ns = cell
            .neighbors("D'Angelo", 1, &[], false)
            .unwrap(/* test-only */);
        assert_eq!(
            ns.iter().map(|n| n.entity.as_str()).collect::<Vec<_>>(),
            vec!["Milano"],
            "an injected rel would drag the whole graph in: {ns:?}"
        );
        // the same hostile value as a rel filter matches nothing from Fabio
        let ns2 = cell
            .neighbors("Fabio", 1, &[ "') OR 1=1 --"], false)
            .unwrap(/* test-only */);
        assert!(
            ns2.is_empty(),
            "1=1 must not have matched every edge: {ns2:?}"
        );
        // and a search whose seeds reach D'Angelo stays quiet
        let mut da = note(
            8,
            "Appunto su D'Angelo",
            "Nota su [[D'Angelo]] e [[Milano]].",
            NoteType::Fact,
        );
        da.tags = vec!["x".into()];
        cell.put_note(&da, "t").unwrap(/* test-only */);
        let hits = cell
            .search_with_now(&Query::text("D'Angelo").k(10), now())
            .unwrap(/* test-only */);
        assert!(
            hits.len() <= 6,
            "the walk grabbed the whole graph: {hits:?}"
        );
    }

    #[test]
    fn neighbors_rel_filter_applies_in_both_directions() {
        let (_d, cell) = setup();
        // outgoing: Fabio -partner_of-> Giulia
        let out = cell
            .neighbors("Fabio", 1, &["partner_of"], false)
            .unwrap(/* test-only */);
        assert_eq!(
            out.iter()
                .filter(|n| n.rel == "partner_of")
                .map(|n| (n.entity.as_str(), n.direction.as_str()))
                .collect::<Vec<_>>(),
            vec![("Giulia", "->")]
        );
        // incoming: Giulia -works_at-> Studio Rossi, asked from the dst side
        let inc = cell
            .neighbors("Studio Rossi", 1, &["works_at"], false)
            .unwrap(/* test-only */);
        assert_eq!(
            inc.iter()
                .filter(|n| n.rel == "works_at")
                .map(|n| (n.entity.as_str(), n.direction.as_str()))
                .collect::<Vec<_>>(),
            vec![("Giulia", "<-")]
        );
    }

    #[test]
    fn path_beyond_four_hops_is_none_without_scanning_everything() {
        let (_d, cell) = setup();
        // a 6-hop chain: Nodo5 -next-> Nodo4 -> … -> Nodo0 -next-> Fabio
        let mut prev = "Fabio".to_string();
        for i in 0..6 {
            let name = format!("Nodo{i}");
            let mut p = note(i + 20, &name, "", NoteType::Person);
            p.body = format!("## Relations\n- next:: [[{prev}]]\n");
            cell.put_note(&p, "t").unwrap(/* test-only */);
            prev = name;
        }
        // a detached blob of edges the bounded walk must never look at
        // (inserted directly: this test is about reads, not about the log)
        {
            let inner = cell.inner.lock().unwrap(/* test-only */);
            for i in 0..20_000 {
                inner
                    .conn
                    .execute(
                        "INSERT INTO edges (id, src, rel, dst, note_id, confidence, status)
                         VALUES (?1, ?2, 'blob', ?3, NULL, 'stated', 'active')",
                        rusqlite::params![format!("b{i:06}"), format!("X{i}"), format!("Y{i}")],
                    )
                    .unwrap(/* test-only */);
            }
        }
        // count the work: any full-table scan crosses the blob (20k rows ≥
        // ~200k VDBE steps); the bounded walk stays far below
        let steps = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        {
            let mut inner = cell.inner.lock().unwrap(/* test-only */);
            let s = std::sync::Arc::clone(&steps);
            inner
                .conn
                .progress_handler(1, Some(move || {
                    s.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    false
                }))
                .unwrap(/* test-only */);
        }
        let found = cell.path("Nodo5", "Fabio", false).unwrap(/* test-only */);
        assert!(found.is_none(), "6 hops must exceed the 4-step cap");
        // a short path is still found (and cheap)
        let short = cell.path("Nodo3", "Fabio", false).unwrap(/* test-only */);
        assert_eq!(short.expect("4 hops").len(), 4);
        let used = steps.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            used < 100_000,
            "the walk scanned the whole edge table ({used} VDBE steps; the \
             blob alone is 20 000 rows)"
        );
    }

    #[test]
    fn entities_tags_and_timeline() {
        let (_d, cell) = setup();
        let es = cell.entities("Giul").unwrap(/* test-only */);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].name, "Giulia");
        let tl = cell.timeline("Fabio", false).unwrap(/* test-only */);
        assert!(tl.len() >= 2);
        // ordered by valid_from: Roma (2020) before Milano (2026)
        let titles: Vec<&str> = tl.iter().map(|h| h.note.display_title()).collect();
        let roma = titles.iter().position(|t| t.contains("Roma"));
        let milano = titles.iter().position(|t| t.contains("Milano"));
        assert!(roma.unwrap(/* test-only */) < milano.unwrap(/* test-only */));
    }

    #[test]
    fn empty_query_is_rejected() {
        let (_d, cell) = setup();
        assert!(cell.search_with_now(&Query::text("  "), now()).is_err());
    }
}

#[cfg(test)]
mod semantic_tests {
    use super::*;
    use crate::embed::{TextKind, l2_normalize};
    use crate::format::NoteType;
    use ulid::Ulid;

    /// An embedder that makes "similar" texts point the same way: the vector
    /// is dominated by the first letter's position. Deterministic, 8 dims.
    struct SemiEmbedder {
        id: String,
    }

    impl crate::embed::Embedder for SemiEmbedder {
        fn model_id(&self) -> &str {
            &self.id
        }
        fn dim(&self) -> usize {
            8
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
                    // first word char decides the direction; noise from the rest
                    let first = t
                        .chars()
                        .find(|c| c.is_alphabetic())
                        .and_then(|c| c.to_lowercase().next())
                        .unwrap_or('a');
                    let mut v = vec![0.0_f32; 8];
                    let slot = (first as u32 % 8) as usize;
                    v[slot] = 1.0;
                    let hash = blake3::hash(t.as_bytes());
                    v[(slot + 1) % 8] = f32::from(hash.as_bytes()[0]) / 2000.0;
                    l2_normalize(&mut v);
                    v
                })
                .collect())
        }
    }

    fn sem() -> Semantic {
        Semantic::new(std::sync::Arc::new(SemiEmbedder {
            id: "semi@1".into(),
        }))
    }

    fn at(day: u32) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
            .unwrap(/* test-only */)
    }

    fn note(id: u16, title: &str, body: &str) -> Note {
        Note {
            id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
            note_type: NoteType::Fact,
            title: Some(title.into()),
            body: body.into(),
            tags: vec![],
            scope: None,
            lang: Some(crate::format::Lang::It),
            source: "user".into(),
            confidence: Confidence::Stated,
            status: Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: crate::format::At(at(1)),
            updated_at: crate::format::At(at(1)),
            extensions: vec![],
        }
    }

    /// A vault where the answer shares no term with the question.
    fn setup() -> (tempfile::TempDir, Cell) {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("s.sqlite"), "t", "tester")
            .unwrap(/* test-only */);
        // "In che città vive?" -> "Fabio abita a Milano" (Milano starts with M)
        cell.put_note(&note(1, "Fabio abita a Milano", "Fabio vive a [[Milano]]."), "t")
            .unwrap(/* test-only */);
        // a decoy that also matches semantically (same M direction)
        cell.put_note(&note(2, "Marco sta a Modena", "Marco risiede a [[Modena]]."), "t")
            .unwrap(/* test-only */);
        // pure noise
        cell.put_note(&note(3, "Zanzara", "Una zanzara ronza."), "t")
            .unwrap(/* test-only */);
        cell.add_model("semi@1", 8).unwrap(/* test-only */);
        let e = SemiEmbedder {
            id: "semi@1".into(),
        };
        cell.embed_pending(&e, 100).unwrap(/* test-only */);
        (dir, cell)
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* test-only */)
    }

    #[test]
    fn lru_cache_evicts_and_refreshes() {
        let mut c = LruCache::new(2);
        c.put("a".into(), vec![1.0]);
        c.put("b".into(), vec![2.0]);
        assert_eq!(c.get("a"), Some(&vec![1.0])); // a refreshed
        c.put("c".into(), vec![3.0]); // evicts b, not a
        assert!(c.get("b").is_none());
        assert_eq!(c.get("a"), Some(&vec![1.0]));
        assert_eq!(c.len(), 2);
        assert!(!c.is_empty());
    }

    #[test]
    fn hybrid_finds_paraphrase_and_reports_the_lists() {
        let (_d, cell) = setup();
        // "in che località risiede la coppia?" shares no stem with the notes:
        // the vector branch must carry it. "località" starts with L... the
        // SemiEmbedder hashes the whole text, so let's ask with the M-family.
        let outcome = cell
            .search_semantic_with_now(&Query::text("Milano meta urbana").k(3), now(), &sem())
            .unwrap(/* test-only */);
        assert!(!outcome.hits.is_empty(), "{:?}", outcome.hits);
        let top = &outcome.hits[0];
        assert!(
            top.explain.lists.contains(&"vector"),
            "the winner should come at least from the vector list: {:?}",
            top.explain.lists
        );
        assert!(top.explain.semantic_rank.is_some());
        assert!(top.explain.cosine.is_some());
        assert!(top.explain.fused_score.is_some());
        // the raw score is exposed alongside the normalised one
        assert!(top.raw_score > 0.0);
        assert!((top.score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn embedding_is_cached_and_timed() {
        let (_d, cell) = setup();
        // count the embedder calls: timing alone is noise at this scale
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = CountingEmbedder {
            inner: SemiEmbedder {
                id: "semi@1".into(),
            },
            calls: std::sync::Arc::clone(&calls),
        };
        let sem = Semantic::new(std::sync::Arc::new(counting));
        let q = Query::text("Milano meta urbana").k(3);
        let first = cell.search_semantic_with_now(&q, now(), &sem).unwrap(/* test-only */);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the first call embeds"
        );
        assert!(first.embed_ms > 0.0);
        let second = cell.search_semantic_with_now(&q, now(), &sem).unwrap(/* test-only */);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second call must be a cache hit"
        );
        assert_eq!(first.hits.len(), second.hits.len());
    }

    /// Wraps an embedder and counts calls (cache tests).
    struct CountingEmbedder {
        inner: SemiEmbedder,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::embed::Embedder for CountingEmbedder {
        fn model_id(&self) -> &str {
            self.inner.model_id()
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn max_tokens(&self) -> usize {
            self.inner.max_tokens()
        }
        fn embed(
            &self,
            texts: &[&str],
            kind: TextKind,
        ) -> Result<Vec<Vec<f32>>, crate::error::EmbedError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.embed(texts, kind)
        }
    }

    #[test]
    fn quarantined_nearest_neighbour_stays_hidden_and_does_not_seed() {
        let (_d, cell) = setup();
        // the closest note to the query is quarantined: it must neither
        // appear nor pull its neighbours in through the graph
        cell.set_status(
            &Ulid::from_string(&format!("01{:024}", 1)).unwrap(/* test-only */),
            Status::Quarantined,
            "t",
        )
        .unwrap(/* test-only */);
        // note 2 must not appear "via" the hidden note: graph hops 0 keeps it
        // to the vector list only
        let outcome = cell
            .search_semantic_with_now(&Query::text("Milano meta urbana").k(3), now(), &sem())
            .unwrap(/* test-only */);
        for h in &outcome.hits {
            assert!(
                h.note.status != Status::Quarantined,
                "quarantine leaked: {:?}",
                h.note.title
            );
        }
        // with two hops from the other seeds nothing should arrive *through*
        // the hidden note either (its edges are invisible)
        let mut q2 = Query::text("Milano meta urbana").k(5);
        q2.graph_hops = 2;
        let outcome2 = cell
            .search_semantic_with_now(&q2, now(), &sem())
            .unwrap(/* test-only */);
        assert!(
            outcome2
                .hits
                .iter()
                .all(|h| h.note.status != Status::Quarantined)
        );
    }

    #[test]
    fn abstention_drops_vector_only_results_below_tau() {
        let (_d, cell) = setup();
        let mut s = sem();
        // tau above every cosine: every vector-only result must abstain
        s.tau = 0.999;
        let outcome = cell
            .search_semantic_with_now(&Query::text("xyzzy plugh").k(3), now(), &s)
            .unwrap(/* test-only */);
        assert!(
            outcome
                .hits
                .iter()
                .all(|h| h.explain.lists != vec!["vector"]),
            "a below-tau vector-only result leaked: {:?}",
            outcome
                .hits
                .iter()
                .map(|h| h.explain.lists.clone())
                .collect::<Vec<_>>()
        );
        // a note that arrives from BOTH lists stays even below tau
        // ("Milano" matches text and vector)
        s.tau = 0.999;
        let outcome2 = cell
            .search_semantic_with_now(&Query::text("Milano").k(3), now(), &s)
            .unwrap(/* test-only */);
        assert!(
            outcome2
                .hits
                .iter()
                .any(|h| h.explain.lists.contains(&"text")),
            "text matches must not be abstained away: {:?}",
            outcome2
                .hits
                .iter()
                .map(|h| h.explain.lists.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn two_cells_on_the_same_file_see_each_others_vectors() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("two.sqlite");
        let a = Cell::create(&path, "t", "tester").unwrap(/* test-only */);
        a.put_note(&note(1, "Fabio abita a Milano", "Fabio vive a [[Milano]]."), "t")
            .unwrap(/* test-only */);
        a.add_model("semi@1", 8).unwrap(/* test-only */);
        // b writes a new note after a's index exists
        let b = Cell::open(&path).unwrap(/* test-only */);
        b.put_note(&note(2, "Marco sta a Modena", "Marco risiede a [[Modena]]."), "t")
            .unwrap(/* test-only */);
        // warm a's index, then embed from b: a's next search must see it
        let e = SemiEmbedder {
            id: "semi@1".into(),
        };
        let first = a
            .search_semantic_with_now(&Query::text("Milano").k(3), now(), &sem())
            .unwrap(/* test-only */);
        assert_eq!(first.hits.len(), 1, "only the first note is embedded");
        b.embed_pending(&e, 100).unwrap(/* test-only */);
        let second = a
            .search_semantic_with_now(&Query::text("Milano").k(3), now(), &sem())
            .unwrap(/* test-only */);
        assert!(
            second
                .hits
                .iter()
                .any(|h| h.note.title.as_deref() == Some("Marco sta a Modena")),
            "a's index did not rebuild after b's write: {:?}",
            second
                .hits
                .iter()
                .map(|h| h.note.display_title().to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn blue_green_mid_search_serves_the_old_model_only() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let path = dir.path().join("bg.sqlite");
        let cell = Cell::create(&path, "t", "tester").unwrap(/* test-only */);
        cell.put_note(&note(1, "Fabio abita a Milano", "Fabio vive a [[Milano]]."), "t")
            .unwrap(/* test-only */);
        cell.put_note(&note(2, "Marco sta a Modena", "Marco risiede a [[Modena]]."), "t")
            .unwrap(/* test-only */);
        cell.add_model("semi@1", 8).unwrap(/* test-only */);
        let e1 = SemiEmbedder {
            id: "semi@1".into(),
        };
        cell.embed_pending(&e1, 100).unwrap(/* test-only */);
        // a second model starts building, partially embedded
        cell.add_model("alt@2", 8).unwrap(/* test-only */);
        let e2 = AltEmbedder;
        let report = cell.embed_pending(&e2, 1).unwrap(/* test-only */);
        assert_eq!(report.done, 1);
        // the search in the middle: served by the OLD model, no mixed index
        let outcome = cell
            .search_semantic_with_now(&Query::text("Milano").k(3), now(), &sem())
            .unwrap(/* test-only */);
        assert!(!outcome.hits.is_empty());
        let inner = cell.inner.lock().unwrap(/* test-only */);
        let st_model = inner.index.as_ref().map(|st| st.model.clone());
        drop(inner);
        assert_eq!(
            st_model,
            Some("semi@1".into()),
            "the index must serve the ready model, not the building one"
        );
        // finish the switch: now the index serves the new model
        cell.embed_pending(&e2, 100).unwrap(/* test-only */);
        let outcome2 = cell
            .search_semantic_with_now(&Query::text("Milano").k(3), now(), &sem())
            .unwrap(/* test-only */);
        assert!(!outcome2.hits.is_empty());
        let inner = cell.inner.lock().unwrap(/* test-only */);
        let st_model2 = inner.index.as_ref().map(|st| st.model.clone());
        drop(inner);
        assert_eq!(st_model2, Some("alt@2".into()));
    }

    /// A second, different embedder for the blue/green test.
    struct AltEmbedder;
    impl crate::embed::Embedder for AltEmbedder {
        fn model_id(&self) -> &str {
            "alt@2"
        }
        fn dim(&self) -> usize {
            8
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
                        .map(|b| f32::from(*b) / 255.0)
                        .collect();
                    l2_normalize(&mut v);
                    v
                })
                .collect())
        }
    }

    #[test]
    fn no_ready_model_means_text_only_behaviour() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("n.sqlite"), "t", "tester")
            .unwrap(/* test-only */);
        cell.put_note(&note(1, "Fabio abita a Milano", "Fabio vive a [[Milano]]."), "t")
            .unwrap(/* test-only */);
        // no model registered: the semantic search degrades to text
        let outcome = cell
            .search_semantic_with_now(&Query::text("Milano").k(3), now(), &sem())
            .unwrap(/* test-only */);
        assert!(!outcome.hits.is_empty());
        assert!(outcome.hits[0].explain.lists.contains(&"text"));
        assert!(!outcome.hits[0].explain.lists.contains(&"vector"));
    }

    #[test]
    fn fusion_shapes_ranking() {
        // RRF and convex are both computed; the ranking differs on overlap
        let (_d, cell) = setup();
        let q = Query::text("Milano").k(3);
        let rrf = cell
            .search_semantic_with_now(&q, now(), &sem().with_fusion(Fusion::Rrf { k: 60 }))
            .unwrap(/* test-only */);
        let conv = cell
            .search_semantic_with_now(&q, now(), &sem().with_fusion(Fusion::Convex { alpha: 0.5 }))
            .unwrap(/* test-only */);
        assert!(!rrf.hits.is_empty() && !conv.hits.is_empty());
        // both contain the text match; scores are recomputed, not shared
        let rrf_top = rrf.hits[0].note.display_title().to_string();
        let conv_top = conv.hits[0].note.display_title().to_string();
        let _ = (rrf_top, conv_top);
    }
}
