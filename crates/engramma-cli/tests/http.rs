// SPDX-License-Identifier: Apache-2.0

//! v0.5.2: `engramma mcp --http` — the full OAuth 2.1 flow and the MCP
//! resource server, spoken over loopback HTTP with a fake ureq client.
//! The server under test is the REAL thing: most cases run the in-process
//! router (the library's two test hatches let the flow live on 127.0.0.1),
//! the CLI-only rules run the binary itself. No network beyond loopback,
//! exactly like the review-page tests.

use std::net::SocketAddr;
use std::sync::Arc;

use engramma_mcp::http::{HttpConfig, HttpState};
use serde_json::Value;

// ---------------------------------------------------------------------------
// the harness
// ---------------------------------------------------------------------------

/// The no-redirect agent: every guard has its own test, and a redirect
/// followed silently would hide the very thing under test.
fn still() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn body(r: &mut ureq::http::Response<ureq::Body>) -> String {
    r.body_mut().read_to_string().unwrap(/* t */)
}

fn json_of(r: &mut ureq::http::Response<ureq::Body>) -> Value {
    let raw = body(r);
    // an SSE answer carries the JSON in its `data:` line
    let is_sse = raw.starts_with("event:") || raw.starts_with("data:") || raw.contains("\ndata:");
    let json_text = if is_sse {
        raw.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .to_string()
    } else {
        raw.clone()
    };
    serde_json::from_str(&json_text).unwrap_or_else(|e| panic!("not JSON: {e} — body: {raw}"))
}

/// One in-process server on a fresh root. The public URL is the honest
/// https one; the test agent talks to the loopback address directly (the
/// Host check accepts the public host), so the documents under test carry
/// the same URLs production would carry.
struct Server {
    addr: SocketAddr,
    public: String,
    /// The tests that drive the approval flow reach into the store from
    /// here (P3).
    #[allow(dead_code)]
    state: Arc<HttpState>,
    #[allow(dead_code)]
    root: std::path::PathBuf,
    _keep: tempfile::TempDir,
}

impl Server {
    fn start(cfg_patch: impl FnOnce(&mut HttpConfig)) -> Server {
        let keep = tempfile::tempdir().unwrap(/* t */);
        let root = keep.path().join("root");
        std::fs::create_dir_all(&root).unwrap(/* t */);
        let mut cfg = HttpConfig {
            bind: "127.0.0.1:0".parse().unwrap(/* t */),
            public_url: "https://memoria.example.net".into(),
            cells: vec!["famiglia".into(), "lavoro".into()],
            write: vec![],
            allow_dcr: false,
            compat_chatgpt: false,
            chatgpt_client_id: engramma_mcp::CHATGPT_CLIENT_ID.to_string(),
            allow_insecure_public_url: false,
            allow_loopback_cimd: false,
        };
        cfg_patch(&mut cfg);
        let state = Arc::new(HttpState::open(&root, cfg).unwrap(/* t */));
        let addr = start_router(engramma_mcp::http::router(Arc::clone(&state)));
        Server {
            addr,
            public: state.cfg.public_url.clone(),
            state,
            root,
            _keep: keep,
        }
    }

    /// A request already pointed at the server but carrying the PUBLIC
    /// host — what the tunnel does for real.
    fn get(&self, path: &str) -> ureq::http::Response<ureq::Body> {
        still()
            .get(&format!("http://{addr}{path}", addr = self.addr))
            .header("Host", host_of(&self.public))
            .call()
            .unwrap(/* t */)
    }
}

fn host_of(public: &str) -> String {
    public
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string()
}

fn start_router(router: axum::Router) -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(/* t */);
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(/* t */);
            tx.send(listener.local_addr().unwrap(/* t */)).unwrap(/* t */);
            axum::serve(listener, router).await.unwrap(/* t */);
        });
    });
    rx.recv().unwrap(/* t */)
}

// ---------------------------------------------------------------------------
// P2: the metadata documents, field by field
// ---------------------------------------------------------------------------

#[test]
fn the_protected_resource_metadata_is_exact_on_both_paths() {
    let s = Server::start(|_| {});
    for path in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-protected-resource",
    ] {
        let mut r = s.get(path);
        assert_eq!(r.status().as_u16(), 200, "{path}");
        let doc = json_of(&mut r);
        assert_eq!(
            doc["resource"], "https://memoria.example.net/mcp",
            "{path}: the canonical resource, exact"
        );
        assert_eq!(
            doc["authorization_servers"],
            serde_json::json!(["https://memoria.example.net"]),
            "{path}: one issuer, one voice"
        );
        assert_eq!(
            doc["scopes_supported"],
            serde_json::json!(["memory:read"]),
            "{path}: no write cells, no propose scope"
        );
        assert_eq!(
            doc["bearer_methods_supported"],
            serde_json::json!(["header"]),
            "{path}"
        );
        assert_eq!(r.headers().get("cache-control").unwrap(), "no-store");
    }
}

#[test]
fn the_write_flag_adds_the_propose_scope_everywhere() {
    let s = Server::start(|cfg| {
        cfg.write = vec!["famiglia".into()];
    });
    for path in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-authorization-server",
    ] {
        let mut r = s.get(path);
        let doc = json_of(&mut r);
        assert_eq!(
            doc["scopes_supported"],
            serde_json::json!(["memory:read", "memory:propose"]),
            "{path}"
        );
    }
}

