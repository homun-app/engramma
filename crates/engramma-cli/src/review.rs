// SPDX-License-Identifier: Apache-2.0

//! `engramma review` (v0.5, P5): the local review page. A server-rendered
//! site with ZERO JavaScript — forms and redirects only — bound to
//! 127.0.0.1, guarded by a random 256-bit token exchanged for an HttpOnly
//! cookie. Every POST checks the Origin and a per-form CSRF token; every
//! response carries a closed CSP; every piece of note text is escaped,
//! because proposals come from models and their text is hostile. The actor
//! of every decision taken here is `user@review`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use ulid::Ulid;

use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, DecisionsFilter, ProposalMode, ProposalsFilter, RevertFilter,
    RevertOutcome,
};
use engramma_core::search::Query as SearchQuery;
use engramma_core::{Cell, CellError, Status};

/// The shared state of the server.
struct Review {
    cell: Cell,
    /// The 256-bit session token: printed once in the entry URL, then
    /// exchanged for the cookie. Never in a page, never in a form.
    token: String,
    /// The port we actually bound (the Host check needs it).
    port: u16,
    /// v0.6.19 (the site capture, S13): what the decisions page shows
    /// ONCE after a POST — the preview of a bulk revert, or how a revert
    /// went. It is HTML this server built itself, every dynamic value
    /// escaped where it was built: the page renders nothing that arrived
    /// in a URL.
    flash: std::sync::Mutex<Option<String>>,
}

/// One query string.
type Q = HashMap<String, String>;

/// HTML-escape EVERYTHING that comes from a note (or anywhere else): the
/// proposals arrive from models, their text is hostile data.
fn esc(s: &str) -> String {
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

/// The per-form CSRF token: derived from the session token and the form's
/// name — unguessable without the cookie, different on every form.
fn csrf_of(review: &Review, form: &str) -> String {
    blake3::hash(format!("{}:csrf:{form}", review.token).as_bytes())
        .to_hex()
        .to_string()[..32]
        .to_string()
}

/// v0.5.2 (P0): the cookie's NAME carries the port. Two `engramma review`
/// servers open together (or a leftover cookie in the browser) must never
/// exchange their session tokens: each port gets its own name.
fn cookie_name(port: u16) -> String {
    format!("engramma_review_{port}")
}

/// v0.5.2 (P0): constant-time equality of two secrets — blake3 of both,
/// then the fixed 256-bit digests compared. A plain `==` on the strings
/// would leak the token's length and prefix byte by byte; everything born
/// in this phase (the OAuth tokens too) compares secrets this way.
fn same_secret(a: &str, b: &str) -> bool {
    blake3::hash(a.as_bytes()) == blake3::hash(b.as_bytes())
}

fn page(title: &str, body: &str) -> Response {
    Html(format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{}</title><link rel=\"stylesheet\" href=\"/style.css\"></head>\
<body><nav><a href=\"/queue\">Queue</a> <a href=\"/notes\">Notes</a> \
<a href=\"/decisions\">Decisions</a> <a href=\"/policy\">Policy</a></nav>\
<main>{body}</main></body></html>",
        esc(title)
    ))
    .into_response()
}

fn see_other(to: &str) -> Response {
    Redirect::to(to).into_response()
}

fn forbidden(why: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Html(format!(
            "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
<title>403</title><body><h1>403</h1><p>{}</p></body></html>",
            esc(why)
        )),
    )
        .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Html(
            "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
<title>401</title><body><h1>401</h1><p>the token is needed: start again from the command \
that printed the address</p></body></html>"
                .to_string(),
        ),
    )
        .into_response()
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(format!(
            "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
<title>404</title><body><h1>404</h1><p>{}</p></body></html>",
            esc(what)
        )),
    )
        .into_response()
}

fn engine_error(e: CellError) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html(format!(
            "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
<title>error</title><body><h1>engine error</h1><pre>{}</pre></body></html>",
            esc(&e.to_string())
        )),
    )
        .into_response()
}

