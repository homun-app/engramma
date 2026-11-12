// SPDX-License-Identifier: Apache-2.0

//! `engramma mcp`: MCP server over stdio (DESIGN §8.3). Read by default;
//! with `--write <cell> --actor <name>` (v0.5, P3) the server ALSO exposes
//! `memory_propose` — an assistant may PROPOSE a fact, never write one.
//!
//! The threat model (DESIGN, v0.5): a proposal is born in quarantine, the
//! per-cell policy decides how far the machine may go, and NO tool here
//! confirms, discards, deletes, purges or changes a policy — those live on
//! human surfaces (CLI, review page) and, for `write: confirm`, in the
//! elicitation form the CLIENT renders. The identity is the `--actor` from
//! the command line, never the `clientInfo` the client declares: the
//! declared client is registered as metadata and nothing more. An agent
//! that has the user's shell can run `engramma proposals accept` — who has
//! your shell is you, and that boundary is declared in the DESIGN.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rmcp::handler::server::tool::{ToolCallContext, ToolRoute, ToolRouter};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResponse, CallToolResult, ClientResult, ContentBlock, ElicitRequestParams,
    ElicitResult, ElicitationAction, InputRequest, InputRequiredResult, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};

use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::{ErrorData as McpError, ServerHandler, serve_server};
use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;

use engramma_core::cell::policy::WritePolicy;
use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, ProposalInput, ProposalMode, ProposalOutcome, ProposalStatus,
    ProposeContext,
};
use engramma_core::format::{At, Note, NoteType};
use engramma_core::review::Reviewers;
use engramma_core::search::{Hit, Query, Semantic};
use engramma_core::{Cell, CellError, Confidence, ModelState, Status};

/// An open, exposed cell.
pub struct MemoryServer {
    tool_router: ToolRouter<Self>,
    cells: HashMap<String, Cell>,
    /// The embedder from `<root>/config.json`, when one is configured: the
    /// vector branch joins automatically, cell by cell, wherever the cell's
    /// ready model matches. Without it every search is text+graph (v0.1).
    embedder: Option<Arc<dyn engramma_core::embed::Embedder>>,
    /// v0.5 (P3): the write surface, when `--write` named cells. `actor`
    /// comes from the command line — the configuration, not the client.
    write: Option<WriteSurface>,
}

/// The write side of the server (v0.5, P3).
pub struct WriteSurface {
    /// The actor label from `--actor`; proposals sign as `mcp:<actor>`.
    actor: String,
    /// The cells that accept proposals through this server.
    cells: Vec<String>,
    /// The parsed `reviewers.yaml` of the data root, when one exists (P2).
    reviewers: Option<Reviewers>,
    /// The MRTR round's pending confirmations (2026-07-28 protocol):
    /// request-state handle → what the retry will decide.
    mrtr: Mutex<HashMap<String, MrtrPending>>,
}

/// One waiting MRTR confirmation.
struct MrtrPending {
    cell: String,
    proposal: ulid::Ulid,
}

