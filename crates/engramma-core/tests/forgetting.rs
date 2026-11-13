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

use engramma_core::cell::{CompactOptions, Replay};
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
        "SELECT seq || '|' || head_hash || '|' || COALESCE(state_digest, 'NULL') \
          || '|' || COALESCE(file, 'NULL') || '|' || COALESCE(redacted_by, 'MISSING')
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
    let snap = cell
        .compact(head, &CompactOptions { keep_snapshot: true })
        .unwrap(/* test-only */)
        .snapshot
        .expect(/* test-only */ "keep_snapshot writes a file");
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
    cell.compact(head, &CompactOptions { keep_snapshot: true })
        .unwrap(/* test-only */);

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

/// v0.3.2 (P0.3): the digest of a checkpoint describes a state that
/// contained the purged note. It is an hash of the whole state — not
/// readable data — but from the purge on it is unverifiable, and purge
/// nulls it and appends its op to `redacted_by`. With the FILE the
/// v0.3.1 behavior stands (the snapshot is scrubbed, digest updated).
#[test]
fn purge_redacts_the_checkpoint_and_scrubs_the_file() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("redazione.sqlite");
    let cell = Cell::create(&path, "redazione", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "zibellinotremante quokkaviolaceo");
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    cell.put_note(&note_of(2, "una nota che resta"), "tester")
        .unwrap(/* test-only */);
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);

    // checkpoint: no file
    cell.compact(head, &CompactOptions::default())
        .unwrap(/* test-only */);
    let before = snapshots_of(&cell);
    assert_eq!(before.len(), 1);
    assert_ne!(before[0].state_digest, "NULL", "{before:?}");
    assert_eq!(before[0].redacted_by, "[]", "{before:?}");

    cell.purge(&secret.id, "oblio", "tester").unwrap(/* t */);
    let after = snapshots_of(&cell);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(
        after[0].state_digest, "NULL",
        "the checkpoint digest is redacted: {after:?}"
    );
    let purge_op = cell
        .ops()
        .unwrap(/* test-only */)
        .into_iter()
        .rev()
        .find(|o| o.kind == engramma_core::cell::ops::OpKind::Purge)
        .unwrap(/* test-only */);
    assert!(
        after[0].redacted_by.contains(&purge_op.op_id.to_string()),
        "{}",
        after[0].redacted_by
    );
    // and verify still passes (chain + derived indexes)
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(vr.ok(), "{vr:?}");

    // with the FILE, the v0.3.1 behavior: scrubbed, digest updated, named
    let path2 = dir.path().join("file.sqlite");
    let cell = Cell::create(&path2, "file", "tester").unwrap(/* test-only */);
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    let head2: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    cell.compact(head2, &CompactOptions { keep_snapshot: true })
        .unwrap(/* test-only */);
    let before2 = snapshots_of(&cell);
    cell.purge(&secret.id, "oblio", "tester").unwrap(/* t */);
    let after2 = snapshots_of(&cell);
    assert_eq!(after2.len(), 1, "{after2:?}");
    assert_ne!(after2[0].state_digest, before2[0].state_digest);
    assert!(
        after2[0].redacted_by.contains(&purge_of(&cell).to_string()),
        "{}",
        after2[0].redacted_by
    );
    assert!(cell.verify().unwrap(/* test-only */).ok());
}

/// The op id of the newest purge in the log.
fn purge_of(cell: &Cell) -> ulid::Ulid {
    cell.ops()
        .unwrap(/* test-only */)
        .into_iter()
        .rev()
        .find(|o| o.kind == engramma_core::cell::ops::OpKind::Purge)
        .unwrap(/* test-only */)
        .op_id
}

