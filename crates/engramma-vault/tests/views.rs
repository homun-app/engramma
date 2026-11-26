// SPDX-License-Identifier: Apache-2.0

//! v0.4 (P0.1): a view has an IDENTITY, not a path. `vault init` writes a
//! `view_id` (ULID) into `engramma.json`; the cell binds `vault_views`
//! rows (and the view's files and conflict copies) to that id. Moving the
//! folder and syncing updates the root — one row, not two — and a purge
//! reaches the view at its NEW place. A view nobody can reach can be
//! unbound (`vault unbind`), and a purge only stays pending for a view
//! that still HOLDS something of the note.

mod common;

use std::path::Path;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use engramma_vault::Vault;

fn ts(day: u8) -> At {
    At(
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
        .unwrap(/* test-only */),
    )
}

fn note(id: u16, title: &str, body: &str) -> Note {
    Note {
        id: ulid::Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some(title.into()),
        body: body.into(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: ts(1),
        updated_at: ts(1),
        extensions: vec![],
    }
}

fn setup(dir: &Path) -> (Vault, Note) {
    let cell = Cell::create(&dir.join("c.sqlite"), "t", "tester").unwrap(/* test-only */);
    let vault = Vault::open(cell, &dir.join("vault")).unwrap(/* test-only */);
    let n = note(1, "Nota della vista", "il corpo segreto quokkaviolaceo");
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.init().unwrap(/* test-only */);
    (vault, n)
}

/// The manifest carries the view id, and the cell has exactly one view.
#[test]
fn init_writes_a_view_id_and_binds_one_view() {
    let dir = common::test_dir();
    let (vault, _) = setup(dir.path());
    let manifest = std::fs::read_to_string(vault.root().join("engramma.json")).unwrap(/* t */);
    let v: serde_json::Value = serde_json::from_str(&manifest).unwrap(/* test-only */);
    let vid = v["view_id"].as_str().unwrap_or_default().to_string();
    assert_eq!(vid.len(), 26, "a canonical ULID: {manifest}");
    let views = vault.cell().vault_views().unwrap(/* test-only */);
    assert_eq!(views.len(), 1, "{views:?}");
    assert_eq!(views[0].view_id, vid);
    assert_eq!(
        views[0].root,
        vault.root().canonicalize().unwrap(/* t */).display().to_string()
    );
}

/// The manual proof of the phase: move the folder, sync from the new place
/// — ONE row with the NEW root (the id travelled in the manifest), and the
/// purge that follows finds the view there and forgets the file.
#[test]
fn moved_view_is_one_row_with_the_new_root_and_purge_reaches_it() {
    let dir = common::test_dir();
    let (vault, n) = setup(dir.path());
    let old_root = vault.root().to_path_buf();
    let new_root = dir.path().join("altrove");
    std::fs::rename(&old_root, &new_root).unwrap(/* test-only */);

    let moved = Vault::open(vault.cell().clone(), &new_root).unwrap(/* test-only */);
    let report = moved.sync_once(false).unwrap(/* test-only */);
    assert!(
        report.is_noop(),
        "an unchanged folder at a new place: {report:?}"
    );

    let views = moved.cell().vault_views().unwrap(/* test-only */);
    assert_eq!(
        views.len(),
        1,
        "the identity travelled, not a second view: {views:?}"
    );
    assert_eq!(
        views[0].root,
        new_root.canonicalize().unwrap(/* t */).display().to_string()
    );

    // the purge reaches the view at its new place, right now
    moved.cell().purge(&n.id, "oblio", "t").unwrap(/* t */);
    assert!(
        moved.forget_note(&n.id).is_ok(),
        "the view is reachable at the new root: nothing pending"
    );
    // honest check: no file of the note survives anywhere in the view
    let mut survivors = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap(/* test-only */).flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    walk(&new_root, &mut survivors);
    assert!(
        survivors
            .iter()
            .all(|p| !p.to_string_lossy().contains("quokkaviolaceo")),
        "the purged body must not survive: {survivors:?}"
    );
    assert!(moved
            .cell()
            .vault_views_of_note(&n.id)
            .unwrap(/* test-only */)
            .is_empty());
}

/// A v0.3.x view (no `view_id` in the manifest) receives one at its first
/// sync, keeping the binding the migration gave its root.
#[test]
fn v03x_view_adopts_a_view_id_at_first_sync() {
    let dir = common::test_dir();
    let (vault, _) = setup(dir.path());
    // strip the manifest's view_id: what a v0.3.2 binary left on disk
    let manifest = vault.root().join("engramma.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap(/* t */))
            .unwrap(/* test-only */);
    let mut stripped = v.clone();
    stripped.as_object_mut().unwrap(/* test-only */).remove("view_id");
    std::fs::write(&manifest, stripped.to_string()).unwrap(/* test-only */);

    let report = vault.sync_once(false).unwrap(/* test-only */);
    assert!(report.is_noop(), "{report:?}");
    let after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap(/* t */))
            .unwrap(/* test-only */);
    assert_eq!(
        after["view_id"].as_str().map(str::len),
        Some(26),
        "the first sync writes the id: {}",
        after
    );
    assert_eq!(
        vault.cell().vault_views().unwrap(/* t */).len(),
        1,
        "still one view, not two"
    );
}

/// `unbind` removes the view and ITS bindings (files, conflict copies);
/// the files on disk stay. After it, a purge of a note of that view is
/// complete even though the folder is gone.
#[test]
fn unbind_removes_the_bindings_and_keeps_the_files() {
    let dir = common::test_dir();
    let (vault, n) = setup(dir.path());
    let file = vault
        .cell()
        .vault_files_of_view("nope") // wrong view: empty
        .unwrap(/* test-only */);
    assert!(file.is_empty());
    let views = vault.cell().vault_views().unwrap(/* test-only */);
    let vid = views[0].view_id.clone();
    let bound: Vec<String> = vault
        .cell()
        .vault_files_of_view(&vid)
        .unwrap(/* test-only */)
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(bound.len(), 1, "{bound:?}");

    // the folder goes away, then the view is unbound
    std::fs::remove_dir_all(vault.root()).unwrap(/* test-only */);
    let (removed_files, removed_conflicts) = vault.cell().vault_unbind(&vid).unwrap(/* t */);
    assert_eq!(removed_files, 1);
    assert_eq!(removed_conflicts, 0);
    assert!(vault.cell().vault_views().unwrap(/* t */).is_empty());
    // and the purge finds nothing to wait for
    vault.cell().purge(&n.id, "oblio", "t").unwrap(/* t */);
    assert!(
        vault.cell().vault_views_of_note(&n.id).unwrap(/* t */).is_empty(),
        "no view holds anything of the note"
    );
}

/// A v0.3.2 cell (`vault_roots`, no view ids) migrates: each root becomes a
/// view with a minted id, the files and conflict copies move to the oldest
/// view, and the first sync of the folder adopts the minted id (found by
/// root) instead of adding a second view.
#[test]
fn v032_cell_migrates_roots_to_views_and_the_first_sync_adopts() {
    let dir = common::test_dir();
    let db = dir.path().join("v032.sqlite");
    let cell = Cell::create(&db, "t", "tester").unwrap(/* t */);
    let vault_dir = dir.path().join("vista");
    let vault = Vault::open(cell, &vault_dir).unwrap(/* test-only */);
    let n = note(2, "Nota v0.3.2", "corpo della nota v0.3.2");
    vault.cell().put_note(&n, "t").unwrap(/* t */);
    vault.init().unwrap(/* test-only */);

    // back to the v0.3.2 shape: vault_roots keyed by path, files and
    // conflicts with no view, schema 11, no view_id in the manifest
    let manifest = vault_dir.join("engramma.json");
    let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap(/* t */))
            .unwrap(/* test-only */);
    v.as_object_mut()
        .unwrap(/* test-only */)
        .remove("view_id");
    std::fs::write(&manifest, v.to_string()).unwrap(/* test-only */);
    {
        let conn = rusqlite::Connection::open(&db)
            .unwrap(/* test-only */);
        conn.execute_batch(
            "CREATE TABLE vault_roots (root TEXT PRIMARY KEY, bound_at TEXT NOT NULL);
             CREATE TABLE vault_conflicts_v11 (path TEXT PRIMARY KEY,
                                               note_id TEXT NOT NULL,
                                               created_at TEXT NOT NULL);
             CREATE TABLE vault_files_v11 (path TEXT PRIMARY KEY,
                                           note_id TEXT NOT NULL,
                                           file_hash TEXT NOT NULL,
                                           note_version INTEGER NOT NULL,
                                           mtime INTEGER NOT NULL,
                                           size INTEGER NOT NULL);
             INSERT INTO vault_roots (root, bound_at)
                 SELECT root, bound_at FROM vault_views;
             INSERT INTO vault_files_v11 SELECT path, note_id, file_hash, note_version,
                                              mtime, size FROM vault_files;
             INSERT INTO vault_conflicts_v11 SELECT path, note_id, created_at
                 FROM vault_conflicts;
             DROP TABLE vault_views;
             DROP TABLE vault_files;
             ALTER TABLE vault_files_v11 RENAME TO vault_files;
             CREATE UNIQUE INDEX idx_vault_files_note_v11 ON vault_files(note_id);
             DROP TABLE vault_conflicts;
             ALTER TABLE vault_conflicts_v11 RENAME TO vault_conflicts;
             CREATE INDEX idx_vault_conflicts_note_v11 ON vault_conflicts(note_id);
             UPDATE meta SET value = '11' WHERE key = 'schema_version';
             -- a real v0.3.2 cell has neither the v13 tables nor the v0.5
             -- digest-widening bookmark
             DROP TABLE IF EXISTS proposals;
             DROP TABLE IF EXISTS decisions;
             DROP TABLE IF EXISTS notes_fts_hidden;  -- v14: a v11 cell has one index
             -- v15/v16 left columns and tables behind: rebuild ops without
             -- them, as a real v11 cell would be
             CREATE TABLE ops_v11 (seq INTEGER PRIMARY KEY, op_id TEXT UNIQUE, hlc TEXT, actor TEXT,
                                   kind TEXT, note_id TEXT, payload_digest TEXT,
                                   prev_hash TEXT, hash TEXT);
             INSERT INTO ops_v11 SELECT seq, op_id, hlc, actor, kind, note_id, payload_digest,
                                         prev_hash, hash FROM ops;
             DROP TABLE ops;
             ALTER TABLE ops_v11 RENAME TO ops;
             DROP TABLE IF EXISTS op_objects;
             DROP TABLE IF EXISTS pending_objects;
             DROP TABLE IF EXISTS conflict_copies;
             DROP TABLE IF EXISTS device_acks;  -- v18  -- v17
             DROP TABLE IF EXISTS object_anchors;  -- v23 (v0.6.13, M1)
             DELETE FROM meta WHERE key = 'dump_v13';",
        )
        .unwrap(/* test-only */);
    }
    drop(vault);

    // reopen: the v12 migration runs
    let cell = Cell::open(&db).unwrap(/* t */);
    let views = cell.vault_views().unwrap(/* test-only */);
    assert_eq!(views.len(), 1, "{views:?}");
    assert_eq!(
        views[0].root,
        vault_dir
            .canonicalize()
            .unwrap(/* t */)
            .display()
            .to_string()
    );
    let minted = views[0].view_id.clone();
    let files = cell.vault_files_of_view(&minted).unwrap(/* t */);
    assert_eq!(files.len(), 1, "the legacy rows belong to the minted view");

    // the first sync of the (manifest-less) folder adopts the minted id
    let vault = Vault::open(cell, &vault_dir).unwrap(/* test-only */);
    let report = vault.sync_once(false).unwrap(/* test-only */);
    assert!(report.is_noop(), "{report:?}");
    let views = vault.cell().vault_views().unwrap(/* t */);
    assert_eq!(views.len(), 1, "adopted, not duplicated: {views:?}");
    assert_eq!(views[0].view_id, minted);
    let after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap(/* t */))
            .unwrap(/* test-only */);
    assert_eq!(after["view_id"].as_str(), Some(minted.as_str()));
}