/// Error conversion into MCP errors.
fn err(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn not_exposed(cell: &str, cells: &[String]) -> McpError {
    McpError::invalid_request(
        format!(
            "cell `{cell}` is not exposed by this server; exposed cells: {}",
            cells.join(", ")
        ),
        None,
    )
}

fn not_writable(cell: &str, writable: &[String]) -> McpError {
    McpError::invalid_request(
        format!(
            "cell `{cell}` does not accept proposals through this server; writable cells: {}",
            writable.join(", ")
        ),
        None,
    )
}

impl MemoryServer {
    /// Open the cells that will be exposed (fails if one is missing).
    /// `write` names the cells that also accept proposals; `actor` is
    /// REQUIRED when any cell is writable.
    ///
    /// # Errors
    /// [`anyhow::Error`] when a cell cannot be opened or the write surface
    /// is inconsistent.
    pub fn open(
        root: &Path,
        names: &[String],
        write: &[String],
        actor: Option<&str>,
    ) -> anyhow::Result<Self> {
        let mut cells = HashMap::new();
        for name in names {
            let path = root.join("cells").join(format!("{name}.sqlite"));
            let cell = Cell::open(&path).map_err(|e| anyhow::anyhow!("cell {name}: {e}"))?;
            cells.insert(name.clone(), cell);
        }
        let Some(actor) = actor else {
            if !write.is_empty() {
                return Err(anyhow::anyhow!(
                    "`--write` needs `--actor <name>`: proposals must be attributable"
                ));
            }
            return Self::open_readonly(root, cells);
        };
        if write.is_empty() {
            return Err(anyhow::anyhow!(
                "`--actor` goes with `--write <cells>`: an actor without a writable cell does nothing"
            ));
        }
        for w in write {
            if !cells.contains_key(w) {
                return Err(anyhow::anyhow!(
                    "`--write {w}` names a cell that is not in `--cells`"
                ));
            }
        }
        let mut server = Self::open_readonly(root, cells)?;
        let reviewers = match Reviewers::load(root) {
            Ok(r) => Some(r),
            Err(e) => {
                // a broken reviewers.yaml must not take the server down:
                // write:auto fails safe (proposal marked, human decides)
                eprintln!("engramma-mcp: reviewers.yaml: {e}");
                None
            }
        };
        server.write = Some(WriteSurface {
            actor: actor.to_string(),
            cells: write.to_vec(),
            reviewers,
            mrtr: Mutex::new(HashMap::new()),
        });
        // the one writing tool rides the same router the macros built
        server.tool_router.add_route(propose_route());
        Ok(server)
    }

    fn open_readonly(root: &Path, cells: HashMap<String, Cell>) -> anyhow::Result<Self> {
        // The embedder is optional: a root without config.json serves the
        // same text+graph search as v0.1.
        let embedder = match engramma_embed::config::load_embedder(root) {
            Ok(e) => e,
            Err(e) => {
                // a broken configuration must not take the server down
                eprintln!("engramma-mcp: embedder config: {e}");
                None
            }
        };
        Ok(Self {
            tool_router: Self::tool_router(),
            cells,
            embedder,
            write: None,
        })
    }

    /// The semantic context for `cell`, when its ready model matches the
    /// configured embedder.
    fn semantic_for(&self, cell: &Cell) -> Option<Semantic> {
        let embedder = self.embedder.as_ref()?;
        let ready = cell
            .models()
            .ok()?
            .into_iter()
            .find(|m| m.state == ModelState::Ready)?;
        if ready.model != embedder.model_id() {
            return None;
        }
        Some(Semantic::new(Arc::clone(embedder)))
    }

    fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.cells.keys().cloned().collect();
        n.sort();
        n
    }

    fn cell(&self, name: &str) -> Result<&Cell, McpError> {
        self.cells
            .get(name)
            .ok_or_else(|| not_exposed(name, &self.names()))
    }

    /// The cell a tool call addresses: the named one, or the only exposed
    /// cell when the caller left `cell` out.
    fn cell_or_default(&self, name: &Option<String>) -> Result<&Cell, McpError> {
        match name.as_deref() {
            Some(n) => self.cell(n),
            None => {
                let mut cells = self.cells.values();
                match (cells.next(), cells.next()) {
                    // exactly one exposed cell: it is the default
                    (Some(only), None) => Ok(only),
                    _ => Err(McpError::invalid_params(
                        format!(
                            "which cell? This server exposes more than one: {}. Pass `cell`.",
                            self.names().join(", ")
                        ),
                        None,
                    )),
                }
            }
        }
    }

    /// The writable cell a propose addresses, with its `--actor`.
    fn writable_cell(&self, name: &Option<String>) -> Result<(&Cell, &WriteSurface), McpError> {
        let Some(write) = &self.write else {
            // the tool is not even listed without --write; reaching here
            // means a client called it by name anyway
            return Err(McpError::invalid_request(
                "this server exposes no writing tool (start it with `--write <cell> --actor <name>`)".to_string(),
                None,
            ));
        };
        let name = name.clone().unwrap_or_else(|| {
            // the single-cell default, exactly like the read tools
            if write.cells.len() == 1 {
                write.cells[0].clone()
            } else {
                String::new()
            }
        });
        if !write.cells.contains(&name) {
            // the error is not an oracle: it names only what the caller
            // already knows (this server's own configuration)
            let exposed = self.names().join(", ");
            if !self.cells.contains_key(&name) {
                return Err(McpError::invalid_request(
                    format!(
                        "cell `{name}` is not exposed by this server; exposed cells: {exposed}"
                    ),
                    None,
                ));
            }
            return Err(not_writable(&name, &write.cells));
        }
        Ok((self.cell(&name)?, write))
    }
}

// ---------------------------------------------------------------------------
// JSON shapes (stable, documented in README)
// ---------------------------------------------------------------------------

fn hit_json(h: &Hit) -> serde_json::Value {
    serde_json::json!({
        "score": h.score,
        "raw_score": h.raw_score,
        "explain": {
            "matched_terms": h.explain.matched_terms,
            "bm25_rank": h.explain.bm25_rank,
            "semantic_rank": h.explain.semantic_rank,
            "cosine": h.explain.cosine,
            "fused_score": h.explain.fused_score,
            "lists": h.explain.lists,
            "support": h.explain.support,
            "winning_chunk": h.explain.winning_chunk,
            "via": h.explain.via,
            "confidence": h.explain.confidence.as_str(),
            "valid_from": h.explain.valid_from,
            "valid_to": h.explain.valid_to,
            "source": h.explain.source,
        },
        "note": {
            "id": h.note.id.to_string(),
            "type": h.note.note_type.as_str(),
            "title": h.note.display_title(),
            "body": h.note.body,
            "tags": h.note.tags,
            "source": h.note.source,
            "confidence": h.note.confidence.as_str(),
            "status": h.note.status.as_str(),
            "valid_from": h.note.valid_from.as_ref().map(|v| v.to_db_string()),
            "valid_to": h.note.valid_to.as_ref().map(|v| v.to_db_string()),
            "created_at": h.note.created_at.to_db_string(),
            "updated_at": h.note.updated_at.to_db_string(),
        }
    })
}

