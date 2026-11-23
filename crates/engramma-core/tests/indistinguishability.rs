// SPDX-License-Identifier: Apache-2.0

//! The indistinguishability property — the SAME question gives BIT-IDENTICAL
//! results whether or not the cell also holds quarantined notes and
//! tombstones that match the question. Over HTTP `explain` is an oracle:
//! ranks that start at 2, a `fused_score` under 1 at the top, a displaced
//! answer, a score that moves by an ε — each of those tells a remote client
//! that a hidden note exists. v0.5.3 held the property on every structural
//! field and declared an ε on the scores (FTS5 computed bm25's IDF over the
//! whole index, hidden rows included); v0.6 (P0) splits the index in two —
//! `notes_fts` holds the visible notes, `notes_fts_hidden` the quarantined
//! ones — so the visible statistics never see a hidden row and the property
//! is EXACT: same ids, same ranks, same scores, same `raw_score`, same
//! `fused_score`, same cosines, bit for bit, in the three modes: text only,
//! hybrid with a fake embedder, text + graph.

use engramma_core::format::{At, Note, NoteType};
use engramma_core::search::{Hit, Mode, Query, Semantic};
use engramma_core::{Cell, Confidence, EmbedError, Embedder, Status, TextKind};
use proptest::prelude::*;
use time::OffsetDateTime;
use ulid::Ulid;
// v0.6.14 (N3): `current` is a trait method of the value trees
use proptest::strategy::ValueTree as _;

/// v0.6.14 (N3): a runner seeded by the property's OWN seed. The body of
/// every property takes that one seed and derives its inputs from it
/// through the same strategies the property always drew — so a failing
/// seed replays in ONE case (`scripts/seme-sul-tag.sh <proprietà> <seme>
/// <tag>`), on any tree that holds the body.
fn seeded_runner(seed: u64) -> proptest::test_runner::TestRunner {
    // ChaCha (the default) wants a 32-byte seed: the u64 is laid four times
    let block = seed.to_le_bytes();
    let mut bytes = [0_u8; 32];
    for k in 0..4 {
        bytes[k * 8..k * 8 + 8].copy_from_slice(&block);
    }
    proptest::test_runner::TestRunner::new_with_rng(
        proptest::test_runner::Config::default(),
        proptest::test_runner::TestRng::from_seed(
            proptest::test_runner::RngAlgorithm::default(),
            &bytes,
        ),
    )
}

/// The vocabulary the notes and the questions draw from: Italian words the
/// stemmers treat differently, so matches are real.
const WORDS: &[&str] = &[
    "gatto",
    "gatti",
    "tonno",
    "Zebedeo",
    "Aurora",
    "martedì",
    "casa",
    "lavoro",
    "lavora",
    "studio",
    "cucina",
    "corsa",
    "palla",
    "orto",
    "treno",
    "ritratto",
    "quaderno",
    "pane",
    "sedia",
    "tavolo",
    "finestra",
    "porto",
    "vela",
    "campo",
    "neve",
    "pioggia",
    "vetro",
    "moneta",
    "lettera",
    "sciarpa",
    "ombrello",
    "zaino",
    "lanterna",
    "cuscino",
    "mattonella",
    "clessidra",
];

fn word() -> impl Strategy<Value = String> {
    proptest::sample::select(WORDS.to_vec()).prop_map(str::to_string)
}

fn sentence(min: usize, max: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(word(), min..=max).prop_map(|w| w.join(" "))
}

/// A body: a sentence, sometimes with a mention (an entity for the graph).
fn body() -> impl Strategy<Value = String> {
    (sentence(2, 6), proptest::option::of(r"[ABG][a-z]{2,6}")).prop_map(|(s, ent)| match ent {
        Some(e) => format!("{s} con [[{e}]]"),
        None => s,
    })
}

