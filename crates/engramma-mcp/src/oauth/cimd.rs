// SPDX-License-Identifier: Apache-2.0

//! v0.5.2 (P2): the Client ID Metadata Document fetch — the one place
//! where engramma connects OUT to an address chosen by whoever calls
//! `/authorize`. That is a textbook SSRF, and the fence is structural:
//! the DNS answer is filtered to PUBLIC addresses only before any
//! connection is made (loopback, RFC 1918, link-local — the cloud metadata
//! service lives there —, CGNAT, ULA, multicast, unspecified and
//! IPv4-mapped IPv6 are all discarded), there are no redirects, a 5 KiB
//! body cap, a 5 s budget and a JSON-only content type. The library's
//! test escape hatch (`allow_loopback`) exists because the test CIMD
//! server runs on 127.0.0.1; the CLI never sets it.

use std::io::Read as _;
use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;

use super::net::{is_public_ip, redirect_uri_ok};
use super::store::ClientRecord;

/// The fetch budget and cap, as the phase fixes them.
const MAX_BODY: u64 = 5 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
/// The cache window clamp: `Cache-Control: max-age` is honored inside
/// [5 min, 24 h]; a missing or absurd value means one hour.
const CACHE_MIN: i64 = 300;
const CACHE_MAX: i64 = 24 * 3600;
const CACHE_DEFAULT: i64 = 3600;

/// Why a CIMD fetch did not produce a client.
#[derive(Debug)]
pub enum CimdError {
    /// The client_id is not an `https://` URL (the loopback test escape
    /// also accepts `http://127.0.0.1`).
    NotAClientUrl,
    /// Transport trouble of any kind — DNS, connect, TLS, timeout. The
    /// caller answers with a GENERIC invalid_client: no details about the
    /// inside of the network go back out.
    Fetch(String),
    /// The body broke a rule (size, content type, JSON shape).
    BadDocument(String),
}

impl std::fmt::Display for CimdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CimdError::NotAClientUrl => write!(f, "the client_id is not a client metadata URL"),
            CimdError::Fetch(_) => write!(f, "the client metadata could not be fetched"),
            CimdError::BadDocument(why) => write!(f, "the client metadata is not valid: {why}"),
        }
    }
}

impl CimdError {
    /// v0.5.3 (P3.7): the LOCAL cause of a refusal — DNS, no public
    /// address, timeout, HTTP status, TLS, or the document rule it broke.
    /// [`Display`] stays generic (it is what the wire would say); this is
    /// what the event log records.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            CimdError::NotAClientUrl => self.to_string(),
            CimdError::Fetch(why) => format!("fetch: {why}"),
            CimdError::BadDocument(why) => format!("document: {why}"),
        }
    }
}

/// The knobs of the fetch. `allow_loopback` is the TEST escape hatch —
/// the library's integration tests run their CIMD server on 127.0.0.1 —
/// and is never set by the CLI.
#[derive(Debug, Clone, Copy, Default)]
pub struct CimdFetch {
    pub allow_loopback: bool,
}

/// What a good fetch produced.
#[derive(Debug)]
pub struct FetchedClient {
    pub record: ClientRecord,
    /// Until when the document may be cached (unix seconds).
    pub cache_until: i64,
}

