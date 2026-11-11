// SPDX-License-Identifier: Apache-2.0

//! The builtin embedder: ONNX via tract, pure Rust (DESIGN §7, decided
//! 2026-11-05: same vectors as ONNX Runtime on Granite R2, no native library
//! to ship — so this also builds for iOS, Android and WASM).

use std::path::Path;
use std::sync::Arc;

use engramma_core::embed::{TextKind, l2_normalize};
use engramma_core::{EmbedError, Embedder};
use tokenizers as hf;
use tract_onnx::prelude::{
    DatumType, Framework, InferenceFact, InferenceModelExt, IntoRunnable, IntoTensor,
    RunnableModel, TValue, ToDim, TypedFact, tvec,
};

use crate::spec::{self, ModelSpec, Pooling};

type Plan = Arc<RunnableModel<TypedFact, Box<dyn tract_onnx::prelude::TypedOp>>>;

/// v0.4.6 (P1): the token budget of one forward pass —
/// `rows × batch_seq ≤ ENGRAMMA_EMBED_TOKEN_BUDGET`. The v0.4.5 rule
/// (lowest peak whose 64-note time stays within +15% of the v0.4.4 8,6 s)
/// now admits (4096, 1) — without the tokenizer padding one worker does
/// the workload in 6,4 s, −26% vs v0.4.4 — and the default FOLLOWS the
/// rule: peak 1229 MB (system allocator), 1126 under mimalloc (the
/// grid of REPORT v0.4.6). The activation memory of a pass stops
/// following the batch the caller happens to send.
const DEFAULT_TOKEN_BUDGET: usize = 4096;

/// v0.4.5 (P2): `ENGRAMMA_EMBED_TOKEN_BUDGET` — a whole number ≥ 1;
/// anything else (0, non-numeric, unset) falls back to the default.
fn parse_budget(v: &str) -> Option<usize> {
    v.trim().parse::<usize>().ok().filter(|&n| n >= 1)
}

// v0.4.5 (P2.3b, ATTEMPTED AND DROPPED): an NNEF cache of the graph —
// warm opens would skip the ONNX decode. It cannot work on this model
// with tract 0.23: the OPTIMIZED graph is not serializable (`OptMul`
// and friends have no NNEF serializer) and the TYPED graph is not
// either (`StridedSlice`); custom serializers are a project of their
// own. Measured: the cold load peaks at ~1,11 GB and takes 0,6–2,4 s
// (file cache aside) — the numbers are in the REPORT v0.4.5 so nobody
// retries this. The load path keeps only (a): conversions that consume
// the previous graph, ours never holding a stage alive.

/// An error from tract, in our typed wrapper.
fn tract_err(e: impl std::fmt::Display) -> EmbedError {
    EmbedError::BadModel(format!("tract: {e}"))
}

/// The loaded builtin model.
pub struct Builtin {
    spec: ModelSpec,
    model_id: String,
    tokenizer: hf::Tokenizer,
    plan: Plan,
    /// For each input slot of the graph: `true` = attention_mask, `false` =
    /// input_ids. Inputs are fed in slot order, whatever the export order.
    mask_slots: Vec<bool>,
    /// The fixed worker pool (v0.4.5 P2).
    threads: usize,
    /// The forward-pass token budget (v0.4.5 P2).
    token_budget: usize,
    /// The pad_id of the tokenizer's own padding config, when it ships
    /// one (both Granite R2 do) — used as OUR batch pad_id after the
    /// config is stripped (v0.4.6 P0).
    pad_id: Option<u32>,
}

impl Builtin {
    /// Load a model by short name or full `model_id` under `root`
    /// (see [`spec::model_root`]). The two big files are sha256-verified
    /// before the ONNX graph is parsed (with the mtime/size result cache).
    ///
    /// # Errors
    /// [`EmbedError`] when the model is missing, fails its hash check or
    /// cannot be loaded.
    pub fn load_named(root: &Path, name_or_id: &str) -> Result<Self, EmbedError> {
        let (spec, _dir) = spec::resolve(root, name_or_id)?;
        Self::load(root, &spec)
    }

