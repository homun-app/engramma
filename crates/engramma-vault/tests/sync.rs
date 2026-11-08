// SPDX-License-Identifier: Apache-2.0

//! The markdown view tests (prompt v0.3, punto 4): every row of the
//! reconciliation table, the guards, the echo, the fixed-point property and
//! the watch integration.

use engramma_core::format::{At, Lang, Note, NoteType, ParseMode};
use engramma_core::{Cell, CellError, Confidence, NotePatch, Status, Validity};
use engramma_vault::{Vault, VaultError};
use std::path::Path;
use time::OffsetDateTime;
use ulid::Ulid;

fn ts(day: u8) -> At {
    At(
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
        .unwrap(/* test-only */)
        .replace_nanosecond(0)
        .unwrap(/* test-only */),
    )
}

fn note(id: u16, title: &str, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
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

fn setup(dir: &Path) -> Vault {
    let cell = Cell::create(&dir.join("c.sqlite"), "t", "tester").unwrap(/* test-only */);
    let vault_dir = dir.join("vault");
    let vault = Vault::open(cell, &vault_dir).unwrap(/* test-only */);
    vault.init().unwrap(/* test-only */);
    vault
}

fn write(vault: &Vault, rel: &str, text: &str) {
    let target = vault.root().join(rel);
    std::fs::create_dir_all(target.parent().unwrap_or(vault.root())).unwrap(/* t */);
    std::fs::write(target, text).unwrap(/* t */);
}

fn read(vault: &Vault, rel: &str) -> String {
    std::fs::read_to_string(vault.root().join(rel)).unwrap(/* test-only */)
}

/// The (single) view's id bound in the cell — v0.4 (P0.1): files and
/// conflict copies belong to a view now.
fn vid(vault: &Vault) -> String {
    vault.cell().vault_views().unwrap(/* test-only */)[0]
        .view_id
        .clone()
}

/// init exports one file per note; sync is a no-op right after (echo).
#[test]
fn init_then_sync_is_a_noop() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    vault.cell().put_note(&note(1, "prima", "corpo [[Milano]]"), "t")
        .unwrap(/* test-only */);
    // the note arrived after init: the file gets written by sync
    let r1 = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r1.updated_files.len(), 1, "{r1:?}");
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.is_noop(), "fixed point after one reconciliation: {r2:?}");
}

/// file changed, note unchanged → the note is updated from the file.
#[test]
fn changed_file_updates_the_note() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let n = note(1, "prima", "corpo iniziale [[Roma]]");
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* t */);
    let id = n.id.to_string();
    let files = std::fs::read_dir(vault.root().join("fact")).unwrap(/* t */);
    let name = files
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .find(|f| f.starts_with("prima"))
        .unwrap_or_default();
    let text = read(&vault, &format!("fact/{name}"));
    let edited = text.replace("corpo iniziale", "corpo modificato a mano");
    write(&vault, &format!("fact/{name}"), &edited);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.updated_from_file, vec![format!("fact/{name}")], "{r:?}");
    let back = vault.cell().get(&n.id).unwrap(/* test-only */);
    assert!(back.body.contains("corpo modificato a mano"));
    assert_eq!(back.id.to_string(), id);
}

/// note changed from the cell, file untouched → the file is rewritten.
#[test]
fn changed_note_rewrites_the_file() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let n = note(2, "seconda", "corpo [[Milano]]");
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* t */);
    vault
        .cell()
        .patch_note(
            &n.id,
            &NotePatch {
                body: Some("nuovo corpo dalla CLI [[Torino]]".into()),
                ..Default::default()
            },
            "cli",
        )
        .unwrap(/* test-only */);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.updated_files.len(), 1, "{r:?}");
    let files = list_md(&vault);
    let text = std::fs::read_to_string(files[0].clone()).unwrap(/* t */);
    assert!(text.contains("nuovo corpo dalla CLI"), "{text}");
    // and the echo: the next sync is a no-op
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.is_noop(), "{r2:?}");
}