impl Review {
    /// The hard security wrapper of every request: Host, token, and — for
    /// POST — Origin and CSRF. Returns Err(response) to answer with.
    fn guard(
        &self,
        method_post: bool,
        headers: &HeaderMap,
        cookies: Option<&str>,
        form_csrf: Option<&str>,
        form_name: &str,
    ) -> Result<(), std::boxed::Box<Response>> {
        // 1. DNS rebinding: only our own loopback host is acceptable
        let host_ok = headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| {
                h == format!("127.0.0.1:{}", self.port) || h == format!("localhost:{}", self.port)
            });
        if !host_ok {
            return Err(Box::new(forbidden("Unacceptable Host")));
        }
        // 2. the session cookie
        let Some(cookies) = cookies else {
            return Err(Box::new(unauthorized()));
        };
        let mine = cookies
            .split(';')
            .filter_map(|c| {
                c.trim()
                    .strip_prefix(&format!("{}=", cookie_name(self.port)))
            })
            .next()
            .is_some_and(|t| same_secret(t, &self.token));
        if !mine {
            return Err(Box::new(unauthorized()));
        }
        // 3. every POST: same-origin AND the form's own CSRF token
        if method_post {
            let origin_ok = headers
                .get("origin")
                .and_then(|o| o.to_str().ok())
                .is_some_and(|o| {
                    o == format!("http://127.0.0.1:{}", self.port)
                        || o == format!("http://localhost:{}", self.port)
                });
            if !origin_ok {
                return Err(Box::new(forbidden("Missing or foreign Origin")));
            }
            if form_csrf != Some(csrf_of(self, form_name).as_str()) {
                return Err(Box::new(forbidden("Invalid CSRF token")));
            }
        }
        Ok(())
    }

    /// Leave a block for the next decisions page: the one the POST that
    /// carries it redirects to.
    fn flash(&self, html: String) {
        if let Ok(mut slot) = self.flash.lock() {
            *slot = Some(html);
        }
    }

    /// The block, once: a reload shows the plain page.
    fn take_flash(&self) -> Option<String> {
        self.flash.lock().ok().and_then(|mut slot| slot.take())
    }
}

// ---------------------------------------------------------------------------
// the pages
// ---------------------------------------------------------------------------

async fn style() -> Response {
    let css = "body{font-family:system-ui,sans-serif;margin:0;color:#1a1a1a}\
nav{background:#222;color:#fff;padding:.6rem 1rem;display:flex;gap:1rem}\
nav a{color:#fff;text-decoration:none}\
main{max-width:46rem;margin:1rem auto;padding:0 1rem}\
.proposal{border:1px solid #ccc;border-radius:8px;padding:1rem;margin:1rem 0}\
.meta{color:#555;font-size:.9rem}\
pre.body{white-space:pre-wrap;background:#f6f6f6;padding:.7rem;border-radius:6px;overflow-wrap:anywhere}\
form{display:inline-block}\
textarea{width:100%;min-height:5rem;font:inherit}\
button{font:inherit;padding:.35rem .9rem;border-radius:6px;border:1px solid #999;cursor:pointer}\
button.ok{background:#0a7d32;color:#fff}button.no{background:#b3261e;color:#fff}\
table{border-collapse:collapse;width:100%}td,th{border-bottom:1px solid #ddd;padding:.4rem;text-align:left}\
.diff-del{color:#b3261e}.diff-add{color:#0a7d32}\
.badge{display:inline-block;padding:.1rem .5rem;border-radius:99px;background:#eee;font-size:.8rem}";
    (
        [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
        css.to_string(),
    )
        .into_response()
}

/// The entry: the token lands in the URL exactly once, is exchanged for
/// the cookie, and the address is rewritten clean.
async fn enter(State(review): State<Arc<Review>>, Query(q): Query<Q>) -> Response {
    let Some(token) = q.get("token") else {
        return unauthorized();
    };
    if !same_secret(token, &review.token) {
        return unauthorized();
    }
    let mut r = see_other("/queue");
    r.headers_mut().append(
        axum::http::header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{}={token}; Path=/; HttpOnly; SameSite=Strict",
            cookie_name(review.port)
        ))
        .unwrap_or(HeaderValue::from_static("")),
    );
    r
}

