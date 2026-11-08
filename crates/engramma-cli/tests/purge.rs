// SPDX-License-Identifier: Apache-2.0

//! v0.4 (P0.1): the CLI's purge and the view's identity. A view nobody can
//! reach keeps the purge pending (exit 5) ONLY when the cell still binds
//! files or conflict copies OF THAT NOTE to THAT view; otherwise there is
//! nothing to delete there and the purge is complete (exit 0). The message
//! names the view and suggests both `vault sync` and `vault unbind`.

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

struct Fixture {
    dir: tempfile::TempDir,
    root: std::path::PathBuf,
    cell_path: std::path::PathBuf,
}

impl Fixture {
    fn new(notes: u16) -> Self {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap(/* test-only */);
        let mut plain = Command::new(env!("CARGO_BIN_EXE_engramma"));
        plain.arg("--root").arg(&root).arg("init");
        plain.output().unwrap(/* test-only */);
        let cell_path = root.join("cells/purge-view.sqlite");
        let cell = Cell::create(&cell_path, "purge-view", "t").unwrap(/* t */);
        for i in 1..=notes {
            cell.put_note(&note(i, &format!("corpo segreto quokkaviolaceo{i}")), "t")
                .unwrap(/* test-only */);
        }
        drop(cell);
        Fixture {
            dir,
            root,
            cell_path,
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_engramma"));
        c.arg("--root")
            .arg(&self.root)
            .arg("--cell")
            .arg("purge-view");
        c
    }

    fn bind_view(&self, name: &str) -> std::path::PathBuf {
        let view = self.dir.path().join(name);
        let out = self
            .cmd()
            .arg("vault")
            .arg("init")
            .arg(&view)
            .output()
            .unwrap(/* test-only */);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        view
    }
}

/// Test 1 of the point: the folder moves, a sync from the new place keeps
/// ONE view bound to the NEW root — and the purge that follows exits 0
/// (the view is reachable, the file goes now).
#[test]
fn purge_after_the_view_moved_exits_zero() {
    let fx = Fixture::new(1);
    let view = fx.bind_view("vista");
    let new_place = fx.dir.path().join("nuovo-posto");
    std::fs::rename(&view, &new_place).unwrap(/* test-only */);
    let out = fx
        .cmd()
        .arg("vault")
        .arg("sync")
        .arg(&new_place)
        .output()
        .unwrap(/* test-only */);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // one view, the new root
    let cell = Cell::open(&fx.cell_path).unwrap(/* test-only */);
    let views = cell.vault_views().unwrap(/* test-only */);
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(views[0].root.contains("nuovo-posto"), "{views:?}");
    drop(cell);

    let out = fx
        .cmd()
        .arg("purge")
        .arg("00000001")
        .arg("--reason")
        .arg("oblio")
        .arg("--yes")
        .output()
        .unwrap(/* test-only */);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut files = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap(/* test-only */).flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    walk(&new_place, &mut files);
    assert!(
        files
            .iter()
            .all(|p| !p.to_string_lossy().contains("quokkaviolaceo")),
        "the body must not survive the purge: {files:?}"
    );
}

/// Test 2: the folder is gone for good, `vault unbind` detaches the view —
/// the purge that follows is COMPLETE (exit 0): nothing of the note is
/// bound to a view the engine cannot reach. v0.4.1 (P2.9): the short id of
/// a view is its LAST 8 characters (the random part — the first 8 are the
/// timestamp, and two views minted minutes apart share them).
#[test]
fn purge_after_unbind_exits_zero() {
    let fx = Fixture::new(1);
    let view = fx.bind_view("vista");
    let vid = {
        let cell = Cell::open(&fx.cell_path).unwrap(/* t */);
        cell.vault_views().unwrap(/* t */)[0].view_id.clone()
    };
    std::fs::remove_dir_all(&view).unwrap(/* test-only */);
    let out = fx
        .cmd()
        .arg("vault")
        .arg("unbind")
        .arg(&vid[vid.len() - 8..])
        .output()
        .unwrap(/* test-only */);
    assert!(
        out.status.success(),
        "unbind by the last 8: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("sganciata"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let out = fx
        .cmd()
        .arg("purge")
        .arg("00000001")
        .arg("--reason")
        .arg("oblio")
        .arg("--yes")
        .output()
        .unwrap(/* test-only */);
    assert_eq!(
        out.status.code(),
        Some(0),
        "nothing of the note is bound to any view: stdout {} stderr {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Test 3: the folder is unreachable AND the cell still binds the note's
/// file to that view — exit 5, and the message names the view (its root)
/// with both ways out: `sync` and `unbind`. Test 4 in the same world: a
/// note with NO file in that view purges completely (exit 0) even though
/// the folder is away.
#[test]
fn unreachable_view_holding_the_note_exits_five_and_names_itself() {
    let fx = Fixture::new(1);
    let view = fx.bind_view("vista");
    // note 2 is born AFTER the view was bound: nothing of it is there
    let n2 = note(2, "corpo quokkaviolaceo2 mai esportato");
    let cell = Cell::open(&fx.cell_path).unwrap(/* test-only */);
    cell.put_note(&n2, "t").unwrap(/* test-only */);
    drop(cell);

    let away = fx.dir.path().join("away");
    std::fs::rename(&view, &away).unwrap(/* test-only */);

    let out = fx
        .cmd()
        .arg("purge")
        .arg("00000001")
        .arg("--reason")
        .arg("oblio")
        .arg("--yes")
        .output()
        .unwrap(/* test-only */);
    assert_eq!(
        out.status.code(),
        Some(5),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("vista"), "names the view: {err}");
    assert!(err.contains("vault sync"), "suggests the sync: {err}");
    assert!(err.contains("unbind"), "suggests the unbind: {err}");

    // test 4 in the same world: note 2 has NO file in that view — its
    // purge is complete even though the folder is away
    let out = fx
        .cmd()
        .arg("purge")
        .arg("00000002")
        .arg("--reason")
        .arg("oblio")
        .arg("--yes")
        .output()
        .unwrap(/* test-only */);
    assert_eq!(
        out.status.code(),
        Some(0),
        "nothing of note 2 lives in that view: stdout {} stderr {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