/// v0.3.2 (P0.2): with a checkpoint (no file) `verify` cannot replay — but
/// it still checks the COHERENCE of what is on disk: a derived edge changed
/// by hand, a vector for a note that no longer lives, and (digest at the
/// head) a note row changed by hand.
#[test]
fn chain_only_catches_tampering_below_the_replay() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    // 1. a tampered DERIVED EDGE fails the coherence check
    let path = dir.path().join("archi.sqlite");
    let cell = Cell::create(&path, "archi", "tester").unwrap(/* test-only */);
    cell.put_note(&note_of(1, "una nota che cita [[Roma]]"), "tester")
        .unwrap(/* test-only */);
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    cell.compact(head, &CompactOptions::default())
        .unwrap(/* test-only */);
    // the head moves past the checkpoint: the recorded digest is history
    // now, only the per-note re-derivation of the derived edges can catch
    // a hand-changed `edges` row
    cell.put_note(&note_of(2, "una seconda nota [[Roma]]"), "tester")
        .unwrap(/* test-only */);
    {
        let conn = rusqlite::Connection::open(&path).unwrap(/* test-only */);
        conn.execute(
            "UPDATE edges SET dst = 'Milano' WHERE rel = 'mentions' AND id LIKE 'd%'",
            [],
        )
        .unwrap(/* test-only */);
    }
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(
        !vr.ok() && vr.replay == Replay::ChainOnly { since_seq: head },
        "a tampered derived edge must fail: {vr:?}"
    );

    // 2. a VECTOR for a note that is not living fails too
    let path2 = dir.path().join("vettori.sqlite");
    let cell = Cell::create(&path2, "vettori", "tester").unwrap(/* test-only */);
    cell.put_note(&note_of(1, "un'altra nota [[Roma]]"), "tester")
        .unwrap(/* test-only */);
    let head2: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    cell.compact(head2, &CompactOptions::default())
        .unwrap(/* test-only */);
    {
        let conn = rusqlite::Connection::open(&path2).unwrap(/* test-only */);
        conn.execute(
            "INSERT INTO note_vectors (note_id, chunk, model, content_hash, note_hash, vec)
             VALUES ('01ZZZZZZZZZZZZZZZZZZZZZZZZ', 0, 'x', 'h', 'h', x'00000000')",
            [],
        )
        .unwrap(/* test-only */);
    }
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(!vr.ok(), "a vector for a non-living note must fail: {vr:?}");

    // 3. the digest at the head: a note row changed by hand moves the state
    //    digest away from the checkpoint's record
    let path3 = dir.path().join("digest.sqlite");
    let cell = Cell::create(&path3, "digest", "tester").unwrap(/* test-only */);
    cell.put_note(&note_of(1, "la nota del digest [[Roma]]"), "tester")
        .unwrap(/* test-only */);
    let head3: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    cell.compact(head3, &CompactOptions::default())
        .unwrap(/* test-only */);
    assert!(cell.verify().unwrap(/* test-only */).ok());
    {
        let conn = rusqlite::Connection::open(&path3).unwrap(/* test-only */);
        conn.execute("UPDATE notes SET title = 'manomesso' WHERE id LIKE '%'", [])
            .unwrap(/* test-only */);
    }
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(
        !vr.ok(),
        "a hand-changed note row must move the digest: {vr:?}"
    );
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
    let snap = cell
        .compact(head - 1, &CompactOptions { keep_snapshot: true })
        .unwrap(/* test-only */)
        .snapshot
        .expect(/* test-only */ "keep_snapshot writes a file");
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
    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.replay, Replay::FromSnapshot { seq: head - 1 });

    // v0.3.2 (P0.1): a compact WITHOUT the flag replaces the file snapshot
    // with a checkpoint — the file goes, ONE row stays, without a file
    cell.put_note(&note_of(9, "dopo la prima compattazione"), "tester")
        .unwrap(/* test-only */);
    let head2: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let plain = cell.compact(head2 - 1, &CompactOptions::default())
        .unwrap(/* test-only */);
    assert!(
        plain.snapshot.is_none(),
        "the default compaction keeps no file"
    );
    assert!(plain.bytes_after < plain.bytes_before, "disk usage drops");
    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{report:?}");
    assert_eq!(
        report.replay,
        Replay::ChainOnly {
            since_seq: head2 - 1
        }
    );
    let rows = snapshots_of(&cell);
    assert_eq!(rows.len(), 1, "only the last row stays: {rows:?}");
    assert_eq!(rows[0].file, "NULL", "the row has no file: {rows:?}");
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
    assert!(snaps.is_empty(), "no snapshot file left: {snaps:?}");
    // the old file is really gone
    assert!(!snap.exists(), "the old file went away");
}

