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
