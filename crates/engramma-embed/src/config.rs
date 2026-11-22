// SPDX-License-Identifier: Apache-2.0

//! `<root>/config.json`: which embedder a cell uses. Without a configuration
//! the search stays text + graph, exactly as in v0.1.

use std::path::Path;
use std::sync::Arc;

use engramma_core::EmbedError;
use engramma_core::embed::Embedder;

use crate::Builtin;
use crate::http::Http;
use crate::ollama::Ollama;
use crate::spec;

/// The `embedder` object of `config.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmbedderConfig {
    /// `builtin` (tract), `ollama` or `http`.
    pub kind: String,
    /// Model name (`granite-embedding-97m-multilingual-r2` for builtin, the
    /// server's model name for the remote kinds).
    pub model: String,
    /// Server URL (remote kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Environment variable holding the bearer token (`http` only); the
    /// token itself never enters this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// Vector dimension (remote kinds: the server cannot be asked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<usize>,
    /// Context length for chunking (remote kinds; builtin reads its manifest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
}

/// Read `<root>/config.json`; `None` when there is no configuration.
///
/// # Errors
/// [`EmbedError::BadModel`] when the file exists but is not a valid
/// configuration.
pub fn load_config(root: &Path) -> Result<Option<EmbedderConfig>, EmbedError> {
    let path = root.join("config.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| EmbedError::BadModel(format!("{}: {e}", path.display())))?;
    if parsed.get("embedder").is_none() {
        return Ok(None);
    }
    let cfg: EmbedderConfig = serde_json::from_value(
        parsed
            .get("embedder")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| EmbedError::BadModel(format!("{}: embedder: {e}", path.display())))?;
    Ok(Some(cfg))
}

/// Build the embedder described by `cfg`, loading builtin models from
/// `model_root`.
///
/// # Errors
/// [`EmbedError`] when the kind is unknown, the model is missing or a
/// required field (url, dim) is absent.
pub fn build_embedder(
    cfg: &EmbedderConfig,
    model_root: &Path,
) -> Result<Arc<dyn Embedder>, EmbedError> {
    match cfg.kind.as_str() {
        "builtin" => {
            let b = Builtin::load_named(model_root, &cfg.model)?;
            Ok(Arc::new(b))
        }
        "ollama" => {
            let url = cfg
                .url
                .clone()
                .ok_or_else(|| EmbedError::BadModel("an `ollama` embedder needs `url`".into()))?;
            let dim = cfg
                .dim
                .ok_or_else(|| EmbedError::BadModel("an `ollama` embedder needs `dim`".into()))?;
            Ok(Arc::new(Ollama::new(
                url,
                &cfg.model,
                dim,
                cfg.max_tokens.unwrap_or(512),
            )))
        }
        "http" => {
            let url = cfg
                .url
                .clone()
                .ok_or_else(|| EmbedError::BadModel("an `http` embedder needs `url`".into()))?;
            let dim = cfg
                .dim
                .ok_or_else(|| EmbedError::BadModel("an `http` embedder needs `dim`".into()))?;
            Ok(Arc::new(Http::new(
                url,
                &cfg.model,
                dim,
                cfg.max_tokens.unwrap_or(512),
                cfg.token_env.clone(),
            )))
        }
        other => Err(EmbedError::BadModel(format!(
            "unknown embedder kind `{other}` (builtin, ollama, http)"
        ))),
    }
}

/// Convenience: config + model root resolution + build in one call.
///
/// # Errors
/// As [`load_config`] and [`build_embedder`].
pub fn load_embedder(root: &Path) -> Result<Option<Arc<dyn Embedder>>, EmbedError> {
    match load_config(root)? {
        Some(cfg) => Ok(Some(build_embedder(&cfg, &spec::model_root())?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_is_none_and_broken_is_an_error() {
        let tmp = tempfile::Builder::new().prefix("engramma-test-").tempdir().unwrap(/* test-only */);
        assert!(load_config(tmp.path()).unwrap(/* test-only */).is_none());
        std::fs::write(
            tmp.path().join("config.json"),
            "{\"embedder\": {\"kind\": \"nope\", \"model\": \"x\"}}",
        )
        .unwrap(/* test-only */);
        let cfg = load_config(tmp.path()).unwrap(/* test-only */);
        assert!(cfg.is_some());
        assert!(build_embedder(&cfg.unwrap(/* test-only */), tmp.path()).is_err());
    }

    #[test]
    fn remote_kinds_need_url_and_dim() {
        let cfg = EmbedderConfig {
            kind: "ollama".into(),
            model: "m".into(),
            url: None,
            token_env: None,
            dim: None,
            max_tokens: None,
        };
        assert!(build_embedder(&cfg, Path::new(".")).is_err());
        let cfg = EmbedderConfig {
            kind: "http".into(),
            model: "m".into(),
            url: Some("http://127.0.0.1:1".into()),
            token_env: Some("TOK".into()),
            dim: Some(4),
            max_tokens: None,
        };
        assert!(build_embedder(&cfg, Path::new(".")).is_ok());
    }
}