fn base_note(id: u32, body: &str, now: At) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{id:08}", "0".repeat(16)))
            .unwrap(/* test-only: valid Crockford */),
        note_type: NoteType::Fact,
        title: None,
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: String::new(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: now,
        updated_at: now,
        extensions: vec![],
    }
}

/// A deterministic fake embedder: the text hashed into a fixed-dim unit
/// vector, same text → same vector. No model, no I/O.
#[derive(Clone)]
struct FakeEmbedder;

const DIM: usize = 16;

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[&str], _kind: TextKind) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = [0.0_f32; DIM];
                for (i, b) in t.bytes().enumerate() {
                    v[(i + usize::from(b)) % DIM] += f32::from(b) / 255.0;
                }
                let norm = (v.iter().map(|x| x * x).sum::<f32>() + 1e-9).sqrt();
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }

    fn model_id(&self) -> &str {
        "fake-16"
    }

    fn dim(&self) -> usize {
        DIM
    }

    fn max_tokens(&self) -> usize {
        512
    }
}

fn now() -> At {
    At(OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */))
}

/// Build a cell from the base notes (all active), embedding included.
fn cell_of(dir: &std::path::Path, bodies: &[String]) -> Cell {
    let cell = Cell::create(&dir.join("p.sqlite"), "p", "t").unwrap(/* test-only */);
    for (i, b) in bodies.iter().enumerate() {
        cell.put_note(&base_note(i as u32 + 1, b, now()), "t")
            .unwrap(/* test-only */);
    }
    cell.embed_pending(&FakeEmbedder, 64).unwrap(/* test-only */);
    cell
}

/// The fields the property compares, all EXACTLY (v0.6, P0: the two indices
/// closed the ε — a hidden row cannot move a visible statistic anymore).
struct Snapshot {
    ids: Vec<String>,
    scores: Vec<f64>,
    raw_scores: Vec<f64>,
    fused: Vec<Option<f64>>,
    bm25_ranks: Vec<Option<usize>>,
    sem_ranks: Vec<Option<usize>>,
    graph_ranks: Vec<Option<usize>>,
    cosines: Vec<Option<f64>>,
    supports: Vec<Option<String>>,
    lists: Vec<Vec<String>>,
    matched: Vec<Vec<String>>,
}

fn snapshot(hits: &[Hit]) -> Snapshot {
    Snapshot {
        ids: hits.iter().map(|h| h.note.id.to_string()).collect(),
        scores: hits.iter().map(|h| h.score).collect(),
        raw_scores: hits.iter().map(|h| h.raw_score).collect(),
        fused: hits.iter().map(|h| h.explain.fused_score).collect(),
        bm25_ranks: hits.iter().map(|h| h.explain.bm25_rank).collect(),
        sem_ranks: hits.iter().map(|h| h.explain.semantic_rank).collect(),
        graph_ranks: hits.iter().map(|h| h.explain.graph_rank).collect(),
        cosines: hits.iter().map(|h| h.explain.cosine).collect(),
        supports: hits
            .iter()
            .map(|h| h.explain.support.map(str::to_string))
            .collect(),
        lists: hits
            .iter()
            .map(|h| h.explain.lists.iter().map(|l| l.to_string()).collect())
            .collect(),
        matched: hits
            .iter()
            .map(|h| h.explain.matched_terms.clone())
            .collect(),
    }
}

