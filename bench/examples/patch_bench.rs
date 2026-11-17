// SPDX-License-Identifier: Apache-2.0

//! v0.6.5 (E1, «Poi»): the LOCAL WRITE path — 10 000 body patches over
//! existing notes. E1 changes what a local write leans on (the visible
//! head, one indexed probe, and its derivation row at the second
//! operation): this bench measures the cost against v0.6.4, notes/s and
//! p95 of the single patch. Run in --release; the corpus is fixed.

use std::time::Instant;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let dir = tempfile::tempdir().expect(/* bench */ "tempdir");
    let cell =
        engramma_core::Cell::create(&dir.path().join("patch-bench.sqlite"), "bench", "bench")
            .expect(/* bench */ "create");
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* fixed */);
    let mut ids: Vec<ulid::Ulid> = Vec::with_capacity(n);
    for i in 0..n {
        let id = ulid::Ulid::from_string(&format!("01{:022}", i + 1)).unwrap(/* bench */);
        ids.push(id);
        let note = engramma_core::format::Note {
            id,
            note_type: engramma_core::format::NoteType::Fact,
            title: Some(format!("nota {i} del banco delle patch")),
            body: format!("prima riga\nil corpo numero {i} del banco\nterza riga"),
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
        cell.put_note(&note, "bench").expect(/* bench */ "put");
    }
    // the measure: one body patch per existing note, sequential (the
    // common editing shape — every patch leans on the note's own head)
    let mut times: Vec<u128> = Vec::with_capacity(n);
    let started = Instant::now();
    for (i, id) in ids.iter().enumerate() {
        let one = Instant::now();
        cell.patch_note(
            id,
            &engramma_core::NotePatch {
                body: Some(format!(
                    "prima riga\nil corpo numero {i} ritoccato dal banco\nterza riga"
                )),
                ..Default::default()
            },
            "bench",
        )
        .expect(/* bench */ "patch");
        times.push(one.elapsed().as_micros());
    }
    let elapsed = started.elapsed();
    times.sort_unstable();
    let p95 = times[n * 95 / 100];
    println!(
        "patch-bench: {n} note, {n} patch di corpo in {:.2} s → {:.0} patch/s, p95 {:.2} ms",
        elapsed.as_secs_f64(),
        n as f64 / elapsed.as_secs_f64(),
        p95 as f64 / 1000.0,
    );
}
