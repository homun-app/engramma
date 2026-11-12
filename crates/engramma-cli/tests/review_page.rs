// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P5): the local review page — every security point with its test,
//! then the human flows. The server is the real binary, spoken to over
//! loopback HTTP; the hostile-Host case uses raw TCP (a client library may
//! refuse to send a lying Host header, which is exactly the point).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};

use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, ProposalInput, ProposalMode, ProposeContext,
};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};

fn note(body: &str) -> Note {
    let t = At(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */));
    Note {
        id: engramma_core::id::new_ulid().unwrap(/* t */),
        note_type: NoteType::Fact,
        title: None,
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "mcp:claude".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: t,
        updated_at: t,
        extensions: vec![],
    }
}

struct Server {
    child: Child,
    url: String,
    token: String,
    port: u16,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The no-redirect agent every raw assertion shares.
fn still() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        // 4xx/5xx arrive as plain responses: every guard has its own test
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn start() -> Server {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap(/* t */);
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&root)
        .args(["init"])
        .output()
        .unwrap(/* t */);
    assert!(out.status.success());
    let mut child = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&root)
        .args(["review"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(/* t */);
    // the first line carries the address with the token
    let line = {
        let stdout = child.stdout.as_mut().unwrap(/* t */);
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap(/* t */);
        line
    };
    let url = line
        .trim()
        .strip_prefix("revisione: ")
        .unwrap_or_else(|| panic!("unexpected banner: {line}"))
        .to_string();
    let token = url.split("token=").nth(1).unwrap_or_default().to_string();
    let port: u16 = url
        .trim_start_matches("http://127.0.0.1:")
        .split('/')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_default();
    Server {
        child,
        url,
        token,
        port,
        dir,
        root,
    }
}

impl Server {
    fn cell(&self) -> Cell {
        Cell::open(&self.root.join("cells/default.sqlite")).unwrap(/* t */)
    }

    fn propose(&self, body: &str) -> ulid::Ulid {
        let cell = self.cell();
        cell.set_policy(serde_json::json!({ "write": "propose" }), "test")
            .unwrap(/* t */);
        let out = cell
            .propose(
                &ProposalInput {
                    note: note(body),
                    mode: ProposalMode::New,
                    client: Some("claude".into()),
                    reason: Some("l'utente l'ha detto".into()),
                },
                "mcp:claude",
                &ProposeContext::default(),
            )
            .unwrap(/* t */);
        out.id.unwrap(/* t */)
    }

    /// A client that already entered: the cookie travels on every request
    /// by hand (no reliance on the client library's cookie behavior).
    fn client(&self) -> Client {
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .new_agent();
        let r = agent
            .get(&format!("http://127.0.0.1:{}/entra?token={}", self.port, self.token))
            .call()
            .unwrap(/* t */);
        assert_eq!(r.status().as_u16(), 303);
        Client {
            agent,
            cookie: format!("engramma_review={}", self.token),
            port: self.port,
        }
    }
}

fn csrf_of_form(html: &str, action: &str) -> String {
    let form = html
        .split("<form")
        .find(|f| f.contains(action))
        .unwrap_or_default();
    form.split("name=\"csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_default()
        .to_string()
}

/// One raw HTTP exchange over TCP, for headers a client would not send.
fn raw(port: u16, request: &str) -> (u16, Vec<String>, String) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap(/* t */);
    stream.write_all(request.as_bytes()).unwrap(/* t */);
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let headers: Vec<String> = head.lines().skip(1).map(str::to_string).collect();
    (status, headers, body.to_string())
}

// ---------------------------------------------------------------------------
// the security contract, point by point
// ---------------------------------------------------------------------------

#[test]
fn the_address_is_loopback_and_the_token_gates_everything() {
    let s = start();
    assert!(s.url.starts_with("http://127.0.0.1:"), "{}", s.url);
    assert_eq!(s.token.len(), 64, "256 random bits, hex");

    // without the cookie: 401, on every page
    for path in ["/coda", "/note", "/decisioni", "/politica"] {
        let r = still()
            .get(&format!("http://127.0.0.1:{}{path}", s.port))
            .call()
            .unwrap(/* t */);
        assert_eq!(r.status().as_u16(), 401, "{path}");
    }
    // a wrong token does not enter
    let r = still()
        .get(&format!(
            "http://127.0.0.1:{}/entra?token={}",
            s.port,
            "0".repeat(64)
        ))
        .call()
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 401);

    // the right token: 303 to a clean address, HttpOnly SameSite=Strict cookie
    let r = still()
        .get(&format!("http://127.0.0.1:{}/entra?token={}", s.port, s.token))
        .call()
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    assert_eq!(
        r.headers().get("set-cookie").and_then(|v| v.to_str().ok()),
        Some(
            format!(
                "engramma_review={}; Path=/; HttpOnly; SameSite=Strict",
                s.token
            )
            .as_str()
        )
    );
    assert_eq!(
        r.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/coda")
    );

    // with the cookie, the pages answer
    let client = s.client();
    let r = client.get("/coda").unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 200);
}

