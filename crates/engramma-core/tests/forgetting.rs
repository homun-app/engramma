// SPDX-License-Identifier: Apache-2.0

//! The right to be forgotten, checked on the raw bytes (v0.3.1, P0).
//!
//! The v0.3 purge tests searched the raw file for the **whole word**, but the
//! FTS index stores the **stems** (after the engine's Snowball stemmer), the
//! WAL keeps every frame until a checkpoint, and the compaction snapshots are
//! full copies made before the purge. These tests search for every root of
//! the purged body's terms — produced by the engine's own normalisation, the
//! same one that fills `body_norm` — in the cell file, in the `-wal`, and in
//! every snapshot file.

use std::path::{Path, PathBuf};

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use time::OffsetDateTime;
use ulid::Ulid;

// Every needle a purge must erase for a body: the whole (lowercased,
// diacritics-folded) words and their stems, exactly as the engine's
// normalisation produces them. Only the body's own terms: the caller picks
// bodies of distinctive words, so a hit is always a leak.
pub(crate) fn roots_of(body: &str, lang: Option<Lang>) -> Vec<String> {
    let mut out = Vec::new();
    for token in body.split(|c: char| !c.is_alphanumeric()) {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric());
        if token.is_empty() {
            continue;
        }
        let lowered = engramma_core::text::strip_diacritics(&token.to_lowercase());
        let stem = engramma_core::text::normalize(&lowered, lang);
        for needle in [lowered, stem] {
            if needle.len() >= 5 && !out.contains(&needle) {
                out.push(needle);
            }
        }
    }
    out
}

/// The first needle found in a file's raw bytes, when any.
fn leaked(path: &Path, needles: &[String]) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    needles
        .iter()
        .find(|n| raw.windows(n.len()).any(|w| w == n.as_bytes()))
        .cloned()
}

/// The cell file, its WAL and every snapshot file must carry none of the
/// needles. The snapshot naming is `compact`'s own (`<stem>-snap-<seq>.sqlite`).
pub(crate) fn assert_forgotten(dir: &Path, stem: &str, needles: &[String]) {
    let mut files: Vec<PathBuf> = vec![
        dir.join(format!("{stem}.sqlite")),
        dir.join(format!("{stem}.sqlite-wal")),
    ];
    let mut snaps: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{stem}-snap-")))
        })
        .collect();
    snaps.sort();
    files.extend(snaps);
    for f in files {
        if let Some(n) = leaked(&f, needles) {
            panic!("«{n}» sopravvive in {}: oblio violato", f.display());
        }
    }
}

fn at(day: u8) -> At {
    At(
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
        .unwrap(/* test-only */),
    )
}

/// A note whose body is made of distinctive words: every root below belongs
/// to it alone.
fn note_of(id: u16, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: None,
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: at(1),
        updated_at: at(1),
        extensions: vec![],
    }
}

/// P0.1: after a plain purge no root of the body survives in the cell file —
/// the FTS5 `delete` alone leaves the stems in the old segments; only the
/// `optimize` merge discards them.
#[test]
fn purge_scrubs_the_stems_from_the_file() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("oblio.sqlite");
    let cell = Cell::create(&path, "oblio", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "quokkaviolaceo zibellinotremante ovoviviparo");
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    let innocent = note_of(2, "passeggiata lungo larghissimo fiume");
    cell.put_note(&innocent, "tester").unwrap(/* test-only */);
    let needles = roots_of(&secret.body, Some(Lang::It));
    assert!(needles.len() >= 3, "{needles:?}");

    cell.purge(&secret.id, "diritto all'oblio", "tester")
        .unwrap(/* test-only */);
    // the chain still verifies and the innocent note survived
    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "verify after purge: {:?}", report.mismatch);
    assert_eq!(
        cell.get(&innocent.id).unwrap(/* test-only */).body,
        innocent.body
    );
    drop(cell);
    assert_forgotten(dir.path(), "oblio", &needles);
}

/// P0.2: with another connection holding an open read transaction (the MCP
/// server, Homun) the WAL cannot be truncated. Purge must NOT say "done":
/// `scrubbed = false`, `pending` holds `"wal"`, the roots are still in the
/// `-wal` — and once the reader leaves, the NEXT WRITE retries the checkpoint
/// and the scrub completes.
#[test]
fn purge_with_an_open_reader_completes_the_scrub_on_the_next_write() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("lettore.sqlite");
    let cell = Cell::create(&path, "lettore", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "zibellinotremante quokkaviolaceo");
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    let needles = roots_of(&secret.body, Some(Lang::It));

    // the concurrent reader: its own connection, one open read transaction
    let mut reader = rusqlite::Connection::open(&path).unwrap(/* test-only */);
    let read_tx = reader
        .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
        .unwrap(/* test-only */);
    let _n: i64 = read_tx
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap(/* test-only */);

    let report = cell
        .purge(&secret.id, "diritto all'oblio", "tester")
        .unwrap(/* test-only */);
    assert!(report.forgotten_payloads >= 1, "{report:?}");
    assert!(!report.scrubbed, "purge cannot say done: {report:?}");
    assert_eq!(report.pending, vec!["wal"], "{report:?}");
    // and it is true: the roots are still in the WAL frames
    let wal = dir.path().join("lettore.sqlite-wal");
    assert!(
        leaked(&wal, &needles).is_some(),
        "the WAL must still hold the roots while the reader is open"
    );
    let (scrubbed, pending) = cell.scrub_state().unwrap(/* test-only */);
    assert!(!scrubbed && pending == vec!["wal"], "still pending");

    // the reader goes away; the next write retries and completes the scrub
    drop(read_tx);
    drop(reader);
    let after = note_of(2, "scrittura successiva");
    cell.put_note(&after, "tester").unwrap(/* test-only */);
    let (scrubbed, pending) = cell.scrub_state().unwrap(/* test-only */);
    assert!(scrubbed && pending.is_empty(), "the scrub must complete");
    assert!(cell.verify().unwrap(/* test-only */).ok());
    drop(cell);
    assert_forgotten(dir.path(), "lettore", &needles);
}
