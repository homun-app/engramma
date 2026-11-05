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
                    via: None,
                    confidence: note.confidence,
                    valid_from: note.valid_from.as_ref().map(|v| v.to_db_string()),
                    valid_to: note.valid_to.as_ref().map(|v| v.to_db_string()),
                    source: note.source.clone(),
                },
                note,
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
                    bm25_rank: None,
                    via: Some(path),
                    confidence: note.confidence,
                    valid_from: note.valid_from.as_ref().map(|v| v.to_db_string()),
                    valid_to: note.valid_to.as_ref().map(|v| v.to_db_string()),
                    source: note.source.clone(),
                },
                note,
                score: raw,
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