/// both changed → conflict: the cell wins, the file's version is a copy.
#[test]
fn conflict_cell_wins_with_a_copy() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let n = note(3, "terza", "versione base [[Roma]]");
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* test-only */);
    let path = &list_md(&vault)[0];
    let text = std::fs::read_to_string(path).unwrap(/* t */);
    std::fs::write(path, text.replace("versione base", "versione dal file"))
        .unwrap(/* t */);
    vault
        .cell()
        .patch_note(
            &n.id,
            &NotePatch {
                body: Some("versione dalla cella".into()),
                ..Default::default()
            },
            "cli",
        )
        .unwrap(/* test-only */);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.conflicts.len(), 1, "{r:?}");
    let after = std::fs::read_to_string(path).unwrap(/* t */);
    assert!(after.contains("versione dalla cella"), "{after}");
    let copies: Vec<_> = std::fs::read_dir(path.parent().unwrap(/* t */))
        .unwrap(/* t */)
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|f| f.contains(".conflitto-"))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    let copy = std::fs::read_to_string(
        path.parent().unwrap(/* t */).join(&copies[0]),
    )
    .unwrap(/* t */);
    assert!(copy.contains("versione dal file"), "{copy}");
    // the copy is ignored from now on
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.is_noop(), "{r2:?}");
}

/// a new file without id → a note is created, then the file is rewritten
/// WITH the id.
#[test]
fn file_without_id_becomes_a_note_and_gets_one() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    vault.sync_once(false).unwrap(/* t */); // settle init
    write(
        &vault,
        "appunto.md",
        "# Idea\n\nUna nota di [[Obsidian]].\n",
    );
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.created, vec!["appunto.md".to_string()], "{r:?}");
    let text = read(&vault, "appunto.md");
    assert!(
        text.contains("id: 01"),
        "the file now carries the id: {text}"
    );
    let parsed = engramma_core::format::parse(&text, ParseMode::Strict)
        .unwrap(/* test-only */)
        .note;
    assert_eq!(parsed.source, "import:obsidian/appunto.md");
    assert_eq!(parsed.note_type, NoteType::Note);
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.is_noop(), "{r2:?}");
}

/// rename and move: same id, new path — no delete, no new note.
#[test]
fn moved_file_keeps_its_note() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let n = note(4, "quarta", "da spostare [[Milano]]");
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* t */);
    let from = list_md(&vault)[0].clone();
    let to = from.parent().unwrap(/* t */).join("spostata.md");
    std::fs::rename(&from, &to).unwrap(/* t */);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.moved.len(), 1, "{r:?}");
    assert!(r.deleted.is_empty(), "{r:?}");
    let rows = vault.cell().vault_files_of_view(&vid(&vault)).unwrap(/* t */);
    assert!(
        rows.iter().any(|f| f.path.ends_with("spostata.md")),
        "{rows:?}"
    );
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.is_noop(), "{r2:?}");
}

/// a vanished file deletes the note (a tombstone); when too many vanish at
/// once the guard stops everything until forced.
#[test]
fn vanished_file_deletes_the_note_and_the_guard_stops_mass_deletions() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    // a real-sized vault: one deletion is under every threshold
    let n = note(5, "quinta", "da cancellare [[Roma]]");
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    for i in 6..=30_u16 {
        vault
            .cell()
            .put_note(&note(i, &format!("guardia {i}"), "corpo"), "t")
            .unwrap(/* t */);
    }
    vault.sync_once(false).unwrap(/* t */); // files written
    let f = list_md(&vault)
        .into_iter()
        .find(|f| f.to_string_lossy().contains("quinta"))
        .expect(/* test-only */ "the quinta file");
    std::fs::remove_file(&f).unwrap(/* t */);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.deleted.len(), 1, "{r:?}");
    assert_eq!(
        vault.cell().get(&n.id).unwrap(/* t */).status,
        Status::Deleted
    );
    assert_eq!(
        vault.cell().vault_files_of_view(&vid(&vault)).unwrap(/* t */).len(),
        25
    );

    // the guard: 25 files vanish together (an unmounted folder)
    for f in list_md(&vault) {
        if f.to_string_lossy().contains("guardia") {
            std::fs::remove_file(&f).unwrap(/* t */);
        }
    }
    let err = vault.sync_once(false).unwrap_err(/* test-only */);
    assert!(
        matches!(&err, VaultError::MassDelete { would_delete, .. } if *would_delete == 25),
        "{err}"
    );
    // nothing was deleted
    let living = vault
        .cell()
        .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM notes WHERE status != 'deleted'")
        .unwrap(/* t */);
    assert_eq!(living[0], "25", "the guard deleted nothing");
    // --allow-deletions goes through
    let r = vault.sync_once(true).unwrap(/* test-only */);
    assert_eq!(r.deleted.len(), 25, "{r:?}");
}

