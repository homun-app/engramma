// SPDX-License-Identifier: Apache-2.0

//! Consolidation support (DESIGN §6.5, v0.3): the engine PROPOSES pairs —
//! near duplicates and probable contradictions — and the application decides
//! with its model and its user. Never a mutation.

use crate::cell::Cell;
use crate::cell::vectors::VectorTx;
use crate::error::CellError;
use crate::format::body;
use crate::vector::VectorIndex as _;
use rusqlite::OptionalExtension as _;
use ulid::Ulid;

/// What kind of pair this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// Two living notes saying nearly the same thing (cosine ≥ 0.95 on the
    /// ready model AND entity Jaccard ≥ 0.5).
    NearDuplicate,
    /// The same subject with the same functional relation pointing at two
    /// different destinations, both living, validities overlapping.
    Contradiction,
}

impl CandidateKind {
    /// Human name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateKind::NearDuplicate => "quasi duplicato",
            CandidateKind::Contradiction => "contraddizione probabile",
        }
    }
}

/// One proposed pair.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Which kind.
    pub kind: CandidateKind,
    /// The two notes (ids).
    pub a: Ulid,
    /// The second note.
    pub b: Ulid,
    /// Why: the cosine and the Jaccard, or the relation and the two
    /// destinations with their validity.
    pub detail: String,
}

/// Tuning of [`Cell::consolidation_candidates`]: the defaults are part of
/// the contract.
#[derive(Debug, Clone)]
pub struct Consolidation {
    /// Minimum cosine (on the ready model) of a near duplicate.
    pub near_duplicate_cosine: f64,
    /// Minimum Jaccard of the entity sets.
    pub near_duplicate_jaccard: f64,
    /// Relations where one destination per subject is expected.
    pub functional_rels: Vec<String>,
}