/// Fetch and validate the CIMD document at `client_id`.
///
/// # Errors
/// [`CimdError`] — see the variant docs. Every variety answers the
/// CALLER as a generic `invalid_client`; the specifics go to the event log.
pub fn fetch(client_id: &str, mode: CimdFetch) -> Result<FetchedClient, CimdError> {
    let https = client_id.starts_with("https://");
    let loopback_http = mode.allow_loopback
        && (client_id.starts_with("http://127.0.0.1") || client_id.starts_with("http://localhost"));
    if !https && !loopback_http {
        return Err(CimdError::NotAClientUrl);
    }

    let agent = fenced_agent(mode);

    let mut response = agent
        .get(client_id)
        .call()
        .map_err(|e| CimdError::Fetch(e.to_string()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(CimdError::Fetch(format!("http {status}")));
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.starts_with("application/json") {
        return Err(CimdError::BadDocument("not application/json".into()));
    }
    let cache_control = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut body = String::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_string(&mut body)
        .map_err(|e| CimdError::Fetch(e.to_string()))?;
    if body.len() as u64 > MAX_BODY {
        return Err(CimdError::BadDocument(format!(
            "larger than {MAX_BODY} bytes"
        )));
    }
    let doc: Value = serde_json::from_str(&body)
        .map_err(|e| CimdError::BadDocument(format!("unreadable JSON: {e}")))?;

    // the document must claim its own URL
    let claimed = doc.get("client_id").and_then(Value::as_str);
    if claimed != Some(client_id) {
        return Err(CimdError::BadDocument(
            "the document's client_id is not its own URL".into(),
        ));
    }
    // the auth method: absent or `none` — or a client that ALSO speaks
    // `none` (ChatGPT's real document prefers `private_key_jwt` but lists
    // `none` among `token_endpoint_auth_methods_supported`; our metadata
    // offers only `none`, so that is what it uses). `private_key_jwt` alone
    // is not spoken in this phase, and the refusal says so.
    let also_none = doc
        .get("token_endpoint_auth_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|m| m.as_str() == Some("none")));
    match doc
        .get("token_endpoint_auth_method")
        .and_then(Value::as_str)
    {
        None | Some("none") => {}
        Some(_) if also_none => {}
        Some(other) => {
            return Err(CimdError::BadDocument(format!(
                "token_endpoint_auth_method `{other}` is not supported here (only `none`, \
                 the public client with PKCE)"
            )));
        }
    }
    let uris: Vec<String> = doc
        .get("redirect_uris")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if uris.is_empty() {
        return Err(CimdError::BadDocument("no redirect_uris".into()));
    }
    for u in &uris {
        if !redirect_uri_ok(u) {
            return Err(CimdError::BadDocument(format!(
                "redirect_uri `{u}` is not acceptable (https anywhere, or http on a loopback host)"
            )));
        }
    }
    // the name is hostile text: truncated, shown escaped everywhere
    let name = doc
        .get("client_name")
        .and_then(Value::as_str)
        .map(|n| n.chars().take(80).collect::<String>());

    let now = OffsetDateTime::now_utc().unix_timestamp();
    let max_age = cache_control
        .split(',')
        .filter_map(|p| p.trim().strip_prefix("max-age="))
        .next()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(CACHE_DEFAULT)
        .clamp(CACHE_MIN, CACHE_MAX);
    Ok(FetchedClient {
        record: ClientRecord {
            client_id: client_id.to_string(),
            kind: super::store::ClientKind::Cimd {
                cache_until: now + max_age,
            },
            redirect_uris: uris,
            name,
            created_at: now,
        },
        cache_until: now + max_age,
    })
}

/// The fetch's agent: no redirects, the global budget, no environment
/// proxy, and the public-only resolver.
fn fenced_agent(mode: CimdFetch) -> ureq::Agent {
    ureq::Agent::with_parts(
        ureq::Agent::config_builder()
            .max_redirects(0)
            // no proxy from the environment: HTTPS_PROXY would send the
            // fetch through a host the resolver fence never sees
            // (verifica v0.5.2)
            .proxy(None)
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build(),
        ureq::unversioned::transport::DefaultConnector::default(),
        PublicOnlyResolver {
            allow_loopback: mode.allow_loopback,
        },
    )
}

/// The SSRF fence: a resolver that hands ureq only PUBLIC addresses.
/// The connection goes to the verified IP while SNI, `Host` and the TLS
/// certificate check keep using the NAME — which is exactly the point.
#[derive(Debug)]
struct PublicOnlyResolver {
    allow_loopback: bool,
}

impl ureq::unversioned::resolver::Resolver for PublicOnlyResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let resolved = ureq::unversioned::resolver::DefaultResolver::default()
            .resolve(uri, config, timeout)?;
        let mut out = self.empty();
        for addr in resolved.iter() {
            let public = is_public_ip(addr.ip());
            let loopback_ok = self.allow_loopback && addr.ip().is_loopback();
            if public || loopback_ok {
                out.push(*addr);
            }
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fetch_never_goes_through_an_environment_proxy() {
        // ureq reads HTTPS_PROXY/ALL_PROXY by default: through a proxy the
        // connection goes to the PROXY's address and the resolver fence
        // judges the wrong host (verifica v0.5.2 — red when run with
        // `HTTPS_PROXY=http://10.0.0.1:3128` in the environment)
        let agent = fenced_agent(CimdFetch::default());
        assert!(agent.config().proxy().is_none());
    }

    /// A minimal CIMD server on loopback, served by a raw thread. The
    /// body's `{URL}` placeholder is replaced with the real address, so
    /// the document can claim its own URL.
    fn cimd_server(
        status: u16,
        headers: &[(&str, &str)],
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(/* test-only */);
        let port = listener.local_addr().unwrap(/* test-only */).port();
        let head = headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}\r\n"))
            .collect::<String>();
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
                        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\n{head}\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (format!("http://127.0.0.1:{port}/client.json"), handle)
    }

    #[test]
    fn a_good_document_is_validated_and_cached() {
        let (url, _h) = cimd_server(
            200,
            &[
                ("content-type", "application/json"),
                ("cache-control", "max-age=600"),
            ],
            r#"{"client_id":"{URL}","redirect_uris":["https://chatgpt.com/connector_platform_oauth_redirect"],"token_endpoint_auth_method":"none","client_name":"ChatGPT"}"#,
        );
        let fetched = fetch(
            &url,
            CimdFetch {
                allow_loopback: true,
            },
        )
        .expect("valid document");
        assert_eq!(
            fetched.record.redirect_uris,
            vec!["https://chatgpt.com/connector_platform_oauth_redirect".to_string()]
        );
        assert_eq!(fetched.record.name.as_deref(), Some("ChatGPT"));
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert!(
            (fetched.cache_until - now - 600).abs() < 30,
            "max-age=600 honored"
        );
    }

    #[test]
    fn the_document_must_claim_its_own_url() {
        let (url, _h) = cimd_server(
            200,
            &[("content-type", "application/json")],
            r#"{"client_id":"https://someone-else.example/x","redirect_uris":["https://x.example/cb"]}"#,
        );
        assert!(matches!(
            fetch(
                &url,
                CimdFetch {
                    allow_loopback: true
                }
            ),
            Err(CimdError::BadDocument(_))
        ));
    }

    #[test]
    fn private_key_jwt_is_refused_and_the_refusal_says_why() {
        let (url, _h) = cimd_server(
            200,
            &[("content-type", "application/json")],
            r#"{"client_id":"{URL}","redirect_uris":["https://x.example/cb"],"token_endpoint_auth_method":"private_key_jwt"}"#,
        );
        match fetch(
            &url,
            CimdFetch {
                allow_loopback: true,
            },
        ) {
            Err(CimdError::BadDocument(why)) => assert!(why.contains("private_key_jwt")),
            Err(CimdError::Fetch(e)) => panic!("expected a refusal, got a fetch error: {e}"),
            Err(CimdError::NotAClientUrl) => panic!("expected a refusal, got NotAClientUrl"),
            Ok(_) => panic!("expected a refusal, got a document"),
        }
    }

    #[test]
    fn a_client_that_also_speaks_none_is_accepted_as_public() {
        // ChatGPT's real document (fetched 2026-11-12 in the tunnel test):
        // the preferred method is private_key_jwt, but `none` is among the
        // supported ones — and our metadata offers only `none`, so the
        // client speaks it. Refusing it meant ChatGPT could never connect.
        let (url, _h) = cimd_server(
            200,
            &[("content-type", "application/json")],
            r#"{"client_id":"{URL}","redirect_uris":["https://chatgpt.com/connector_platform_oauth_redirect"],"token_endpoint_auth_method":"private_key_jwt","token_endpoint_auth_methods_supported":["none","private_key_jwt"],"client_name":"ChatGPT"}"#,
        );
        let got = fetch(
            &url,
            CimdFetch {
                allow_loopback: true,
            },
        );
        assert!(got.is_ok(), "{:?}", got.err().map(|e| e.to_string()));
    }

    #[test]
    fn a_document_without_redirects_or_with_bad_ones_is_refused() {
        let (url, _h) = cimd_server(
            200,
            &[("content-type", "application/json")],
            r#"{"client_id":"{URL}","redirect_uris":[]}"#,
        );
        assert!(matches!(
            fetch(
                &url,
                CimdFetch {
                    allow_loopback: true
                }
            ),
            Err(CimdError::BadDocument(_))
        ));
        let (url, _h) = cimd_server(
            200,
            &[("content-type", "application/json")],
            r#"{"client_id":"{URL}","redirect_uris":["http://evil.example/cb"]}"#,
        );
        assert!(matches!(
            fetch(
                &url,
                CimdFetch {
                    allow_loopback: true
                }
            ),
            Err(CimdError::BadDocument(_))
        ));
    }

    #[test]
    fn six_kib_is_too_much() {
        let big = format!(
            "{{\"client_id\":\"{{URL}}\",\"redirect_uris\":[\"https://x.example/cb\"],\"pad\":\"{}\"}}",
            "x".repeat(6 * 1024)
        );
        let (url, _h) = cimd_server(
            200,
            &[("content-type", "application/json")],
            Box::leak(big.into_boxed_str()),
        );
        assert!(matches!(
            fetch(
                &url,
                CimdFetch {
                    allow_loopback: true
                }
            ),
            Err(CimdError::BadDocument(_))
        ));
    }

    #[test]
    fn loopback_is_refused_without_the_test_hatch() {
        let (url, _h) = cimd_server(200, &[("content-type", "application/json")], "{}");
        // the layers stack: an http client_id is not a CIMD URL at all
        // before any connection is attempted; the resolver fence behind it
        // (public addresses only) has its own property tests
        assert!(matches!(
            fetch(&url, CimdFetch::default()),
            Err(CimdError::NotAClientUrl)
        ));
    }

    /// v0.5.3 (P3.6): the four borders of the cache window, owed by the
    /// v0.5.2 REPORT ("nel codice, senza test").
    #[test]
    fn the_cache_control_window_clamps_at_both_borders() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        // max-age=1 → the 5-minute floor
        let (url, _h) = cimd_server(
            200,
            &[
                ("content-type", "application/json"),
                ("cache-control", "max-age=1"),
            ],
            r#"{"client_id":"{URL}","redirect_uris":["https://x.example/cb"]}"#,
        );
        let f = fetch(
            &url,
            CimdFetch {
                allow_loopback: true,
            },
        )
        .expect("ok");
        assert!(
            (f.cache_until - now - 300).abs() <= 5,
            "floor: {}",
            f.cache_until - now
        );
        // max-age=999999 → the 24-hour ceiling
        let (url, _h) = cimd_server(
            200,
            &[
                ("content-type", "application/json"),
                ("cache-control", "max-age=999999"),
            ],
            r#"{"client_id":"{URL}","redirect_uris":["https://x.example/cb"]}"#,
        );
        let f = fetch(
            &url,
            CimdFetch {
                allow_loopback: true,
            },
        )
        .expect("ok");
        assert!(
            (f.cache_until - now - 86_400).abs() <= 5,
            "ceiling: {}",
            f.cache_until - now
        );
        // absent → one hour
        let (url, _h) = cimd_server(
            200,
            &[("content-type", "application/json")],
            r#"{"client_id":"{URL}","redirect_uris":["https://x.example/cb"]}"#,
        );
        let f = fetch(
            &url,
            CimdFetch {
                allow_loopback: true,
            },
        )
        .expect("ok");
        assert!(
            (f.cache_until - now - 3_600).abs() <= 5,
            "default: {}",
            f.cache_until - now
        );
        // non-numeric → one hour, not an error
        let (url, _h) = cimd_server(
            200,
            &[
                ("content-type", "application/json"),
                ("cache-control", "max-age=presto"),
            ],
            r#"{"client_id":"{URL}","redirect_uris":["https://x.example/cb"]}"#,
        );
        let f = fetch(
            &url,
            CimdFetch {
                allow_loopback: true,
            },
        )
        .expect("ok");
        assert!(
            (f.cache_until - now - 3_600).abs() <= 5,
            "garbage: {}",
            f.cache_until - now
        );
    }

    #[test]
    fn a_plain_http_client_id_is_not_a_cimd_url() {
        assert!(matches!(
            fetch("http://127.0.0.1:9/client.json", CimdFetch::default()),
            Err(CimdError::NotAClientUrl)
        ));
        assert!(matches!(
            fetch("not-a-url", CimdFetch::default()),
            Err(CimdError::NotAClientUrl)
        ));
    }
}
