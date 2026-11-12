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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};

use crate::oauth::cimd::{self, CimdFetch};
use crate::oauth::mint;
use crate::oauth::net::{parse_redirect_uri, redirect_matches, redirect_uri_ok};
use crate::oauth::store::{
    AuthRequest, ClientRecord, CodeUse, OAuthStore, RefreshUse, RequestStatus, SCOPE_PROPOSE,
    SCOPE_READ,
};

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
    /// The MCP server behind `/mcp`, when the process exposes one (the
    /// CLI always does; the library's metadata tests may not).
    pub mcp: Option<crate::MemoryServer>,
    /// Legacy-protocol MCP sessions (2026-07-28 has none): session id →
    /// the grant that created it. Another grant's token on someone
    /// else's session answers 404, exactly like an unknown session.
    pub sessions: Mutex<HashMap<String, String>>,
    /// The global `/token` limiter: (window start, count). Behind a tunnel
    /// every caller looks like 127.0.0.1, so the ceiling is global — 60 a
    /// minute, then 429 with Retry-After. Forwarded headers are not
    /// consulted: they are not trustworthy.
    token_window: Mutex<(i64, usize)>,
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
        Ok(Self {
            cfg,
            store,
            mcp: None,
            sessions: Mutex::new(HashMap::new()),
            token_window: Mutex::new((0, 0)),
        })
    }

    /// [`HttpState::open`] plus the MCP server: the shape the CLI runs.
    ///
    /// # Errors
    /// The store's errors, or a cell that cannot be opened.
    pub fn open_with_mcp(root: &std::path::Path, cfg: HttpConfig) -> anyhow::Result<Self> {
        let compat = cfg.compat_chatgpt;
        let mut state = Self::open(root, cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mcp = crate::MemoryServer::open_http(root, &state.cfg.cells, &state.cfg.write, compat)?;
        state.mcp = Some(mcp);
        Ok(state)
    }

    fn now() -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp()
    }

    /// One tick of the global limiter: `false` when the window is full.
    fn token_rate_tick(&self) -> bool {
        const CAP: usize = 60;
        let now = Self::now();
        let Ok(mut w) = self.token_window.lock() else {
            return false; // a poisoned limiter closes the door
        };
        if now - w.0 >= 60 {
            w.0 = now;
            w.1 = 0;
        }
        if w.1 >= CAP {
            return false;
        }
        w.1 += 1;
        true
    }

    /// The client behind a `client_id`: the cache first, then a fresh CIMD
    /// fetch (the SSRF-fenced one), then nothing. Never DCR-fetches: a DCR
    /// client is only what `/register` minted.
    async fn client_of(&self, client_id: &str) -> Option<ClientRecord> {
        let now = Self::now();
        if let Some(cached) = self.store.client_get(client_id, now).ok().flatten() {
            return Some(cached);
        }
        // a CIMD URL is https:// (the loopback http escape is the library
        // test hatch only)
        let looks_like_url = client_id.starts_with("https://")
            || (self.cfg.allow_loopback_cimd && client_id.starts_with("http://127.0.0.1"));
        if !looks_like_url {
            return None;
        }
        let mode = CimdFetch {
            allow_loopback: self.cfg.allow_loopback_cimd,
        };
        let url = client_id.to_string();
        let fetched = tokio::task::spawn_blocking(move || cimd::fetch(&url, mode)).await;
        match fetched {
            Ok(Ok(f)) => {
                let cache_until = f.cache_until;
                if self
                    .store
                    .cimd_put(client_id, &f.record, cache_until)
                    .is_ok()
                {
                    return Some(f.record);
                }
                None
            }
            Ok(Err(e)) => {
                // the refusal is generic on the wire; the reason is an event
                self.store
                    .event(Self::now(), "client_rejected", &format!("{client_id}: {e}"));
                None
            }
            Err(_) => None,
        }
    }
}

/// HTML-escape everything that will land in a page: client names come
/// from the outside world and their text is hostile.
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

// ---------------------------------------------------------------------------
// P3: the authorization flow
// ---------------------------------------------------------------------------