#[test]
fn a_lying_host_is_refused() {
    let s = start();
    let _client = s.client();
    // DNS rebinding: the browser sends the attacker's own Host
    let (status, _, _) = raw(
        s.port,
        &format!(
            "GET /coda HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n",
            port = s.port
        ),
    );
    assert_eq!(status, 403, "the Host must be the loopback one");
    // with the cookie but the wrong host: still 403 (checked before the cookie)
    let cookie = format!("engramma_review={}", s.token);
    let (status, _, _) = raw(
        s.port,
        &format!(
            "GET /coda HTTP/1.1\r\nHost: evil.example:{port}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n",
            port = s.port
        ),
    );
    assert_eq!(status, 403);
}

#[test]
fn every_post_wants_the_origin_and_the_csrf_token() {
    let s = start();
    let id = s.propose("Fabio va in barca a vela.");
    let client = s.client();

    // scrape the REAL token of the form under test (every form has its own)
    let queue_html = client.body("/coda");
    let csrf = csrf_of_form(&queue_html, &format!("/coda/{id}/rifiuta"));
    assert!(!csrf.is_empty(), "the form carries its token");

    let no_origin = format!("http://127.0.0.1:{}/coda/{id}/rifiuta", s.port);
    let bad_csrf = "0".repeat(32);
    let cases: [(&str, &str, &str, u16, &str); 4] = [
        // (origin, csrf, motivo, expected, why)
        ("", csrf.as_str(), "no", 403, "without Origin"),
        (
            "https://evil.example",
            csrf.as_str(),
            "no",
            403,
            "foreign Origin",
        ),
        ("mine", bad_csrf.as_str(), "no", 403, "forged CSRF"),
        (
            "mine",
            csrf.as_str(),
            "non è vero",
            303,
            "the page's own token",
        ),
    ];
    for (origin, token, motivo, want, why) in cases {
        let mut req = client
            .agent
            .post(&no_origin)
            .header("Cookie", &client.cookie);
        req = if origin.is_empty() {
            req
        } else if origin == "mine" {
            req.header("Origin", format!("http://127.0.0.1:{}", s.port))
        } else {
            req.header("Origin", origin)
        };
        let r = req.send_form(vec![("csrf", token), ("motivo", motivo)]).unwrap(/* t */);
        assert_eq!(r.status().as_u16(), want, "{why}");
    }
    assert_eq!(s.cell().get(&id).unwrap(/* t */).status, Status::Deleted);
}

