// SPDX-License-Identifier: Apache-2.0

//! `engramma`: the command line. Layout of a data root:
//! `<root>/cells/<name>.sqlite` (default root: `~/.engramma`).

mod output;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::port::{export, import_cell, import_obsidian};
use engramma_core::search::Query;
use engramma_core::{Cell, Confidence, NotePatch, Status, Validity};
use output::{CliError, hit_human, hit_json, note_json};
use ulid::Ulid;

#[derive(Parser)]
#[command(
    name = "engramma",
    version,
    about = "Motore di memoria personale: note, grafo, tempo, ricerca",
    disable_help_subcommand = true
)]
struct Cli {
    /// Data root (default: ~/.engramma)
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Cell to operate on (default: "default")
    #[arg(long, global = true, default_value = "default")]
    cell: String,

    /// Stable JSON output
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the data root and the default cell
    Init,
    /// Manage cells
    Cell {
        #[command(subcommand)]
        action: CellAction,
    },
    /// Add a note (body from a file or stdin)
    Add {
        /// File with the body markdown ("-" = stdin)
        file: String,
        /// Skip the embedding pass at the end (notes stay pending)
        #[arg(long)]
        no_embed: bool,
        /// Provenance (required)
        #[arg(long)]
        source: String,
        /// Reliability (required)
        #[arg(long)]
        confidence: String,
        /// Note type
        #[arg(long, default_value = "fact")]
        r#type: String,
        /// Title
        #[arg(long)]
        title: Option<String>,
        /// Tag (repeatable)
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Valid from (date or datetime)
        #[arg(long)]
        valid_from: Option<String>,
        /// Language override for stemming
        #[arg(long)]
        lang: Option<String>,
    },
    /// Read a note (id or id prefix)
    Get { id: String },
    /// Edit a note in $EDITOR and apply the difference
    Edit { id: String },
    /// Logical delete (tombstone)
    Rm { id: String },
    /// Promote a quarantined note to active
    Confirm { id: String },
    /// Close a note and replace it with a new one from a file
    Supersede {
        /// Note to close (id or prefix)
        old: String,
        /// File with the new note body
        file: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        confidence: String,
        #[arg(long, default_value = "fact")]
        r#type: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        valid_from: Option<String>,
    },
    /// Search notes
    Search {
        /// Query text
        text: String,
        /// text | vector | hybrid (default: hybrid when an embedder is ready)
        #[arg(long)]
        mode: Option<String>,
        /// Fusion for hybrid search: rrf | convex-0.3 | convex-0.5 | convex-0.7
        #[arg(long)]
        fusion: Option<String>,
        /// Abstention threshold for vector-only results (default 0.30)
        #[arg(long)]
        tau: Option<f32>,
        #[arg(long, default_value = "10")]
        k: usize,
        /// Graph expansion 0-2
        #[arg(long, default_value = "1")]
        hops: u8,
        /// Valid at this date (YYYY-MM-DD)
        #[arg(long)]
        as_of: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long = "type")]
        types: Vec<String>,
        #[arg(long)]
        include_quarantined: bool,
        #[arg(long)]
        lang: Option<String>,
    },
    /// Graph neighbours of an entity
    Neighbors {
        entity: String,
        #[arg(long, default_value = "1")]
        depth: u8,
        #[arg(long = "rel")]
        rels: Vec<String>,
        /// Include edges declared by quarantined notes
        #[arg(long)]
        include_quarantined: bool,
    },
    /// Shortest path between two entities
    Path {
        a: String,
        b: String,
        /// Include edges declared by quarantined notes
        #[arg(long)]
        include_quarantined: bool,
    },
    /// Tags (with children)
    Tags {
        prefix: Option<String>,
        /// Count quarantined notes too
        #[arg(long)]
        include_quarantined: bool,
    },
    /// Entities by prefix
    Entities { prefix: Option<String> },
    /// Notes about an entity ordered by validity
    Timeline {
        entity: String,
        /// Include edges declared by quarantined notes
        #[arg(long)]
        include_quarantined: bool,
    },
    /// Versions of a note
    History { id: String },
    /// Restore a version
    Restore { id: String, version: i64 },
    /// Import a folder (Engramma export or `--obsidian` vault)
    Import {
        dir: PathBuf,
        /// Import a plain Obsidian vault
        #[arg(long)]
        obsidian: bool,
        /// Skip the embedding pass at the end (notes stay pending)
        #[arg(long)]
        no_embed: bool,
    },
    /// Export the cell as an Engramma folder
    Export { dir: PathBuf },
    /// Manage embedding models (list, pull, use)
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Embed pending notes with the configured embedder
    Embed {
        /// Re-embed every living note, even fresh ones
        #[arg(long)]
        all: bool,
    },
    /// Verify the log (hash chain + replay)
    Verify,
    /// Start the MCP server (stdio, read-only)
    Mcp {
        /// Exposed cells (comma-separated; without this flag no cell is exposed)
        #[arg(long, value_delimiter = ',')]
        cells: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// List the known models and their state in this cell
    List,
    /// Download a model (pinned revision, sha256-verified)
    Pull { id: String },
    /// Set the cell's embedding model (blue/green: builds while the old one serves)
    Use { id: String },
}

