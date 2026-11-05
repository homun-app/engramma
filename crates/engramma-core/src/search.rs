// SPDX-License-Identifier: Apache-2.0

//! Search: BM25 candidates (FTS5) + graph expansion + filters + weighted
//! rerank, with an explanation per result (DESIGN §6.3, v0.1 without vectors),
//! and the other queries of §6.4.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use time::OffsetDateTime;

use crate::cell::Cell;
use crate::error::SearchError;
use crate::format::{Confidence, Note, NoteType, Status};
use crate::text;

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

/// Why a result is in the list (§6.3.6).
#[derive(Debug, Clone, PartialEq)]
pub struct Explain {
    /// Query terms found in the note (stemmed index terms).
    pub matched_terms: Vec<String>,
    /// Position in the BM25 candidate list (1-based).
    pub bm25_rank: Option<usize>,
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

/// One search result.
#[derive(Debug, Clone)]
pub struct Hit {
    /// The note.
    pub note: Note,
    /// Final score.
    pub score: f64,
    /// Why it matched.
    pub explain: Explain,
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

        // 2. RRF fusion over the single ranked list (the function exists and
        // is tested for v0.2; with one list the rerank below uses bm25_norm).
        let _rrf = rrf_fuse(
            &[text_hits.iter().map(|(id, _)| id.clone()).collect()],
            60,
        );

        // Normalised seed scores for the graph expansion (§6.3.3: the added
        // notes keep a weight that decays per hop from the seed they came
        // from).
        let max_bm25 = text_hits
            .iter()
            .map(|(_, b)| *b)
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON);
        let seeds: Vec<(String, f64)> = text_hits
            .iter()
            .take(10)
            .map(|(id, b)| (id.clone(), *b / max_bm25))
            .collect();

        // 3. Graph expansion from the top text hits.
        let as_of = q.filters.as_of;
        let mut extra: BTreeMap<String, (u8, String, f64)> = BTreeMap::new(); // (hops, path, seed score)
        if q.graph_hops > 0 && !text_hits.is_empty() {
            expand_graph(
                conn,
                &seeds,
                q.graph_hops,
                as_of,
                &text_hits.iter().map(|(i, _)| i.clone()).collect(),
                &mut extra,
            )?;
        }

        // 4. Collect note rows for all candidates and apply filters.
        let mut candidates: Vec<(String, Option<usize>, Option<(u8, String, f64)>)> = Vec::new();