/// v0.4 (P0.2): a `.conflitto` copy made by a v0.3.1 binary is not in
/// `vault_conflicts` — a purge would never find it. The scanner registers
/// it for the note its base file is bound to; an orphan copy (no bound
/// base file) stays the user's and is warned about ONCE, not at every sync.
#[test]
fn pre_v032_conflict_copy_is_registered_and_purged() {
    let dir = common::test_dir();
    let (vault, n) = setup(dir.path());
    // the bound file, as the last sync saw it
    let bound: Vec<String> = {
        let views = vault.cell().vault_views().unwrap(/* t */);
        vault
            .cell()
            .vault_files_of_view(&views[0].view_id)
            .unwrap(/* t */)
            .into_iter()
            .map(|f| f.path)
            .collect()
    };
    assert_eq!(bound.len(), 1, "{bound:?}");
    // a v0.3.1-style copy, hand-made next to it: `<base>.conflitto-<ts>.md`
    let base = bound[0].clone();
    let copy = format!(
        "{}.conflitto-2026-01-01T00_00_00.md",
        base.trim_end_matches(".md")
    );
    std::fs::write(
        vault.root().join(&copy),
        "versione persa col vecchio binario, corpo quokkaviolaceo",
    )
    .unwrap(/* test-only */);

    let report = vault.sync_once(false).unwrap(/* test-only */);
    assert!(
        report.warnings.iter().all(|w| !w.contains(&copy)),
        "a copy with a BOUND base file is not an orphan: {report:?}"
    );
    let views = vault.cell().vault_views().unwrap(/* t */);
    assert_eq!(
        vault
            .cell()
            .vault_conflicts_of(&views[0].view_id, &n.id)
            .unwrap(/* t */),
        vec![copy.clone()],
        "the legacy copy is registered for its note"
    );

    // and the purge of the note takes it away
    vault.cell().purge(&n.id, "oblio", "t").unwrap(/* t */);
    vault.forget_note(&n.id).unwrap(/* test-only */);
    assert!(
        !vault.root().join(&copy).exists(),
        "the legacy conflict copy must not survive the purge"
    );
}