async fn queue(State(review): State<Arc<Review>>, headers: HeaderMap) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    if let Err(r) = review.guard(false, &headers, cookies, None, "") {
        return *r;
    }
    let rows = match review.cell.proposals(&ProposalsFilter::default()) {
        Ok(r) => r,
        Err(e) => return engine_error(e),
    };
    let mut body = String::from("<h1>Queue</h1>");
    let queue: Vec<_> = rows.iter().filter(|r| r.state.in_queue()).collect();
    if queue.is_empty() {
        body.push_str("<p>The queue is empty.</p>");
    }
    for r in &queue {
        let id = r.id.to_string();
        let mut card = format!(
            "<section class=\"proposal\"><h2>{}</h2><pre class=\"body\">{}</pre>\
<p class=\"meta\">proposed by {} at {} — {}",
            esc(r.note.display_title()),
            esc(&r.note.body),
            esc(&r.proposer),
            esc(&r.proposed_at),
            esc(r.reason.as_deref().unwrap_or("no reason given"))
        );
        if let ProposalMode::Supersede(t) = &r.mode {
            card.push_str(&format!(
                " · correction of <a href=\"/notes/{}\">{}</a>",
                t,
                esc(&engramma_core::id::id8(t))
            ));
        }
        if let Some(c) = &r.client {
            card.push_str(&format!(" · declared client: {}", esc(c)));
        }
        card.push_str("</p>");
        if r.auto_failed {
            card.push_str("<p class=\"meta\">automatic review failed: your call</p>");
        }
        // accept (possibly with an edited text), reject
        card.push_str(&format!(
            "<form method=\"post\" action=\"/queue/{id}/accept\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<textarea name=\"text\" placeholder=\"edit the text before accepting (optional)\">{}</textarea>\
<button class=\"ok\">Accept</button></form> ",
            csrf_of(&review, &format!("accept-{id}")),
            esc(&r.note.body)
        ));
        card.push_str(&format!(
            "<form method=\"post\" action=\"/queue/{id}/reject\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<input name=\"reason\" placeholder=\"why (optional)\">\
<button class=\"no\">Reject</button></form>",
            csrf_of(&review, &format!("reject-{id}")),
        ));
        card.push_str("</section>");
        body.push_str(&card);
    }
    if rows.len() > queue.len() {
        body.push_str(&format!(
            "<p class=\"meta\">{} proposals already decided (see Decisions).</p>",
            rows.len() - queue.len()
        ));
    }
    page("Queue", &body)
}

#[derive(serde::Deserialize)]
struct CsrfForm {
    csrf: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    reason: String,
}

async fn accept(
    State(review): State<Arc<Review>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    let form_name = format!("accept-{id}");
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), &form_name) {
        return *r;
    }
    let Ok(note_id) = Ulid::from_string(&id) else {
        return not_found("no such proposal");
    };
    // accept-with-edit when the textarea came back different
    let edited = if !form.text.trim().is_empty() {
        let mut current = match review.cell.get(&note_id) {
            Ok(n) => n,
            Err(e) => return engine_error(e),
        };
        if form.text != current.body {
            current.body = form.text;
            Some(current)
        } else {
            None
        }
    } else {
        None
    };
    let outcome = review.cell.decide(
        &note_id,
        &DecisionInput {
            action: DecisionAction::Accept { edited },
            reason: Some("accepted from the review page".into()),
            review: None,
        },
        "user@review",
    );
    match outcome {
        Ok(_) => see_other("/queue"),
        Err(e) => engine_error(e),
    }
}

async fn reject(
    State(review): State<Arc<Review>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    let form_name = format!("reject-{id}");
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), &form_name) {
        return *r;
    }
    let Ok(note_id) = Ulid::from_string(&id) else {
        return not_found("no such proposal");
    };
    let outcome = review.cell.decide(
        &note_id,
        &DecisionInput {
            action: DecisionAction::Reject,
            reason: (!form.reason.is_empty()).then(|| form.reason.clone()),
            review: None,
        },
        "user@review",
    );
    match outcome {
        Ok(_) => see_other("/queue"),
        Err(e) => engine_error(e),
    }
}

