// SPDX-License-Identifier: Apache-2.0

//! v0.6.5 (E7, scratch): where does the purge's time go? Builds the
//! same 10 000-note corpus as the bench, compacts with the snapshot,
//! then PURGES one note — with markers around each phase so `sample`
//! and the log tell the story. Same file builds on fase-v0.6.3.

use std::time::Instant;

fn main() {
    let n: usize = 10_000;
    let dir = tempfile::tempdir().expect(/* probe */ "tempdir");
    let cell =
        engramma_core::Cell::create(&dir.path().join("purge-probe.sqlite"), "bench", "bench")
            .expect(/* probe */ "create");
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* fixed */);
    let mut first = ulid::Ulid::nil();
    let mut started = Instant::now();
    for i in 0..n {
        let id = ulid::Ulid::from_string(&format!("01{:024}", i + 1)).unwrap(/* probe */);
        if i == 0 {
            first = id;
        }
        let note = engramma_core::format::Note {
            id,
            note_type: engramma_core::format::NoteType::Fact,
            title: Some(format!("nota {i} del banco della purga")),
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
        cell.put_note(&note, "bench").expect(/* probe */ "put");
    }
    eprintln!("PHASE insert {:?}", started.elapsed());
    started = Instant::now();
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .expect(/* probe */ "head")
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    cell.compact(
        head,
        &engramma_core::cell::CompactOptions {
            keep_snapshot: true,
        },
    )
    .expect(/* probe */ "compact");
    eprintln!("PHASE compact {:?}", started.elapsed());
    started = Instant::now();
    cell.purge(&first, "bench", "bench")
        .expect(/* probe */ "purge");
    eprintln!("PHASE purge {:?}", started.elapsed());
    eprintln!("PHASE done");
}