/// One query string.
type Q = HashMap<String, String>;

/// The waiting page: hard headers on every page this phase serves.
fn page(title: &str, body: &str) -> Response {
    let mut r = Html(format!(
        "<!doctype html>\n<html lang=\"it\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{}</title></head><body><main>{body}</main></body></html>",
        esc(title)
    ))
    .into_response();
    let headers = r.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'"),
    );
    headers.insert(
        axum::http::header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    r
}

/// The ERROR page (not a redirect): used when the client_id or the
/// redirect_uri did not pass — answering with a redirect would BE the
/// open-redirect bug.
fn error_page(why: &str) -> Response {
    page(
        "Non riesco",
        &format!(
            "<h1>Non posso avviare il collegamento</h1><p>{}</p>\
             <p>Torna all'app da cui hai cliccato e riprova; se continua, \
             il client non è compatibile con questo server.</p>",
            esc(why)
        ),
    )
}

/// Percent-encode one query value.
fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Append parameters to a redirect URI (it may already carry a query).
fn redirect_with(redirect: &str, params: &[(&str, String)]) -> String {
    let sep = if redirect.contains('?') { '&' } else { '?' };
    let joined = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlenc(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{redirect}{sep}{joined}")
}

/// A 302 Found redirect (what OAuth flows speak).
fn found(to: &str) -> Response {
    let mut r = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        r.headers_mut().insert(axum::http::header::LOCATION, v);
    }
    r
}

/// The error redirect: always carries `iss` (RFC 9207), errors included,
/// and the client's `state` when it sent one.
fn error_redirect(
    redirect: &str,
    error: &str,
    description: &str,
    state: &Option<String>,
    issuer: &str,
) -> Response {
    let mut params = vec![
        ("error", error.to_string()),
        ("error_description", description.to_string()),
    ];
    if let Some(st) = state {
        params.push(("state", st.clone()));
    }
    params.push(("iss", issuer.to_string()));
    found(&redirect_with(redirect, &params))
}

/// The host of a URL, for the pages (and only for them).
fn host_of_url(url: &str) -> &str {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', ':', '?'])
        .next()
        .unwrap_or(url)
}