async fn notes(
    State(review): State<Arc<Review>>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    if let Err(r) = review.guard(false, &headers, cookies, None, "") {
        return *r;
    }
    let mut body = String::from(
        "<h1>Notes</h1>\
<form method=\"get\" action=\"/notes\">\
<input name=\"q\" placeholder=\"search the text\" value=\"{}\">\
<button>Search</button></form>",
    );
    let query = q.get("q").cloned().unwrap_or_default();
    body = body.replacen("{}", &esc(&query), 1);
    if query.is_empty() {
        let ids = match review.cell.note_ids() {
            Ok(i) => i,
            Err(e) => return engine_error(e),
        };
        body.push_str(&format!("<p class=\"meta\">{} living notes</p>", ids.len()));
    } else {
        let hits = review
            .cell
            .search(&SearchQuery::text(&query).k(50))
            .map_err(|e| forbidden(&e.to_string()))
            .unwrap_or_default();
        body.push_str("<ul>");
        for h in &hits {
            body.push_str(&format!(
                "<li><a href=\"/notes/{}\">{}</a> <span class=\"meta\">{}</span></li>",
                h.note.id,
                esc(h.note.display_title()),
                esc(h.note.status.as_str())
            ));
        }
        body.push_str("</ul>");
    }
    page("Notes", &body)
}

async fn note_detail(
    State(review): State<Arc<Review>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    if let Err(r) = review.guard(false, &headers, cookies, None, "") {
        return *r;
    }
    let note_id = review
        .cell
        .resolve_note_id(&id)
        .or_else(|_| Ulid::from_string(&id))
        .ok();
    let Some(note_id) = note_id else {
        return not_found("no such note");
    };
    let note = match review.cell.get(&note_id) {
        Ok(n) => n,
        Err(_) => return not_found("no such note"),
    };
    let hist = match review.cell.history(&note_id) {
        Ok(h) => h,
        Err(e) => return engine_error(e),
    };
    let all_ops = match review.cell.ops() {
        Ok(o) => o,
        Err(e) => return engine_error(e),
    };
    let ops: std::collections::HashMap<String, String> = all_ops
        .iter()
        .map(|o| {
            (
                o.op_id.to_string(),
                format!("{} · {}", o.kind.as_str(), o.actor),
            )
        })
        .collect();
    let proposal = review
        .cell
        .proposals(&ProposalsFilter::default())
        .ok()
        .and_then(|rows| rows.into_iter().find(|r| r.id == note_id));

    let mut body = format!(
        "<h1>{}</h1><p class=\"meta\">{} · {} · {}",
        esc(note.display_title()),
        esc(note.note_type.as_str()),
        esc(note.confidence.as_str()),
        note.status.as_str()
    );
    if let Some(p) = &proposal {
        body.push_str(&format!(
            " · <span class=\"badge\">proposal {}</span>",
            p.state.as_str()
        ));
    }
    body.push_str("</p>");
    body.push_str(&format!("<pre class=\"body\">{}</pre>", esc(&note.body)));
    if matches!(note.status, Status::Quarantined) {
        body.push_str("<p class=\"meta\">in quarantine: invisible to search</p>");
    }
    if let Some(p) = &proposal {
        body.push_str(&format!(
            "<p class=\"meta\">proposed by {} at {} — {}</p>",
            esc(&p.proposer),
            esc(&p.proposed_at),
            esc(p.reason.as_deref().unwrap_or("no reason given"))
        ));
        if let (Some(who), Some(at)) = (&p.decided_by, &p.decided_at) {
            body.push_str(&format!(
                "<p class=\"meta\">decided by {} at {}</p>",
                esc(who),
                esc(at)
            ));
        }
    }
    // the versions, with the diff against the previous one and the restore
    body.push_str("<h2>Versions</h2>");
    if hist.is_empty() {
        body.push_str("<p class=\"meta\">no earlier version</p>");
    }
    let current = note.body.clone();
    let mut prev_body: Option<String> = None;
    for v in &hist {
        let who = ops.get(&v.op_id.to_string()).cloned().unwrap_or_default();
        body.push_str(&format!(
            "<h3>v{} <span class=\"meta\">{}</span></h3>",
            v.version,
            esc(&who)
        ));
        if let Some(prev) = &prev_body {
            for line in super::line_diff(prev, &v.note.body) {
                let (class, text) = if let Some(t) = line.strip_prefix("- ") {
                    ("diff-del", t)
                } else if let Some(t) = line.strip_prefix("+ ") {
                    ("diff-add", t)
                } else {
                    ("", line.as_str())
                };
                body.push_str(&format!("<div class=\"{class}\">{}</div>", esc(text)));
            }
        }
        prev_body = Some(v.note.body.clone());
        let restore_action = format!("/notes/{}/restore", note_id);
        body.push_str(&format!(
            "<form method=\"post\" action=\"{restore_action}\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<input type=\"hidden\" name=\"version\" value=\"{}\">\
<button>Restore this version</button></form>",
            csrf_of(&review, &format!("restore-{}-{restore_action}", v.version)),
            v.version
        ));
    }
    if let Some(prev) = prev_body
        && prev != current
    {
        body.push_str("<h3>now</h3>");
        for line in super::line_diff(&prev, &current) {
            let (class, text) = if let Some(t) = line.strip_prefix("- ") {
                ("diff-del", t)
            } else if let Some(t) = line.strip_prefix("+ ") {
                ("diff-add", t)
            } else {
                ("", line.as_str())
            };
            body.push_str(&format!("<div class=\"{class}\">{}</div>", esc(text)));
        }
    }
    page(note.display_title(), &body)
}