/// a broken header never touches the cell: error in the report, retry later.
#[test]
fn broken_header_is_reported_and_leaves_the_cell_alone() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    vault.sync_once(false).unwrap(/* t */);
    write(&vault, "rotta.md", "---\ntitle: manca tutto\nqualcosa: {[");
    let before = vault.cell().ops().unwrap(/* t */).len();
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r.errors.iter().any(|e| e.contains("rotta.md")), "{r:?}");
    assert_eq!(vault.cell().ops().unwrap(/* t */).len(), before);
    // fixing the file lets the next reconciliation import it
    write(
        &vault,
        "rotta.md",
        "---\ntitle: manca tutto\nlang: it\nsource: user\nconfidence: stated\nstatus: active\ncreated_at: 2026-11-01T10:00:00Z\nupdated_at: 2026-11-01T10:00:00Z\n---\nAdesso funziona.\n",
    );
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.errors.is_empty(), "{r2:?}");
}

/// a file cannot promote an external note to active (the cell's rule).
#[test]
fn file_cannot_promote_an_external_note() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let mut n = note(6, "sesta", "voci dal web [[Milano]]");
    n.confidence = Confidence::External;
    n.source = "import:web".into();
    n.status = Status::Quarantined;
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* t */);
    let f = list_md(&vault)[0].clone();
    let text = std::fs::read_to_string(&f).unwrap(/* t */);
    // the hand edit flips the status to active
    let edited = text
        .replace("status: quarantined", "status: active")
        .replace("updated_at:", "x-zz: 1\nupdated_at:");
    std::fs::write(&f, edited).unwrap(/* t */);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r.warnings.iter().any(|w| w.contains("quarantena")), "{r:?}");
    assert_eq!(
        vault.cell().get(&n.id).unwrap(/* t */).status,
        Status::Quarantined,
        "the file cannot promote it"
    );
}

/// the purge deletes the note: the next reconciliation removes its file.
#[test]
fn purged_note_loses_its_file() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let n = note(7, "settima", "da purgare [[Roma]]");
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* t */);
    let f = list_md(&vault)[0].clone();
    vault.cell().purge(&n.id, "oblio", "tester").unwrap(/* t */);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert!(!f.exists(), "the file is gone with the note");
    assert!(
        r.deleted
            .contains(&f.strip_prefix(vault.root()).unwrap(/* t */).to_string_lossy().to_string()),
        "{r:?}"
    );
    assert!(vault.cell().vault_files_of_view(&vid(&vault)).unwrap(/* t */).is_empty());
}

