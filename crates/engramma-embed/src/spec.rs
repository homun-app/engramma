// SPDX-License-Identifier: Apache-2.0

//! Model manifests: everything the builtin embedder needs to load a model
//! folder, and everything `engramma model` needs to download and verify one
//! (DESIGN §7).

use std::path::{Path, PathBuf};

use engramma_core::EmbedError;
use serde::{Deserialize, Serialize};

/// How the sentence vector is pooled from `last_hidden_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pooling {
    /// Take the first token's hidden state (Granite R2: CLS).
    Cls,
    /// Attention-mask-weighted mean of all token states.
    Mean,
}

/// One file of a model, with its verified sha256.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFile {
    /// Path inside the model folder (e.g. `onnx/model.onnx`).
    pub path: String,
    /// Expected sha256 (hex).
    pub sha256: String,
    /// Size in bytes.
    pub bytes: u64,
}

/// A model manifest: the builtin registry entries and the user-supplied
/// folders (EmbeddingGemma comes this way — its Gemma licence forbids
/// redistributing or auto-downloading the weights).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Short name, e.g. `granite-embedding-97m-multilingual-r2`.
    pub id: String,
    /// Hugging Face repository (`<org>/<name>`), or empty for local-only
    /// models.
    pub repo: String,
    /// Pinned revision (commit sha).
    pub revision: String,
    /// The two big files, verified at open time.
    pub files: Vec<ModelFile>,
    /// Vector dimension.
    pub dim: usize,
    /// Pooling over `last_hidden_state`.
    pub pooling: Pooling,
    /// Maximum sequence length in tokens (documents are chunked to this).
    pub max_tokens: usize,
    /// Optional instruction prepended to queries.
    #[serde(default)]
    pub query_prefix: Option<String>,
    /// Optional instruction prepended to documents.
    #[serde(default)]
    pub document_prefix: Option<String>,
}

impl ModelSpec {
    /// The full model identifier used everywhere a vector is stamped:
    /// `<id>@<first 8 chars of the revision>`.
    ///
    /// ```
    /// # use engramma_embed::spec::ModelSpec;
    /// let spec = ModelSpec::granite_97m();
    /// assert!(spec.model_id().starts_with("granite-embedding-97m-multilingual-r2@"));
    /// ```
    #[must_use]
    pub fn model_id(&self) -> String {
        let rev: String = self.revision.chars().take(8).collect();
        format!("{}@{rev}", self.id)
    }

    /// The builtin 97M model (Apache-2.0, 384 dims).
    #[must_use]
    pub fn granite_97m() -> Self {
        Self {
            id: "granite-embedding-97m-multilingual-r2".into(),
            repo: "ibm-granite/granite-embedding-97m-multilingual-r2".into(),
            revision: "835ad14087e140460703cf0fae09f97d469d65c2".into(),
            files: vec![
                ModelFile {
                    path: "tokenizer.json".into(),
                    sha256: "4f2842d568e2724370aec203652a42ac783c7937f8347a1a2cc7506d71f1582f"
                        .into(),
                    bytes: 30_635_493,
                },
                ModelFile {
                    path: "onnx/model.onnx".into(),
                    sha256: "68e592b160673d30250824c1116bc6ab33f70efb22b97c9e1d7ce1e69c1c9d70"
                        .into(),
                    bytes: 390_004_608,
                },
            ],
            dim: 384,
            pooling: Pooling::Cls,
            max_tokens: 512,
            query_prefix: None,
            document_prefix: None,
        }
    }

    /// The builtin 311M model (Apache-2.0, 768 dims).
    #[must_use]
    pub fn granite_311m() -> Self {
        Self {
            id: "granite-embedding-311m-multilingual-r2".into(),
            repo: "ibm-granite/granite-embedding-311m-multilingual-r2".into(),
            revision: "44399559930365213510b1ee2eb15ded83374f0e".into(),
            files: vec![
                ModelFile {
                    path: "tokenizer.json".into(),
                    sha256: "0087c868b33bad550a78a08d19798cfd7f713cde4f020803b8f51f405503e15f"
                        .into(),
                    bytes: 30_631_637,
                },
                ModelFile {
                    path: "onnx/model.onnx".into(),
                    sha256: "75f9f258bf5013f5fe8a4dad61dd0fd16ac0cbaa7a106e3d3f41c2d04a42d541"
                        .into(),
                    bytes: 1_247_170_481,
                },
            ],
            dim: 768,
            pooling: Pooling::Cls,
            max_tokens: 512,
            query_prefix: None,
            document_prefix: None,
        }
    }

    /// The models `engramma model` knows how to download.
    #[must_use]
    pub fn builtin_models() -> Vec<ModelSpec> {
        vec![Self::granite_97m(), Self::granite_311m()]
    }

