// SPDX-License-Identifier: Apache-2.0

//! OpenAI-compatible HTTP embedder: `POST {url}/v1/embeddings`
//! (`{"model", "input": [..]}` → `data[i].embedding`). An optional bearer
//! token is read from an environment variable and never appears in logs or
//! errors (DESIGN §7).

use std::time::Duration;

use engramma_core::embed::{TextKind, l2_normalize};
use engramma_core::{EmbedError, Embedder};

/// POST JSON, return the body text. `bearer` is attached as
/// `Authorization: Bearer …` when present; its value is never included in
/// errors (ureq messages carry the URL and status, not headers).
pub(crate) fn post_json(
    url: &str,
    body: &serde_json::Value,
    bearer: Option<&str>,
    timeout: Duration,
) -> Result<String, EmbedError> {
    let mut request = ureq::post(url)
        .config()
        .timeout_global(Some(timeout))
        .build();
    if let Some(token) = bearer {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request
        .send_json(body)
        .map_err(|e| EmbedError::Remote(format!("{url}: {e}")))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| EmbedError::Remote(format!("{url}: body: {e}")))?;
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(300).collect();
        return Err(EmbedError::Remote(format!(
            "{url} answered {status}: {snippet}"
        )));
    }
    Ok(text)
}

/// An OpenAI-compatible endpoint (Ollama's `/v1`, vLLM, OpenAI itself, …).
#[derive(Debug, Clone)]
pub struct Http {
    url: String,
    model: String,
    model_id: String,
    dim: usize,
    max_tokens: usize,
    /// Environment variable holding the bearer token, if any.
    token_env: Option<String>,
    timeout: Duration,
}

impl Http {
    /// A server at `url` (the `/v1/embeddings` path is appended) serving
    /// `model` with vectors of `dim` dimensions. The bearer token, when
    /// `token_env` is set, is read from that environment variable at request
    /// time and never stored.
    #[must_use]
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
        max_tokens: usize,
        token_env: Option<String>,
    ) -> Self {
        let model = model.into();
        let model_id = format!("http:{model}");
        Self {
            url: url.into(),
            model,
            model_id,
            dim,
            max_tokens,
            token_env,
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

impl Embedder for Http {
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
        let bearer = self
            .token_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|t| !t.is_empty());
        let text = post_json(
            &format!("{}/v1/embeddings", self.url.trim_end_matches('/')),
            &body,
            bearer.as_deref(),
            self.timeout,
        )?;
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| EmbedError::Remote(format!("malformed JSON from the endpoint: {e}")))?;
        let data = parsed
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| EmbedError::Remote("the endpoint sent no `data` array".into()))?;
        let mut out = vec![Vec::new(); data.len()];
        for item in data {
            let index = item
                .get("index")
                .and_then(|i| i.as_u64())
                .ok_or_else(|| EmbedError::Remote("an embedding has no `index`".into()))?
                as usize;
            let raw = item
                .get("embedding")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    EmbedError::Remote("an embedding has no `embedding` array".into())
                })?;
            if raw.len() != self.dim {
                return Err(EmbedError::Dimension {
                    expected: self.dim,
                    got: raw.len(),
                });
            }
            let mut vec: Vec<f32> = raw
                .iter()
                .map(|x| {
                    x.as_f64()
                        .map(|f| f as f32)
                        .ok_or_else(|| EmbedError::Remote("an embedding holds a non-number".into()))
                })
                .collect::<Result<_, _>>()?;
            l2_normalize(&mut vec);
            let slot = out.get_mut(index).ok_or_else(|| {
                EmbedError::Remote(format!("embedding index {index} out of range"))
            })?;
            *slot = vec;
        }
        if out.len() != texts.len() || out.iter().any(|v| v.is_empty()) {
            return Err(EmbedError::Remote(format!(
                "the endpoint returned {} embeddings for {} texts",
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

    fn embedder(url: &str, timeout: Duration) -> Http {
        Http::new(url, "m", 4, 512, None).with_timeout(timeout)
    }

    #[test]
    fn request_shape_and_shuffled_indices() {
        let server = FakeServer::start(|_| {
            (
                200,
                r#"{"data": [
                     {"index": 1, "embedding": [0.0, 8.0, 0.0, 0.0]},
                     {"index": 0, "embedding": [5.0, 0.0, 0.0, 0.0]}
                   ]}"#
                .into(),
            )
        });
        let e = embedder(&server.url(), Duration::from_secs(5));
        let out = e
            .embed(&["prima", "seconda"], TextKind::Document)
            .unwrap(/* test-only */);
        // index 0 is "prima", whatever the order in the payload
        assert!((out[0][0] - 1.0).abs() < 1e-6, "{out:?}");
        assert!((out[1][1] - 1.0).abs() < 1e-6, "{out:?}");
        let reqs = server.requests();
        assert_eq!(reqs[0].path, "/v1/embeddings");
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap(/* test-only */);
        assert_eq!(body["model"], "m");
        assert_eq!(body["input"], serde_json::json!(["prima", "seconda"]));
    }

    #[test]
    fn bearer_token_travels_and_is_not_in_errors() {
        let server = FakeServer::start(|req| {
            assert!(
                req.headers
                    .iter()
                    .any(|h| h == "authorization: Bearer sekret"),
                "missing bearer: {:?}",
                req.headers
            );
            (
                200,
                r#"{"data": [{"index": 0, "embedding": [1.0, 0.0, 0.0, 0.0]}]}"#.into(),
            )
        });
        let text = post_json(
            &format!("{}/v1/embeddings", server.url()),
            &serde_json::json!({"model": "m", "input": ["x"]}),
            Some("sekret"),
            Duration::from_secs(5),
        )
        .unwrap(/* test-only */);
        assert!(text.contains("embedding"));
    }

    #[test]
    fn http_500_malformed_and_timeout() {
        let server = FakeServer::start(|_| (500, "bad".into()));
        let err = embedder(&server.url(), Duration::from_secs(5))
            .embed(&["x"], TextKind::Query)
            .unwrap_err(/* test-only */);
        assert!(err.to_string().contains("500"), "{err}");

        let server2 = FakeServer::start(|_| (200, "{".into()));
        let err2 = embedder(&server2.url(), Duration::from_secs(5))
            .embed(&["x"], TextKind::Query)
            .unwrap_err(/* test-only */);
        assert!(matches!(err2, EmbedError::Remote(_)), "{err2}");

        let server3 = FakeServer::start(|_| {
            std::thread::sleep(Duration::from_millis(1200));
            (200, "{}".into())
        });
        let err3 = embedder(&server3.url(), Duration::from_millis(300))
            .embed(&["x"], TextKind::Query)
            .unwrap_err(/* test-only */);
        assert!(matches!(err3, EmbedError::Remote(_)), "{err3}");
    }
}