/// v0.3.2 (P0.1): the DEFAULT compaction is a checkpoint without a file —
/// no `*-snap-*` next to the cell, the total on disk DROPS, `verify` says
/// `ChainOnly` and passes, and a hand-tampered surviving payload fails it.
#[test]
fn compact_default_is_a_checkpoint_on_disk() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("punto.sqlite");
    let cell = Cell::create(&path, "punto", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "zibellinotremante quokkaviolaceo");
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    // enough payload bytes that forgetting them shrinks the file
    for i in 2..=40_u16 {
        let body = format!("riempimento {i} {}", "parola lunga ".repeat(40));
        cell.put_note(&note_of(i, &body), "tester").unwrap(/* t */);
    }
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);

    let report = cell.compact(head, &CompactOptions::default())
        .unwrap(/* test-only */);
    assert_eq!(report.cut_seq, head);
    assert_eq!(report.head_seq, head);
    assert_ne!(report.head_hash, "");
    assert!(report.snapshot.is_none(), "no file by default");
    // the total on disk (cell + WAL + snapshots) DROPS
    assert!(
        report.bytes_after < report.bytes_before,
        "footprint {} -> {}",
        report.bytes_before,
        report.bytes_after
    );
    // no *-snap-* file next to the cell
    let snaps: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap(/* test-only */)
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("-snap-"))
        })
        .collect();
    assert!(snaps.is_empty(), "unexpected snapshot files: {snaps:?}");
    // one row: the checkpoint, with the digest of the current state
    let live_digest = cell.state_digest().unwrap(/* test-only */);
    assert_eq!(report.state_digest, live_digest);
    let rows = snapshots_of(&cell);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].file, "NULL", "{rows:?}");
    assert_eq!(rows[0].state_digest, live_digest, "{rows:?}");
    // verify: chain-only, and it passes
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(vr.ok(), "{vr:?}");
    assert_eq!(vr.replay, Replay::ChainOnly { since_seq: head });

    // a payload that SURVIVES the cut (a write after it), tampered by hand,
    // fails the verification: the chain check covers every remaining digest
    cell.put_note(&note_of(90, "dopo il punto di controllo"), "tester")
        .unwrap(/* test-only */);
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(vr.ok() && vr.replay == Replay::ChainOnly { since_seq: head });
    {
        let conn = rusqlite::Connection::open(&path).unwrap(/* test-only */);
        conn.execute(
            "UPDATE op_payloads SET body = replace(body, 'dopo il punto', 'DOPO IL PUNTO')",
            [],
        )
        .unwrap(/* test-only */);
    }
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(
        !vr.ok() && !vr.hash_chain_ok,
        "a tampered surviving payload must fail: {vr:?}"
    );
}

/// v0.3.2 (P0.4): the old snapshot FILE goes before its row. When the
/// removal fails (here: a read-only directory), `compact` is an error, the
/// row stays WITH its file — no orphaned file that no row points at — and
/// a later purge scrubs that file all the same.
#[test]
#[cfg(unix)]
fn compact_fails_cleanly_when_the_old_file_cannot_go() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("ordine.sqlite");
    let cell = Cell::create(&path, "ordine", "tester").unwrap(/* test-only */);
    let secret = note_of(1, "zibellinotremante quokkaviolaceo");
    cell.put_note(&secret, "tester").unwrap(/* test-only */);
    cell.put_note(&note_of(2, "una nota che resta"), "tester")
        .unwrap(/* test-only */);
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    cell.compact(head, &CompactOptions { keep_snapshot: true })
        .unwrap(/* test-only */);
    let snap_file = dir.path().join(format!("ordine-snap-{head}.sqlite"));
    assert!(snap_file.is_file(), "{}", snap_file.display());
    let needles = roots_of(&secret.body, Some(Lang::It));

    // one more write, then a directory the compaction cannot remove from
    cell.put_note(&note_of(3, "dopo l'istantanea"), "tester")
        .unwrap(/* test-only */);
    let head2: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let meta = std::fs::metadata(dir.path()).unwrap(/* test-only */);
    let mode = meta.permissions().mode();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))
        .unwrap(/* test-only */);
    // v0.4.4 (P2): root ignores the 0o555 — probe it. If a file can still
    // be created inside, the permissions do not bind and the removal below
    // would succeed: the test would assert a failure that never happens.
    // Skip explicitly instead (no `geteuid`: no unsafe, and capability
    // sets can lie the same way root does).
    let probe = dir.path().join("perm-probe");
    let perms_bind = std::fs::write(&probe, b"probe").is_err();
    if !perms_bind {
        std::fs::remove_file(&probe).ok();
        // restore before anything else can run in this test
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode))
            .unwrap(/* test-only */);
        drop(cell);
        eprintln!(
            "SKIP: {} does not honor directory permissions (root or equivalent); \
             the compaction failure cannot be forced here",
            std::env::consts::OS
        );
        return;
    }
    let failed = cell.compact(head2, &CompactOptions::default());
    // restore before any early return below can skip the cleanup
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode))
        .unwrap(/* test-only */);
    let err = failed.expect_err(/* the removal must fail */ "read-only dir");
    assert!(
        err.to_string().contains("cannot remove the old snapshot"),
        "{err}"
    );
    // the row and its file both stay — verify still passes on them
    let rows = snapshots_of(&cell);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].file,
        format!("ordine-snap-{head}.sqlite"),
        "the old row keeps its file: {rows:?}"
    );
    assert!(snap_file.is_file());
    assert!(cell.verify().unwrap(/* test-only */).ok());

    // the purge scrubs that file: no root of the purged body anywhere
    cell.purge(&secret.id, "oblio", "tester").unwrap(/* t */);
    assert!(cell.verify().unwrap(/* test-only */).ok());
    drop(cell);
    assert_forgotten(dir.path(), "ordine", &needles);
}