/// What the property holds, BIT for BIT (v0.6, P0; DESIGN §5):
///
/// - the SET of result ids: a hidden note never appears, and no visible
///   result is pushed out of the window by one (the displacement tell);
/// - the ranks are DENSE over the visible matches, starting at 1: no gap
///   where a hidden note sits (the "ranks 2 and 3 without 1" tell);
/// - the top result's score is 1.0: every normalisation (bm25 max, fused
///   max) is computed over the visible lists alone (the "fused_score under
///   1" tell);
/// - per id, EXACTLY: score, raw_score, fused_score, cosine, support,
///   lists, matched terms, semantic rank, graph rank.
///
/// Before v0.6 the scores carried the ε of FTS5's table-wide IDF (the
/// quarantined rows had to stay in the one index for the owner's
/// `include_quarantined`); with the hidden rows in their own index the
/// visible statistics cannot see them, and what cannot move a statistic
/// cannot move a score.
fn diff(a: &Snapshot, b: &Snapshot, what: &str) -> Option<String> {
    // the same ids, as a set
    let mut sa = a.ids.clone();
    let mut sb = b.ids.clone();
    sa.sort();
    sb.sort();
    if sa != sb {
        return Some(format!("{what}: ids differ\n  {:?}\n  {:?}", a.ids, b.ids));
    }
    // dense ranks from 1 over the visible matches (no hidden gaps)
    for (snap, name) in [(a, "A"), (b, "B")] {
        let ranks: Vec<Option<usize>> = snap
            .ids
            .iter()
            .enumerate()
            .map(|(i, _)| snap.bm25_ranks[i])
            .collect();
        let text_hits = ranks.iter().filter(|r| r.is_some()).count();
        let dense: Vec<usize> = (1..=text_hits).collect();
        // the final order may legitimately differ from the bm25 order
        // (coverage, recency, confidence rerank) — the RANKS must be dense
        // as a set: no gap where a hidden note sits
        let mut got: Vec<usize> = ranks.iter().filter_map(|r| *r).collect();
        got.sort();
        if got != dense {
            return Some(format!(
                "{what}: {name} bm25 ranks are not dense from 1 over the visible matches: {got:?}"
            ));
        }
    }
    // the top of each list normalises to 1
    for (snap, name) in [(a, "A"), (b, "B")] {
        if let Some(top) = snap.scores.first()
            && *top != 1.0
        {
            return Some(format!(
                "{what}: {name} top score {top} — a hidden maximum?"
            ));
        }
    }
    let at = |snap: &Snapshot, id: &str| -> usize {
        snap.ids.iter().position(|i| i == id).unwrap_or(usize::MAX)
    };
    for id in &a.ids {
        let (ia, ib) = (at(a, id), at(b, id));
        if a.scores[ia] != b.scores[ib] {
            return Some(format!(
                "{what}: {id} score {} vs {}",
                a.scores[ia], b.scores[ib]
            ));
        }
        if a.raw_scores[ia] != b.raw_scores[ib] {
            return Some(format!(
                "{what}: {id} raw_score {} vs {}",
                a.raw_scores[ia], b.raw_scores[ib]
            ));
        }
        if a.fused[ia] != b.fused[ib] {
            return Some(format!(
                "{what}: {id} fused_score {:?} vs {:?}",
                a.fused[ia], b.fused[ib]
            ));
        }
        if a.cosines[ia] != b.cosines[ib] {
            return Some(format!("{what}: {id} cosine differs"));
        }
        if a.supports[ia] != b.supports[ib] {
            return Some(format!(
                "{what}: {id} support {:?} vs {:?}",
                a.supports[ia], b.supports[ib]
            ));
        }
        if a.lists[ia] != b.lists[ib] {
            return Some(format!("{what}: {id} lists differ"));
        }
        if a.matched[ia] != b.matched[ib] {
            return Some(format!("{what}: {id} matched terms differ"));
        }
        if a.sem_ranks[ia] != b.sem_ranks[ib] {
            return Some(format!("{what}: {id} semantic rank differs"));
        }
        if a.graph_ranks[ia] != b.graph_ranks[ib] {
            return Some(format!("{what}: {id} graph rank differs"));
        }
    }
    None
}

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn hidden_notes_leave_no_fingerprint(seed in any::<u64>()) {
        hidden_notes_leave_no_fingerprint_body(seed)?;
    }
}

