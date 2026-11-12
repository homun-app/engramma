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

fn page(title: &str, body: &str) -> Response {
    Html(format!(
        "<!doctype html>\n<html lang=\"it\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{}</title><link rel=\"stylesheet\" href=\"/stile.css\"></head>\
<body><nav><a href=\"/coda\">Coda</a> <a href=\"/note\">Note</a> \
<a href=\"/decisioni\">Decisioni</a> <a href=\"/politica\">Politica</a></nav>\
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
            "<!doctype html><html lang=\"it\"><meta charset=\"utf-8\">\
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
            "<!doctype html><html lang=\"it\"><meta charset=\"utf-8\">\
<title>401</title><body><h1>401</h1><p>servirebbe il token: riparti dal \
comando che ha stampato l'indirizzo</p></body></html>"
                .to_string(),
        ),
    )
        .into_response()
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(format!(
            "<!doctype html><html lang=\"it\"><meta charset=\"utf-8\">\
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
            "<!doctype html><html lang=\"it\"><meta charset=\"utf-8\">\
<title>errore</title><body><h1>errore del motore</h1><pre>{}</pre></body></html>",
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
            return Err(Box::new(forbidden("Host non accettabile")));
        }
        // 2. the session cookie
        let Some(cookies) = cookies else {
            return Err(Box::new(unauthorized()));
        };
        let mine = cookies
            .split(';')
            .filter_map(|c| c.trim().strip_prefix("engramma_review="))
            .next()
            .is_some_and(|t| t == self.token);
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
                return Err(Box::new(forbidden("Origin mancante o estraneo")));
            }
            if form_csrf != Some(csrf_of(self, form_name).as_str()) {
                return Err(Box::new(forbidden("token CSRF non valido")));
            }
        }
        Ok(())
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
async fn entra(State(review): State<Arc<Review>>, Query(q): Query<Q>) -> Response {
    let Some(token) = q.get("token") else {
        return unauthorized();
    };
    if token.as_str() != review.token.as_str() {
        return unauthorized();
    }
    let mut r = see_other("/coda");
    r.headers_mut().append(
        axum::http::header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "engramma_review={token}; Path=/; HttpOnly; SameSite=Strict"
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
    let mut body = String::from("<h1>La coda</h1>");
    let queue: Vec<_> = rows.iter().filter(|r| r.state.in_queue()).collect();
    if queue.is_empty() {
        body.push_str("<p>La coda è vuota.</p>");
    }
    for r in &queue {
        let id = r.id.to_string();
        let mut card = format!(
            "<section class=\"proposal\"><h2>{}</h2><pre class=\"body\">{}</pre>\
<p class=\"meta\">proposta da {} il {} — {}",
            esc(r.note.display_title()),
            esc(&r.note.body),
            esc(&r.proposer),
            esc(&r.proposed_at),
            esc(r.reason.as_deref().unwrap_or("senza motivo dichiarato"))
        );
        if let ProposalMode::Supersede(t) = &r.mode {
            card.push_str(&format!(
                " · rettifica di <a href=\"/note/{}\">{}</a>",
                t,
                esc(&engramma_core::id::id8(t))
            ));
        }
        if let Some(c) = &r.client {
            card.push_str(&format!(" · client dichiarato: {}", esc(c)));
        }
        card.push_str("</p>");
        if r.auto_failed {
            card.push_str("<p class=\"meta\">revisione automatica fallita: decidi tu</p>");
        }
        // accept (possibly with an edited text), reject
        card.push_str(&format!(
            "<form method=\"post\" action=\"/coda/{id}/accetta\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<textarea name=\"testo\" placeholder=\"modifica il testo prima di accettare (opzionale)\">{}</textarea>\
<button class=\"ok\">Accetta</button></form> ",
            csrf_of(&review, &format!("accetta-{id}")),
            esc(&r.note.body)
        ));
        card.push_str(&format!(
            "<form method=\"post\" action=\"/coda/{id}/rifiuta\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<input name=\"motivo\" placeholder=\"perché (opzionale)\">\
<button class=\"no\">Rifiuta</button></form>",
            csrf_of(&review, &format!("rifiuta-{id}")),
        ));
        card.push_str("</section>");
        body.push_str(&card);
    }
    if rows.len() > queue.len() {
        body.push_str(&format!(
            "<p class=\"meta\">{} proposte già decise (vedi Decisioni).</p>",
            rows.len() - queue.len()
        ));
    }
    page("Coda", &body)
}

#[derive(serde::Deserialize)]
struct CsrfForm {
    csrf: String,
    #[serde(default)]
    testo: String,
    #[serde(default)]
    motivo: String,
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
    let form_name = format!("accetta-{id}");
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), &form_name) {
        return *r;
    }
    let Ok(note_id) = Ulid::from_string(&id) else {
        return not_found(" proposta inesistente");
    };
    // accept-with-edit when the textarea came back different
    let edited = if !form.testo.trim().is_empty() {
        let mut current = match review.cell.get(&note_id) {
            Ok(n) => n,
            Err(e) => return engine_error(e),
        };
        if form.testo != current.body {
            current.body = form.testo;
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
            reason: Some("accettata dalla pagina di revisione".into()),
            review: None,
        },
        "user@review",
    );
    match outcome {
        Ok(_) => see_other("/coda"),
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
    let form_name = format!("rifiuta-{id}");
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), &form_name) {
        return *r;
    }
    let Ok(note_id) = Ulid::from_string(&id) else {
        return not_found("proposta inesistente");
    };
    let outcome = review.cell.decide(
        &note_id,
        &DecisionInput {
            action: DecisionAction::Reject,
            reason: (!form.motivo.is_empty()).then(|| form.motivo.clone()),
            review: None,
        },
        "user@review",
    );
    match outcome {
        Ok(_) => see_other("/coda"),
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
        "<h1>Le note</h1>\
<form method=\"get\" action=\"/note\">\
<input name=\"q\" placeholder=\"cerca nel testo\" value=\"{}\">\
<button>Cerca</button></form>",
    );
    let query = q.get("q").cloned().unwrap_or_default();
    body = body.replacen("{}", &esc(&query), 1);
    if query.is_empty() {
        let ids = match review.cell.note_ids() {
            Ok(i) => i,
            Err(e) => return engine_error(e),
        };
        body.push_str(&format!("<p class=\"meta\">{} note vive</p>", ids.len()));
    } else {
        let hits = review
            .cell
            .search(&SearchQuery::text(&query).k(50))
            .map_err(|e| forbidden(&e.to_string()))
            .unwrap_or_default();
        body.push_str("<ul>");
        for h in &hits {
            body.push_str(&format!(
                "<li><a href=\"/note/{}\">{}</a> <span class=\"meta\">{}</span></li>",
                h.note.id,
                esc(h.note.display_title()),
                esc(h.note.status.as_str())
            ));
        }
        body.push_str("</ul>");
    }
    page("Note", &body)
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
        return not_found("nota inesistente");
    };
    let note = match review.cell.get(&note_id) {
        Ok(n) => n,
        Err(_) => return not_found("nota inesistente"),
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
            " · <span class=\"badge\">proposta {}</span>",
            p.state.as_str()
        ));
    }
    body.push_str("</p>");
    body.push_str(&format!("<pre class=\"body\">{}</pre>", esc(&note.body)));
    if matches!(note.status, Status::Quarantined) {
        body.push_str("<p class=\"meta\">in quarantena: invisibile alla ricerca</p>");
    }
    if let Some(p) = &proposal {
        body.push_str(&format!(
            "<p class=\"meta\">proposta da {} il {} — {}</p>",
            esc(&p.proposer),
            esc(&p.proposed_at),
            esc(p.reason.as_deref().unwrap_or("senza motivo dichiarato"))
        ));
        if let (Some(who), Some(at)) = (&p.decided_by, &p.decided_at) {
            body.push_str(&format!(
                "<p class=\"meta\">decisa da {} il {}</p>",
                esc(who),
                esc(at)
            ));
        }
    }
    // the versions, with the diff against the previous one and the restore
    body.push_str("<h2>Versioni</h2>");
    if hist.is_empty() {
        body.push_str("<p class=\"meta\">nessuna versione precedente</p>");
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
        let restore_action = format!("/note/{}/ripristina", note_id);
        body.push_str(&format!(
            "<form method=\"post\" action=\"{restore_action}\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<input type=\"hidden\" name=\"versione\" value=\"{}\">\
<button>Ripristina questa versione</button></form>",
            csrf_of(
                &review,
                &format!("ripristina-{}-{restore_action}", v.version)
            ),
            v.version
        ));
    }
    if let Some(prev) = prev_body
        && prev != current
    {
        body.push_str("<h3>ora</h3>");
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
    versione: i64,
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
    let form_name = format!("ripristina-{}-/note/{}/ripristina", form.versione, id);
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), &form_name) {
        return *r;
    }
    let note_id = review
        .cell
        .resolve_note_id(&id)
        .or_else(|_| Ulid::from_string(&id))
        .ok();
    let Some(note_id) = note_id else {
        return not_found("nota inesistente");
    };
    match review.cell.restore(&note_id, form.versione, "user@review") {
        Ok(()) => see_other(&format!("/note/{note_id}")),
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
        "<h1>Le decisioni</h1><table><tr>\
<th>quando</th><th>chi</th><th>cosa</th><th>perché</th><th></th></tr>",
    );
    for r in &rows {
        let op8 = engramma_core::id::id8(&r.op_id);
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{} {} <a href=\"/note/{}\">{}</a></td><td>{}</td>",
            esc(&r.decided_at),
            esc(&r.actor),
            if r.decision == "accept" {
                "accetta"
            } else {
                "rifiuta"
            },
            if r.reverted_by.is_some() {
                "<span class=\"badge\">annullata</span>"
            } else {
                ""
            },
            r.proposal,
            op8,
            esc(r.reason.as_deref().unwrap_or(""))
        ));
        if let Some(review_json) = &r.review {
            body.push_str(&format!(
                "<div class=\"meta\">revisore {} · modello {} · confidenza {} · prompt {}</div>",
                esc(review_json["name"].as_str().unwrap_or("?")),
                esc(review_json["model"].as_str().unwrap_or("?")),
                esc(&review_json["confidence"].to_string()),
                esc(&review_json["prompt_hash"].as_str().unwrap_or("?")[..8])
            ));
        }
        if r.reverted_by.is_none() {
            body.push_str(&format!(
                "<td><form method=\"post\" action=\"/decisioni/annulla/{}\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<button>Annulla</button></form></td>",
                r.op_id,
                csrf_of(&review, &format!("annulla-{}", r.op_id))
            ));
        } else {
            body.push_str("<td></td>");
        }
        body.push_str("</tr>");
    }
    body.push_str("</table>");

    // the bulk revert: by (prefix) and dates, preview first, then confirm
    let by = q.get("by").cloned().unwrap_or_default();
    let since = q.get("dal").cloned().unwrap_or_default();
    let until = q.get("al").cloned().unwrap_or_default();
    body.push_str(&format!(
        "<h2>Annullamento in blocco</h2>\
<form method=\"post\" action=\"/decisioni/blocco\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<label>attore (o prefisso) <input name=\"by\" value=\"{}\"></label>\
<label>dal <input name=\"dal\" placeholder=\"AAAA-MM-GG\" value=\"{}\"></label>\
<label>al <input name=\"al\" placeholder=\"AAAA-MM-GG\" value=\"{}\"></label>\
<button>Anteprima</button></form>",
        csrf_of(&review, "blocco"),
        esc(&by),
        esc(&since),
        esc(&until)
    ));
    if let Some(prev) = q.get("anteprima") {
        body.push_str(prev);
    }
    page("Decisioni", &body)
}