#[derive(serde::Deserialize)]
struct RestoreForm {
    csrf: String,
    version: i64,
}

async fn restore(
    State(review): State<Arc<Review>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<RestoreForm>,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    let form_name = format!("restore-{}-/notes/{}/restore", form.version, id);
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), &form_name) {
        return *r;
    }
    let note_id = review
        .cell
        .resolve_note_id(&id)
        .or_else(|_| Ulid::from_string(&id))
        .ok();
    let Some(note_id) = note_id else {
        return not_found("no such note");
    };
    match review.cell.restore(&note_id, form.version, "user@review") {
        Ok(()) => see_other(&format!("/notes/{note_id}")),
        Err(e) => engine_error(e),
    }
}

async fn decisions(
    State(review): State<Arc<Review>>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    if let Err(r) = review.guard(false, &headers, cookies, None, "") {
        return *r;
    }
    let rows = match review.cell.decisions(&DecisionsFilter::default()) {
        Ok(r) => r,
        Err(e) => return engine_error(e),
    };
    let mut body = String::from(
        "<h1>Decisions</h1><table><tr>\
<th>when</th><th>who</th><th>what</th><th>why</th><th></th></tr>",
    );
    for r in &rows {
        let op8 = engramma_core::id::id8(&r.op_id);
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{} {} <a href=\"/notes/{}\">{}</a></td><td>{}</td>",
            esc(&r.decided_at),
            esc(&r.actor),
            if r.decision == "accept" {
                "accept"
            } else {
                "reject"
            },
            if r.reverted_by.is_some() {
                "<span class=\"badge\">reverted</span>"
            } else {
                ""
            },
            r.proposal,
            op8,
            esc(r.reason.as_deref().unwrap_or(""))
        ));
        if let Some(review_json) = &r.review {
            body.push_str(&format!(
                "<div class=\"meta\">reviewer {} · model {} · confidence {} · prompt {}</div>",
                esc(review_json["name"].as_str().unwrap_or("?")),
                esc(review_json["model"].as_str().unwrap_or("?")),
                esc(&review_json["confidence"].to_string()),
                esc(&review_json["prompt_hash"].as_str().unwrap_or("?")[..8])
            ));
        }
        if r.reverted_by.is_none() {
            body.push_str(&format!(
                "<td><form method=\"post\" action=\"/decisions/revert/{}\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<button>Revert</button></form></td>",
                r.op_id,
                csrf_of(&review, &format!("revert-{}", r.op_id))
            ));
        } else {
            body.push_str("<td></td>");
        }
        body.push_str("</tr>");
    }
    body.push_str("</table>");

    // the bulk revert: by (prefix) and dates, preview first, then confirm
    let by = q.get("by").cloned().unwrap_or_default();
    let since = q.get("since").cloned().unwrap_or_default();
    let until = q.get("until").cloned().unwrap_or_default();
    body.push_str(&format!(
        "<h2>Bulk revert</h2>\
<form method=\"post\" action=\"/decisions/bulk\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<label>actor (or prefix) <input name=\"by\" value=\"{}\"></label>\
<label>since <input name=\"since\" placeholder=\"YYYY-MM-DD\" value=\"{}\"></label>\
<label>until <input name=\"until\" placeholder=\"YYYY-MM-DD\" value=\"{}\"></label>\
<button>Preview</button></form>",
        csrf_of(&review, "bulk"),
        esc(&by),
        esc(&since),
        esc(&until)
    ));
    if let Some(prev) = review.take_flash() {
        body.push_str(&prev);
    }
    page("Decisions", &body)
}

