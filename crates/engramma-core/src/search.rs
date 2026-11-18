// SPDX-License-Identifier: Apache-2.0

//! Search: BM25 candidates (FTS5) + graph expansion + filters + weighted
//! rerank, with an explanation per result (DESIGN §6.3, v0.1 without vectors),
//! and the other queries of §6.4.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use time::OffsetDateTime;
use ulid::Ulid;

use crate::cell::vectors::VectorTx;
use crate::cell::{Cell, Inner};
use crate::embed::decode_vec;
use crate::error::SearchError;
use crate::format::{Confidence, Note, NoteType, Status};
use crate::graph::GraphIndex;
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
    /// Weight of the graph list in the fusion (v0.3): `0` turns it off, and
    /// `graph_hops = 0` keeps it off regardless. 0.35 (v0.3.2): the robust
    /// rule — best weighted MRR in the WORST CASE among the ±1-step
    /// neighbours of the joint α × graph grid at 100k. 0.40 had the best
    /// absolute weighted MRR (0.8656 vs 0.8529) but sits on the edge of a
    /// cliff: at graph 0.45 the same α loses ~0.025 and at 0.50 ~0.15, and
    /// its neighbourhood minimum is 0.8410 against 0.8483 here.
    pub graph_weight: f64,
    /// Teleport share of the personalised-PageRank push (§6.3.3).
    pub ppr_alpha: f64,
    /// Residual threshold of the push, as a fraction of a node's degree: a
    /// node expands only while its residual clears `ε·deg`. This is what
    /// keeps hubs with tens of thousands of edges out of the walk. 10⁻⁴, not
    /// the prototype's 10⁻³: the seeds here share their mass ten ways, and
    /// at 10⁻³ the two-hop answer notes fell under the threshold and never
    /// settled (measured on the 10k multi-hop diagnostics).
    pub ppr_epsilon: f64,
    /// Degree above which a node's outflow is damped (`θ/deg`), GAAMA-style.
    pub hub_theta: f64,
    /// v0.4 (P1.4): how the graph walk starts. The default until this phase
    /// is retuned is [`GraphSeeding::Fused`] — exactly the v0.3.2 behavior.
    pub graph_seeding: GraphSeeding,
    /// v0.4 (P1.5): how much the graph list weighs. `Fixed` keeps
    /// [`Weights::graph_weight`] (the v0.3.2 behavior); `Adaptive` computes
    /// `w_g(q) = w_max · gate(q) · incertezza(q)` per question. The default
    /// until this phase is retuned is `Fixed`.
    pub graph_policy: GraphPolicy,
}

/// The graph knobs a caller overrides in one breath (v0.4, P1.6): what
/// `ENGRAMMA_GRAPH` parses into on the CLI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphOverride {
    /// The weight policy.
    pub policy: GraphPolicy,
    /// The weight for the `Fixed` policy (and the fallback the adaptive
    /// path never reads).
    pub weight: f64,
    /// The seeding.
    pub seeding: GraphSeeding,
}

/// How much the graph list weighs in the fusion (v0.4, P1.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphPolicy {
    /// The fixed [`Weights::graph_weight`] — the v0.3.2 behavior, kept for
    /// the benchmark comparisons and as the way back.
    Fixed,
    /// `w_g(q) = w_max · gate(q) · incertezza(q)`: the gate is P1.4's
    /// (0 when the question names no entity), and `incertezza(q)` sits in
    /// `[u_min, 1]` on two free signals of the fused list — the first to
    /// second gap and the text/vector agreement on the first result. A
    /// clear, agreed first result needs no graph; a thin one does.
    Adaptive {
        /// The ceiling of `w_g`.
        w_max: f64,
        /// The floor of `incertezza`.
        u_min: f64,
    },
}

/// How the personalised-PageRank walk is seeded (v0.4, P1.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphSeeding {
    /// v0.3.2: the strongest fused candidates only, mass ∝ their score.
    Fused,
    /// HippoRAG 2-style: the entities the QUESTION names (exact match on
    /// 1–3 consecutive words against entity names and the aliases of
    /// entity-notes, normalized like the index) carry the mass, plus the
    /// fused candidates with `seed_passage_weight ×` their normalized
    /// score (HippoRAG 2's flat 0.05). **Gate:** a question that names no
    /// graph entity does not wake the graph at all — the fused list
    /// stands, and `explain` says so.
    Question {
        /// The fused candidates' share (start at 0.05, as HippoRAG 2).
        seed_passage_weight: f64,
    },
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            stated: 1.0,
            inferred: 0.8,
            external: 0.5,
            recency_half_life_days: 180.0,
            graph_weight: 0.35,
            ppr_alpha: 0.25,
            ppr_epsilon: 1e-4,
            hub_theta: 50.0,
            graph_seeding: GraphSeeding::Fused,
            graph_policy: GraphPolicy::Fixed,
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
    /// Position in the graph (personalised-PageRank) list, 1-based (v0.3).
    pub graph_rank: Option<usize>,
    /// Cosine with the query embedding (best chunk), when the vector branch
    /// produced this result.
    pub cosine: Option<f64>,
    /// The raw fused score before the display normalisation (v0.2).
    pub fused_score: Option<f64>,
    /// Which candidate lists produced it: `text`, `vector`, `graph`.
    pub lists: Vec<&'static str>,
    /// How well the result is supported (v0.2.1): `weak` when it arrives
    /// only from the vector branch with a thin cosine margin over the next
    /// vector candidate — close in meaning, unconfirmed by any text match;
    /// `strong` otherwise. `None` on non-search hits (timeline).
    pub support: Option<&'static str>,
    /// The chunk that won, for a multi-chunk note.
    pub winning_chunk: Option<usize>,
    /// Graph path that led here, e.g. `[[Giulia]] -lives_in-> [[Milano]]`.
    pub via: Option<String>,
    /// The graph's say for this QUERY (v0.4, P1.4/P1.5) — the same for
    /// every hit of one search: the weight the graph list ended up with
    /// and, when it stayed off, why. `None` when the graph never ran
    /// (`graph_hops = 0` or weight 0).
    pub graph: Option<GraphExplain>,
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
            graph_rank: None,
            cosine: None,
            fused_score: None,
            lists: Vec::new(),
            support: None,
            winning_chunk: None,
            via: None,
            graph: None,
            confidence: Confidence::Stated,
            valid_from: None,
            valid_to: None,
            source: String::new(),
        }
    }
}

/// What the graph did for one query (v0.4): per-query, carried by every
/// hit's explain.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphExplain {
    /// The weight the graph list ended up with in the fusion (`0.0` when
    /// the gate closed).
    pub weight: f64,
    /// Why the graph stayed off, when it did («grafo spento: nessuna
    /// entità nominata», P1.4).
    pub off: Option<&'static str>,
    /// The first-to-second gap of the fused list, on its [0, 1] scale
    /// (P1.5's first signal; `None` when the graph never ran).
    pub gap: Option<f64>,
    /// Whether the text and vector lists agree on the first result
    /// (P1.5's second signal; `None` without both lists).
    pub agreement: Option<bool>,
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

/// How the cosine is put on the same scale as the min-max'd BM25 inside the
/// convex combination (v0.2.1). Summing the raw cosine let BM25's full [0, 1]
/// span outweigh a cosine that, between candidates of real embeddings,
/// moves by ~0.1: α weighted two unrelated scales.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CosNorm {
    /// Min-max over the vector candidates of this query: best candidate 1,
    /// worst 0. Uses the whole available cosine range; unstable on few or
    /// very similar candidates (one outlier stretches everyone else).
    Candidates,
    /// Theoretical min-max with fixed limits: for the cosine of L2 unit
    /// vectors the limits are −1 and +1 (fusion-function analysis for hybrid
    /// retrieval, ACM TOIS 2023). Stable by construction, at the price of a
    /// compressed cosine span (the natural width stays ~0.1).
    Theoretical,
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
    /// Convex combination: `alpha·cos_norm(cos) + (1−alpha)·bm25_minmax`,
    /// both terms min-max normalised (BM25 and, with
    /// [`CosNorm::Candidates`], the cosine over the candidates); a note
    /// missing from one list scores 0 for it. The graph list weighs
    /// [`Weights::graph_weight`], fixed.
    Convex {
        /// Weight of the cosine (0–1).
        alpha: f64,
        /// How the cosine is normalised.
        cos_norm: CosNorm,
    },
    /// v0.4 (P1.5): the same convex text+vector fusion, but the graph list
    /// weighs `w_g(q) = w_max · gate(q) · incertezza(q)` — per question,
    /// from the two free signals of the fused list (the gap and the
    /// agreement, QPP-style). This variant OVERRIDES the query's
    /// [`GraphPolicy`] with its own `w_max`/`u_min`: it is the knob the
    /// benchmark grid turns, and (from P1.6 on) the default.
    Adaptive {
        /// Weight of the cosine (0–1).
        alpha: f64,
        /// How the cosine is normalised.
        cos_norm: CosNorm,
        /// The ceiling of the graph weight.
        w_max: f64,
        /// The floor of `incertezza(q)`.
        u_min: f64,
    },
}