#[derive(serde::Deserialize)]
struct BulkForm {
    csrf: String,
    #[serde(default)]
    by: String,
    #[serde(default)]
    dal: String,
    #[serde(default)]
    al: String,
    #[serde(default)]
    conferma: String,
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
    // carries the same fields back plus `conferma`)
    let form_name = if form.conferma.is_empty() {
        "blocco"
    } else {
        "blocco-conferma"
    };
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), form_name) {
        return *r;
    }
    let since = match parse_day_param(&form.dal, false) {
        Ok(v) => v,
        Err(e) => return forbidden(&format!("dal: {e}")),
    };
    let until = match parse_day_param(&form.al, true) {
        Ok(v) => v,
        Err(e) => return forbidden(&format!("al: {e}")),
    };
    let filter = RevertFilter {
        by: (!form.by.is_empty()).then(|| form.by.clone()),
        since,
        until,
    };
    if form.conferma.is_empty() {
        // the preview
        let report = match review.cell.revert_decisions(&filter, true) {
            Ok(r) => r,
            Err(e) => return engine_error(e),
        };
        let prev = format!(
            "<div class=\"proposal\"><h3>Anteprima</h3>\
<p>annullerò <strong>{}</strong> decisioni · conflitti {} (non toccati) · purgate {} \
(annullate nel registro, il contenuto non torna)</p><ul>{}</ul>\
<form method=\"post\" action=\"/decisioni/blocco\">\
<input type=\"hidden\" name=\"csrf\" value=\"{}\">\
<input type=\"hidden\" name=\"by\" value=\"{}\">\
<input type=\"hidden\" name=\"dal\" value=\"{}\">\
<input type=\"hidden\" name=\"al\" value=\"{}\">\
<input type=\"hidden\" name=\"conferma\" value=\"1\">\
<button class=\"no\">Confermo: annulla</button></form></div>",
            report.reverted,
            report.conflicts.len(),
            report.purged,
            report
                .conflicts
                .iter()
                .map(|c| format!("<li>{}</li>", esc(c)))
                .collect::<Vec<_>>()
                .join(""),
            csrf_of(&review, "blocco-conferma"),
            esc(&form.by),
            esc(&form.dal),
            esc(&form.al),
        );
        return see_other(&format!(
            "/decisioni?by={}&dal={}&al={}&anteprima={}",
            urlencode(&form.by),
            urlencode(&form.dal),
            urlencode(&form.al),
            urlencode(&prev)
        ));
    }
    // confirmed: apply
    match review.cell.revert_decisions(&filter, false) {
        Ok(report) => see_other(&format!(
            "/decisioni?anteprima={}",
            urlencode(&format!(
                "<p class=\"meta\">annullate {}, conflitti {}, purgate {}</p>",
                report.reverted,
                report.conflicts.len(),
                report.purged
            ))
        )),
        Err(e) => engine_error(e),
    }
}

