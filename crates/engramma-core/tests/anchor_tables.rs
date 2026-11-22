// SPDX-License-Identifier: Apache-2.0

//! v0.6.13 (M2): the two lists are held honest by the SCHEMA itself.
//! The anchor of a note (M1) is every derived row a reconstruction
//! deletes — the v0.6.12 rule listed the version rows alone, and the
//! queue, the registry and the `supersedes` rows opened the hole the
//! reviewer walked through. The next table must not open it a third
//! time: this test reads the REAL schema (`sqlite_master`), finds
//! every table with a note column, and fails when one is in neither
//! list — the anchor's (`ANCHOR_NOTE_TABLES`) or the declared-out one
//! (`NON_ANCHOR_NOTE_TABLES`, each with its reason). The lists are
//! written ONCE, in the engine, beside the code that deletes and
//! restores the rows.

use engramma_core::cell::schema::{ANCHOR_NOTE_TABLES, NON_ANCHOR_NOTE_TABLES};

fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

/// The columns that make a table speak of ONE note — the note's own id
/// under any of its names (`note_id`, `new_id`, `copy_id`,
/// `original_id`, `proposal`, `target`) or the object key the
/// per-object materialization walks (`object`).
const NOTE_COLUMNS: &[&str] = &[
    "note_id",
    "new_id",
    "old_id",
    "copy_id",
    "original_id",
    "proposal",
    "target",
    "object",
];

#[test]
fn every_table_with_a_note_column_is_anchor_or_declared_out() {
    let dir = test_dir();
    let cell = engramma_core::Cell::create(&dir.path().join("tabelle.sqlite"), "p", "tabelle")
        .expect(/* t */ "a fresh cell holds the whole schema");
    let mut stmt = cell
        .debug_column("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect(/* t */ "sqlite_master is readable")
        .into_iter();
    let _ = &mut stmt;
    let tables = cell
        .debug_column("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect(/* t */ "readable");
    let mut missing: Vec<String> = Vec::new();
    for table in &tables {
        // the FTS shadow tables are the index's own internals (rebuilt
        // from the content table, never deleted by a reconstruction)
        if table.starts_with("notes_fts") {
            continue;
        }
        let cols = cell
            .debug_column(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .expect(/* t */ "readable");
        let speaks_of_a_note = table == "notes"
            || cols
                .iter()
                .any(|c| NOTE_COLUMNS.contains(&c.as_str()));
        if !speaks_of_a_note {
            continue;
        }
        let anchored = ANCHOR_NOTE_TABLES.contains(&table.as_str());
        let declared_out = NON_ANCHOR_NOTE_TABLES.iter().any(|(t, _)| t == table);
        if !anchored && !declared_out {
            missing.push(table.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "tables with a note column in neither the anchor nor the declared-out list \
         (a reconstruction that deletes them loses them below the cut): {missing:?} \
         — add them to ANCHOR_NOTE_TABLES or to NON_ANCHOR_NOTE_TABLES with a reason"
    );
    // and the lists themselves name tables that exist — a renamed table
    // must not leave a dead entry behind
    for t in ANCHOR_NOTE_TABLES {
        assert!(
            tables.contains(&t.to_string()),
            "ANCHOR_NOTE_TABLES names `{t}`, which the schema does not have"
        );
    }
    for (t, _) in NON_ANCHOR_NOTE_TABLES {
        assert!(
            tables.contains(&t.to_string()),
            "NON_ANCHOR_NOTE_TABLES names `{t}`, which the schema does not have"
        );
    }
}
