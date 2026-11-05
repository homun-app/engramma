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
    threads: usize,
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

        // The tokenizer itself stays unconfigured: truncation and padding are
        // ours (see `embed`). A truncating tokenizer would also cap
        // `approx_tokens`, which must see the true length to chunk notes.
        let tokenizer = hf::Tokenizer::from_file(&tok_path)
            .map_err(|e| EmbedError::BadModel(format!("{}: {e}", tok_path.display())))?;

        // Load the ONNX graph and give both inputs symbolic batch and
        // sequence dims. The symbols must come from `model.symbols` — a
        // fresh SymbolScope makes tract fail on dim unification.
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

        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        Ok(Self {
            model_id: spec.model_id(),
            spec: spec.clone(),
            tokenizer,
            plan,
            mask_slots,
            threads: threads.max(1),
        })
    }

    /// The manifest this model was loaded from.
    #[must_use]
    pub fn spec(&self) -> &ModelSpec {
        &self.spec
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
        // Truncate to max_tokens first, then pad the batch to its longest
        // sequence (right padding; CLS pooling and the attention mask make
        // the result padding-invariant — proven by the fidelity tests).
        let max = self.spec.max_tokens;
        let seq = encodings
            .iter()
            .map(|e| e.get_ids().len().min(max))
            .max()
            .unwrap_or(0);
        if seq == 0 {
            return Err(EmbedError::BadModel("empty token sequence".into()));
        }
        let pad_id = encodings
            .first()
            .map(|e| e.get_ids().last().copied().unwrap_or(0))
            .unwrap_or(0);
        let mut ids: Vec<i64> = Vec::with_capacity(encodings.len() * seq);
        let mut mask: Vec<i64> = Vec::with_capacity(encodings.len() * seq);
        for e in &encodings {
            let n = e.get_ids().len().min(max);
            ids.extend(e.get_ids()[..n].iter().map(|&t| i64::from(t)));
            mask.extend(e.get_attention_mask()[..n].iter().map(|&m| i64::from(m)));
            for _ in n..seq {
                ids.push(i64::from(pad_id));
                mask.push(0);
            }
        }

        // Independent sub-batches run in parallel (std::thread::scope), up to
        // available_parallelism(); the shared plan only takes &self in run().
        let batch_len = encodings.len().div_ceil(self.threads).max(1);
        let mut chunks: Vec<(Vec<i64>, Vec<i64>)> = Vec::new();
        let mut rest_ids = ids.as_slice();
        let mut rest_mask = mask.as_slice();
        while !rest_ids.is_empty() {
            let take = batch_len.min(rest_ids.len() / seq);
            chunks.push((
                rest_ids[..take * seq].to_vec(),
                rest_mask[..take * seq].to_vec(),
            ));
            rest_ids = &rest_ids[take * seq..];
            rest_mask = &rest_mask[take * seq..];
        }
        let plan = Arc::clone(&self.plan);
        let mask_slots = &self.mask_slots;
        let dim = self.spec.dim;
        let pooling = self.spec.pooling;
        let results: Vec<Result<Vec<Vec<f32>>, EmbedError>> =
            std::thread::scope(|scope| -> Vec<Result<Vec<Vec<f32>>, EmbedError>> {
                let mut handles = Vec::with_capacity(chunks.len());
                for (cids, cmask) in &chunks {
                    // Each thread gets its own Arc clone of the plan.
                    let plan = Arc::clone(&plan);
                    let cids = cids.clone();
                    let cmask = cmask.clone();
                    handles.push(scope.spawn(move || {
                        Self::embed_chunk(&plan, mask_slots, dim, pooling, &cids, &cmask, seq)
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .map_err(|_| {
                                EmbedError::BadModel("an embedding worker thread panicked".into())
                            })
                            .and_then(|r| r)
                    })
                    .collect()
            });
        let mut all: Vec<Vec<f32>> = Vec::with_capacity(encodings.len());
        for r in results {
            all.extend(r?);
        }
        if all.len() != texts.len() {
            return Err(EmbedError::BadModel(format!(
                "embedded {} texts out of {}",
                all.len(),
                texts.len()
            )));
        }
        Ok(all)
    }
}