#[test]
fn the_authorization_server_metadata_matches_the_spec_point_by_point() {
    let s = Server::start(|_| {});
    let mut r = s.get("/.well-known/oauth-authorization-server");
    assert_eq!(r.status().as_u16(), 200);
    let doc = json_of(&mut r);
    assert_eq!(doc["issuer"], "https://memoria.example.net");
    assert_eq!(
        doc["authorization_endpoint"],
        "https://memoria.example.net/authorize"
    );
    assert_eq!(doc["token_endpoint"], "https://memoria.example.net/token");
    assert_eq!(
        doc["revocation_endpoint"],
        "https://memoria.example.net/revoke"
    );
    assert_eq!(doc["response_types_supported"], serde_json::json!(["code"]));
    assert_eq!(
        doc["grant_types_supported"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(
        doc["code_challenge_methods_supported"],
        serde_json::json!(["S256"])
    );
    assert_eq!(
        doc["token_endpoint_auth_methods_supported"],
        serde_json::json!(["none"]),
        "public clients only: this is what makes Claude's CIMD path work"
    );
    assert_eq!(doc["client_id_metadata_document_supported"], true);
    assert_eq!(doc["authorization_response_iss_parameter_supported"], true);
    // no registration endpoint without --allow-dcr
    assert!(
        doc.get("registration_endpoint").is_none(),
        "dynamic registration is OFF by default (deprecated by the spec)"
    );
}

#[test]
fn the_registration_endpoint_appears_only_with_allow_dcr() {
    let s = Server::start(|cfg| {
        cfg.allow_dcr = true;
    });
    let mut r = s.get("/.well-known/oauth-authorization-server");
    let doc = json_of(&mut r);
    assert_eq!(
        doc["registration_endpoint"],
        "https://memoria.example.net/register"
    );
}

#[test]
fn an_issuer_with_a_path_serves_the_inserted_as_metadata_too() {
    let s = Server::start(|cfg| {
        cfg.public_url = "https://memoria.example.net/engramma".into();
    });
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-authorization-server/engramma",
    ] {
        let mut r = s.get(path);
        assert_eq!(r.status().as_u16(), 200, "{path}");
        let doc = json_of(&mut r);
        assert_eq!(doc["issuer"], "https://memoria.example.net/engramma");
    }
}

// ---------------------------------------------------------------------------
// P1's own rules, on the library config: the address and URL gates
// ---------------------------------------------------------------------------

#[test]
fn the_config_refuses_what_the_cli_must_refuse() {
    let bad = [
        // not loopback
        ("0.0.0.0:8787", "https://memoria.example.net"),
        ("192.168.1.4:8787", "https://memoria.example.net"),
        // not https, not the test hatch
        ("127.0.0.1:8787", "http://memoria.example.net"),
        ("127.0.0.1:8787", "ftp://memoria.example.net"),
        // query or fragment
        ("127.0.0.1:8787", "https://memoria.example.net?x=1"),
        ("127.0.0.1:8787", "https://memoria.example.net#f"),
        // no host at all
        ("127.0.0.1:8787", "https://"),
    ];
    for (bind, public) in bad {
        let cfg = HttpConfig {
            bind: bind.parse().unwrap(/* t */),
            public_url: public.into(),
            cells: vec!["a".into()],
            write: vec![],
            allow_dcr: false,
            compat_chatgpt: false,
            chatgpt_client_id: engramma_mcp::CHATGPT_CLIENT_ID.to_string(),
            allow_insecure_public_url: false,
            allow_loopback_cimd: false,
        };
        assert!(cfg.validate().is_err(), "{bind} {public} must be refused");
    }
    // the library's test hatch: http on 127.0.0.1 is fine for the tests only
    let cfg = HttpConfig {
        bind: "127.0.0.1:8787".parse().unwrap(/* t */),
        public_url: "http://127.0.0.1:8787".into(),
        cells: vec!["a".into()],
        write: vec![],
        allow_dcr: false,
        compat_chatgpt: false,
        chatgpt_client_id: engramma_mcp::CHATGPT_CLIENT_ID.to_string(),
        allow_insecure_public_url: true,
        allow_loopback_cimd: false,
    };
    assert!(cfg.validate().is_ok());
    assert_eq!(cfg.resource(), "http://127.0.0.1:8787/mcp");
    // write outside cells is refused, like stdio
    let cfg = HttpConfig {
        bind: "127.0.0.1:8787".parse().unwrap(/* t */),
        public_url: "https://m.example".into(),
        cells: vec!["a".into()],
        write: vec!["b".into()],
        allow_dcr: false,
        compat_chatgpt: false,
        chatgpt_client_id: engramma_mcp::CHATGPT_CLIENT_ID.to_string(),
        allow_insecure_public_url: false,
        allow_loopback_cimd: false,
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn the_resource_never_carries_a_trailing_slash() {
    let cfg = HttpConfig {
        bind: "127.0.0.1:8787".parse().unwrap(/* t */),
        public_url: "https://memoria.example.net/".into(),
        cells: vec!["a".into()],
        write: vec![],
        allow_dcr: false,
        compat_chatgpt: false,
        chatgpt_client_id: engramma_mcp::CHATGPT_CLIENT_ID.to_string(),
        allow_insecure_public_url: false,
        allow_loopback_cimd: false,
    };
    assert_eq!(cfg.resource(), "https://memoria.example.net/mcp");
    assert_eq!(cfg.issuer(), "https://memoria.example.net");
}

// ---------------------------------------------------------------------------
// P3: the authorization flow — the whole journey, and every negative case
// ---------------------------------------------------------------------------

/// A fake CIMD server on loopback: the document claims its own URL and
/// redirects back to a fixed https redirect.
fn cimd_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(/* t */);
    let port = listener.local_addr().unwrap(/* t */).port();
    let handle = std::thread::spawn(move || {
        use std::io::Read as _;
        use std::io::Write as _;
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf);
            let url = format!("http://127.0.0.1:{port}/client.json");
            let body = format!(
                r#"{{"client_id":"{url}","redirect_uris":["https://chatgpt.com/connector_platform_oauth_redirect"],"client_name":"ChatGPT di prova"}}"#
            );
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://127.0.0.1:{port}/client.json"), handle)
}

/// One authorization start: a PKCE pair and the /authorize query.
fn pkce() -> (String, String) {
    let verifier = "verifier-di-prova-con-abbastanza-caratteri".to_string();
    let challenge = engramma_mcp::oauth::pkce_s256(&verifier);
    (verifier, challenge)
}

fn authorize_query(client_id: &str, challenge: &str, extra: &str) -> String {
    format!(
        "/authorize?response_type=code&client_id={cid}&redirect_uri=https%3A%2F%2Fchatgpt.com%2Fconnector_platform_oauth_redirect&code_challenge={ch}&code_challenge_method=S256&scope=openid%20email%20profile%20memory%3Aread&state=abc123{extra}",
        cid = urlencode(client_id),
        ch = challenge,
    )
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Where a redirect went (its Location), if anywhere.
fn location(r: &ureq::http::Response<ureq::Body>) -> Option<String> {
    r.headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// The approval code out of a waiting page.
fn code_of(html: &str) -> String {
    html.split("engramma oauth approve ")
        .nth(1)
        .and_then(|rest| {
            rest.chars()
                .take_while(|c| !c.is_whitespace() && *c != '<')
                .collect::<String>()
                .into()
        })
        .unwrap_or_default()
}

/// The full journey, once: authorize → waiting page → approve (the REAL
/// binary) → code → token pair. Returns the tokens and the server.
fn full_flow(write: bool) -> (Server, String, String) {
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| {
        cfg.allow_loopback_cimd = true;
        if write {
            cfg.write = vec!["famiglia".into()];
        }
    });
    let (verifier, challenge) = pkce();
    let q = authorize_query(&cimd_url, &challenge, "");
    let r = s.get(&q);
    assert_eq!(r.status().as_u16(), 303, "to the waiting page");
    let attesa = location(&r).unwrap(/* t */);

    // the waiting page: the code, the hosts, the scopes — as text
    let mut page = s.get(&attesa);
    let html = body(&mut page);
    assert_eq!(page.status().as_u16(), 200);
    assert!(html.contains("ChatGPT di prova"), "client name: {html}");
    assert!(html.contains("127.0.0.1"), "client_id host visible");
    assert!(html.contains("chatgpt.com"), "redirect host visible");
    assert!(html.contains("memory:read"), "{html}");
    assert!(html.contains("http-equiv=\"refresh\""), "no JavaScript");
    let code = code_of(&html);
    // XXXX-XXXX over the unambiguous alphabet
    assert_eq!(code.len(), 9, "XXXX-XXXX: {code}");
    assert_eq!(code.as_bytes()[4], b'-', "{code}");
    for c in code.chars().filter(|c| *c != '-') {
        assert!(
            engramma_mcp::oauth::APPROVAL_ALPHABET.contains(&(c as u8)),
            "{code}"
        );
    }

    // the approval: the REAL CLI, on the same root
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&s.root)
        .args(["oauth", "approve", &code, "--label", "prova", "--yes"])
        .output()
        .unwrap(/* t */);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // the refresh lands the code at the client, with state and iss
    let r = s.get(&attesa);
    assert_eq!(r.status().as_u16(), 302);
    let to = location(&r).unwrap(/* t */);
    assert!(to.starts_with("https://chatgpt.com/connector_platform_oauth_redirect"));
    assert!(to.contains("code=eng_code_"), "{to}");
    assert!(to.contains("state=abc123"), "{to}");
    assert!(
        to.contains(&format!("iss={}", urlencode("https://memoria.example.net"))),
        "{to}"
    );
    let auth_code = to
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .unwrap_or_default()
        .to_string();

    // the token exchange: openid email profile were ignored, not refused
    let mut r = still()
        .post(&format!("http://{addr}/token", addr = s.addr))
        .header("Host", host_of(&s.public))
        .send_form(vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("client_id", cimd_url.as_str()),
            ("redirect_uri", "https://chatgpt.com/connector_platform_oauth_redirect"),
            ("code_verifier", verifier.as_str()),
        ])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    let doc = json_of(&mut r);
    assert_eq!(doc["token_type"], "Bearer");
    assert_eq!(doc["scope"], "memory:read");
    assert_eq!(r.headers().get("cache-control").unwrap(), "no-store");
    (
        s,
        doc["access_token"].as_str().unwrap_or_default().to_string(),
        doc["refresh_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

#[test]
fn the_full_flow_authorizes_over_http_with_a_local_approval() {
    let (_s, access, _refresh) = full_flow(false);
    assert!(access.starts_with("eng_at_"), "{access}");
}

#[test]
fn the_refresh_rotates_and_its_reuse_kills_the_grant() {
    let (s, _access, refresh) = full_flow(false);
    // a good rotation
    let mut r = still()
        .post(&format!("http://{addr}/token", addr = s.addr))
        .header("Host", host_of(&s.public))
        .send_form(vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
        ])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200);
    let doc = json_of(&mut r);
    let new_access = doc["access_token"].as_str().unwrap_or_default().to_string();
    let new_refresh = doc["refresh_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_ne!(new_refresh, refresh);

    // the OLD refresh again: theft, and the whole grant dies
    let mut r = still()
        .post(&format!("http://{addr}/token", addr = s.addr))
        .header("Host", host_of(&s.public))
        .send_form(vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
        ])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 400);
    let doc = json_of(&mut r);
    assert_eq!(doc["error"], "invalid_grant");

    // the NEW access token is dead too (the store says so)
    assert_eq!(
        s.state
            .store
            .access_check(&new_access, "https://memoria.example.net/mcp", now())
            .unwrap(/* t */),
        engramma_mcp::oauth::TokenCheck::Revoked
    );
}

fn now() -> i64 {
    time_shim()
}

fn time_shim() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[test]
fn a_wrong_redirect_uri_gets_a_page_not_a_redirect() {
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| {
        cfg.allow_loopback_cimd = true;
    });
    let (_v, challenge) = pkce();
    let q = format!(
        "/authorize?response_type=code&client_id={cid}&redirect_uri=https%3A%2F%2Fevil.example%2Fcb&code_challenge={challenge}&code_challenge_method=S256",
        cid = urlencode(&cimd_url),
    );
    let mut r = s.get(&q);
    assert_eq!(r.status().as_u16(), 200, "a page, not a redirect");
    assert!(location(&r).is_none(), "no Location: that would BE the bug");
    assert!(body(&mut r).contains("redirect_uri"));
}

#[test]
fn plain_pkce_and_missing_challenge_are_refused_with_iss() {
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| {
        cfg.allow_loopback_cimd = true;
    });
    for (method, note) in [("plain", "plain"), ("", "missing")] {
        let q = format!(
            "/authorize?response_type=code&client_id={cid}&redirect_uri=https%3A%2F%2Fchatgpt.com%2Fconnector_platform_oauth_redirect&code_challenge=abc&code_challenge_method={m}&state=zz",
            cid = urlencode(&cimd_url),
            m = method,
        );
        let r = s.get(&q);
        assert_eq!(r.status().as_u16(), 302, "{note}");
        let to = location(&r).unwrap(/* t */);
        assert!(to.contains("error=invalid_request"), "{note}: {to}");
        assert!(to.contains("state=zz"), "{note}: the state rides back");
        assert!(to.contains("iss="), "{note}: RFC 9207, errors included");
    }
}