/// `GET /authorize`: the whole validation cascade of P3.
async fn authorize(State(state): State<Arc<HttpState>>, Query(q): Query<Q>) -> Response {
    let now = HttpState::now();
    // 1. BEFORE trusting the redirect_uri: the client must be known. A bad
    // client or a bad redirect answers with a PAGE, never a redirect —
    // that is the open-redirect defense.
    let Some(client_id) = q.get("client_id").cloned() else {
        return error_page("manca il client_id");
    };
    let Some(client) = state.client_of(&client_id).await else {
        return error_page(
            "il client non è noto a questo server (documento client irraggiungibile o non valido)",
        );
    };
    let Some(redirect) = q.get("redirect_uri").cloned() else {
        return error_page("manca il redirect_uri");
    };
    let registered = client
        .redirect_uris
        .iter()
        .any(|r| redirect_matches(r, &redirect));
    if !registered {
        state.store.event(
            now,
            "client_rejected",
            &format!("{client_id}: redirect_uri not registered"),
        );
        return error_page("il redirect_uri non è registrato da questo client");
    }

    // 2. everything else answers at the redirect, with `iss` and `state`
    let issuer = state.cfg.issuer();
    let client_state = q.get("state").cloned();
    let err = |error: &str, d: &str| error_redirect(&redirect, error, d, &client_state, &issuer);

    if q.get("response_type").map(String::as_str) != Some("code") {
        return err("unsupported_response_type", "only `code` is spoken here");
    }
    let Some(challenge) = q.get("code_challenge").cloned() else {
        return err(
            "invalid_request",
            "PKCE is required: bring a code_challenge (S256)",
        );
    };
    match q.get("code_challenge_method").map(String::as_str) {
        Some("S256") => {}
        _ => {
            return err(
                "invalid_request",
                "code_challenge_method must be S256 (`plain` is not accepted)",
            );
        }
    }
    let resource = match q.get("resource") {
        Some(r) if r == &state.cfg.resource() => Some(r.clone()),
        Some(_) => {
            return err(
                "invalid_target",
                "the resource must be this server's canonical one",
            );
        }
        None => {
            // RFC 8707 asks for it; we bind OUR resource and say so
            state
                .store
                .event(now, "authorize_without_resource", &client_id);
            Some(state.cfg.resource())
        }
    };
    // the scopes: known ones kept, everything else ignored (ChatGPT asks
    // for `openid email profile`; we do not speak OIDC and do not refuse)
    let offered = state.cfg.scopes();
    let mut scopes: Vec<String> = q
        .get("scope")
        .map(|s| {
            s.split(' ')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|s| offered.contains(&s.as_str()))
        .collect();
    // `memory:read` is the floor: the read tools are the point of the
    // connection, and the token answer says what was really granted
    if !scopes.iter().any(|s| s == SCOPE_READ) {
        scopes.push(SCOPE_READ.to_string());
    }

    // 3. the ceiling: at most 5 requests waiting, 10 minutes each
    match state.store.pending_count(now) {
        Ok(n) if n >= 5 => {
            state.store.event(now, "authorize_flood", &client_id);
            return page(
                "Troppe richieste",
                "<h1>Troppe richieste in attesa</h1>\
                 <p>ci sono già cinque collegamenti da decidere: approvale o rifiutale \
                 (`engramma oauth pending`) e riprova.</p>",
            );
        }
        Ok(_) => {}
        Err(_) => return error_page("oauth.db non è leggibile"),
    }

    // 4. record and send the browser to the waiting page
    let handle = mint::handle().unwrap_or_default();
    let code = mint::approval_code().unwrap_or_default();
    if handle.is_empty() || code.is_empty() {
        return error_page("il generatore casuale non risponde");
    }
    let req = AuthRequest {
        handle: handle.clone(),
        approval_code: code.clone(),
        client_id: client_id.clone(),
        client_name: client.name.clone(),
        redirect_uri: redirect.clone(),
        scopes,
        resource,
        code_challenge: challenge,
        state: client_state,
        offered_cells: state.cfg.cells.clone(),
        offered_write: state.cfg.write.clone(),
        status: RequestStatus::Pending,
        grant_id: None,
        created_at: now,
        expires_at: now + 600,
    };
    if state.store.request_insert(&req).is_err() {
        return error_page("oauth.db non è scrivibile");
    }
    state.store.event(
        now,
        "request_created",
        &format!("code={code} client={client_id}"),
    );
    axum::response::Redirect::to(&format!("/attesa/{handle}")).into_response()
}

/// `GET /attesa/<handle>`: the page the browser refreshes until the human
/// decides on the computer where engramma runs.
async fn attesa(State(state): State<Arc<HttpState>>, Path(handle): Path<String>) -> Response {
    let now = HttpState::now();
    let Ok(Some(req)) = state.store.request_by_handle(&handle) else {
        return page("Sconosciuto", "<h1>Richiesta sconosciuta</h1>");
    };
    let issuer = state.cfg.issuer();
    match req.status {
        RequestStatus::Pending if req.expires_at > now => {
            let client_host = host_of_url(&req.client_id);
            let redirect_host = host_of_url(&req.redirect_uri);
            let loopback_note = if parse_redirect_uri(&req.redirect_uri).is_some_and(|u| {
                u.scheme == "http" && (u.host == "127.0.0.1" || u.host == "localhost")
            }) {
                "<p class=\"avviso\">Attenzione: il client riceverà la risposta su un \
                 indirizzo locale del TUO computer (redirect http su loopback).</p>"
            } else {
                ""
            };
            let body = format!(
                "<meta http-equiv=\"refresh\" content=\"3\">\
<h1>Collegamento in attesa</h1>\
<p><strong>{}</strong> chiede di leggere la tua memoria.</p>\
<table>\
<tr><th>client</th><td><code>{}</code></td></tr>\
<tr><th>riceverà la risposta su</th><td><code>{}</code></td></tr>\
<tr><th>ambiti</th><td>{}</td></tr>\
</table>\
{loopback_note}\
<p>Se i nomi sopra sono quelli che ti aspetti, approva sul computer dove \
 gira Engramma:</p>\
<pre>engramma oauth approve {}</pre>\
<p class=\"meta\">Questa pagina si aggiorna da sola; il codice scade con la \
 richiesta (10 minuti).</p>",
                esc(req.client_name.as_deref().unwrap_or("Un client")),
                esc(client_host),
                esc(redirect_host),
                esc(&req.scopes.join(" ")),
                esc(&req.approval_code),
            );
            page("In attesa", &body)
        }
        RequestStatus::Pending => error_redirect(
            &req.redirect_uri,
            "access_denied",
            "la richiesta è scaduta",
            &req.state,
            &issuer,
        ),
        RequestStatus::Denied => error_redirect(
            &req.redirect_uri,
            "access_denied",
            "rifiutata sul computer dove gira engramma",
            &req.state,
            &issuer,
        ),
        RequestStatus::Approved => {
            // the code is minted HERE, the first time the page sees the
            // approval: its 60 s window starts when the client can
            // actually pick it up
            if state.store.code_already_minted(&handle).unwrap_or(false) {
                return page(
                    "Completato",
                    "<h1>Collegamento già completato</h1>\
                     <p>il codice d'autorizzazione è già stato emesso: torna all'app.</p>",
                );
            }
            match state.store.code_mint(&handle, &state.cfg.resource(), now) {
                Ok(Some((code, _record, _expires))) => {
                    let mut params = vec![("code", code)];
                    if let Some(st) = &req.state {
                        params.push(("state", st.clone()));
                    }
                    params.push(("iss", issuer));
                    found(&redirect_with(&req.redirect_uri, &params))
                }
                _ => error_redirect(
                    &req.redirect_uri,
                    "access_denied",
                    "la richiesta non è più valida",
                    &req.state,
                    &issuer,
                ),
            }
        }
    }
}

/// A token-endpoint JSON answer (always `Cache-Control: no-store`).
fn token_json(v: serde_json::Value) -> Response {
    let mut r = (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()),
    )
        .into_response();
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    r
}