fn urlencode(s: &str) -> String {
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

async fn revert_one(
    State(review): State<Arc<Review>>,
    Path(op): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let cookies = headers
        .get(axum::http::header::COOKIE)
        .and_then(|c| c.to_str().ok());
    let form_name = format!("annulla-{op}");
    if let Err(r) = review.guard(true, &headers, cookies, Some(&form.csrf), &form_name) {
        return *r;
    }
    let Ok(op_id) = Ulid::from_string(&op) else {
        return not_found("decisione inesistente");
    };
    match review
        .cell
        .revert_decision(&op_id, "annullata dalla pagina di revisione", "user@review")
    {
        Ok(RevertOutcome::Reverted { .. }) => see_other("/decisioni"),
        Ok(RevertOutcome::Purged { .. }) => {
            see_other("/decisioni?anteprima=annullata%2C%20ma%20il%20contenuto%20era%20purgato")
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
        "<h1>La politica della cella</h1>\
<table>\
<tr><th>write</th><td>{}</td></tr>\
<tr><th>reviewer</th><td>{}</td></tr>\
<tr><th>min_confidence</th><td>{}</td></tr>\
<tr><th>kinds</th><td>{}</td></tr>\
<tr><th>allow_supersede</th><td>{}</td></tr>\
<tr><th>max_auto_per_hour</th><td>{}</td></tr>\
</table>\
<p class=\"meta\">in sola lettura: si cambia solo con <code>engramma policy set</code></p>",
        esc(policy.write.as_str()),
        esc(policy.reviewer.as_deref().unwrap_or("—")),
        policy.min_confidence,
        esc(&if policy.kinds.is_empty() {
            "tutti".to_string()
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
    page("Politica", &body)
}

async fn not_found_route() -> Response {
    not_found("pagina inesistente")
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
    let url = format!("http://127.0.0.1:{}/entra?token={token}", bound.port());
    println!("revisione: {url}");
    if open {
        // best effort: the address is already on stdout
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    let review = Arc::new(Review {
        cell,
        token,
        port: bound.port(),
    });
    let app = Router::new()
        .route("/", get(|| async { see_other("/coda") }))
        .route("/entra", get(entra))
        .route("/stile.css", get(style))
        .route("/coda", get(queue))
        .route("/coda/{id}/accetta", post(accept))
        .route("/coda/{id}/rifiuta", post(reject))
        .route("/note", get(notes))
        .route("/note/{id}", get(note_detail))
        .route("/note/{id}/ripristina", post(restore))
        .route("/decisioni", get(decisions))
        .route("/decisioni/blocco", post(bulk))
        .route("/decisioni/annulla/{op}", post(revert_one))
        .route("/politica", get(policy))
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