#[derive(serde::Deserialize)]
struct BulkForm {
    csrf: String,
    #[serde(default)]
    by: String,
    #[serde(default)]
    since: String,
    #[serde(default)]
    until: String,
    #[serde(default)]
    confirm: String,
}

fn parse_day_param(s: &str, end: bool) -> Result<Option<time::OffsetDateTime>, String> {
    if s.is_empty() {
        return Ok(None);
    }
    let v = engramma_core::Validity::parse(s).map_err(|e| e.to_string())?;
    let day = v.start().date();
    let t = if end {
        day.with_hms(23, 59, 59).map_err(|e| e.to_string())?
    } else {
        day.with_hms(0, 0, 0).map_err(|e| e.to_string())?
    };
    Ok(Some(t.assume_utc()))
}

async fn bulk(
    State(review): State<Arc<Review>>,
    headers: HeaderMap,
    Form(form): Form<BulkForm>,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    // the preview and the confirmation share the form (the confirmation
    // carries the same fields back plus `confirm`)
    let form_name = if form.confirm.is_empty() {
        "bulk"
    } else {
        "bulk-confirm"
    };
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), form_name) {
        return *r;
    }
    let since = match parse_day_param(&form.since, false) {
        Ok(v) => v,
        Err(e) => return forbidden(&format!("since: {e}")),
    };
    let until = match parse_day_param(&form.until, true) {
        Ok(v) => v,
        Err(e) => return forbidden(&format!("until: {e}")),
    };
    let filter = RevertFilter {
        by: (!form.by.is_empty()).then(|| form.by.clone()),
        // v0.6.18 (F2): the page's bulk form stays actor-based; the
        // proposer filter is a CLI surface (`revert --proposed-by`)
        proposer: None,
        since,
        until,
    };
    if form.confirm.is_empty() {
        // the preview
        let report = match review.cell.revert_decisions(&filter, true) {
            Ok(r) => r,
            Err(e) => return engine_error(e),
        };
        let prev = format!(
            "<div class=\"proposal\"><h3>Preview</h3>\
<p>would revert <strong>{}</strong> decisions · {} conflicts (left alone) · {} purged \
(reverted in the registry, the content does not come back)</p><ul>{}</ul>\
<form method=\"post\" action=\"/decisions/bulk\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<input type=\"hidden\" name=\"by\" value=\"{}\">\
<input type=\"hidden\" name=\"since\" value=\"{}\">\
<input type=\"hidden\" name=\"until\" value=\"{}\">\
<input type=\"hidden\" name=\"confirm\" value=\"1\">\
<button class=\"no\">Confirm: revert</button></form></div>",
            report.reverted,
            report.conflicts.len(),
            report.purged,
            report
                .conflicts
                .iter()
                .map(|c| format!("<li>{}</li>", esc(c)))
                .collect::<Vec<_>>()
                .join(""),
            csrf_of(&review, "bulk-confirm"),
            esc(&form.by),
            esc(&form.since),
            esc(&form.until),
        );
        // the block goes to the next page through the server, not the URL
        review.flash(prev);
        return see_other("/decisions");
    }
    // confirmed: apply
    match review.cell.revert_decisions(&filter, false) {
        Ok(report) => {
            review.flash(format!(
                "<p class=\"meta\">reverted {}, conflicts {}, purged {}</p>",
                report.reverted,
                report.conflicts.len(),
                report.purged
            ));
            see_other("/decisions")
        }
        Err(e) => engine_error(e),
    }
}

async fn revert_one(
    State(review): State<Arc<Review>>,
    Path(op): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    let form_name = format!("revert-{op}");
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), &form_name) {
        return *r;
    }
    let Ok(op_id) = Ulid::from_string(&op) else {
        return not_found("no such decision");
    };
    match review
        .cell
        .revert_decision(&op_id, "reverted from the review page", "user@review")
    {
        Ok(RevertOutcome::Reverted { .. }) => see_other("/decisions"),
        Ok(RevertOutcome::Purged { .. }) => {
            review.flash(
                "<p class=\"meta\">reverted, but the content was purged: it does not come \
                 back</p>"
                    .to_string(),
            );
            see_other("/decisions")
        }
        Err(e) => engine_error(e),
    }
}