#[derive(Subcommand)]
enum CellAction {
    /// List cells
    List,
    /// Create a cell
    Create { name: String },
    /// Delete a cell file (irreversible)
    Drop { name: String },
}

fn root(cli: &Cli) -> PathBuf {
    cli.root
        .clone()
        .or_else(|| std::env::var("ENGRAMMA_ROOT").ok().map(PathBuf::from))
        .or_else(home_root)
        .unwrap_or_else(|| PathBuf::from(".engramma"))
}

fn home_root() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| Path::new(&h).join(".engramma"))
}

fn cells_dir(r: &Path) -> PathBuf {
    r.join("cells")
}

fn cell_path(cli: &Cli, name: &str) -> Result<PathBuf, CliError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        || name.starts_with('-')
    {
        return Err(CliError::usage(format!(
            "cell name `{name}` is invalid (use lowercase letters, digits, - and _)"
        )));
    }
    Ok(cells_dir(&root(cli)).join(format!("{name}.sqlite")))
}

fn open_cell(cli: &Cli) -> Result<Cell, CliError> {
    let path = cell_path(cli, &cli.cell)?;
    Cell::open(&path).map_err(|e| {
        CliError::internal(format!(
            "cell `{}`: {e} (run `engramma init` first?)",
            cli.cell
        ))
    })
}

fn parse_confidence(s: &str) -> Result<Confidence, CliError> {
    Confidence::parse(s).map_err(|e| CliError::usage(e.to_string()))
}

fn parse_type(s: &str) -> Result<NoteType, CliError> {
    NoteType::parse(s).map_err(|e| CliError::usage(e.to_string()))
}

fn parse_validity(s: &str) -> Result<Option<Validity>, CliError> {
    if s.is_empty() {
        return Ok(None);
    }
    Validity::parse(s)
        .map(Some)
        .map_err(|e| CliError::usage(e.to_string()))
}

fn read_body(file: &str) -> Result<String, CliError> {
    if file == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| CliError::internal(format!("stdin: {e}")))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(file).map_err(|e| CliError::usage(format!("{file}: {e}")))
    }
}

// Every field of a note as an explicit argument: the count is the point.
#[allow(clippy::too_many_arguments)]
fn build_note(
    id: Ulid,
    now: At,
    body: String,
    note_type: NoteType,
    title: Option<String>,
    tags: Vec<String>,
    valid_from: Option<Validity>,
    lang: Option<Lang>,
    source: &str,
    confidence: Confidence,
) -> Note {
    let status = if confidence == Confidence::External {
        Status::Quarantined
    } else {
        Status::Active
    };
    Note {
        id,
        note_type,
        title,
        body,
        tags,
        scope: None,
        lang,
        source: source.to_string(),
        confidence,
        status,
        valid_from,
        valid_to: None,
        supersedes: vec![],
        created_at: now,
        updated_at: now,
        extensions: vec![],
    }
}

fn parse_as_of(s: &str) -> Result<time::OffsetDateTime, CliError> {
    Validity::parse(s)
        .map(|v| v.start())
        .map_err(|e| CliError::usage(format!("--as-of: {e}")))
}

fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engramma: {e}");
            ExitCode::from(e.code.clamp(1, 255) as u8)
        }
    }
}