    /// Load a model described by an explicit manifest.
    ///
    /// # Errors
    /// As [`Builtin::load_named`].
    pub fn load(root: &Path, spec: &ModelSpec) -> Result<Self, EmbedError> {
        crate::verify::verify_model(root, spec)?;
        let dir = root.join(&spec.id).join(&spec.revision);
        let onnx_path = dir.join("onnx").join("model.onnx");
        let tok_path = dir.join("tokenizer.json");

        // The tokenizer of both Granite R2 models ships with BatchLongest
        // PADDING in tokenizer.json (v0.4.6 P0): a bare `from_file` makes
        // `encode_batch` pad every text to the longest of the call — the
        // v0.4.5 length sort sorted nothing (all lens equal) and a short
        // note paid the longest's compute. Padding is OURS (see `embed`);
        // the config's pad_id, when there is one, is kept for it. The
        // tokenizer's own TRUNCATION stays (max_length 32768, far above
        // our max_tokens cut): `approx_tokens` must see the true length
        // to chunk notes.
        let mut tokenizer = hf::Tokenizer::from_file(&tok_path)
            .map_err(|e| EmbedError::BadModel(format!("{}: {e}", tok_path.display())))?;
        let pad_id = tokenizer.get_padding().map(|p| p.pad_id);
        tokenizer.with_padding(None);

        // v0.4.6 (P1): ONE worker by default. The v0.4.5 rule (lowest
        // peak within +15% of the v0.4.4 64-note time) had already picked
        // (4096, 1) by peak and rejected it only on time (+42%); with the
        // tokenizer padding gone the time is 6,4 s (−26%) and the default
        // FOLLOWS the rule — the v0.4.5 phase chose (2048, 2) and then
        // kept min(4, available_parallelism), an inconsistency this
        // closes. Peak 1229 MB at one worker vs 1331–2355 everywhere else
        // on the grid (REPORT v0.4.6). `ENGRAMMA_EMBED_THREADS` (a whole
        // number ≥ 1) still overrides — the bulk-import knob: each worker
        // adds rate and peak in equal measure.
        let threads = std::env::var("ENGRAMMA_EMBED_THREADS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(1);
        let token_budget = std::env::var("ENGRAMMA_EMBED_TOKEN_BUDGET")
            .ok()
            .as_deref()
            .and_then(parse_budget)
            .unwrap_or(DEFAULT_TOKEN_BUDGET);

        // The load path (v0.4.5 P2.3a): decode the ONNX, give both inputs
        // symbolic batch and sequence dims. The symbols must come from
        // `model.symbols` — a fresh SymbolScope makes tract fail on dim
        // unification.
        let mut model = tract_onnx::onnx()
            .model_for_path(&onnx_path)
            .map_err(tract_err)?;
        let batch = model.symbols.new_with_prefix("batch");
        let seq = model.symbols.new_with_prefix("seq");
        let fact = InferenceFact::dt_shape(DatumType::I64, tvec!(batch.to_dim(), seq.to_dim()));
        let mut mask_slots = Vec::with_capacity(model.inputs.len());
        for outlet in &model.inputs {
            let name = model.nodes[outlet.node].name.clone();
            match name.as_str() {
                "input_ids" => mask_slots.push(false),
                "attention_mask" => mask_slots.push(true),
                other => {
                    return Err(EmbedError::BadModel(format!(
                        "unexpected model input `{other}` (want input_ids and attention_mask)"
                    )));
                }
            }
        }
        for slot in 0..model.inputs.len() {
            model
                .set_input_fact(slot, fact.clone())
                .map_err(tract_err)?;
        }
        // v0.4.5 (P2.3a): each conversion consumes the previous graph —
        // `into_typed(self)`, `into_optimized(self)` — so at most two
        // weight copies are alive at any instant, ours never longer.
        let typed = model.into_typed().map_err(tract_err)?;
        let typed = typed.into_optimized().map_err(tract_err)?;
        // Sanity: one output, the hidden states, shaped [batch, seq, dim].
        let out_shape = &typed.output_fact(0).map_err(tract_err)?.shape;
        if out_shape.rank() != 3 {
            return Err(EmbedError::BadModel(format!(
                "expected a [batch, seq, dim] output, got {out_shape:?}"
            )));
        }
        let plan = typed.into_runnable().map_err(tract_err)?;

        Ok(Self {
            model_id: spec.model_id(),
            spec: spec.clone(),
            tokenizer,
            plan,
            mask_slots,
            threads: threads.max(1),
            token_budget,
            pad_id,
        })
    }

    /// The manifest this model was loaded from.
    #[must_use]
    pub fn spec(&self) -> &ModelSpec {
        &self.spec
    }

    /// Override this instance's forward-pass token budget (v0.4.5 P2;
    /// otherwise `ENGRAMMA_EMBED_TOKEN_BUDGET` or the default stands).
    /// Values < 1 keep the current budget. The vectors are bit-identical
    /// at the SAME configuration; across budgets the batch shape changes
    /// the summation order inside tract and the last digits move — the
    /// fidelity tests bound it at cos ≥ 0,999999, |Δ| ≤ 1e-5 (v0.4.6 P0).
    pub fn set_token_budget(&mut self, budget: usize) {
        if budget >= 1 {
            self.token_budget = budget;
        }
    }

    /// Embed a slice of the batch: token tensors in graph-slot order, one
    /// forward pass, pooling and L2 per sample.
    fn embed_chunk(
        plan: &Plan,
        mask_slots: &[bool],
        dim: usize,
        pooling: Pooling,
        ids: &[i64],
        mask: &[i64],
        seq: usize,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let n = ids.len() / seq;
        let ids_t = tract_onnx::prelude::Tensor::from_shape(&[n, seq], ids).map_err(tract_err)?;
        let mask_t = tract_onnx::prelude::Tensor::from_shape(&[n, seq], mask).map_err(tract_err)?;
        // Feed the inputs in the graph's slot order.
        let mut inputs: tract_onnx::prelude::TVec<TValue> = tvec![];
        for is_mask in mask_slots {
            inputs.push(if *is_mask {
                TValue::from(mask_t.clone())
            } else {
                TValue::from(ids_t.clone())
            });
        }
        let outputs = plan.run(inputs).map_err(tract_err)?;
        let hidden = outputs
            .into_iter()
            .next()
            .ok_or_else(|| EmbedError::BadModel("the model returned no output".into()))?
            .into_tensor();
        let view = hidden
            .to_plain_array_view::<f32>()
            .map_err(|e| EmbedError::BadModel(format!("output tensor: {e}")))?;
        let flat: &[f32] = view.as_slice().unwrap_or(&[]);
        let mut out = Vec::with_capacity(n);
        for s in 0..n {
            let base = s * seq * dim;
            let mut v: Vec<f32> = match pooling {
                Pooling::Cls => (0..dim).map(|i| flat[base + i]).collect(),
                Pooling::Mean => {
                    let mut acc = vec![0.0_f64; dim];
                    let mut weight = 0.0_f64;
                    for t in 0..seq {
                        let m = mask[s * seq + t] as f64;
                        if m == 0.0 {
                            continue;
                        }
                        weight += m;
                        let row = base + t * dim;
                        for (i, a) in acc.iter_mut().enumerate() {
                            *a += m * f64::from(flat[row + i]);
                        }
                    }
                    acc.iter().map(|a| (a / weight.max(1.0)) as f32).collect()
                }
            };
            l2_normalize(&mut v);
            out.push(v);
        }
        Ok(out)
    }
}

impl Embedder for Builtin {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.spec.dim
    }