/// the property: random edits from the file side and from the cell, two
/// reconciliations — the second does nothing, and every living note has its
/// file with `serialize(note)` as content.
#[test]
fn random_edits_converge_to_a_fixed_point() {
    for seed in 1..=8_u64 {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let vault = setup(dir.path());
        let mut s = seed | 1;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut ids: Vec<Ulid> = Vec::new();
        for i in 1..=6_u16 {
            let n = note(i, &format!("nota {i}"), &format!("corpo {i} [[Milano]]"));
            vault.cell().put_note(&n, "t").unwrap(/* t */);
            ids.push(n.id);
        }
        vault.sync_once(false).unwrap(/* t */); // files exist
        for round in 0..12 {
            let pick = ids[(next() % ids.len() as u64) as usize];
            // v0.3.2 (P1.6): the mix now makes CONFLICTS (file and cell both
            // move) and PURGES too — the fixed point must survive them, and
            // the conflict copies of purged notes must not survive them
            let mut conflict_edit = false;
            match next() % 5 {
                0 => {
                    // edit the file behind the engine's back
                    let rows = vault.cell().vault_files_of_view(&vid(&vault)).unwrap(/* t */);
                    let row = rows.iter().find(|r| r.note_id == pick.to_string());
                    if let Some(row) = row {
                        let path = vault.root().join(&row.path);
                        let text = std::fs::read_to_string(&path).unwrap_or_default();
                        let edited = text.replace(
                            &format!(
                                "corpo {}",
                                ids.iter().position(|i| *i == pick).unwrap_or(0) + 1
                            ),
                            &format!("corpo {round} modificato"),
                        );
                        if edited != text {
                            std::fs::write(&path, edited).unwrap(/* t */);
                        }
                    }
                }
                1 | 2 => {
                    conflict_edit = next() % 2 == 0;
                    vault
                        .cell()
                        .patch_note(
                            &pick,
                            &NotePatch {
                                body: Some(format!("corpo {round} dalla cella [[Roma]]")),
                                ..Default::default()
                            },
                            "cli",
                        )
                        .unwrap_or(()); // tombstoned notes reject patches
                }
                // a purge: the note goes, its file and conflict copies
                // must follow (at the next sync, when the view is here)
                3 if vault.cell().purge(&pick, "prop", "tester").is_ok() => {
                    ids.retain(|i| *i != pick);
                }
                _ => {}
            }
            if conflict_edit
                && let Some(row) = vault
                    .cell()
                    .vault_files_of_view(&vid(&vault))
                    .unwrap(/* t */)
                    .iter()
                    .find(|r| r.note_id == pick.to_string())
                    .cloned()
            {
                // the OTHER side of a conflict: the file moves too, and the
                // next sync saves its version as a `.conflitto` copy
                let path = vault.root().join(&row.path);
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let edited = text.replace(
                    &format!(
                        "corpo {}",
                        ids.iter()
                            .position(|i| *i == pick)
                            .map(|p| p + 1)
                            .unwrap_or(0)
                    ),
                    &format!("corpo {round} dal file"),
                );
                if edited != text {
                    std::fs::write(&path, edited).unwrap(/* t */);
                }
            }
            // v0.3.1 (P1.5/P1.6): new files with an OBSIDIAN header land in
            // the vault during the run — their header must be read (not
            // swallowed) and they must not grow an export twin
            if next() % 5 == 0 {
                let k = 100 + (next() % 5) as u16;
                write(
                    &vault,
                    &format!("nuova-{k}.md"),
                    &format!(
                        "---\ntitle: Nuova {k}\ntags: [batch{k}]\n---\nCorpo {k} di [[Roma]].\n"
                    ),
                );
            }
            if next() % 4 == 0 {
                vault.sync_once(false).unwrap(/* t */);
            }
        }
        let _ = vault.sync_once(false);
        let r2 = vault.sync_once(false).unwrap(/* test-only */);
        assert!(
            r2.is_noop(),
            "seed {seed}: the second reconciliation must do nothing: {r2:?}"
        );
        // every living note has its file, with serialize(note) as content
        let rows = vault.cell().vault_files_of_view(&vid(&vault)).unwrap(/* t */);
        // one row per note, structurally and in the files on disk
        let mut bound_notes: Vec<&String> = rows.iter().map(|r| &r.note_id).collect();
        bound_notes.sort();
        let mut dedup = bound_notes.clone();
        dedup.dedup();
        assert_eq!(
            bound_notes, dedup,
            "seed {seed}: a note is bound to exactly one file"
        );
        let plain_files = list_md(&vault)
            .into_iter()
            .filter(|f| {
                !f.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".conflitto-"))
            })
            .count();
        assert_eq!(
            plain_files,
            rows.len(),
            "seed {seed}: exactly one file per living note on disk"
        );
        for row in &rows {
            let id = Ulid::from_string(&row.note_id).unwrap(/* t */);
            let note = vault.cell().get(&id).unwrap(/* t */);
            assert_eq!(note.status, Status::Active, "seed {seed}: {row:?}");
            let on_disk = std::fs::read_to_string(vault.root().join(&row.path)).unwrap_or_default();
            assert_eq!(
                on_disk,
                note.serialize().unwrap(/* t */),
                "seed {seed}, {}",
                row.path
            );
        }
        let living = vault
            .cell()
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM notes WHERE status = 'active'")
            .unwrap(/* t */);
        assert_eq!(
            living[0].parse::<usize>().unwrap(/* t */),
            rows.len(),
            "seed {seed}: every living note has its file"
        );
        // the conflict registry holds only notes that still exist: the
        // copies of the purged ones went with them (seed {seed})
        for (path, note_id) in vault.cell().vault_conflicts_of_view(&vid(&vault)).unwrap(/* t */) {
            let id = Ulid::from_string(&note_id).unwrap(/* t */);
            assert!(
                vault.cell().get(&id).is_ok(),
                "seed {seed}: the conflict copy {path} outlives its note"
            );
        }
    }
}

