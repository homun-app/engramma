// SPDX-License-Identifier: Apache-2.0

//! Golden tests (§13.8): the Italian vault of ~60 notes (Fabio and Giulia's
//! 2026) and 30 questions with expected answers, plus `verify` on the golden
//! cell.

use std::path::{Path, PathBuf};

use engramma_core::Cell;
use engramma_core::format::{Lang, Note, ParseMode};
use engramma_core::search::Query;
use serde::Deserialize;
use time::OffsetDateTime;

#[derive(Deserialize)]
struct GoldenQuery {
    #[allow(dead_code, reason = "kept for readability of the JSON file")]
    id: u32,
    #[serde(default)]
    category: String,
    /// `text` (default) runs in this text+graph test; `hybrid` questions need
    /// the semantic search and live in engramma-embed's golden tests.
    #[serde(default)]
    mode: String,
    q: String,
    #[serde(default)]
    expect: Vec<String>,
    #[serde(default)]
    expect_not: Vec<String>,
    #[serde(default)]
    expect_empty: bool,
    #[serde(default)]
    as_of: Option<String>,
    /// Result-set size for this question (default 5).
    #[serde(default)]
    k: Option<usize>,
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden")
}

/// All `.md` files of the vault, in a deterministic order.
fn vault_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap(/* test-only */)
            .map(|e| e.unwrap(/* test-only */).path())
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
    let mut out = Vec::new();
    walk(&golden_dir().join("vault-it"), &mut out);
    out
}

/// The "now" of the golden questions: everything in the story has happened.
fn golden_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed golden instant */)
}

/// Build the golden cell from the vault files (deterministic order).
fn golden_cell() -> (tempfile::TempDir, Cell) {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("famiglia.sqlite"), "famiglia", "golden")
        .unwrap(/* test-only */);
    let files = vault_files();
    assert!(files.len() >= 55, "golden vault too small: {}", files.len());
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap(/* test-only */);
        let parsed = engramma_core::format::parse(&text, ParseMode::Strict)
            .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let mut note = parsed.note;
        // The vault is authored as of Nov 2026; keep created/updated as-is but
        // ensure quarantine rules at birth (the two external notes are written
        // already `quarantined`, so put_note accepts them as-is).
        let _ = &mut note;
        cell.put_note(&note, "golden")
            .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
    }
    (dir, cell)
}

fn titles(hits: &[engramma_core::search::Hit]) -> Vec<String> {
    hits.iter()
        .map(|h| h.note.display_title().to_string())
        .collect()
}

#[test]
fn golden_vault_imports_and_verifies() {
    let (_d, cell) = golden_cell();
    let meta = cell.meta().unwrap(/* test-only */);
    assert_eq!(meta.get("cell_name").map(String::as_str), Some("famiglia"));
    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "verify failed: {:?}", report.mismatch);
    assert!(report.ops >= 55);
    // the two external notes are quarantined and stay so
    let quarantined = cell
        .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM notes WHERE status = 'quarantined'")
        .unwrap(/* test-only */);
    assert_eq!(
        quarantined[0], "2",
        "the two web rumors must be quarantined"
    );
}

