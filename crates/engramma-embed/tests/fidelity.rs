// SPDX-License-Identifier: Apache-2.0

//! Fidelity tests against ONNX Runtime reference vectors
//! (`tests/fixtures/*.reference.json`, generated with onnxruntime 1.31.1).
//!
//! These tests need the real models under the model root (`$ENGRAMMA_MODEL_DIR`
//! or `~/.cache/engramma/models`). When the models are absent the tests FAIL
//! with a clear message — silent skips are not allowed; CI without models can
//! set `ENGRAMMA_SKIP_MODEL_TESTS=1` (documented in README).

use std::path::PathBuf;

use engramma_core::{Embedder, TextKind};
use engramma_embed::{Builtin, ModelSpec, spec};

fn root() -> PathBuf {
    spec::model_root()
}

fn models_skipped() -> bool {
    std::env::var("ENGRAMMA_SKIP_MODEL_TESTS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn load_or_skip(spec: &ModelSpec) -> Option<Builtin> {
    if models_skipped() {
        eprintln!("ENGRAMMA_SKIP_MODEL_TESTS=1: skipping {}", spec.id);
        return None;
    }
    match Builtin::load(&root(), spec) {
        Ok(b) => Some(b),
        Err(e) => panic!(
            "model `{}` is not usable under {}: {e}\n\
             pull it (`engramma model pull {}`) or set ENGRAMMA_SKIP_MODEL_TESTS=1",
            spec.id,
            root().display(),
            spec.id
        ),
    }
}

#[derive(serde::Deserialize)]
struct Reference {
    texts: Vec<String>,
    vectors: Vec<Vec<f32>>,
}

fn reference(name: &str) -> Reference {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.reference.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    d
}

fn assert_fidelity(builtin: &Builtin, refs: &Reference, what: &str) {
    let texts: Vec<&str> = refs.texts.iter().map(String::as_str).collect();
    let got = builtin
        .embed(&texts, TextKind::Document)
        .unwrap_or_else(|e| panic!("{what}: {e}"));
    assert_eq!(got.len(), refs.texts.len(), "{what}: count");
    for (i, (g, want)) in got.iter().zip(&refs.vectors).enumerate() {
        assert_eq!(g.len(), want.len(), "{what}: vector {i} has wrong length");
        let cos = cosine(g, want);
        assert!(
            cos >= 0.9999,
            "{what}: vector {i} diverges from ONNX Runtime (cos {cos:.6})"
        );
    }
}

#[test]
fn granite_97m_matches_onnxruntime() {
    let Some(b) = load_or_skip(&ModelSpec::granite_97m()) else {
        return;
    };
    assert_eq!(
        b.model_id(),
        "granite-embedding-97m-multilingual-r2@835ad140"
    );
    assert_eq!(b.dim(), 384);
    assert_eq!(b.max_tokens(), 512);
    assert_fidelity(
        &b,
        &reference("granite-embedding-97m-multilingual-r2"),
        "97M",
    );
    // Queries take the same path (Granite is symmetric; the test proves the
    // kind travels without breaking anything).
    let q = b
        .embed(&["Dove vive Fabio adesso?"], TextKind::Query)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(q.len(), 1);
}

#[test]
fn granite_311m_matches_onnxruntime() {
    let Some(b) = load_or_skip(&ModelSpec::granite_311m()) else {
        return;
    };
    assert_eq!(b.dim(), 768);
    assert_fidelity(
        &b,
        &reference("granite-embedding-311m-multilingual-r2"),
        "311M",
    );
}

#[test]
fn padding_does_not_change_the_vector() {
    let Some(b) = load_or_skip(&ModelSpec::granite_97m()) else {
        return;
    };
    let alone = b
        .embed(&["Il gatto dorme sul divano."], TextKind::Document)
        .unwrap_or_else(|e| panic!("{e}"))
        .pop()
        .unwrap_or_else(|| panic!("no vector"));
    let batch = b
        .embed(
            &[
                "Fabio abita a Milano con Giulia dal settembre 2026, dopo il trasloco da Roma.",
                "Il gatto dorme sul divano.",
                "Un testo molto più lungo: la riunione di novembre con lo Studio Rossi è stata \
                 rinviata due volte, prima per le ferie e poi per il cantiere del bagno.",
            ],
            TextKind::Document,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let padded = &batch[1];
    let cos = cosine(&alone, padded);
    assert!(cos >= 0.9999, "padding changed the vector (cos {cos:.6})");
}

#[test]
fn truncation_keeps_the_first_tokens_and_the_batch_shape() {
    let Some(b) = load_or_skip(&ModelSpec::granite_97m()) else {
        return;
    };
    // ~2500 tokens of Italian: far past the 512 limit.
    let sentence = "Fabio ha traslocato a Milano con Giulia dopo il matrimonio di Sara. ";
    let long: String = sentence.repeat(50);
    assert!(
        b.approx_tokens(&long) > 512,
        "the text must exceed the limit"
    );
    let got = b
        .embed(&[long.as_str(), "corto"], TextKind::Document)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].len(), 384);
    let norm: f32 = got[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "not L2-normalised: {norm}");
    // deterministic: the same input embeds to the same vector
    let again = b
        .embed(&[long.as_str()], TextKind::Document)
        .unwrap_or_else(|e| panic!("{e}"));
    let cos = cosine(&got[0], &again[0]);
    assert!(cos >= 0.9999, "not deterministic (cos {cos:.6})");
}

