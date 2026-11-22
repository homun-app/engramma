// SPDX-License-Identifier: Apache-2.0

//! v0.5.3 (P0): the short-question recall bank — 1-to-3-word questions,
//! proper names included, on the golden Italian vault. The whole point of
//! the phase: a one-word question usually finds nothing when the stemmer
//! trusts `whatlang`'s guess («Zebedeo» was German, «tonno» French); an
//! uncertain question now expands over the languages of the cell. The
//! numbers this test prints are the phase's evidence (REPORT §P0); the
//! floor pins them.

use std::path::{Path, PathBuf};

use engramma_core::Cell;
use engramma_core::format::ParseMode;
use engramma_core::search::Query;
use serde::Deserialize;

#[derive(Deserialize)]
struct ShortQuery {
    #[allow(dead_code, reason = "kept for readability of the JSON file")]
    id: u32,
    category: String,
    q: String,
    #[serde(default)]
    expect: Vec<String>,
    #[serde(default)]
    expect_not: Vec<String>,
    #[serde(default)]
    expect_empty: bool,
    /// Only notes valid at this date answer (the bitemporal question).
    #[serde(default)]
    as_of: Option<String>,
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden")
}

fn golden_now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed golden instant */)
}

/// The Italian function words the bank's questions may carry around their
/// 1–3 significant words.
const FILLERS: &[&str] = &[
    "di", "a", "da", "il", "lo", "la", "i", "gli", "le", "un", "una", "e", "o", "che", "in", "con",
    "su", "per", "al", "del",
];

fn significant_words(q: &str) -> usize {
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && !FILLERS.contains(t))
        .count()
}

fn titles(hits: &[engramma_core::search::Hit]) -> Vec<String> {
    hits.iter()
        .map(|h| h.note.display_title().to_string())
        .collect()
}


/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

#[test]
fn short_question_recall_on_the_golden_vault() {
    let raw = std::fs::read_to_string(golden_dir().join("queries-short.json")).unwrap(/* t */);
    let queries: Vec<ShortQuery> = serde_json::from_str(&raw).unwrap(/* t */);
    let scored: Vec<&ShortQuery> = queries.iter().filter(|q| !q.expect_empty).collect();
    assert!(
        scored.len() >= 40,
        "the bank wants at least 40 scored short questions, got {}",
        scored.len()
    );
    for q in &queries {
        let sig = significant_words(&q.q);
        assert!(
            (1..=3).contains(&sig),
            "`{}` has {sig} significant words: the bank is 1-to-3-word questions",
            q.q
        );
    }

    // the golden cell, exactly as golden.rs builds it
    let dir = test_dir();
    let cell = Cell::create(&dir.path().join("famiglia.sqlite"), "famiglia", "golden")
        .unwrap(/* t */);
    let mut files: Vec<PathBuf> = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap(/* t */)
            .map(|e| e.unwrap(/* t */).path())
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "md") {
                out.push(p);
            }
        }
    }
    walk(&golden_dir().join("vault-it"), &mut files);
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap(/* t */);
        let parsed = engramma_core::format::parse(&text, ParseMode::Strict)
            .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        cell.put_note(&parsed.note, "golden").unwrap(/* t */);
    }

    // NO explicit language: the uncertain path is the point of the phase
    let mut hits_at_5 = 0_usize;
    let mut misses: Vec<String> = Vec::new();
    let mut leaks: Vec<String> = Vec::new();
    for gq in &queries {
        let mut q = Query::text(&gq.q).k(5).graph_hops(2);
        if let Some(a) = &gq.as_of {
            let v = engramma_core::Validity::parse(a).unwrap_or_else(|e| panic!("{}: {e}", gq.q));
            q.filters.as_of = Some(v.start());
        }
        let hits = cell.search_with_now(&q, golden_now()).unwrap(/* t */);
        let top = titles(&hits);
        if gq.expect_empty {
            for banned in &gq.expect_not {
                if top
                    .iter()
                    .any(|t| t.to_lowercase().contains(&banned.to_lowercase()))
                {
                    leaks.push(format!("`{}` surfaced `{banned}`", gq.q));
                }
            }
            continue;
        }
        let found = gq.expect.iter().any(|wanted| {
            top.iter()
                .any(|t| t.to_lowercase().contains(&wanted.to_lowercase()))
        });
        if found {
            hits_at_5 += 1;
        } else {
            misses.push(format!(
                "[{}] `{}` wanted {:?}, got {:?}",
                gq.category, gq.q, gq.expect, top
            ));
        }
    }
    let recall = hits_at_5 as f64 / scored.len() as f64;
    println!(
        "banco domande corte: richiamo@5 = {}/{} = {:.3}",
        hits_at_5,
        scored.len(),
        recall
    );
    if !misses.is_empty() {
        println!("mancate:\n{}", misses.join("\n"));
    }
    assert!(leaks.is_empty(), "quarantine leaks:\n{}", leaks.join("\n"));
    assert!(
        recall >= 0.90,
        "richiamo@5 {recall:.3} sotto il pavimento 0.90; mancate:\n{}",
        misses.join("\n")
    );
}
