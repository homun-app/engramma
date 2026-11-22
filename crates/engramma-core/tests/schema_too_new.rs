// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P0): a cell whose schema is newer than the binary must refuse to
//! open, on every surface (CLI, MCP, Node, vault — they all come through
//! [`Cell::open`]). Before the guard, an old binary opened the cell, applied
//! "no" migrations (its list was exhausted) and kept writing: the derived
//! tables it did not know would diverge from the log. With the operations of
//! this phase (a derived proposals table maintained by `apply`) the divergence
//! would be silent state corruption, so the refusal is the only safe answer.

use engramma_core::cell::schema::SCHEMA_VERSION;
use engramma_core::{Cell, CellError};

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

#[test]
fn a_schema_newer_than_the_binary_refuses_to_open() {
    let dir = test_dir();
    let path = dir.path().join("c.sqlite");
    {
        let cell = Cell::create(&path, "t", "t").unwrap(/* test-only */);
        cell.checkpoint().unwrap(/* test-only */); // fold the WAL in
    }
    // A newer binary migrated this cell one step further.
    let conn = rusqlite::Connection::open(&path).unwrap(/* test-only */);
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
        [(SCHEMA_VERSION + 1).to_string()],
    )
    .unwrap(/* test-only */);
    drop(conn);
    match Cell::open(&path) {
        Err(CellError::SchemaTooNew { found, supported }) => {
            assert_eq!(found, SCHEMA_VERSION + 1);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        Err(other) => panic!("expected SchemaTooNew, got: {other}"),
        Ok(_cell) => panic!(
            "a schema-newer cell opened: the old binary would write under \
             tables it does not know"
        ),
    }
    // And the refusal left the cell exactly as it was: the version we wrote
    // is still there, unread and unmigrated.
    let conn = rusqlite::Connection::open(&path).unwrap(/* test-only */);
    let v: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap(/* test-only */);
    assert_eq!(v, (SCHEMA_VERSION + 1).to_string());
}

/// The guard must not fire on the binary's own cells: a fresh cell opens, a
/// current-schema cell reopens.
#[test]
fn the_own_schema_still_opens() {
    let dir = test_dir();
    let path = dir.path().join("c.sqlite");
    {
        let cell = Cell::create(&path, "t", "t").unwrap(/* test-only */);
        cell.checkpoint().unwrap(/* test-only */);
    }
    let conn = rusqlite::Connection::open(&path).unwrap(/* test-only */);
    let v: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap(/* test-only */);
    drop(conn);
    assert_eq!(v.parse::<i64>().unwrap(/* test-only */), SCHEMA_VERSION);
    Cell::open(&path).unwrap(/* test-only */);
}