/// A token-endpoint error (RFC 6749 §5.2: 400 + `error`).
fn token_error(error: &str, description: &str) -> Response {
    let mut r = token_json(serde_json::json!({ "error": error, "error_description": description }));
    *r.status_mut() = StatusCode::BAD_REQUEST;
    r
}

/// `POST /token`: authorization_code (with PKCE) and refresh_token.
async fn token(State(state): State<Arc<HttpState>>, Form(f): Form<Q>) -> Response {
    if !state.token_rate_tick() {
        let mut r = token_error("slow_down", "troppo: riprova tra poco");
        *r.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        r.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            HeaderValue::from_static("60"),
        );
        return r;
    }
    let now = HttpState::now();
    match f.get("grant_type").map(String::as_str) {
        Some("authorization_code") => token_code(&state, &f, now).await,
        Some("refresh_token") => token_refresh(&state, &f, now),
        Some(_) => token_error(
            "unsupported_grant_type",
            "authorization_code o refresh_token",
        ),
        None => token_error("invalid_request", "manca grant_type"),
    }
}

async fn token_code(state: &HttpState, f: &Q, now: i64) -> Response {
    let Some(code) = f.get("code") else {
        return token_error("invalid_request", "manca code");
    };
    let Some(verifier) = f.get("code_verifier") else {
        return token_error("invalid_request", "manca code_verifier (PKCE)");
    };
    let Some(client_id) = f.get("client_id") else {
        return token_error("invalid_request", "manca client_id");
    };
    let Some(redirect) = f.get("redirect_uri") else {
        return token_error("invalid_request", "manca redirect_uri");
    };
    match state.store.code_consume(code, now) {
        Err(_) => token_error("oauth.db", "non leggibile"),
        Ok(Err(CodeUse::Unknown)) => token_error("invalid_grant", "codice sconosciuto"),
        Ok(Err(CodeUse::Expired)) => token_error("invalid_grant", "codice scaduto"),
        Ok(Err(CodeUse::Reused { family })) => {
            // a code answers twice: assume theft, kill the whole chain
            let _ = state.store.revoke_family(&family, now);
            state
                .store
                .event(now, "code_reused", &format!("family={family}"));
            token_error(
                "invalid_grant",
                "il codice era già stato usato: tutto ciò che ne era nato è revocato",
            )
        }
        Ok(Ok(rec)) => {
            if rec.client_id != *client_id {
                token_error("invalid_grant", "il codice è di un altro client");
            }
            if !redirect_matches(&rec.redirect_uri, redirect) {
                token_error("invalid_grant", "redirect_uri non corrispondente");
            }
            if let Some(want) = f.get("resource")
                && Some(want.as_str()) != rec.resource.as_deref()
            {
                return token_error("invalid_target", "resource non corrispondente");
            }
            if !mint::pkce_verify(&rec.code_challenge, verifier) {
                return token_error("invalid_grant", "PKCE non verificato");
            }
            let Some(grant) = state.store.grant(&rec.grant_id, now).ok().flatten() else {
                return token_error("invalid_grant", "il grant non vive più");
            };
            // the grant can only NARROW what the code asked
            let scopes: Vec<String> = rec
                .scopes
                .iter()
                .filter(|s| grant.scopes.contains(s))
                .cloned()
                .collect();
            let audience = state.cfg.resource();
            let (access, access_exp) =
                match state
                    .store
                    .access_issue(&grant, &scopes, &audience, &rec.family, now)
                {
                    Ok(v) => v,
                    Err(_) => return token_error("server_error", "oauth.db"),
                };
            let (refresh, _refresh_exp) =
                match state
                    .store
                    .refresh_issue(&grant, &scopes, &audience, &rec.family, now)
                {
                    Ok(v) => v,
                    Err(_) => return token_error("server_error", "oauth.db"),
                };
            state.store.event(
                now,
                "token_issued",
                &format!(
                    "grant={} client={}",
                    grant.id,
                    host_of_url(&grant.client_id)
                ),
            );
            token_json(serde_json::json!({
                "access_token": access,
                "token_type": "Bearer",
                "expires_in": access_exp - now,
                "refresh_token": refresh,
                "scope": scopes.join(" "),
            }))
        }
    }
}