    fn max_tokens(&self) -> usize {
        self.spec.max_tokens
    }

    /// Exact tokenizer count (no special tokens): chunking decisions match
    /// what the model will actually see.
    fn approx_tokens(&self, text: &str) -> usize {
        self.tokenizer
            .encode(text, false)
            .map(|e| e.get_ids().len())
            .unwrap_or_else(|_| engramma_core::embed::default_token_estimate(text))
    }

    fn embed(&self, texts: &[&str], kind: TextKind) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prefix = match kind {
            TextKind::Query => self.spec.query_prefix.as_deref(),
            TextKind::Document => self.spec.document_prefix.as_deref(),
        };
        let owned: Vec<String> = texts
            .iter()
            .map(|t| match prefix {
                Some(p) => format!("{p}{t}"),
                None => (*t).to_string(),
            })
            .collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let encodings = self
            .tokenizer
            .encode_batch(refs, true)
            .map_err(|e| EmbedError::BadModel(format!("tokenize: {e}")))?;
        // Truncate to max_tokens first (right padding; CLS pooling and the
        // attention mask make the result padding-invariant — proven by the
        // fidelity tests).
        let max = self.spec.max_tokens;
        let lens: Vec<usize> = encodings
            .iter()
            .map(|e| e.get_ids().len().min(max))
            .collect();
        if lens.iter().all(|&l| l == 0) {
            return Err(EmbedError::BadModel("empty token sequence".into()));
        }