fn tool_ok(value: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value).map_err(err)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

// ---------------------------------------------------------------------------
// The read tools (unchanged surface, now with their annotations)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct SearchParams {
    /// Cell to search. Must be one of the exposed cells; may be omitted
    /// when the server exposes exactly one.
    cell: Option<String>,
    /// Natural-language query, same language as the notes.
    query: String,
    /// Maximum results (default 10).
    #[serde(default)]
    limit: Option<usize>,
    /// Ask for the past: only notes valid at this date (YYYY-MM-DD) are
    /// returned, including superseded ones that were the truth then.
    #[serde(default)]
    as_of: Option<String>,
    /// Graph expansion, 0-2 hops (default 1). Two hops answers questions like
    /// "where does the partner of Fabio work?".
    #[serde(default)]
    graph_hops: Option<u8>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct GetParams {
    /// Cell that holds the note. May be omitted when the server exposes
    /// exactly one.
    cell: Option<String>,
    /// Note id (or an id prefix).
    id: String,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct NeighborsParams {
    /// Cell to query. May be omitted when the server exposes exactly one.
    cell: Option<String>,
    /// Entity name, e.g. "Fabio".
    entity: String,
    /// BFS depth, default 1 (max 3).
    #[serde(default)]
    depth: Option<u8>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct TagsParams {
    /// Cell to query. May be omitted when the server exposes exactly one.
    cell: Option<String>,
    /// Tag prefix; children are included (e.g. "casa" matches "casa/milano").
    #[serde(default)]
    prefix: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct TimelineParams {
    /// Cell to query. May be omitted when the server exposes exactly one.
    cell: Option<String>,
    /// Entity name whose timeline you want.
    entity: String,
}

#[tool_router(router = tool_router)]
impl MemoryServer {
    #[tool(name = "memory_search", annotations(read_only_hint = true))]
    /// Search a person's or a group's memory. Returns the most relevant notes
    /// about the query: each result carries the note text, where it came from
    /// (`source`), when it was true (`valid_from`/`valid_to`) and why it
    /// matched (`explain`: terms, semantic rank and cosine when the meaning
    /// branch is active, graph path). When `explain.support` is "weak" the
    /// result is close in meaning but not confirmed by any text match —
    /// verify it (ask again with other words, or read the note) before
    /// asserting it as fact. Paraphrases work: ask with different words than
    /// the note uses. Facts that changed over time are linked by `supersedes`:
    /// for "what is true now" rely on notes whose `valid_to` is empty; use
    /// `as_of` for questions about the past ("where did they live in
    /// August?"). Quarantined notes — unconfirmed content from third
    /// parties — never appear in these results: the memory engine hides them
    /// until a human confirms them, so do not treat their absence as a
    /// denial.
    async fn memory_search(
        &self,
        params: Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;
        let cell = self.cell_or_default(&p.cell)?;
        let mut q = Query::text(&p.query).k(p.limit.unwrap_or(10));
        if let Some(h) = p.graph_hops {
            q.graph_hops = h.min(2);
        }
        if let Some(a) = &p.as_of {
            let v = engramma_core::Validity::parse(a)
                .map_err(|e| McpError::invalid_params(format!("as_of: {e}"), None))?;
            q.filters.as_of = Some(v.start());
        }
        // The vector branch joins automatically when the cell has a ready
        // model matching the configured embedder; otherwise this is the
        // text+graph search of v0.1.
        let (hits, mode) = match self.semantic_for(cell) {
            Some(sem) => {
                let outcome = cell.search_semantic(&q, &sem).map_err(err)?;
                (outcome.hits, "hybrid")
            }
            None => (cell.search(&q).map_err(err)?, "text"),
        };
        let arr: Vec<serde_json::Value> = hits.iter().map(hit_json).collect();
        tool_ok(&serde_json::json!({ "query": p.query, "mode": mode, "hits": arr }))
    }

    #[tool(name = "memory_get", annotations(read_only_hint = true))]
    /// Read one note in full by id (or a unique id prefix), including its
    /// exact body, source, confidence and validity. Use it when a search
    /// result is promising but truncated. Quarantined and deleted notes
    /// answer exactly like missing ones: the server gives no oracle on
    /// content waiting for human confirmation.
    async fn memory_get(&self, params: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;
        let cell = self.cell_or_default(&p.cell)?;
        // Hidden (quarantined/deleted) and absent notes answer with the same
        // error, which echoes only what the caller sent: resolving a short id
        // must not reveal a hidden note's full id or existence.
        let not_found = || McpError::invalid_params(format!("note `{}` not found", p.id), None);
        let id = match ulid::Ulid::from_string(&p.id) {
            Ok(id) => id,
            Err(_) => match cell.resolve_visible_note_id(&p.id) {
                Ok(id) => id,
                Err(CellError::NoteNotFound(_)) => return Err(not_found()),
                Err(e) => return Err(McpError::invalid_params(e.to_string(), None)),
            },
        };
        let note = cell.get(&id).map_err(|_| not_found())?;
        if matches!(note.status, Status::Quarantined | Status::Deleted) {
            return Err(not_found());
        }
        tool_ok(&serde_json::json!({
            "note": {
                "id": note.id.to_string(),
                "type": note.note_type.as_str(),
                "title": note.display_title(),
                "body": note.body,
                "tags": note.tags,
                "source": note.source,
                "confidence": note.confidence.as_str(),
                "status": note.status.as_str(),
                "valid_from": note.valid_from.as_ref().map(|v| v.to_db_string()),
                "valid_to": note.valid_to.as_ref().map(|v| v.to_db_string()),
                "created_at": note.created_at.to_db_string(),
                "updated_at": note.updated_at.to_db_string(),
            }
        }))
    }

    #[tool(name = "memory_neighbors", annotations(read_only_hint = true))]
    /// Graph neighbours of an entity (a person, a place, a project): what it
    /// is connected to and through which relation (`partner_of`, `works_at`,
    /// `lives_in`, …), with the validity of each edge. Useful for "who/what
    /// is related to X" questions that search text cannot answer.
    async fn memory_neighbors(
        &self,
        params: Parameters<NeighborsParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;
        let cell = self.cell_or_default(&p.cell)?;
        let ns = cell
            .neighbors(&p.entity, p.depth.unwrap_or(1).min(3), &[], false)
            .map_err(err)?;
        let arr: Vec<serde_json::Value> = ns
            .iter()
            .map(|n| {
                serde_json::json!({
                    "depth": n.depth, "entity": n.entity, "rel": n.rel,
                    "note_id": n.note_id,
                    "valid_from": n.valid_from, "valid_to": n.valid_to,
                })
            })
            .collect();
        tool_ok(&serde_json::json!({ "entity": p.entity, "neighbors": arr }))
    }

    #[tool(name = "memory_tags", annotations(read_only_hint = true))]
    /// Tags used in the memory with their note counts. A prefix includes its
    /// children ("casa" also counts "casa/milano"). Use it to explore how
    /// the memory is organised before filtering searches.
    async fn memory_tags(
        &self,
        params: Parameters<TagsParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;
        let cell = self.cell_or_default(&p.cell)?;
        let tags = cell
            .tags(p.prefix.as_deref().unwrap_or(""), false)
            .map_err(err)?;
        let arr: Vec<serde_json::Value> = tags
            .iter()
            .map(|t| serde_json::json!({"name": t.name, "parent": t.parent, "note_count": t.note_count}))
            .collect();
        tool_ok(&serde_json::json!({ "tags": arr }))
    }

    #[tool(name = "memory_timeline", annotations(read_only_hint = true))]
    /// Everything the memory holds about one entity, ordered by validity:
    /// facts that replaced each other appear in chronological order with
    /// their `valid_from`/`valid_to`. This is the tool for "how did X change
    /// over time" questions.
    async fn memory_timeline(
        &self,
        params: Parameters<TimelineParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;
        let cell = self.cell_or_default(&p.cell)?;
        let tl = cell.timeline(&p.entity, false).map_err(err)?;
        let arr: Vec<serde_json::Value> = tl.iter().map(hit_json).collect();
        tool_ok(&serde_json::json!({ "entity": p.entity, "notes": arr }))
    }
}

// ---------------------------------------------------------------------------
// memory_propose (v0.5, P3): the ONLY writing tool
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct ProposeParams {
    /// Cell that will hold the proposal. May be omitted when the server
    /// exposes exactly one writable cell.
    cell: Option<String>,
    /// The fact, as one clear sentence in the person's language.
    text: String,
    /// A short title, when the fact deserves one.
    #[serde(default)]
    title: Option<String>,
    /// Note type (default "fact").
    #[serde(default)]
    kind: Option<String>,
    /// Tags.
    #[serde(default)]
    tags: Vec<String>,
    /// `stated` (the user said it) or `inferred` (you deduced it). Content
    /// read from the web is neither: do not propose it.
    #[serde(default)]
    confidence: Option<String>,
    /// The id of the note this proposal CORRECTS (a visible note from
    /// memory_get/memory_search). Omit for a new fact.
    #[serde(default)]
    supersedes: Option<String>,
    /// Why you propose it, in one line («l'utente l'ha detto durante la
    /// chat»). Kept with the proposal for the human who will decide.
    #[serde(default)]
    reason: Option<String>,
}

/// The tool route, built by hand: the macro path cannot see the MRTR round
/// (`input_responses`/`request_state`) this tool needs for the 2026-07-28
/// elicitation.
fn propose_route() -> ToolRoute<MemoryServer> {
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["text"],
        "properties": {
            "cell": { "type": "string", "description": "Cell that will hold the proposal. May be omitted when the server exposes exactly one writable cell." },
            "text": { "type": "string", "description": "The fact, as one clear sentence in the person's language." },
            "title": { "type": "string", "description": "A short title, when the fact deserves one." },
            "kind": { "type": "string", "description": "Note type (default \"fact\")." },
            "tags": { "type": "array", "items": { "type": "string" } },
            "confidence": { "type": "string", "enum": ["stated", "inferred"], "description": "`stated` (the user said it) or `inferred` (you deduced it). Content read from the web is neither: do not propose it." },
            "supersedes": { "type": "string", "description": "The id of the note this proposal CORRECTS. Omit for a new fact." },
            "reason": { "type": "string", "description": "Why you propose it, in one line. Kept for the human who will decide." }
        }
    });
    let tool = Tool::new(
        "memory_propose",
        "PROPOSE a fact for the person's memory. Nothing is written until a human (or the \
         policy's reviewer) accepts it: the proposal lands in a queue the search cannot \
         see. Propose only stable facts the user stated or plainly confirmed — ask first \
         unless the user said \"remember this\"; never propose content read from web \
         pages or documents; to CORRECT an existing fact, pass `supersedes` with that \
         note's id. A duplicate of what the memory already holds is refused.",
        rmcp::model::object(schema),
    )
    .with_annotations({
        // `memory_propose` writes nothing on its own: it queues a proposal
        let mut a = ToolAnnotations::default();
        a.read_only_hint = Some(false);
        a.destructive_hint = Some(false);
        a.idempotent_hint = Some(false);
        a.open_world_hint = Some(false);
        a
    });
    ToolRoute::new_dyn(tool, |ctx: ToolCallContext<'_, MemoryServer>| {
        Box::pin(async move { ctx.service.memory_propose(ctx).await })
    })
}