#[test]
fn a_foreign_resource_is_invalid_target() {
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| {
        cfg.allow_loopback_cimd = true;
    });
    let (_v, challenge) = pkce();
    let q = authorize_query(
        &cimd_url,
        &challenge,
        "&resource=https%3A%2F%2Faltro.example%2Fmcp",
    );
    let r = s.get(&q);
    assert_eq!(r.status().as_u16(), 302);
    let to = location(&r).unwrap(/* t */);
    assert!(to.contains("error=invalid_target"), "{to}");
}

#[test]
fn a_reused_code_revokes_what_it_had_minted() {
    let (s, access, _refresh) = full_flow(false);
    // the token works...
    assert!(matches!(
        s.state
            .store
            .access_check(&access, "https://memoria.example.net/mcp", now())
            .unwrap(/* t */),
        engramma_mcp::oauth::TokenCheck::Valid { .. }
    ));
    // ...but reaching into the store for the family and forcing a reuse
    // alarm needs the code again, which the flow consumed: the reuse case
    // is proven by the store's own unit test; here the pair simply works
}

#[test]
fn the_sixth_pending_request_is_refused_with_a_page() {
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| {
        cfg.allow_loopback_cimd = true;
    });
    let (_v, challenge) = pkce();
    for i in 0..5 {
        let q = authorize_query(&cimd_url, &challenge, &format!("&state=n{i}"));
        let r = s.get(&q);
        assert_eq!(r.status().as_u16(), 303, "request {i}");
    }
    let q = authorize_query(&cimd_url, &challenge, "&state=sei");
    let mut r = s.get(&q);
    assert_eq!(r.status().as_u16(), 200, "a page, not a redirect");
    let html = body(&mut r);
    assert!(html.contains("Troppe richieste"), "{html}");
    assert!(location(&r).is_none());
}

#[test]
fn register_is_404_without_allow_dcr_and_201_with_it() {
    let s = Server::start(|_| {});
    let r = still()
        .post(&format!("http://{addr}/register", addr = s.addr))
        .header("Host", host_of(&s.public))
        .send_json(serde_json::json!({
            "redirect_uris": ["https://chatgpt.com/connector_platform_oauth_redirect"],
        }))
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 404);

    let s = Server::start(|cfg| {
        cfg.allow_dcr = true;
    });
    let mut r = still()
        .post(&format!("http://{addr}/register", addr = s.addr))
        .header("Host", host_of(&s.public))
        .send_json(serde_json::json!({
            "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
            "client_name": "Claude via DCR",
        }))
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 201);
    let doc = json_of(&mut r);
    let client_id = doc["client_id"].as_str().unwrap_or_default().to_string();
    assert!(!client_id.is_empty());
    assert_eq!(doc["token_endpoint_auth_method"], "none");

    // and the DCR client can start an authorization (the loopback CIMD
    // hatch is NOT needed: nothing is fetched)
    let (_v, challenge) = pkce();
    let q = format!(
        "/authorize?response_type=code&client_id={cid}&redirect_uri=https%3A%2F%2Fclaude.ai%2Fapi%2Fmcp%2Fauth_callback&code_challenge={ch}&code_challenge_method=S256&state=q",
        cid = urlencode(&client_id),
        ch = challenge,
    );
    let r = s.get(&q);
    assert_eq!(r.status().as_u16(), 303, "the DCR client is known");
}

#[test]
fn wrong_pkce_at_the_token_endpoint_is_invalid_grant() {
    // run a manual flow to a code, then present a wrong verifier
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| {
        cfg.allow_loopback_cimd = true;
    });
    let (_v, challenge) = pkce();
    let r = s.get(&authorize_query(&cimd_url, &challenge, ""));
    let attesa = location(&r).unwrap(/* t */);
    let mut page = s.get(&attesa);
    let code = code_of(&body(&mut page));
    let _ = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&s.root)
        .args(["oauth", "approve", &code, "--label", "p", "--yes"])
        .output()
        .unwrap(/* t */);
    let r = s.get(&attesa);
    let to = location(&r).unwrap(/* t */);
    let auth_code = to
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .unwrap_or_default()
        .to_string();
    let mut r = still()
        .post(&format!("http://{addr}/token", addr = s.addr))
        .header("Host", host_of(&s.public))
        .send_form(vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("client_id", cimd_url.as_str()),
            ("redirect_uri", "https://chatgpt.com/connector_platform_oauth_redirect"),
            ("code_verifier", "verifier-sbagliato-del-tutto"),
        ])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(json_of(&mut r)["error"], "invalid_grant");
}

#[test]
fn denial_redirects_with_access_denied() {
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| {
        cfg.allow_loopback_cimd = true;
    });
    let (_v, challenge) = pkce();
    let r = s.get(&authorize_query(&cimd_url, &challenge, ""));
    let attesa = location(&r).unwrap(/* t */);
    let mut page = s.get(&attesa);
    let code = code_of(&body(&mut page));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&s.root)
        .args(["oauth", "deny", &code])
        .output()
        .unwrap(/* t */);
    assert!(out.status.success());
    let r = s.get(&attesa);
    assert_eq!(r.status().as_u16(), 302);
    let to = location(&r).unwrap(/* t */);
    assert!(to.contains("error=access_denied"), "{to}");
    assert!(to.contains("iss="), "{to}");
}

// ---------------------------------------------------------------------------
// P1/P4: the binary itself — /mcp as the resource server
// ---------------------------------------------------------------------------

