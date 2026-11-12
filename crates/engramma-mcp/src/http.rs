// SPDX-License-Identifier: Apache-2.0

//! v0.5.2: `engramma mcp --http` — the HTTP surface. One process serves
//! BOTH roles on one loopback listener (the tunnel in front of it makes
//! it public):
//!
//! - the **authorization server** (P2/P3): the RFC 9728 Protected
//!   Resource Metadata, the RFC 8414 authorization server metadata,
//!   `/authorize` with a LOCAL human approval, `/token`, `/revoke`, and —
//!   only with `--allow-dcr` — RFC 7591 registration;
//! - the **resource server** (P1/P4): `/mcp`, rmcp's Streamable HTTP
//!   transport behind a bearer-token gate that resolves each token to its
//!   grant.
//!
//! The engine does not change nature: engramma stays a local program with
//! one owner. TLS is terminated by the tunnel, the address is only ever
//! loopback, and there is no anonymous mode — even `initialize` wants a
//! token, because read-only tools are personal data too.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

use crate::oauth::store::{OAuthStore, SCOPE_PROPOSE, SCOPE_READ};

/// The configuration of the HTTP surface. The CLI builds one with both
/// test hatches off; the library's own tests may turn them on. The hatches
/// exist because the integration tests run their CIMD server and their
/// whole OAuth flow on 127.0.0.1.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The loopback address to listen on. Anything else is refused.
    pub bind: SocketAddr,
    /// The public URL the tunnel publishes (`https://…`, no query, no
    /// fragment). Everything canonical derives from it.
    pub public_url: String,
    /// The cells the process exposes (the CEILING; grants can only
    /// restrict).
    pub cells: Vec<String>,
    /// The cells that also accept proposals (a subset of `cells`).
    pub write: Vec<String>,
    /// RFC 7591 dynamic registration (deprecated in the spec, off by
    /// default).
    pub allow_dcr: bool,
    /// The `search`/`fetch` wrappers for ChatGPT's connectors (P6).
    pub compat_chatgpt: bool,
    /// TEST HATCH: accept `http://127.0.0.1:<port>` as the public URL.
    /// Never set from the CLI.
    pub allow_insecure_public_url: bool,
    /// TEST HATCH: let the CIMD fetch reach 127.0.0.1. Never from the CLI.
    pub allow_loopback_cimd: bool,
}

impl HttpConfig {
    /// The canonical protected resource: `<public-url>/mcp`, no trailing
    /// slash. It is the audience of every token.
    #[must_use]
    pub fn resource(&self) -> String {
        format!("{}/mcp", self.public_url.trim_end_matches('/'))
    }

    /// The issuer: the public URL itself.
    #[must_use]
    pub fn issuer(&self) -> String {
        self.public_url.trim_end_matches('/').to_string()
    }

    /// The scopes this process offers: read always, propose only when a
    /// cell is writable.
    #[must_use]
    pub fn scopes(&self) -> Vec<&'static str> {
        let mut s = vec![SCOPE_READ];
        if !self.write.is_empty() {
            s.push(SCOPE_PROPOSE);
        }
        s
    }

    /// Validate the whole shape: loopback bind, sane public URL, write a
    /// subset of cells. `Err` carries the usage error for the CLI to print.
    ///
    /// # Errors
    /// A human-facing message for each rule broken.
    pub fn validate(&self) -> Result<(), String> {
        if !self.bind.ip().is_loopback() {
            return Err(format!(
                "`--http {}` is not a loopback address: engramma listens only on \
                 loopback and lets the tunnel (or a local proxy) own TLS and the \
                 public interface — the public side is `engramma serve`, later",
                self.bind
            ));
        }
        let https = self.public_url.starts_with("https://");
        let insecure_ok = self.allow_insecure_public_url
            && (self.public_url.starts_with("http://127.0.0.1")
                || self.public_url.starts_with("http://localhost"));
        if !https && !insecure_ok {
            return Err(format!(
                "`--public-url {}` must be https:// (without query or fragment): \
                 the tunnel terminates TLS{hint}",
                self.public_url,
                hint = if self.allow_insecure_public_url {
                    ""
                } else {
                    " (an http://127.0.0.1 URL is a library-only test exception)"
                }
            ));
        }
        if self.public_url.contains('?') || self.public_url.contains('#') {
            return Err(
                "`--public-url` carries a query or a fragment: it must be the bare \
                 origin (and path, if any) of the tunnel"
                    .into(),
            );
        }
        if self.public_url.trim_end_matches('/').len() <= "https://".len() {
            return Err("`--public-url` has no host".into());
        }
        for w in &self.write {
            if !self.cells.contains(w) {
                return Err(format!(
                    "`--write {w}` names a cell that is not in `--cells`"
                ));
            }
        }
        if self.cells.is_empty() {
            return Err("`--http` needs --cells: without them no cell would be exposed".into());
        }
        Ok(())
    }
}