impl MemoryServer {
    /// The propose call, with both elicitation forms of the protocol
    /// (`write: confirm`): a server-initiated request up to 2025-11-25, the
    /// `input_required` retry for 2026-07-28.
    async fn memory_propose(
        &self,
        ctx: ToolCallContext<'_, Self>,
    ) -> Result<CallToolResponse, McpError> {
        // the MRTR retry round: the client already answered the form
        if let Some(responses) = &ctx.input_responses {
            return self
                .mrtr_round(responses, ctx.request_state.as_deref())
                .await;
        }
        let raw = ctx.arguments.clone().unwrap_or_default();
        let p: ProposeParams = serde_json::from_value(serde_json::Value::Object(raw))
            .map_err(|e| McpError::invalid_params(format!("memory_propose: {e}"), None))?;
        let (cell, write) = self.writable_cell(&p.cell)?;
        let cell_name = self
            .cells
            .iter()
            .find(|(_, c)| std::ptr::eq(*c, cell))
            .map(|(n, _)| n.clone())
            .unwrap_or_default();

        // the confidence is the assistant's honesty about itself, never
        // `external` (that is the import path, born quarantined on purpose)
        let confidence = match p.confidence.as_deref() {
            None => Confidence::Inferred,
            Some("stated") => Confidence::Stated,
            Some("inferred") => Confidence::Inferred,
            Some(other) => {
                return Err(McpError::invalid_params(
                    format!("confidence must be `stated` or `inferred`, not `{other}`"),
                    None,
                ));
            }
        };
        let kind = match p.kind.as_deref() {
            None => NoteType::Fact,
            Some(k) => NoteType::parse(k)
                .map_err(|e| McpError::invalid_params(format!("kind: {e}"), None))?,
        };
        // the correction target: a VISIBLE note, exactly like memory_get
        let mode = match &p.supersedes {
            None => ProposalMode::New,
            Some(s) => ProposalMode::Supersede(resolve_visible(cell, s)?),
        };
        let now = At(OffsetDateTime::now_utc());
        let note = Note {
            id: engramma_core::id::new_ulid().map_err(err)?,
            note_type: kind,
            title: p.title,
            body: p.text.clone(),
            tags: p.tags,
            scope: None,
            lang: None,
            source: format!("mcp:{}", write.actor),
            confidence,
            status: Status::Quarantined,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: now,
            updated_at: now,
            extensions: vec![],
        };
        let input = ProposalInput {
            note,
            mode,
            client: None, // filled below, from the declared clientInfo
            reason: p.reason.clone(),
        };
        // the DECLARED clientInfo rides as metadata — registered, never an
        // identity (the actor from the command line is the identity)
        let declared = ctx
            .request_context
            .peer
            .peer_info()
            .map(|i| format!("{} {}", i.client_info.name, i.client_info.version));
        let input = ProposalInput {
            client: declared,
            ..input
        };

        // what the confirmation form will show, lifted before any move
        let p_text = p.text.clone();
        let p_reason = p.reason.clone().unwrap_or_default();
        // the engine call (and, under write:auto, the reviewer's own time
        // limit) must not block the async runtime
        let actor = format!("mcp:{}", write.actor);
        let engine_cell = cell.clone();
        let embedder = self.embedder.clone();
        let reviewers = write.reviewers.clone();
        let moved_input = input;
        let propose_actor = actor.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let ctx = ProposeContext {
                embedder: embedder.as_deref(),
                reviewers: reviewers.as_ref(),
            };
            engine_cell.propose(&moved_input, &propose_actor, &ctx)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("join: {e}"), None))?;
        let outcome = match outcome {
            Ok(o) => o,
            // the policy refusal is a fact for the caller, not a server bug
            Err(CellError::Invalid(m)) => return Err(McpError::invalid_request(m, None)),
            Err(e) => return Err(err(e)),
        };

        match outcome.status {
            ProposalStatus::Duplicate => {
                tool_ok(&outcome_json("duplicate", outcome.id, &outcome, None))
                    .map(CallToolResponse::from)
            }
            ProposalStatus::Accepted | ProposalStatus::Rejected => tool_ok(&outcome_json(
                if outcome.status == ProposalStatus::Accepted {
                    "accepted"
                } else {
                    "rejected"
                },
                outcome.id,
                &outcome,
                None,
            ))
            .map(CallToolResponse::from),
            ProposalStatus::Pending => {
                // `write: confirm`: ask the human through the client's form,
                // in whichever shape the negotiated protocol allows
                let policy_write = {
                    let engine_cell = cell.clone();
                    tokio::task::spawn_blocking(move || engine_cell.policy_of())
                        .await
                        .map_err(|e| McpError::internal_error(format!("join: {e}"), None))?
                        .map_err(err)?
                        .write
                };
                // what the client can do: the capabilities of the request's
                // `_meta` (the 2026-07-28 discover flow, where every request
                // declares itself) or, for a classic initialize handshake,
                // the capabilities negotiated then
                let rc = ctx.request_context.clone();
                let meta_forms = rc
                    .meta
                    .client_capabilities()
                    .is_some_and(|c| c.elicitation.is_some());
                let peer_forms = rc
                    .peer
                    .supported_elicitation_modes()
                    .contains(&rmcp::service::ElicitationMode::Form);
                let form_supported = meta_forms || peer_forms;
                if policy_write == WritePolicy::Confirm && form_supported {
                    // the NEGOTIATED version (the request's `_meta` first,
                    // the handshake state after): rmcp only lets an
                    // `input_required` result reach a 2026-07-28 peer
                    let modern = rc
                        .protocol_version()
                        .is_some_and(|v| v >= rmcp::model::ProtocolVersion::V_2026_07_28);
                    if modern {
                        // form 2 (2026-07-28): the tool answers
                        // `input_required`; the retry brings the answer
                        let state = engramma_core::id::new_ulid().map_err(err)?.to_string();
                        self.write.as_ref().and_then(|w| {
                            w.mrtr.lock().ok().map(|mut m| {
                                m.insert(
                                    state.clone(),
                                    MrtrPending {
                                        cell: cell_name.clone(),
                                        proposal: outcome.id.unwrap_or_default(),
                                    },
                                )
                            })
                        });
                        let mut requests = std::collections::BTreeMap::new();
                        requests.insert(
                            "confirm".to_string(),
                            InputRequest::Elicitation(confirm_request(&p_text, &p_reason)?),
                        );
                        return Ok(CallToolResponse::InputRequired(InputRequiredResult::new(
                            Some(requests),
                            Some(state),
                        )));
                    }
                    // form 1 (up to 2025-11-25): the server asks now; the
                    // answer decides inside this very call
                    let request = confirm_request(&p_text, &p_reason)?;
                    match rc
                        .peer
                        .send_request(rmcp::model::ServerRequest::ElicitRequest(request))
                        .await
                    {
                        Ok(ClientResult::ElicitResult(answer)) => {
                            self.apply_form_answer(cell, outcome.id, &answer, &actor)
                                .await
                        }
                        Ok(_) => {
                            // anything but an elicitation answer: pending
                            pending_result(outcome, true)
                        }
                        Err(_) => {
                            // transport trouble is not a refusal: pending
                            pending_result(outcome, true)
                        }
                    }
                } else {
                    // no elicitation (or another policy): the queue, with a
                    // pointer to where the human decides
                    pending_result(outcome, policy_write == WritePolicy::Confirm)
                }
            }
        }
    }

    /// Apply the human's answer to the confirmation form (both forms end
    /// here): accept (with the possibly edited text), decline = reject,
    /// cancel or silence = still pending.
    async fn apply_form_answer(
        &self,
        cell: &Cell,
        proposal: Option<ulid::Ulid>,
        answer: &ElicitResult,
        actor: &str,
    ) -> Result<CallToolResponse, McpError> {
        let Some(id) = proposal else {
            return Err(McpError::internal_error(
                "no proposal to confirm".to_string(),
                None,
            ));
        };
        let decision_actor = format!("user@{actor}");
        match answer.action {
            ElicitationAction::Accept => {
                let edited = answer
                    .content
                    .as_ref()
                    .and_then(|c| c.get("text"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                self.decide_and_report(cell, id, true, edited, &decision_actor)
                    .await
            }
            // the form's RIFIUTA button: a decision, kept with its reasons
            ElicitationAction::Decline => {
                self.decide_and_report(cell, id, false, None, &decision_actor)
                    .await
            }
            // ANNULLA, or a future action we do not know: the proposal
            // stays in the queue — silence never decides
            ElicitationAction::Cancel | _ => Ok(tool_ok(&serde_json::json!({
                "status": "pending",
                "id": id.to_string(),
                "note": "annullato: la proposta resta in coda, la decidi dalla CLI o dalla pagina di revisione",
            }))?
            .into()),
        }
    }

    /// One decision (from a form answer or an MRTR round) and the final
    /// tool result.
    async fn decide_and_report(
        &self,
        cell: &Cell,
        id: ulid::Ulid,
        accept: bool,
        edited_text: Option<String>,
        actor: &str,
    ) -> Result<CallToolResponse, McpError> {
        let owned_actor = actor.to_string();
        let engine_cell = cell.clone();
        let edited_for_log = edited_text.clone();
        let decision = tokio::task::spawn_blocking(move || {
            // accept-with-edit: the human fixed the text while accepting
            let edited = match &edited_text {
                Some(text) if !text.trim().is_empty() => {
                    let mut note = engine_cell.get(&id)?;
                    if note.body != *text {
                        note.body = text.clone();
                    }
                    Some(note)
                }
                _ => None,
            };
            engine_cell.decide(
                &id,
                &DecisionInput {
                    action: if accept {
                        DecisionAction::Accept { edited }
                    } else {
                        DecisionAction::Reject
                    },
                    reason: Some("confermato dal modulo del client".into()),
                    review: None,
                },
                &owned_actor,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(format!("join: {e}"), None))?
        .map_err(err)?;
        let _ = decision;
        let note = tokio::task::spawn_blocking({
            let cell = cell.clone();
            move || cell.get(&id)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("join: {e}"), None))?
        .map_err(err)?;
        tool_ok(&serde_json::json!({
            "status": if accept { "accepted" } else { "rejected" },
            "id": id.to_string(),
            "decided_by": actor,
            "reason": if accept {
                if edited_for_log.is_some() { "confermato (con modifica) dal modulo" } else { "confermato dal modulo" }
            } else {
                "rifiutato dal modulo"
            },
            "note_status": note.status.as_str(),
        }))
        .map(CallToolResponse::from)
    }

    /// The MRTR retry (2026-07-28): the client fulfilled the form and calls
    /// the tool again with `inputResponses` and the echoed `requestState`.
    async fn mrtr_round(
        &self,
        responses: &std::collections::BTreeMap<String, serde_json::Value>,
        state: Option<&str>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(surface) = &self.write else {
            return Err(McpError::internal_error(
                "no write surface".to_string(),
                None,
            ));
        };
        let Some(state) = state else {
            return Err(McpError::invalid_request(
                "inputResponses need the requestState of the input_required round".to_string(),
                None,
            ));
        };
        let pending = surface
            .mrtr
            .lock()
            .map_err(|_| McpError::internal_error("mrtr state poisoned".to_string(), None))?
            .remove(state);
        let Some(pending) = pending else {
            return Err(McpError::invalid_request(
                format!("unknown requestState `{state}`").to_string(),
                None,
            ));
        };
        let cell = self.cell(&pending.cell)?;
        let Some(answer) = responses.get("confirm") else {
            return Err(McpError::invalid_params(
                "the retry must answer the `confirm` request".to_string(),
                None,
            ));
        };
        // the wire shape of the client's ElicitResult
        let action = match answer.get("action").and_then(serde_json::Value::as_str) {
            Some("accept") => ElicitationAction::Accept,
            Some("decline") => ElicitationAction::Decline,
            Some("cancel") => ElicitationAction::Cancel,
            _ => {
                return Err(McpError::invalid_params(
                    "the answer's action must be accept, decline or cancel".to_string(),
                    None,
                ));
            }
        };
        let mut elicited = ElicitResult::new(action);
        elicited.content = answer.get("content").cloned();
        let actor = format!("user@mcp:{}", surface.actor);
        self.apply_form_answer(cell, Some(pending.proposal), &elicited, &actor)
            .await
    }
}

/// Resolve a short or full id to a VISIBLE note (the correction target).
fn resolve_visible(cell: &Cell, input: &str) -> Result<ulid::Ulid, McpError> {
    let not_found = || McpError::invalid_params(format!("note `{input}` not found"), None);
    let id = match ulid::Ulid::from_string(input) {
        Ok(id) => id,
        Err(_) => match cell.resolve_visible_note_id(input) {
            Ok(id) => id,
            Err(CellError::NoteNotFound(_)) => return Err(not_found()),
            Err(e) => return Err(McpError::invalid_params(e.to_string(), None)),
        },
    };
    // visible means living: a quarantined or deleted note is not a target
    let note = cell.get(&id).map_err(|_| not_found())?;
    if matches!(note.status, Status::Quarantined | Status::Deleted) {
        return Err(not_found());
    }
    Ok(id)
}

/// The confirmation form: the editable text plus the client's own
/// accept/decline/cancel buttons.
fn confirm_request(text: &str, reason: &str) -> Result<rmcp::model::ElicitRequest, McpError> {
    use rmcp::model::ElicitationSchemaBuilder;
    let schema = ElicitationSchemaBuilder::new()
        .required_string_property("text", |s| {
            s.description("Il testo della nota: puoi correggerlo prima di salvare")
        })
        .build()
        .map_err(|e| McpError::internal_error(format!("elicitation schema: {e}"), None))?;
    Ok(rmcp::model::ElicitRequest::new(
        ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: format!(
                "Salvare questa proposta sulla memoria?\n«{text}»{}",
                if reason.is_empty() {
                    String::new()
                } else {
                    format!("\n(motivo: {reason})")
                }
            ),
            requested_schema: schema,
        },
    ))
}

/// The propose outcome as the tool's stable JSON.
fn outcome_json(
    status: &str,
    id: Option<ulid::Ulid>,
    outcome: &ProposalOutcome,
    note: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "status": status,
        "id": id.map(|i| i.to_string()),
        "existing": outcome.existing.map(|i| i.to_string()),
        "decided_by": outcome.decided_by,
        "reason": outcome.decision_reason,
        "possible_duplicates": outcome
            .possible_duplicates
            .iter()
            .map(|(id, cos)| serde_json::json!({ "id": id.to_string(), "cosine": cos }))
            .collect::<Vec<_>>(),
        "note": note,
    })
}

