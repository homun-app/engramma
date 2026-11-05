// SPDX-License-Identifier: Apache-2.0

//! Ollama embedder: `POST {url}/api/embed` (DESIGN §7).

use std::time::Duration;

use engramma_core::embed::{TextKind, l2_normalize};
use engramma_core::{EmbedError, Embedder};

/// An Ollama endpoint. Vectors are L2-normalised locally (Ollama returns raw
/// model output).
#[derive(Debug, Clone)]
pub struct Ollama {
    url: String,
    model: String,
    model_id: String,
    dim: usize,
    max_tokens: usize,
    timeout: Duration,
}

impl Ollama {
    /// A server at `url` (e.g. `http://127.0.0.1:11434`) serving `model`
    /// with vectors of `dim` dimensions. `max_tokens` is the context length
    /// you want documents chunked to.
    ///
    /// # Errors
    /// Never; constructors are total.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
        max_tokens: usize,
    ) -> Self {
        let model = model.into();
        let model_id = format!("ollama:{model}");
        Self {
            url: url.into(),
            model,
            model_id,
            dim,
            max_tokens,
            timeout: Duration::from_secs(60),
        }
    }

    /// Override the request timeout (default 60 s).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Embedder for Ollama {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    fn embed(&self, texts: &[&str], _kind: TextKind) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let text = crate::http::post_json(
            &format!("{}/api/embed", self.url),
            &body,
            None,
            self.timeout,
        )?;
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| EmbedError::Remote(format!("ollama sent malformed JSON: {e}")))?;
        let embeddings = parsed
            .get("embeddings")
            .and_then(|v| v.as_array())
            .ok_or_else(|| EmbedError::Remote("ollama sent no `embeddings` array".into()))?;
        let mut out = Vec::with_capacity(embeddings.len());
        for (i, v) in embeddings.iter().enumerate() {
            let raw = v
                .as_array()
                .ok_or_else(|| EmbedError::Remote(format!("embedding {i} is not an array")))?;
            if raw.len() != self.dim {
                return Err(EmbedError::Dimension {
                    expected: self.dim,
                    got: raw.len(),
                });
            }
            let mut vec: Vec<f32> = raw
                .iter()
                .map(|x| {
                    x.as_f64().map(|f| f as f32).ok_or_else(|| {
                        EmbedError::Remote(format!("embedding {i} has a non-number"))
                    })
                })
                .collect::<Result<_, _>>()?;
            l2_normalize(&mut vec);
            out.push(vec);
        }
        if out.len() != texts.len() {
            return Err(EmbedError::Remote(format!(
                "ollama returned {} embeddings for {} texts",
                out.len(),
                texts.len()
            )));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::FakeServer;
    use std::time::Duration;

    fn embedder(url: &str, timeout: Duration) -> Ollama {
        Ollama::new(url, "granite", 4, 512).with_timeout(timeout)
    }

    #[test]
    fn request_body_and_response() {
        let server = FakeServer::start(|_req| {
            (
                200,
                r#"{"embeddings": [[3.0, 0.0, 0.0, 0.0], [0.0, 6.0, 0.0, 0.0]]}"#.into(),
            )
        });
        let e = embedder(&server.url(), Duration::from_secs(5));
        let out = e
            .embed(&["prima", "seconda"], TextKind::Document)
            .unwrap(/* test-only */);
        // normalised locally
        assert!((out[0][0] - 1.0).abs() < 1e-6, "{out:?}");
        assert!((out[1][1] - 1.0).abs() < 1e-6, "{out:?}");
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert!(reqs[0].path.ends_with("/api/embed"), "{}", reqs[0].path);
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap(/* test-only */);
        assert_eq!(body["model"], "granite");
        assert_eq!(body["input"], serde_json::json!(["prima", "seconda"]));
    }

    #[test]
    fn http_500_and_malformed_json_are_typed_errors() {
        let server = FakeServer::start(|_| (500, r#"{"error": "boom"}"#.into()));
        let e = embedder(&server.url(), Duration::from_secs(5));
        let err = e.embed(&["x"], TextKind::Query).unwrap_err(/* test-only */);
        assert!(matches!(err, EmbedError::Remote(_)), "{err}");
        assert!(err.to_string().contains("500"), "{err}");

        let server2 = FakeServer::start(|_| (200, "not json at all".into()));
        let e2 = embedder(&server2.url(), Duration::from_secs(5));
        let err2 = e2.embed(&["x"], TextKind::Query).unwrap_err(/* test-only */);
        assert!(matches!(err2, EmbedError::Remote(_)), "{err2}");
        assert!(err2.to_string().contains("malformed"), "{err2}");
    }

    #[test]
    fn timeout_is_a_typed_error() {
        let server = FakeServer::start(|_| {
            std::thread::sleep(Duration::from_millis(1200));
            (200, r#"{"embeddings": [[1.0, 0.0, 0.0, 0.0]]}"#.into())
        });
        let e = embedder(&server.url(), Duration::from_millis(300));
        let err = e.embed(&["x"], TextKind::Query).unwrap_err(/* test-only */);
        assert!(matches!(err, EmbedError::Remote(_)), "{err}");
    }

    #[test]
    fn wrong_dimension_is_caught() {
        let server = FakeServer::start(|_| (200, r#"{"embeddings": [[1.0, 2.0]]}"#.into()));
        let e = embedder(&server.url(), Duration::from_secs(5));
        let err = e.embed(&["x"], TextKind::Query).unwrap_err(/* test-only */);
        assert!(
            matches!(
                err,
                EmbedError::Dimension {
                    expected: 4,
                    got: 2
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn empty_batch_never_calls_the_server() {
        let server = FakeServer::start(|_| (200, "{}".into()));
        let e = embedder(&server.url(), Duration::from_secs(5));
        assert!(e.embed(&[], TextKind::Query).unwrap(/* test-only */).is_empty());
        assert!(server.requests().is_empty());
    }
}