fn token_refresh(state: &HttpState, f: &Q, now: i64) -> Response {
    let Some(refresh) = f.get("refresh_token") else {
        return token_error("invalid_request", "manca refresh_token");
    };
    let audience = state.cfg.resource();
    let wanted: Vec<String> = f
        .get("scope")
        .map(|s| {
            s.split(' ')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match state.store.refresh_rotate(refresh, &wanted, &audience, now) {
        Err(_) => token_error("server_error", "oauth.db"),
        Ok(Err(RefreshUse::Reused)) => token_error(
            "invalid_grant",
            "il refresh era già stato ruotato: il grant è revocato, ricollega il client",
        ),
        Ok(Err(RefreshUse::Unknown)) => token_error("invalid_grant", "refresh sconosciuto"),
        Ok(Err(RefreshUse::Expired)) => {
            token_error("invalid_grant", "refresh scaduto: ricollega il client")
        }
        Ok(Ok(rot)) => token_json(serde_json::json!({
            "access_token": rot.access,
            "token_type": "Bearer",
            "expires_in": rot.access_exp - now,
            "refresh_token": rot.refresh,
            "scope": rot.scopes.join(" "),
        })),
    }
}

/// `POST /revoke` (RFC 7009): 200 even for an unknown token.
async fn revoke(State(state): State<Arc<HttpState>>, Form(f): Form<Q>) -> Response {
    if let Some(token) = f.get("token") {
        let _ = state.store.token_revoke(token, HttpState::now());
    }
    let mut r = StatusCode::OK.into_response();
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    r
}

/// `POST /register` (RFC 7591) — only with `--allow-dcr`.
async fn register(
    State(state): State<Arc<HttpState>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !state.cfg.allow_dcr {
        return (
            StatusCode::NOT_FOUND,
            "il server non accetta registrazioni dinamiche",
        )
            .into_response();
    }
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !ct.starts_with("application/json") {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "registration wants application/json",
        )
            .into_response();
    }
    if body.len() > 16 * 1024 {
        return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
    }
    let doc: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "unreadable JSON").into_response(),
    };
    match doc
        .get("token_endpoint_auth_method")
        .and_then(|v| v.as_str())
    {
        None | Some("none") => {}
        Some(_other) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("token_endpoint_auth_method `{_other}` is not supported: only `none` (public client with PKCE)"),
            )
                .into_response();
        }
    }
    let uris: Vec<String> = doc
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if uris.is_empty() || uris.iter().any(|u| !redirect_uri_ok(u)) {
        return (
            StatusCode::BAD_REQUEST,
            "redirect_uris: https anywhere, or http on a loopback host",
        )
            .into_response();
    }
    let now = HttpState::now();
    // the flood cap: 20 registered-and-never-used clients
    match state.store.dcr_unused_count() {
        Ok(n) if n >= 20 => {
            let mut r = (
                StatusCode::TOO_MANY_REQUESTS,
                "too many unused registrations",
            )
                .into_response();
            r.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                HeaderValue::from_static("3600"),
            );
            return r;
        }
        Ok(_) => {}
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "oauth.db").into_response(),
    }
    let client_id = mint::opaque_token("eng_client_").unwrap_or_default();
    let rec = ClientRecord {
        client_id: client_id.clone(),
        kind: crate::oauth::store::ClientKind::Dcr,
        redirect_uris: uris.clone(),
        name: doc
            .get("client_name")
            .and_then(|v| v.as_str())
            .map(|n| n.chars().take(80).collect::<String>()),
        created_at: now,
    };
    if state.store.dcr_put(&rec).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "oauth.db").into_response();
    }
    state
        .store
        .event(now, "client_registered", &format!("{client_id} (dcr)"));
    let mut r = (
        StatusCode::CREATED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&serde_json::json!({
            "client_id": client_id,
            "client_id_issued_at": now,
            "redirect_uris": uris,
            "token_endpoint_auth_method": "none",
        }))
        .unwrap_or_default(),
    )
        .into_response();
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    r
}