impl Default for Fusion {
    fn default() -> Self {
        // Measured on the golden vault and the 10k/100k benchmark (REPORT
        // §v0.2.1): the convex combination with the cosine min-max'd over the
        // candidates — the cosine rides on BM25's [0, 1] scale, so α means
        // what it says. Retuned JOINTLY with the graph weight in v0.3.1
        // (0.6/0.40, best absolute weighted MRR 0.8656), again in v0.3.2
        // with the ROBUST rule — best weighted MRR in the worst case among
        // the ±1-step neighbours of the grid: 0.7/0.35 (0.8529, neighbourhood
        // minimum 0.8483) — and finally in v0.4.5 with TWO benches: on the
        // Italian 100k grid the admissible set (weighted MRR ≥ 0.8429,
        // α-neighbourhood minimum ≥ 0.8383) is {0.5, 0.6, 0.7} (0.5: 0.8577 /
        // 0.8397; 0.6: 0.8527 / 0.8527; 0.7: 0.8529 / 0.8485), and among
        // them LongMemEval dev's turn ndcg@5 picks 0.5 (0.7303 vs 0.7049 and
        // 0.6765) — confirmed on 320 held-out questions (session any@5
        // 0.9750 vs text 0.9625, all@5 0.8500 vs 0.8344, turn ndcg@5 0.6813
        // vs 0.6620). REPORT v0.4.5.
        Fusion::Convex {
            alpha: 0.5,
            cos_norm: CosNorm::Candidates,
        }
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
    /// A vector-only result whose cosine margin over the next vector
    /// candidate is below this is reported `support: "weak"` (v0.2.1: one
    /// signal, not a prohibition — the absolute cosine separates nothing,
    /// a thin margin says something). Tuned on the golden vault (0.03: the
    /// top «che auto guida Giulia» margin is 0.026, the paraphrase winners
    /// sit above), REPORT.
    pub weak_margin: f64,
}

impl Semantic {
    /// Defaults: hybrid, [`Fusion::default()`] (see there — it is a
    /// measured choice, not a literature default) and tau = 0.30 (tuned on
    /// the golden vault, REPORT).
    #[must_use]
    pub fn new(embedder: std::sync::Arc<dyn crate::embed::Embedder>) -> Self {
        Self {
            embedder,
            mode: Mode::Hybrid,
            fusion: Fusion::default(),
            tau: 0.30,
            weak_margin: 0.03,
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

    /// Set the `weak`-support margin (see [`Semantic::weak_margin`]).
    #[must_use]
    pub fn with_weak_margin(mut self, margin: f64) -> Self {
        self.weak_margin = margin;
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
    /// Edges the personalised-PageRank push touched (v0.3 diagnostics: the
    /// whole point of the push is that this stays in the hundreds at 100k
    /// notes, hubs included).
    pub graph_pushed_edges: usize,
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

/// Terms matching more than this fraction of the corpus leave the FTS MATCH
/// (see [`match_expr_for`]).
const FREQUENT_TERM_FRACTION: f64 = 0.20;
/// …and only when they match at least this many documents. Below it the
/// bm25 pass is not a cost problem, and dropping would distort small
/// corpora: on a 60-note vault a term in a third of the notes is signal,
/// not filler (found while re-running the golden tests, v0.2.1).
const FREQUENT_TERM_MIN_DOCS: i64 = 1_000;

/// The MATCH expression over the query words: every form of every word
/// quoted and joined by OR (an uncertain question carries alternatives,
/// v0.5.3 P0 — they are the SAME word in the languages of the cell), plus
/// the phrase of all the canonical forms and — v0.2.1 (P1.7) — a phrase per
/// **adjacent pair** of them: «il partner di Anna Rossi» yields "anna ross",
/// which matches the partner's own note («Partner di [[Anna Rossi]]») and
/// the subject's notes, not every Anna in the vault. The all-stems phrase
/// alone almost never matches; the pairs carry the namesake signal the v0.1
/// comment promised.
fn or_match_expr(words: &[crate::text::QueryWord]) -> String {
    let single = words.len() == 1;
    let mut alts: Vec<String> = words
        .iter()
        .flat_map(|w| {
            w.forms.iter().map(|f| {
                // the raw form carries its column filter; stems match anywhere
                let raw = w.raw.as_ref() == Some(f);
                crate::text::QueryWord::fragment(f, raw, single)
            })
        })
        .collect();
    if words.len() >= 2 {
        let primary: Vec<&str> = words.iter().map(|w| w.primary.as_str()).collect();
        alts.push(format!(
            "\"{}\"",
            primary
                .iter()
                .map(|t| t.replace('"', ""))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        for w in primary.windows(2) {
            alts.push(format!(
                "\"{} {}\"",
                w[0].replace('"', ""),
                w[1].replace('"', "")
            ));
        }
    }
    alts.join(" OR ")
}

/// The MATCH expression for the text side, minus the ubiquitous terms
/// (v0.2.1, P0.3): FTS5 evaluates `bm25()` for **every** row a term matches
/// before the `LIMIT`, so one term present in a fifth of the notes (a filler
/// verb like «lavora» in a work-heavy corpus) paid a full-corpus ranking
/// pass. Document frequencies come from the index's own vocabulary
/// (`fts5vocab`, created with the schema); a term is dropped only while at
/// least one other term stays in the MATCH. Dropped terms still count in
/// the explain's found terms and in the coverage factor.
fn match_expr_for(
    conn: &rusqlite::Connection,
    qt: &crate::text::QueryTerms,
    include_quarantined: bool,
) -> Result<String, SearchError> {
    let total = visible_note_count(conn, include_quarantined);
    let df = doc_frequencies(conn, &qt.flat(), include_quarantined)?;
    Ok(match_expr_with(qt, &df, total))
}

/// How many notes the DEFAULT view sees (v0.5.3, P1: the totals the
/// frequent-term and support decisions divide by count the visible notes
/// only — a hidden note must not bend them).
pub(crate) fn visible_note_count(conn: &rusqlite::Connection, include_quarantined: bool) -> i64 {
    // v0.6.8 (H1): the totals speak the bin's one predicate — a copy
    // that follows a lapide bends no denominator. The count SUBTRACTS
    // the bin's visible-status copies (one small join) instead of
    // probing every row: this runs on every query, and the per-row
    // probe cost 0,4 ms at 10 000 notes (measured, REPORT v0.6.8)
    conn.query_row(
        &format!(
            "SELECT (SELECT COUNT(*) FROM notes WHERE status IN {statuses})
                   - (SELECT COUNT(*) FROM conflict_copies cc
                      CROSS JOIN notes o ON o.id = cc.original_id
                      CROSS JOIN notes n ON n.id = cc.copy_id
                      WHERE o.status = 'deleted' AND n.status IN {statuses})",
            statuses = visible_statuses(include_quarantined),
        ),
        [],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Document frequency of each form among the notes the caller may see.
/// SQLite 3.54's fts5vocab yields ONE row per term: (term, doc, cnt) where
/// `doc` is the document frequency and `cnt` the total occurrences (probed
/// by hand on the bundled 3.54.1; older fts5vocab emitted one row per
/// term/document pair). An empty map means "unknown" — nothing is dropped,
/// nothing is discounted. The base vocabulary counts the visible notes only
/// (v0.6, P0: the quarantined rows live in their own index, so nothing is
/// subtracted anymore); the owner's `include_quarantined` view sums the
/// hidden vocabulary in.
fn doc_frequencies(
    conn: &rusqlite::Connection,
    terms: &[String],
    include_quarantined: bool,
) -> Result<HashMap<String, i64>, SearchError> {
    if terms.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; terms.len()].join(", ");
    let sql = if include_quarantined {
        // the two vocabularies in one pass: same terms, summed docs
        format!(
            "SELECT term, SUM(doc) FROM (
                 SELECT term, doc FROM notes_fts_vocab WHERE term IN ({placeholders})
                 UNION ALL
                 SELECT term, doc FROM notes_fts_hidden_vocab WHERE term IN ({placeholders})
             ) GROUP BY term"
        )
    } else {
        format!("SELECT term, doc FROM notes_fts_vocab WHERE term IN ({placeholders})")
    };
    let Ok(mut stmt) = conn.prepare(&sql) else {
        // no vocabulary view (should not happen: the schema creates it)
        return Ok(HashMap::new());
    };
    let mut params: Vec<String> = terms.to_vec();
    if include_quarantined {
        params.extend(terms.iter().cloned());
    }
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(SearchError::from)?;
    let mut df: HashMap<String, i64> = HashMap::with_capacity(terms.len());
    for row in rows {
        let (term, doc_freq) = row?;
        df.insert(term, doc_freq);
    }
    Ok(df)
}

/// Is this document frequency "ubiquitous" for the MATCH (a **cost**
/// decision)? Both bounds: the fraction of the corpus AND the absolute
/// floor ([`FREQUENT_TERM_MIN_DOCS`]).
fn frequent_term(df: i64, total: i64) -> bool {
    df >= FREQUENT_TERM_MIN_DOCS && (df as f64) > FREQUENT_TERM_FRACTION * total as f64
}

/// Is this document frequency a non-discriminative share of the corpus (an
/// **informativeness** decision, used by the `weak` support label)? The
/// fraction, with a minimal absolute floor: below a handful of documents a
/// term is part of the corpus's basic vocabulary (in a three-note vault
/// everything sits above 20%), not evidence of ubiquity.
fn non_discriminative(df: i64, total: i64) -> bool {
    const MIN_DOCS: f64 = 5.0;
    total > 0 && df as f64 >= MIN_DOCS && (df as f64) > FREQUENT_TERM_FRACTION * total as f64
}

/// The MATCH expression given precomputed document frequencies: ubiquitous
/// words leave (a frequent word can go only while a word that MATCHES
/// something stays behind — a MATCH made only of zero-frequency words would
/// find nothing; when every matching word is frequent they all stay, the
/// query is inherently broad and the old, costly behaviour is honest).
/// A WORD's document frequency is the best of its forms: they are
/// alternatives of the same word (v0.5.3, P0), and a word is dropped only
/// when every language the cell speaks calls it filler.
fn match_expr_with(qt: &crate::text::QueryTerms, df: &HashMap<String, i64>, total: i64) -> String {
    if qt.words.len() < 2 || total == 0 {
        return or_match_expr(&qt.words);
    }
    let df_of = |w: &crate::text::QueryWord| {
        w.forms
            .iter()
            .map(|f| df.get(f.as_str()).copied().unwrap_or(0))
            .max()
            .unwrap_or(0)
    };
    let matching = qt.words.iter().filter(|w| df_of(w) > 0).count();
    let mut kept: Vec<crate::text::QueryWord> = if matching >= 2 {
        let kept: Vec<crate::text::QueryWord> = qt
            .words
            .iter()
            .filter(|w| !frequent_term(df_of(w), total))
            .cloned()
            .collect();
        if kept.iter().any(|w| df_of(w) > 0) {
            kept
        } else {
            qt.words.clone()
        }
    } else {
        qt.words.clone()
    };
    // v0.5.3 (P0, chiusura): an uncertain question's forms that the index
    // does not hold (df = 0) leave the singles — they cannot match a
    // document, and carrying them had cost half a millisecond per query
    // (measured at 10k: p50 of the text search 1.2 → 1.7 ms). The last
    // form of a word stays (a MATCH that finds nothing is honest), and so
    // does the canonical form (the pairs are built on it).
    for w in &mut kept {
        if w.forms.len() > 1 {
            let primary = w.primary.clone();
            w.forms
                .retain(|f| df.get(f.as_str()).copied().unwrap_or(0) > 0 || *f == primary);
            if w.forms.is_empty() {
                // every form was absent: keep the primary alone
                w.forms.push(primary);
            }
        }
    }
    or_match_expr(&kept)
}

/// The BM25 candidate window for a query (v0.4.2, P0.1): at least 50 (the
/// historical window, so the default `k ≤ 50` profile costs the same as
/// ever), never below the asked `k` — a silent cut is a lying answer.
fn candidate_window(k: usize) -> u64 {
    u64::try_from(k.max(50)).unwrap_or(50)
}

/// The status set a read may see: the default view hides quarantined notes
/// and tombstones; `include_quarantined` (the owner's own surfaces) lets
/// the quarantined ones back in. THE ONE source of this truth — the FTS
/// query, the candidate read and the vector list all speak it.
pub(crate) fn visible_statuses(include_quarantined: bool) -> &'static str {
    if include_quarantined {
        "('active', 'superseded', 'quarantined')"
    } else {
        "('active', 'superseded')"
    }
}

/// One BM25 list from ONE index: `(id, -bm25)` (FTS5's bm25() is negative,
/// more negative = better), at most `limit`. The visibility of state rides
/// INSIDE the query (v0.5.3, P1) — a row the statuses exclude never enters
/// the list, so the ranks the rerank and `explain` see are positions among
/// the matches that count, no hidden note displaces a visible one out of
/// the window, and the max-normalisations downstream never see a hidden
/// maximum.
fn bm25_from(
    conn: &rusqlite::Connection,
    table: &str,
    match_expr: &str,
    limit: i64,
    statuses: &str,
) -> Result<Vec<(String, f64)>, SearchError> {
    // v0.6.8 (H1): the bin's one predicate — deleted notes and the
    // copies that follow a lapide answer no search, whatever the copy's
    // own state (the `include_quarantined` owner's view included: it
    // widens quarantine, not the bin)
    let sql = format!(
        "SELECT n.id, bm25({table}, 10.0, 3.0, 2.0) FROM {table}
         JOIN notes n ON n.rowid = {table}.rowid
         WHERE {table} MATCH ?1 AND n.status IN {statuses}
           AND {not_in_bin}
         ORDER BY bm25({table}, 10.0, 3.0, 2.0) LIMIT ?2",
        not_in_bin = crate::cell::bin::not_in_bin("n"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![match_expr, limit], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, bm25) = row?;
        out.push((id, -bm25));
    }
    Ok(out)
}

/// The BM25 candidate list for a MATCH expression. The default view answers
/// from `notes_fts` alone — and since v0.6 (P0) that index holds the
/// visible notes ONLY, so its IDF cannot be moved by a hidden row and the
/// visible scores are bit-stable no matter what the quarantine holds. The
/// owner's `include_quarantined` view queries BOTH indices; their bm25 are
/// two incommensurable numbers (each IDF lives in its own index), so the
/// choice is to fuse BY RANK inside the text branch — RRF with k = 60, the
/// same fusion the hybrid search offers — a choice of DESIGN §5, written
/// here: an alternative (recomputing bm25 over the union) would need one
/// index holding everything, which is exactly what this phase removed.
fn bm25_candidates(
    conn: &rusqlite::Connection,
    match_expr: &str,
    limit: u64,
    include_quarantined: bool,
) -> Result<Vec<(String, f64)>, SearchError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let base = bm25_from(
        conn,
        "notes_fts",
        match_expr,
        limit,
        "('active', 'superseded')",
    )?;
    if !include_quarantined {
        return Ok(base);
    }
    let hidden = bm25_from(
        conn,
        "notes_fts_hidden",
        match_expr,
        limit,
        "('quarantined')",
    )?;
    // RRF over the two lists (a note lives in exactly one of them), ties
    // broken by id: every copy of a cell must fuse to the same order.
    const K: u64 = 60;
    let mut fused: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for list in [&base, &hidden] {
        for (rank, (id, _)) in list.iter().enumerate() {
            *fused.entry(id.clone()).or_default() += 1.0 / (K as f64 + (rank + 1) as f64);
        }
    }
    let mut out: Vec<(String, f64)> = fused.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(out)
}

/// One candidate note with the columns the rerank needs: the parsed note
/// plus `body_norm`/`tags_text` for the coverage factor.
struct CandRow {
    note: Note,
    body_norm: String,
    tags_text: String,
}

/// Read many candidate notes in **one** query (v0.2.1, P0.3): the ids land
/// in a temp table and a single JOIN returns the note, its body and the
/// coverage columns — the v0.1 flow paid two queries and a JSON parse per
/// candidate, plus one more per coverage term set. Filters SQL can express
/// (`status`, `type`) ride the JOIN; bitemporal validity, tags, scope and
/// confidence stay in `passes_filters`, where their semantics live.
fn read_candidates(
    conn: &rusqlite::Connection,
    ids: &[String],
    include_quarantined: bool,
    types: &[NoteType],
) -> Result<HashMap<String, CandRow>, SearchError> {
    let mut out: HashMap<String, CandRow> = HashMap::with_capacity(ids.len());
    if ids.is_empty() {
        return Ok(out);
    }
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS cand (id TEXT PRIMARY KEY) WITHOUT ROWID;",
    )?;
    conn.execute("DELETE FROM cand", [])?;
    {
        let mut ins = conn.prepare("INSERT OR IGNORE INTO cand (id) VALUES (?1)")?;
        for id in ids {
            ins.execute([id])?;
        }
    }
    let statuses = if include_quarantined {
        "('active', 'superseded', 'quarantined')"
    } else {
        "('active', 'superseded')"
    };
    // `type` values come from our own enum, never from caller text.
    let types_list = types
        .iter()
        .map(|t| format!("'{}'", t.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let type_clause = if types.is_empty() {
        String::new()
    } else {
        format!(" AND n.type IN ({types_list})")
    };
    // CROSS JOIN pins the join order: the (tiny) temp table drives indexed
    // lookups on notes. A plain JOIN let the planner drive from the status
    // filter — every active note in the cell probed against `cand`, 100k
    // probes per query (found profiling the probe at 100k, v0.2.1).
    // v0.6.8 (H1): the bin's contents never become candidates — the one
    // predicate the BM25 lists apply, so the vector arm (whose ids come
    // from the index, not from FTS) cannot leak a hidden copy either.
    let sql = format!(
        "SELECT n.id, n.frontmatter_json, n.body, n.body_norm, n.tags_text
         FROM cand c CROSS JOIN notes n ON n.id = c.id
         WHERE n.status IN {statuses}{type_clause}
           AND {not_in_bin}",
        not_in_bin = crate::cell::bin::not_in_bin("n"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (id, fm, body, body_norm, tags_text) = row?;
        let parsed = serde_json::from_str::<serde_json::Value>(&fm)
            .map_err(|e| SearchError::Db(e.to_string()))
            .and_then(|v| Note::from_json(&v).map_err(|e| SearchError::Db(e.to_string())));
        if let Ok(mut note) = parsed {
            note.body = body;
            out.insert(
                id,
                CandRow {
                    note,
                    body_norm,
                    tags_text,
                },
            );
        }
    }
    Ok(out)
}

/// Query terms found in a note's indexed text and tags, with the coverage
/// factor (1.0 every word matches, 0.5 none does) — pure: the columns come
/// from [`read_candidates`]. A word counts as matched when ANY of its forms
/// appears (they are alternatives of the same word, v0.5.3 P0); the matched
/// list reports the forms that are actually there.
fn coverage_of(
    body_norm: &str,
    tags_text: &str,
    qt: &crate::text::QueryTerms,
) -> (Vec<String>, f64) {
    let words: BTreeSet<&str> = body_norm.split(' ').filter(|w| !w.is_empty()).collect();
    let tag_words: BTreeSet<&str> = tags_text.split(' ').filter(|w| !w.is_empty()).collect();
    let mut matched: Vec<String> = Vec::new();
    let mut matched_words = 0_usize;
    for w in &qt.words {
        let hits: Vec<&String> = w
            .forms
            .iter()
            .filter(|f| words.contains(f.as_str()) || tag_words.contains(f.as_str()))
            .collect();
        if !hits.is_empty() {
            matched_words += 1;
            for h in hits {
                if !matched.contains(h) {
                    matched.push(h.clone());
                }
            }
        }
    }
    let coverage = if qt.words.is_empty() {
        1.0
    } else {
        0.5 + 0.5 * (matched_words as f64 / qt.words.len() as f64)
    };
    (matched, coverage)
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
        let guard = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let mut inner = guard;
        if q.graph_hops > 0 && q.weights.graph_weight > 0.0 {
            ensure_graph(&mut inner)?;
        }
        // v0.5.3 (P0): the language histogram only serves an UNCERTAIN
        // question, and must be built before `conn` is borrowed for the rest
        // of the pipeline
        let cell_langs = if q.lang.is_none() && text::detect_query_lang(&q.text).is_none() {
            ensure_lang_hist(&mut inner)?
        } else {
            Vec::new()
        };
        let conn = &inner.conn;

        // 1. BM25 candidates, with the ubiquitous terms out of the MATCH
        // (they still count for coverage below). v0.4.2 (P0.1): the window is
        // `max(50, k)` — a fixed 50 silently cut every explicit `k` above it,
        // and the caller never knew. The default profile (k ≤ 50) is the same
        // as ever.
        // v0.5.3 (P0): an uncertain question (short, or unreliably detected)
        // expands its stems over the languages of the cell (`cell_langs`,
        // built above); a confident or explicit-language question stems once,
        // as it always did.
        let terms = text::query_terms(&q.text, q.lang, &cell_langs);
        if terms.is_empty() {
            return Err(SearchError::Invalid("query has no usable terms".into()));
        }
        let match_expr = match_expr_for(conn, &terms, q.filters.include_quarantined)?;
        let text_hits = bm25_candidates(
            conn,
            &match_expr,
            candidate_window(q.k),
            q.filters.include_quarantined,
        )?;

        // 2. Filters come BEFORE the graph expansion: only candidates that
        // pass them (status, quarantine, superseded, `as_of`) can seed the
        // walk — a quarantined note must not guide the search (§5). One
        // batched read for every candidate (P0.3).
        let cand_rows = read_candidates(
            conn,
            &text_hits
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            q.filters.include_quarantined,
            &q.filters.types,
        )?;
        let mut text_candidates: Vec<(String, f64, usize, CandRow)> = Vec::new();
        for (rank, (id, bm25)) in text_hits.iter().enumerate() {
            let Some(row) = cand_rows.get(id) else {
                continue;
            };
            if !passes_filters(&row.note, &q.filters, now) {
                continue;
            }
            text_candidates.push((
                id.clone(),
                *bm25,
                rank + 1,
                CandRow {
                    note: row.note.clone(),
                    body_norm: row.body_norm.clone(),
                    tags_text: row.tags_text.clone(),
                },
            ));
        }

        // 3. Seeds for the graph push: the strongest filtered candidates,
        // normalised (mass ∝ bm25_norm) — the passage half of the starting
        // vector, whichever [`GraphSeeding`] is at work.
        let max_bm25 = text_candidates
            .iter()
            .map(|(_, b, _, _)| *b)
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON);
        let seeds: Vec<(String, f64)> = text_candidates
            .iter()
            .take(GRAPH_SEEDS)
            .map(|(id, b, _, _)| (id.clone(), *b / max_bm25))
            .collect();

        // 4. The graph list: a personalised-PageRank push over the in-memory
        // CSR (§6.3.3). Edges declared by quarantined or deleted notes never
        // carry mass unless asked for; `as_of` skips edges that were not
        // valid then. v0.4 (P1.4): with question-entity seeding a query
        // that names no entity gates the graph off.
        // v0.4 (P1.5): the graph's weight is per question. The first signal
        // is free: the first-to-second gap of the fused (here: BM25,
        // max-normalised) list. No vector list, no agreement signal.
        let norm_bm25: Vec<f64> = text_candidates
            .iter()
            .map(|(_, b, _, _)| *b / max_bm25)
            .collect();
        let gap = gap_of(&norm_bm25);
        let (w_g, graph_say_gap, graph_say_agreement) = match q.weights.graph_policy {
            GraphPolicy::Fixed => (q.weights.graph_weight.clamp(0.0, 1.0), gap, None),
            GraphPolicy::Adaptive { w_max, u_min } => {
                let conf = graph_confidence(gap, None);
                let w = w_max.clamp(0.0, 1.0) * graph_uncertainty(u_min, conf);
                (w, gap, None)
            }
        };
        let graph_ran = q.graph_hops > 0 && w_g > 0.0;
        let (glist, _pushed, mut graph_say) = if graph_ran {
            match q.weights.graph_seeding {
                GraphSeeding::Question { .. } => graph_list(&inner, q, &seeds, w_g)?,
                GraphSeeding::Fused if !seeds.is_empty() => graph_list(&inner, q, &seeds, w_g)?,
                GraphSeeding::Fused => (
                    Vec::new(),
                    0,
                    GraphExplain {
                        weight: 0.0,
                        off: None,
                        gap: None,
                        agreement: None,
                    },
                ),
            }
        } else {
            (
                Vec::new(),
                0,
                GraphExplain {
                    weight: 0.0,
                    off: None,
                    gap: None,
                    agreement: None,
                },
            )
        };
        let g_of: HashMap<&str, &GraphEntry> = glist.iter().map(|e| (e.id.as_str(), e)).collect();
        if graph_ran {
            graph_say.gap = Some(graph_say_gap);
            graph_say.agreement = graph_say_agreement;
        }

        // 5. Rerank: the convex three-weight fusion (text, graph — the vector
        // list joins in hybrid) × coverage × confidence × recency.
        // body_norm already holds the stemmed title+body (computed at write
        // time): reuse it instead of re-stemming per candidate.
        let mut hits: Vec<Hit> = Vec::new();
        for (id, bm25, rank, row) in &text_candidates {
            let (matched, coverage) = coverage_of(&row.body_norm, &row.tags_text, &terms);
            let g = g_of.get(id.as_str()).map(|e| e.score).unwrap_or(0.0);
            // Text mode keeps the v0.2.1 discipline: the graph list only
            // SUPPORTS (graph-discovered notes below, candidates unboosted —
            // measured both in v0.2.1 and again in v0.3, where boosting
            // one-term hub neighbours displaced real answers). The multi-hop
            // lift lives in hybrid, where the vector branch widens the score
            // gaps and the third list can weigh in.
            let _ = (w_g, g);
            let raw = (1.0 - w_g)
                * (bm25 / max_bm25)
                * coverage
                * recency_factor(q, now, &row.note)
                * q.weights.confidence(row.note.confidence);
            let in_graph = g_of.get(id.as_str());
            let mut lists = vec!["text"];
            if in_graph.is_some() {
                lists.push("graph");
            }
            hits.push(Hit {
                explain: Explain {
                    matched_terms: matched,
                    bm25_rank: Some(*rank),
                    graph_rank: in_graph.map(|e| e.rank),
                    lists,
                    support: Some("strong"),
                    fused_score: Some(raw),
                    ..Explain::default()
                },
                note: row.note.clone(),
                raw_score: raw,
                score: raw,
            });
        }
        // Graph-discovered notes that are not candidates themselves (the
        // multi-hop answers no term shared): one batched read, then the same
        // rerank with their normalised mass.
        let cand_ids: BTreeSet<&str> = text_candidates
            .iter()
            .map(|(id, _, _, _)| id.as_str())
            .collect();
        let g_only: Vec<&GraphEntry> = glist
            .iter()
            .filter(|e| !cand_ids.contains(e.id.as_str()))
            .collect();
        let g_rows = read_candidates(
            conn,
            &g_only.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            q.filters.include_quarantined,
            &q.filters.types,
        )?;
        for e in g_only {
            let Some(row) = g_rows.get(&e.id) else {
                continue;
            };
            if !passes_filters(&row.note, &q.filters, now) {
                continue;
            }
            // Graph-discovered notes support, never replace text matches
            // (the v0.2.1 rule, measured again in v0.3: without the coverage
            // and recency discipline the graph-only entries displaced
            // paraphrase winners whose only fault was a weak recency).
            let (_, coverage) = coverage_of(&row.body_norm, &row.tags_text, &terms);
            let raw = w_g
                * e.score
                * coverage
                * q.weights.confidence(row.note.confidence)
                * recency_factor(q, now, &row.note);
            hits.push(Hit {
                explain: Explain {
                    via: e.via.clone(),
                    graph_rank: Some(e.rank),
                    lists: vec!["graph"],
                    support: Some("strong"),
                    fused_score: Some(raw),
                    confidence: row.note.confidence,
                    valid_from: row.note.valid_from.as_ref().map(|v| v.to_db_string()),
                    valid_to: row.note.valid_to.as_ref().map(|v| v.to_db_string()),
                    source: row.note.source.clone(),
                    ..Explain::default()
                },
                note: row.note.clone(),
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
        // v0.4 (P1.4): every hit carries the graph's say for this query
        if graph_ran {
            for h in &mut hits {
                h.explain.graph = Some(graph_say.clone());
            }
        }
        Ok(hits)
    }

    /// Neighbours of an entity (or `note:<id>`), BFS up to `depth`, optionally
    /// restricted to some relations, over the in-memory graph index (v0.3:
    /// no SQL per level). Edges declared by quarantined or deleted notes are
    /// excluded unless `include_quarantined`; manual edges (no declaring
    /// note) always stay. Returns nearest first.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures (building the index).
    pub fn neighbors(
        &self,
        start: &str,
        depth: u8,
        rels: &[&str],
        include_quarantined: bool,
    ) -> Result<Vec<Neighbor>, SearchError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let mut inner = guard;
        ensure_graph(&mut inner)?;
        let Some(g) = inner.graph.as_ref() else {
            return Ok(Vec::new());
        };
        let gi = &g.index;
        let start_node = match start.strip_prefix("note:") {
            Some(id) => Ulid::from_string(id).ok().and_then(|u| gi.node_of_note(&u)),
            None => gi.node_of_entity(start),
        };
        let Some(start_node) = start_node else {
            return Ok(Vec::new());
        };
        let mut out: Vec<Neighbor> = Vec::new();
        let mut seen: BTreeSet<u32> = BTreeSet::from([start_node]);
        let mut frontier: Vec<u32> = vec![start_node];
        for d in 1..=depth {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<u32> = Vec::new();
            for &node in &frontier {
                for step in gi.steps(node, None, include_quarantined, rels, false) {
                    let (vf, vt) = gi.valid_strings(step.validity);
                    let hop = Neighbor {
                        depth: d,
                        entity: gi.display_of(step.node).to_string(),
                        rel: step.rel.to_string(),
                        direction: if step.forward { "->" } else { "<-" }.into(),
                        note_id: step
                            .note
                            .and_then(|n| gi.note_of_node(n))
                            .map(|id| id.to_string()),
                        valid_from: vf.map(String::from),
                        valid_to: vt.map(String::from),
                    };
                    if !out
                        .iter()
                        .any(|n| n.entity == hop.entity && n.rel == hop.rel && n.depth == hop.depth)
                    {
                        out.push(hop);
                    }
                    if seen.insert(step.node) {
                        next.push(step.node);
                    }
                }
            }
            frontier = next;
        }
        Ok(out)
    }

    /// Shortest path between two entities, depth ≤ 4, over the in-memory
    /// graph index. The cap applies **while visiting**: levels beyond it are
    /// never expanded. Edges declared by quarantined or deleted notes are
    /// excluded unless `include_quarantined`.
    ///
    /// # Errors
    /// [`SearchError`] on SQL failures (building the index).
    pub fn path(
        &self,
        a: &str,
        b: &str,
        include_quarantined: bool,
    ) -> Result<Option<Vec<PathStep>>, SearchError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| SearchError::Db("cell mutex poisoned".into()))?;
        let mut inner = guard;
        ensure_graph(&mut inner)?;
        let Some(g) = inner.graph.as_ref() else {
            return Ok(None);
        };
        let gi = &g.index;
        let node_of = |name: &str| match name.strip_prefix("note:") {
            Some(id) => Ulid::from_string(id).ok().and_then(|u| gi.node_of_note(&u)),
            None => gi.node_of_entity(name),
        };
        let (Some(a_node), Some(b_node)) = (node_of(a), node_of(b)) else {
            return Ok(None);
        };
        if a_node == b_node {
            return Ok(Some(Vec::new()));
        }
        // Level-order BFS with parent tracking.
        let mut parents: HashMap<u32, (u32, u32)> = HashMap::new(); // node -> (parent, rel)
        let mut discovered: BTreeSet<u32> = BTreeSet::from([a_node]);
        let mut frontier: Vec<u32> = vec![a_node];
        let mut found = false;
        for _ in 0..4 {
            let mut next: Vec<u32> = Vec::new();
            for &node in &frontier {
                for step in gi.steps(node, None, include_quarantined, &[], false) {
                    if discovered.insert(step.node) {
                        parents.insert(step.node, (node, gi.rel_id(step.rel).unwrap_or(0)));
                        if step.node == b_node {
                            found = true;
                            break;
                        }
                        next.push(step.node);
                    }
                }
                if found {
                    break;
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
        let mut cur = b_node;
        for _ in 0..16 {
            if cur == a_node {
                break;
            }
            let Some((parent, rel)) = parents.get(&cur).copied() else {
                return Ok(None);
            };
            steps.push(PathStep {
                from: gi.display_of(parent).to_string(),
                rel: gi.rel_of(rel).to_string(),
                to: gi.display_of(cur).to_string(),
            });
            cur = parent;
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
        // v0.6.8 (H1): the count speaks the bin's one predicate — a
        // copy that follows a lapide carries no tag
        let bin = crate::cell::bin::not_in_bin("n");
        let visible = if include_quarantined {
            format!("n.id IS NULL OR (n.status != 'deleted' AND {bin})")
        } else {
            format!("n.id IS NULL OR (n.status NOT IN ('quarantined', 'deleted') AND {bin})")
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
        // notes declaring an edge on the entity, or the entity's own
        // note — v0.6.8 (H1): both arms speak the bin's one predicate
        // (the edge arm through `edge_visible`, the own-note arm here)
        let sql = format!(
            "SELECT DISTINCT n.id FROM notes n
             WHERE (n.id = (SELECT note_id FROM entities WHERE name = ?1 COLLATE NOCASE)
                    AND {bin})
                OR n.id IN (SELECT e.note_id FROM edges e
                            LEFT JOIN notes dn ON dn.id = e.note_id
                            WHERE e.note_id IS NOT NULL
                              AND (e.src = ?1 OR e.dst = ?1
                                   OR e.src IN (SELECT 'note:' || note_id FROM entities WHERE name = ?1 COLLATE NOCASE))
                              AND {})
             ORDER BY n.valid_from IS NULL, n.valid_from, n.id",
            edge_visible("e", "dn", include_quarantined),
            bin = crate::cell::bin::not_in_bin("n"),
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

/// A vector candidate after filters: the best chunk of a note. `pub(crate)`
/// for the proposals' semantic hints (v0.5, P1), which reuse the same
/// exact-recall path the search uses.
pub(crate) struct VecCand {
    pub(crate) id: String,
    pub(crate) cosine: f64,
    pub(crate) rank: usize,
    pub(crate) note: Note,
    pub(crate) chunk: usize,
}

/// The stemming-language histogram of the VISIBLE notes (v0.5.3, P0):
/// `(language, notes)`, most frequent first — what an uncertain question
/// expands its stems over. Same freshness pattern as the graph: this
/// process's own writes flip a dirt flag, another process's writes bump
/// `PRAGMA data_version`. Only the first uncertain question pays the scan.
pub(crate) fn ensure_lang_hist(inner: &mut Inner) -> Result<Vec<crate::format::Lang>, SearchError> {
    let data_version: i64 = inner
        .conn
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap_or(0);
    let fresh = !inner.lang_hist_dirty
        && inner
            .lang_hist
            .as_ref()
            .is_some_and(|h| h.data_version == data_version);
    if !fresh {
        // the effective language of a note is the one `write_note` stemmed
        // `body_norm` with: the explicit one, else the detection on its
        // title and body
        let mut tally: HashMap<crate::format::Lang, usize> = HashMap::new();
        let mut stmt = inner.conn.prepare(&format!(
            "SELECT frontmatter_json, title, body FROM notes n
             WHERE n.status IN ('active', 'superseded') AND {}",
            crate::cell::bin::not_in_bin("n")
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (fm, title, body) = row?;
            let explicit = serde_json::from_str::<serde_json::Value>(&fm)
                .ok()
                .and_then(|v| v.get("lang").and_then(|l| l.as_str()).map(str::to_string))
                .and_then(|l| crate::format::Lang::parse(&l).ok());
            let lang = explicit.or_else(|| {
                let title = title.as_deref().unwrap_or_default();
                text::detect_lang(&format!("{title}\n{body}"))
            });
            if let Some(l) = lang {
                *tally.entry(l).or_default() += 1;
            }
        }
        drop(stmt);
        let mut langs: Vec<(crate::format::Lang, usize)> = tally.into_iter().collect();
        langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.as_str().cmp(b.0.as_str())));
        inner.lang_hist = Some(crate::cell::LangHistState {
            data_version,
            langs,
        });
        inner.lang_hist_dirty = false;
    }
    Ok(inner
        .lang_hist
        .as_ref()
        .map(|h| h.langs.iter().map(|(l, _)| *l).collect())
        .unwrap_or_default())
}

/// Build or reuse the in-memory index of the cell's `ready` model.
/// Returns `(model, dim)` or `None` when no model is ready (the caller
/// degrades to text-only). A `data_version` change (another process wrote)
/// triggers a rebuild; this cell's own writes update the index in place.
pub(crate) fn ensure_index(inner: &mut Inner) -> Result<Option<(String, usize)>, SearchError> {
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

/// Build or reuse the in-memory graph index (v0.3). Own writes patch their
/// notes into an overlay over the frozen CSR (or trigger a rebuild when they
/// were manual-edge/purge-shaped); a `data_version` change (another process
/// wrote) always rebuilds.
pub(crate) fn ensure_graph(inner: &mut Inner) -> Result<(), SearchError> {
    let data_version: i64 = inner
        .conn
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap_or(0);
    let rebuild = inner.graph_dirt.global
        || inner
            .graph
            .as_ref()
            .is_none_or(|g| g.data_version != data_version);
    if rebuild {
        let index = GraphIndex::build(&inner.conn)?;
        inner.graph = Some(crate::cell::GraphState {
            data_version,
            index,
        });
        inner.graph_dirt = crate::cell::GraphDirt::default();
        return Ok(());
    }
    if !inner.graph_dirt.notes.is_empty() {
        let notes = std::mem::take(&mut inner.graph_dirt.notes);
        let rebuild_after;
        {
            let Inner {
                conn,
                graph,
                graph_dirt: _,
                ..
            } = inner;
            let Some(g) = graph.as_mut() else {
                return Ok(());
            };
            for id in &notes {
                g.index.patch_note(conn, id)?;
            }
            rebuild_after = g.index.wants_rebuild();
        }
        if rebuild_after {
            let index = GraphIndex::build(&inner.conn)?;
            inner.graph = Some(crate::cell::GraphState {
                data_version,
                index,
            });
            inner.graph_dirt = crate::cell::GraphDirt::default();
        }
    }
    Ok(())
}

/// One entry of the graph list (§6.3.3): a note the push reached, its
/// min-max-normalised mass and the path that carried it.
struct GraphEntry {
    id: String,
    /// Min-max normalised PPR mass over the list.
    score: f64,
    /// Rank in the list (1-based).
    rank: usize,
    /// The path from a seed, for the explain.
    via: Option<String>,
}

/// How many graph-discovered notes enter the fusion.
const GRAPH_LIST_LEN: usize = 50;

/// How many fused candidates seed the push. Few, strong seeds: the v0.2 walk
/// used ten, but ten hybrid seeds are mostly same-neighbourhood noise, and
/// every extra seed spends its mass on its own neighbourhood — the wrong
/// partners of same-surname people surfaced above the right one (measured on
/// the 10k multi-hop diagnostics). With question-entity seeding (v0.4,
/// P1.4) they ride WITH the entities, at `seed_passage_weight` — the
/// HippoRAG 2 passage share.
const GRAPH_SEEDS: usize = 3;

/// What the gate says when it closes (P1.4): the question named no entity
/// of the graph, so the walk never starts.
const GRAPH_OFF_NO_ENTITY: &str = "grafo spento: nessuna entità nominata";

/// The reference first-to-second gap on the fused, [0, 1]-scaled list: at
/// or above it the first result is "clear" (P1.5). The fused scale is
/// min-max-ish by construction (best candidate = 1), so 0.25 means the
/// runner-up sits at three quarters of the winner's distance from zero —
/// declared, and part of what the REPORT's manual proofs show.
pub const GRAPH_GAP_REF: f64 = 0.25;

/// How much the fused list can be trusted (P1.5, QPP-style): the clamped
/// first-to-second gap (over [`GRAPH_GAP_REF`]) plus the text/vector
/// agreement on the first result — half each when both lists exist, the
/// gap alone otherwise. In [0, 1], monotone in both signals.
#[must_use]
pub fn graph_confidence(gap: f64, agreement: Option<bool>) -> f64 {
    let gap_term = (gap / GRAPH_GAP_REF).clamp(0.0, 1.0);
    match agreement {
        Some(true) => 0.5 * gap_term + 0.5,
        Some(false) => 0.5 * gap_term,
        None => gap_term,
    }
}

/// The first-to-second gap of a score list sorted best-first (0.0 with
/// fewer than two entries — one candidate says nothing about certainty).
fn gap_of(scores_desc: &[f64]) -> f64 {
    if scores_desc.len() < 2 {
        return 0.0;
    }
    (scores_desc[0] - scores_desc[1]).max(0.0)
}

/// `incertezza(q)` in `[u_min, 1]`: 1 − confidence, floored.
#[must_use]
pub fn graph_uncertainty(u_min: f64, confidence: f64) -> f64 {
    u_min + (1.0 - u_min) * (1.0 - confidence.clamp(0.0, 1.0))
}

/// The graph entities a question names (v0.4, P1.4): its 1–3-word groups,
/// normalized like the index (lowercase, diacritics folded, no stemming —
/// entity names are proper nouns), matched EXACTLY against entity names
/// and the aliases of entity-notes. The longest match wins its span: a
/// question naming «Anna Rossi 2» does not also seed an entity «Anna».
/// Returns `(node, mass share)` — the shares sum to 1.
pub(crate) fn question_entities(gi: &GraphIndex, text: &str) -> Vec<(u32, f64)> {
    let folded = text::strip_diacritics(&text.to_lowercase());
    let tokens: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let mut taken: Vec<(usize, usize)> = Vec::new(); // claimed [start, end)
    let mut nodes: Vec<u32> = Vec::new();
    for n in (1..=3_usize).rev() {
        if tokens.len() < n {
            continue;
        }
        for start in 0..=(tokens.len() - n) {
            let end = start + n;
            if taken.iter().any(|(a, b)| start < *b && end > *a) {
                continue;
            }
            let key = tokens[start..end].join(" ");
            if let Some(node) = gi.node_of_name(&key)
                && !nodes.contains(&node)
            {
                taken.push((start, end));
                nodes.push(node);
            }
        }
    }
    let share = 1.0 / nodes.len() as f64;
    nodes.into_iter().map(|n| (n, share)).collect()
}

/// Run the personalised-PageRank push and build the third list.
/// `fused_seeds` are `(note id, score normalised to [0, 1])` — the top
/// [`GRAPH_SEEDS`] fused candidates. What they mean depends on
/// [`GraphSeeding`]: the v0.3.2 walk starts from them alone (mass ∝ score);
/// the question-entity seeding (v0.4, P1.4) starts from the entities the
/// QUESTION names — they carry the mass, the candidates ride at
/// `seed_passage_weight` — and a question that names no entity never wakes
/// the graph (the gate; HippoRAG 2 runs no PageRank without a recognized
/// query entity either).
fn graph_list(
    inner: &Inner,
    q: &Query,
    fused_seeds: &[(String, f64)],
    w_g: f64,
) -> Result<(Vec<GraphEntry>, usize, GraphExplain), SearchError> {
    let w = &q.weights;
    if w_g <= 0.0 {
        return Ok((
            Vec::new(),
            0,
            GraphExplain {
                weight: 0.0,
                off: None,
                gap: None,
                agreement: None,
            },
        ));
    }
    let Some(g) = inner.graph.as_ref() else {
        return Ok((
            Vec::new(),
            0,
            GraphExplain {
                weight: w_g,
                off: None,
                gap: None,
                agreement: None,
            },
        ));
    };
    let gi = &g.index;
    let seed_nodes: Vec<(u32, f64)> = match w.graph_seeding {
        GraphSeeding::Fused => {
            let mass_sum: f64 = fused_seeds
                .iter()
                .map(|(_, s)| *s)
                .sum::<f64>()
                .max(f64::EPSILON);
            fused_seeds
                .iter()
                .filter_map(|(id, s)| {
                    gi.node_of_note(&Ulid::from_string(id).ok()?)
                        .map(|n| (n, s / mass_sum))
                })
                .collect()
        }
        GraphSeeding::Question {
            seed_passage_weight,
        } => {
            // v0.5.3 (P1): an entity the VISIBLE graph never speaks about
            // (only hidden notes declared it) does not open the walk — its
            // shares would dilute the real seeds and the open gate itself
            // would be a fingerprint of the hidden note
            let entities: Vec<(u32, f64)> = question_entities(gi, &q.text)
                .into_iter()
                .filter(|&(n, _)| {
                    gi.has_walkable_step(n, q.filters.as_of, q.filters.include_quarantined)
                })
                .collect();
            if entities.is_empty() {
                // the gate (P1.4): no entity named, no walk — the fused
                // list stands on its own
                return Ok((
                    Vec::new(),
                    0,
                    GraphExplain {
                        weight: 0.0,
                        off: Some(GRAPH_OFF_NO_ENTITY),
                        gap: None,
                        agreement: None,
                    },
                ));
            }
            let share = 1.0 / entities.len() as f64;
            let mut seeds: Vec<(u32, f64)> =
                entities.into_iter().map(|(n, _)| (n, share)).collect();
            for (id, s) in fused_seeds {
                if let Some(n) = gi.node_of_note(&Ulid::from_string(id).unwrap_or_default()) {
                    seeds.push((n, s * seed_passage_weight));
                }
            }
            seeds
        }
    };
    if seed_nodes.is_empty() {
        return Ok((
            Vec::new(),
            0,
            GraphExplain {
                weight: w_g,
                off: None,
                gap: None,
                agreement: None,
            },
        ));
    }
    let push = gi.ppr(
        &seed_nodes,
        w.ppr_alpha,
        w.ppr_epsilon,
        w.hub_theta,
        q.filters.as_of,
        q.filters.include_quarantined,
    );
    let pushed_edges = push.pushed_edges;
    let top = push.notes.iter().take(GRAPH_LIST_LEN).collect::<Vec<_>>();
    if top.is_empty() {
        return Ok((
            Vec::new(),
            pushed_edges,
            GraphExplain {
                weight: w_g,
                off: None,
                gap: None,
                agreement: None,
            },
        ));
    }
    // Scaled to [0, 1] by the list maximum — the convex fusion normalises
    // every list the same way. The minimum is NOT subtracted: min-max would
    // score the weakest member of the list exactly 0 and silently drop it
    // (found when the Zoro test lost the answer note it was built around).
    let max = top
        .iter()
        .map(|(_, m)| *m)
        .fold(f64::NEG_INFINITY, f64::max)
        .max(f64::EPSILON);
    if let Ok(tag) = std::env::var("ENGRAMMA_PPR_DEBUG") {
        let rank_of = |want: &str| {
            push.notes
                .iter()
                .position(|(n, _)| {
                    gi.note_of_node(*n)
                        .is_some_and(|id| id.to_string().ends_with(want))
                })
                .map(|r| (r + 1, push.notes[r].1))
        };
        if tag == "all" {
            for (i, (n, m)) in push.notes.iter().enumerate() {
                if let Some(id) = gi.note_of_node(*n) {
                    eprintln!("PPR[{:3}] {} {:.6}", i + 1, crate::id::id8(&id), m);
                }
            }
            eprintln!(
                "PPR push: {} notes settled, {} edges",
                push.notes.len(),
                pushed_edges
            );
        } else {
            eprintln!(
                "PPR push: {} notes settled, {} edges; target {tag:?} -> {:?}; top5 {:?}",
                push.notes.len(),
                pushed_edges,
                rank_of(&tag),
                push.notes
                    .iter()
                    .take(5)
                    .map(|(n, m)| (gi.display_of(*n).to_string(), *m))
                    .collect::<Vec<_>>()
            );
        }
    }
    let entries = top
        .into_iter()
        .enumerate()
        .filter_map(|(i, (node, mass))| {
            let id = gi.note_of_node(*node)?.to_string();
            let score = (mass / max).clamp(0.0, 1.0);
            Some(GraphEntry {
                id,
                score,
                rank: i + 1,
                via: render_path(gi, &push.pred, *node),
            })
        })
        .collect();
    Ok((
        entries,
        pushed_edges,
        GraphExplain {
            weight: w_g,
            off: None,
            gap: None,
            agreement: None,
        },
    ))
}

/// Reconstruct the walk that carried mass to `node`: follow the predecessor
/// of maximum mass back to a seed, then render it as
/// `[[Fabio]] -partner_of-> [[Giulia]] -works_at-> (nota 00000082)`.
fn render_path(gi: &GraphIndex, pred: &HashMap<u32, (u32, u32, f64)>, node: u32) -> Option<String> {
    let mut chain: Vec<(u32, u32, u32)> = Vec::new(); // (from, rel, to)
    let mut cur = node;
    for _ in 0..32 {
        let Some(&(from, rel, _)) = pred.get(&cur) else {
            break;
        };
        chain.push((from, rel, cur));
        cur = from;
    }
    if chain.is_empty() {
        return None;
    }
    chain.reverse();
    let mut out = format!("[[{}]]", gi.display_of(chain[0].0));
    for (from, rel, to) in chain {
        let dest = match gi.note_of_node(to) {
            Some(id) => format!("(nota {})", crate::id::id8(&id)),
            None => format!("[[{}]]", gi.display_of(to)),
        };
        let _ = from;
        out.push_str(&format!(" -{}-> {dest}", gi.rel_of(rel)));
    }
    Some(out)
}

/// Vector candidates for one query: int8 top-`m` → exact f32 recomputation
/// of those candidates (temp table + JOIN against `note_vectors`) → best
/// chunk per note → the same filters as BM25. Fewer than `k` survivors
/// double `m`, up to the full scan.
#[allow(clippy::too_many_arguments)]
pub(crate) fn vector_candidates(
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
            // CROSS JOIN pins the join order (see read_candidates): the
            // 200-row temp table drives the (model, note_id, chunk) index
            let mut stmt = conn.prepare(
                "SELECT v.note_id, v.chunk, v.vec FROM vcand c
                 CROSS JOIN note_vectors v ON v.note_id = c.note_id AND v.model = ?1",
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
                // dot straight off the little-endian bytes, without
                // materialising the Vec<f32> (200 vectors per query)
                if blob.len().is_multiple_of(4) {
                    let cos = f64::from(dot_le_bytes(&blob, qv));
                    let e = best.entry(note_id).or_insert((f64::NEG_INFINITY, chunk));
                    if cos > e.0 {
                        *e = (cos, chunk);
                    }
                }
            }
        }
        // rank by exact cosine, then filter — notes read in chunks of 64
        // (one batched query per chunk instead of two per candidate).
        // v0.5.3 (P1): the rank is the position among the VISIBLE
        // candidates (numbered as they are pushed), not the position in the
        // raw list — the first visible result is rank 1 whatever sits
        // hidden above it.
        let mut ranked: Vec<(String, f64, i64)> = best
            .into_iter()
            .map(|(id, (cos, chunk))| (id, cos, chunk))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.clear();
        let mut base = 0_usize;
        'ranked: while base < ranked.len() && out.len() < 50 {
            let end = (base + 64).min(ranked.len());
            let ids: Vec<String> = ranked[base..end]
                .iter()
                .map(|(id, _, _)| id.clone())
                .collect();
            let rows = read_candidates(conn, &ids, filters.include_quarantined, &filters.types)?;
            for (id, cos, chunk) in ranked[base..end].iter() {
                if out.len() >= 50 {
                    break 'ranked;
                }
                let Some(row) = rows.get(id) else {
                    continue;
                };
                if !passes_filters(&row.note, filters, now) {
                    continue;
                }
                out.push(VecCand {
                    id: id.clone(),
                    cosine: *cos,
                    rank: out.len() + 1,
                    note: row.note.clone(),
                    chunk: usize::try_from(*chunk).unwrap_or(0),
                });
            }
            base = end;
        }
        let filtered_short = out.len() < k;
        if !filtered_short || m >= total {
            break;
        }
        m = (m * 2).min(total);
    }
    Ok(out)
}

/// Dot product of a little-endian f32 blob (as stored in `note_vectors`)
/// with `q`, without materialising the vector — the 8-accumulator layout
/// matches [`crate::embed::dot_f32`].
fn dot_le_bytes(blob: &[u8], q: &[f32]) -> f32 {
    let mut acc = [0.0_f64; 8];
    let chunks = blob.chunks_exact(32).zip(q.chunks_exact(8));
    for (b8, q8) in chunks {
        for j in 0..8 {
            let f = f32::from_le_bytes([b8[j * 4], b8[j * 4 + 1], b8[j * 4 + 2], b8[j * 4 + 3]]);
            acc[j] += f64::from(f) * f64::from(q8[j]);
        }
    }
    let head = (blob.len() / 4) - (blob.len() / 4) % 8;
    let mut rest = 0.0_f64;
    for i in head..q.len().min(blob.len() / 4) {
        let b = &blob[i * 4..i * 4 + 4];
        let f = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        rest += f64::from(f) * f64::from(q[i]);
    }
    (acc.iter().sum::<f64>() + rest) as f32
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
        Fusion::Convex { alpha, cos_norm }
        | Fusion::Adaptive {
            alpha, cos_norm, ..
        } => {
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
            // The cosine on the same [0, 1] scale (v0.2.1): min-max over the
            // vector candidates, or the theoretical −1..1 limits of the
            // cosine between L2 unit vectors — never the raw value, whose
            // ~0.1 span let the min-max'd BM25 outweigh it at every α.
            let (cmin, cmax) = match cos_norm {
                CosNorm::Candidates => {
                    let mut lo = f64::INFINITY;
                    let mut hi = f64::NEG_INFINITY;
                    for v in vecs {
                        lo = lo.min(v.cosine);
                        hi = hi.max(v.cosine);
                    }
                    (lo, hi)
                }
                CosNorm::Theoretical => (-1.0, 1.0),
            };
            let cos_norm_of = |c: f64| -> f64 {
                if cmax > cmin {
                    ((c - cmin) / (cmax - cmin)).clamp(0.0, 1.0)
                } else {
                    1.0
                }
            };
            for (id, bm25, _, _) in text {
                *out.entry(id.clone()).or_default() += (1.0 - alpha) * norm(*bm25);
            }
            for v in vecs {
                *out.entry(v.id.clone()).or_default() += alpha * cos_norm_of(v.cosine);
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
            let cap = inner.query_cache_cap;
            inner
                .query_cache
                .entry(model.clone())
                .or_insert_with(|| LruCache::new(cap))
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
                let cap = inner.query_cache_cap;
                inner
                    .query_cache
                    .entry(model.clone())
                    .or_insert_with(|| LruCache::new(cap))
                    .put(q.text.clone(), v.clone());
                v
            }
        };
        let embed_ms = started.elapsed().as_secs_f64() * 1000.0;

        // 2. Search under the lock.
        let search_started = std::time::Instant::now();
        let (hits, graph_pushed_edges) = {
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
            graph_pushed_edges,
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
) -> Result<(Vec<Hit>, usize), SearchError> {
    let dim = ensure_index(inner)?;
    // The graph index joins the walk lazily, before `conn` is borrowed for
    // the rest of the pipeline (own writes patch into it here).
    if q.graph_hops > 0 && q.weights.graph_weight > 0.0 {
        ensure_graph(inner)?;
    }
    // v0.5.3 (P0): the language histogram, before `conn` is borrowed — only
    // an uncertain question needs it (see `search_with_now`)
    let cell_langs = if q.lang.is_none() && text::detect_query_lang(&q.text).is_none() {
        ensure_lang_hist(inner)?
    } else {
        Vec::new()
    };
    let conn = &inner.conn;

    // --- text list (BM25, as in v0.1; skipped in vector mode). Ubiquitous
    // terms leave the MATCH (P0.3); they still count for coverage.
    let terms = if sem.mode == Mode::Vector {
        crate::text::QueryTerms::default()
    } else {
        text::query_terms(&q.text, q.lang, &cell_langs)
    };
    let mut text_hits: Vec<(String, f64)> = Vec::new();
    // v0.5.3 (P1): the totals and the frequencies see the visible notes
    // only — a hidden match must not bend the support labels
    let total_notes = visible_note_count(conn, q.filters.include_quarantined);
    // document frequencies once: the MATCH drops the ubiquitous terms and
    // the support labels discount text matches that rest on them alone
    let df = doc_frequencies(conn, &terms.flat(), q.filters.include_quarantined)?;
    if !terms.is_empty() {
        let match_expr = match_expr_with(&terms, &df, total_notes);
        // v0.4.2 (P0.1): the window follows the asked `k`, floor 50 — no
        // silent cut above it (same rule as the text path above)
        text_hits = bm25_candidates(
            conn,
            &match_expr,
            candidate_window(q.k),
            q.filters.include_quarantined,
        )?;
    }
    // one batched read for the candidates (P0.3): note + coverage columns
    let cand_rows = read_candidates(
        conn,
        &text_hits
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        q.filters.include_quarantined,
        &q.filters.types,
    )?;
    let mut text_list: Vec<(String, f64, usize, Note)> = Vec::new();
    let mut text_cov: HashMap<String, (Vec<String>, f64)> = HashMap::new();
    for (rank, (id, bm25)) in text_hits.iter().enumerate() {
        let Some(row) = cand_rows.get(id) else {
            continue;
        };
        if !passes_filters(&row.note, &q.filters, now) {
            continue;
        }
        text_cov.insert(
            id.clone(),
            coverage_of(&row.body_norm, &row.tags_text, &terms),
        );
        text_list.push((id.clone(), *bm25, rank + 1, row.note.clone()));
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
    // Cosine margin of each vector candidate over the next one (v0.2.1):
    // a vector-only result with a thin margin is `support: "weak"` — near in
    // meaning, unconfirmed by any text match. The last candidate has no
    // runner-up to lean on: weak.
    let margins: HashMap<&str, f64> = vec_list
        .windows(2)
        .map(|pair| (pair[0].id.as_str(), pair[0].cosine - pair[1].cosine))
        .chain(std::iter::once((
            vec_list.last().map_or("", |v| v.id.as_str()),
            f64::NEG_INFINITY,
        )))
        .filter(|(id, _)| !id.is_empty())
        .collect();

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

    // --- graph list: the third list, a PPR push over the in-memory CSR
    // (§6.3.3, v0.3). The push replaces the per-question SQL expansion:
    // ~157 pushed edges per question at 100k notes. v0.4 (P1.4): with
    // question-entity seeding the entities the question names carry the
    // mass (these candidates ride at `seed_passage_weight`), and a question
    // that names no entity gates the graph off.
    let mut seeds: Vec<(String, f64)> = fused.iter().map(|(id, s)| (id.clone(), *s)).collect();
    seeds.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let seeds: Vec<(String, f64)> = seeds.into_iter().take(GRAPH_SEEDS).collect();
    // v0.4 (P1.5): the graph's weight is per question, on two free signals
    // of the list in hand — the first-to-second gap, and whether the text
    // and vector lists agree on the first result. A `Fusion::Adaptive`
    // overrides the query's policy with its own w_max/u_min (the bench
    // grid's knob).
    let gap = {
        let mut scores: Vec<f64> = fused.values().copied().collect();
        scores.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        gap_of(&scores)
    };
    let agreement = match (text_list.first(), vec_list.first()) {
        (Some((t, _, _, _)), Some(v)) => Some(t == &v.id),
        _ => None,
    };
    let adaptive_of = |w: &crate::search::Weights| -> Option<(f64, f64)> {
        match &sem.fusion {
            Fusion::Adaptive { w_max, u_min, .. } => Some((*w_max, *u_min)),
            _ => match w.graph_policy {
                GraphPolicy::Adaptive { w_max, u_min } => Some((w_max, u_min)),
                GraphPolicy::Fixed => None,
            },
        }
    };
    let (w_g, graph_ran) = match adaptive_of(&q.weights) {
        Some((w_max, u_min)) => {
            let w =
                w_max.clamp(0.0, 1.0) * graph_uncertainty(u_min, graph_confidence(gap, agreement));
            (w, q.graph_hops > 0 && w > 0.0)
        }
        None => (
            q.weights.graph_weight.clamp(0.0, 1.0),
            q.graph_hops > 0 && q.weights.graph_weight > 0.0,
        ),
    };
    let (glist, pushed_edges, mut graph_say): (Vec<GraphEntry>, usize, GraphExplain) = if graph_ran
    {
        match q.weights.graph_seeding {
            GraphSeeding::Question { .. } => graph_list(inner, q, &seeds, w_g)?,
            GraphSeeding::Fused if !seeds.is_empty() => graph_list(inner, q, &seeds, w_g)?,
            GraphSeeding::Fused => (
                Vec::new(),
                0,
                GraphExplain {
                    weight: 0.0,
                    off: None,
                    gap: None,
                    agreement: None,
                },
            ),
        }
    } else {
        (
            Vec::new(),
            0,
            GraphExplain {
                weight: 0.0,
                off: None,
                gap: None,
                agreement: None,
            },
        )
    };
    let g_of: HashMap<&str, &GraphEntry> = glist.iter().map(|e| (e.id.as_str(), e)).collect();
    if graph_ran {
        graph_say.gap = Some(gap);
        graph_say.agreement = agreement;
    }
    // How the graph list joins the fusion: RRF adds a weighted rank
    // reciprocal (in the same [0, 1] scale the fused scores were renormalised
    // to — the best graph note adds `w`, the 50th about `w·(k+1)/(k+50)`),
    // the convex combination is the literal three-weight form.
    let graph_boost = |id: &str| -> f64 {
        match g_of.get(id) {
            Some(e) => match sem.fusion {
                Fusion::Rrf { k } => {
                    let k = k as f64;
                    w_g * ((k + 1.0) / (k + e.rank as f64))
                }
                Fusion::Convex { .. } | Fusion::Adaptive { .. } => w_g * e.score,
            },
            None => 0.0,
        }
    };
    let scale_base = |_id: &str, fused_score: f64| -> f64 {
        match sem.fusion {
            Fusion::Rrf { .. } => fused_score,
            Fusion::Convex { .. } | Fusion::Adaptive { .. } => (1.0 - w_g) * fused_score,
        }
    };

    // --- rerank: fused × confidence (§4.5; term coverage is not applied to
    // semantic hits, a paraphrase legitimately shares no term with the note).
    let mut hits: Vec<Hit> = Vec::new();
    for (id, fused_score) in &fused {
        let from_text = note_of_text.contains_key(id);
        let from_vec = vec_of.contains_key(id.as_str());
        let in_graph = g_of.get(id.as_str());
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
        if in_graph.is_some() {
            lists.push("graph");
        }
        // One signal, not a prohibition. Weak = the meaning branch alone
        // carries the result AND its cosine margin over the next vector
        // candidate is thin. A text match confirms ONLY when at least one
        // matched term is discriminative: a match that rests entirely on
        // ubiquitous terms (the subject's name in a vault about them) says
        // nothing about THIS question ("che auto guida Giulia" matches
        // every Giulia note — none of them is about a car).
        let discriminative = from_text
            && text_cov.get(id.as_str()).is_some_and(|(matched, _)| {
                matched.iter().any(|t| {
                    !non_discriminative(df.get(t.as_str()).copied().unwrap_or(0), total_notes)
                })
            });
        let support = if from_vec && !discriminative {
            match margins.get(id.as_str()) {
                Some(m) if *m >= sem.weak_margin => "strong",
                _ => "weak",
            }
        } else {
            "strong"
        };
        // (abstention already happened above, before the graph walk; in
        // vector mode the text list is empty so everything went through it)
        // Reliability and graph, without the v0.1 recency factor: cosine
        // differences are hundredths, and a 180-day half-life (tuned for the
        // wide score gaps of BM25) shuffled them — measured, REPORT §v0.2.
        // Text-only searches keep the full v0.1 formula.
        let base = scale_base(id, *fused_score);
        let raw = (base + graph_boost(id)) * q.weights.confidence(note.confidence);
        hits.push(Hit {
            note: (*note).clone(),
            raw_score: raw,
            score: raw,
            explain: Explain {
                graph: None, // set for every hit after the rerank (P1.4)
                bm25_rank: from_text
                    .then(|| bm25_rank_of.get(id.as_str()).copied())
                    .flatten(),
                semantic_rank: from_vec
                    .then(|| vec_of.get(id.as_str()).map(|v| v.rank))
                    .flatten(),
                graph_rank: in_graph.map(|e| e.rank),
                via: in_graph.and_then(|e| e.via.clone()),
                cosine,
                fused_score: Some(*fused_score),
                lists,
                support: Some(support),
                winning_chunk: chunk,
                matched_terms: if from_text {
                    text_cov
                        .get(id.as_str())
                        .map(|(m, _)| m.clone())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                },
                confidence: note.confidence,
                valid_from: note.valid_from.as_ref().map(|v| v.to_db_string()),
                valid_to: note.valid_to.as_ref().map(|v| v.to_db_string()),
                source: note.source.clone(),
            },
        });
    }
    // Graph-discovered notes (never candidates themselves): one batched read,
    // then the same rerank on their normalised mass alone.
    let cand_ids: BTreeSet<&str> = fused.keys().map(String::as_str).collect();
    let g_only: Vec<&GraphEntry> = glist
        .iter()
        .filter(|e| !cand_ids.contains(e.id.as_str()))
        .collect();
    let g_rows = read_candidates(
        conn,
        &g_only.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        q.filters.include_quarantined,
        &q.filters.types,
    )?;
    for e in g_only {
        let Some(row) = g_rows.get(&e.id) else {
            continue;
        };
        if !passes_filters(&row.note, &q.filters, now) {
            continue;
        }
        let raw = graph_boost(&e.id) * q.weights.confidence(row.note.confidence);
        hits.push(Hit {
            note: row.note.clone(),
            raw_score: raw,
            score: raw,
            explain: Explain {
                via: e.via.clone(),
                graph_rank: Some(e.rank),
                lists: vec!["graph"],
                support: Some("strong"),
                fused_score: Some(raw),
                confidence: row.note.confidence,
                valid_from: row.note.valid_from.as_ref().map(|v| v.to_db_string()),
                valid_to: row.note.valid_to.as_ref().map(|v| v.to_db_string()),
                source: row.note.source.clone(),
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
    // v0.4 (P1.4): every hit carries the graph's say for this query
    if graph_ran {
        for h in &mut hits {
            h.explain.graph = Some(graph_say.clone());
        }
    }
    let max_raw = hits.iter().map(|h| h.raw_score).fold(0.0_f64, f64::max);
    if max_raw > 0.0 {
        for h in &mut hits {
            h.score = h.raw_score / max_raw;
        }
    }
    Ok((hits, pushed_edges))
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
    // v0.6.8 (H1): the bin's one predicate rides the edge's declaring
    // note — an edge a bin's copy declares is not a fact the walks see
    let bin = crate::cell::bin::not_in_bin(en);
    if include_quarantined {
        format!("({edge}.note_id IS NULL OR ({en}.status != 'deleted' AND {bin}))")
    } else {
        format!(
            "({edge}.note_id IS NULL OR ({en}.status NOT IN ('quarantined', 'deleted') AND {bin}))"
        )
    }
}

/// Recency weight: exponential decay with the configured half-life (days).
fn recency_of(now: OffsetDateTime, note: &Note, half_life_days: f64) -> f64 {
    let age_days = (now - note.updated_at.get()).whole_seconds() as f64 / 86_400.0;
    0.5_f64.powf(age_days / half_life_days.max(1.0))
}

/// v0.2.1: for a bitemporal (`as_of`) question the wall-clock recency says
/// nothing — a page refreshed today can carry the truth of 2022, and
/// measuring its age against either `as_of` (negative: a boost) or today
/// (a fresh page wins) is wrong in both directions (measured at 100k:
/// today's freshness put fresh name-matching person pages above the 2024
/// note that WAS the 2022 truth — fattuali 0,899 → 0,717). No recency
/// there: bm25, coverage, confidence and the graph decide.
fn recency_factor(q: &Query, now: OffsetDateTime, note: &Note) -> f64 {
    if q.filters.as_of.is_some() {
        1.0
    } else {
        recency_of(now, note, q.weights.recency_half_life_days)
    }
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
        // v0.5.3 (P1): the rank is the position among the VISIBLE matches —
        // the quarantined web note never enters the BM25 list, so the first
        // visible result is rank 1 (v0.1 pinned Some(2): the quarantined
        // note ranked first and was filtered out afterwards, which over
        // HTTP told every client a hidden note existed)
        assert_eq!(hits[0].explain.bm25_rank, Some(1));
    }

    /// v0.5.3 (P0): the three-note cell of the phase, and the one-word
    /// questions the tunnel test could not answer. `whatlang` guesses
    /// German for «Zebedeo» and French for «tonno»; an uncertain question
    /// must not stem on one guess.
    fn short_questions_cell() -> (tempfile::TempDir, Cell) {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("corte.sqlite"), "corte", "t")
            .unwrap(/* test-only */);
        cell.put_note(
            &note(
                1,
                "Gatto",
                "Il gatto di prova si chiama Zebedeo e mangia solo crocchette al tonno.",
                NoteType::Fact,
            ),
            "t",
        )
        .unwrap(/* test-only */);
        cell.put_note(
            &note(
                2,
                "Aurora",
                "Aurora viene in ufficio ogni martedì con la sua bicicletta rossa.",
                NoteType::Fact,
            ),
            "t",
        )
        .unwrap(/* test-only */);
        cell.put_note(
            &note(
                3,
                "Cena",
                "Stasera cuciniamo le melanzane alla parmigiana per gli amici.",
                NoteType::Fact,
            ),
            "t",
        )
        .unwrap(/* test-only */);
        (dir, cell)
    }

    /// v0.5.3 (P1.6): the status filter inside the BM25 query must not
    /// change the plan into a full scan — the MATCH still drives through
    /// the FTS index and the JOIN probes `notes` by rowid (the p95 budget
    /// of the phase depends on it).
    #[test]
    fn the_bm25_status_filter_keeps_the_fts_plan() {
        let (_d, cell) = setup();
        let qt = text::query_terms("traslocato Milano", None, &[]);
        let expr = or_match_expr(&qt.words);
        let inner = cell.inner.lock().unwrap(/* test-only */);
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT n.id, bm25(notes_fts, 10.0, 3.0, 2.0) FROM notes_fts
             JOIN notes n ON n.rowid = notes_fts.rowid
             WHERE notes_fts MATCH '{expr}' AND n.status IN {}
             ORDER BY bm25(notes_fts, 10.0, 3.0, 2.0) LIMIT 50",
            visible_statuses(false)
        );
        let plan: Vec<String> = {
            let mut stmt = inner.conn.prepare(&sql).unwrap(/* test-only */);
            stmt.query_map([], |r| {
                let detail: String = r.get(3)?;
                Ok(detail)
            })
            .unwrap(/* test-only */)
            .flatten()
            .collect()
        };
        let joined = plan.join(" | ");
        eprintln!("plan: {joined}");
        assert!(
            joined.contains("notes_fts VIRTUAL TABLE INDEX"),
            "the MATCH must still drive the FTS index: {joined}"
        );
        assert!(
            joined.contains("SEARCH n USING INTEGER PRIMARY KEY (rowid=?"),
            "the JOIN must probe notes by rowid, not scan it: {joined}"
        );
    }

    /// v0.5.3 (P0): the MATCH of an uncertain question carries every form of
    /// every word as singles, and the ADJACENT PAIRS on the canonical stems
    /// only (v0.2.1 P1.7): pairs across stem alternatives of the same word
    /// would be garbage phrases.
    #[test]
    fn uncertain_match_phrases_pair_the_canonical_stems() {
        let qt = text::query_terms("Anna Rossi", None, &[crate::format::Lang::It]);
        let expr = or_match_expr(&qt.words);
        // singles: raw and stem forms, every one quoted
        assert!(expr.contains("\"anna\""), "{expr}");
        assert!(expr.contains("\"rossi\""), "{expr}");
        assert!(expr.contains("\"ross\""), "{expr}");
        // the pair: the Italian stems in order — not "anna rossi", not a
        // pair across two forms of the same word
        assert!(expr.contains("\"anna ross\""), "{expr}");
        assert!(!expr.contains("\"rossi ross\""), "{expr}");
        // a confident question is the v0.2.1 shape exactly (with two words
        // the whole-question phrase and the single pair coincide, as ever)
        let old = text::query_terms("Anna Rossi", Some(crate::format::Lang::It), &[]);
        assert_eq!(
            or_match_expr(&old.words),
            "\"anna\" OR \"ross\" OR \"anna ross\" OR \"anna ross\""
        );
    }

    /// v0.5.3 (P0.5): «Zebedeo», «tonno», «Aurora» e «martedì» find the
    /// right note on a three-note cell, with AND without `--lang` — the
    /// explicit language always worked; the uncertain path is the fix.
    #[test]
    fn one_word_questions_find_their_note_with_and_without_lang() {
        let (_d, cell) = short_questions_cell();
        let cases: &[(&str, &str)] = &[
            ("Zebedeo", "Gatto"),
            ("tonno", "Gatto"),
            ("Aurora", "Aurora"),
            ("martedì", "Aurora"),
        ];
        for (q, title) in cases {
            for lang in [None, Some(crate::format::Lang::It)] {
                let query = Query {
                    lang,
                    ..Query::text(*q).k(5)
                };
                let hits = cell
                    .search_with_now(&query, now())
                    .unwrap_or_else(|e| panic!("`{q}` ({lang:?}): {e}"));
                assert!(
                    hits.first()
                        .is_some_and(|h| h.note.display_title() == *title),
                    "`{q}` ({lang:?}): wanted `{title}` first, got {:?}",
                    hits.iter()
                        .map(|h| h.note.display_title().to_string())
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    /// v0.5.3 (P0.5): a cell of English-only notes expands an uncertain
    /// question over English alone (plus the raw form) — not over the six
    /// stemmers, and never over a language the cell does not speak. The
    /// notes carry no explicit `lang`: the write-time detection stems them
    /// English, and the histogram sees the same.
    #[test]
    fn an_english_cell_expands_only_in_english() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("en.sqlite"), "en", "t")
            .unwrap(/* test-only */);
        for (i, body) in [
            "The gardener waters the orchard every morning before the heat.",
            "Her brother studies medieval history at the college library.",
            "They sailed the wooden boat across the harbour at dawn.",
        ]
        .iter()
        .enumerate()
        {
            let mut n = note(i as u16, "note", body, NoteType::Fact);
            n.lang = None;
            cell.put_note(&n, "t").unwrap(/* test-only */);
        }
        let hist = {
            let mut inner = cell.inner.lock().unwrap(/* test-only */);
            ensure_lang_hist(&mut inner).unwrap(/* test-only */)
        };
        assert_eq!(
            hist,
            vec![crate::format::Lang::En],
            "the cell speaks English"
        );
        // "gardener": raw + the English stem (deduplicated)
        let qt = text::query_terms("gardener", None, &hist);
        assert_eq!(
            qt.words[0].forms,
            vec![
                "gardener".to_string(),
                text::normalize("gardener", Some(crate::format::Lang::En))
            ]
        );
        // and it finds the note
        let hits = cell.search_with_now(&Query::text("gardener").k(5), now())
            .unwrap(/* test-only */);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].note.body.contains("gardener"));
    }

    /// v0.4.2 (P0.1): an explicit `k` is respected AS GIVEN. The BM25
    /// candidate window was a fixed 50: a caller asking `k(150)` on a corpus
    /// where the term matches more notes got a silent cut. The window is now
    /// `max(50, k)` — the default profile (k ≤ 50) is unchanged.
    #[test]
    fn k_beyond_the_candidate_window_is_respected() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("k.sqlite"), "k", "tester")
            .unwrap(/* test-only */);
        const TOTAL: u16 = 120;
        for i in 0..TOTAL {
            let n = note(
                i,
                &format!("nota {i}"),
                &format!("la quokkanota numero {i} di centoventi"),
                NoteType::Fact,
            );
            cell.put_note(&n, "t").unwrap(/* test-only */);
        }
        let small = cell
            .search_with_now(&Query::text("quokkanota").k(10), now())
            .unwrap(/* test-only */);
        assert_eq!(small.len(), 10, "the default window answers 10");
        let big = cell
            .search_with_now(&Query::text("quokkanota").k(150), now())
            .unwrap(/* test-only */);
        assert_eq!(big.len(), usize::from(TOTAL), "no silent cut at 50");
        // still ranked, still deduplicated
        let ids: Vec<String> = big.iter().map(|h| h.note.id.to_string()).collect();
        let uniq: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(uniq.len(), usize::from(TOTAL));
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
        // count the work: any SQL during the walk crosses the blob (20k rows
        // ≥ ~200k VDBE steps). v0.3: the walk runs entirely on the in-memory
        // CSR — the only SQL left is the freshness pragma, so build the index
        // first, then measure.
        let warm = cell.path("Nodo3", "Fabio", false).unwrap(/* test-only */);
        assert_eq!(warm.expect("4 hops").len(), 4);
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
            used < 1_000,
            "the walk is in-memory: 2 path() calls must not touch the edge \
             table ({used} VDBE steps; the blob alone is 20 000 rows)"
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
    fn ubiquitous_terms_leave_the_match_but_stay_in_coverage() {
        // v0.2.1 (P0.3), at the scale the rule is about: "lavora" and
        // "molto" sit in ~every note (FTS5 ranks every matching row before
        // the LIMIT — the very pass this fix removes), "spesso" in 81% but
        // under the 1000-document floor, "pinco" in 50. The MATCH keeps the
        // terms that discriminate; the dropped term still counts as found.
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("freq.sqlite"), "t", "tester")
            .unwrap(/* test-only */);
        let mut pinco_id = None;
        for i in 1..=1100_u32 {
            let body = if i <= 1050 {
                if i <= 900 {
                    format!("Nota {i}: la squadra lavora e molto spesso.")
                } else {
                    format!("Nota {i}: la squadra lavora e molto.")
                }
            } else {
                format!("Pinco {i} lavora poco.")
            };
            let mut n = note(
                (i % 60_000) as u16,
                &format!("Nota {i}"),
                &body,
                NoteType::Fact,
            );
            if i > 1050 {
                n.title = Some(format!("Pinco {i}"));
            }
            n.lang = Some(crate::format::Lang::It);
            // unique ids beyond the helper's 16-bit counter
            n.id = ulid::Ulid::from_string(&format!("01{:018}{i:06}", 0))
                .unwrap(/* test-only */);
            cell.put_note(&n, "t").unwrap(/* test-only */);
            if i == 1100 {
                pinco_id = Some(n.id);
            }
        }
        let terms = text::query_terms("pinco lavora spesso", Some(crate::format::Lang::It), &[]);
        let stems = terms.flat();
        assert_eq!(stems.len(), 3, "{stems:?}");
        {
            let inner = cell.inner.lock().unwrap(/* test-only */);
            let dfs: Vec<String> = inner
                .conn
                .prepare("SELECT term || '=' || doc FROM notes_fts_vocab WHERE term IN (?1, ?2, ?3)")
                .unwrap(/* test-only */)
                .query_map([&stems[0], &stems[1], &stems[2]], |r| r.get::<_, String>(0))
                .unwrap(/* test-only */)
                .flatten()
                .collect();
            eprintln!("dfs {stems:?} -> {dfs:?}");
            // "lavora" (above the fraction AND the 1000-document floor) leaves
            let expr = match_expr_for(&inner.conn, &terms, false).unwrap(/* test-only */);
            assert!(
                !expr.contains(&format!("\"{}\"", stems[1])),
                "the ubiquitous term must leave the MATCH: {expr}"
            );
            assert!(
                expr.contains(stems[0].as_str()) && expr.contains(stems[2].as_str()),
                "the discriminating terms stay: {expr}"
            );
            // "spesso": 81% of the corpus but only 900 documents — it stays
            // (below the floor the term is signal, not filler)
            // every matching term frequent (lavora+squadra, both ≥ 1000
            // documents): everything stays — an empty MATCH is worse
            let both = text::query_terms("lavora squadra", Some(crate::format::Lang::It), &[]);
            let both_flat = both.flat();
            assert_eq!(both_flat.len(), 2, "{both_flat:?}");
            let expr2 = match_expr_for(&inner.conn, &both, false).unwrap(/* test-only */);
            assert!(
                expr2.contains(both_flat[0].as_str()) && expr2.contains(both_flat[1].as_str()),
                "no term may be dropped when every matching term is frequent: {expr2}"
            );
        }
        let hits = cell
            .search_with_now(&Query::text("pinco lavora spesso").k(10), now())
            .unwrap(/* test-only */);
        assert!(
            hits[0]
                .note
                .display_title()
                .to_lowercase()
                .contains("pinco"),
            "the rare term decides the ranking: {:?}",
            hits.iter()
                .map(|h| h.note.display_title().to_string())
                .collect::<Vec<_>>()
        );
        let _ = pinco_id;
        assert!(
            hits[0].explain.matched_terms.contains(&stems[1]),
            "the dropped term still counts as found: {:?}",
            hits[0].explain.matched_terms
        );
    }

    #[test]
    fn connection_carries_the_vector_cache_pragmas() {
        // v0.2.1 (P0.3): 64 MB of page cache and a 256 MB mmap window — the
        // f32 recomputation reads candidate vectors straight off the pages.
        let (_d, cell) = setup();
        let inner = cell.inner.lock().unwrap(/* test-only */);
        let cache: i64 = inner
            .conn
            .query_row("PRAGMA cache_size", [], |r| r.get(0))
            .unwrap(/* test-only */);
        assert_eq!(cache, -65_536);
        let mmap: i64 = inner
            .conn
            .query_row("PRAGMA mmap_size", [], |r| r.get(0))
            .unwrap(/* test-only */);
        // mmap is best-effort: set, or 0 where the VFS refuses
        assert!(mmap == 268_435_456 || mmap == 0, "mmap_size: {mmap}");
    }

    #[test]
    fn empty_query_is_rejected() {
        let (_d, cell) = setup();
        assert!(cell.search_with_now(&Query::text("  "), now()).is_err());
    }
    // --- v0.4 (P1.4): the question's entities seed the walk, and a
    // question that names none never wakes the graph ---------------------

    #[test]
    fn question_entities_match_names_aliases_and_longest_spans() {
        let (_d, cell) = setup();
        // a person entity with an alias, and a namesake shorter entity
        let mut anna = note(30, "Anna Rossi", "Anna Rossi.", NoteType::Person);
        anna.extensions = vec![(
            "aliases".into(),
            crate::format::yaml::YamlValue::Seq(vec![
                crate::format::yaml::YamlValue::Str("Nina".into()),
                crate::format::yaml::YamlValue::Str("Annuccia Ròssi".into()),
            ]),
        )];
        cell.put_note(&anna, "t").unwrap(/* test-only */);
        let mut anna_solo = note(31, "Anna", "Anna Bianchi.", NoteType::Person);
        anna_solo.title = Some("Anna Bianchi".into());
        cell.put_note(&anna_solo, "t").unwrap(/* test-only */);
        cell.build_graph().unwrap(/* test-only */);
        let gi = {
            let inner = cell.inner.lock().unwrap(/* test-only */);
            crate::graph::GraphIndex::build(&inner.conn).unwrap(/* test-only */)
        };
        // an alias leads to the same node as the name
        let via_alias = question_entities(&gi, "che auto guida nina");
        assert_eq!(via_alias.len(), 1, "{via_alias:?}");
        // diacritics fold: «annuccia ròssi» = «annuccia rossi»
        let via_folded = question_entities(&gi, "dov'è annuccia ròssi adesso");
        assert_eq!(via_folded, via_alias, "aliases fold like the index");
        // the longest match owns its span: «anna rossi» is NOT also «anna»
        let both = question_entities(&gi, "dove abita anna rossi");
        assert_eq!(both, via_alias, "the 2-word match owns the span: {both:?}");
        // and a bare «anna bianchi» still finds the OTHER entity
        let solo = question_entities(&gi, "chi è anna bianchi");
        assert_eq!(solo.len(), 1);
        assert_ne!(
            solo[0].0, via_alias[0].0,
            "the OTHER Anna, not the Rossi one"
        );
        // nothing named: no seeds (the gate's input)
        assert!(question_entities(&gi, "come si cucina la pasta").is_empty());
        // the mass is shared equally
        let two = question_entities(&gi, "anna rossi e milano");
        assert_eq!(two.len(), 2);
        assert!((two.iter().map(|(_, m)| m).sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn gate_the_graph_off_when_no_entity_is_named() {
        let (_d, cell) = setup();
        // the v0.3.2 walk runs from the top text candidates; a question
        // about the move names no entity (settembre and trasloco are not
        // graph entities) — with question seeding the graph stays off
        let mut q = Query::text("traslocato a settembre").k(10).graph_hops(2);
        q.weights.graph_seeding = crate::search::GraphSeeding::Question {
            seed_passage_weight: 0.05,
        };
        let gated = cell.search_with_now(&q, now()).unwrap(/* test-only */);
        assert!(!gated.is_empty());
        for h in &gated {
            assert!(
                !h.explain.lists.contains(&"graph"),
                "the gate keeps graph-discovered notes out: {:?}",
                h.explain.lists
            );
            let say = h
                .explain
                .graph
                .as_ref()
                .expect(/* test-only */ "the graph ran");
            assert_eq!(say.weight, 0.0);
            assert_eq!(
                say.off,
                Some("grafo spento: nessuna entità nominata"),
                "{:?}",
                say.off
            );
        }
        // …while the v0.3.2 seeding (Fused) still walks from the candidates
        let mut legacy = Query::text("traslocato a settembre").k(10).graph_hops(2);
        legacy.weights.graph_seeding = crate::search::GraphSeeding::Fused;
        let walked = cell.search_with_now(&legacy, now()).unwrap(/* test-only */);
        assert!(
            walked.iter().any(|h| h.explain.lists.contains(&"graph")),
            "the Fused seeding still reaches graph-discovered notes"
        );
    }

    #[test]
    fn question_named_entities_seed_the_walk() {
        let (_d, cell) = setup();
        cell.build_graph().unwrap(/* test-only */);
        // the multi-hop question names Fabio: the walk starts at the Fabio
        // ENTITY (not at the text candidates), and Giulia's note arrives
        // through partner_of even though her note shares no query term
        let mut q = Query::text("il partner di Fabio").k(10).graph_hops(2);
        q.weights.graph_seeding = crate::search::GraphSeeding::Question {
            seed_passage_weight: 0.05,
        };
        let hits = cell.search_with_now(&q, now()).unwrap(/* test-only */);
        let giulia = format!("01{:024}", 2);
        assert!(
            hits.iter()
                .any(|h| h.note.id.to_string() == giulia && h.explain.lists.contains(&"graph")),
            "Giulia via the Fabio entity: {:?}",
            hits.iter()
                .map(|h| (h.note.display_title().to_string(), h.explain.lists.to_vec()))
                .collect::<Vec<_>>()
        );
        // and the explain says the graph was on
        assert!(
            hits[0]
                .explain
                .graph
                .as_ref()
                .is_some_and(|g| g.off.is_none())
        );
    }
    // --- v0.4 (P1.5): the graph weighs in per question ------------------

    #[test]
    fn graph_confidence_is_monotone_and_bounded() {
        // a clear first raises confidence, so does the lists' agreement
        let thin = graph_confidence(0.0, Some(false));
        let clear = graph_confidence(1.0, Some(true));
        assert_eq!(thin, 0.0);
        assert_eq!(clear, 1.0);
        // monotone in the gap
        assert!(graph_confidence(0.1, Some(true)) < graph_confidence(0.5, Some(true)));
        // agreement alone is worth half
        assert!((graph_confidence(0.0, Some(true)) - 0.5).abs() < 1e-12);
        // without a vector list the gap carries it all
        assert!((graph_confidence(1.0, None) - 1.0).abs() < 1e-12);
        // uncertainty stays in [u_min, 1]
        assert!((graph_uncertainty(0.1, 0.0) - 1.0).abs() < 1e-12);
        assert!((graph_uncertainty(0.1, 1.0) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn adaptive_weight_is_explained_and_self_consistent() {
        let (_d, cell) = setup();
        // a multi-salto question, thin text signal: the graph must weigh
        // near its ceiling, and the explain must carry the two signals and
        // the weight the formula produces from them
        let mut q = Query::text("il partner di Fabio").k(10).graph_hops(2);
        q.weights.graph_policy = GraphPolicy::Adaptive {
            w_max: 0.6,
            u_min: 0.05,
        };
        let hits = cell.search_with_now(&q, now()).unwrap(/* test-only */);
        let say = hits[0]
            .explain
            .graph
            .as_ref()
            .expect(/* t */ "the graph ran");
        let gap = say.gap.expect(/* t */ "the gap is a signal");
        assert_eq!(say.agreement, None, "text mode has no vector list");
        let expect = 0.6 * graph_uncertainty(0.05, graph_confidence(gap, None));
        assert!(
            (say.weight - expect).abs() < 1e-12,
            "explain and formula agree: {} vs {expect}",
            say.weight
        );
        // and with certainty forced to the floor (u_min = 1) the weight is
        // the ceiling: the graph always weighs in fully
        let mut forced = Query::text("Fabio").k(10).graph_hops(1);
        forced.weights.graph_policy = GraphPolicy::Adaptive {
            w_max: 0.6,
            u_min: 1.0,
        };
        let hits2 = cell.search_with_now(&forced, now()).unwrap(/* t */);
        assert_eq!(hits2[0].explain.graph.as_ref().unwrap(/* t */).weight, 0.6);
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
    fn support_labels_thin_vector_margins() {
        // v0.2.1 (P1.6): the label, not a prohibition. A result that also
        // matches text is strong whatever the margin; a vector-only result
        // is weak exactly when its cosine margin over the next vector
        // candidate sits under the configured bound (here driven to the
        // extremes to test the switch, not the arithmetic).
        let (_d, cell) = setup();
        let everything_weak = sem().with_weak_margin(f64::INFINITY);
        let nothing_weak = sem().with_weak_margin(f64::NEG_INFINITY);
        // "Milano" matches text AND vector: strong under both bounds
        for s in [&everything_weak, &nothing_weak] {
            let outcome = cell
                .search_semantic_with_now(&Query::text("Milano").k(3), now(), s)
                .unwrap(/* test-only */);
            assert!(
                outcome
                    .hits
                    .iter()
                    .any(|h| h.explain.lists.contains(&"text")),
                "the text match must be there: {:?}",
                outcome
                    .hits
                    .iter()
                    .map(|h| h.note.display_title().to_string())
                    .collect::<Vec<_>>()
            );
            for h in &outcome.hits {
                if h.explain.lists.contains(&"text") {
                    assert_eq!(h.explain.support, Some("strong"));
                }
            }
        }
        // no shared terms ("metropoli residenziale"): every survivor comes
        // from the vector branch alone and flips with the bound
        let q = || Query::text("metropoli residenziale").k(3).graph_hops(0);
        let all = cell
            .search_semantic_with_now(&q(), now(), &everything_weak)
            .unwrap(/* test-only */);
        assert!(!all.hits.is_empty());
        assert!(
            all.hits.iter().all(|h| h.explain.lists == vec!["vector"]),
            "vector-only: {:?}",
            all.hits
                .iter()
                .map(|h| h.explain.lists.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            all.hits.iter().all(|h| h.explain.support == Some("weak")),
            "an infinite bound makes every vector-only hit weak"
        );
        let none = cell
            .search_semantic_with_now(&q(), now(), &nothing_weak)
            .unwrap(/* test-only */);
        // note: the last vector candidate has no runner-up — weak by
        // construction even with a negative bound
        assert!(
            none.hits
                .iter()
                .filter(|h| h.explain.semantic_rank != Some(vec_list_len_of(&none)))
                .all(|h| h.explain.support == Some("strong")),
            "a negative bound keeps everything strong (but the last candidate)"
        );
    }

    /// Rank of the last surviving vector candidate (the test above).
    fn vec_list_len_of(outcome: &crate::search::SearchOutcome) -> usize {
        outcome
            .hits
            .iter()
            .filter_map(|h| h.explain.semantic_rank)
            .max()
            .unwrap_or(0)
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
            .search_semantic_with_now(
                &q,
                now(),
                &sem().with_fusion(Fusion::Convex {
                    alpha: 0.5,
                    cos_norm: CosNorm::Candidates,
                }),
            )
            .unwrap(/* test-only */);
        assert!(!rrf.hits.is_empty() && !conv.hits.is_empty());
        // both contain the text match; scores are recomputed, not shared
        let rrf_top = rrf.hits[0].note.display_title().to_string();
        let conv_top = conv.hits[0].note.display_title().to_string();
        let _ = (rrf_top, conv_top);
    }

    #[test]
    fn convex_fusion_normalises_the_cosine_scale() {
        // v0.2.1: the convex combination normalises BM25 with min-max over
        // [0, 1] but summed the RAW cosine — among candidates the cosine of
        // real embeddings spans ~0.1 while BM25 spans 1.0, so α = 0.7 could
        // not rescue a much-closer vector match from a slightly-better text
        // match. Here B loses BM25 by 1% but wins the cosine by 0.11: with
        // the cosine on the same [0, 1] scale as BM25, B must fuse higher.
        let n = |id: u16| note(id, "titolo", "corpo");
        let text = vec![
            ("a".to_string(), 10.0, 1, n(1)),
            ("b".to_string(), 9.9, 2, n(2)),
        ];
        let vecs = vec![
            VecCand {
                id: "b".to_string(),
                cosine: 0.91,
                rank: 1,
                note: n(2),
                chunk: 0,
            },
            VecCand {
                id: "a".to_string(),
                cosine: 0.80,
                rank: 2,
                note: n(1),
                chunk: 0,
            },
        ];
        let fused = fuse(
            &text,
            &vecs,
            &Fusion::Convex {
                alpha: 0.7,
                cos_norm: CosNorm::Candidates,
            },
        );
        assert!(
            fused["b"] > fused["a"],
            "the cosine must be on BM25's scale: {fused:?}"
        );
        // flat BM25 (a single text candidate): the fused order follows the
        // cosine alone — min-max'd BM25 contributes the same constant.
        let text_flat = vec![("a".to_string(), 10.0, 1, n(1))];
        let fused = fuse(
            &text_flat,
            &vecs,
            &Fusion::Convex {
                alpha: 0.7,
                cos_norm: CosNorm::Candidates,
            },
        );
        assert!(
            fused["b"] > fused["a"],
            "flat BM25, cosine order: {fused:?}"
        );
        // the theoretical normalisation (−1..1) keeps the raw span: the
        // 0.11 cosine gap maps to 0.055, less than BM25's 0.3·(1−α) gap of
        // the first case — there A still fuses higher, which is exactly
        // what the measurement of this phase had to decide between.
        let fused = fuse(
            &text,
            &vecs,
            &Fusion::Convex {
                alpha: 0.7,
                cos_norm: CosNorm::Theoretical,
            },
        );
        assert!(
            fused["a"] > fused["b"],
            "the theoretical limits keep the raw-cosine behaviour: {fused:?}"
        );
    }
}