#[test]
fn the_headers_close_the_page() {
    let s = start();
    let client = s.client();
    for path in ["/coda", "/note", "/decisioni", "/politica", "/stile.css"] {
        let r = client.get(path).unwrap(/* t */);
        assert!(
            r.headers().contains_key("content-security-policy"),
            "{path}"
        );
        let csp = r
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(csp.contains("default-src 'none'"), "{path}: {csp}");
        assert!(csp.contains("style-src 'self'"), "{path}: {csp}");
        assert!(csp.contains("form-action 'self'"), "{path}: {csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
        assert!(csp.contains("base-uri 'none'"), "{path}: {csp}");
        assert!(
            r.headers()
                .get("x-content-type-options")
                .is_some_and(|v| v == "nosniff")
        );
        assert!(
            r.headers()
                .get("referrer-policy")
                .is_some_and(|v| v == "no-referrer")
        );
    }
}

#[test]
fn hostile_proposal_text_is_shown_as_text() {
    let s = start();
    let hostile = "<script>alert(1)</script> e \"'><img src=x onerror=alert(2)> e \
                   [click](javascript:alert(3)) e Fabio nuota.";
    let id = s.propose(hostile);
    let client = s.client();
    let html = client.body("/coda");
    assert!(
        html.contains("&lt;script&gt;"),
        "escaped: no raw script tag"
    );
    assert!(!html.contains("<script>alert"), "never a live tag");
    assert!(!html.contains("<img src=x"), "never a live img");
    assert!(!html.contains("href=\"javascript:"), "never a live link");
    // the text is all there, readable
    assert!(html.contains("alert(3)"), "the content is visible as text");
    // the note page escapes too
    let html = client.body(&format!("/note/{id}"));
    assert!(html.contains("&lt;script&gt;"));
}

// ---------------------------------------------------------------------------
// the human flows
// ---------------------------------------------------------------------------

/// An entered client: the agent plus the session cookie, with the
/// origin-ready GET/POST helpers the flows need.
struct Client {
    agent: ureq::Agent,
    cookie: String,
    port: u16,
}

impl Client {
    fn get(&self, path: &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        self.agent
            .get(&format!("http://127.0.0.1:{}{path}", self.port))
            .header("Cookie", &self.cookie)
            .call()
    }
    fn post(
        &self,
        path: &str,
        form: &[(&str, &str)],
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        self.agent
            .post(&format!("http://127.0.0.1:{}{path}", self.port))
            .header("Cookie", &self.cookie)
            .header("Origin", &format!("http://127.0.0.1:{}", self.port))
            .send_form(form.to_vec())
    }
    fn body(&self, path: &str) -> String {
        let mut r = self.get(path).unwrap(/* t */);
        r.body_mut().read_to_string().unwrap(/* t */)
    }
}

#[test]
fn the_queue_flows_accept_with_edit_and_reject() {
    let s = start();
    let a = s.propose("Fabio sciaga.");
    let b = s.propose("Fabio vende la casa.");
    let client = s.client();

    // accept with an edited text
    let html = client.body("/coda");
    let csrf = csrf_of_form(&html, &format!("/coda/{a}/accetta"));
    let r = client
        .post(
            &format!("/coda/{a}/accetta"),
            &[("csrf", csrf.as_str()), ("testo", "Fabio sciaga in Dolomiti.")],
        )
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    let n = s.cell().get(&a).unwrap(/* t */);
    assert_eq!(n.status, Status::Active);
    assert_eq!(n.body, "Fabio sciaga in Dolomiti.");

    // reject keeps the text under the lapide, and the queue empties
    let html = client.body("/coda");
    let csrf = csrf_of_form(&html, &format!("/coda/{b}/rifiuta"));
    let r = client
        .post(
            &format!("/coda/{b}/rifiuta"),
            &[("csrf", csrf.as_str()), ("motivo", "falso")],
        )
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    let n = s.cell().get(&b).unwrap(/* t */);
    assert_eq!(n.status, Status::Deleted);
    assert_eq!(n.body, "Fabio vende la casa.");
}

#[test]
fn the_decisions_page_reverts_single_and_bulk() {
    let s = start();
    let a = s.propose("Fabio parla inglese.");
    let b = s.propose("Fabio parla francese.");
    let cell = s.cell();
    for id in [&a, &b] {
        cell.decide(
            id,
            &DecisionInput {
                action: DecisionAction::Accept { edited: None },
                reason: Some("ok".into()),
                review: None,
            },
            "reviewer:bravo",
        )
        .unwrap(/* t */);
    }
    let client = s.client();

    // the registry names the reviewer and the metadata
    let html = client.body("/decisioni");
    assert!(html.contains("reviewer:bravo"), "{html}");

    // the single revert (the route takes the DECISION op id)
    let decision_a = cell
        .decisions(&Default::default())
        .unwrap(/* t */)
        .iter()
        .find(|d| d.proposal == a)
        .unwrap(/* t */)
        .op_id
        .to_string();
    let csrf = csrf_of_form(&html, &format!("/decisioni/annulla/{decision_a}"));
    let r = client
        .post(
            &format!("/decisioni/annulla/{decision_a}"),
            &[("csrf", csrf.as_str()), ("", "")],
        )
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    assert_eq!(cell.get(&a).unwrap(/* t */).status, Status::Quarantined);

    // the bulk: preview first (dry), then the confirmation applies
    let html = client.body("/decisioni");
    let csrf = csrf_of_form(&html, "/decisioni/blocco");
    // step 1: the preview lands on the decisions page as an anteprima
    let r = client
        .post(
            "/decisioni/blocco",
            &[("csrf", csrf.as_str()), ("by", "reviewer:bravo"), ("dal", ""), ("al", "")],
        )
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    let to = r
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let html = client.body(&to); // the location is already percent-encoded
    assert!(html.contains("Anteprima"), "{}", urldecode(&html));
    assert!(html.contains("Confermo"), "{html}");
    // nothing applied yet
    assert_eq!(cell.get(&b).unwrap(/* t */).status, Status::Active);
    // step 2: the confirmation (its form is the one carrying `conferma`)
    let csrf = html
        .split("<form")
        .find(|f| f.contains("/decisioni/blocco") && f.contains("conferma"))
        .and_then(|f| {
            f.split("name=\"csrf\" value=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
        })
        .unwrap_or_default()
        .to_string();
    assert!(!csrf.is_empty(), "the confirmation form carries its token");
    let r = client
        .post(
            "/decisioni/blocco",
            &[
                ("csrf", csrf.as_str()),
                ("by", "reviewer:bravo"),
                ("dal", ""),
                ("al", ""),
                ("conferma", "1"),
            ],
        )
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    assert_eq!(cell.get(&b).unwrap(/* t */).status, Status::Quarantined);
}

fn urldecode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or_default();
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[test]
fn the_note_page_shows_history_diff_and_restores() {
    let s = start();
    let id = s.propose("Fabio corre la maratona.");
    let cell = s.cell();
    cell.decide(
        &id,
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: None,
            review: None,
        },
        "user@review",
    )
    .unwrap(/* t */);
    cell.patch_note(
        &id,
        &engramma_core::NotePatch {
            body: Some("Fabio corre la mezza maratona.".into()),
            ..Default::default()
        },
        "cli",
    )
    .unwrap(/* t */);

    let client = s.client();
    let html = client.body(&format!("/note/{id}"));
    // the story and the diff
    assert!(html.contains("proposta da mcp:claude"), "{html}");
    assert!(html.contains("decisa da"), "{html}");
    assert!(html.contains("diff-del"), "{html}");
    assert!(html.contains("Fabio corre la maratona."), "{html}");
    assert!(html.contains("Fabio corre la mezza maratona."), "{html}");

    // restore the first version
    let csrf = csrf_of_form(&html, &format!("/note/{id}/ripristina"));
    // the first restore form on the page belongs to v1
    let version = html
        .split("<h3>v")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default()
        .to_string();
    let r = client
        .post(
            &format!("/note/{id}/ripristina"),
            &[("csrf", csrf.as_str()), ("versione", version.as_str())],
        )
        .unwrap(/* t */);
    assert_eq!(r.status().as_u16(), 303);
    assert_eq!(
        cell.get(&id).unwrap(/* t */).body,
        "Fabio corre la maratona."
    );
}

#[test]
fn the_policy_page_is_read_only() {
    let s = start();
    s.cell()
        .set_policy(serde_json::json!({ "write": "propose" }), "test")
        .unwrap(/* t */);
    let client = s.client();
    let html = client.body("/politica");
    assert!(html.contains("write"), "{html}");
    assert!(html.contains("sola lettura"), "{html}");
    assert!(!html.contains("<form"), "no forms on the policy page");
    // and there is no POST route to it
    let r = client.post("/politica", &[("write", "auto")]).unwrap(/* t */);
    let code = r.status().as_u16();
    assert!(
        code == 404 || code == 405,
        "no route listens for a policy POST: got {code}"
    );
}