/// The reviewer's measured case (v0.6, P0): on «Zebedeo tonno» the second
/// VISIBLE result moved from `1e-06` to `0.4156` when a quarantined note
/// matching the question landed in the one index FTS5 shares — the IDF of
/// both terms moved, and with them every visible score. A remote client
/// repeating the question reads the arrival of a hidden note off the
/// numbers. With the quarantined rows in their own index the visible
/// statistics cannot move: every score is bit-identical.
#[test]
fn the_zebedeo_tonno_case_is_bit_identical() {
    let question = "Zebedeo tonno";
    // the strong first result (both terms), a thin second (one term, long
    // body — the one the reviewer saw jump), and fillers so the corpus is
    // not two notes tall
    let strong = "Zebedeo compra il tonno al mercato".to_string();
    let thin = format!("tonno {}", "porto vela campo neve pioggia ".repeat(60));
    let fillers: Vec<String> = (1..=6)
        .map(|i| {
            format!(
                "la nota {i} parla di {} e {}",
                WORDS[10 + i % 20],
                WORDS[11 + i % 20]
            )
        })
        .collect();
    let mut bodies = vec![strong, thin];
    bodies.extend(fillers);
    let dir_a = test_dir();
    let cell_a = cell_of(dir_a.path(), &bodies);
    let dir_b = test_dir();
    let cell_b = cell_of(dir_b.path(), &bodies);
    // the hidden note: quarantined, external, matching BOTH terms — the
    // fingerprint carrier
    let mut hidden = base_note(100, "Zebedeo tonno Zebedeo tonno", now());
    hidden.confidence = Confidence::External;
    hidden.status = Status::Quarantined;
    hidden.source = "import:web".into();
    cell_b.put_note(&hidden, "t").unwrap(/* test-only */);

    let q = Query::text(question).k(8);
    let a = cell_a.search_with_now(&q, now().get()).unwrap(/* test-only */);
    let b = cell_b.search_with_now(&q, now().get()).unwrap(/* test-only */);
    // the case is about the SECOND result: it must exist and be thin in A
    assert!(
        a.len() >= 2,
        "the case needs a second visible result, got {}",
        a.len()
    );
    assert!(
        a[1].score < 0.2,
        "the second result must be thin (one term of two, a long body), got {}",
        a[1].score
    );
    if let Some(why) = diff(&snapshot(&a), &snapshot(&b), "zebedeo") {
        panic!("{why}");
    }
}

/// v0.6.14 (N3): the body of `hidden_notes_leave_no_fingerprint` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn hidden_notes_leave_no_fingerprint_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (bodies, question, n_quarantined, n_tombstones) = (
        proptest::collection::vec(body(), 4..10).prop_map(|v| {
            // distinct bodies: two identical texts are an exact bm25 tie the
            // tie-break must not be asked to settle — the property hunts the
            // hidden-row fingerprint, not the sorter's patience
            let mut seen = std::collections::BTreeSet::new();
            v.into_iter()
                .filter(|b| seen.insert(b.clone()))
                .collect::<Vec<_>>()
        }),
        sentence(1, 3),
        0_u32..3,
        0_u32..3,
    )
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    hidden_notes_leave_no_fingerprint_with(bodies, question, n_quarantined, n_tombstones)
}