#[test]
fn golden_queries() {
    let (_d, cell) = golden_cell();
    let raw = std::fs::read_to_string(golden_dir().join("queries.json")).unwrap(/* t */);
    let queries: Vec<GoldenQuery> = serde_json::from_str(&raw).unwrap(/* test-only */);
    assert!(queries.len() >= 25, "want at least 25 golden questions");

    // Quarantined and deleted notes must never surface — not as results, and
    // not as a hop of a graph path (`via` renders short note ids).
    let all_ids = cell.debug_column("SELECT id FROM notes").unwrap(/* t */);
    let hidden: std::collections::BTreeSet<String> = cell
        .debug_column("SELECT id FROM notes WHERE status IN ('quarantined', 'deleted')")
        .unwrap(/* t */)
        .into_iter()
        .collect();
    // `(nota <id8>)` fragments resolve to one note only: when that note is
    // hidden, the path crossed quarantined ground.
    let via_crosses_hidden = |via: &str| -> bool {
        via.split("(nota ")
            .skip(1)
            .filter_map(|rest| rest.split(')').next())
            .any(|frag| {
                let matches: Vec<&String> = all_ids
                    .iter()
                    .filter(|id| id.starts_with(frag) || id.ends_with(frag))
                    .collect();
                matches.len() == 1 && hidden.contains(matches[0].as_str())
            })
    };

    let mut failures: Vec<String> = Vec::new();
    for gq in &queries {
        if gq.mode == "hybrid" {
            continue; // semantic territory: engramma-embed's golden tests
        }
        let mut q = Query::text(&gq.q).k(gq.k.unwrap_or(5)).graph_hops(2);
        q.lang = Some(Lang::It);
        if let Some(a) = &gq.as_of {
            let v = engramma_core::Validity::parse(a).unwrap_or_else(|e| panic!("{}: {e}", gq.q));
            q.filters.as_of = Some(v.start());
        }
        let hits = cell.search_with_now(&q, golden_now()).unwrap(/* test-only */);
        let titles = titles(&hits);
        for h in &hits {
            if let Some(via) = &h.explain.via
                && via_crosses_hidden(via)
            {
                failures.push(format!(
                    "[{}] `{}` has a path through a quarantined note: {via}",
                    gq.category, gq.q
                ));
            }
        }
        if gq.expect_empty {
            if !hits.is_empty() {
                failures.push(format!(
                    "[{}] `{}` should have no results, got {:?}",
                    gq.category, gq.q, titles
                ));
            }
            continue;
        }
        // the expect list holds acceptable alternatives: any of them is a
        // correct answer in the top 5; an empty list (quarantine questions)
        // only checks the bans below
        let found = gq.expect.is_empty()
            || gq.expect.iter().any(|wanted| {
                titles
                    .iter()
                    .any(|t| t.to_lowercase().contains(&wanted.to_lowercase()))
            });
        if !found {
            failures.push(format!(
                "[{}] `{}` expected a note matching one of {:?} in top 5, got {:?}",
                gq.category, gq.q, gq.expect, titles
            ));
        }
        for banned in &gq.expect_not {
            let leaked = titles
                .iter()
                .any(|t| t.to_lowercase().contains(&banned.to_lowercase()));
            if leaked {
                failures.push(format!(
                    "[{}] `{}` leaked quarantined note matching {:?} in {:?}",
                    gq.category, gq.q, banned, titles
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} golden failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn golden_story_invariants() {
    let (_d, cell) = golden_cell();
    // Fabio lives in Milan now (valid_from September), and the old Roma note
    // is superseded with closed validity.
    let fabio_new = cell
        .search_with_now(
            &Query::text("Fabio abita Milano").k(3),
            golden_now(),
        )
        .unwrap(/* test-only */);
    assert!(
        titles(&fabio_new)
            .iter()
            .any(|t| t.contains("Fabio abita a Milano"))
    );
    let statuses = cell
        .debug_column("SELECT status FROM notes WHERE title LIKE '%abitava%' ORDER BY id")
        .unwrap(/* test-only */);
    assert_eq!(
        statuses,
        vec!["superseded".to_string(), "superseded".to_string()]
    );

    // multi-hop graph: the direct Fabio→Studio Rossi path is `works_at`;
    // the partner path Fabio→Giulia is one step.
    let path = cell
        .path("Fabio", "Studio Rossi", false)
        .unwrap(/* test-only */);
    let steps = path.expect("path Fabio→Studio Rossi exists");
    assert!(steps.len() <= 4);
    assert_eq!(steps.last().unwrap(/* test-only */).to, "Studio Rossi");
    let partner = cell
        .path("Fabio", "Giulia", false)
        .unwrap(/* test-only */);
    assert!(partner.is_some_and(|p| p.len() == 1 && p[0].rel == "partner_of"));
    // and the two-hop answer exists: Giulia -works_at-> Studio Rossi
    let via_giulia = cell
        .path("Giulia", "Studio Rossi", false)
        .unwrap(/* test-only */);
    assert!(via_giulia.is_some_and(|p| p.len() == 1 && p[0].rel == "works_at"));
    // no external note ever reaches active without confirm
    let active_external = cell
        .debug_column(
            "SELECT CAST(COUNT(*) AS TEXT) FROM notes WHERE confidence = 'external' AND status = 'active'",
        )
        .unwrap(/* test-only */);
    assert_eq!(active_external[0], "0");

    // every superseded note carries a closed validity: two "current" facts
    // about the same subject must never coexist
    let open_superseded = cell
        .debug_column(
            "SELECT CAST(COUNT(*) AS TEXT) FROM notes
             WHERE status = 'superseded' AND valid_to IS NULL",
        )
        .unwrap(/* test-only */);
    assert_eq!(
        open_superseded[0], "0",
        "a superseded note must have valid_to"
    );

    // confirm one quarantined note, then it appears
    let qid = cell
        .debug_column("SELECT id FROM notes WHERE status = 'quarantined' LIMIT 1")
        .unwrap(/* test-only */)[0]
        .clone();
    let id = ulid_from(&qid);
    cell.confirm(&id, "golden-test").unwrap(/* test-only */);
    let now_active = cell
        .debug_column(
            "SELECT CAST(COUNT(*) AS TEXT) FROM notes WHERE confidence = 'external' AND status = 'active'",
        )
        .unwrap(/* test-only */);
    assert_eq!(now_active[0], "1");
    // and the verify still passes after confirm
    assert!(cell.verify().unwrap(/* test-only */).ok());
}

/// The paraphrase questions (§5) must share **no root** with the note they
/// expect, after the normalisation of `text.rs`: what they test is the
/// meaning branch, not lexical overlap.
#[test]
fn paraphrase_questions_share_no_roots_with_their_notes() {
    let (_d, cell) = golden_cell();
    let raw = std::fs::read_to_string(golden_dir().join("queries.json")).unwrap(/* t */);
    let queries: Vec<GoldenQuery> = serde_json::from_str(&raw).unwrap(/* test-only */);
    let paraphrases: Vec<&GoldenQuery> = queries
        .iter()
        .filter(|q| q.category == "parafrasi")
        .collect();
    assert!(paraphrases.len() >= 15, "want 15 paraphrases");
    for gq in paraphrases {
        let q_terms: std::collections::BTreeSet<String> =
            engramma_core::text::query_terms(&gq.q, Some(Lang::It), &[])
                .flat()
                .into_iter()
                .collect();
        assert!(!q_terms.is_empty(), "`{}` has no usable terms", gq.q);
        for wanted in &gq.expect {
            // the note whose title contains `wanted` (how expect works)
            let note_norm = cell
                .debug_column(&format!(
                    "SELECT body_norm FROM notes WHERE lower(title) LIKE '%{}%' LIMIT 1",
                    wanted.to_lowercase().replace('\'', "''")
                ))
                .unwrap(/* test-only */);
            let found = note_norm.first().cloned().unwrap_or_default();
            assert!(!found.is_empty(), "`{}`: no note matches `{wanted}`", gq.q);
            let shared: Vec<String> = found
                .split(' ')
                .filter(|stem| q_terms.contains(*stem))
                .map(String::from)
                .collect();
            assert!(
                shared.is_empty(),
                "`{}` shares stems {shared:?} with the note `{wanted}` \
                 (body_norm: {found})",
                gq.q
            );
        }
    }
}

/// Small helper to rebuild a ULID from the golden ids.
fn ulid_from(s: &str) -> ulid::Ulid {
    ulid::Ulid::from_string(s).unwrap(/* test-only: from our own column */)
}

/// The golden corpus also feeds a Note round-trip check: every vault file is
/// stable through parse→serialize.
#[test]
fn golden_files_are_canonical() {
    let files = vault_files();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap(/* test-only */);
        let parsed = engramma_core::format::parse(&text, ParseMode::Strict)
            .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let canonical: Note = parsed.note;
        let again = canonical.serialize().unwrap(/* test-only */);
        assert_eq!(text, again, "{} is not canonical", f.display());
    }
}

/// v0.3: a multi-hop question is only real when the text branch alone cannot
/// see the answer — with `graph_hops = 0` the expected note must stay out of
/// the top 5. Questions failing this are not multi-hop: they get corrected in
/// the vault/queries, not tolerated here (prompt v0.3, punto 0).
#[test]
fn multihop_questions_need_the_graph() {
    let (_d, cell) = golden_cell();
    let raw = std::fs::read_to_string(golden_dir().join("queries.json")).unwrap(/* test-only */);
    let queries: Vec<GoldenQuery> = serde_json::from_str(&raw).unwrap(/* test-only */);
    let multihop: Vec<&GoldenQuery> = queries
        .iter()
        .filter(|q| q.category == "multi-salto")
        .collect();
    assert!(
        multihop.len() >= 5,
        "want at least 5 true multi-hop questions, got {}",
        multihop.len()
    );
    for gq in &multihop {
        let mut q = Query::text(&gq.q).k(5).graph_hops(0);
        q.lang = Some(Lang::It);
        let hits = cell.search_with_now(&q, golden_now()).unwrap(/* test-only */);
        let top = titles(&hits);
        let leaked = gq.expect.iter().any(|wanted| {
            top.iter()
                .any(|t| t.to_lowercase().contains(&wanted.to_lowercase()))
        });
        assert!(
            !leaked,
            "[{}] `{}` is answered by text alone (top 5 {top:?}): \
             not a real multi-hop question — fix the story",
            gq.category, gq.q
        );
    }
}