/// The shared state of the HTTP server.
pub struct HttpState {
    /// The validated configuration.
    pub cfg: HttpConfig,
    /// oauth.db.
    pub store: OAuthStore,
}

impl HttpState {
    /// Open the store at `<root>/oauth.db` and bind it to a configuration.
    ///
    /// # Errors
    /// [`crate::oauth::StoreError`] when oauth.db cannot be opened or
    /// speaks a newer schema; the config's own usage errors come from
    /// [`HttpConfig::validate`], called here again as a net.
    pub fn open(root: &std::path::Path, cfg: HttpConfig) -> Result<Self, crate::oauth::StoreError> {
        cfg.validate().map_err(crate::oauth::StoreError::Db)?;
        let store = OAuthStore::open(&root.join("oauth.db"))?;
        Ok(Self { cfg, store })
    }
}

/// HTML-escape everything that will land in a page: client names come
/// from the outside world and their text is hostile.
// used by the /authorize pages from P3 on
#[expect(dead_code, reason = "the OAuth pages arrive with P3")]
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn json_response(doc: serde_json::Value) -> Response {
    let body = serde_json::to_string(&doc).unwrap_or_else(|_| "{}".into());
    let mut r = (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response();
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    r
}

/// The RFC 9728 Protected Resource Metadata document.
fn protected_resource_doc(state: &HttpState) -> serde_json::Value {
    serde_json::json!({
        "resource": state.cfg.resource(),
        "authorization_servers": [state.cfg.issuer()],
        "scopes_supported": state.cfg.scopes(),
        "bearer_methods_supported": ["header"],
    })
}

/// The RFC 8414 authorization server metadata document.
fn authorization_server_doc(state: &HttpState) -> serde_json::Value {
    let issuer = state.cfg.issuer();
    let mut doc = serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "revocation_endpoint": format!("{issuer}/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": state.cfg.scopes(),
    });
    if state.cfg.allow_dcr {
        doc["registration_endpoint"] = serde_json::json!(format!("{issuer}/register"));
    }
    doc
}

async fn protected_resource(State(state): State<Arc<HttpState>>) -> Response {
    json_response(protected_resource_doc(&state))
}

async fn authorization_server(State(state): State<Arc<HttpState>>) -> Response {
    json_response(authorization_server_doc(&state))
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html("<!doctype html><meta charset=\"utf-8\"><title>404</title>".to_string()),
    )
        .into_response()
}

/// The whole HTTP router. `/mcp` joins in P1; the OAuth flow in P3.
pub fn router(state: Arc<HttpState>) -> Router {
    // RFC 9728 puts the metadata at /.well-known/oauth-protected-resource
    // (+ /mcp); several clients also try the bare path — same document
    let issuer = state.cfg.issuer();
    let issuer_path = issuer
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    let mut r = Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .fallback(not_found);
    // RFC 8414 path insertion, when the issuer carries a path
    if let Some((_, path)) = issuer_path.split_once('/') {
        let path = path.trim_matches('/');
        r = r.route(
            &format!("/.well-known/oauth-authorization-server/{path}"),
            get(authorization_server),
        );
    }
    r.with_state(state)
}

/// Serve until the process stops. The shutdown wiring (SIGINT/SIGTERM →
/// rmcp's cancellation token) lives with the CLI command.
///
/// # Errors
/// [`std::io::Error`] when the bind or the serve fails.
pub async fn serve(state: Arc<HttpState>) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(state.cfg.bind).await?;
    axum::serve(listener, router(state)).await
}