        // v0.4.5 (P2): sort by token length first — the padding of a batch
        // collapses to the gap between neighbours — then cut the sorted
        // order into batches under a token budget: `rows × batch_seq ≤
        // ENGRAMMA_EMBED_TOKEN_BUDGET` (a text longer than the budget runs
        // alone, seq is capped by max_tokens anyway). The activation
        // memory of a forward pass stops following the caller's batch.
        // v0.4.6 (P0): now that the tokenizer no longer pads, the lens
        // are the TRUE lengths — this sort finally has something to sort.
        let mut order: Vec<usize> = (0..lens.len()).collect();
        order.sort_by_key(|&i| lens[i]);
        let budget = self.token_budget.max(1);
        // batches of the sorted order: (row start, rows, batch seq,
        // buffer offset). The offset is the PREFIX SUM of `rows × seq` —
        // `start × seq_b` was right only while every batch shared the
        // same seq, which the v0.4.5 tokenizer-padding accident made true
        // and a no-padding tokenizer breaks (a worker would slice out of
        // bounds).
        let mut batches: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut start = 0_usize;
        let mut offset = 0_usize;
        while start < order.len() {
            let mut rows = 1_usize;
            while start + rows < order.len() {
                // sorted ascending: the next text is the new longest
                let seq_try = lens[order[start + rows]];
                if rows.saturating_add(1).saturating_mul(seq_try) > budget {
                    break;
                }
                rows += 1;
            }
            let seq_b = lens[order[start + rows - 1]];
            batches.push((start, rows, seq_b, offset));
            offset += rows * seq_b;
            start += rows;
        }

        // ids/mask laid out batch by batch in SORTED order: every batch is
        // one contiguous slice of these two buffers, so the workers take
        // slices — not the Vec-copied-twice chunks of v0.4.4. The pad id
        // is the one of the tokenizer's padding config when it ships one
        // (v0.4.6 P0), else the last id of the first text — the attention
        // mask zeroes the pad positions, so the choice cannot change the
        // vectors (the padding tests prove it).
        let pad_id = self.pad_id.unwrap_or_else(|| {
            encodings
                .first()
                .map(|e| e.get_ids().last().copied().unwrap_or(0))
                .unwrap_or(0)
        });
        let total: usize = batches.iter().map(|&(_, rows, seq, _)| rows * seq).sum();
        let mut ids: Vec<i64> = Vec::with_capacity(total);
        let mut mask: Vec<i64> = Vec::with_capacity(total);
        for &(bstart, brows, seq_b, _) in &batches {
            for row in bstart..bstart + brows {
                let e = &encodings[order[row]];
                let n = lens[order[row]];
                ids.extend(e.get_ids()[..n].iter().map(|&t| i64::from(t)));
                mask.extend(e.get_attention_mask()[..n].iter().map(|&m| i64::from(m)));
                for _ in n..seq_b {
                    ids.push(i64::from(pad_id));
                    mask.push(0);
                }
            }
        }

