// SPDX-License-Identifier: Apache-2.0

//! `engramma mcp`: read-only MCP server over stdio (DESIGN §8.3).
//!
//! Only the cells named in `--cells` are exposed; every other cell (and any
//! note in quarantine) is unreachable through this server. Tool descriptions
//! are written for the models that will call them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};

use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::{ErrorData as McpError, ServerHandler, serve_server};
use schemars::JsonSchema;
use serde::Deserialize;

use engramma_core::format::Status;
use engramma_core::search::{Hit, Query, Semantic};
use engramma_core::{Cell, ModelState};

/// An open, exposed cell.
pub struct MemoryServer {
    tool_router: ToolRouter<Self>,
    cells: HashMap<String, Cell>,
    /// The embedder from `<root>/config.json`, when one is configured: the
    /// vector branch joins automatically, cell by cell, wherever the cell's
    /// ready model matches. Without it every search is text+graph (v0.1).
    embedder: Option<std::sync::Arc<dyn engramma_core::embed::Embedder>>,
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

impl MemoryServer {
    /// Open the cells that will be exposed (fails if one is missing).
    ///
    /// # Errors
    /// [`anyhow::Error`] when a cell cannot be opened.
    pub fn open(root: &Path, names: &[String]) -> anyhow::Result<Self> {
        let mut cells = HashMap::new();
        for name in names {
            let path = root.join("cells").join(format!("{name}.sqlite"));
            let cell = Cell::open(&path).map_err(|e| anyhow::anyhow!("cell {name}: {e}"))?;
            cells.insert(name.clone(), cell);
        }
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
        Some(Semantic::new(std::sync::Arc::clone(embedder)))
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
// Tools
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
    #[tool(name = "memory_search")]
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

    #[tool(name = "memory_get")]
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
                Err(engramma_core::CellError::NoteNotFound(_)) => return Err(not_found()),
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

    #[tool(name = "memory_neighbors")]
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

    #[tool(name = "memory_tags")]
    /// Tags used in the memory with their note counts. A prefix includes its
    /// children ("casa" also counts "casa/milano"). Use it to explore how the
    /// memory is organised before filtering searches.
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

    #[tool(name = "memory_timeline")]
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

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "engramma",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Read-only access to the engramma memory engine. Exposed cells: {}. \
             Quarantined notes never appear: they wait for human confirmation.",
                self.names().join(", ")
            ))
    }
}

/// Run the stdio server. Never returns on success until stdin closes.
///
/// # Errors
/// Returns an error when the transport fails or a cell cannot be opened.
pub fn run(root: PathBuf, cells: &[String]) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio: {e}"))?;
    rt.block_on(async move {
        let server = MemoryServer::open(&root, cells)?;
        let running = serve_server(server, (tokio::io::stdin(), tokio::io::stdout())).await?;
        let _quit_reason = running.waiting().await?;
        Ok(())
    })
}