/// The public authority (host, or host:port) of the public URL.
fn public_authority(cfg: &HttpConfig) -> String {
    cfg.public_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// The whole HTTP router: the OAuth endpoints, and `/mcp` behind the
/// bearer gate when the process exposes the memory.
pub fn router(state: Arc<HttpState>) -> Router {
    router_inner(state, None)
}

fn router_inner(state: Arc<HttpState>, ct: Option<tokio_util::sync::CancellationToken>) -> Router {
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
        .route("/authorize", get(authorize))
        .route("/attesa/{handle}", get(attesa))
        .route("/token", post(token))
        .route("/revoke", post(revoke))
        .route("/register", post(register))
        .fallback(not_found);
    // RFC 8414 path insertion, when the issuer carries a path
    if let Some((_, path)) = issuer_path.split_once('/') {
        let path = path.trim_matches('/');
        r = r.route(
            &format!("/.well-known/oauth-authorization-server/{path}"),
            get(authorization_server),
        );
    }

    // /mcp: rmcp's streamable HTTP service behind the bearer gate
    if let Some(mcp) = state.mcp.clone() {
        let mut hosts = vec![
            public_authority(&state.cfg),
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ];
        hosts.push(format!("127.0.0.1:{}", state.cfg.bind.port()));
        // ChatGPT speaks 2025-11-25: the legacy session mode stays on
        let mut config = rmcp::transport::StreamableHttpServerConfig::default()
            .with_legacy_session_mode(true)
            .with_json_response(true)
            .with_allowed_hosts(hosts)
            .with_max_request_body_bytes(1024 * 1024)
            .enforce_origin_validation();
        if let Some(ct) = ct {
            config.cancellation_token = ct;
        }
        let service = rmcp::transport::StreamableHttpService::new(
            move || Ok(mcp.clone()),
            std::sync::Arc::new(
                rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
            ),
            config,
        );
        let mcp_router = Router::new().route_service("/mcp", service).layer(
            axum::middleware::from_fn_with_state(Arc::clone(&state), mcp_gate),
        );
        r = r.merge(mcp_router);
    }

    // the OAuth endpoints a SERVER calls never carry an Origin; the Host
    // fence wraps everything, tunnel-side DNS rebinding first
    r.route_layer(axum::middleware::from_fn(no_origin_guard))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            host_guard,
        ))
        .with_state(state)
}