/// v0.3.2 (P0.5): a v0.3.1 cell (schema 9, `snapshots` with NOT NULL file
/// and digest, a logical snapshot on disk) opens through the migration,
/// `verify` passes as before (`FromSnapshot`), and the FIRST compaction
/// without the flag turns it into a checkpoint and takes the file away.
#[test]
fn a_v031_cell_with_a_snapshot_migrates_and_compacts_to_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let path = dir.path().join("vecchia.sqlite");
    let cell = Cell::create(&path, "vecchia", "tester").unwrap(/* test-only */);
    for i in 1..=3_u16 {
        cell.put_note(&note_of(i, &format!("nota {i} che resta [[Roma]]")), "tester")
            .unwrap(/* test-only */);
    }
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let snap = cell
        .compact(head, &CompactOptions { keep_snapshot: true })
        .unwrap(/* test-only */)
        .snapshot
        .expect(/* test-only */ "keep_snapshot writes a file");
    assert!(snap.is_file());
    drop(cell);

    // the v0.3.1 shape: snapshots with NOT NULL file and digest, schema 9
    // (in both files — the snapshot's own table was emptied by the strip,
    // its `meta` still says the version the copy was taken at). The row's
    // digest is recomputed over the DOWNGRADED snapshot, as a real v0.3.1
    // compaction would have recorded it (the digest covers meta, schema
    // version included).
    let downgrade = |p: &std::path::Path| {
        let conn = rusqlite::Connection::open(p).unwrap(/* test-only */);
        conn.execute_batch(
            "CREATE TABLE snapshots_v9 (seq INTEGER PRIMARY KEY,
                                         head_hash TEXT NOT NULL,
                                         state_digest TEXT NOT NULL,
                                         file TEXT NOT NULL,
                                         redacted_by TEXT NOT NULL DEFAULT '[]',
                                         created_at TEXT NOT NULL);
             INSERT INTO snapshots_v9 SELECT * FROM snapshots;
             DROP TABLE snapshots;
             ALTER TABLE snapshots_v9 RENAME TO snapshots;
             DROP TABLE IF EXISTS vault_views;
             DROP TABLE IF EXISTS vault_roots;
             DROP TABLE IF EXISTS vault_conflicts;
             CREATE TABLE vault_files_v9 (path TEXT PRIMARY KEY,
                                          note_id TEXT NOT NULL,
                                          file_hash TEXT NOT NULL,
                                          note_version INTEGER NOT NULL,
                                          mtime INTEGER NOT NULL,
                                          size INTEGER NOT NULL);
             INSERT INTO vault_files_v9 SELECT path, note_id, file_hash, note_version,
                                            mtime, size FROM vault_files;
             DROP TABLE vault_files;
             ALTER TABLE vault_files_v9 RENAME TO vault_files;
             CREATE UNIQUE INDEX idx_vault_files_note_v9 ON vault_files(note_id);
             DROP TABLE IF EXISTS proposals;
             DROP TABLE IF EXISTS decisions;
             DROP TABLE IF EXISTS notes_fts_hidden;  -- v14: a real v9 cell has one index
             -- v15/v16 left columns and tables behind: rebuild ops without
             -- them and drop the object index, as a real v9 cell would be
             CREATE TABLE ops_v9 (seq INTEGER PRIMARY KEY, op_id TEXT UNIQUE, hlc TEXT, actor TEXT,
                                   kind TEXT, note_id TEXT, payload_digest TEXT,
                                   prev_hash TEXT, hash TEXT);
             INSERT INTO ops_v9 SELECT seq, op_id, hlc, actor, kind, note_id, payload_digest,
                                        prev_hash, hash FROM ops;
             DROP TABLE ops;
             ALTER TABLE ops_v9 RENAME TO ops;
             DROP TABLE IF EXISTS op_objects;
             DROP TABLE IF EXISTS pending_objects;
             UPDATE meta SET value = '9' WHERE key = 'schema_version';
             DELETE FROM meta WHERE key = 'dump_v13';",
        )
        .unwrap(/* test-only */);
    };
    downgrade(&snap);
    let v9_digest = {
        let conn = rusqlite::Connection::open(&snap).unwrap(/* test-only */);
        // the v0.5 dump covers the queue tables: a v9 cell has none. The
        // recorded digest stands for what a v0.3.1 binary wrote over ITS
        // dump; the empty tables live in a throwaway in-memory COPY (adding
        // them to the file itself would collide with the v13 migration the
        // open is about to run). What matters to the fixture is that this
        // value is STALE the moment a v0.5 binary opens the cell: the
        // digest-widening pass re-digests the migrated file and its row.
        let mut mem = rusqlite::Connection::open_in_memory().unwrap(/* test-only */);
        let backup = rusqlite::backup::Backup::new(&conn, &mut mem).unwrap(/* test-only */);
        backup
            .run_to_completion(128, std::time::Duration::from_millis(0), None)
            .unwrap(/* test-only */);
        drop(backup);
        mem.execute_batch(
            "CREATE TABLE proposals (note_id TEXT PRIMARY KEY, mode TEXT NOT NULL,
                                    target TEXT, proposer TEXT NOT NULL, client TEXT,
                                    reason TEXT, state TEXT NOT NULL,
                                    proposed_at TEXT NOT NULL, decision_op TEXT,
                                    decided_by TEXT, decided_at TEXT, decision_reason TEXT,
                                    review_json TEXT,
                                    auto_failed INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE decisions (op_id TEXT PRIMARY KEY, proposal TEXT NOT NULL,
                                     decision TEXT NOT NULL, actor TEXT NOT NULL,
                                     decided_at TEXT NOT NULL, reason TEXT, review_json TEXT,
                                     reverted_by TEXT);",
        )
        .unwrap(/* test-only */);
        engramma_core::cell::state_digest(&mem).unwrap(/* test-only */)
    };
    downgrade(&path);
    {
        let conn = rusqlite::Connection::open(&path).unwrap(/* test-only */);
        conn.execute(
            "UPDATE snapshots SET state_digest = ?1",
            [&v9_digest],
        )
        .unwrap(/* test-only */);
    }

    // it opens (the v10 migration runs), and verify is as before
    let cell = Cell::open(&path).unwrap(/* test-only */);
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(vr.ok(), "{vr:?}");
    assert_eq!(vr.replay, Replay::FromSnapshot { seq: head });
    assert_eq!(
        cell.debug_column("SELECT value FROM meta WHERE key = 'schema_version'")
            .unwrap(/* test-only */),
        vec![engramma_core::cell::schema::SCHEMA_VERSION.to_string()]
    );

    // the first default compaction: checkpoint, the old file goes
    let head2: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let report = cell
        .compact(head2, &CompactOptions::default())
        .unwrap(/* test-only */);
    assert!(report.snapshot.is_none());
    assert!(!snap.exists(), "the v0.3.1 snapshot file is gone");
    let rows = snapshots_of(&cell);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].file, "NULL", "{rows:?}");
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(vr.ok() && vr.replay == Replay::ChainOnly { since_seq: head2 });
}