impl Default for Consolidation {
    fn default() -> Self {
        Self {
            near_duplicate_cosine: 0.95,
            near_duplicate_jaccard: 0.5,
            functional_rels: ["lives_in", "works_at", "partner_of"]
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

impl Cell {
    /// Pairs worth consolidating (DESIGN §6.5): near duplicates and probable
    /// contradictions, at most `limit` of each. Read-only; the engine
    /// proposes, the application decides.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failure.
    pub fn consolidation_candidates(
        &self,
        limit: usize,
        cfg: &Consolidation,
    ) -> Result<Vec<Candidate>, CellError> {
        let mut out = Vec::new();
        out.extend(self.near_duplicates(limit, cfg)?);
        out.extend(self.contradictions(limit, cfg)?);
        Ok(out)
    }

    /// Near duplicates: the vector index proposes the neighbours (cosine
    /// ≥ 0.95 recomputed exactly), the entity Jaccard confirms (≥ 0.5).
    fn near_duplicates(
        &self,
        limit: usize,
        cfg: &Consolidation,
    ) -> Result<Vec<Candidate>, CellError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| CellError::Db("cell mutex poisoned".into()))?;
        let Some(ready) = VectorTx { conn: &inner.conn }.ready_model()? else {
            return Ok(Vec::new()); // no vectors: nothing to compare
        };
        // the neighbour proposals come from the same index the search uses
        crate::search::ensure_index(&mut inner).map_err(|e| CellError::Db(e.to_string()))?;
        let dim = usize::try_from(ready.dim).unwrap_or(0);
        // the exact vectors, in memory: pairs need the true cosine —
        // v0.6.8 (H1): the bin's copies sit out (their vectors stay for
        // the reopen, the candidates do not)
        let in_bin = crate::cell::bin::bin_note_ids(&inner.conn)?;
        let mut vectors: std::collections::HashMap<String, Vec<Vec<f32>>> =
            std::collections::HashMap::new();
        {
            let mut stmt = inner.conn.prepare(
                "SELECT note_id, vec FROM note_vectors WHERE model = ?1 ORDER BY note_id, chunk",
            )?;
            let rows = stmt.query_map([&ready.model], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (id, blob) = row?;
                if in_bin.contains(&id) {
                    continue;
                }
                if let Ok(v) = crate::embed::decode_vec(&blob, dim) {
                    vectors.entry(id).or_default().push(v);
                }
            }
        }
        let mut seen: std::collections::BTreeSet<(String, String)> =
            std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for (id, chunks) in &vectors {
            if chunks.is_empty() {
                continue;
            }
            let Some(state) = inner.index.as_ref() else {
                continue;
            };
            let mut hits = state.idx.search(&chunks[0], 6);
            hits.retain(|h| &h.note_id != id);
            for h in hits {
                let pair = if id < &h.note_id {
                    (id.clone(), h.note_id.clone())
                } else {
                    (h.note_id.clone(), id.clone())
                };
                if !seen.insert(pair.clone()) {
                    continue;
                }
                let (Some(va), Some(vb)) = (vectors.get(&pair.0), vectors.get(&pair.1)) else {
                    continue;
                };
                let cos = best_cosine(va, vb);
                if cos < cfg.near_duplicate_cosine {
                    continue;
                }
                // v0.3.1 (P2.8): a `person`/`place` page is a HUB — it talks
                // about its subject by design, so it sits close to any note
                // about the same person (the two golden false positives:
                // Luca Ferrari ↔ «Luca e la matematica», 0.961; Elena Bianchi
                // ↔ «Il ruolo di Elena Bianchi», 0.954). A hub page never
                // duplicates a note of another kind; the other kinds compare
                // freely among themselves (the golden duplicate is a project
                // against a fact — the bathroom plan and its twin).
                let hub_of = |id: &str| -> Result<bool, CellError> {
                    let t: Option<String> = inner
                        .conn
                        .query_row("SELECT type FROM notes WHERE id = ?1", [id], |r| r.get(0))
                        .optional()
                        .map_err(CellError::from)?;
                    Ok(matches!(t.as_deref(), Some("person") | Some("place")))
                };
                if hub_of(&pair.0)? != hub_of(&pair.1)? {
                    continue;
                }
                let jac = entity_jaccard(&inner.conn, &pair.0, &pair.1)?;
                if jac < cfg.near_duplicate_jaccard {
                    continue;
                }
                let (Ok(a), Ok(b)) = (Ulid::from_string(&pair.0), Ulid::from_string(&pair.1))
                else {
                    continue;
                };
                out.push(Candidate {
                    kind: CandidateKind::NearDuplicate,
                    a,
                    b,
                    detail: format!("cosine {cos:.3}, entities in common {jac:.2}"),
                });
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// Probable contradictions: same subject, same functional relation, two
    /// different destinations, both notes living, validities overlapping.
    fn contradictions(
        &self,
        limit: usize,
        cfg: &Consolidation,
    ) -> Result<Vec<Candidate>, CellError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| CellError::Db("cell mutex poisoned".into()))?;
        let placeholders = vec!["?"; cfg.functional_rels.len()].join(", ");
        // v0.6.8 (H1): the bin's one predicate — an edge a bin's copy
        // declares is nobody's contradiction
        let sql = format!(
            "SELECT e.src, e.rel, e.dst, e.note_id, e.valid_from, e.valid_to
             FROM edges e JOIN notes n ON n.id = e.note_id
             WHERE e.rel IN ({placeholders}) AND e.status = 'active' AND {}
             ORDER BY e.src, e.rel, e.dst",
            crate::cell::bin::not_in_bin("n")
        );
        let mut stmt = inner.conn.prepare(&sql)?;
        let rels: Vec<String> = cfg.functional_rels.clone();
        let rows = stmt.query_map(rusqlite::params_from_iter(rels), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        // (src, rel) -> [(dst, note, from, to)]
        type Claim = (String, String, Option<String>, Option<String>);
        let mut groups: std::collections::BTreeMap<(String, String), Vec<Claim>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let (src, rel, dst, note_id, vf, vt) = row?;
            groups
                .entry((src, rel))
                .or_default()
                .push((dst, note_id, vf, vt));
        }
        let mut out = Vec::new();
        for ((src, rel), claims) in &groups {
            for i in 0..claims.len() {
                for j in i + 1..claims.len() {
                    let (dst_a, note_a, from_a, to_a) = &claims[i];
                    let (dst_b, note_b, from_b, to_b) = &claims[j];
                    if dst_a == dst_b {
                        continue; // same destination: no contradiction
                    }
                    if !overlaps(from_a, to_a, from_b, to_b) {
                        continue;
                    }
                    let (Ok(a), Ok(b)) = (Ulid::from_string(note_a), Ulid::from_string(note_b))
                    else {
                        continue;
                    };
                    // the same note naming two destinations of a functional
                    // relation IS the contradiction (the golden case: two
                    // lives_in on the same page, no supersede)
                    out.push(Candidate {
                        kind: CandidateKind::Contradiction,
                        a,
                        b,
                        detail: format!(
                            "{src} {rel} {dst_a} ({}) and {rel} {dst_b} ({})",
                            validity_of(from_a, to_a),
                            validity_of(from_b, to_b),
                        ),
                    });
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The best cosine between any chunk pair (near duplicates are short notes:
/// usually one chunk each).
fn best_cosine(a: &[Vec<f32>], b: &[Vec<f32>]) -> f64 {
    let mut best = f64::NEG_INFINITY;
    for va in a {
        for vb in b {
            let dot: f32 = va.iter().zip(vb.iter()).map(|(x, y)| x * y).sum::<f32>();
            best = best.max(f64::from(dot)); // L2-normalised: dot == cosine
        }
    }
    best
}

/// Jaccard of the entity sets two notes talk about (mentions, relations,
/// subject — lowercased).
fn entity_jaccard(conn: &rusqlite::Connection, a: &str, b: &str) -> Result<f64, CellError> {
    let entities_of = |id: &str| -> Result<std::collections::BTreeSet<String>, CellError> {
        let body: String =
            conn.query_row("SELECT body FROM notes WHERE id = ?1", [id], |r| r.get(0))?;
        let mut set = std::collections::BTreeSet::new();
        if let Ok(analysis) = body::analyze(&body) {
            for name in analysis.links {
                set.insert(name.to_lowercase());
            }
            for r in analysis.relations {
                set.insert(r.dst.to_lowercase());
            }
        }
        Ok(set)
    };
    let (ea, eb) = (entities_of(a)?, entities_of(b)?);
    if ea.is_empty() && eb.is_empty() {
        return Ok(0.0);
    }
    let inter = ea.intersection(&eb).count();
    let union = ea.union(&eb).count();
    Ok(inter as f64 / union as f64)
}

/// Do two validity windows overlap? (Open sides are ±∞.)
fn overlaps(
    from_a: &Option<String>,
    to_a: &Option<String>,
    from_b: &Option<String>,
    to_b: &Option<String>,
) -> bool {
    let parse = |s: &Option<String>| -> Option<i64> {
        s.as_deref()
            .and_then(|x| crate::format::Validity::parse(x).ok())
            .map(|v| v.start().unix_timestamp())
    };
    let (fa, ta) = (parse(from_a), parse(to_a));
    let (fb, tb) = (parse(from_b), parse(to_b));
    let before = fa.is_some_and(|x| tb.is_some_and(|y| x >= y));
    let after = fb.is_some_and(|x| ta.is_some_and(|y| x >= y));
    !(before || after)
}

fn validity_of(from: &Option<String>, to: &Option<String>) -> String {
    format!(
        "from {} to {}",
        from.as_deref().unwrap_or("always"),
        to.as_deref().unwrap_or("now")
    )
}