/// Serve until the token cancels: SIGINT/SIGTERM cancel it (the CLI wires
/// the signals), rmcp's sessions die with it, then the listener drains.
///
/// # Errors
/// [`std::io::Error`] when the bind or the serve fails.
pub async fn serve_until(
    state: Arc<HttpState>,
    ct: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(state.cfg.bind).await?;
    let app = router_with_cancellation(Arc::clone(&state), ct.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { ct.cancelled().await })
        .await
}

/// [`router`] with the cancellation token threaded into the rmcp service.
pub fn router_with_cancellation(
    state: Arc<HttpState>,
    ct: tokio_util::sync::CancellationToken,
) -> Router {
    router_inner(state, Some(ct))
}

// ---------------------------------------------------------------------------
// P1/P4: /mcp as the resource server
// ---------------------------------------------------------------------------

/// The loopback names the Host check (and rmcp's) accept as-is.
fn is_loopback_host_name(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// The Host middleware of the whole surface: only the public authority
/// and loopback names pass (with any port on loopback — the local port is
/// nobody's secret). Everything else is a 403 with no useful body, the
/// DNS-rebinding fence for every endpoint at once.
async fn host_guard(
    State(state): State<Arc<HttpState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let public = public_authority(&state.cfg);
    let ok = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h == public || is_loopback_host_name(h.split(':').next().unwrap_or_default())
        });
    if !ok {
        return (StatusCode::FORBIDDEN, "").into_response();
    }
    next.run(req).await
}

/// Remote MCP clients call from SERVERS, not browsers: a request that
/// carries an `Origin` is not one of them. Rejected on the nose, exactly
/// what `enforce_origin_validation()` means for the OAuth endpoints the
/// spec names.
async fn no_origin_guard(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    if req.headers().contains_key(axum::http::header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "Origin is not accepted here").into_response();
    }
    next.run(req).await
}

/// One line of the access log: time, grant (never the token), MCP method,
/// tool, HTTP status, duration. Never note text, never arguments.
fn access_log(
    grant: Option<&str>,
    method: Option<&str>,
    tool: Option<&str>,
    status: u16,
    dur_ms: u128,
) {
    eprintln!(
        "mcp-http ts={} grant={} method={} tool={} status={status} dur_ms={dur_ms}",
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        grant.unwrap_or("-"),
        method.unwrap_or("-"),
        tool.unwrap_or("-"),
    );
}