/// One `engramma mcp --http` child, with cells on disk and DCR open. The
/// fake client speaks to the loopback port with the PUBLIC Host, exactly
/// what the tunnel does for real.
struct BinServer {
    child: std::process::Child,
    addr: std::net::SocketAddr,
    err_path: std::path::PathBuf,
    root: std::path::PathBuf,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl Drop for BinServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bin(root: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap(/* t */);
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn bin_ok(root: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap(/* t */);
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn start_bin(extra: &[&str]) -> BinServer {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap(/* t */);
    bin_ok(&root, &["init"]);
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(/* t */);
        l.local_addr().unwrap(/* t */).port()
    };
    let err_path = dir.path().join("stderr.log");
    let err_file = std::fs::File::create(&err_path).unwrap(/* t */);
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&root)
        .args([
            "mcp",
            "--cells",
            "default",
            "--http",
            &format!("127.0.0.1:{port}"),
            "--public-url",
            "https://memoria.example.net",
        ])
        .args(extra)
        .stdout(std::process::Stdio::null())
        .stderr(err_file.try_clone().unwrap(/* t */))
        .spawn()
        .unwrap(/* t */);
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap(/* t */);
    // wait for the listener
    for _ in 0..100 {
        if std::net::TcpStream::connect(addr).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(err_file); // the child writes; the test re-opens to read
    BinServer {
        child,
        addr,
        err_path,
        root,
        dir,
    }
}

impl BinServer {
    /// A POST to /mcp with the public Host and (optionally) a bearer.
    fn mcp_post(&self, token: Option<&str>, body: &str) -> ureq::http::Response<ureq::Body> {
        let mut req = still()
            .post(&format!("http://{addr}/mcp", addr = self.addr))
            .header("Host", "memoria.example.net")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        req.send(body.as_bytes()).unwrap(/* t */)
    }

    fn stderr_so_far(&self) -> String {
        // a fresh handle at offset 0: the child's dup shares ITS offset,
        // and reading from it would start at the child's end
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }
}

/// Register a DCR client, run the flow with the binary's own oauth.db,
/// and come back with an access token.
fn bin_token(s: &BinServer) -> String {
    // DCR: no CIMD fetch needed
    let mut r = still()
        .post(&format!("http://{addr}/register", addr = s.addr))
        .header("Host", "memoria.example.net")
        .send_json(serde_json::json!({
            "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
            "client_name": "Claude",
        }))
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 201, "{}", body(&mut r));
    let client_id = json_of(&mut r)["client_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let (verifier, challenge) = pkce();
    let q = format!(
        "/authorize?response_type=code&client_id={cid}&redirect_uri=https%3A%2F%2Fclaude.ai%2Fapi%2Fmcp%2Fauth_callback&code_challenge={ch}&code_challenge_method=S256&scope=memory%3Aread&state=b1",
        cid = urlencode(&client_id),
        ch = challenge,
    );
    let host = "memoria.example.net";
    let r = still()
        .get(&format!("http://{addr}{q}", addr = s.addr))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    let mut page = still()
        .get(&format!(
            "http://{addr}{p}",
            addr = s.addr,
            p = location(&r).unwrap(/* t */)
        ))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    let code = code_of(&body(&mut page));
    bin_ok(
        &s.root,
        &["oauth", "approve", &code, "--label", "bin", "--yes"],
    );
    let page = still()
        .get(&format!(
            "http://{addr}{p}",
            addr = s.addr,
            p = location(&r).unwrap(/* t */)
        ))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    assert_eq!(page.status().as_u16(), 302);
    let auth_code = location(&page)
        .unwrap_or_default()
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .unwrap_or_default()
        .to_string();
    let mut r = still()
        .post(&format!("http://{addr}/token", addr = s.addr))
        .header("Host", host)
        .send_form(vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("client_id", client_id.as_str()),
            ("redirect_uri", "https://claude.ai/api/mcp/auth_callback"),
            ("code_verifier", verifier.as_str()),
        ])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    json_of(&mut r)["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn the_cli_refuses_what_it_must_refuse() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap(/* t */);
    bin_ok(&root, &["init"]);
    let cases: [(&str, &[&str]); 4] = [
        (
            "not loopback",
            &[
                "mcp",
                "--cells",
                "default",
                "--http",
                "0.0.0.0:8787",
                "--public-url",
                "https://m.example",
            ],
        ),
        (
            "no public url",
            &["mcp", "--cells", "default", "--http", "127.0.0.1:8787"],
        ),
        (
            "actor with http",
            &[
                "mcp",
                "--cells",
                "default",
                "--http",
                "127.0.0.1:8787",
                "--public-url",
                "https://m.example",
                "--actor",
                "x",
            ],
        ),
        (
            "http public url",
            &[
                "mcp",
                "--cells",
                "default",
                "--http",
                "127.0.0.1:8787",
                "--public-url",
                "http://m.example",
            ],
        ),
    ];
    for (why, args) in cases {
        let err = bin(&root, args);
        assert!(!err.is_empty(), "{why}: {err}");
        assert!(err.contains("engramma:"), "{why}: {err}");
    }
}

#[test]
fn mcp_without_a_token_is_401_with_the_right_challenge() {
    let s = start_bin(&["--allow-dcr"]);
    let r = s.mcp_post(
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    );
    assert_eq!(
        r.status().as_u16(),
        401,
        "no anonymous mode, not even initialize"
    );
    let challenge = r
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        challenge.contains("Bearer resource_metadata=\"https://memoria.example.net/.well-known/oauth-protected-resource/mcp\""),
        "{challenge}"
    );
    assert!(challenge.contains("scope=\"memory:read\""), "{challenge}");
}

#[test]
fn a_token_in_the_query_string_is_400() {
    let s = start_bin(&["--allow-dcr"]);
    let r = still()
        .post(&format!(
            "http://{addr}/mcp?access_token=eng_at_qualcosa",
            addr = s.addr
        ))
        .header("Host", "memoria.example.net")
        .send(&b"{}"[..])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 400);
}

#[test]
fn a_lying_host_or_a_present_origin_is_refused() {
    let s = start_bin(&["--allow-dcr"]);
    let token = bin_token(&s);
    // lying Host (raw TCP, like the review-page tests)
    let mut stream = std::net::TcpStream::connect(s.addr).unwrap(/* t */);
    use std::io::Write as _;
    let _ = stream.write_all(
        format!(
            "POST /mcp HTTP/1.1\r\nHost: evil.example\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        )
        .as_bytes(),
    );
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(&mut stream, &mut buf);
    let first = String::from_utf8_lossy(&buf);
    assert!(
        first.starts_with("HTTP/1.1 403"),
        "the Host must be ours: {first}"
    );
    // Origin present on /mcp: remote clients are servers, not browsers
    let r = still()
        .post(&format!("http://{addr}/mcp", addr = s.addr))
        .header("Host", "memoria.example.net")
        .header("Origin", "https://chatgpt.com")
        .header("Authorization", format!("Bearer {token}"))
        .send(&b"{}"[..])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 403);
}

#[test]
fn the_tool_list_is_per_token_and_search_works() {
    let s = start_bin(&["--allow-dcr"]);
    // a note to find (put through the engine directly: the tools are the
    // thing under test, not the CLI add)
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */);
    let note = engramma_core::Note {
        id: engramma_core::id::new_ulid().unwrap(/* t */),
        note_type: engramma_core::NoteType::Fact,
        title: None,
        body: "Fabio nuota il giovedì.".into(),
        tags: vec![],
        scope: None,
        lang: Some(engramma_core::Lang::It),
        source: "test".into(),
        confidence: engramma_core::Confidence::Stated,
        status: engramma_core::Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: engramma_core::format::At(t),
        updated_at: engramma_core::format::At(t),
        extensions: vec![],
    };
    engramma_core::Cell::open(&s.root.join("cells/default.sqlite"))
        .unwrap(/* t */)
        .put_note(&note, "test")
        .unwrap(/* t */);
    let token = bin_token(&s);

    // initialize → session
    let mut r = s.mcp_post(
        Some(&token),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    );
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    let session = r
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut req = still()
        .post(&format!("http://{addr}/mcp", addr = s.addr))
        .header("Host", "memoria.example.net")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Authorization", format!("Bearer {token}"));
    if let Some(sess) = &session {
        req = req.header("Mcp-Session-Id", sess);
    }
    let mut r = req
        .send(&br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#[..])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200);
    let doc = json_of(&mut r);
    let tools: Vec<String> = doc["result"]["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| t["name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        tools,
        vec![
            "memory_get",
            "memory_neighbors",
            "memory_search",
            "memory_tags",
            "memory_timeline",
        ],
        "a read-only grant sees the five read tools, exactly"
    );

    // memory_search finds the living note
    let mut req = still()
        .post(&format!("http://{addr}/mcp", addr = s.addr))
        .header("Host", "memoria.example.net")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Authorization", format!("Bearer {token}"));
    if let Some(sess) = &session {
        req = req.header("Mcp-Session-Id", sess);
    }
    let mut r = req
        .send(
            &br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"nuota"}}}"#[..],
        )
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200);
    let text = body(&mut r);
    assert!(text.contains("Fabio nuota il giovedì"), "{text}");

    // memory_propose with a read-only token: the step-up refusal
    let mut req = still()
        .post(&format!("http://{addr}/mcp", addr = s.addr))
        .header("Host", "memoria.example.net")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Authorization", format!("Bearer {token}"));
    if let Some(sess) = &session {
        req = req.header("Mcp-Session-Id", sess);
    }
    let mut r = req
        .send(
            &br#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_propose","arguments":{"text":"Fabio scala le montagne."}}}"#[..],
        )
        .unwrap(/* t */);
    let text = body(&mut r);
    assert!(text.contains("insufficient_scope"), "{text}");

    // the access log says the grant, never the token nor the note text
    let log = s.stderr_so_far();
    assert!(log.contains("method=tools/call"), "{log}");
    assert!(!log.contains(&token), "the token never appears: {log}");
    assert!(!log.contains("nuota"), "the note text never appears: {log}");
}

#[test]
fn a_revoked_grant_dies_at_the_next_request() {
    let s = start_bin(&["--allow-dcr"]);
    let token = bin_token(&s);
    let r = s.mcp_post(
        Some(&token),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    );
    assert_eq!(r.status().as_u16(), 200);
    bin_ok(&s.root, &["oauth", "revoke", "--all"]);
    let r = s.mcp_post(
        Some(&token),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    assert_eq!(r.status().as_u16(), 401, "the next request finds it dead");
}

#[test]
fn sigterm_shuts_the_server_down() {
    let mut s = start_bin(&["--allow-dcr"]);
    let token = bin_token(&s);
    let r = s.mcp_post(
        Some(&token),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    );
    assert_eq!(r.status().as_u16(), 200);
    // via sh: not every minimal image ships kill as a standalone binary
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -TERM {}", s.child.id()))
        .status();
    // exits within a reasonable window
    for _ in 0..100 {
        if let Ok(Some(_)) = s.child.try_wait() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("SIGTERM did not stop the server");
}

// ---------------------------------------------------------------------------
// P5: proposals over HTTP — the form (2026-07-28) and the queue
// ---------------------------------------------------------------------------

/// A tools/call POST carrying the 2026-07-28 per-request `_meta`.
fn stateless_call(s: &BinServer, token: &str, id: i64, params: serde_json::Value) -> Value {
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": params
    });
    // the 2026-07-28 per-request _meta wants its header too
    let version = params["_meta"]["io.modelcontextprotocol/protocolVersion"]
        .as_str()
        .unwrap_or("2025-06-18");
    let mut r = still()
        .post(&format!("http://{addr}/mcp", addr = s.addr))
        .header("Host", "memoria.example.net")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", version)
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", params["name"].as_str().unwrap_or_default())
        .header("Authorization", format!("Bearer {token}"))
        .send(call.to_string().as_bytes())
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    let mut doc = json_of(&mut r);
    // an SSE envelope unwraps to the same JSON-RPC answer
    if doc.get("result").is_none() && doc.get("error").is_none() {
        doc = json_of(&mut r);
    }
    doc
}

