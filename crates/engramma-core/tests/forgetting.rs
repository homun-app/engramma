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

/// The snapshots of a cell, as registered at purge time (seq, head hash,
/// digest, file name, why it was redacted).
#[derive(Debug)]
struct SnapshotRow {
    #[allow(dead_code, reason = "read for completeness of the row's picture")]
    seq: i64,
    head_hash: String,
    state_digest: String,
    #[allow(dead_code, reason = "read for completeness of the row's picture")]
    file: String,
    redacted_by: String,
}

fn snapshots_of(cell: &Cell) -> Vec<SnapshotRow> {
    cell.debug_column(
        "SELECT seq || '|' || head_hash || '|' || state_digest || '|' || file || '|' \
          || COALESCE(redacted_by, 'MISSING')
         FROM snapshots ORDER BY seq",
    )
    .unwrap(/* test-only */)
    .into_iter()
    .map(|row| {
        let mut parts = row.splitn(5, '|');
        let mut next = move || parts.next().unwrap_or_default().to_string();
        SnapshotRow {
            seq: next().parse().unwrap_or(0),
            head_hash: next(),
            state_digest: next(),
            file: next(),
            redacted_by: next(),
        }
    })
    .collect()
}

/// P0.3: a purge must reach every registered snapshot — the snapshot is a
/// full copy made before the purge, roots and all. After the purge the file
/// is scrubbed, its row records a new digest under the same head hash with
/// `redacted_by` naming the purge op, and `verify` accepts it (the tail
/// replay, purge included, reproduces the live state).
#[test]
fn compact_then_purge_scrubs_the_snapshot() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("istantanea.sqlite");
    let cell = Cell::create(&path, "istantanea", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "zibellinotremante quokkaviolaceo");
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    let innocent = note_of(2, "una nota che resta");
    cell.put_note(&innocent, "tester").unwrap(/* test-only */);
    let needles = roots_of(&secret.body, Some(Lang::It));

    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let snap = cell.compact(head).unwrap(/* test-only */);
    assert!(snap.is_file(), "{}", snap.display());
    let before = snapshots_of(&cell);
    assert_eq!(before.len(), 1, "{before:?}");

    let report = cell
        .purge(&secret.id, "oblio anche dall'istantanea", "tester")
        .unwrap(/* test-only */);
    assert!(report.scrubbed, "{report:?}");
    // the snapshot row: same head hash, new digest, redacted_by names the purge
    let after = snapshots_of(&cell);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].head_hash, before[0].head_hash, "head stays");
    assert_ne!(after[0].state_digest, before[0].state_digest);
    let purge_op = cell
        .ops()
        .unwrap(/* test-only */)
        .into_iter()
        .rev()
        .find(|o| o.kind == engramma_core::cell::ops::OpKind::Purge)
        .unwrap(/* test-only */);
    assert!(
        after[0].redacted_by.contains(&purge_op.op_id.to_string()),
        "redacted_by must name the purge op: {}",
        after[0].redacted_by
    );
    // verify accepts the redacted snapshot: the tail replay (purge included)
    // reproduces the live state
    let report = cell.verify().unwrap(/* test-only */);
    assert!(
        report.ok(),
        "verify after snapshot purge: {:?}",
        report.mismatch
    );
    assert!(report.snapshot_ok, "{report:?}");
    drop(cell);
    assert_forgotten(dir.path(), "istantanea", &needles);
}

