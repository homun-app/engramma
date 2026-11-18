// SPDX-License-Identifier: Apache-2.0

//! v0.6.8 (H1's measure): `list()` and the search over the same 10 000
//! notes the purge probe builds — p50/p95 of the text+graph search and
//! the wall time of the living list (note_ids + one get per id, exactly
//! what the Node binding's `list()` walks). Same file builds on
//! fase-v0.6.3 and on main: the two sides answer the same words.

use std::time::Instant;

fn main() {
    let n: usize = 10_000;
    let dir = tempfile::tempdir().expect(/* probe */ "tempdir");
    let cell = engramma_core::Cell::create(&dir.path().join("list-probe.sqlite"), "bench", "bench")
        .expect(/* probe */ "create");
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* fixed */);
    for i in 0..n {
        let id = ulid::Ulid::from_string(&format!("01{:024}", i + 1)).unwrap(/* probe */);
        let note = engramma_core::format::Note {
            id,
            note_type: engramma_core::format::NoteType::Fact,
            title: Some(format!("nota {i} del banco della lista")),
            body: format!(
                "prima riga\nil corpo numero {i} del banco menziona [[Roma]]\nterza riga"
            ),
            tags: vec![],
            scope: None,
            lang: None,
            source: "bench".into(),
            confidence: engramma_core::Confidence::Stated,
            status: engramma_core::Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: engramma_core::format::At(t),
            updated_at: engramma_core::format::At(t),
            extensions: vec![],
        };
        cell.put_note(&note, "bench").expect(/* probe */ "put");
    }
    // warm the graph once (the first search builds it)
    let _ = cell.search(&engramma_core::search::Query::text("Roma").k(10));
    // 100 deterministic queries: corpo <k> — half hit, half near-miss
    let mut latencies: Vec<f64> = Vec::new();
    for q in 0..100_u32 {
        let needle = if q % 2 == 0 {
            format!("corpo numero {}", q * 97 % 10_000)
        } else {
            format!("corpo numero {} inesistente", q * 97 % 10_000)
        };
        let started = Instant::now();
        let hits = cell
            .search(&engramma_core::search::Query::text(&needle).k(10))
            .expect(/* probe */ "search");
        let _ = hits.len();
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).expect(/* probe */ "f64"));
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    eprintln!("PROBE search p50={p50:.2}ms p95={p95:.2}ms");
    // text-only (graph off): isolates the text arm from the walk
    let mut text_only: Vec<f64> = Vec::new();
    for q in 0..100_u32 {
        let needle = if q % 2 == 0 {
            format!("corpo numero {}", q * 97 % 10_000)
        } else {
            format!("corpo numero {} inesistente", q * 97 % 10_000)
        };
        let started = Instant::now();
        let hits = cell
            .search(
                &engramma_core::search::Query::text(&needle)
                    .k(10)
                    .graph_hops(0),
            )
            .expect(/* probe */ "search");
        let _ = hits.len();
        text_only.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    text_only.sort_by(|a, b| a.partial_cmp(b).expect(/* probe */ "f64"));
    eprintln!(
        "PROBE text-only p50={:.2}ms p95={:.2}ms",
        text_only[text_only.len() / 2],
        text_only[text_only.len() * 95 / 100]
    );
    // list(): the living ids + one read per id, three passes, median
    let mut list_ms: Vec<f64> = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let ids = cell.note_ids().expect(/* probe */ "note_ids");
        let mut read = 0_usize;
        for id in &ids {
            if cell.get(id).is_ok() {
                read += 1;
            }
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        let _ = read;
        list_ms.push(elapsed);
    }
    list_ms.sort_by(|a, b| a.partial_cmp(b).expect(/* probe */ "f64"));
    eprintln!("PROBE list median={:.1}ms", list_ms[1]);
    // the planner's own word on the two hot queries (text arm)
    for sql in [
        "EXPLAIN QUERY PLAN SELECT (SELECT COUNT(*) FROM notes WHERE status IN ('active', 'superseded'))
                - (SELECT COUNT(*) FROM conflict_copies cc
                   JOIN notes o ON o.id = cc.original_id
                   JOIN notes n ON n.id = cc.copy_id
                   WHERE o.status = 'deleted' AND n.status IN ('active', 'superseded'))",
        "EXPLAIN QUERY PLAN SELECT n.id FROM notes_fts
         JOIN notes n ON n.rowid = notes_fts.rowid
         WHERE notes_fts MATCH 'x' AND n.status IN ('active', 'superseded')
           AND n.status != 'deleted' AND n.id NOT IN (
               SELECT cc.copy_id FROM conflict_copies cc
               JOIN notes o ON o.id = cc.original_id WHERE o.status = 'deleted')",
    ] {
        for row in cell.debug_column(sql).unwrap_or_default() {
            eprintln!("PROBE PLAN {row}");
        }
    }
}