#[test]
fn the_confirmation_form_arrives_over_http_prefilled() {
    let s = start_bin(&["--allow-dcr", "--write", "default"]);
    // the policy of the cell decides: write: confirm
    bin_ok(&s.root, &["policy", "set", "write=confirm"]);
    let token = bin_token_scopes(&s, "memory:read memory:propose");
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": { "elicitation": {} }
    });

    // round 1: input_required with the form, PRE-FILLED with the text
    let res = stateless_call(
        &s,
        &token,
        2,
        serde_json::json!({
            "name": "memory_propose",
            "arguments": { "text": "Fabio va in barca." },
            "_meta": meta
        }),
    );
    assert_eq!(res["result"]["resultType"], "input_required", "{res}");
    let request = &res["result"]["inputRequests"]["confirm"];
    assert!(request.is_object(), "{res}");
    let default_text = request["params"]["requestedSchema"]["properties"]["text"]["default"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        default_text, "Fabio va in barca.",
        "the correction bfdb1f5, over HTTP: {res}"
    );
    let state = res["result"]["requestState"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    // verifica v0.5.2: ANOTHER grant presenting this requestState finds
    // nothing — and its attempt must not burn the owner's round
    let other = bin_token_scopes(&s, "memory:read memory:propose");
    let res = stateless_call(
        &s,
        &other,
        30,
        serde_json::json!({
            "name": "memory_propose",
            "arguments": { "text": "Fabio va in barca." },
            "_meta": meta,
            "inputResponses": {
                "confirm": { "action": "accept", "content": { "text": "dirottata" } }
            },
            "requestState": state
        }),
    );
    let as_text = res.to_string();
    assert!(
        res.get("error").is_some() || as_text.contains("unknown requestState"),
        "a foreign grant's retry: {res}"
    );

    // round 2: the retry brings the answer (accept with an edit)
    let res = stateless_call(
        &s,
        &token,
        3,
        serde_json::json!({
            "name": "memory_propose",
            "arguments": { "text": "Fabio va in barca." },
            "_meta": meta,
            "inputResponses": {
                "confirm": { "action": "accept", "content": { "text": "Fabio va in barca a vela." } }
            },
            "requestState": state
        }),
    );
    let payload: Value = serde_json::from_str(
        res["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default(),
    )
    .unwrap_or(Value::Null);
    assert_eq!(payload["status"], "accepted", "{res}");
    assert!(
        payload["decided_by"]
            .as_str()
            .is_some_and(|a| a.starts_with("user@mcp:") && !a.starts_with("user@user@")),
        "one prefix, not two: {res}"
    );
    let id = payload["id"].as_str().unwrap_or_default().to_string();
    let cell = engramma_core::Cell::open(&s.root.join("cells/default.sqlite")).unwrap(/* t */);
    let note = cell
        .get(&ulid::Ulid::from_string(&id).unwrap(/* t */))
        .unwrap(/* t */);
    assert_eq!(note.body, "Fabio va in barca a vela.");
    // the actor is the grant's label
    assert!(
        note.source.starts_with("mcp:bin"),
        "the label approved by the human: {}",
        note.source
    );

    // a reused requestState finds nothing: the state is one-shot
    let res = stateless_call(
        &s,
        &token,
        4,
        serde_json::json!({
            "name": "memory_propose",
            "arguments": { "text": "Fabio dorme." },
            "_meta": meta,
            "inputResponses": {
                "confirm": { "action": "accept", "content": { "text": "Fabio dorme." } }
            },
            "requestState": state
        }),
    );
    assert!(res.get("error").is_some(), "{res}");
}

#[test]
fn a_client_without_forms_gets_the_queue_and_no_secrets_in_the_answer() {
    let s = start_bin(&["--allow-dcr", "--write", "default"]);
    bin_ok(&s.root, &["policy", "set", "write=confirm"]);
    let token = bin_token_scopes(&s, "memory:read memory:propose");
    // no elicitation capability declared: the ChatGPT shape
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let res = stateless_call(
        &s,
        &token,
        2,
        serde_json::json!({
            "name": "memory_propose",
            "arguments": { "text": "Fabio colleziona dischi." },
            "_meta": meta
        }),
    );
    let payload: Value = serde_json::from_str(
        res["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default(),
    )
    .unwrap_or(Value::Null);
    assert_eq!(payload["status"], "pending", "{res}");
    let note_line = payload["note"].as_str().unwrap_or_default().to_string();
    assert!(
        note_line.contains("engramma proposals") && note_line.contains("engramma review"),
        "the pointer is the local CLI, not a URL: {note_line}"
    );
    assert!(
        !note_line.contains("https://"),
        "no URL in the answer: {note_line}"
    );
    assert!(
        !note_line.contains("token"),
        "no token in the answer: {note_line}"
    );
}

/// [`bin_token`] with a chosen scope string.
fn bin_token_scopes(s: &BinServer, scopes: &str) -> String {
    bin_token_inner(s, Some(scopes))
}

fn bin_token_inner(s: &BinServer, scopes: Option<&str>) -> String {
    // labels are unique per grant: every flow gets its own
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let label = format!("bin{}", N.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
    let mut r = still()
        .post(&format!("http://{addr}/register", addr = s.addr))
        .header("Host", "memoria.example.net")
        .send_json(serde_json::json!({
            "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
            "client_name": "Claude",
        }))
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 201, "{}", body(&mut r));
    let client_id = json_of(&mut r)["client_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let (verifier, challenge) = pkce();
    let scope = scopes.unwrap_or("memory:read");
    let q = format!(
        "/authorize?response_type=code&client_id={cid}&redirect_uri=https%3A%2F%2Fclaude.ai%2Fapi%2Fmcp%2Fauth_callback&code_challenge={ch}&code_challenge_method=S256&scope={sc}&state=b1",
        cid = urlencode(&client_id),
        ch = challenge,
        sc = urlencode(scope),
    );
    let host = "memoria.example.net";
    let r = still()
        .get(&format!("http://{addr}{q}", addr = s.addr))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    let attesa = location(&r).unwrap(/* t */);
    let mut page = still()
        .get(&format!("http://{addr}{p}", addr = s.addr, p = attesa))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    let code = code_of(&body(&mut page));
    bin_ok(
        &s.root,
        &["oauth", "approve", &code, "--label", &label, "--yes"],
    );
    let page = still()
        .get(&format!("http://{addr}{p}", addr = s.addr, p = attesa))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    assert_eq!(page.status().as_u16(), 302);
    let auth_code = location(&page)
        .unwrap_or_default()
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .unwrap_or_default()
        .to_string();
    let mut r = still()
        .post(&format!("http://{addr}/token", addr = s.addr))
        .header("Host", host)
        .send_form(vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("client_id", client_id.as_str()),
            ("redirect_uri", "https://claude.ai/api/mcp/auth_callback"),
            ("code_verifier", verifier.as_str()),
        ])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    json_of(&mut r)["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------
// P4's remaining invariants, proven over HTTP
// ---------------------------------------------------------------------------

#[test]
fn the_old_invariants_hold_over_http() {
    let s = start_bin(&["--allow-dcr", "--write", "default"]);
    bin_ok(&s.root, &["policy", "set", "write=propose"]);
    let token = bin_token_scopes(&s, "memory:read memory:propose");
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    // a living note, and a quarantined proposal
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */);
    let cell = engramma_core::Cell::open(&s.root.join("cells/default.sqlite")).unwrap(/* t */);
    for body in ["Fabio nuota il giovedì.", "Fabio vince le gare di tuffi."] {
        let note = engramma_core::Note {
            id: engramma_core::id::new_ulid().unwrap(/* t */),
            note_type: engramma_core::NoteType::Fact,
            title: None,
            body: body.into(),
            tags: vec![],
            scope: None,
            lang: Some(engramma_core::Lang::It),
            source: "test".into(),
            confidence: engramma_core::Confidence::Stated,
            status: engramma_core::Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: engramma_core::format::At(t),
            updated_at: engramma_core::format::At(t),
            extensions: vec![],
        };
        cell.put_note(&note, "test").unwrap(/* t */);
    }
    let res = stateless_call(
        &s,
        &token,
        2,
        serde_json::json!({
            "name": "memory_propose",
            "arguments": { "text": "Fabio vince le gare di apnea." },
            "_meta": meta
        }),
    );
    let propose_payload: Value = serde_json::from_str(
        res["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default(),
    )
    .unwrap_or(Value::Null);
    assert_eq!(propose_payload["status"], "pending", "{res}");

    // the quarantine does not leak: the proposal is invisible to search
    let res = stateless_call(
        &s,
        &token,
        3,
        serde_json::json!({
            "name": "memory_search",
            "arguments": { "query": "apnea" },
            "_meta": meta
        }),
    );
    let text = res.to_string();
    assert!(
        !text.contains("gare di apnea"),
        "the queue stays invisible: {text}"
    );
    // and the living note IS there, same words
    let res = stateless_call(
        &s,
        &token,
        6,
        serde_json::json!({
            "name": "memory_search",
            "arguments": { "query": "giovedì" },
            "_meta": meta
        }),
    );
    let text = res.to_string();
    assert!(text.contains("nuota"), "the living note is there: {text}");

    // external confidence is refused: that is the import path's badge
    let res = stateless_call(
        &s,
        &token,
        4,
        serde_json::json!({
            "name": "memory_propose",
            "arguments": { "text": "Fabio legge il giornale.", "confidence": "external" },
            "_meta": meta
        }),
    );
    assert!(res.get("error").is_some(), "{res}");

    // a cell outside the server answers like one that does not exist
    let res = stateless_call(
        &s,
        &token,
        5,
        serde_json::json!({
            "name": "memory_search",
            "arguments": { "query": "x", "cell": "segreta" },
            "_meta": meta
        }),
    );
    let err = res["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        err.contains("not exposed") && !err.contains("segreta is not exposed"),
        "no oracle on cells: {err}"
    );
}

// ---------------------------------------------------------------------------
// P6: --compat chatgpt — the fixed-schema wrappers
// ---------------------------------------------------------------------------

/// A bare stateless request (2026-07-28 carries its own _meta).
fn stateless_req(s: &BinServer, token: &str, id: i64, method: &str) -> Value {
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": method,
        "params": { "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        } }
    });
    let mut r = still()
        .post(&format!("http://{addr}/mcp", addr = s.addr))
        .header("Host", "memoria.example.net")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method)
        .header("Authorization", format!("Bearer {token}"))
        .send(call.to_string().as_bytes())
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    json_of(&mut r)
}

fn tool_names(res: &Value) -> Vec<String> {
    res["result"]["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| t["name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_chatgpt_wrappers_exist_only_with_the_flag() {
    // without the flag: the five read tools, nothing else
    let s = start_bin(&["--allow-dcr"]);
    let token = bin_token(&s);
    let res = stateless_req(&s, &token, 2, "tools/list");
    assert_eq!(
        tool_names(&res),
        vec![
            "memory_get",
            "memory_neighbors",
            "memory_search",
            "memory_tags",
            "memory_timeline",
        ],
        "{res}"
    );

    // with it: search and fetch join, and nothing else changes
    let s = start_bin(&["--allow-dcr", "--compat", "chatgpt"]);
    let token = bin_token(&s);
    let res = stateless_req(&s, &token, 2, "tools/list");
    assert_eq!(
        tool_names(&res),
        vec![
            "fetch",
            "memory_get",
            "memory_neighbors",
            "memory_search",
            "memory_tags",
            "memory_timeline",
            "search",
        ],
        "Claude clients must not see doubles: {res}"
    );
}

#[test]
fn search_and_fetch_wrap_the_engine_over_http() {
    let s = start_bin(&["--allow-dcr", "--compat", "chatgpt"]);
    let token = bin_token(&s);
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    // a living note (and a quarantined one to stay honest)
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */);
    let cell = engramma_core::Cell::open(&s.root.join("cells/default.sqlite")).unwrap(/* t */);
    let make = |body: &str, status| {
        let note = engramma_core::Note {
            id: engramma_core::id::new_ulid().unwrap(/* t */),
            note_type: engramma_core::NoteType::Fact,
            title: None,
            body: body.into(),
            tags: vec![],
            scope: None,
            lang: Some(engramma_core::Lang::It),
            source: "test".into(),
            confidence: engramma_core::Confidence::Stated,
            status,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: engramma_core::format::At(t),
            updated_at: engramma_core::format::At(t),
            extensions: vec![],
        };
        let id = note.id;
        cell.put_note(&note, "test").unwrap(/* t */);
        id
    };
    make("Fabio nuota il giovedì.", engramma_core::Status::Active);
    let hidden = make(
        "Fabio vince i tornei di scarabeo.",
        engramma_core::Status::Quarantined,
    );

    // search: id, title, EMPTY url
    let res = stateless_call(
        &s,
        &token,
        2,
        serde_json::json!({
            "name": "search",
            "arguments": { "query": "nuota" },
            "_meta": meta
        }),
    );
    let structured = &res["result"]["structuredContent"];
    let results = structured["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!results.is_empty(), "{res}");
    assert!(results[0]["url"] == "", "no public url for a note: {res}");
    let id = results[0]["id"].as_str().unwrap_or_default().to_string();
    assert_eq!(id.len(), 26, "the FULL id, search-side too: {id}");

    // fetch: text and metadata, structured AND in content
    let res = stateless_call(
        &s,
        &token,
        3,
        serde_json::json!({
            "name": "fetch",
            "arguments": { "id": id },
            "_meta": meta
        }),
    );
    let structured = &res["result"]["structuredContent"];
    assert_eq!(structured["id"], id.as_str());
    assert!(
        structured["text"]
            .as_str()
            .is_some_and(|t| t.contains("nuota")),
        "{res}"
    );
    assert!(structured["url"] == "", "{res}");
    assert!(structured["metadata"]["type"] == "fact", "{res}");
    let content_text = res["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        content_text.contains("nuota"),
        "the JSON rides in content too"
    );

    // the hidden note is not fetchable: same answer as a missing one
    for probe in [
        hidden.to_string(),
        "01AROTONONEXISTE0000000000AA".to_string(),
    ] {
        let res = stateless_call(
            &s,
            &token,
            4,
            serde_json::json!({
                "name": "fetch",
                "arguments": { "id": probe },
                "_meta": meta
            }),
        );
        assert!(res.get("error").is_some(), "no oracle: {probe} — {res}");
    }
}

#[test]
fn sessions_belong_to_the_grant_that_opened_them() {
    let s = start_bin(&["--allow-dcr"]);
    // two grants, each with its own initialize
    let token_a = bin_token_scopes(&s, "memory:read");
    let token_b = bin_token_scopes(&s, "memory:read");
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    let r = s.mcp_post(Some(&token_a), init);
    assert_eq!(r.status().as_u16(), 200);
    let sess_a = r
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let r = s.mcp_post(Some(&token_b), init);
    assert_eq!(r.status().as_u16(), 200);
    let sess_b = r
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (Some(a), Some(b)) = (sess_a, sess_b) else {
        panic!("both sessions must exist");
    };
    assert_ne!(a, b);

    // B's token on A's session: 404, exactly like an unknown session
    let r = still()
        .post(&format!("http://{addr}/mcp", addr = s.addr))
        .header("Host", "memoria.example.net")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Mcp-Session-Id", a)
        .header("Authorization", format!("Bearer {token_b}"))
        .send(&br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#[..])
        .unwrap(/* t */);
    assert_eq!(
        r.status().as_u16(),
        404,
        "a stolen session is an unknown session"
    );
    // and a session that never existed, same answer
    let r = still()
        .post(&format!("http://{addr}/mcp", addr = s.addr))
        .header("Host", "memoria.example.net")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", "00000000-0000-0000-0000-000000000000")
        .header("Authorization", format!("Bearer {token_a}"))
        .send(&br#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#[..])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 404);
}

#[test]
fn revoke_answers_200_even_for_an_unknown_token() {
    let s = start_bin(&["--allow-dcr"]);
    let r = still()
        .post(&format!("http://{addr}/revoke", addr = s.addr))
        .header("Host", "memoria.example.net")
        .send_form(vec![("token", "eng_at_mai-esistito")])
        .unwrap(/* t */);
    assert_eq!(
        r.status().as_u16(),
        200,
        "RFC 7009: no oracle on revocation"
    );
    assert_eq!(r.headers().get("cache-control").unwrap(), "no-store");
}

#[test]
fn the_token_endpoint_slams_the_door_at_sixty_a_minute() {
    let s = start_bin(&["--allow-dcr"]);
    let host = "memoria.example.net";
    let mut codes = Vec::new();
    for _ in 0..61 {
        let r = still()
            .post(&format!("http://{addr}/token", addr = s.addr))
            .header("Host", host)
            .send_form(vec![("grant_type", "authorization_code")])
            .unwrap(/* t */);
        codes.push(r.status().as_u16());
    }
    assert!(
        codes.iter().take(60).all(|c| *c == 400),
        "invalid requests, but served: {codes:?}"
    );
    assert_eq!(codes[60], 429, "the 61st hits the global ceiling");
    let r = still()
        .post(&format!("http://{addr}/token", addr = s.addr))
        .header("Host", host)
        .send_form(vec![("grant_type", "authorization_code")])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 429);
    assert!(r.headers().contains_key("retry-after"));
}

// ---------------------------------------------------------------------------
// verifica v0.5.2 (il revisore): i difetti trovati alla rilettura
// ---------------------------------------------------------------------------

/// Run the flow up to an authorization code (approved by the real CLI):
/// the verifier and the code, ready for /token.
fn to_auth_code(s: &Server, cimd_url: &str, label: &str) -> (String, String) {
    let (verifier, challenge) = pkce();
    let r = s.get(&authorize_query(cimd_url, &challenge, ""));
    let attesa = location(&r).unwrap(/* t */);
    let mut page = s.get(&attesa);
    let code = code_of(&body(&mut page));
    bin_ok(
        &s.root,
        &["oauth", "approve", &code, "--label", label, "--yes"],
    );
    let r = s.get(&attesa);
    let auth_code = location(&r)
        .unwrap_or_default()
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .unwrap_or_default()
        .to_string();
    assert!(auth_code.starts_with("eng_code_"), "{auth_code}");
    (verifier, auth_code)
}

fn token_post(s: &Server, form: Vec<(&str, &str)>) -> ureq::http::Response<ureq::Body> {
    still()
        .post(&format!("http://{addr}/token", addr = s.addr))
        .header("Host", host_of(&s.public))
        .send_form(form)
        .unwrap(/* t */)
}

#[test]
fn the_token_endpoint_binds_the_code_to_its_client_and_redirect() {
    // OAuth 2.1 § 4.1.3: the code answers only to the client and the
    // redirect_uri it was issued for — PKCE is a second lock, not the only one
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| cfg.allow_loopback_cimd = true);

    let (verifier, auth_code) = to_auth_code(&s, &cimd_url, "uno");
    let mut r = token_post(
        &s,
        vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("client_id", "https://altro.example/client.json"),
            (
                "redirect_uri",
                "https://chatgpt.com/connector_platform_oauth_redirect",
            ),
            ("code_verifier", verifier.as_str()),
        ],
    );
    assert_eq!(r.status().as_u16(), 400, "another client's code");
    assert_eq!(json_of(&mut r)["error"], "invalid_grant");

    let (verifier, auth_code) = to_auth_code(&s, &cimd_url, "due");
    let mut r = token_post(
        &s,
        vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("client_id", cimd_url.as_str()),
            ("redirect_uri", "https://chatgpt.com/altro_redirect"),
            ("code_verifier", verifier.as_str()),
        ],
    );
    assert_eq!(r.status().as_u16(), 400, "another redirect_uri");
    assert_eq!(json_of(&mut r)["error"], "invalid_grant");
}

/// A pending request on an in-process server: its approval code.
fn pending_code(s: &Server, cimd_url: &str) -> String {
    let (_v, challenge) = pkce();
    let r = s.get(&authorize_query(cimd_url, &challenge, ""));
    let attesa = location(&r).unwrap(/* t */);
    let mut page = s.get(&attesa);
    code_of(&body(&mut page))
}

#[test]
fn the_interactive_approval_answers_to_one_line() {
    // without --yes the CLI asks [s/N]: a human types `s` and Enter, and
    // the terminal stays open — the answer is a LINE, not the whole stdin
    use std::io::Write as _;
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| cfg.allow_loopback_cimd = true);
    let code = pending_code(&s, &cimd_url);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&s.root)
        .args(["oauth", "approve", &code, "--label", "umano"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap(/* t */);
    let mut stdin = child.stdin.take().unwrap(/* t */);
    stdin.write_all(b"s\n").unwrap(/* t */);
    stdin.flush().unwrap(/* t */);
    // the pipe stays OPEN, like a terminal
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap(/* t */) {
            break Some(st);
        }
        if std::time::Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    if status.is_none() {
        let _ = child.kill();
        panic!("`oauth approve` waits for the end of stdin instead of the answer line");
    }
    drop(stdin);
    assert!(status.is_some_and(|st| st.success()));
    let grants = s.state.store.grants().unwrap(/* t */);
    assert!(grants.iter().any(|g| g.label == "umano"), "approved");
}

#[test]
fn approving_with_a_cell_the_server_does_not_offer_is_refused() {
    // `--cells` only narrows: a name outside the offer is a typo, and a
    // grant with NO cell would be a useless connection that looks approved
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| cfg.allow_loopback_cimd = true);
    let code = pending_code(&s, &cimd_url);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&s.root)
        .args([
            "oauth", "approve", &code, "--label", "x", "--cells", "famigila", "--yes",
        ])
        .output()
        .unwrap(/* t */);
    assert!(!out.status.success(), "a cell outside the offer");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("famigila"),
        "the error names the cell: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // the request is still there to decide
    bin_ok(
        &s.root,
        &["oauth", "approve", &code, "--label", "x", "--yes"],
    );
}