/// watch: a hand edit becomes a note update within seconds.
#[test]
fn watch_updates_the_note_within_three_seconds() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let n = note(9, "watch", "versione iniziale [[Milano]]");
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* t */);
    let handle = vault
        .watch(200, std::time::Duration::from_millis(300))
        .unwrap(/* test-only */);
    let f = list_md(&vault)[0].clone();
    let text = std::fs::read_to_string(&f).unwrap(/* t */);
    std::thread::sleep(std::time::Duration::from_millis(200));
    std::fs::write(&f, text.replace("versione iniziale", "versione guardata"))
        .unwrap(/* t */);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut seen = false;
    while std::time::Instant::now() < deadline {
        if vault
            .cell()
            .get(&n.id)
            .is_ok_and(|note| note.body.contains("versione guardata"))
        {
            seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(seen, "the watch picked the edit up within 3 s");
    handle.stop().unwrap(/* test-only */);
}

fn list_md(vault: &Vault) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap(/* t */)
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "md") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(vault.root(), &mut out);
    out
}

// keep the unused imports honest across cfg
#[allow(unused_imports)]
use CellError as _;
#[allow(unused_imports)]
use Validity as _;

/// v0.3.1 (P1.5): a file with an Obsidian header (title, tags — no Engramma
/// fields) is imported as its title, tags and BODY, and the rewrite merges
/// the header into ONE `---` block with the user's fields kept; the second
/// sync does nothing.
#[test]
fn obsidian_header_file_imports_with_its_own_fields() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    vault.sync_once(false).unwrap(/* test-only */); // settle init
    write(
        &vault,
        "nuova.md",
        "---\ntitle: Nuova da file\ntags: [frutta]\n---\n[[Fabio]] ama i fichi.\n",
    );
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.created, vec!["nuova.md".to_string()], "{r:?}");
    let text = read(&vault, "nuova.md");
    // ONE header block, the user's fields kept, the id merged in, the body
    // without the header
    assert_eq!(text.matches("---\n").count(), 2, "one header block: {text}");
    assert!(text.contains("title: Nuova da file"), "{text}");
    assert!(text.contains("tags: [frutta]"), "{text}");
    assert!(text.contains("id: 01"), "{text}");
    assert!(text.contains("[[Fabio]] ama i fichi."), "{text}");
    let parsed = engramma_core::format::parse(&text, ParseMode::Strict)
        .unwrap(/* test-only */)
        .note;
    assert_eq!(parsed.title.as_deref(), Some("Nuova da file"));
    assert_eq!(parsed.tags, vec!["frutta".to_string()]);
    assert_eq!(parsed.body, "[[Fabio]] ama i fichi.\n");
    assert_eq!(parsed.source, "import:obsidian/nuova.md");
    // the note is a real note, found by its title
    let hits = vault
        .cell()
        .search_with_now(
            &engramma_core::search::Query::text("fichi").k(3),
            time::OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* t */),
        )
        .unwrap(/* test-only */);
    assert!(
        hits.iter()
            .any(|h| h.note.title.as_deref() == Some("Nuova da file")),
        "{:?}",
        hits.iter()
            .map(|h| h.note.display_title())
            .collect::<Vec<_>>()
    );
    // the second reconciliation is a no-op
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.is_noop(), "{r2:?}");
}

