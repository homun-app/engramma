// SPDX-License-Identifier: Apache-2.0

//! The embedding contract (DESIGN §7): the trait lives in the core, which
//! stays free of network I/O; the implementations (builtin ONNX via tract,
//! Ollama, OpenAI-compatible HTTP) live in `engramma-embed`.
//!
//! Embeddings are **derived data**: they never enter `ops`, the replay or the
//! export. Every vector carries the model it came from and the hash of the
//! text it embeds (see [`note_hash`]/[`content_hash`]), so staleness is
//! checkable without floating-point comparisons.

use crate::error::EmbedError;

/// What kind of text is being embedded. Asymmetric models (EmbeddingGemma)
/// prepend different instructions to queries and documents; symmetric ones
/// (Granite R2) ignore the distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// The question side of a search.
    Query,
    /// The indexed side (a note, or one of its chunks).
    Document,
}

/// An embedding model behind the mixed search (DESIGN §7).
///
/// Implementations must be deterministic given the same input text: the same
/// model and text must always produce the same (L2-normalised) vector, or the
/// derived-table bookkeeping (`note_vectors`) drifts.
pub trait Embedder: Send + Sync {
    /// Stable identifier, e.g. `granite-embedding-97m-multilingual-r2@835ad140`.
    fn model_id(&self) -> &str;
    /// Vector dimension.
    fn dim(&self) -> usize;
    /// Maximum sequence length in tokens.
    fn max_tokens(&self) -> usize;
    /// Embed `texts`, returning one L2-normalised vector per text, in order.
    ///
    /// # Errors
    /// [`EmbedError`] on model or transport failures.
    fn embed(&self, texts: &[&str], kind: TextKind) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// Token-count estimate used to chunk long notes before embedding
    /// ([`chunk_note`]). The builtin model overrides this with the exact
    /// tokenizer count; remote embedders keep the conservative default
    /// (~4 characters per token for European languages).
    fn approx_tokens(&self, text: &str) -> usize {
        default_token_estimate(text)
    }
}