#[test]
fn empty_batch_is_empty() {
    let Some(b) = load_or_skip(&ModelSpec::granite_97m()) else {
        return;
    };
    assert!(
        b.embed(&[], TextKind::Query)
            .unwrap_or_else(|e| panic!("{e}"))
            .is_empty()
    );
}

/// v0.4.5 (P2): the sorted, budgeted batching must be invisible in the
/// output — the SAME texts in a different order, and under very
/// different token budgets (one text per forward pass at 1, the default,
/// a wide 8192), give BIT-IDENTICAL vectors (not cosines: equality).
#[test]
fn order_and_budget_do_not_change_the_vectors() {
    let Some(mut b) = load_or_skip(&ModelSpec::granite_97m()) else {
        return;
    };
    let sentence = "Fabio ha traslocato a Milano con Giulia dopo il matrimonio di Sara. ";
    let texts: Vec<String> = (0..12)
        .map(|i| match i % 3 {
            0 => format!("nota {i}: {}", sentence.repeat(i + 1)),
            1 => format!("nota {i}: {}", sentence.repeat(20 + i)),
            _ => format!("nota corta {i}: una cosa da ricordare."),
        })
        .collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let base = b
        .embed(&refs, TextKind::Document)
        .unwrap_or_else(|e| panic!("{e}"));

    // reversed order: vector i must equal base[i], bit for bit
    let mut rev = texts.clone();
    rev.reverse();
    let rrefs: Vec<&str> = rev.iter().map(String::as_str).collect();
    let got = b
        .embed(&rrefs, TextKind::Document)
        .unwrap_or_else(|e| panic!("{e}"));
    for (i, v) in got.iter().enumerate() {
        let want = &base[refs.len() - 1 - i];
        assert_eq!(v, want, "reversed order changed vector {i}");
    }

    // budgets: 1 = one text per forward pass, then a wide 8192 — the
    // batches fall apart completely, the vectors must not
    for budget in [1_usize, 8192] {
        b.set_token_budget(budget);
        let got = b
            .embed(&refs, TextKind::Document)
            .unwrap_or_else(|e| panic!("budget {budget}: {e}"));
        for (i, v) in got.iter().enumerate() {
            assert_eq!(v, &base[i], "budget {budget} changed vector {i}");
        }
    }
}