        // v0.4.5 (P2): a fixed pool of workers (min(threads, batches))
        // pulls the batches off a queue; the shared plan only takes &self
        // in run(), the buffers are borrowed, nothing is cloned on the way
        // in. Results land per batch, the original order is restored at
        // the end.
        let workers = self.threads.min(batches.len()).max(1);
        let plan = Arc::clone(&self.plan);
        let mask_slots = &self.mask_slots;
        let dim = self.spec.dim;
        let pooling = self.spec.pooling;
        let queue: std::sync::Mutex<std::collections::VecDeque<usize>> =
            std::sync::Mutex::new((0..batches.len()).collect());
        let (tx, rx) = std::sync::mpsc::channel::<(usize, Result<Vec<Vec<f32>>, EmbedError>)>();
        std::thread::scope(|scope| {
            let batches = batches.as_slice();
            let ids = ids.as_slice();
            let mask = mask.as_slice();
            let mask_slots = mask_slots.as_slice();
            for _ in 0..workers {
                // each worker owns its Arc clone; the buffers travel as
                // shared slices
                let plan = Arc::clone(&plan);
                let tx = tx.clone();
                let queue = &queue;
                scope.spawn(move || {
                    loop {
                        let next = queue.lock().map_or(None, |mut q| q.pop_front());
                        let Some(b) = next else {
                            break;
                        };
                        let (_bstart, brows, seq_b, boff) = batches[b];
                        let from = boff;
                        let to = boff + brows * seq_b;
                        let r = Self::embed_chunk(
                            &plan,
                            mask_slots,
                            dim,
                            pooling,
                            &ids[from..to],
                            &mask[from..to],
                            seq_b,
                        );
                        if tx.send((b, r)).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        // the scope joined: the worker senders are gone; drop the LAST
        // one too, or the drain below would wait for a close that never
        // comes (the bug of the first cut — a deadlock on the very
        // first query)
        drop(tx);
        let mut per_batch: Vec<Option<Result<Vec<Vec<f32>>, EmbedError>>> =
            (0..batches.len()).map(|_| None).collect();
        for (b, r) in rx {
            per_batch[b] = Some(r);
        }
        let mut out: Vec<Option<Vec<f32>>> = (0..texts.len()).map(|_| None).collect();
        for (b, slot) in per_batch.into_iter().enumerate() {
            let vecs = slot.unwrap_or_else(|| {
                Err(EmbedError::BadModel("a batch produced no result".into()))
            })?;
            let (bstart, _rows, _seq_b, _off) = batches[b];
            for (j, v) in vecs.into_iter().enumerate() {
                out[order[bstart + j]] = Some(v);
            }
        }
        let mut all: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for (i, slot) in out.into_iter().enumerate() {
            match slot {
                Some(v) => all.push(v),
                None => return Err(EmbedError::BadModel(format!("text {i} produced no vector"))),
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::{Builtin, parse_budget};

    #[test]
    fn budget_override_needs_a_whole_number_at_least_one() {
        assert_eq!(parse_budget("1"), Some(1));
        assert_eq!(parse_budget(" 4096 "), Some(4096));
        // 0 and junk both mean "the default" (v0.4.5 P2)
        assert_eq!(parse_budget("0"), None);
        assert_eq!(parse_budget(""), None);
        assert_eq!(parse_budget("non-numeric"), None);
        assert_eq!(parse_budget("-8"), None);
        assert_eq!(parse_budget("1.5"), None);
    }

    /// v0.4.6 (P0): the tokenizer of `Builtin::load` must NOT pad. Both
    /// Granite R2 `tokenizer.json` ship with BatchLongest padding, so a
    /// bare `from_file` makes `encode_batch` pad every text to the longest
    /// of the call — the v0.4.5 length sort sorted nothing and a short
    /// note paid the longest's compute (the 100k bulk rate fell 201 → 74
    /// notes/s). This test is the red one of the phase: it fails until
    /// the padding config is stripped in `load`.
    #[test]
    fn the_loaded_tokenizer_does_not_pad() {
        if std::env::var("ENGRAMMA_SKIP_MODEL_TESTS")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            eprintln!("ENGRAMMA_SKIP_MODEL_TESTS=1: skipping the tokenizer test");
            return;
        }
        let root = crate::spec::model_root();
        let b = Builtin::load_named(&root, "granite-embedding-97m-multilingual-r2")
            .unwrap_or_else(|e| panic!("model load: {e}"));
        let encodings = b
            .tokenizer
            .encode_batch(
                vec![
                    "corto",
                    "un testo molto più lungo, con molte parole diverse dentro, abbastanza da \
                     contare davvero qualche decina di token",
                ],
                true,
            )
            .unwrap_or_else(|e| panic!("encode_batch: {e}"));
        assert_eq!(encodings.len(), 2);
        let (l0, l1) = (encodings[0].get_ids().len(), encodings[1].get_ids().len());
        assert_ne!(
            l0, l1,
            "the tokenizer pads the batch: both texts came out {l0} tokens"
        );
    }
}