/// Conservative token estimate for embedders without a local tokenizer:
/// ~4 characters per token for European languages, at least one.
#[must_use]
pub fn default_token_estimate(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// The text a note is embedded on: `title + "\n\n" + body`, without the
/// header (DESIGN v0.2 §2).
pub fn note_embedding_text(title: Option<&str>, body: &str) -> String {
    match title {
        Some(t) if !t.trim().is_empty() => format!("{t}\n\n{body}"),
        _ => body.to_string(),
    }
}

/// blake3 content hash of one chunk: `blake3(model_id \0 chunk_text)`.
pub fn content_hash(model_id: &str, chunk_text: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(model_id.as_bytes());
    h.update(&[0]);
    h.update(chunk_text.as_bytes());
    h.finalize().to_hex().to_string()
}

/// blake3 hash of the whole embedding text of a note (same layout as
/// [`content_hash`]). Two rows with the same `note_hash` describe the same
/// text for the same model, so staleness is checkable without a tokenizer.
pub fn note_hash(model_id: &str, title: Option<&str>, body: &str) -> String {
    content_hash(model_id, &note_embedding_text(title, body))
}

/// Split a note's embedding text into chunks of at most `max_tokens` tokens,
/// along Markdown paragraphs (blank-line separated), with a one-paragraph
/// overlap between consecutive chunks. A note that fits is a single chunk.
/// `tokens` decides the budget: exact for the builtin model, an estimate for
/// remote embedders — either way it is deterministic for a given model.
pub fn chunk_note(
    title: Option<&str>,
    body: &str,
    max_tokens: usize,
    tokens: &dyn Fn(&str) -> usize,
) -> Vec<String> {
    let text = note_embedding_text(title, body);
    if tokens(&text) <= max_tokens {
        return vec![text];
    }
    // Paragraph split on blank lines, keeping the separators deterministic.
    let paragraphs: Vec<String> = text
        .split("\n\n")
        .flat_map(|p| {
            // A single paragraph longer than the budget is hard-split on word
            // boundaries; the tokenizer would truncate it otherwise.
            if tokens(p) > max_tokens {
                split_long_paragraph(p, max_tokens, tokens)
            } else {
                vec![p.to_string()]
            }
        })
        .collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_total = 0_usize;
    let mut last_paragraph: Option<String> = None;
    for p in paragraphs {
        let pt = tokens(&p);
        if !current.is_empty() && current_total + pt > max_tokens {
            chunks.push(current.join("\n\n"));
            current.clear();
            current_total = 0;
            // The new chunk starts with the previous chunk's last paragraph
            // (one-paragraph overlap) — unless that alone would not leave
            // room for the incoming paragraph (the overlap is a courtesy,
            // the budget is the law).
            if let Some(l) = last_paragraph.clone() {
                let lt = tokens(&l);
                if lt + pt <= max_tokens {
                    current.push(l);
                    current_total = lt;
                }
            }
        }
        current.push(p.clone());
        current_total += pt;
        last_paragraph = Some(p);
    }
    if !current.is_empty() {
        chunks.push(current.join("\n\n"));
    }
    chunks.retain(|c| !c.trim().is_empty());
    if chunks.is_empty() {
        chunks.push(text);
    }
    chunks
}

/// Word-boundary split of one oversized paragraph into pieces that fit.
fn split_long_paragraph(p: &str, max_tokens: usize, tokens: &dyn Fn(&str) -> usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for word in p.split(' ') {
        if !current.is_empty() {
            let candidate = format!("{} {word}", current.join(" "));
            if tokens(&candidate) > max_tokens {
                out.push(current.join(" "));
                current.clear();
            }
        }
        current.push(word);
    }
    if !current.is_empty() {
        out.push(current.join(" "));
    }
    if out.is_empty() {
        out.push(p.to_string());
    }
    // A single word can still exceed the budget: keep it whole (truncation at
    // the tokenizer level is the only option for unbreakable content).
    out
}

/// Decode an f32 vector stored as a little-endian BLOB (see schema v4).
///
/// # Errors
/// [`EmbedError::BadModel`] when the blob length is not a multiple of 4.
pub fn decode_vec(blob: &[u8], dim: usize) -> Result<Vec<f32>, EmbedError> {
    if !blob.len().is_multiple_of(4) || blob.len() / 4 != dim {
        return Err(EmbedError::BadModel(format!(
            "vector blob holds {} floats, expected {dim}",
            blob.len() / 4
        )));
    }
    Ok(blob
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

/// Encode an f32 vector as a little-endian BLOB.
pub fn encode_vec(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// L2-normalise a vector in place (zero vectors stay zero).
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>();
    if norm <= 0.0 {
        return;
    }
    let inv = 1.0 / norm.sqrt();
    for x in v.iter_mut() {
        *x = ((*x) as f64 * inv) as f32;
    }
}

/// Cosine similarity of two L2-normalised vectors (a plain dot product).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    dot_f32(&a[..n], &b[..n])
}

/// Dot product with 8 independent accumulators (`chunks_exact(8)`), so LLVM
/// vectorises it: the naive sum measured 20× slower on 384-dim vectors.
pub fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = [0.0_f64; 8];
    for (x, y) in a[..n].chunks_exact(8).zip(b[..n].chunks_exact(8)) {
        for j in 0..8 {
            acc[j] += f64::from(x[j]) * f64::from(y[j]);
        }
    }
    let mut rest = 0.0_f64;
    let head = n - n % 8;
    for i in head..n {
        rest += f64::from(a[i]) * f64::from(b[i]);
    }
    (acc.iter().sum::<f64>() + rest) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOK: fn(&str) -> usize = |s: &str| s.split_whitespace().count();

    #[test]
    fn short_note_is_one_chunk() {
        assert_eq!(
            chunk_note(Some("Titolo"), "corpo", 8, &TOK),
            vec!["Titolo\n\ncorpo".to_string()]
        );
    }

    #[test]
    fn long_note_chunks_by_paragraph_with_overlap() {
        let body = "uno due tre.\n\nquattro cinque.\n\nsei sette.\n\notto nove.";
        let chunks = chunk_note(None, body, 4, &TOK);
        assert!(chunks.len() >= 2, "{chunks:?}");
        // each chunk fits
        for c in &chunks {
            assert!(TOK(c) <= 4 + 1, "{c}"); // +1: the separator words
        }
        // consecutive chunks share one paragraph (the overlap)
        assert!(chunks[1].contains("quattro"), "{chunks:?}");
        // nothing is lost
        let joined: String = chunks.concat();
        for w in ["uno", "otto", "nove"] {
            assert!(joined.contains(w), "lost {w}: {chunks:?}");
        }
    }

    #[test]
    fn oversized_paragraph_is_hard_split() {
        let body = "aa bb cc dd ee ff gg hh ii jj kk ll mm nn oo pp";
        let chunks = chunk_note(None, body, 4, &TOK);
        assert!(chunks.len() >= 3, "{chunks:?}");
        for c in &chunks {
            assert!(TOK(c) <= 4, "{c}");
        }
        assert!(chunks.concat().contains("pp"));
    }

    #[test]
    fn hashes_bind_the_model() {
        let a = content_hash("m1", "testo");
        let b = content_hash("m2", "testo");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert_eq!(
            note_hash("m1", Some("t"), "b"),
            content_hash("m1", "t\n\nb")
        );
    }

    #[test]
    fn vec_round_trip_and_normalise() {
        let mut v = vec![3.0_f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        let blob = encode_vec(&v);
        assert_eq!(decode_vec(&blob, 2).unwrap(/* test-only */), v);
        assert!(decode_vec(&blob[..5], 2).is_err());
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn dot_matches_naive_sum() {
        let a: Vec<f32> = (0..37).map(|i| (i as f32) * 0.5 - 9.0).collect();
        let b: Vec<f32> = (0..37).map(|i| 1.0 / (i as f32 + 1.0)).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot_f32(&a, &b) - naive).abs() < 1e-4);
    }
}