    /// Load a user-supplied manifest (`manifest.json` inside a model folder).
    /// EmbeddingGemma is supported this way only: the Gemma licence terms do
    /// not allow bundling or auto-downloading its weights.
    ///
    /// # Errors
    /// [`EmbedError::BadModel`] when the manifest is unreadable or invalid.
    pub fn from_folder(dir: &Path) -> Result<Self, EmbedError> {
        let path = dir.join("manifest.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| EmbedError::ModelMissing(format!("{}: {e}", path.display())))?;
        let spec: ModelSpec = serde_json::from_str(&text)
            .map_err(|e| EmbedError::BadModel(format!("{}: {e}", path.display())))?;
        if spec.files.len() < 2 {
            return Err(EmbedError::BadModel(
                "a model manifest must list at least the tokenizer and the ONNX file".into(),
            ));
        }
        Ok(spec)
    }

    /// Absolute path of one of this model's files, given the model root.
    #[must_use]
    pub fn file_path(&self, root: &Path, file: &ModelFile) -> PathBuf {
        root.join(&self.id).join(&self.revision).join(&file.path)
    }

    /// The download URL of one of this model's files.
    #[must_use]
    pub fn download_url(&self, file: &ModelFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, file.path
        )
    }
}

/// Resolve the model root: `$ENGRAMMA_MODEL_DIR` when set, otherwise the
/// default cache (`~/.cache/engramma/models`).
#[must_use]
pub fn model_root() -> PathBuf {
    if let Ok(dir) = std::env::var("ENGRAMMA_MODEL_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var("HOME")
        .map(|h| Path::new(&h).join(".cache").join("engramma").join("models"))
        .unwrap_or_else(|_| PathBuf::from(".engramma-models"))
}

/// A one-character directory-name check for model folders.
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Find a model folder by short name (`granite-embedding-97m…`) or full
/// `model_id` (`granite-embedding-97m…@835ad140`) under `root`, and return
/// its spec. Builtin names match their pinned specs; anything else must be a
/// user folder with its own `manifest.json`.
///
/// # Errors
/// [`EmbedError::ModelMissing`] when nothing matches;
/// [`EmbedError::BadModel`] for a broken user manifest.
pub fn resolve(root: &Path, name_or_id: &str) -> Result<(ModelSpec, PathBuf), EmbedError> {
    let short = name_or_id.split('@').next().unwrap_or(name_or_id);
    if !safe_component(short) {
        return Err(EmbedError::BadModel(format!(
            "`{short}` is not a valid model name"
        )));
    }
    for spec in ModelSpec::builtin_models() {
        if spec.id == short {
            let dir = root.join(&spec.id).join(&spec.revision);
            return Ok((spec, dir));
        }
    }
    // User-supplied folder: <root>/<name>/manifest.json (any revision layout)
    let direct = root.join(short);
    if direct.join("manifest.json").is_file() {
        return Ok((ModelSpec::from_folder(&direct)?, direct));
    }
    // or <root>/<name>/<revision>/manifest.json
    let entries = std::fs::read_dir(&direct).map_err(|_| {
        EmbedError::ModelMissing(format!(
            "no model named `{short}` under {} (run `engramma model list`)",
            root.display()
        ))
    })?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.join("manifest.json").is_file() {
            return Ok((ModelSpec::from_folder(&dir)?, dir));
        }
    }
    Err(EmbedError::ModelMissing(format!(
        "no model named `{short}` under {}",
        root.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_carry_the_revision() {
        let m97 = ModelSpec::granite_97m();
        assert_eq!(
            m97.model_id(),
            "granite-embedding-97m-multilingual-r2@835ad140"
        );
        let m311 = ModelSpec::granite_311m();
        assert_eq!(
            m311.model_id(),
            "granite-embedding-311m-multilingual-r2@44399559"
        );
    }

    #[test]
    fn urls_point_at_the_pinned_revision() {
        let spec = ModelSpec::granite_97m();
        assert_eq!(
            spec.download_url(&spec.files[1]),
            "https://huggingface.co/ibm-granite/granite-embedding-97m-multilingual-r2/\
             resolve/835ad14087e140460703cf0fae09f97d469d65c2/onnx/model.onnx"
        );
    }

    #[test]
    fn resolve_rejects_unknown_names() {
        let tmp = tempfile::tempdir().unwrap(/* test-only */);
        assert!(matches!(
            resolve(tmp.path(), "non-esiste"),
            Err(EmbedError::ModelMissing(_))
        ));
        assert!(matches!(
            resolve(tmp.path(), "../escape"),
            Err(EmbedError::BadModel(_))
        ));
    }

    #[test]
    fn user_folder_manifest_round_trip() {
        let tmp = tempfile::tempdir().unwrap(/* test-only */);
        let dir = tmp.path().join("mymodel").join("rev1");
        std::fs::create_dir_all(&dir).unwrap(/* test-only */);
        let spec = ModelSpec {
            id: "mymodel".into(),
            repo: String::new(),
            revision: "rev1".into(),
            files: vec![
                ModelFile {
                    path: "tokenizer.json".into(),
                    sha256: "00".into(),
                    bytes: 1,
                },
                ModelFile {
                    path: "onnx/model.onnx".into(),
                    sha256: "00".into(),
                    bytes: 2,
                },
            ],
            dim: 8,
            pooling: Pooling::Mean,
            max_tokens: 128,
            query_prefix: Some("q: ".into()),
            document_prefix: None,
        };
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string(&spec).unwrap(/* test-only */),
        )
        .unwrap(/* test-only */);
        let (got, path) = resolve(tmp.path(), "mymodel").unwrap(/* test-only */);
        assert_eq!(got.model_id(), "mymodel@rev1");
        assert!(path.ends_with("rev1"));
        assert_eq!(got.query_prefix.as_deref(), Some("q: "));
    }
}