async fn policy(State(review): State<Arc<Review>>, headers: HeaderMap) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    if let Err(r) = review.guard(false, &headers, cookies, None, "") {
        return *r;
    }
    // READ-ONLY: the policy changes only from the CLI
    let policy = match review.cell.policy_of() {
        Ok(p) => p,
        Err(e) => return engine_error(e),
    };
    let body = format!(
        "<h1>The cell's policy</h1>\
<table>\
<tr><th>write</th><td>{}</td></tr>\
<tr><th>reviewer</th><td>{}</td></tr>\
<tr><th>min_confidence</th><td>{}</td></tr>\
<tr><th>kinds</th><td>{}</td></tr>\
<tr><th>allow_supersede</th><td>{}</td></tr>\
<tr><th>max_auto_per_hour</th><td>{}</td></tr>\
</table>\
<p class=\"meta\">read-only: it changes only with <code>engramma policy set</code></p>",
        esc(policy.write.as_str()),
        esc(policy.reviewer.as_deref().unwrap_or("—")),
        policy.min_confidence,
        esc(&if policy.kinds.is_empty() {
            "all".to_string()
        } else {
            policy
                .kinds
                .iter()
                .map(|k| k.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }),
        esc(policy.allow_supersede.as_str()),
        policy.max_auto_per_hour,
    );
    page("Policy", &body)
}

async fn not_found_route() -> Response {
    not_found("no such page")
}

/// v0.6.19 (F2.4): the beta's routes stay as redirects — 308, for good:
/// a bookmark must not break. The forms' routes and their fields are
/// renamed instead: the forms are served by this same binary.
fn moved_permanently(to: String) -> Response {
    Redirect::permanent(&to).into_response()
}

/// `/entra?token=…`, the beta's entry: the token rides along.
async fn old_entry(uri: axum::http::Uri) -> Response {
    moved_permanently(match uri.query() {
        Some(query) => format!("/enter?{query}"),
        None => "/enter".to_string(),
    })
}

/// `/note/<id>`: the note keeps its address under the new route.
async fn old_note(Path(id): Path<String>) -> Response {
    moved_permanently(format!("/notes/{id}"))
}

// ---------------------------------------------------------------------------
// the server
// ---------------------------------------------------------------------------

/// Run the review page: bind 127.0.0.1 (port 0 unless asked), print the
/// entry URL with the token, serve until Ctrl-C.
///
/// # Errors
/// [`std::io::Error`] when the bind or the server fails.
pub async fn run(cell: Cell, port: Option<u16>, open: bool) -> std::io::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(0)));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    // 256 random bits (ring): printed once, then only in the cookie
    let mut bytes = [0_u8; 32];
    use ring::rand::SecureRandom as _;
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let url = format!("http://127.0.0.1:{}/enter?token={token}", bound.port());
    println!("review: {url}");
    if open {
        // best effort: the address is already on stdout
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    let review = Arc::new(Review {
        cell,
        token,
        port: bound.port(),
        flash: std::sync::Mutex::new(None),
    });
    let app = Router::new()
        .route("/", get(|| async { see_other("/queue") }))
        .route("/enter", get(enter))
        .route("/style.css", get(style))
        .route("/queue", get(queue))
        .route("/queue/{id}/accept", post(accept))
        .route("/queue/{id}/reject", post(reject))
        .route("/notes", get(notes))
        .route("/notes/{id}", get(note_detail))
        .route("/notes/{id}/restore", post(restore))
        .route("/decisions", get(decisions))
        .route("/decisions/bulk", post(bulk))
        .route("/decisions/revert/{op}", post(revert_one))
        .route("/policy", get(policy))
        // the beta's routes: 308 to the new ones, the bookmarks live on
        .route("/entra", get(old_entry))
        .route(
            "/coda",
            get(|| async { moved_permanently("/queue".to_string()) }),
        )
        .route(
            "/note",
            get(|| async { moved_permanently("/notes".to_string()) }),
        )
        .route("/note/{id}", get(old_note))
        .route(
            "/decisioni",
            get(|| async { moved_permanently("/decisions".to_string()) }),
        )
        .route(
            "/politica",
            get(|| async { moved_permanently("/policy".to_string()) }),
        )
        .route(
            "/stile.css",
            get(|| async { moved_permanently("/style.css".to_string()) }),
        )
        .fallback(not_found_route)
        .with_state(review)
        // the closed CSP and friends ride on EVERY response
        .layer(axum::middleware::from_fn(security_headers));
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

async fn security_headers(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}