/// v0.6.14 (N3): the property's story on its inputs EXPLICITLY — the shape
/// it drew until this phase, the one in which its persisted counterexamples
/// and its declared defect were written down. The body above derives the
/// inputs from one seed (so a failing seed replays in one case); this takes
/// them as they are, for the cases history recorded.
fn hidden_notes_leave_no_fingerprint_with(
    bodies: Vec<String>,
    question: String,
    n_quarantined: u32,
    n_tombstones: u32,
) -> Result<(), proptest::test_runner::TestCaseError> {
    // base + hidden notes that MATCH the question (they must, to be a
    // fingerprint), in their own cell
    let dir_a = test_dir();
    let cell_a = cell_of(dir_a.path(), &bodies);
    let dir_b = test_dir();
    let cell_b = cell_of(dir_b.path(), &bodies);
    let matching = format!("{question} {question}");
    for k in 0..n_quarantined {
        let mut n = base_note(100 + k, &matching, now());
        n.confidence = Confidence::External;
        n.status = Status::Quarantined;
        n.source = "import:web".into();
        cell_b.put_note(&n, "t").unwrap(/* test-only */);
    }
    for k in 0..n_tombstones {
        let n = base_note(200 + k, &matching, now());
        let id = n.id;
        cell_b.put_note(&n, "t").unwrap(/* test-only */);
        cell_b.delete_note(&id, "t").unwrap(/* test-only */);
    }
    // v0.6, P0: no ε is declared anymore, so the hybrid mode runs on the
    // DEFAULT (convex) fusion too — v0.5.3 kept it on RRF because the
    // IDF ε could flip which candidate is the worst of the union and the
    // convex `retain(score > 0)` cut it: a cliff born with the shared
    // index, gone with it.
    let sem = Semantic::new(std::sync::Arc::new(FakeEmbedder)).with_mode(Mode::Hybrid);
    let modes: &[(&str, Query, bool)] = &[
        ("testo", Query::text(&question).k(8), false),
        ("ibrida", Query::text(&question).k(8), true),
        (
            "testo+grafo",
            Query::text(&question).k(8).graph_hops(2),
            false,
        ),
    ];
    for (what, q, hybrid) in modes {
        let a = if *hybrid {
            cell_a.search_semantic_with_now(q, now().get(), &sem).unwrap(/* test-only */).hits
        } else {
            cell_a.search_with_now(q, now().get()).unwrap(/* test-only */)
        };
        let b = if *hybrid {
            cell_b.search_semantic_with_now(q, now().get(), &sem).unwrap(/* test-only */).hits
        } else {
            cell_b.search_with_now(q, now().get()).unwrap(/* test-only */)
        };
        if let Some(why) = diff(&snapshot(&a), &snapshot(&b), what) {
            prop_assert!(false, "{}", why);
        }
    }
    Ok(())
}

/// v0.6.14 (N3): the counterexamples proptest persisted while the property
/// drew `(bodies, question, n_quarantined, n_tombstones)` separately
/// (`indistinguishability.proptest-regressions`, whose `cc` lines left
/// with the tuple shape). Written out, replayed.
#[test]
fn the_regressions_of_the_tuple_era_stay_green() {
    let cases: Vec<(Vec<&str>, &str, u32, u32)> = vec![
        (vec!["ritratto gatto", "gatto gatto"], "ritratto", 1, 0),
        (
            vec!["gatto gatto", "gatto tonno con [[Gaa]]"],
            "gatto",
            1,
            0,
        ),
        (vec!["corsa gatto", "treno martedì"], "corsa treno", 1, 0),
        (
            vec![
                "gatto studio",
                "gatto gatto",
                "gatto gatto gatto lavoro",
                "tavolo gatti",
            ],
            "tavolo lavoro",
            1,
            0,
        ),
        (
            vec![
                "moneta cucina",
                "gatto gatto",
                "gatto gatto con [[Aaa]]",
                "moneta ritratto",
                "gatto gatto con [[Gaa]]",
            ],
            "moneta",
            2,
            0,
        ),
        (
            vec![
                "gatto gatto",
                "gatto gatti",
                "ombrello gatto gatto con [[Aaa]]",
                "gatto tonno",
                "gatto sedia con [[Gaa]]",
                "sedia gatto",
            ],
            "ombrello sedia sedia",
            0,
            0,
        ),
    ];
    for (bodies, question, n_quarantined, n_tombstones) in cases {
        hidden_notes_leave_no_fingerprint_with(
            bodies.into_iter().map(str::to_string).collect(),
            question.to_string(),
            n_quarantined,
            n_tombstones,
        )
        .unwrap_or_else(|e| panic!("«{question}» ({n_quarantined}, {n_tombstones}): {e:?}"));
    }
}