/// The bearer gate in front of the rmcp service: resolves the token into
/// a grant (or refuses), binds legacy sessions to the grant that opened
/// them, and writes the access log.
async fn mcp_gate(
    State(state): State<Arc<HttpState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let started = std::time::Instant::now();
    let resource = state.cfg.resource();
    let scopes = state.cfg.scopes().join(" ");
    let challenge = |error: Option<&str>| {
        let mut v = format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"{scopes}\"",
            state.cfg.issuer()
        );
        if let Some(e) = error {
            v.push_str(&format!(", error=\"{e}\""));
        }
        v
    };
    // a token in the query string is never accepted (the spec says 400)
    if req
        .uri()
        .query()
        .is_some_and(|q| q.contains("access_token="))
    {
        return (
            StatusCode::BAD_REQUEST,
            "the token does not travel in the query string",
        )
            .into_response();
    }
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = bearer else {
        let mut r = (StatusCode::UNAUTHORIZED, "").into_response();
        if let Ok(hv) = HeaderValue::from_str(&challenge(None)) {
            r.headers_mut()
                .insert(axum::http::header::WWW_AUTHENTICATE, hv);
        }
        return r;
    };
    // the validation runs OFF the async runtime's threads (SQLite, however
    // brief, is blocking)
    let check = {
        let store = state.store.clone();
        let token = token.to_string();
        let resource = resource.clone();
        tokio::task::spawn_blocking(move || store.access_check(&token, &resource, HttpState::now()))
            .await
    };
    let grant = match check {
        Ok(Ok(crate::oauth::TokenCheck::Valid { grant, scopes })) => Some((grant, scopes)),
        Ok(Ok(_)) => None,
        _ => None,
    };
    let Some((grant, token_scopes)) = grant else {
        let mut r = (StatusCode::UNAUTHORIZED, "").into_response();
        if let Ok(hv) = HeaderValue::from_str(&challenge(Some("invalid_token"))) {
            r.headers_mut()
                .insert(axum::http::header::WWW_AUTHENTICATE, hv);
        }
        return r;
    };
    if !token_scopes.iter().any(|s| s == SCOPE_READ) {
        // a token without memory:read has nothing to do here
        let mut r = (StatusCode::UNAUTHORIZED, "").into_response();
        if let Ok(hv) = HeaderValue::from_str(&challenge(Some("insufficient_scope"))) {
            r.headers_mut()
                .insert(axum::http::header::WWW_AUTHENTICATE, hv);
        }
        return r;
    }
    // the grant rides to the handler through the request extensions —
    // rmcp carries the HTTP Parts into the JSON-RPC context
    let mut req = req;
    req.extensions_mut().insert(crate::RequestGrant {
        id: grant.id.clone(),
        label: grant.label.clone(),
        cells: grant.cells.clone(),
        scopes: token_scopes.clone(),
    });

    // legacy sessions belong to the grant that created them: another
    // grant's session id answers 404, like an unknown one
    if let Some(sess) = req
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    {
        let bound = state
            .sessions
            .lock()
            .ok()
            .and_then(|m| m.get(&sess).cloned());
        match bound {
            Some(owner) if owner != grant.id => {
                return (StatusCode::NOT_FOUND, "session not found").into_response();
            }
            Some(_) => {}
            None => {
                // a session the middleware never saw created: unknown
                return (StatusCode::NOT_FOUND, "session not found").into_response();
            }
        }
    }

    // the access log needs the MCP method and tool: read the (bounded)
    // body, then put it back — same head, same extensions
    let (parts, body) = req.into_parts();
    let mut method_desc: Option<(Option<String>, Option<String>)> = None;
    let req = if parts.method == axum::http::Method::POST {
        match axum::body::to_bytes(body, 1024 * 1024 + 1).await {
            Ok(bytes) => {
                method_desc = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .map(|v| {
                        (
                            v.get("method").and_then(|m| m.as_str()).map(str::to_string),
                            v.get("params")
                                .and_then(|p| p.get("name"))
                                .and_then(|n| n.as_str())
                                .map(str::to_string),
                        )
                    });
                axum::http::Request::from_parts(parts, axum::body::Body::from(bytes))
            }
            Err(_) => axum::http::Request::from_parts(parts, axum::body::Body::empty()),
        }
    } else {
        axum::http::Request::from_parts(parts, body)
    };

    let response = next.run(req).await;
    // a session born in this response belongs to the calling grant
    if let Some(sess) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        && let Ok(mut m) = state.sessions.lock()
    {
        m.insert(sess, grant.id.clone());
    }
    access_log(
        Some(&grant.id),
        method_desc.as_ref().and_then(|(m, _)| m.as_deref()),
        method_desc.as_ref().and_then(|(_, t)| t.as_deref()),
        response.status().as_u16(),
        started.elapsed().as_millis(),
    );
    response
}
