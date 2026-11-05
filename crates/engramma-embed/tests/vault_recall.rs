// SPDX-License-Identifier: Apache-2.0

//! Recall of the int8 flat index on **real** vault vectors (the golden vault
//! of `tests/golden`), against the exact f32 scan. Needs the models (see
//! `fidelity.rs` for `ENGRAMMA_SKIP_MODEL_TESTS=1`).

use std::path::PathBuf;

use engramma_core::vector::{FlatI8, IndexRow, VectorIndex};
use engramma_core::{Embedder, TextKind};
use engramma_embed::{Builtin, ModelSpec, spec};

fn root() -> PathBuf {
    spec::model_root()
}

fn vault_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/vault-it")
}

/// All markdown notes of the vault, parsed and flattened.
fn vault_texts() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let dir = vault_dir();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display()));
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "md") {
                let text =
                    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
                let parsed = engramma_core::format::parse(&text, engramma_core::ParseMode::Strict)
                    .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
                let note = parsed.note;
                let body = note.body.clone();
                let title = note.display_title().to_string();
                out.push((note.id.to_string(), title, body));
            }
        }
    }
    assert!(out.len() > 40, "the golden vault should hold ~62 notes");
    out
}

#[test]
fn flat_i8_recall_on_real_vault_vectors() {
    if std::env::var("ENGRAMMA_SKIP_MODEL_TESTS")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        eprintln!("ENGRAMMA_SKIP_MODEL_TESTS=1: skipping");
        return;
    }
    let b = Builtin::load(&root(), &ModelSpec::granite_97m())
        .unwrap_or_else(|e| panic!("model missing under {}: {e}", root().display()));
    let texts = vault_texts();
    // Embed documents and the golden questions.
    let refs: Vec<String> = texts
        .iter()
        .map(|(id, title, body)| {
            let _ = id;
            format!("{title}\n\n{body}")
        })
        .collect();
    let doc_refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let doc_vecs = b
        .embed(&doc_refs, TextKind::Document)
        .unwrap_or_else(|e| panic!("{e}"));
    let rows: Vec<IndexRow> = texts
        .iter()
        .zip(doc_vecs.iter())
        .map(|((id, _, _), v)| IndexRow {
            note_id: id.clone(),
            chunk: 0,
            vec: v.clone(),
        })
        .collect();
    let mut index = FlatI8::new(384);
    index.rebuild(rows.clone());

    // The golden questions as queries.
    let queries_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/queries.json");
    let queries: Vec<String> = {
        let text = std::fs::read_to_string(&queries_path).unwrap_or_else(|e| panic!("{e}"));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
        v.as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|q| q.get("q").and_then(|q| q.as_str()).map(String::from))
            .collect()
    };
    assert!(queries.len() >= 30, "{}", queries.len());
    let qrefs: Vec<&str> = queries.iter().map(String::as_str).collect();
    let qvecs = b
        .embed(&qrefs, TextKind::Query)
        .unwrap_or_else(|e| panic!("{e}"));

    // The pipeline as it runs in the engine: int8 scan -> top-200 candidates
    // -> exact f32 recomputation -> top-10. Compare that with the brute-force
    // exact top-10; the raw int8 ranking alone is not the contract.
    let by_id: std::collections::HashMap<String, &Vec<f32>> =
        rows.iter().map(|r| (r.note_id.clone(), &r.vec)).collect();
    let mut overlap = 0_usize;
    let mut total = 0_usize;
    for qv in &qvecs {
        let exact: Vec<String> = {
            let mut scored: Vec<(f32, &IndexRow)> = rows
                .iter()
                .map(|r| (engramma_core::embed::dot_f32(&r.vec, qv), r))
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            scored
                .iter()
                .take(10)
                .map(|(_, r)| r.note_id.clone())
                .collect()
        };
        let fused: Vec<String> = {
            let mut candidates: Vec<(f32, String)> = index
                .search(qv, 200)
                .into_iter()
                .filter_map(|h| {
                    by_id
                        .get(&h.note_id)
                        .map(|v| (engramma_core::embed::dot_f32(v, qv), h.note_id))
                })
                .collect();
            candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            candidates.into_iter().take(10).map(|(_, id)| id).collect()
        };
        overlap += exact.iter().filter(|e| fused.contains(e)).count();
        total += 10;
    }
    let recall = overlap as f32 / total as f32;
    assert!(
        recall >= 0.98,
        "int8+refine recall@10 on the vault: {recall:.4}"
    );
}