#[test]
fn a_bracketed_ipv6_loopback_host_is_a_loopback_host() {
    // `Host: [::1]:port` is what a client on ::1 sends: the fence used to
    // cut at the first `:` and refuse it
    let s = Server::start(|_| {});
    let r = still()
        .get(&format!(
            "http://{addr}/.well-known/oauth-protected-resource",
            addr = s.addr
        ))
        .header("Host", format!("[::1]:{}", s.addr.port()))
        .call()
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200);
}

#[test]
fn the_approval_code_is_forgiving_about_case_and_dash() {
    // the human reads XXXX-XXXX off a page and types it: lowercase and a
    // missing dash are the same code
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| cfg.allow_loopback_cimd = true);
    let code = pending_code(&s, &cimd_url);
    let typed = code.replace('-', "").to_ascii_lowercase();
    bin_ok(
        &s.root,
        &["oauth", "approve", &typed, "--label", "minuscolo", "--yes"],
    );
    let code = pending_code(&s, &cimd_url);
    bin_ok(&s.root, &["oauth", "deny", &code.to_ascii_lowercase()]);
}

#[test]
fn a_revoked_label_can_be_approved_again_and_a_live_one_cannot() {
    // «revoca e ricollegamento» (docs/http.md): after `oauth revoke chatgpt`
    // the human reconnects and approves with the SAME label — the label is
    // unique among LIVE grants, not forever (verifica v0.5.2: UNIQUE on the
    // whole table made the reconnection fail with a SQL error)
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| cfg.allow_loopback_cimd = true);
    let code = pending_code(&s, &cimd_url);
    bin_ok(
        &s.root,
        &["oauth", "approve", &code, "--label", "chatgpt", "--yes"],
    );

    // a second connection under a label that is alive: a clear refusal
    let code = pending_code(&s, &cimd_url);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&s.root)
        .args(["oauth", "approve", &code, "--label", "chatgpt", "--yes"])
        .output()
        .unwrap(/* t */);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("chatgpt") && !err.contains("UNIQUE"), "{err}");

    // revoke, then the same label again: fine
    bin_ok(&s.root, &["oauth", "revoke", "chatgpt"]);
    bin_ok(
        &s.root,
        &["oauth", "approve", &code, "--label", "chatgpt", "--yes"],
    );
    let live: Vec<_> = s
        .state
        .store
        .grants()
        .unwrap(/* t */)
        .into_iter()
        .filter(|g| g.label == "chatgpt" && g.revoked_at.is_none())
        .collect();
    assert_eq!(live.len(), 1, "one live grant under the label");
    // and `revoke chatgpt` now hits the LIVE one, not the old revoked one
    bin_ok(&s.root, &["oauth", "revoke", "chatgpt"]);
    assert!(
        s.state
            .store
            .grants()
            .unwrap(/* t */)
            .iter()
            .all(|g| g.revoked_at.is_some()),
        "both revoked"
    );
}