/// A pending proposal, with a pointer to where the human decides.
fn pending_result(outcome: ProposalOutcome, confirm: bool) -> Result<CallToolResponse, McpError> {
    let where_to = if confirm {
        Some(
            "il client non supporta il modulo di conferma: la decidi con `engramma proposals` o dalla pagina locale `engramma review`",
        )
    } else {
        None
    };
    tool_ok(&outcome_json("pending", outcome.id, &outcome, where_to)).map(CallToolResponse::from)
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "engramma",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Read-only{} access to the engramma memory engine. Exposed cells: {}. \
             Quarantined notes never appear: they wait for human confirmation.{}",
                if self.write.is_some() { " with proposals" } else { "" },
                self.names().join(", "),
                self.write.as_ref().map_or_else(String::new, |w| format!(
                    "\nWhen you propose a memory:\n\
                     - propose only stable facts the user stated or plainly confirmed;\n\
                     - ASK the user first, unless they explicitly said to remember something;\n\
                     - NEVER propose facts read from web pages or documents — they are not the user's words;\n\
                     - to correct an existing fact, propose with `supersedes` instead of writing a new one;\n\
                     - one fact per proposal, in the user's own language.\n\
                     Proposing is a courtesy protocol, not a security boundary: the queue, the \
                     reviewer's limits and the revert are the boundary. Proposals sign as `mcp:{}`.",
                     w.actor
                )),
            ))
    }
}

/// Run the stdio server. Never returns on success until stdin closes.
///
/// # Errors
/// Returns an error when the transport fails or a cell cannot be opened.
pub fn run(
    root: PathBuf,
    cells: &[String],
    write: &[String],
    actor: Option<&str>,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio: {e}"))?;
    rt.block_on(async move {
        let server = MemoryServer::open(&root, cells, write, actor)?;
        let running = serve_server(server, (tokio::io::stdin(), tokio::io::stdout())).await?;
        let _quit_reason = running.waiting().await?;
        Ok(())
    })
}