/// The orphan half of P0.2: a copy whose base file is NOT bound stays the
/// user's, warned about once — the second quiet sync says nothing.
#[test]
fn orphan_conflict_copy_is_warned_once_and_stays() {
    let dir = common::test_dir();
    let (vault, _) = setup(dir.path());
    let orphan = "fact/sconosciuta.conflitto-2025-06-01T12_00_00.md";
    std::fs::create_dir_all(vault.root().join("fact")).unwrap(/* t */);
    std::fs::write(
        vault.root().join(orphan),
        "una copia orfana, il suo file base non è legato a nessuna nota",
    )
    .unwrap(/* test-only */);

    let first = vault.sync_once(false).unwrap(/* test-only */);
    assert!(
        first
            .warnings
            .iter()
            .any(|w| w.contains(orphan) && w.contains("orphan")),
        "the orphan is listed, once: {first:?}"
    );
    assert!(
        vault.root().join(orphan).exists(),
        "an orphan stays the user's"
    );
    // the copy becomes a known orphan: no warning at the next quiet sync
    let second = vault.sync_once(false).unwrap(/* test-only */);
    assert!(
        second.warnings.iter().all(|w| !w.contains(orphan)),
        "warned once, not at every sync: {second:?}"
    );
    // …and when the user deletes it by hand, the marker row goes too
    std::fs::remove_file(vault.root().join(orphan)).unwrap(/* t */);
    let third = vault.sync_once(false).unwrap(/* test-only */);
    assert!(third.is_noop(), "{third:?}");
}