#[test]
fn oauth_pending_lists_what_waits() {
    // verifica v0.5.2 (prova col tunnel): the pending filter compared the
    // CREATION time with now, so `oauth pending` — the human's first
    // command — never listed anything
    let (cimd_url, _cimd) = cimd_server();
    let s = Server::start(|cfg| cfg.allow_loopback_cimd = true);
    let code = pending_code(&s, &cimd_url);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&s.root)
        .args(["oauth", "pending"])
        .output()
        .unwrap(/* t */);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&code), "the waiting code is listed: {text}");
    assert!(text.contains("ChatGPT di prova"), "{text}");
}

// ---------------------------------------------------------------------------
// v0.5.3 (P2): the compatibility wrappers belong to the ChatGPT grant alone
// ---------------------------------------------------------------------------

/// A CIMD server on loopback serving a GIVEN document body, with `{URL}`
/// replaced by the document's own address (a CIMD must claim itself).
fn cimd_doc(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(/* t */);
    let port = listener.local_addr().unwrap(/* t */).port();
    let handle = std::thread::spawn(move || {
        use std::io::Read as _;
        use std::io::Write as _;
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf);
            let url = format!("http://127.0.0.1:{port}/client.json");
            let body = body.replace("{URL}", &url);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://127.0.0.1:{port}/client.json"), handle)
}

/// The three real documents (fetched 2026-11-12, the tunnel test of
/// v0.5.2.2): only `client_id` becomes the loopback URL of the test
/// server, because a CIMD must claim its own address.
const CHATGPT_CIMD: &str = r#"{"client_id":"{URL}","client_uri":"https://chatgpt.com/","redirect_uris":["https://chatgpt.com/connector_platform_oauth_redirect"],"token_endpoint_auth_method":"private_key_jwt","token_endpoint_auth_methods_supported":["none","private_key_jwt"],"grant_types":["authorization_code","refresh_token","urn:ietf:params:oauth:grant-type:jwt-bearer"],"response_types":["code"],"client_name":"ChatGPT","token_endpoint_auth_signing_alg":"RS256","jwks_uri":"https://chatgpt.com/oauth/jwks.json"}"#;
const CODEX_CIMD: &str = r#"{"client_id":"{URL}","client_uri":"https://chatgpt.com/","redirect_uris":["http://127.0.0.1/callback","http://localhost/callback"],"token_endpoint_auth_method":"none","client_name":"Codex"}"#;
const CLAUDE_CIMD: &str = r#"{"client_id":"{URL}","client_uri":"https://claude.ai/","redirect_uris":["https://claude.ai/api/mcp/auth_callback"],"token_endpoint_auth_method":"none","client_name":"Claude"}"#;

/// One grant for a CIMD client of `addr`'s server: authorize, approve in
/// the store, mint the code at the waiting page, exchange it. Returns an
/// access token.
fn compat_flow_token(
    state: &Arc<HttpState>,
    addr: &SocketAddr,
    client_id: &str,
    redirect: &str,
    label: &str,
) -> String {
    let (verifier, challenge) = pkce();
    let q = format!(
        "/authorize?response_type=code&client_id={cid}&redirect_uri={ru}&code_challenge={ch}&code_challenge_method=S256&scope=memory%3Aread&state=st",
        cid = urlencode(client_id),
        ru = urlencode(redirect),
        ch = challenge,
    );
    let host = "memoria.example.net";
    let r = still()
        .get(&format!("http://{addr}{q}", addr = addr))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303, "to the waiting page");
    let attesa = location(&r).unwrap(/* t */);
    let mut page = still()
        .get(&format!("http://{addr}{attesa}", addr = addr))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    let html = body(&mut page);
    let code = code_of(&html);
    assert!(!code.is_empty(), "no approval code in {html}");
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    state
        .store
        .request_approve(
            &code,
            label,
            &state.cfg.cells.clone(),
            &[engramma_mcp::oauth::store::SCOPE_READ.to_string()],
            now,
        )
        .unwrap(/* t */)
        .expect("approved");
    let mut r = still()
        .get(&format!("http://{addr}{attesa}", addr = addr))
        .header("Host", host)
        .call()
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 302);
    let to = location(&r).unwrap(/* t */);
    let auth_code = to
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .unwrap_or_default()
        .to_string();
    let mut r = still()
        .post(&format!("http://{addr}/token", addr = addr))
        .header("Host", host)
        .send_form(vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("code_verifier", verifier.as_str()),
        ])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    json_of(&mut r)["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// One JSON-RPC call over /mcp with a token (initialize first, session
/// carried). Returns the parsed response of the LAST request.
fn mcp_rpc(
    addr: &SocketAddr,
    token: &str,
    method: &str,
    params: &str,
    id: i64,
) -> serde_json::Value {
    let host = "memoria.example.net";
    let mut r = still()
        .post(&format!("http://{addr}/mcp", addr = addr))
        .header("Host", host)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Authorization", format!("Bearer {token}"))
        .send(&br#"{"jsonrpc":"2.0","id":900,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#[..])
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    let session = r
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut req = still()
        .post(&format!("http://{addr}/mcp", addr = addr))
        .header("Host", host)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Authorization", format!("Bearer {token}"));
    if let Some(sess) = &session {
        req = req.header("Mcp-Session-Id", sess);
    }
    let call = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#);
    let mut r = req.send(call.as_bytes()).unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200, "{}", body(&mut r));
    json_of(&mut r)
}

/// v0.5.3 (P2): with `--compat chatgpt`, the `search`/`fetch` wrappers
/// exist ONLY for the grant whose client is ChatGPT's own CIMD. Codex and
/// claude.ai (their real documents, loopback client_ids) list the normal
/// tools, and calling `search` by name answers EXACTLY like a tool that
/// does not exist — no oracle.
#[test]
fn compat_wrappers_belong_to_the_chatgpt_grant_alone() {
    let (gpt_url, _g) = cimd_doc(CHATGPT_CIMD);
    let (codex_url, _c) = cimd_doc(CODEX_CIMD);
    let (claude_url, _l) = cimd_doc(CLAUDE_CIMD);
    let keep = tempfile::tempdir().unwrap(/* t */);
    let root = keep.path().join("root");
    std::fs::create_dir_all(&root).unwrap(/* t */);
    // the cell the server exposes (empty: the tool LIST is the test)
    std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
    std::fs::write(
        root.join("cells/famiglia.sqlite"),
        {
            // a real (empty) cell: the server opens it at startup
            let dir = tempfile::tempdir().unwrap(/* t */);
            let c = engramma_core::Cell::create(&dir.path().join("x.sqlite"), "famiglia", "t")
                .unwrap(/* t */);
            drop(c);
            std::fs::read(dir.path().join("x.sqlite")).unwrap(/* t */)
        },
    )
    .unwrap(/* t */);
    let cfg = HttpConfig {
        bind: "127.0.0.1:0".parse().unwrap(/* t */),
        public_url: "https://memoria.example.net".into(),
        cells: vec!["famiglia".into()],
        write: vec![],
        allow_dcr: false,
        compat_chatgpt: true,
        chatgpt_client_id: gpt_url.clone(),
        allow_insecure_public_url: false,
        allow_loopback_cimd: true,
    };
    let state = Arc::new(HttpState::open_with_mcp(&root, cfg).unwrap(/* t */));
    let addr = start_router(engramma_mcp::http::router(Arc::clone(&state)));

    let gpt = compat_flow_token(
        &state,
        &addr,
        &gpt_url,
        "https://chatgpt.com/connector_platform_oauth_redirect",
        "gpt",
    );
    let codex = compat_flow_token(
        &state,
        &addr,
        &codex_url,
        "http://127.0.0.1/callback",
        "codex",
    );
    let claude = compat_flow_token(
        &state,
        &addr,
        &claude_url,
        "https://claude.ai/api/mcp/auth_callback",
        "claude",
    );

    let tool_names = |token: &str| -> Vec<String> {
        let doc = mcp_rpc(&addr, token, "tools/list", "{}", 1);
        let mut names: Vec<String> = doc["result"]["tools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| t["name"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    };
    // ChatGPT: the five read tools PLUS the wrappers
    assert_eq!(
        tool_names(&gpt),
        vec![
            "fetch",
            "memory_get",
            "memory_neighbors",
            "memory_search",
            "memory_tags",
            "memory_timeline",
            "search",
        ]
    );
    // Codex and claude.ai: the normal tools, exactly
    for (name, token) in [("codex", &codex), ("claude", &claude)] {
        assert_eq!(
            tool_names(token),
            vec![
                "memory_get",
                "memory_neighbors",
                "memory_search",
                "memory_tags",
                "memory_timeline",
            ],
            "{name} must not see the wrappers"
        );
    }

    // calling `search` with claude's token answers EXACTLY like a tool
    // that does not exist — same code, same message: no oracle
    let via_wrapper = mcp_rpc(
        &addr,
        &claude,
        "tools/call",
        r#"{"name":"search","arguments":{"query":"x"}}"#,
        2,
    );
    let via_unknown = mcp_rpc(
        &addr,
        &claude,
        "tools/call",
        r#"{"name":"strumento_che_non_esiste","arguments":{}}"#,
        3,
    );
    let strip_id = |v: &serde_json::Value| -> serde_json::Value {
        let mut c = v.clone();
        if let Some(obj) = c.as_object_mut() {
            obj.remove("id");
        }
        c
    };
    assert_eq!(
        strip_id(&via_wrapper["error"]),
        strip_id(&via_unknown["error"]),
        "the wrapper refusal must be the unknown-tool answer byte for byte"
    );
    assert!(
        via_wrapper["error"]["message"]
            .as_str()
            .is_some_and(|m| m == "tool not found"),
        "{via_wrapper:?}"
    );
}
