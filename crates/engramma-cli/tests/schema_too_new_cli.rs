// SPDX-License-Identifier: Apache-2.0

//! v0.6.17 (F1): the data promise, on the surface a user touches. A cell
//! written by a NEWER Engramma is refused with a readable error (exit 6)
//! and the file stays byte for byte what it was — no half migration, no
//! quiet write under tables this binary does not know. The engine's own
//! guard is `schema_too_new.rs` in engramma-core; this is the CLI's
//! answer, the one the compatibility page quotes.

mod common;

use std::process::Command;

use engramma_core::cell::schema::SCHEMA_VERSION;

#[test]
fn a_newer_cell_is_refused_with_a_readable_error_and_untouched() {
    let dir = common::test_dir();
    let root = dir.path().join("root");
    let init = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&root)
        .arg("init")
        .output()
        .expect(/* test-only */ "the binary runs");
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let cell = root.join("cells").join("default.sqlite");
    assert!(cell.is_file(), "the default cell is there");

    // a newer binary migrated this cell one step further (the same hand
    // the core test plays). The cell stays in ITS journal mode — WAL, the
    // mode a real Engramma keeps — and closing the connection folds the
    // WAL into the file, as the last connection does
    {
        let conn = rusqlite::Connection::open(&cell).unwrap(/* test-only */);
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            [(SCHEMA_VERSION + 1).to_string()],
        )
        .unwrap(/* test-only */);
        // the checkpoint returns a row (busy, log, checkpointed): read it
        let _: (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap(/* test-only: fold the WAL in */);
    }
    let before = std::fs::read(&cell).unwrap(/* test-only */);

    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&root)
        .arg("verify")
        .output()
        .expect(/* test-only */ "the binary runs");
    assert_eq!(
        out.status.code(),
        Some(6),
        "the refusal's exit code (schema too new)"
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("newer than this binary"),
        "the error says what happened: {said}"
    );
    assert!(
        said.contains("left untouched"),
        "the error says the cell was not touched: {said}"
    );

    let after = std::fs::read(&cell).unwrap(/* test-only */);
    assert_eq!(
        before, after,
        "the refused cell is identical, byte for byte"
    );
}