        for (rank, (id, _)) in text_hits.iter().enumerate() {
            candidates.push((id.clone(), Some(rank + 1), None));
        }
        // Cap the expansion: the nearest graph notes only.
        let mut extras: Vec<(String, (u8, String, f64))> = extra.into_iter().take(40).collect();
        extras.sort_by_key(|(_, (hops, _, _))| *hops);
        for (id, (hops, path, seed)) in &extras {
            candidates.push((id.clone(), None, Some((*hops, path.clone(), *seed))));
        }
        let mut hits: Vec<Hit> = Vec::new();
        for (id, bm25_rank, graph) in candidates {
            let Ok(note) = read_note_by_id(conn, &id) else {
                continue;
            };
            if !passes_filters(&note, &q.filters, now) {
                continue;
            }
            // Raw score: RRF base (graph notes continue the ranking after the
            // text list), normalised once at the end over all candidates.
            // §13.4 rerank: bm25_norm × confidence × recency × graph. With a
            // single candidate list (v0.1) the base is the normalised BM25
            // score — RRF (computed for the fusion) arrives with the semantic
            // list in v0.2. Graph-discovered notes keep a decaying share of
            // the seed's score (with a floor for pure multi-hop answers).
            // §13.4 rerank: bm25_norm × coverage × confidence × recency ×
            // graph. With a single candidate list (v0.1) the base is the
            // normalised BM25 score — RRF (computed for the fusion) arrives
            // with the semantic list in v0.2. Graph-discovered notes keep a
            // decaying share of the seed's score (with a floor for pure
            // multi-hop answers); the coverage factor halves notes that share
            // none of the query terms, so pure graph neighbours support —
            // never replace — real text matches.
            // body_norm already holds the stemmed title+body (computed at
            // write time): reuse it instead of re-stemming per candidate.
            let norm: String = conn
                .query_row(
                    "SELECT body_norm FROM notes WHERE id = ?1",
                    [&id],
                    |r| r.get(0),
                )
                .unwrap_or_default();
            let words: BTreeSet<&str> = norm.split(' ').filter(|w| !w.is_empty()).collect();
            let tag_words = note_tag_string(conn, &id);
            let matched: Vec<String> = terms
                .iter()
                .filter(|t| {
                    words.contains(t.as_str())
                        || tag_words.split(' ').any(|w| w == t.as_str())
                })
                .cloned()
                .collect();
            let coverage = if terms.is_empty() {
                1.0
            } else {
                0.5 + 0.5 * (matched.len() as f64 / terms.len() as f64)
            };
            let base = match &graph {
                None => text_hits
                    .iter()
                    .find(|(i, _)| i == &id)
                    .map(|(_, b)| b / max_bm25)
                    .unwrap_or(0.0),
                Some((_, _, seed_norm)) => GRAPH_FLOOR.max(*seed_norm),
            };
            let w_graph = match &graph {
                None => 1.0,
                // one extra decay step: no text match is one hop less evidence
                Some((hops, _, _)) => q.weights.hop_decay.powi(i32::from(*hops) + 1),
            };
            let age_days = (now - note.updated_at.get()).whole_seconds() as f64 / 86_400.0;
            let recency = 0.5_f64.powf(age_days / q.weights.recency_half_life_days.max(1.0));
            let raw = base * coverage * q.weights.confidence(note.confidence) * recency * w_graph;
            let explain = Explain {
                matched_terms: matched,
                bm25_rank,
                via: graph.map(|(_, p, _)| p),
                confidence: note.confidence,
                valid_from: note.valid_from.as_ref().map(|v| v.to_db_string()),
                valid_to: note.valid_to.as_ref().map(|v| v.to_db_string()),
                source: note.source.clone(),
            };
            hits.push(Hit {
                note,
                score: raw,
                explain,
            });
        }
        let max_raw = hits.iter().map(|h| h.score).fold(0.0_f64, f64::max);
        if max_raw > 0.0 {
            for h in &mut hits {
                h.score /= max_raw;
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
    /// restricted to some relations. Returns the edges, nearest first.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn neighbors(
        &self,
        start: &str,
        depth: u8,
        rels: &[&str],
    ) -> Result<Vec<Neighbor>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let edges = load_edges(&inner.conn, rels)?;
        let mut out: Vec<Neighbor> = Vec::new();
        let mut seen_entities: BTreeSet<String> = BTreeSet::from([start.to_string()]);
        let mut frontier: Vec<String> = vec![start.to_string()];
        for d in 1..=depth {
            let mut next: Vec<String> = Vec::new();
            for e in &edges {
                if !frontier.contains(&e.src) && !frontier.contains(&e.dst) {
                    continue;
                }
                let (other, direction) = if e.src == *frontier.iter().find(|s| **s == e.src).unwrap_or(&e.dst) && frontier.contains(&e.src)
                {
                    (e.dst.clone(), "->")
                } else {
                    (e.src.clone(), "<-")
                };
                let hop = Neighbor {
                    depth: d,
                    entity: other.clone(),
                    rel: e.rel.clone(),
                    direction: direction.to_string(),
                    note_id: e.note_id.clone(),
                    valid_from: e.valid_from.clone(),
                    valid_to: e.valid_to.clone(),
                };
                if !out.iter().any(|n| n.entity == hop.entity && n.rel == hop.rel && n.depth == hop.depth) {
                    out.push(hop);
                }
                if seen_entities.insert(other.clone()) {
                    next.push(other);
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// Shortest path between two entities, depth ≤ 4.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn path(&self, a: &str, b: &str) -> Result<Option<Vec<PathStep>>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let edges = load_edges(&inner.conn, &[])?;
        // BFS with parent tracking.
        let mut parents: HashMap<String, (String, String)> = HashMap::new(); // node -> (from, rel)
        let mut queue: Vec<String> = vec![a.to_string()];
        let mut visited: BTreeSet<String> = BTreeSet::from([a.to_string()]);
        while let Some(node) = queue.first().cloned() {
            queue.remove(0);
            if node == b {
                break;
            }
            for e in &edges {
                if e.src == node && !visited.contains(&e.dst) {
                    visited.insert(e.dst.clone());
                    parents.insert(e.dst.clone(), (node.clone(), e.rel.clone()));
                    queue.push(e.dst.clone());
                } else if e.dst == node && !visited.contains(&e.src) {
                    visited.insert(e.src.clone());
                    parents.insert(e.src.clone(), (node.clone(), e.rel.clone()));
                    queue.push(e.src.clone());
                }
            }
        }
        if !visited.contains(&b.to_string()) {
            return Ok(None);
        }
        // Reconstruct (depth guard: give up beyond 4 steps).
        let mut steps: Vec<PathStep> = Vec::new();
        let mut cur = b.to_string();
        let mut guard = 0;
        while cur != a.to_string() && guard < 5 {
            let Some((from, rel)) = parents.get(&cur) else {
                return Ok(None);
            };
            steps.push(PathStep {
                from: from.clone(),
                rel: rel.clone(),
                to: cur.clone(),
            });
            cur = from.clone();
            guard += 1;
        }
        if cur != a.to_string() || steps.len() > 4 {
            return Ok(None);
        }
        steps.reverse();
        Ok(Some(steps))
    }

    /// Tags with note counts; a prefix includes its children.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn tags(&self, prefix: &str) -> Result<Vec<TagInfo>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let like = format!("{prefix}%");
        let mut stmt = inner.conn.prepare(
            "SELECT t.name, t.parent, COUNT(nt.note_id)
             FROM tags t LEFT JOIN note_tags nt ON nt.tag = t.name
             WHERE t.name = ?1 OR t.name LIKE ?2
             GROUP BY t.name, t.parent ORDER BY t.name",
        )?;
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

    /// Notes about an entity ordered by validity (its timeline).
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures.
    pub fn timeline(&self, entity: &str) -> Result<Vec<Hit>, SearchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        // notes declaring an edge on the entity, or the entity's own note.
        let mut stmt = inner.conn.prepare(
            "SELECT DISTINCT n.id FROM notes n
             WHERE n.id = (SELECT note_id FROM entities WHERE name = ?1 COLLATE NOCASE)
                OR n.id IN (SELECT note_id FROM edges
                            WHERE note_id IS NOT NULL
                              AND (src = ?1 OR dst = ?1
                                   OR src IN (SELECT 'note:' || note_id FROM entities WHERE name = ?1 COLLATE NOCASE)))
             ORDER BY n.valid_from IS NULL, n.valid_from, n.id",
        )?;
        let rows = stmt.query_map([entity], |r| r.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        drop(stmt);
        let mut out = Vec::new();
        for id in ids {
            if let Ok(note) = read_note_by_id(&inner.conn, &id) {
                if note.status == Status::Deleted {
                    continue;
                }
                out.push(Hit {
                    score: 0.0,
                    note,
                    explain: Explain {
                        matched_terms: vec![],
                        bm25_rank: None,
                        via: Some(format!("[[{entity}]]")),
                        confidence: Confidence::Stated,
                        valid_from: None,
                        valid_to: None,
                        source: String::new(),
                    },
                });
                if let Some(last) = out.last_mut() {
                    last.explain.valid_from = last.note.valid_from.as_ref().map(|v| v.to_db_string());
                    last.explain.valid_to = last.note.valid_to.as_ref().map(|v| v.to_db_string());
                    last.explain.confidence = last.note.confidence;
                    last.explain.source = last.note.source.clone();
                }
            }
        }
        Ok(out)
    }
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
    let body: String = conn.query_row("SELECT body FROM notes WHERE id = ?1", [id], |r| r.get(0))?;
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
    if let Some(min) = f.min_confidence {
        if confidence_rank(note.confidence) < confidence_rank(min) {
            return false;
        }
    }
    if let Some(scope) = &f.scope {
        if note.scope.as_deref() != Some(scope.as_str()) {
            return false;
        }
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
        if !note.valid_from.as_ref().is_none_or(|v| v.covers_from(as_of)) {
            return false;
        }
        if !note.valid_to.as_ref().is_none_or(|v| v.covers_to(as_of)) {
            return false;
        }
    } else {
        // Current truth by default: not yet started, already expired — out.
        if note.valid_from.as_ref().is_some_and(|v| !v.covers_from(now)) {
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

fn load_edges(conn: &rusqlite::Connection, rels: &[&str]) -> Result<Vec<EdgeRow>, SearchError> {
    load_edges_where(conn, "1=1", rels)
}

/// Load edges filtered by an SQL condition (over `src`/`dst`/`rel` columns).
fn load_edges_where(
    conn: &rusqlite::Connection,
    filter: &str,
    rels: &[&str],
) -> Result<Vec<EdgeRow>, SearchError> {
    let rel_clause = if rels.is_empty() {
        String::new()
    } else {
        let placeholders = rels
            .iter()
            .map(|r| format!("'{}'", r.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        format!(" AND rel IN ({placeholders})")
    };
    let sql = format!(
        "SELECT src, rel, dst, note_id, valid_from, valid_to FROM edges WHERE {filter}{rel_clause}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(EdgeRow {
            src: r.get(0)?,
            rel: r.get(1)?,
            dst: r.get(2)?,
            note_id: r.get(3)?,
            valid_from: r.get(4)?,
            valid_to: r.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Graph expansion (§6.3.3): from the seed notes, walk the entity graph
/// (edges valid at `as_of`) up to `max_hops` note-hops; collect newly
/// discovered notes with the path that led to them and the seed score they
/// inherit (decaying per hop in the rerank).
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
        if let Some(subject) = note.subject_entity() {
            if !names.contains(&subject) {
                names.push(subject);
            }
        }
        for name in &names {
            entities
                .entry(name.clone())
                .or_insert_with(|| (format!("[[{name}]]"), *score));
        }
    }

    // BFS over entity adjacency, one edge query per level.
    let mut frontier: Vec<String> = entities.keys().cloned().collect();
    for hop in 1..=max_hops {
        if frontier.is_empty() {
            break;
        }
        let listed = frontier
            .iter()
            .map(|n| format!("'{}'", n.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        // Bounded, deterministic walk: a hub entity (a city mentioned by
        // hundreds of notes) would otherwise drag its whole neighbourhood in.
        let edges = load_edges_where(
            conn,
            &format!("src IN ({listed}) OR dst IN ({listed}) ORDER BY id LIMIT 400"),
            &[],
        )?;
        let mut next: BTreeSet<String> = BTreeSet::new();
        for e in &edges {
            if !valid_at(&e.valid_from, &e.valid_to, as_of) {
                continue;
            }
            // Which endpoint is in the frontier, and what lies on the other side?
            let (from, to) = if frontier.contains(&e.src) && !e.src.starts_with("note:") {
                (e.src.clone(), e.dst.clone())
            } else if frontier.contains(&e.dst) && !e.dst.starts_with("note:") {
                (e.dst.clone(), e.src.clone())
            } else {
                continue;
            };
            let note_like = to.starts_with("note:");
            let (base, score) = entities.get(&from).cloned().unwrap_or_default();
            let dest = if note_like {
                format!("(nota {})", to.get(5..).map(|id| &id[..8.min(id.len())]).unwrap_or("?"))
            } else {
                format!("[[{to}]]")
            };
            let path = format!("{base} -{}-> {dest}", e.rel);
            if !note_like {
                entities.entry(to.clone()).or_insert((path.clone(), score));
                next.insert(to.clone());
            }
            if let Some(nid) = &e.note_id {
                if !exclude.contains(nid) && !extra.contains_key(nid) && *nid != from {
                    extra.insert(nid.clone(), (hop, path, score));
                }
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
    use ulid::Ulid;
    use crate::format::NoteType;

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
        let mut fact = note(3, "Fabio abita a Milano", "[[Fabio]] ha traslocato a [[Milano]] a settembre.", NoteType::Fact);
        fact.valid_from = Some(crate::format::Validity::parse("2026-09-01").unwrap(/* test-only */));
        cell.put_note(&fact, "t").unwrap(/* test-only */);
        let mut old = note(4, "Fabio abitava a Roma", "[[Fabio]] viveva a [[Roma]].", NoteType::Fact);
        old.valid_from = Some(crate::format::Validity::parse("2020-01-01").unwrap(/* test-only */));
        cell.put_note(&old, "t").unwrap(/* test-only */);
        let mut ext = note(5, "Notizia web", "Si dice che [[Fabio]] traslochi a [[Torino]].", NoteType::Fact);
        ext.confidence = Confidence::External;
        ext.status = Status::Quarantined;
        ext.source = "import:web".into();
        cell.put_note(&ext, "t").unwrap(/* test-only */);
        (dir, cell)
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* test-only */)
    }
    const NOW_TS: i64 = 1_810_000_000;

    #[test]
    fn rrf_single_list_preserves_order() {
        let r = rrf_fuse(&[vec!["a".into(), "b".into()]], 60);
        assert!(r["a"] > r["b"]);
        // two lists: overlap wins
        let r2 = rrf_fuse(
            &[
                vec!["a".into(), "b".into()],
                vec!["b".into(), "c".into()],
            ],
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
        assert!(hits[0].explain.matched_terms.contains(&"trasloc".to_string()));
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
        let mut new = note(6, "Fabio vive a Milano", "Fabio abita a [[Milano]].", NoteType::Fact);
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
        assert!(hits_now.iter().all(|h| !h.note.display_title().contains("Roma")));
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
        assert!(ids.contains(&format!("01{:024}", 2)), "Giulia note via graph: {hits:?}");
        let via = hits
            .iter()
            .find(|h| h.note.id.to_string() == format!("01{:024}", 2))
            .unwrap(/* test-only */)
            .explain
            .via
            .clone();
        assert!(via.as_deref().is_some_and(|v| v.contains("->")), "path: {via:?}");
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
        let mut n = note(7, "Lista cose", "cose da fare per il trasloco", NoteType::Note);
        n.tags = vec!["casa/milano/trasloco".into()];
        cell.put_note(&n, "t").unwrap(/* test-only */);
        let mut q = Query::text("trasloco");
        q.filters.tags = vec!["casa".into()];
        let hits = cell.search_with_now(&q, now()).unwrap(/* test-only */);
        assert!(hits.iter().any(|h| h.note.id == n.id));
        let tags = cell.tags("casa").unwrap(/* test-only */);
        assert!(tags.iter().any(|t| t.name == "casa/milano/trasloco"));
    }

    #[test]
    fn neighbors_and_path() {
        let (_d, cell) = setup();
        let ns = cell.neighbors("Fabio", 2, &[]).unwrap(/* test-only */);
        assert!(ns.iter().any(|n| n.entity == "Giulia" && n.rel == "partner_of"));
        assert!(ns.iter().any(|n| n.entity == "Studio Rossi" && n.rel == "works_at"));
        let p = cell.path("Fabio", "Studio Rossi").unwrap(/* test-only */);
        let steps = p.expect("path exists");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].rel, "partner_of");
        assert_eq!(steps[1].to, "Studio Rossi");
        assert!(cell.path("Fabano", "Studio Rossi").unwrap(/* test-only */).is_none());
    }

    #[test]
    fn entities_tags_and_timeline() {
        let (_d, cell) = setup();
        let es = cell.entities("Giul").unwrap(/* test-only */);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].name, "Giulia");
        let tl = cell.timeline("Fabio").unwrap(/* test-only */);
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
