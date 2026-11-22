// SPDX-License-Identifier: Apache-2.0

//! Integration test for `engramma verify`: the vector coverage per model
//! must appear in the human output and in `--json` (v0.2.1, P1.4 — the
//! REPORT v0.2 claimed it and the command never showed it).

mod common;

use std::process::Command;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, EmbedError, Embedder, Status, TextKind};

/// 8-dim deterministic embedder: one fixed unit vector is all coverage
/// bookkeeping needs.
struct FakeEmbedder;

impl Embedder for FakeEmbedder {
    fn model_id(&self) -> &str {
        "fake@1"
    }
    fn dim(&self) -> usize {
        8
    }
    fn max_tokens(&self) -> usize {
        8_192
    }
    fn embed(&self, texts: &[&str], _k: TextKind) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.25_f32; 8]).collect())
    }
}

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

fn run(root: &std::path::Path, json: bool) -> (String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engramma"));
    cmd.arg("--root").arg(root);
    if json {
        cmd.arg("--json");
    }
    cmd.arg("verify");
    let out = cmd.output().expect(/* test-only */ "spawn engramma");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn verify_reports_coverage_per_model() {
    let dir = common::test_dir();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("cells")).unwrap(/* test-only */);
    let cell = Cell::create(&root.join("cells/default.sqlite"), "default", "t")
        .unwrap(/* test-only */);
    for i in 1..=3_u16 {
        cell.put_note(&note(i, "corpo della nota"), "t")
            .unwrap(/* test-only */);
    }
    // no model yet: coverage is empty and the verify still passes
    let (human, _) = run(&root, false);
    assert!(
        human.contains("registro verificato"),
        "human output: {human}"
    );
    let (json, _) = run(&root, true);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap(/* test-only */);
    assert_eq!(
        v["coverage"].as_array().map(Vec::len),
        Some(0),
        "no models yet: {json}"
    );

    // embed with a fake model: covered must follow
    cell.add_model("fake@1", 8).unwrap(/* test-only */);
    let e = FakeEmbedder;
    cell.embed_pending(&e, 2).unwrap(/* test-only */);
    let (human, _) = run(&root, false);
    assert!(
        human.contains("copertura") && human.contains("fake@1"),
        "one line per model: {human}"
    );
    assert!(human.contains("2/3"), "two of three notes covered: {human}");
    let (json, _) = run(&root, true);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap(/* test-only */);
    let coverage = v["coverage"].as_array().cloned().unwrap_or_default();
    assert_eq!(coverage.len(), 1, "{json}");
    assert_eq!(coverage[0]["model"], "fake@1");
    assert_eq!(coverage[0]["state"], "building");
    assert_eq!(coverage[0]["covered"], 2);
    assert_eq!(coverage[0]["living"], 3);
    assert_eq!(v["ok"], serde_json::json!(true));
}

/// v0.3.2 (P1.6): the CLI's purge reaches every registered view — the note's
/// file and its `.conflitto` copies go at once. When the view is away the
/// exit code is 5 (`pending: ["vault"]`), like the WAL case.
#[test]
fn cli_purge_reaches_the_view_or_exits_five() {
    let dir = common::test_dir();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap(/* test-only */);
    let cmd = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_engramma"));
        c.arg("--root").arg(&root).arg("--cell").arg("cli-vault");
        c
    };
    // `init` builds the root; the cell is created directly below
    let mut plain = Command::new(env!("CARGO_BIN_EXE_engramma"));
    plain.arg("--root").arg(&root);
    plain.arg("init").output().unwrap(/* test-only */);
    // a cell with one note, and a view bound to it
    let cell = Cell::create(&root.join("cells/cli-vault.sqlite"), "cli-vault", "t")
        .unwrap(/* test-only */);
    let n = note(21, "segreta quokkaviolaceo da purgare");
    cell.put_note(&n, "t").unwrap(/* test-only */);
    drop(cell);
    let view = root.join("view");
    let out = cmd()
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

    // the view goes away: the purge must exit 5 with pending ["vault"]
    let away = dir.path().join("away");
    std::fs::rename(&view, &away).unwrap(/* test-only */);
    let out = cmd()
        .arg("purge")
        .arg("00000021") // the id8 suffix of note(21)
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("purgata"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("vista"));

    // back in place: the purge completes (exit 0) and takes the file
    std::fs::rename(&away, &view).unwrap(/* test-only */);
    let cell = Cell::open(&root.join("cells/cli-vault.sqlite")).unwrap(/* t */);
    assert!(cell.get(&n.id).is_err(), "the note is gone already");
    // the note is gone; re-run purge via CLI is a no-op — instead check the
    // sync finishes the work on the restored view
    drop(cell);
    let out = cmd()
        .arg("vault")
        .arg("sync")
        .arg(&view)
        .output()
        .unwrap(/* test-only */);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut files = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap(/* t */).flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    walk(&view, &mut files);
    assert!(!files.is_empty(), "other notes' files stay");
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
