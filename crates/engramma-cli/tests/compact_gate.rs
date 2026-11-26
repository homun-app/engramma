// SPDX-License-Identifier: Apache-2.0

//! v0.6.15 (F2): `engramma compact` is behind `--experimental` (the
//! beta knew it as `--sperimentale`, still accepted). Without the flag
//! it is a USAGE refusal (exit 2) that says why — the declared defects
//! of the cut are open — and the cell does not change: no checkpoint
//! row, verify green. With the flag it compacts as it always did (the
//! cut only goes forward, v0.6.14).

mod common;

use std::process::Command;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};

fn note(n: u16, body: &str) -> Note {
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* test-only */);
    Note {
        id: ulid::Ulid::from_string(&format!("01{:024}", n)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some(format!("nota {n}")),
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
        created_at: At(t),
        updated_at: At(t),
        extensions: vec![],
    }
}

/// The compaction cut in force: 0 when the cell never compacted (the
/// checkpoint row lives in `snapshots`, file or no file).
fn cut_of(path: &std::path::Path) -> i64 {
    let cell = Cell::open(path).unwrap(/* t */);
    cell.debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM snapshots")
        .unwrap(/* t */)
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn compact_without_the_switch_refuses_and_moves_nothing() {
    let dir = common::test_dir();
    let cells = dir.path().join("cells");
    std::fs::create_dir_all(&cells).unwrap(/* t */);
    let db = cells.join("gate.sqlite");
    {
        let cell = Cell::create(&db, "gate", "test").unwrap(/* t */);
        cell.put_note(&note(1, "una nota [[Roma]]"), "test").unwrap(/* t */);
        cell.put_note(&note(2, "un'altra [[Roma]]"), "test").unwrap(/* t */);
    }

    // without the flag: a usage refusal that says why
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(dir.path())
        .arg("--cell")
        .arg("gate")
        .arg("compact")
        .output()
        .unwrap(/* t */);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a usage error, like every bad argument"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--experimental"),
        "the message names the switch: {stderr}"
    );
    assert!(
        stderr.contains("open-defects.txt"),
        "the message names the reason: {stderr}"
    );

    // the cell did not move: no checkpoint row, verify green
    assert_eq!(cut_of(&db), 0, "no cut landed");
    let verify = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(dir.path())
        .arg("--cell")
        .arg("gate")
        .arg("verify")
        .output()
        .unwrap(/* t */);
    assert!(
        verify.status.success(),
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );

    // with the flag: the compaction runs as it always did
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(dir.path())
        .arg("--cell")
        .arg("gate")
        .arg("compact")
        .arg("--experimental")
        .arg("--keep-last")
        .arg("1")
        .output()
        .unwrap(/* t */);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cut_of(&db) >= 1, "the cut landed");

    // v0.6.19: the beta's name of the switch still answers (the scripts
    // that use it must not break) — it is accepted, not a usage error
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(dir.path())
        .arg("--cell")
        .arg("gate")
        .arg("compact")
        .arg("--sperimentale")
        .output()
        .unwrap(/* t */);
    assert!(
        out.status.success(),
        "the beta's `--sperimentale` is still the switch: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
