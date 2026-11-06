// SPDX-License-Identifier: Apache-2.0

//! Integration test for `engramma verify`: the vector coverage per model
//! must appear in the human output and in `--json` (v0.2.1, P1.4 — the
//! REPORT v0.2 claimed it and the command never showed it).

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
    let dir = tempfile::tempdir().unwrap(/* test-only */);
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