/// v0.3.1 (P1.5): `import --obsidian` of the same folder gives the same
/// note (same title, tags and body), not a `note-02KZ…` with the header in
/// the body.
#[test]
fn obsidian_import_reads_the_header_too() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let src = dir.path().join("vault");
    std::fs::create_dir_all(&src).unwrap(/* test-only */);
    std::fs::write(
        src.join("nuova.md"),
        "---\ntitle: Nuova da file\ntags: [frutta]\naliases: [Nuova]\n---\n[[Fabio]] ama i fichi.\n",
    )
    .unwrap(/* test-only */);
    let cell = engramma_core::Cell::create(&dir.path().join("c.sqlite"), "t", "t")
        .unwrap(/* test-only */);
    let s = engramma_core::port::import_obsidian(&cell, &src, "t")
        .unwrap(/* test-only */);
    assert_eq!(s.imported, 1, "{s:?}");
    let id = cell.note_ids().unwrap(/* test-only */)[0];
    let note = cell.get(&id).unwrap(/* test-only */);
    assert_eq!(note.title.as_deref(), Some("Nuova da file"));
    assert_eq!(note.tags, vec!["frutta".to_string()]);
    assert_eq!(note.body, "[[Fabio]] ama i fichi.\n");
    // the alias rides in the extensions and survives a rewrite
    assert!(
        note.extensions.iter().any(|(k, _)| k == "aliases"),
        "{:?}",
        note.extensions
    );
}

/// v0.3.1 (P1.6): one note, one file. The export phase must see the binding
/// the SAME sync created (before: the user's file became a note AND the
/// phase-3 export wrote `note/<slug>-<id8>.md`, two files for one note and
/// edits bouncing between the copies), `vault_files` carries a UNIQUE
/// constraint on `note_id`, and a duplicate copy of a bound file is reported
/// and left alone instead of stealing the binding back and forth.
#[test]
fn one_file_per_note_no_second_export_no_ping_pong() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    vault.sync_once(false).unwrap(/* test-only */); // settle init
    write(
        &vault,
        "nuova.md",
        "---\ntitle: Nuova da file\ntags: [frutta]\n---\n[[Fabio]] ama i fichi.\n",
    );
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.created, vec!["nuova.md".to_string()], "{r:?}");
    // exactly ONE file for the note: its own, no export twin
    let all = list_md(&vault);
    assert_eq!(all.len(), 1, "one file for one note: {all:?}");
    assert!(
        all[0].ends_with("nuova.md"),
        "the file is the user's own: {all:?}"
    );
    // one row per note in the reconciliation state
    let rows = vault.cell().vault_files_of_view(&vid(&vault)).unwrap(/* test-only */);
    assert_eq!(rows.len(), 1, "{rows:?}");
    // the second sync does nothing at all
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.is_noop(), "{r2:?}");

    // a hand-made duplicate with the same id: the binding wins, no bounce
    let text = read(&vault, "nuova.md");
    write(&vault, "copia.md", &text);
    let r3 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(
        r3.warnings.iter().any(|w| w.contains("copia.md")),
        "the duplicate is reported: {r3:?}"
    );
    assert!(r3.deleted.is_empty() && r3.moved.is_empty(), "{r3:?}");
    let rows = vault.cell().vault_files_of_view(&vid(&vault)).unwrap(/* test-only */);
    assert_eq!(rows.len(), 1, "still one binding: {rows:?}");
    assert!(rows[0].path.ends_with("nuova.md"), "{rows:?}");
    // and editing the bound file still updates the note (no bounce)
    let edited = text.replace("fichi", "fichi e more");
    write(&vault, "nuova.md", &edited);
    let r4 = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r4.updated_from_file, vec!["nuova.md".to_string()], "{r4:?}");
}