fn run(cli: &Cli) -> Result<(), CliError> {
    match &cli.command {
        Command::Init => {
            let r = root(cli);
            std::fs::create_dir_all(cells_dir(&r))?;
            let default = cell_path(cli, "default")?;
            if !default.exists() {
                Cell::create(&default, "default", "cli")?;
            }
            if cli.json {
                print_json(&serde_json::json!({ "root": r.display().to_string() }));
            } else {
                println!("inizializzato: {}", r.display());
            }
            Ok(())
        }
        Command::Cell { action } => run_cell(cli, action),
        Command::Add {
            file,
            no_embed,
            source,
            confidence,
            r#type,
            title,
            tags,
            valid_from,
            lang,
        } => {
            let cell = open_cell(cli)?;
            let body = read_body(file)?;
            let confidence = parse_confidence(confidence)?;
            let note_type = parse_type(r#type)?;
            let lang = match lang.as_deref() {
                Some(l) => Some(Lang::parse(l).map_err(|e| CliError::usage(e.to_string()))?),
                None => None,
            };
            let now = At(time::OffsetDateTime::now_utc());
            let note = build_note(
                engramma_core::id::new_ulid().map_err(CliError::from)?,
                now,
                body,
                note_type,
                title.clone(),
                tags.clone(),
                parse_validity(valid_from.as_deref().unwrap_or(""))?,
                lang,
                source,
                confidence,
            );
            cell.put_note(&note, "cli")?;
            let embedded = if *no_embed {
                None
            } else {
                embed_after_write(cli, &cell)?
            };
            if cli.json {
                let mut v = serde_json::json!({ "id": note.id.to_string() });
                if let Some((done, remaining)) = embedded {
                    v["embedded"] = done.into();
                    v["pending"] = remaining.into();
                }
                print_json(&v);
            } else {
                println!("aggiunta: {}", note.id);
                if let Some((done, remaining)) = embedded {
                    println!("incorporate: {done} (in attesa: {remaining})");
                }
            }
            Ok(())
        }
        Command::Get { id } => {
            let cell = open_cell(cli)?;
            let id = cell.resolve_note_id(id)?;
            let note = cell.get(&id)?;
            if cli.json {
                print_json(&note_json(&note));
            } else {
                if matches!(note.status, Status::Quarantined | Status::Deleted) {
                    println!("⚠ stato: {}", note.status.as_str());
                }
                println!(
                    "{}",
                    note.serialize()
                        .map_err(|e| CliError::internal(e.to_string()))?
                );
            }
            Ok(())
        }
        Command::Edit { id } => {
            let cell = open_cell(cli)?;
            let id = cell.resolve_note_id(id)?;
            let note = cell.get(&id)?;
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let tmp = tempfile_in_engramma_tmp()?;
            std::fs::write(
                &tmp,
                note.serialize()
                    .map_err(|e| CliError::internal(e.to_string()))?,
            )?;
            let status = std::process::Command::new(&editor)
                .arg(&tmp)
                .status()
                .map_err(|e| CliError::internal(format!("{editor}: {e}")))?;
            if !status.success() {
                std::fs::remove_file(&tmp).ok();
                return Err(CliError::internal(format!("{editor} exited with {status}")));
            }
            let edited = std::fs::read_to_string(&tmp)?;
            std::fs::remove_file(&tmp).ok();
            let parsed = engramma_core::format::parse(&edited, engramma_core::ParseMode::Strict)
                .map_err(CliError::from)?;
            let mut n = parsed.note;
            if n.id != note.id {
                return Err(CliError::usage("the note id cannot change in edit"));
            }
            n.created_at = note.created_at; // not editable
            if n == note {
                if !cli.json {
                    println!("nessuna modifica");
                }
                return Ok(());
            }
            let patch = NotePatch {
                title: (n.title != note.title).then(|| n.title.clone()),
                body: (n.body != note.body).then(|| n.body.clone()),
                tags: (n.tags != note.tags).then(|| n.tags.clone()),
                scope: (n.scope != note.scope).then(|| n.scope.clone()),
                lang: n.lang.filter(|l| Some(*l) != note.lang),
                source: (n.source != note.source).then(|| n.source.clone()),
                confidence: (n.confidence != note.confidence).then_some(n.confidence),
                valid_from: (n.valid_from != note.valid_from).then_some(n.valid_from),
                valid_to: (n.valid_to != note.valid_to).then_some(n.valid_to),
                extensions: (n.extensions != note.extensions).then(|| n.extensions.clone()),
            };
            cell.patch_note(&id, &patch, "cli")?;
            if cli.json {
                print_json(&serde_json::json!({ "id": id.to_string(), "patched": true }));
            } else {
                println!("aggiornata: {}", id);
            }
            Ok(())
        }
        Command::Rm { id } => {
            let cell = open_cell(cli)?;
            let id = cell.resolve_note_id(id)?;
            cell.delete_note(&id, "cli")?;
            report_ok(cli, "cancellata (lapide)", &id.to_string());
            Ok(())
        }
        Command::Confirm { id } => {
            let cell = open_cell(cli)?;
            let id = cell.resolve_note_id(id)?;
            cell.confirm(&id, "cli-user")?;
            report_ok(cli, "confermata", &id.to_string());
            Ok(())
        }
        Command::Supersede {
            old,
            file,
            source,
            confidence,
            r#type,
            title,
            tags,
            valid_from,
        } => {
            let cell = open_cell(cli)?;
            let old_id = cell.resolve_note_id(old)?;
            let body = read_body(file)?;
            let confidence = parse_confidence(confidence)?;
            let now = At(time::OffsetDateTime::now_utc());
            let new = build_note(
                engramma_core::id::new_ulid().map_err(CliError::from)?,
                now,
                body,
                parse_type(r#type)?,
                title.clone(),
                tags.clone(),
                parse_validity(valid_from.as_deref().unwrap_or(""))?,
                None,
                source,
                confidence,
            );
            cell.supersede(&old_id, &new, None, "cli")?;
            if cli.json {
                print_json(&serde_json::json!({
                    "closed": old_id.to_string(),
                    "new": new.id.to_string(),
                }));
            } else {
                println!("sostituita: {} → {}", old_id, new.id);
            }
            Ok(())
        }
        Command::Search {
            text,
            mode,
            fusion,
            tau,
            k,
            hops,
            as_of,
            tags,
            types,
            include_quarantined,
            lang,
        } => {
            let cell = open_cell(cli)?;
            let mut q = Query::text(text).k(*k).graph_hops(*hops);
            q.filters.include_quarantined = *include_quarantined;
            if let Some(a) = as_of {
                q.filters.as_of = Some(parse_as_of(a)?);
            }
            for t in tags {
                q.filters.tags.push(t.clone());
            }
            for t in types {
                q.filters.types.push(parse_type(t)?);
            }
            if let Some(l) = lang {
                q.lang = Some(Lang::parse(l).map_err(|e| CliError::usage(e.to_string()))?);
            }
            // Semantic side: the configured embedder decides what "hybrid"
            // (the default) can do.
            let embedder = load_embedder_or_warn(cli)?;
            let ready_model = cell.models().ok().and_then(|ms| {
                ms.into_iter()
                    .find(|m| m.state == engramma_core::ModelState::Ready)
            });
            let hybrid_ready = embedder.as_ref().is_some_and(|e| {
                ready_model
                    .as_ref()
                    .is_some_and(|m| e.model_id() == m.model)
            });
            let chosen = match mode.as_deref() {
                Some("text") => "text",
                Some("vector") | Some("hybrid") if hybrid_ready => {
                    mode.as_deref().unwrap_or("hybrid")
                }
                Some("vector") | Some("hybrid") => {
                    return Err(CliError::usage(
                        "no ready embedding model for this cell: run `engramma model use <id>` \
                         and `engramma embed`, or search with --mode text",
                    ));
                }
                Some(other) => {
                    return Err(CliError::usage(format!(
                        "unknown --mode `{other}` (text, vector, hybrid)"
                    )));
                }
                None if hybrid_ready => "hybrid",
                None => "text",
            };
            let outcome = if chosen == "text" {
                let hits = cell.search(&q)?;
                engramma_core::search::SearchOutcome {
                    hits,
                    embed_ms: 0.0,
                    search_ms: 0.0,
                }
            } else {
                let e = embedder
                    .as_ref()
                    .unwrap_or_else(|| unreachable!("hybrid_ready implies an embedder"));
                let mut sem = engramma_core::search::Semantic::new(std::sync::Arc::clone(e))
                    .with_tau(tau.unwrap_or(0.30));
                if let Some(f) = fusion.as_deref() {
                    sem = sem.with_fusion(parse_fusion(f)?);
                }
                if chosen == "vector" {
                    cell.search_semantic(&q, &sem.with_mode(engramma_core::search::Mode::Vector))?
                } else {
                    cell.search_semantic(&q, &sem)?
                }
            };
            let hits = outcome.hits;
            if cli.json {
                let arr: Vec<serde_json::Value> = hits.iter().map(hit_json).collect();
                print_json(&serde_json::json!({
                    "query": text,
                    "mode": chosen,
                    "embed_ms": outcome.embed_ms,
                    "search_ms": outcome.search_ms,
                    "hits": arr,
                }));
            } else {
                for (i, h) in hits.iter().enumerate() {
                    if i > 0 {
                        println!();
                    }
                    println!("{}", hit_human(h, i + 1));
                }
                if hits.is_empty() {
                    println!("nessun risultato sopra soglia");
                }
            }
            Ok(())
        }
        Command::Neighbors {
            entity,
            depth,
            rels,
            include_quarantined,
        } => {
            let cell = open_cell(cli)?;
            let rel_refs: Vec<&str> = rels.iter().map(String::as_str).collect();
            let ns = cell.neighbors(entity, *depth, &rel_refs, *include_quarantined)?;
            if cli.json {
                let arr: Vec<serde_json::Value> = ns
                    .iter()
                    .map(|n| {
                        serde_json::json!({
                            "depth": n.depth, "entity": n.entity, "rel": n.rel,
                            "direction": n.direction, "note_id": n.note_id,
                            "valid_from": n.valid_from, "valid_to": n.valid_to,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "entity": entity, "neighbors": arr }));
            } else {
                for n in &ns {
                    let note = n
                        .note_id
                        .as_deref()
                        .filter(|id| id.len() >= 8)
                        .map(|id| format!(" (nota {})", &id[id.len() - 8..]))
                        .unwrap_or_default();
                    println!(
                        "{}{} {} {}{}",
                        "  ".repeat(usize::from(n.depth.saturating_sub(1))),
                        if n.direction == "->" { "-" } else { "<-" },
                        n.rel,
                        n.entity,
                        note
                    );
                }
            }
            Ok(())
        }
        Command::Path {
            a,
            b,
            include_quarantined,
        } => {
            let cell = open_cell(cli)?;
            match cell.path(a, b, *include_quarantined)? {
                Some(steps) => {
                    if cli.json {
                        let arr: Vec<serde_json::Value> = steps
                            .iter()
                            .map(|s| serde_json::json!({"from": s.from, "rel": s.rel, "to": s.to}))
                            .collect();
                        print_json(&serde_json::json!({ "from": a, "to": b, "steps": arr }));
                    } else {
                        let mut line = format!("[[{a}]]");
                        for s in &steps {
                            line.push_str(&format!(" -{}-> [[{}]]", s.rel, s.to));
                        }
                        println!("{line}");
                    }
                }
                None => {
                    if cli.json {
                        print_json(&serde_json::json!({ "from": a, "to": b, "steps": null }));
                    } else {
                        println!("nessun percorso entro 4 passi");
                    }
                }
            }
            Ok(())
        }
        Command::Tags {
            prefix,
            include_quarantined,
        } => {
            let cell = open_cell(cli)?;
            let tags = cell.tags(prefix.as_deref().unwrap_or(""), *include_quarantined)?;
            if cli.json {
                let arr: Vec<serde_json::Value> = tags
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name, "parent": t.parent, "note_count": t.note_count
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "tags": arr }));
            } else {
                for t in &tags {
                    println!("{:<4} {}", t.note_count, t.name);
                }
            }
            Ok(())
        }
        Command::Entities { prefix } => {
            let cell = open_cell(cli)?;
            let es = cell.entities(prefix.as_deref().unwrap_or(""))?;
            if cli.json {
                let arr: Vec<serde_json::Value> = es
                    .iter()
                    .map(|e| serde_json::json!({"name": e.name, "id": e.id, "note_id": e.note_id}))
                    .collect();
                print_json(&serde_json::json!({ "entities": arr }));
            } else {
                for e in &es {
                    println!("{}", e.name);
                }
            }
            Ok(())
        }
        Command::Timeline {
            entity,
            include_quarantined,
        } => {
            let cell = open_cell(cli)?;
            let tl = cell.timeline(entity, *include_quarantined)?;
            if cli.json {
                let arr: Vec<serde_json::Value> = tl.iter().map(hit_json).collect();
                print_json(&serde_json::json!({ "entity": entity, "notes": arr }));
            } else {
                for h in &tl {
                    println!(
                        "{} → {}  {} ({})",
                        h.explain.valid_from.as_deref().unwrap_or("sempre"),
                        h.explain.valid_to.as_deref().unwrap_or("ora"),
                        h.note.display_title(),
                        engramma_core::id::id8(&h.note.id),
                    );
                }
            }
            Ok(())
        }
        Command::History { id } => {
            let cell = open_cell(cli)?;
            let id = cell.resolve_note_id(id)?;
            let hist = cell.history(&id)?;
            if cli.json {
                let arr: Vec<serde_json::Value> = hist
                    .iter()
                    .map(|v| {
                        serde_json::json!({
                            "version": v.version,
                            "op_id": v.op_id.to_string(),
                            "note": note_json(&v.note),
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "id": id.to_string(), "versions": arr }));
            } else {
                for v in &hist {
                    println!(
                        "v{}  {}  {}",
                        v.version,
                        v.note.updated_at,
                        v.note.display_title()
                    );
                }
            }
            Ok(())
        }
        Command::Restore { id, version } => {
            let cell = open_cell(cli)?;
            let id = cell.resolve_note_id(id)?;
            cell.restore(&id, *version, "cli")?;
            report_ok(cli, "ripristinata", &id.to_string());
            Ok(())
        }
        Command::Import {
            dir,
            obsidian,
            no_embed,
        } => {
            let cell = open_cell(cli)?;
            let summary = if *obsidian {
                import_obsidian(&cell, dir, "cli")?
            } else {
                import_cell(&cell, dir, "cli")?
            };
            let embedded = if *no_embed {
                None
            } else {
                embed_after_write(cli, &cell)?
            };
            if cli.json {
                let mut v = serde_json::json!({
                    "imported": summary.imported,
                    "skipped": summary.skipped,
                    "errors": summary.errors,
                    "warnings": summary.warnings,
                });
                if let Some((done, remaining)) = embedded {
                    v["embedded"] = done.into();
                    v["pending"] = remaining.into();
                }
                print_json(&v);
            } else {
                if let Some((done, remaining)) = embedded {
                    println!("incorporate: {done} (in attesa: {remaining})");
                }
                println!("importate: {}", summary.imported);
                println!("saltate: {}", summary.skipped.len());
                for s in &summary.skipped {
                    println!("  - {s}");
                }
                println!("errori: {}", summary.errors.len());
                for e in &summary.errors {
                    println!("  ! {e}");
                }
                for w in &summary.warnings {
                    println!("  ⚠ {w}");
                }
            }
            Ok(())
        }
        Command::Export { dir } => {
            let cell = open_cell(cli)?;
            let s = export(&cell, dir)?;
            if cli.json {
                print_json(&serde_json::json!({
                    "written": s.written,
                    "tombstones": s.tombstones,
                    "errors": s.errors,
                }));
            } else {
                println!("esportate: {} note in {}", s.written, dir.display());
                if !s.errors.is_empty() {
                    println!("errori: {}", s.errors.len());
                }
            }
            Ok(())
        }
        Command::Model { action } => run_model(cli, action),
        Command::Embed { all } => {
            let cell = open_cell(cli)?;
            let Some(embedder) = load_embedder_or_warn(cli)? else {
                return Err(CliError::usage(
                    "no embedder configured: write <root>/config.json with \
                     {\"embedder\": {\"kind\": \"builtin\", \"model\": \"granite-embedding-97m-multilingual-r2\"}}",
                ));
            };
            let mut done_total = 0_usize;
            let mut promoted = None;
            loop {
                let report = if *all && done_total == 0 {
                    // one force pass, then normal passes settle the count
                    cell.embed_all(embedder.as_ref(), 512)?
                } else {
                    cell.embed_pending(embedder.as_ref(), 512)?
                };
                done_total += report.done;
                if report.promoted.is_some() {
                    promoted = report.promoted.clone();
                }
                if report.remaining == 0 || report.done == 0 {
                    break;
                }
            }
            if cli.json {
                let mut v = serde_json::json!({ "embedded": done_total, "pending": 0 });
                if let Some(p) = promoted {
                    v["promoted"] = p.into();
                }
                print_json(&v);
            } else {
                println!("incorporate: {done_total} note");
                if let Some(p) = promoted {
                    println!("modello pronto: {p}");
                }
            }
            Ok(())
        }
        Command::Verify => {
            let cell = open_cell(cli)?;
            let report = cell.verify()?;
            if cli.json {
                print_json(&serde_json::json!({
                    "ops": report.ops,
                    "hash_chain_ok": report.hash_chain_ok,
                    "replay_ok": report.replay_ok,
                    "fts_ok": report.fts_ok,
                    "mismatch": report.mismatch,
                    "ok": report.ok(),
                }));
            } else if report.ok() {
                println!(
                    "registro verificato: {} operazioni, catena hash, ricostruzione e indice FTS a posto",
                    report.ops
                );
            } else {
                return Err(CliError::internal(format!(
                    "verifica fallita: chain={} replay={} {}",
                    report.hash_chain_ok,
                    report.replay_ok,
                    report.mismatch.unwrap_or_default()
                )));
            }
            Ok(())
        }
        Command::Mcp { cells } => {
            if cells.is_empty() {
                return Err(CliError::usage(
                    "`engramma mcp` needs --cells <a,b>: without it no cell would be exposed",
                ));
            }
            let names: Vec<String> = cells.clone();
            engramma_mcp::run(root(cli), &names)
                .map_err(|e| CliError::internal(format!("mcp: {e}")))
        }
    }
}

fn run_cell(cli: &Cli, action: &CellAction) -> Result<(), CliError> {
    match action {
        CellAction::List => {
            let dir = cells_dir(&root(cli));
            let mut names: Vec<String> = Vec::new();
            if dir.is_dir() {
                for entry in std::fs::read_dir(&dir)? {
                    let entry = entry?;
                    let p = entry.path();
                    if p.extension().is_some_and(|e| e == "sqlite")
                        && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    {
                        names.push(stem.to_string());
                    }
                }
            }
            names.sort();
            if cli.json {
                print_json(&serde_json::json!({ "cells": names }));
            } else {
                for n in names {
                    println!("{n}");
                }
            }
            Ok(())
        }
        CellAction::Create { name } => {
            let path = cell_path(cli, name)?;
            Cell::create(&path, name, "cli")?;
            report_ok(cli, "creata", name);
            Ok(())
        }
        CellAction::Drop { name } => {
            let path = cell_path(cli, name)?;
            if !path.exists() {
                return Err(CliError::not_found(format!("cell {name} not found")));
            }
            for suffix in ["", "-wal", "-shm"] {
                let mut p = path.clone();
                p.set_file_name(format!("{name}.sqlite{suffix}"));
                std::fs::remove_file(&p).ok();
            }
            report_ok(cli, "eliminata", name);
            Ok(())
        }
    }
}

fn report_ok(cli: &Cli, verb: &str, what: &str) {
    if cli.json {
        print_json(&serde_json::json!({ "ok": what }));
    } else {
        println!("{verb}: {what}");
    }
}

/// Temp file inside the system temp dir with an engramma prefix.
fn tempfile_in_engramma_tmp() -> Result<PathBuf, CliError> {
    let dir = std::env::temp_dir().join("engramma-edit");
    std::fs::create_dir_all(&dir)?;
    let name = format!(
        "{}.md",
        engramma_core::id::new_ulid().map_err(CliError::from)?
    );
    Ok(dir.join(name))
}

// ---------------------------------------------------------------------------
// embedding: CLI plumbing (models, embed passes, search helpers)
// ---------------------------------------------------------------------------

/// The configured embedder, or None with a stderr note when configuration is
/// missing/broken. A cell without an embedder keeps the v0.1 search.
fn load_embedder_or_warn(
    cli: &Cli,
) -> Result<Option<std::sync::Arc<dyn engramma_core::embed::Embedder>>, CliError> {
    match engramma_embed::config::load_embedder(&root(cli)) {
        Ok(Some(e)) => Ok(Some(e)),
        Ok(None) => Ok(None),
        Err(e) => {
            eprintln!("engramma: embedder configurato ma non utilizzabile: {e}");
            Ok(None)
        }
    }
}

/// After a write command (add/import): embed what the cell is holding, when
/// an embedder is configured and matches the cell's target model. Returns
/// `(done, remaining)` or None when nothing ran.
fn embed_after_write(cli: &Cli, cell: &Cell) -> Result<Option<(usize, usize)>, CliError> {
    let Some(embedder) = load_embedder_or_warn(cli)? else {
        return Ok(None);
    };
    // Wrong model (or no target yet): leave the notes pending, silently —
    // `engramma embed` will say what to do.
    let target_ok = cell
        .models()
        .ok()
        .and_then(|ms| {
            ms.into_iter()
                .find(|m| m.state != engramma_core::ModelState::Retired)
        })
        .is_some_and(|m| m.model == embedder.model_id());
    if !target_ok {
        return Ok(None);
    }
    let report = cell
        .embed_pending(embedder.as_ref(), 512)
        .map_err(CliError::from)?;
    Ok(Some((report.done, report.remaining)))
}

/// `engramma model list|pull|use`.
fn run_model(cli: &Cli, action: &ModelAction) -> Result<(), CliError> {
    let model_root = engramma_embed::spec::model_root();
    match action {
        ModelAction::List => {
            // builtin specs + user folders, with the cell's state when known
            let mut specs = engramma_embed::spec::ModelSpec::builtin_models();
            // user folders (e.g. EmbeddingGemma, licence Gemma: never bundled)
            if let Ok(entries) = std::fs::read_dir(&model_root) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if specs.iter().any(|s| s.id == name) {
                        continue;
                    }
                    let dir = entry.path();
                    let candidates = [
                        dir.join("manifest.json"),
                        dir.join(
                            std::fs::read_dir(&dir)
                                .ok()
                                .and_then(|mut it| {
                                    it.next()
                                        .and_then(|e| e.ok())
                                        .map(|e| e.path().join("manifest.json"))
                                })
                                .unwrap_or_else(|| dir.join("manifest.json")),
                        ),
                    ];
                    if candidates.iter().any(|p| p.is_file()) {
                        match engramma_embed::spec::resolve(&model_root, &name) {
                            Ok((spec, _)) => specs.push(spec),
                            Err(e) => eprintln!("engramma: {name}: {e}"),
                        }
                    }
                }
            }
            let states = open_cell(cli)
                .ok()
                .and_then(|cell| cell.models().ok())
                .unwrap_or_default();
            let mut lines: Vec<serde_json::Value> = Vec::new();
            for spec in &specs {
                let present = spec
                    .files
                    .iter()
                    .all(|f| spec.file_path(&model_root, f).is_file());
                let state = states
                    .iter()
                    .find(|m| m.model == spec.model_id())
                    .map(|m| m.state.as_str().to_string());
                lines.push(serde_json::json!({
                    "id": spec.id,
                    "model_id": spec.model_id(),
                    "dim": spec.dim,
                    "max_tokens": spec.max_tokens,
                    "bytes": spec.files.iter().map(|f| f.bytes).sum::<u64>(),
                    "downloaded": present,
                    "state": state,
                }));
            }
            if cli.json {
                print_json(
                    &serde_json::json!({ "model_root": model_root.display().to_string(), "models": lines }),
                );
            } else {
                println!("modelli in {}", model_root.display());
                for l in &lines {
                    let state = l["state"]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "—".into());
                    println!(
                        "{}  {} dim, {} MB, {}, stato: {}",
                        l["id"].as_str().unwrap_or("?"),
                        l["dim"],
                        l["bytes"].as_u64().unwrap_or(0) / 1_000_000,
                        if l["downloaded"].as_bool().unwrap_or(false) {
                            "scaricato"
                        } else {
                            "da scaricare"
                        },
                        state,
                    );
                }
            }
            Ok(())
        }
        ModelAction::Pull { id } => {
            // Only the pinned builtin specs are downloadable.
            let spec = engramma_embed::spec::ModelSpec::builtin_models()
                .into_iter()
                .find(|s| s.id == *id || s.model_id().starts_with(&format!("{id}@")))
                .ok_or_else(|| {
                    CliError::usage(format!(
                        "`{id}` is not a downloadable model (see `engramma model list`)"
                    ))
                })?;
            let report = engramma_embed::pull::pull(
                &model_root,
                &spec,
                None,
                std::time::Duration::from_secs(600),
            )
            .map_err(|e| CliError::internal(e.to_string()))?;
            if cli.json {
                print_json(&serde_json::json!({
                    "pulled": spec.model_id(),
                    "downloaded": report.downloaded,
                    "skipped": report.skipped,
                }));
            } else {
                println!("scaricato: {} in {}", spec.model_id(), model_root.display());
            }
            Ok(())
        }
        ModelAction::Use { id } => {
            let (spec, _dir) = engramma_embed::spec::resolve(&model_root, id)
                .map_err(|e| CliError::usage(e.to_string()))?;
            let cell = open_cell(cli)?;
            cell.add_model(&spec.model_id(), spec.dim)?;
            // The configuration follows the choice: <root>/config.json now
            // embeds with this model (remote embedders are configured by
            // writing the file by hand, see docs/cli.md).
            let cfg_path = root(cli).join("config.json");
            let cfg = engramma_embed::config::EmbedderConfig {
                kind: "builtin".into(),
                model: spec.id.clone(),
                url: None,
                token_env: None,
                dim: None,
                max_tokens: None,
            };
            std::fs::write(
                &cfg_path,
                serde_json::to_string_pretty(&serde_json::json!({ "embedder": cfg }))
                    .unwrap_or_default(),
            )?;
            if cli.json {
                print_json(&serde_json::json!({
                    "model": spec.model_id(),
                    "state": "building",
                    "config": cfg_path.display().to_string(),
                }));
            } else {
                println!(
                    "modello {} in costruzione (config in {}): `engramma embed` lo riempie, \
                     la ricerca continua con il modello attuale",
                    spec.model_id(),
                    cfg_path.display()
                );
            }
            Ok(())
        }
    }
}

/// Parse `--fusion rrf|convex-0.3|convex-0.5|convex-0.7`.
fn parse_fusion(s: &str) -> Result<engramma_core::search::Fusion, CliError> {
    match s {
        "rrf" => Ok(engramma_core::search::Fusion::Rrf { k: 60 }),
        "convex-0.3" | "convex-0,3" => Ok(engramma_core::search::Fusion::Convex { alpha: 0.3 }),
        "convex-0.5" | "convex-0,5" => Ok(engramma_core::search::Fusion::Convex { alpha: 0.5 }),
        "convex-0.7" | "convex-0,7" => Ok(engramma_core::search::Fusion::Convex { alpha: 0.7 }),
        other => Err(CliError::usage(format!(
            "unknown --fusion `{other}` (rrf, convex-0.3, convex-0.5, convex-0.7)"
        ))),
    }
}
