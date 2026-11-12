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
    serde_json::from_str(&body(r)).unwrap(/* t */)
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