/// v0.3.2 (P1.6): a conflict copy holds the file's version of a note; when
/// that note is purged, the copy and the bound file go IMMEDIATELY (the
/// purge reaches every registered view through `forget_note`) — no root of
/// the body anywhere in the view folder, without any sync.
#[test]
fn purge_takes_the_conflict_copy_and_the_file_now() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let secret_body = "il quokkaviolaceo zibellinotremante segreto";
    let n = note(11, "segreta", secret_body);
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* test-only */);
    // a conflict: the file's version (with the secret words) is saved as a
    // `.conflitto` copy
    let path = list_md(&vault)[0].clone();
    let text = std::fs::read_to_string(&path).unwrap(/* test-only */);
    std::fs::write(path, text.replace("segreta", "versione dal file segreta"))
        .unwrap(/* test-only */);
    vault
        .cell()
        .patch_note(
            &n.id,
            &NotePatch {
                body: Some("versione dalla cella [[Roma]]".into()),
                ..Default::default()
            },
            "cli",
        )
        .unwrap(/* test-only */);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.conflicts.len(), 1, "{r:?}");
    // the conflict copy is registered
    assert_eq!(
        vault.cell().vault_conflicts_of(&vid(&vault), &n.id).unwrap(/* t */).len(),
        1
    );
    assert_eq!(
        vault.cell().vault_views().unwrap(/* t */).len(),
        1,
        "the view is bound in the cell"
    );

    // the purge: cell first, then every registered view
    vault.cell().purge(&n.id, "oblio", "tester").unwrap(/* t */);
    for view in vault.cell().vault_views().unwrap(/* t */) {
        let v = Vault::open(
            vault.cell().clone(),
            std::path::PathBuf::from(&view.root).as_path(),
        )
        .unwrap(/* test-only */);
        v.forget_note(&n.id).unwrap(/* test-only */);
    }
    // no file in the WHOLE view folder holds a root of the body — now,
    // without any sync
    let needles: Vec<String> = ["quokkaviolaceo", "zibellinotremante", "segret"]
        .into_iter()
        .map(str::to_string)
        .collect();
    fn walk_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap(/* t */).flatten() {
            let p = e.path();
            if p.is_dir() {
                walk_files(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk_files(vault.root(), &mut files);
    assert!(!files.is_empty());
    for f in files {
        let raw = std::fs::read_to_string(&f).unwrap_or_default();
        for n in &needles {
            assert!(
                !raw.contains(n.as_str()),
                "«{n}» sopravvive in {}",
                f.display()
            );
        }
    }
    // and the rows are gone too
    assert!(
        vault
            .cell()
            .vault_files_of_view(&vid(&vault))
            .unwrap(/* t */)
            .iter()
            .all(|r| r.note_id != n.id.to_string())
    );
    assert!(vault.cell().vault_conflicts_of_view(&vid(&vault)).unwrap(/* t */).is_empty());
}

/// v0.3.2 (P1.6): an unreachable view (unmounted, renamed away) makes
/// `forget_note` fail — the caller marks `pending: ["vault"]` — and once
/// the view is back, the NEXT sync finishes the work: the bound file, the
/// conflict copies and their rows all go.
#[test]
fn unreachable_view_is_pending_and_the_next_sync_finishes() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    let secret_body = "il quokkaviolaceo segreto della vista";
    let n = note(12, "nascosta", secret_body);
    vault.cell().put_note(&n, "t").unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* test-only */);
    let path = list_md(&vault)[0].clone();
    let text = std::fs::read_to_string(&path).unwrap(/* test-only */);
    std::fs::write(&path, text.replace("nascosta", "dal file quokkaviolaceo"))
        .unwrap(/* test-only */);
    vault
        .cell()
        .patch_note(
            &n.id,
            &NotePatch {
                body: Some("dalla cella [[Roma]]".into()),
                ..Default::default()
            },
            "cli",
        )
        .unwrap(/* test-only */);
    vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(
        vault.cell().vault_conflicts_of(&vid(&vault), &n.id).unwrap(/* t */).len(),
        1
    );

    // the purge happens while the view is AWAY (renamed, like an unmounted
    // volume): forget_note cannot reach it
    let rel = path
        .strip_prefix(vault.root())
        .unwrap(/* test-only */)
        .to_path_buf();
    vault.cell().purge(&n.id, "oblio", "tester").unwrap(/* t */);
    let away = dir.path().join("via");
    std::fs::rename(vault.root(), &away).unwrap(/* test-only */);
    let root = vault.cell().vault_views().unwrap(/* t */)[0].root.clone();
    let v = Vault::open(
        vault.cell().clone(),
        std::path::PathBuf::from(&root).as_path(),
    )
    .unwrap(/* test-only */);
    assert!(
        v.forget_note(&n.id).is_err(),
        "the unreachable view must be an error, not a silent ok"
    );
    assert!(
        away.join(&rel).exists(),
        "the file is still there, with the secret"
    );

    // back in place: the sync finishes the purge's work
    std::fs::rename(&away, vault.root()).unwrap(/* test-only */);
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert!(!path.exists(), "the bound file went with the note");
    assert!(
        vault.cell().vault_conflicts_of_view(&vid(&vault)).unwrap(/* t */).is_empty(),
        "the conflict copies went too: {:?}",
        vault.cell().vault_conflicts_of_view(&vid(&vault)).unwrap(/* t */)
    );
    let _ = r;
    fn walk_files2(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap(/* t */).flatten() {
            let p = e.path();
            if p.is_dir() {
                walk_files2(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk_files2(vault.root(), &mut files);
    for f in files {
        let raw = std::fs::read_to_string(&f).unwrap_or_default();
        for needle in ["quokkaviolaceo", "segret"] {
            assert!(
                !raw.contains(needle),
                "«{needle}» sopravvive in {}",
                f.display()
            );
        }
    }
}

/// v0.3.2 (P1.7): a plain file WITHOUT a header takes the file name as its
/// title (as Obsidian displays it) — the v0.3.1 code only fell back when a
/// header existed. The rewrite adds `title:` to the new block.
#[test]
fn plain_file_title_is_the_file_name() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    write(&vault, "Mele rosse.md", "Mi piacciono.\n");
    let r = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r.created, vec!["Mele rosse.md".to_string()], "{r:?}");
    let ids = vault.cell().note_ids().unwrap(/* t */);
    assert_eq!(ids.len(), 1);
    let note = vault.cell().get(&ids[0]).unwrap(/* t */);
    assert_eq!(note.title.as_deref(), Some("Mele rosse"));
    assert_eq!(note.body, "Mi piacciono.\n");
    let on_disk = read(&vault, "Mele rosse.md");
    assert!(
        on_disk.contains("title: Mele rosse"),
        "the rewrite carries the title: {on_disk}"
    );
    // and the fixed point holds
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(r2.is_noop(), "{r2:?}");
}

/// v0.3.2 (P1.8): a file's warnings (unknown keys, unusable values) are
/// emitted only when the file is (re)read because it is new or changed —
/// the second sync of a quiet view is an EMPTY report, warnings included.
#[test]
fn second_sync_of_a_quiet_view_is_empty_warnings_included() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let vault = setup(dir.path());
    write(
        &vault,
        "con-campi.md",
        "---\ntitle: Con campi\nrating: 5\ncssclass: wide\n---\nCorpo con [[Roma]].\n",
    );
    let r1 = vault.sync_once(false).unwrap(/* test-only */);
    assert_eq!(r1.created, vec!["con-campi.md".to_string()], "{r1:?}");
    assert!(
        r1.warnings.iter().any(|w| w.contains("rating")),
        "the first read warns about the unknown keys: {:?}",
        r1.warnings
    );
    // the view is quiet now: the second reconciliation says NOTHING
    let r2 = vault.sync_once(false).unwrap(/* test-only */);
    assert!(
        r2.is_noop(),
        "the second sync must be empty, warnings included: {r2:?}"
    );
}