/// P0.3, the hard case: the purged note was the NEW note of a supersede that
/// sits INSIDE the snapshot. The in-snapshot purge must censor that supersede
/// payload and rehash the snapshot's log tail exactly like the live purge
/// did — only then does the tail replay reproduce the live state.
#[test]
fn snapshot_purge_censors_the_supersede_inside_it() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("sopr.sqlite");
    let cell = Cell::create(&path, "t", "tester").unwrap(/* test-only */);
    let mut old = note_of(1, "la vecchia versione resta");
    old.valid_from = Some(engramma_core::Validity::parse("2024-01-01").unwrap(/* t */));
    cell.put_note(&old, "tester").unwrap(/* test-only */);
    let mut secret = note_of(2, "zibellinotremante quokkaviolaceo");
    secret.supersedes = vec![old.id];
    secret.valid_from = Some(engramma_core::Validity::parse("2026-09-01").unwrap(/* t */));
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    // close the old note through the real supersede op (payload carries the
    // secret note's content: the censor case)
    let mut replacement = note_of(3, "il fatto nuovo dopo la chiusura");
    replacement.supersedes = vec![old.id];
    replacement.valid_from = Some(engramma_core::Validity::parse("2026-09-01").unwrap(/* t */));
    cell.supersede(&old.id, &secret, None, "tester")
        .unwrap(/* test-only */);
    let needles = roots_of(&secret.body, Some(Lang::It));

    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    cell.compact(head).unwrap(/* test-only */);

    cell.purge(&secret.id, "oblio", "tester").unwrap(/* t */);
    let report = cell.verify().unwrap(/* test-only */);
    assert!(
        report.ok() && report.snapshot_ok,
        "verify after the in-snapshot censor: {:?}",
        report.mismatch
    );
    // the old note is still closed, the censored supersede still replays it
    let back = cell.get(&old.id).unwrap(/* test-only */);
    assert_eq!(back.status, Status::Superseded);
    drop(cell);
    assert_forgotten(dir.path(), "sopr", &needles);
}

/// P0.4: a compaction snapshot is the LOGICAL state — payloads up to the
/// cut, vectors, vault bindings and the FTS content are derived or
/// recomputable, and they all carry content. And only the LAST snapshot
/// stays: two compacts leave one row and one file.
#[test]
fn compact_makes_a_logical_snapshot_and_keeps_only_the_last() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("logica.sqlite");
    let cell = Cell::create(&path, "logica", "tester").unwrap(/* test-only */);
    for i in 1..=3_u16 {
        cell.put_note(&note_of(i, &format!("nota {i} che resta")), "tester")
            .unwrap(/* test-only */);
    }
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let snap = cell.compact(head - 1).unwrap(/* test-only */);
    assert!(snap.is_file(), "{}", snap.display());

    // the snapshot carries the logical state only
    let snap_conn = rusqlite::Connection::open(&snap).unwrap(/* test-only */);
    let count = |sql: &str| -> i64 {
        snap_conn
            .query_row(sql, [], |r| r.get(0))
            .unwrap_or(/* test-only */ -1)
    };
    assert_eq!(
        count(
            "SELECT COUNT(*) FROM op_payloads p JOIN ops o ON o.op_id = p.op_id WHERE o.seq <= (SELECT MAX(seq) FROM ops) - 1"
        ),
        0,
        "no payload up to the cut"
    );
    // the tail payload (the last op) stays replayable
    assert_eq!(
        count("SELECT COUNT(*) FROM op_payloads"),
        1,
        "exactly the head payload stays"
    );
    assert_eq!(count("SELECT COUNT(*) FROM note_vectors"), 0, "vectors go");
    assert_eq!(
        count("SELECT COUNT(*) FROM embedding_models"),
        0,
        "the model registry goes"
    );
    assert_eq!(count("SELECT COUNT(*) FROM vault_files"), 0, "bindings go");
    assert_eq!(
        count("SELECT COUNT(*) FROM notes_fts_docsize"),
        0,
        "the FTS content goes (recomputed at verify)"
    );
    drop(snap_conn);
    // and the cell still verifies with it
    assert!(cell.verify().unwrap(/* test-only */).ok());

    // a second compact after one more write: ONE row, ONE file
    cell.put_note(&note_of(9, "dopo la prima compattazione"), "tester")
        .unwrap(/* test-only */);
    let head2: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let snap2 = cell.compact(head2 - 1).unwrap(/* test-only */);
    assert!(snap2.is_file(), "{}", snap2.display());
    assert!(cell.verify().unwrap(/* test-only */).ok());
    let rows = snapshots_of(&cell);
    assert_eq!(rows.len(), 1, "only the last snapshot stays: {rows:?}");
    let snaps: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap(/* test-only */)
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("logica-snap-"))
        })
        .collect();
    assert_eq!(snaps.len(), 1, "one snapshot file: {snaps:?}");
    // the old file is really gone
    assert!(!snap.exists() || snap == snap2, "the old file went away");
}
