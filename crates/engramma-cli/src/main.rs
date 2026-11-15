// SPDX-License-Identifier: Apache-2.0

//! `engramma`: the command line. Layout of a data root:
//! `<root>/cells/<name>.sqlite` (default root: `~/.engramma`).

mod mailboxcli;
mod output;
mod review;
mod synccli;

// v0.4.4 (P1) → v0.4.5 (P2): the executable picks the allocator, and it
// picks it PER PLATFORM: mimalloc on macOS (the system allocator keeps
// the freed pages of the work resident; mimalloc holds far less), the
// system allocator on Linux (glibc returns them: 637 vs 1166 MB at
// rest, REPORT v0.4.5). The libraries stay allocator-neutral — whoever
// links them chooses.
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::port::{export, import_cell, import_obsidian};
use engramma_core::search::Query;
use engramma_core::{Cell, CellError, Confidence, NotePatch, Status, Validity};
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
    /// Right to be forgotten: the note, its versions, its edges and the
    /// payloads of its operations go, for good (v0.3)
    Purge {
        id: String,
        /// Why (kept in the log; never the content)
        #[arg(long)]
        reason: String,
        /// Skip the interactive confirmation (for scripts)
        #[arg(long)]
        yes: bool,
    },
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
        /// Drop `support: "weak"` results (vector-only, thin cosine margin:
        /// near in meaning, unconfirmed by any text match)
        #[arg(long)]
        strict: bool,
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
    /// Versions of a note (v0.5: who proposed, who decided, and why)
    History {
        id: String,
        /// Line-by-line diff between consecutive versions
        #[arg(long)]
        diff: bool,
    },
    /// The proposals queue and its decisions (v0.5)
    Proposals {
        /// Only this state (pending | accepted | rejected | reverted)
        #[arg(long)]
        state: Option<String>,
        /// Only this proposer (exact or prefix, e.g. `mcp:`)
        #[arg(long)]
        by: Option<String>,
        #[command(subcommand)]
        action: Option<ProposalsAction>,
    },
    /// The decisions registry: who, when, what, why (v0.5)
    Decisions {
        /// Only this actor (exact or prefix, e.g. `reviewer:`)
        #[arg(long)]
        by: Option<String>,
        /// Decided since this date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Decided until this date (YYYY-MM-DD, inclusive)
        #[arg(long)]
        until: Option<String>,
        /// Only this standing (active | reverted)
        #[arg(long)]
        state: Option<String>,
    },
    /// Cancel decisions — the git revert of the registry (v0.5)
    Revert {
        /// A decision op id (or its last-8 prefix)
        decision: Option<String>,
        /// Bulk: every decision of this actor (prefix), e.g. `reviewer:x`
        #[arg(long)]
        by: Option<String>,
        /// Bulk: only decisions taken since this date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Bulk: only decisions taken until this date (YYYY-MM-DD)
        #[arg(long)]
        until: Option<String>,
        /// Show what would happen, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive confirmation (for scripts)
        #[arg(long)]
        yes: bool,
    },
    /// The cell's policy: who may propose, how far the machine may go (v0.5)
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// The local review page: the queue, the decisions, the notes
    /// (127.0.0.1 only, Ctrl-C to stop) (v0.5)
    Review {
        /// Port (default: any free one)
        #[arg(long)]
        port: Option<u16>,
        /// Open the browser at the printed address
        #[arg(long)]
        open: bool,
    },
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
    /// Manage the devices of this data root (v0.6)
    Devices {
        #[command(subcommand)]
        action: DevicesAction,
    },
    /// Synchronize this data root's cells (v0.6)
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },
    /// The blind mailbox node (v0.6)
    Mailbox {
        #[command(subcommand)]
        action: MailboxAction,
    },
    /// Verify the log (hash chain + replay)
    Verify,
    /// Compact the log: register a checkpoint (no second copy of the cell)
    /// and forget the old payloads
    Compact {
        /// Keep the last N operations uncompacted (default 1000)
        #[arg(long, default_value_t = 1_000)]
        keep_last: i64,
        /// Also keep a snapshot FILE next to the cell (the v0.3.1 behavior:
        /// the logical state; purge scrubs it with the cell)
        #[arg(long)]
        keep_snapshot: bool,
    },
    /// Consolidation candidates: near duplicates and probable
    /// contradictions — the engine proposes, the application decides (v0.3)
    Consolidate {
        /// Max candidates per kind
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// The markdown view: two-way sync with a folder of notes (v0.3)
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
    /// The OAuth grants of `mcp --http`: what waits for you, what you
    /// approved, and how to cut it off (v0.5.2)
    Oauth {
        #[command(subcommand)]
        action: OauthAction,
    },
    /// Start the MCP server (stdio, or HTTP with --http)
    Mcp {
        /// Exposed cells (comma-separated; without this flag no cell is exposed)
        #[arg(long, value_delimiter = ',')]
        cells: Vec<String>,
        /// Cells that also accept PROPOSALS through `memory_propose`
        /// (comma-separated; a subset of --cells)
        #[arg(long, value_delimiter = ',')]
        write: Vec<String>,
        /// Who signs the proposals (`mcp:<actor>`); required with --write
        /// (stdio only: over HTTP the actor is the grant's label)
        #[arg(long)]
        actor: Option<String>,
        /// Serve over HTTP on this loopback address instead of stdio
        /// (v0.5.2): `--http 127.0.0.1:8787`
        #[arg(long)]
        http: Option<String>,
        /// The https URL the tunnel publishes for this server (required
        /// with --http)
        #[arg(long)]
        public_url: Option<String>,
        /// Accept RFC 7591 dynamic client registration (off by default)
        #[arg(long)]
        allow_dcr: bool,
        /// Client compatibility shims: `chatgpt` adds the fixed-schema
        /// `search`/`fetch` tools its connectors want
        #[arg(long)]
        compat: Option<String>,
    },
}

#[derive(Subcommand)]
enum OauthAction {
    /// The authorization requests waiting for your decision
    Pending,
    /// Approve one: the code is the one the user's browser shows
    Approve {
        /// The approval code shown in the browser (`XXXX-XXXX`)
        code: String,
        /// How this connection's proposals sign: `mcp:<label>`
        /// ([a-z0-9-]{1,32})
        #[arg(long)]
        label: String,
        /// Keep only these cells of what the server offers (comma-separated)
        #[arg(long, value_delimiter = ',')]
        cells: Vec<String>,
        /// Keep only these scopes (memory:read, memory:propose)
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
        /// Skip the interactive confirmation (for scripts)
        #[arg(long)]
        yes: bool,
    },
    /// Refuse one (or, with --all, every pending request at once)
    Deny {
        /// The code the user's browser shows
        code: Option<String>,
        /// Refuse EVERY pending request (the flood answer)
        #[arg(long)]
        all: bool,
    },
    /// The grants: label, client, cells, scopes, when, last use
    Grants,
    /// Revoke a grant (or all of them): its tokens die at the next request
    Revoke {
        /// The grant's label
        grant: Option<String>,
        /// Every grant
        #[arg(long)]
        all: bool,
    },
    /// The event log: approvals, refusals, revocations, alarms
    Log,
}

#[derive(Subcommand)]
enum ProposalsAction {
    /// One proposal in full: note, proposer, decision, reviewer metadata
    Show { id: String },
    /// Accept it: the note becomes active (with --edit, fix the text first)
    Accept {
        id: String,
        /// Edit the text in $EDITOR before accepting
        #[arg(long)]
        edit: bool,
    },
    /// Reject it: the lapide keeps the text, the revert can undo
    Reject {
        id: String,
        /// Why
        #[arg(long)]
        reason: Option<String>,
    },
    /// Retry the FAILED automatic reviews (never the escalated ones)
    Review,
}

#[derive(Subcommand)]
enum PolicyAction {
    /// The current policy
    Show,
    /// Change it: `write=... reviewer=... min_confidence=... kinds=a,b
    /// allow_supersede=... max_auto_per_hour=...` (only from here — never
    /// from the web, never from MCP)
    Set { pairs: Vec<String> },
}

#[derive(Subcommand)]
enum VaultAction {
    /// Export the cell into the folder and bind every note to its file
    Init { dir: PathBuf },
    /// One reconciliation, with the report
    Sync {
        dir: PathBuf,
        /// Proceed even when too many files vanished at once
        #[arg(long)]
        allow_deletions: bool,
    },
    /// Reconcile on filesystem events (500 ms debounce) and every 60 s;
    /// Ctrl-C exits
    Watch { dir: PathBuf },
    /// Detach a view (v0.4): the cell forgets it and ITS bindings — the
    /// files stay on disk, they were never the engine's. Accepts the
    /// folder or the view id (even just its first 8 characters).
    Unbind { dir_or_view_id: String },
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

/// The relay flags every command that binds an endpoint carries (R2):
/// `--relay <url>` for one's own, `--no-relay` for direct connections only,
/// nothing for the saved choice (n0's public relays when nothing is saved).
#[derive(clap::Args)]
struct RelayArgs {
    /// A relay of one's own (https://host:port)
    #[arg(long, conflicts_with = "no_relay")]
    relay: Option<String>,
    /// No relay: direct connections only
    #[arg(long)]
    no_relay: bool,
}

/// The flags to the sync layer's resolver: `Some` when given explicitly
/// (it wins over what is saved), `None` when the saved choice rules.
fn relay_flag(args: &RelayArgs) -> Option<engramma_sync::relay::RelayChoice> {
    if let Some(url) = &args.relay {
        Some(engramma_sync::relay::RelayChoice::Own(url.clone()))
    } else if args.no_relay {
        Some(engramma_sync::relay::RelayChoice::Disabled)
    } else {
        None
    }
}

#[derive(Subcommand)]
enum DevicesAction {
    /// Create the device key (or show the existing one)
    Init,
    /// Bind the pairing endpoint and print the invite ticket with the real
    /// address; waits (10 minutes, one use) for the device that enters
    Invite {
        #[command(flatten)]
        relay: RelayArgs,
    },
    /// Print a short pairing code
    Code {
        #[command(flatten)]
        relay: RelayArgs,
    },
    /// Join with a ticket or a code
    Join {
        /// The ticket (textual) or the code (NUMBER-WORD-WORD)
        ticket_or_code: String,
        /// This device's name
        #[arg(long)]
        name: String,
        #[command(flatten)]
        relay: RelayArgs,
    },
    /// List the devices (name, short id, last contact, epochs)
    List,
    /// Remove a device (rotates every shared cell's key)
    Remove { name: String },
    /// The device list's own log
    Log,
}

#[derive(Subcommand)]
enum SyncAction {
    /// Share a cell with the other devices (adopt + key epoch 0)
    Share { cell: String },
    /// Stop sharing a cell from here (copies elsewhere stay)
    Unshare { cell: String },
    /// One pass with every reachable device and the mailbox, then exit
    Run {
        #[command(flatten)]
        relay: RelayArgs,
    },
    /// Serve: sync on every local write and every 5 minutes
    Serve {
        #[command(flatten)]
        relay: RelayArgs,
    },
    /// Per cell: devices, last session, pending ops, frontier, epoch
    Status,
    /// The conflict copies this copy holds
    Conflicts,
}

#[derive(Subcommand)]
enum MailboxAction {
    /// Run the blind mailbox node
    Serve {
        /// The node's data root
        #[arg(long)]
        root: PathBuf,
        #[command(flatten)]
        relay: RelayArgs,
    },
    /// Add the mailbox to this data root's device list
    Add { ticket: String },
    /// Remove the mailbox
    Remove,
    /// The mailbox's health
    Status,
}

#[derive(Subcommand)]
enum CellAction {
    /// List cells
    List,
    /// Create a cell
    Create { name: String },
    /// Forget a whole cell: its notes, its files (WAL and snapshots
    /// included) and the note files of its bound views (v0.4.2) —
    /// irreversible
    Drop {
        name: String,
        /// Why (the caller's audit trail; the engine keeps no registry)
        #[arg(long)]
        reason: String,
    },
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
    let cell = open_cell_armed(&path)?;
    Ok(cell)
}

/// Open a cell and arm the sync era's signer when this root has a device
/// key: every LOCAL write on a shared cell must carry its device and
/// signature, or the peers' gate refuses it (rightly).
fn open_cell_armed(path: &std::path::Path) -> Result<Cell, CliError> {
    let cell = Cell::open(path).map_err(|e| {
        if matches!(e, CellError::SchemaTooNew { .. }) {
            CliError::from(e)
        } else {
            CliError::internal(format!(
                "cell `{}`: {e} (run `engramma init` first?)",
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            ))
        }
    })?;
    // the root two levels up from cells/<name>.sqlite
    if let Some(root) = path.parent().and_then(|c| c.parent())
        && let Ok(key) = engramma_sync::device::DeviceKey::load_or_create(root)
    {
        cell.set_signer(std::sync::Arc::new(
            engramma_sync::arrival::DeviceSigner::new(&key),
        ))
        .ok();
    }
    Ok(cell)
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
        Command::Purge { id, reason, yes } => {
            let cell = open_cell(cli)?;
            let note_id = cell.resolve_note_id(id)?;
            let note = cell.get(&note_id)?;
            if !*yes {
                println!(
                    "stai per purgare «{}» ({}) IRREVERSIBILMENTE:",
                    note.display_title(),
                    engramma_core::id::id8(&note_id)
                );
                println!("  la nota, le sue versioni, gli archi che dichiara, i vettori");
                println!("  e i payload delle sue operazioni spariranno dal registro.");
                println!(
                    "Le esportazioni fatte prima di ora restano fuori dal controllo del motore."
                );
                print!("procedo? [s/N] ");
                use std::io::Write as _;
                std::io::stdout().flush().ok();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| CliError::internal(format!("stdin: {e}")))?;
                if !answer.trim().eq_ignore_ascii_case("s") {
                    if !cli.json {
                        println!("annullato");
                    }
                    return Ok(());
                }
            }
            // v0.4.1 (P0.2): the purge that knows about the views lives in
            // one place now — `engramma_vault::purge_everywhere`, shared
            // with the Node binding. The messages and the exit codes are
            // the ones the CLI has printed since v0.3.2/v0.4.
            let (report, problems) = engramma_vault::purge_everywhere(
                &cell, &note_id, reason, "cli-user",
            )
            .map_err(|e| match e {
                engramma_vault::VaultError::Cell(c) => c.into(),
                other => CliError::internal(format!("purge: {other}")),
            })?;
            for problem in &problems {
                eprintln!("engramma: {}", problem.reason);
            }
            if cli.json {
                print_json(&serde_json::json!({
                    "purged": note_id.to_string(),
                    "forgotten_payloads": report.forgotten_payloads,
                    "scrubbed": report.scrubbed,
                    "pending": report.pending,
                }));
            } else {
                println!(
                    "purgata: {} ({} payload dimenticati)",
                    note_id, report.forgotten_payloads
                );
            }
            if !report.pending.is_empty() {
                // the purge is not done: say so, with its own exit code
                let why = report
                    .pending
                    .iter()
                    .map(|p| match *p {
                        "wal" => "il WAL non è stato troncato (un lettore teneva \
                                  la cella aperta; verrà ritentato alla prossima \
                                  scrittura)"
                            .to_string(),
                        "vault" => "una vista markdown non era raggiungibile \
                                    (la `vault sync` finisce il lavoro, o \
                                    `vault unbind` la sgancia)"
                            .to_string(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(CliError::incomplete_forgetting(format!(
                    "oblio incompleto ({}): {why}",
                    report.pending.join(", ")
                )));
            }
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
            strict,
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
            if let Ok(spec) = std::env::var("ENGRAMMA_GRAPH")
                && !spec.is_empty()
            {
                let o = parse_graph_env(&spec)?;
                q.weights.graph_policy = o.policy;
                q.weights.graph_weight = o.weight;
                q.weights.graph_seeding = o.seeding;
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
                    graph_pushed_edges: 0,
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
            let mut hits = outcome.hits;
            if *strict {
                // a weak result is a signal to verify, not an answer
                hits.retain(|h| h.explain.support != Some("weak"));
            }
            if cli.json {
                let arr: Vec<serde_json::Value> = hits.iter().map(hit_json).collect();
                print_json(&serde_json::json!({
                    "query": text,
                    "mode": chosen,
                    "embed_ms": outcome.embed_ms,
                    "search_ms": outcome.search_ms,
                    "graph_pushed_edges": outcome.graph_pushed_edges,
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
        Command::History { id, diff } => {
            let cell = open_cell(cli)?;
            let id = cell.resolve_note_id(id)?;
            run_history(cli, &cell, &id, *diff)
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
        Command::Compact {
            keep_last,
            keep_snapshot,
        } => {
            let cell = open_cell(cli)?;
            let head: i64 = cell
                .debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM ops")
                .ok()
                .and_then(|v| v.first().and_then(|s| s.parse().ok()))
                .unwrap_or(0);
            let target = head - keep_last;
            if target >= 1 {
                // the payloads up to `target` are forgotten, the last
                // `keep_last` stay replayable; the restart point is a
                // checkpoint without a file unless --keep-snapshot
                let report = cell.compact(
                    target,
                    &engramma_core::cell::CompactOptions {
                        keep_snapshot: *keep_snapshot,
                    },
                )?;
                if cli.json {
                    print_json(&serde_json::json!({
                        "cut_seq": report.cut_seq,
                        "head_seq": report.head_seq,
                        "head_hash": report.head_hash,
                        "state_digest": report.state_digest,
                        "snapshot": report.snapshot.as_ref()
                            .map(|p| p.display().to_string()),
                        "bytes_before": report.bytes_before,
                        "bytes_after": report.bytes_after,
                    }));
                } else if let Some(snap) = &report.snapshot {
                    println!(
                        "compattata fino a seq {} (testa {}): {} \
                         (cella+WAL+istantanee: {} → {} byte)",
                        report.cut_seq,
                        report.head_seq,
                        snap.display(),
                        report.bytes_before,
                        report.bytes_after
                    );
                } else {
                    println!(
                        "compattata fino a seq {} (testa {}): punto di controllo \
                         senza file (cella+WAL+istantanee: {} → {} byte)",
                        report.cut_seq, report.head_seq, report.bytes_before, report.bytes_after
                    );
                }
            } else if cli.json {
                print_json(&serde_json::json!({ "compacted": false, "seq": head }));
            } else {
                println!(
                    "niente da compattare: {} operazioni, ne tengo {keep_last}",
                    head
                );
            }
            Ok(())
        }
        Command::Devices { action } => devices_run(cli, action),
        Command::Sync { action } => sync_run(cli, action),
        Command::Mailbox { action } => mailbox_run(cli, action),
        Command::Verify => {
            let cell = open_cell(cli)?;
            let report = cell.verify()?;
            // Vector coverage per model (derived data: reported, never a
            // verification failure) — v0.2.1: the REPORT v0.2 claimed it,
            // the command never showed it.
            let coverage: Vec<serde_json::Value> = report
                .coverage
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "model": c.model,
                        "state": c.state.as_str(),
                        "covered": c.covered,
                        "living": c.living,
                    })
                })
                .collect();
            // v0.3.2 (P0.2): say WHAT was verified — a checkpoint compaction
            // leaves no snapshot to replay on, and silence would imply the
            // full reconstruction.
            let replay_json = match &report.replay {
                engramma_core::cell::Replay::Full => serde_json::json!({"kind": "full"}),
                engramma_core::cell::Replay::FromSnapshot { seq } => {
                    serde_json::json!({"kind": "from_snapshot", "seq": seq})
                }
                engramma_core::cell::Replay::ChainOnly { since_seq } => {
                    serde_json::json!({"kind": "chain_only", "since_seq": since_seq})
                }
            };
            if cli.json {
                print_json(&serde_json::json!({
                    "ops": report.ops,
                    "replay": replay_json,
                    "hash_chain_ok": report.hash_chain_ok,
                    "replay_ok": report.replay_ok,
                    "fts_ok": report.fts_ok,
                    "mismatch": report.mismatch,
                    "coverage": coverage,
                    "ok": report.ok(),
                }));
            } else if report.ok() {
                match &report.replay {
                    engramma_core::cell::Replay::Full => println!(
                        "registro verificato: {} operazioni, catena hash, ricostruzione e indice FTS a posto",
                        report.ops
                    ),
                    engramma_core::cell::Replay::FromSnapshot { seq } => {
                        println!(
                            "registro verificato: {} operazioni, catena hash, ricostruzione \
                             (coda da istantanea di seq {seq}) e indice FTS a posto",
                            report.ops
                        );
                    }
                    engramma_core::cell::Replay::ChainOnly { since_seq } => {
                        println!(
                            "registro verificato: {} operazioni, catena hash e indici \
                             derivati a posto",
                            report.ops
                        );
                        println!(
                            "rigioco: solo catena da seq {since_seq} (compattata senza istantanea)"
                        );
                    }
                }
                for c in &report.coverage {
                    println!(
                        "copertura vettori: {} [{}] {}/{} note",
                        c.model,
                        c.state.as_str(),
                        c.covered,
                        c.living
                    );
                }
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
        Command::Consolidate { limit } => {
            let cell = open_cell(cli)?;
            let out =
                cell.consolidation_candidates(*limit, &engramma_core::Consolidation::default())?;
            if cli.json {
                let arr: Vec<serde_json::Value> = out
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "kind": c.kind.as_str(),
                            "a": c.a.to_string(),
                            "b": c.b.to_string(),
                            "detail": c.detail,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "candidates": arr }));
            } else {
                for c in &out {
                    let (ta, tb) = (cell.get(&c.a), cell.get(&c.b));
                    let title = |r: Result<engramma_core::Note, _>| {
                        r.map(|n| n.display_title().to_string())
                            .unwrap_or_else(|_| "?".into())
                    };
                    println!(
                        "[{}] {} ({}) ↔ {} ({})
  {}",
                        c.kind.as_str(),
                        title(ta),
                        engramma_core::id::id8(&c.a),
                        title(tb),
                        engramma_core::id::id8(&c.b),
                        c.detail
                    );
                }
                if out.is_empty() {
                    println!("nessun candidato");
                }
            }
            Ok(())
        }
        Command::Proposals { state, by, action } => run_proposals(cli, state, by, action),
        Command::Decisions {
            by,
            since,
            until,
            state,
        } => run_decisions(cli, by, since, until, state),
        Command::Revert {
            decision,
            by,
            since,
            until,
            dry_run,
            yes,
        } => run_revert(cli, decision, by, since, until, *dry_run, *yes),
        Command::Policy { action } => run_policy(cli, action),
        Command::Review { port, open } => {
            let cell = open_cell(cli)?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| CliError::internal(format!("tokio: {e}")))?;
            rt.block_on(review::run(cell, *port, *open))
                .map_err(|e| CliError::internal(format!("review: {e}")))
        }
        Command::Oauth { action } => run_oauth(cli, action),
        Command::Vault { action } => run_vault(cli, action),
        Command::Mcp {
            cells,
            write,
            actor,
            http,
            public_url,
            allow_dcr,
            compat,
        } => {
            if cells.is_empty() {
                return Err(CliError::usage(
                    "`engramma mcp` needs --cells <a,b>: without it no cell would be exposed",
                ));
            }
            if !write.is_empty() && actor.is_none() && http.is_none() {
                return Err(CliError::usage(
                    "`--write` needs `--actor <name>`: proposals must be attributable",
                ));
            }
            for w in write {
                if !cells.contains(w) {
                    return Err(CliError::usage(format!(
                        "`--write {w}` names a cell that is not in `--cells`"
                    )));
                }
            }
            if let Some(spec) = compat.as_deref()
                && spec != "chatgpt"
            {
                return Err(CliError::usage(format!(
                    "`--compat {spec}` non esiste (per ora solo `chatgpt`)"
                )));
            }
            // v0.5.3 (P2): the wrappers are a per-grant HTTP matter — over
            // stdio there is no grant to bind them to, and no ChatGPT
            // either: the flag was silently ignored before
            if compat.is_some() && http.is_none() {
                return Err(CliError::usage(
                    "`--compat chatgpt` va con `--http`: su stdio non esiste il                      grant (il client ChatGPT) a cui legare gli strumenti di                      compatibilità",
                ));
            }
            if http.is_some() && actor.is_some() {
                return Err(CliError::usage(
                    "`--actor` va con lo stdio: su HTTP l'attore nasce dal grant \
                     (mcp:<etichetta>, scelto con `engramma oauth approve --label`)",
                ));
            }
            if let Some(addr) = http {
                return run_mcp_http(
                    cli,
                    cells.clone(),
                    write.clone(),
                    addr,
                    public_url.clone(),
                    *allow_dcr,
                    compat.is_some(),
                );
            }
            let names: Vec<String> = cells.clone();
            let writable: Vec<String> = write.clone();
            engramma_mcp::run(root(cli), &names, &writable, actor.as_deref())
                .map_err(|e| CliError::internal(format!("mcp: {e}")))
        }
    }
}

fn open_vault(cli: &Cli, dir: &Path) -> Result<engramma_vault::Vault, CliError> {
    let cell = open_cell(cli)?;
    engramma_vault::Vault::open(cell, dir).map_err(|e| CliError::internal(format!("vault: {e}")))
}

fn run_vault(cli: &Cli, action: &VaultAction) -> Result<(), CliError> {
    match action {
        VaultAction::Init { dir } => {
            let vault = open_vault(cli, dir)?;
            let report = vault
                .init()
                .map_err(|e| CliError::internal(format!("vault init: {e}")))?;
            if cli.json {
                print_json(&serde_json::json!({
                    "root": vault.root().display().to_string(),
                    "warnings": report.warnings,
                }));
            } else {
                println!("vault inizializzato: {}", vault.root().display());
            }
            Ok(())
        }
        VaultAction::Sync {
            dir,
            allow_deletions,
        } => {
            let vault = open_vault(cli, dir)?;
            let report = vault
                .sync_once(*allow_deletions)
                .map_err(|e| CliError::internal(format!("vault sync: {e}")))?;
            if cli.json {
                print_json(&serde_json::json!({
                    "created": report.created,
                    "updated_from_file": report.updated_from_file,
                    "updated_files": report.updated_files,
                    "conflicts": report.conflicts,
                    "deleted": report.deleted,
                    "moved": report.moved,
                    "errors": report.errors,
                    "warnings": report.warnings,
                }));
            } else {
                print_sync_report(&report);
            }
            Ok(())
        }
        VaultAction::Watch { dir } => {
            let vault = open_vault(cli, dir)?;
            let handle = vault
                .watch(500, std::time::Duration::from_secs(60))
                .map_err(|e| CliError::internal(format!("vault watch: {e}")))?;
            if !cli.json {
                println!(
                    "in ascolto su {} (debounce 500 ms, scansione ogni 60 s, Ctrl-C per uscire)",
                    vault.root().display()
                );
            }
            // one reconciliation right away, then wait for Ctrl-C
            let report = vault
                .sync_once(false)
                .map_err(|e| CliError::internal(format!("{e}")))?;
            if !cli.json && !report.is_noop() {
                print_sync_report(&report);
            }
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .map_err(|e| CliError::internal(format!("tokio: {e}")))?;
            rt.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
            });
            if !cli.json {
                println!("chiudo");
            }
            handle
                .stop()
                .map_err(|e| CliError::internal(format!("watch stop: {e}")))?;
            Ok(())
        }
        VaultAction::Unbind { dir_or_view_id } => {
            let cell = open_cell(cli)?;
            let arg = dir_or_view_id.trim();
            // a folder (canonicalized when it exists) first, then the view
            // id — full, or its LAST 8 characters (v0.4.1, P2.9: the short
            // id is the random part, like the notes'; two views minted
            // minutes apart share the first 8)
            let by_path = std::fs::canonicalize(arg)
                .ok()
                .and_then(|c| cell.vault_view_by_root(&c.to_string_lossy()).ok().flatten());
            let short_of = |vid: &str| -> String {
                ulid::Ulid::from_string(vid)
                    .map(|u| engramma_core::id::id8(&u))
                    .unwrap_or_else(|_| vid.split_at(vid.len().saturating_sub(8)).1.to_string())
            };
            let view = match by_path {
                Some(v) => Some(v),
                None => {
                    let hits: Vec<_> = cell
                        .vault_views()?
                        .into_iter()
                        .filter(|v| {
                            v.view_id == arg || (arg.len() == 8 && v.view_id.ends_with(arg))
                        })
                        .collect();
                    match hits.len() {
                        1 => Some(hits[0].clone()),
                        0 => None,
                        _ => {
                            let list = hits
                                .iter()
                                .map(|v| format!("{} ({})", v.root, short_of(&v.view_id)))
                                .collect::<Vec<_>>()
                                .join(", ");
                            return Err(CliError::usage(format!(
                                "«{arg}» indica {} viste: {list}",
                                hits.len()
                            )));
                        }
                    }
                }
            };
            let Some(view) = view else {
                return Err(CliError::not_found(format!(
                    "nessuna vista legata a «{arg}»"
                )));
            };
            let (files, conflicts) = cell.vault_unbind(&view.view_id)?;
            if cli.json {
                print_json(&serde_json::json!({
                    "unbound": view.view_id,
                    "root": view.root,
                    "files": files,
                    "conflicts": conflicts,
                }));
            } else {
                println!(
                    "sganciata: {} ({}) — {} file e {} copie di conflitto \
                     dimenticati dalla cella; i file restano dove sono",
                    view.root,
                    short_of(&view.view_id),
                    files,
                    conflicts
                );
            }
            Ok(())
        }
    }
}

fn print_sync_report(report: &engramma_vault::SyncReport) {
    let line = |verb: &str, items: &[String]| {
        if !items.is_empty() {
            println!("{verb}: {}", items.len());
            for i in items {
                println!("  - {i}");
            }
        }
    };
    line("nuove note da file", &report.created);
    line("note aggiornate da file", &report.updated_from_file);
    line("file riscritti dalla cella", &report.updated_files);
    line(
        "conflitti (vince la cella, copia .conflitto)",
        &report.conflicts,
    );
    line("note cancellate (file spariti)", &report.deleted);
    line("file spostati o rinominati", &report.moved);
    line("errori", &report.errors);
    line("avvisi", &report.warnings);
    if report.is_noop() {
        println!("nulla da fare");
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
                    // v0.4.3 (P0.1): a compaction snapshot is not a cell. A
                    // cell can no longer have that name (refused at
                    // creation), so a file in that shape is ALWAYS a
                    // snapshot — it does not list
                    if p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| engramma_core::snapshot_of(n).is_some())
                    {
                        continue;
                    }
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
            // v0.4.3 (P0.1): the snapshot-shaped name is reserved — a cell
            // called `x-snap-7` would be indistinguishable from a snapshot
            // of `x` (its file would ride with x's drop, `cell list` could
            // never show it). Opening an existing cell by name stays
            // possible: the CLI is the way out for legacy cells
            if engramma_core::snapshot_of(&format!("{name}.sqlite")).is_some() {
                return Err(CliError::usage(format!(
                    "cell name `{name}` is reserved (the `-snap-<digits>` shape \
                     belongs to compact's snapshots)"
                )));
            }
            // v0.4.4 (P2): on Windows `con.sqlite` still resolves to the
            // device; gated so macOS and Linux keep accepting the names.
            #[cfg(windows)]
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "con"
                    | "prn"
                    | "aux"
                    | "nul"
                    | "com1"
                    | "com2"
                    | "com3"
                    | "com4"
                    | "com5"
                    | "com6"
                    | "com7"
                    | "com8"
                    | "com9"
                    | "lpt1"
                    | "lpt2"
                    | "lpt3"
                    | "lpt4"
                    | "lpt5"
                    | "lpt6"
                    | "lpt7"
                    | "lpt8"
                    | "lpt9"
            ) {
                return Err(CliError::usage(format!(
                    "cell name `{name}` is reserved on Windows"
                )));
            }
            let path = cell_path(cli, name)?;
            Cell::create(&path, name, "cli")?;
            report_ok(cli, "creata", name);
            Ok(())
        }
        CellAction::Drop { name, reason } => {
            if reason.trim().is_empty() {
                return Err(CliError::usage("--reason è obbligatoria"));
            }
            let path = cell_path(cli, name)?;
            if !path.exists() {
                return Err(CliError::not_found(format!("cell {name} not found")));
            }
            // v0.4.2 (P0.3): the whole-cell oblivion the engine binding's
            // dropCell runs, one place for both — the views lose the note
            // files they hold for this cell NOW, and the honest report says
            // what could not be reached. The old code just unlinked the
            // file: the bound views kept their copies forever.
            let cell =
                Cell::open(&path).map_err(|e| CliError::internal(format!("cell `{name}`: {e}")))?;
            let report = engramma_vault::drop_cell_everywhere(&cell).map_err(|e| match e {
                engramma_vault::VaultError::Cell(c) => c.into(),
                other => CliError::internal(format!("drop: {other}")),
            })?;
            for problem in &report.problems {
                eprintln!("engramma: {}", problem.reason);
            }
            // v0.4.3 (P1.2): the connection closes before the files go —
            // retire swaps in an in-memory placeholder and closes the real
            // one (the engine binding needs it: its Cell clones outlive the
            // drop; here it also lets SQLite drop the -wal/-shm cleanly)
            cell.retire()?;
            drop(cell);
            // the cell, its WAL, its shm and every snapshot file
            for suffix in ["", "-wal", "-shm"] {
                let mut p = path.clone();
                p.set_file_name(format!("{name}.sqlite{suffix}"));
                std::fs::remove_file(&p).ok();
            }
            if let Some(dir) = path.parent() {
                // v0.4.3 (P0.1): only THIS cell's own snapshots — the exact
                // compact format, one predicate in the core. The old prefix
                // match took a neighboring cell called `storia-snap-archivio`
                // with it
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let file_name = entry.file_name();
                        let n = file_name.to_string_lossy();
                        if engramma_core::snapshot_of(&n).is_some_and(|(cell, _)| cell == name) {
                            std::fs::remove_file(entry.path()).ok();
                        }
                    }
                }
            }
            if cli.json {
                print_json(&serde_json::json!({
                    "dropped": name,
                    "reason": reason,
                    "notes": report.notes,
                    "files": report.files,
                    "scrubbed": report.pending.is_empty(),
                    "pending": report.pending,
                }));
            } else {
                println!(
                    "dimenticata: {name} — {reason} ({} note, {} file di viste)",
                    report.notes, report.files
                );
                if !report.pending.is_empty() {
                    println!(
                        "ATTENZIONE: l'oblio non è completo ({}): una vista \
                         irraggiungibile tiene ancora i file delle note",
                        report.pending.join(", ")
                    );
                }
            }
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

// ---------------------------------------------------------------------------
// v0.5.2 (P3): `engramma oauth` — the human surface of the authorization
// ---------------------------------------------------------------------------

/// The host of a client_id or redirect URL, for the approval's own display.
fn oauth_host(url: &str) -> &str {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', ':', '?'])
        .next()
        .unwrap_or(url)
}

fn run_oauth(cli: &Cli, action: &OauthAction) -> Result<(), CliError> {
    use engramma_mcp::oauth::store::{OAuthStore, RequestStatus, SCOPE_PROPOSE, SCOPE_READ};
    let store = OAuthStore::open(&root(cli).join("oauth.db"))
        .map_err(|e| CliError::internal(format!("{e}")))?;
    match action {
        OauthAction::Pending => {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let pending = store
                .requests_pending(now)
                .map_err(|e| CliError::internal(format!("{e}")))?;
            if cli.json {
                let arr: Vec<serde_json::Value> = pending
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "code": r.approval_code,
                            "client": r.client_name,
                            "client_id_host": oauth_host(&r.client_id),
                            "redirect_host": oauth_host(&r.redirect_uri),
                            "scopes": r.scopes,
                            "age_secs": now - r.created_at,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "pending": arr }));
            } else {
                if pending.is_empty() {
                    println!("nessuna richiesta in attesa");
                }
                for r in &pending {
                    println!(
                        "{}  {} (client {}) → {} — {} — {} s",
                        r.approval_code,
                        r.client_name.as_deref().unwrap_or("client senza nome"),
                        oauth_host(&r.client_id),
                        oauth_host(&r.redirect_uri),
                        r.scopes.join(" "),
                        now - r.created_at
                    );
                }
            }
            Ok(())
        }
        OauthAction::Approve {
            code,
            label,
            cells,
            scope,
            yes,
        } => {
            if !label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                || label.is_empty()
                || label.len() > 32
            {
                return Err(CliError::usage(format!(
                    "`--label {label}` non va: servono 1-32 caratteri fra a-z, 0-9 e -"
                )));
            }
            let code = &engramma_mcp::oauth::normalize_approval_code(code);
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let Some(req) = store
                .request_by_code(code)
                .map_err(|e| CliError::internal(format!("{e}")))?
            else {
                return Err(CliError::not_found(format!(
                    "nessuna richiesta con codice {code}"
                )));
            };
            if req.status != RequestStatus::Pending || req.expires_at <= now {
                return Err(CliError::usage(format!(
                    "la richiesta {code} non è più decidibile (scaduta o già decisa)"
                )));
            }
            // the phishing fence: SHOW whose client and whose redirect,
            // then let the human say yes
            println!(
                "client: {} ({})",
                req.client_name.as_deref().unwrap_or("senza nome"),
                oauth_host(&req.client_id)
            );
            println!("redirect: {}", oauth_host(&req.redirect_uri));
            println!("ambiti chiesti: {}", req.scopes.join(" "));
            if !*yes {
                use std::io::Write as _;
                print!("approvo {code} con etichetta `{label}`? [s/N] ");
                std::io::stdout().flush().ok();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| CliError::internal(format!("stdin: {e}")))?;
                if !answer.trim().eq_ignore_ascii_case("s") {
                    println!("annullato");
                    return Ok(());
                }
            }
            // one live connection per label: the label is the actor
            // `mcp:<label>` of its proposals, two would sign alike
            let label_taken = store
                .grants()
                .map_err(|e| CliError::internal(format!("{e}")))?
                .iter()
                .any(|g| &g.label == label && g.revoked_at.is_none() && g.absolute_expiry > now);
            if label_taken {
                return Err(CliError::usage(format!(
                    "l'etichetta `{label}` è già di un collegamento vivo: revocalo \
                     (`engramma oauth revoke {label}`) o scegline un'altra"
                )));
            }
            // the grant can only RESTRICT what the server offered: a name
            // outside the offer is a typo, said before anything is written
            if let Some(stray) = cells.iter().find(|c| !req.offered_cells.contains(c)) {
                return Err(CliError::usage(format!(
                    "`--cells {stray}`: il server offre solo {}",
                    req.offered_cells.join(", ")
                )));
            }
            let cells: Vec<String> = req
                .offered_cells
                .iter()
                .filter(|c| cells.is_empty() || cells.contains(c))
                .cloned()
                .collect();
            let mut scopes: Vec<String> = req
                .scopes
                .iter()
                .filter(|sc| scope.is_empty() || scope.contains(sc))
                .cloned()
                .collect();
            // propose survives only where the server can write AND the
            // grant kept a writable cell
            if !req.offered_write.iter().any(|w| cells.contains(w)) {
                scopes.retain(|sc| sc != SCOPE_PROPOSE);
            }
            if !scopes.iter().any(|sc| sc == SCOPE_READ) {
                scopes.push(SCOPE_READ.to_string());
            }
            store
                .request_approve(code, label, &cells, &scopes, now)
                .map_err(|e| CliError::internal(format!("{e}")))?
                .ok_or_else(|| CliError::usage("l'approvazione non è andata: riprova"))?;
            if cli.json {
                print_json(&serde_json::json!({
                    "approved": code,
                    "label": label,
                    "cells": cells,
                    "scopes": scopes,
                }));
            } else {
                println!(
                    "approvato: {label} (celle: {}, ambiti: {})",
                    cells.join(", "),
                    scopes.join(" ")
                );
            }
            Ok(())
        }
        OauthAction::Deny { code, all } => {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            match (code, *all) {
                (Some(typed), false) => {
                    let code = &engramma_mcp::oauth::normalize_approval_code(typed);
                    let denied = store
                        .request_deny(code, now)
                        .map_err(|e| CliError::internal(format!("{e}")))?;
                    if denied {
                        report_ok(cli, "rifiutata", code);
                    } else {
                        return Err(CliError::not_found(format!(
                            "nessuna richiesta in attesa con codice {code}"
                        )));
                    }
                }
                (None, true) => {
                    let n = store
                        .request_deny_all(now)
                        .map_err(|e| CliError::internal(format!("{e}")))?;
                    if cli.json {
                        print_json(&serde_json::json!({ "denied_all": n }));
                    } else if n == 0 {
                        println!("nessuna richiesta in attesa");
                    } else {
                        println!(
                            "rifiutate {n} richieste: le pagine in attesa redirigono i client con access_denied"
                        );
                    }
                }
                _ => {
                    return Err(CliError::usage(
                        "serve un codice, o --all per rifiutarle tutte",
                    ));
                }
            }
            Ok(())
        }
        OauthAction::Grants => {
            let grants = store
                .grants()
                .map_err(|e| CliError::internal(format!("{e}")))?;
            if cli.json {
                let arr: Vec<serde_json::Value> = grants
                    .iter()
                    .map(|g| {
                        serde_json::json!({
                            "id": g.id,
                            "label": g.label,
                            "client": oauth_host(&g.client_id),
                            "cells": g.cells,
                            "scopes": g.scopes,
                            "created_at": g.created_at,
                            "last_used_at": g.last_used_at,
                            "revoked_at": g.revoked_at,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "grants": arr }));
            } else {
                if grants.is_empty() {
                    println!("nessun grant");
                }
                for g in &grants {
                    println!(
                        "{}  {}  celle: {}  ambiti: {}  creato {}  {}",
                        g.label,
                        oauth_host(&g.client_id),
                        g.cells.join(","),
                        g.scopes.join(" "),
                        g.created_at,
                        if g.revoked_at.is_some() {
                            "REVOCATO"
                        } else {
                            "vivo"
                        }
                    );
                }
            }
            Ok(())
        }
        OauthAction::Revoke { grant, all } => {
            let (id, label) = match (grant, *all) {
                (Some(label), false) => {
                    let grants = store
                        .grants()
                        .map_err(|e| CliError::internal(format!("{e}")))?;
                    // the LIVE one first: a label comes back after a revoke
                    let pick = grants
                        .iter()
                        .find(|g| &g.label == label && g.revoked_at.is_none())
                        .or_else(|| grants.iter().find(|g| &g.label == label));
                    match pick {
                        Some(g) => (Some(g.id.clone()), label.clone()),
                        None => {
                            return Err(CliError::not_found(format!(
                                "nessun grant con etichetta `{label}`"
                            )));
                        }
                    }
                }
                (None, true) => (None, "*".to_string()),
                _ => {
                    return Err(CliError::usage(
                        "serve un'etichetta, o --all per revocare tutto",
                    ));
                }
            };
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let n = store
                .grant_revoke(id.as_deref(), now)
                .map_err(|e| CliError::internal(format!("{e}")))?;
            if cli.json {
                print_json(&serde_json::json!({ "revoked": label, "grants": n }));
            } else {
                println!(
                    "revocati {n} grant ({label}): i loro token muoiono alla prossima richiesta"
                );
            }
            Ok(())
        }
        OauthAction::Log => {
            let events = store
                .events()
                .map_err(|e| CliError::internal(format!("{e}")))?;
            if cli.json {
                let arr: Vec<serde_json::Value> = events
                    .iter()
                    .map(|(at, kind, detail)| {
                        serde_json::json!({ "at": at, "kind": kind, "detail": detail })
                    })
                    .collect();
                print_json(&serde_json::json!({ "events": arr }));
            } else {
                if events.is_empty() {
                    println!("nessun evento");
                }
                for (at, kind, detail) in &events {
                    println!("{at}  {kind}  {detail}");
                }
            }
            Ok(())
        }
    }
}

/// `engramma mcp --http` (v0.5.2, P1): the HTTP surface — OAuth and `/mcp`
/// on one loopback listener; TLS belongs to the tunnel in front of it.
fn run_mcp_http(
    cli: &Cli,
    cells: Vec<String>,
    write: Vec<String>,
    addr: &str,
    public_url: Option<String>,
    allow_dcr: bool,
    compat_chatgpt: bool,
) -> Result<(), CliError> {
    use engramma_mcp::http::{HttpConfig, HttpState};
    let Some(public) = public_url else {
        return Err(CliError::usage(
            "`--http` needs `--public-url https://…`: it is the address the \
             tunnel publishes, and every OAuth URL is derived from it",
        ));
    };
    let bind: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| CliError::usage(format!("`--http {addr}` is not an address: {e}")))?;
    let cfg = HttpConfig {
        bind,
        public_url: public.clone(),
        cells,
        write,
        allow_dcr,
        compat_chatgpt,
        chatgpt_client_id: engramma_mcp::CHATGPT_CLIENT_ID.to_string(),
        allow_insecure_public_url: false,
        allow_loopback_cimd: false,
    };
    if let Err(why) = cfg.validate() {
        return Err(CliError::usage(why));
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(format!("tokio: {e}")))?;
    rt.block_on(async move {
        let root = root(cli);
        let state = HttpState::open_with_mcp(&root, cfg)
            .map_err(|e| CliError::internal(format!("mcp --http: {e}")))?;
        let bound = cfg_bind_of(&state);
        println!(
            "mcp-http: in ascolto su http://{bound} — pubblico: {public} \
             (approva i collegamenti con `engramma oauth pending`)"
        );
        let ct = tokio_util_shim::token();
        let server = tokio::spawn(engramma_mcp::http::serve_until(
            std::sync::Arc::new(state),
            ct.clone(),
        ));
        // SIGINT or SIGTERM: cancel rmcp, let the listener drain, the
        // cells close when the state drops
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = signal(SignalKind::terminate())
                .map_err(|e| CliError::internal(format!("signal: {e}")))?;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        eprintln!("mcp-http: chiudo");
        ct.cancel();
        server
            .await
            .map_err(|e| CliError::internal(format!("join: {e}")))?
            .map_err(|e| CliError::internal(format!("serve: {e}")))?;
        Ok(())
    })
}

fn cfg_bind_of(state: &engramma_mcp::http::HttpState) -> std::net::SocketAddr {
    state.cfg.bind
}

/// A tiny indirection so the CLI does not name tokio-util directly.
mod tokio_util_shim {
    pub fn token() -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }
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

/// Parse `--fusion`: `rrf`, or the convex combination as
/// `convex[-mm|-th]-α` — `mm` min-maxes the cosine over the candidates
/// (the default), `th` uses the theoretical −1..1 limits.
/// v0.4 (P1.6): `ENGRAMMA_GRAPH` overrides the graph's knobs of a search
/// without touching the defaults — `adaptive:<w_max>,<u_min>,<spw>`
/// (question-entity seeding, gate, per-question weight) or `fixed:<w>`
/// (the v0.3.2 behavior with an explicit weight). The manual proofs of
/// the REPORT run on it.
fn parse_graph_env(spec: &str) -> Result<engramma_core::search::GraphOverride, CliError> {
    use engramma_core::search::{GraphOverride, GraphPolicy, GraphSeeding};
    if let Some(w) = spec.strip_prefix("fixed:") {
        let w: f64 = w
            .replace(',', ".")
            .parse()
            .map_err(|_| CliError::usage(format!("ENGRAMMA_GRAPH: fixed:<peso> — {spec:?}")))?;
        return Ok(GraphOverride {
            policy: GraphPolicy::Fixed,
            weight: w,
            seeding: GraphSeeding::Fused,
        });
    }
    let Some(rest) = spec.strip_prefix("adaptive:") else {
        return Err(CliError::usage(
            "ENGRAMMA_GRAPH: adaptive:<w_max>,<u_min>,<spw> | fixed:<peso>",
        ));
    };
    let nums: Vec<&str> = rest.split(',').map(str::trim).collect();
    if nums.len() != 3 {
        return Err(CliError::usage(
            "ENGRAMMA_GRAPH: adaptive:<w_max>,<u_min>,<spw> (tre numeri col punto decimale)",
        ));
    }
    let parse = |v: &str| -> Result<f64, CliError> {
        v.parse::<f64>()
            .map_err(|_| CliError::usage(format!("ENGRAMMA_GRAPH: numero non valido {v:?}")))
    };
    let in_unit = |v: f64, name: &str| -> Result<f64, CliError> {
        (0.0..=1.0)
            .contains(&v)
            .then_some(v)
            .ok_or_else(|| CliError::usage(format!("ENGRAMMA_GRAPH: {name}={v} fuori da [0, 1]")))
    };
    let w_max = in_unit(parse(nums[0])?, "w_max")?;
    let u_min = in_unit(parse(nums[1])?, "u_min")?;
    let spw = in_unit(parse(nums[2])?, "spw")?;
    Ok(GraphOverride {
        policy: GraphPolicy::Adaptive { w_max, u_min },
        weight: w_max,
        seeding: GraphSeeding::Question {
            seed_passage_weight: spw,
        },
    })
}

fn parse_fusion(s: &str) -> Result<engramma_core::search::Fusion, CliError> {
    use engramma_core::search::{CosNorm, Fusion};
    if s == "rrf" {
        return Ok(Fusion::Rrf { k: 60 });
    }
    let (alpha_str, norm) = if let Some(a) = s.strip_prefix("convex-th-") {
        (a, CosNorm::Theoretical)
    } else if let Some(a) = s
        .strip_prefix("convex-mm-")
        .or_else(|| s.strip_prefix("convex-"))
    {
        (a, CosNorm::Candidates)
    } else {
        return Err(CliError::usage(fusion_usage(s)));
    };
    let alpha = match alpha_str.replace(',', ".").as_str() {
        "0.3" => 0.3,
        "0.5" => 0.5,
        "0.7" => 0.7,
        _ => return Err(CliError::usage(fusion_usage(s))),
    };
    Ok(Fusion::Convex {
        alpha,
        cos_norm: norm,
    })
}

fn fusion_usage(s: &str) -> String {
    format!("unknown --fusion `{s}` (rrf, convex-0.3, convex-mm-0.5, convex-th-0.7, …)")
}

// ---------------------------------------------------------------------------
// v0.5 (P4): the queue, the decisions, the revert, the policy
// ---------------------------------------------------------------------------

/// `--since/--until` as a day, inclusive: since = start of day,
/// until = end of day (the engine compares fixed-width Z timestamps).
fn parse_day(s: &str, end: bool) -> Result<time::OffsetDateTime, CliError> {
    let v = Validity::parse(s)
        .map_err(|e| CliError::usage(format!("--{}: {e}", if end { "until" } else { "since" })))?;
    let day = v.start().date();
    let t = if end {
        day.with_hms(23, 59, 59)
            .map_err(|e| CliError::usage(e.to_string()))?
    } else {
        day.with_hms(0, 0, 0)
            .map_err(|e| CliError::usage(e.to_string()))?
    };
    Ok(t.assume_utc())
}

fn proposal_json(row: &engramma_core::cell::proposals::ProposalRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.id.to_string(),
        "state": row.state.as_str(),
        "mode": match &row.mode {
            engramma_core::cell::proposals::ProposalMode::New => "new",
            engramma_core::cell::proposals::ProposalMode::Supersede(_) => "supersede",
        },
        "target": row.target.map(|t| t.to_string()),
        "proposer": row.proposer,
        "client": row.client,
        "reason": row.reason,
        "proposed_at": row.proposed_at,
        "decided_by": row.decided_by,
        "decided_at": row.decided_at,
        "decision_reason": row.decision_reason,
        "review": row.review,
        "auto_failed": row.auto_failed,
        "note": note_json(&row.note),
    })
}

fn decision_json(row: &engramma_core::cell::proposals::DecisionRow) -> serde_json::Value {
    serde_json::json!({
        "op_id": row.op_id.to_string(),
        "proposal": row.proposal.to_string(),
        "target": row.target.map(|t| t.to_string()),
        "decision": row.decision,
        "actor": row.actor,
        "decided_at": row.decided_at,
        "reason": row.reason,
        "review": row.review,
        "reverted_by": row.reverted_by.map(|r| r.to_string()),
        "state": if row.reverted_by.is_some() { "reverted" } else { "active" },
    })
}

fn run_proposals(
    cli: &Cli,
    state: &Option<String>,
    by: &Option<String>,
    action: &Option<ProposalsAction>,
) -> Result<(), CliError> {
    let cell = open_cell(cli)?;
    match action {
        None => {
            let state = match state {
                None => None,
                Some(s) => Some(parse_proposal_state(s)?),
            };
            let rows = cell.proposals(&engramma_core::cell::proposals::ProposalsFilter {
                state,
                by: by.clone(),
                ..Default::default()
            })?;
            if cli.json {
                let arr: Vec<serde_json::Value> = rows.iter().map(proposal_json).collect();
                print_json(&serde_json::json!({ "cell": cli.cell, "proposals": arr }));
            } else {
                if rows.is_empty() {
                    println!("coda vuota");
                }
                for r in &rows {
                    let marker = match r.state {
                        engramma_core::cell::proposals::ProposalState::Pending
                        | engramma_core::cell::proposals::ProposalState::Reverted => "[in coda]",
                        _ => "",
                    };
                    println!(
                        "{} {} {} — {}",
                        marker,
                        engramma_core::id::id8(&r.id),
                        r.proposer,
                        r.note.display_title()
                    );
                    if let Some(reason) = &r.reason {
                        println!("      motivo: {reason}");
                    }
                    if let engramma_core::cell::proposals::ProposalMode::Supersede(t) = &r.mode {
                        println!("      rettifica di {}", engramma_core::id::id8(t));
                    }
                    if r.auto_failed {
                        println!(
                            "      revisione automatica fallita: riprova con `proposals review`"
                        );
                    }
                    if let (Some(who), Some(at)) = (&r.decided_by, &r.decided_at) {
                        println!("      decisa da {who} il {at}");
                    }
                }
            }
            Ok(())
        }
        Some(ProposalsAction::Show { id }) => {
            let id = cell.resolve_note_id(id)?;
            let row = cell
                .proposals(&engramma_core::cell::proposals::ProposalsFilter::default())?
                .into_iter()
                .find(|r| r.id == id)
                .ok_or_else(|| CliError::not_found(format!("proposta {id}")))?;
            if cli.json {
                print_json(&proposal_json(&row));
            } else {
                println!(
                    "{}",
                    row.note
                        .serialize()
                        .map_err(|e| CliError::internal(e.to_string()))?
                );
                println!();
                println!("proposta da {} il {}", row.proposer, row.proposed_at);
                if let Some(c) = &row.client {
                    println!("client dichiarato: {c}");
                }
                if let Some(r) = &row.reason {
                    println!("motivo: {r}");
                }
                if let (Some(who), Some(at)) = (&row.decided_by, &row.decided_at) {
                    println!(
                        "decisa da {who} il {at}: {}",
                        row.decision_reason.clone().unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        Some(ProposalsAction::Accept { id, edit }) => {
            let id = cell.resolve_note_id(id)?;
            let edited = if *edit {
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
                let text = std::fs::read_to_string(&tmp)?;
                std::fs::remove_file(&tmp).ok();
                let parsed = engramma_core::format::parse(&text, engramma_core::ParseMode::Strict)
                    .map_err(CliError::from)?;
                let mut n = parsed.note;
                if n.id != note.id {
                    return Err(CliError::usage(
                        "the note id cannot change in accept --edit",
                    ));
                }
                n.created_at = note.created_at;
                Some(n)
            } else {
                None
            };
            cell.decide(
                &id,
                &engramma_core::cell::proposals::DecisionInput {
                    action: engramma_core::cell::proposals::DecisionAction::Accept { edited },
                    reason: Some("accettata dalla CLI".into()),
                    review: None,
                },
                "user@cli",
            )?;
            report_ok(cli, "accettata", &id.to_string());
            Ok(())
        }
        Some(ProposalsAction::Reject { id, reason }) => {
            let id = cell.resolve_note_id(id)?;
            cell.decide(
                &id,
                &engramma_core::cell::proposals::DecisionInput {
                    action: engramma_core::cell::proposals::DecisionAction::Reject,
                    reason: reason.clone(),
                    review: None,
                },
                "user@cli",
            )?;
            report_ok(cli, "rifiutata (lapide)", &id.to_string());
            Ok(())
        }
        Some(ProposalsAction::Review) => {
            let reviewers = engramma_core::review::Reviewers::load(&root(cli))
                .map_err(|e| CliError::usage(e.to_string()))?;
            let embedder = load_embedder_or_warn(cli)?;
            let ctx = engramma_core::cell::proposals::ProposeContext {
                embedder: embedder.as_deref(),
                reviewers: Some(&reviewers),
            };
            let decided = cell
                .review_failed(&ctx)
                .map_err(|e| CliError::internal(e.to_string()))?;
            if cli.json {
                print_json(&serde_json::json!({ "decided": decided }));
            } else {
                match decided {
                    0 => println!("nulla da ritentare (o niente da decidere per il revisore)"),
                    n => println!("ritentate e decise: {n}"),
                }
            }
            Ok(())
        }
    }
}

fn parse_proposal_state(
    s: &str,
) -> Result<engramma_core::cell::proposals::ProposalState, CliError> {
    Ok(match s {
        "pending" => engramma_core::cell::proposals::ProposalState::Pending,
        "accepted" => engramma_core::cell::proposals::ProposalState::Accepted,
        "rejected" => engramma_core::cell::proposals::ProposalState::Rejected,
        "reverted" => engramma_core::cell::proposals::ProposalState::Reverted,
        other => {
            return Err(CliError::usage(format!(
                "`{other}` is not a proposal state (pending | accepted | rejected | reverted)"
            )));
        }
    })
}

fn run_decisions(
    cli: &Cli,
    by: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
    state: &Option<String>,
) -> Result<(), CliError> {
    let cell = open_cell(cli)?;
    let state = match state {
        None => None,
        Some(s) => Some(match s.as_str() {
            "active" => engramma_core::cell::proposals::DecisionState::Active,
            "reverted" => engramma_core::cell::proposals::DecisionState::Reverted,
            other => {
                return Err(CliError::usage(format!(
                    "`{other}` is not a decision state (active | reverted)"
                )));
            }
        }),
    };
    let rows = cell.decisions(&engramma_core::cell::proposals::DecisionsFilter {
        by: by.clone(),
        since: since.as_deref().map(|s| parse_day(s, false)).transpose()?,
        until: until.as_deref().map(|s| parse_day(s, true)).transpose()?,
        state,
    })?;
    if cli.json {
        let arr: Vec<serde_json::Value> = rows.iter().map(decision_json).collect();
        print_json(&serde_json::json!({ "cell": cli.cell, "decisions": arr }));
    } else {
        if rows.is_empty() {
            println!("nessuna decisione");
        }
        for r in &rows {
            println!(
                "{}  {}  {}  {}",
                engramma_core::id::id8(&r.op_id),
                if r.decision == "accept" {
                    "accetta"
                } else {
                    "rifiuta"
                },
                r.actor,
                r.decided_at
            );
            println!("      proposta {}", engramma_core::id::id8(&r.proposal));
            if let Some(reason) = &r.reason {
                println!("      perché: {reason}");
            }
            if let Some(review) = &r.review {
                println!(
                    "      revisore {} modello {} confidenza {} prompt {}",
                    review["name"].as_str().unwrap_or("?"),
                    review["model"].as_str().unwrap_or("?"),
                    review["confidence"],
                    &review["prompt_hash"].as_str().unwrap_or("?")[..8],
                );
            }
            if r.reverted_by.is_some() {
                println!("      annullata");
            }
        }
    }
    Ok(())
}

fn run_revert(
    cli: &Cli,
    decision: &Option<String>,
    by: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
    dry_run: bool,
    yes: bool,
) -> Result<(), CliError> {
    let cell = open_cell(cli)?;
    if let Some(id) = decision {
        if by.is_some() || since.is_some() || until.is_some() {
            return Err(CliError::usage(
                "a single revert takes a decision id, not --by/--since/--until",
            ));
        }
        let full = resolve_decision_op(&cell, id)?;
        let outcome = cell
            .revert_decision(&full, "annullata dalla CLI", "user@cli")
            .map_err(CliError::from)?;
        if cli.json {
            print_json(&serde_json::json!({
                "outcome": match &outcome {
                    engramma_core::cell::proposals::RevertOutcome::Reverted { decision, proposal } =>
                        serde_json::json!({ "reverted": true, "decision": decision.to_string(), "proposal": proposal.to_string() }),
                    engramma_core::cell::proposals::RevertOutcome::Purged { decision } =>
                        serde_json::json!({ "reverted": false, "purged": true, "decision": decision.to_string() }),
                }
            }));
        } else {
            match outcome {
                engramma_core::cell::proposals::RevertOutcome::Reverted { proposal, .. } => {
                    report_ok(
                        cli,
                        "annullata: proposta di nuovo in coda",
                        &id8_or(&proposal),
                    );
                }
                engramma_core::cell::proposals::RevertOutcome::Purged { .. } => {
                    println!("annullata, ma il contenuto era purgato: non torna");
                }
            }
        }
        return Ok(());
    }
    if by.is_none() && since.is_none() && until.is_none() {
        return Err(CliError::usage(
            "either a decision id, or --by/--since/--until for the bulk revert",
        ));
    }
    let filter = engramma_core::cell::proposals::RevertFilter {
        by: by.clone(),
        since: since.as_deref().map(|s| parse_day(s, false)).transpose()?,
        until: until.as_deref().map(|s| parse_day(s, true)).transpose()?,
    };
    let report = cell
        .revert_decisions(&filter, true)
        .map_err(CliError::from)?;
    // the human preview stays out of --json (the JSON answers carry it all)
    if !cli.json {
        println!("annullerò {} decisioni:", report.reverted);
        println!("  conflitti: {} (non toccati)", report.conflicts.len());
        for c in &report.conflicts {
            println!("    {c}");
        }
        println!(
            "  purgate: {} (annullate nel registro, il contenuto non torna)",
            report.purged
        );
    }
    if dry_run {
        if cli.json {
            print_json(&serde_json::json!({
                "dry_run": true,
                "reverted": report.reverted,
                "conflicts": report.conflicts,
                "purged": report.purged,
            }));
        }
        return Ok(());
    }
    if !yes {
        if !cli.json {
            print!("procedo? [s/N] ");
        }
        use std::io::Write as _;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| CliError::internal(format!("stdin: {e}")))?;
        if !answer.trim().eq_ignore_ascii_case("s") {
            if cli.json {
                print_json(&serde_json::json!({ "aborted": true }));
            } else {
                println!("annullato");
            }
            return Ok(());
        }
    }
    let report = cell
        .revert_decisions(&filter, false)
        .map_err(CliError::from)?;
    if cli.json {
        print_json(&serde_json::json!({
            "reverted": report.reverted,
            "conflicts": report.conflicts,
            "purged": report.purged,
        }));
    } else {
        println!(
            "annullate {}, conflitti {}, purgate {}",
            report.reverted,
            report.conflicts.len(),
            report.purged
        );
    }
    Ok(())
}

fn id8_or(u: &Ulid) -> String {
    engramma_core::id::id8(u)
}

/// A decision op id, or its unique last-8 prefix.
fn resolve_decision_op(cell: &Cell, input: &str) -> Result<Ulid, CliError> {
    let rows = cell
        .decisions(&engramma_core::cell::proposals::DecisionsFilter::default())
        .map_err(CliError::from)?;
    if input.len() == 26 {
        return Ulid::from_string(input)
            .map_err(|_| CliError::usage(format!("`{input}` is not a ULID")));
    }
    let matches: Vec<&engramma_core::cell::proposals::DecisionRow> = rows
        .iter()
        .filter(|r| r.op_id.to_string().ends_with(input))
        .collect();
    match matches.first() {
        Some(only) if matches.len() == 1 => Ok(only.op_id),
        None => Err(CliError::not_found(format!(
            "no decision ends with `{input}`"
        ))),
        Some(_) => Err(CliError::usage(format!("`{input}` is ambiguous"))),
    }
}

fn run_policy(cli: &Cli, action: &PolicyAction) -> Result<(), CliError> {
    let cell = open_cell(cli)?;
    match action {
        PolicyAction::Show => {
            let policy = cell.policy_of().map_err(CliError::from)?;
            if cli.json {
                print_json(&serde_json::json!({
                    "cell": cli.cell,
                    "set": cell.policy().map_err(CliError::from)?.is_some(),
                    "policy": {
                        "write": policy.write.as_str(),
                        "reviewer": policy.reviewer,
                        "min_confidence": policy.min_confidence,
                        "kinds": policy.kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
                        "allow_supersede": policy.allow_supersede.as_str(),
                        "max_auto_per_hour": policy.max_auto_per_hour,
                    }
                }));
            } else {
                println!("write: {}", policy.write.as_str());
                if let Some(r) = &policy.reviewer {
                    println!("reviewer: {r}");
                }
                println!("min_confidence: {}", policy.min_confidence);
                println!(
                    "kinds: {}",
                    if policy.kinds.is_empty() {
                        "tutti".into()
                    } else {
                        policy
                            .kinds
                            .iter()
                            .map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    }
                );
                println!("allow_supersede: {}", policy.allow_supersede.as_str());
                println!("max_auto_per_hour: {}", policy.max_auto_per_hour);
            }
            Ok(())
        }
        PolicyAction::Set { pairs } => {
            if pairs.is_empty() {
                return Err(CliError::usage(
                    "policy set wants key=value pairs (write=propose reviewer=bravo …)",
                ));
            }
            // the current document (or the defaults), then the pairs on top
            let mut doc = cell
                .policy()
                .map_err(CliError::from)?
                .unwrap_or_else(|| serde_json::json!({}));
            for pair in pairs {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| CliError::usage(format!("`{pair}` is not key=value")))?;
                doc[k] = parse_policy_value(k, v)?;
            }
            // refuse to store what the engine could not read back
            engramma_core::cell::policy::Policy::parse(&doc).map_err(CliError::from)?;
            cell.set_policy(doc.clone(), "cli")?;
            if cli.json {
                print_json(&doc);
            } else {
                println!("politica aggiornata");
            }
            Ok(())
        }
    }
}

fn parse_policy_value(key: &str, raw: &str) -> Result<serde_json::Value, CliError> {
    match key {
        "write" | "allow_supersede" => Ok(serde_json::Value::String(raw.to_string())),
        "reviewer" => Ok(serde_json::Value::String(raw.to_string())),
        "min_confidence" => raw
            .parse::<f64>()
            .map(serde_json::Value::from)
            .map_err(|_| CliError::usage("min_confidence wants a number in [0, 1]")),
        "max_auto_per_hour" => raw
            .parse::<u32>()
            .map(serde_json::Value::from)
            .map_err(|_| CliError::usage("max_auto_per_hour wants a whole number")),
        "kinds" => Ok(serde_json::Value::Array(
            raw.split(',')
                .map(str::to_string)
                .map(serde_json::Value::String)
                .collect(),
        )),
        other => Err(CliError::usage(format!(
            "`{other}` is not a policy field (write, reviewer, min_confidence, kinds, \
             allow_supersede, max_auto_per_hour)"
        ))),
    }
}

/// v0.5 (P4): the note's history — the proposal's story (who proposed,
/// who decided, why) and the versions, with the line diff between them.
fn run_history(cli: &Cli, cell: &Cell, id: &Ulid, diff: bool) -> Result<(), CliError> {
    let hist = cell.history(id)?;
    let all_ops = cell.ops().map_err(CliError::from)?;
    let ops: std::collections::HashMap<String, &engramma_core::cell::ops::Op> =
        all_ops.iter().map(|o| (o.op_id.to_string(), o)).collect();
    // the queue row, when this note ever was a proposal
    let proposal = cell
        .proposals(&engramma_core::cell::proposals::ProposalsFilter::default())
        .map_err(CliError::from)?
        .into_iter()
        .find(|r| r.id == *id);

    if cli.json {
        let arr: Vec<serde_json::Value> = hist
            .iter()
            .map(|v| {
                let mut entry = serde_json::json!({
                    "version": v.version,
                    "op_id": v.op_id.to_string(),
                    "note": note_json(&v.note),
                });
                if let Some(op) = ops.get(&v.op_id.to_string()) {
                    entry["op"] = serde_json::json!({
                        "kind": op.kind.as_str(),
                        "actor": op.actor,
                    });
                }
                entry
            })
            .collect();
        let mut out = serde_json::json!({ "id": id.to_string(), "versions": arr });
        if let Some(r) = &proposal {
            out["proposal"] = serde_json::json!({
                "proposer": r.proposer,
                "client": r.client,
                "reason": r.reason,
                "state": r.state.as_str(),
                "proposed_at": r.proposed_at,
                "decided_by": r.decided_by,
                "decided_at": r.decided_at,
            });
        }
        print_json(&out);
        return Ok(());
    }

    if let Some(r) = &proposal {
        println!(
            "proposta da {} il {} — {}",
            r.proposer,
            r.proposed_at,
            r.reason
                .clone()
                .unwrap_or_else(|| "senza motivo dichiarato".into())
        );
        if let (Some(who), Some(at)) = (&r.decided_by, &r.decided_at) {
            println!("decisa da {who} il {at}");
        }
    }
    if diff {
        // each snapshot is the state BEFORE its op: the diff of consecutive
        // snapshots shows what the later op changed, and the last window is
        // the newest snapshot against the CURRENT note
        let current = cell.get(id)?;
        let mut prev: Option<&engramma_core::cell::VersionEntry> = None;
        for (i, v) in hist.iter().enumerate() {
            if let Some(p) = prev {
                let op_line = ops
                    .get(&v.op_id.to_string())
                    .map_or_else(String::new, |op| {
                        format!("  ({}, {})", op.kind.as_str(), op.actor)
                    });
                println!("v{} → v{}{op_line}", p.version, v.version);
                for line in line_diff(&p.note.body, &v.note.body) {
                    println!("  {line}");
                }
            }
            prev = Some(v);
            if i + 1 == hist.len() && v.note.body != current.body {
                println!("v{} → ora  (stato attuale)", v.version);
                for line in line_diff(&v.note.body, &current.body) {
                    println!("  {line}");
                }
            }
        }
    } else {
        for v in &hist {
            let op_line = ops
                .get(&v.op_id.to_string())
                .map_or_else(String::new, |op| {
                    format!("  [{}, {}]", op.kind.as_str(), op.actor)
                });
            println!(
                "v{}  {}  {}{op_line}",
                v.version,
                v.note.updated_at,
                v.note.display_title()
            );
        }
    }
    Ok(())
}

/// A tiny line diff (LCS over the lines): `-` removed, `+` added.
fn line_diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // dp[i][j] = LCS length of a[i..], b[j..]
    let mut dp = vec![vec![0_usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0_usize, 0_usize);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    while i < a.len() {
        out.push(format!("- {}", a[i]));
        i += 1;
    }
    while j < b.len() {
        out.push(format!("+ {}", b[j]));
        j += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// v0.6: devices, sync, mailbox
// ---------------------------------------------------------------------------

fn devices_run(cli: &Cli, action: &DevicesAction) -> Result<(), CliError> {
    use engramma_sync::device::DeviceKey;
    use engramma_sync::registry::DeviceRegistry;
    let r = root(cli);
    match action {
        DevicesAction::Init => {
            let dk =
                DeviceKey::load_or_create(&r).map_err(|e| CliError::internal(e.to_string()))?;
            let reg = DeviceRegistry::open(&r).map_err(|e| CliError::internal(e.to_string()))?;
            let n = reg
                .live_members()
                .map_err(|e| CliError::internal(e.to_string()))?
                .len();
            if n == 0 {
                reg.create_first(&dk, "portatile")
                    .map_err(|e| CliError::internal(e.to_string()))?;
            }
            if cli.json {
                print_json(&serde_json::json!({ "device": short_id(&dk.device_id()) }));
            } else {
                println!(
                    "dispositivo {} (elenco: {} vivi)",
                    short_id(&dk.device_id()),
                    n.max(1)
                );
            }
            Ok(())
        }
        DevicesAction::Invite { relay } => synccli::devices_invite(&r, cli.json, relay_flag(relay)),
        DevicesAction::Code { relay } => {
            crate::mailboxcli::devices_code(&r, cli.json, relay_flag(relay))
        }
        DevicesAction::Join {
            ticket_or_code,
            name,
            relay,
        } => synccli::devices_join(&r, cli.json, ticket_or_code, name, relay_flag(relay)),
        DevicesAction::List => {
            let reg = DeviceRegistry::open(&r).map_err(|e| CliError::internal(e.to_string()))?;
            let members = reg
                .live_members()
                .map_err(|e| CliError::internal(e.to_string()))?;
            if cli.json {
                let v: Vec<serde_json::Value> = members
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "name": m.name, "device": short_id(&m.device),
                            "transport": short_id(&m.transport),
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "devices": v }));
            } else {
                for m in &members {
                    println!(
                        "{}  {}  {}",
                        m.name,
                        short_id(&m.device),
                        short_id(&m.transport)
                    );
                }
            }
            Ok(())
        }
        DevicesAction::Remove { name } => synccli::devices_remove_cmd(&r, cli.json, name),
        DevicesAction::Log => {
            let reg = DeviceRegistry::open(&r).map_err(|e| CliError::internal(e.to_string()))?;
            let rows = reg.rows_json();
            if cli.json {
                print_json(&serde_json::json!({ "ops": rows }));
            } else {
                for row in rows {
                    println!(
                        "{}  {}  {}",
                        row["hlc"].as_str().unwrap_or(""),
                        row["kind"].as_str().unwrap_or(""),
                        row["payload"]["name"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(())
        }
    }
}

fn short_id(hex: &str) -> String {
    hex.chars().take(8).collect()
}

fn sync_run(cli: &Cli, action: &SyncAction) -> Result<(), CliError> {
    let r = root(cli);
    match action {
        SyncAction::Share { cell } => {
            let path = cell_path(cli, cell)?;
            synccli::sync_share_cmd(&r, cli.json, cell, &path)
        }
        SyncAction::Unshare { cell } => synccli::sync_unshare_cmd(&r, cli.json, cell),
        SyncAction::Run { relay } => synccli::sync_run_cmd(&r, cli.json, relay_flag(relay)),
        SyncAction::Serve { relay } => synccli::sync_serve_cmd(&r, cli.json, relay_flag(relay)),
        SyncAction::Status => {
            let cells = engramma_sync::sharedcell::SharedCell::all_of(&r)
                .map_err(|e| CliError::internal(e.to_string()))?;
            if cells.is_empty() {
                if cli.json {
                    print_json(&serde_json::json!({ "cells": [] }));
                } else {
                    println!("nessuna cella condivisa da qui");
                }
                return Ok(());
            }
            let reg = engramma_sync::registry::DeviceRegistry::open(&r)
                .map_err(|e| CliError::internal(e.to_string()))?;
            let devices = reg
                .live_members()
                .map_err(|e| CliError::internal(e.to_string()))?
                .len();
            // R4: the operations that still cannot leave (each an event,
            // no note text)
            let unsent = engramma_sync::sharedcell::unsent_events(&r)
                .map_err(|e| CliError::internal(e.to_string()))?;
            if cli.json {
                let rows: Vec<serde_json::Value> = cells
                    .iter()
                    .map(|c| {
                        let head = engramma_sync::protocol::SyncedCell::head_seq(c);
                        let frontier = c.cell().stable_frontier().unwrap_or(0);
                        let epoch = c.current_epoch().unwrap_or(0);
                        let name =
                            engramma_sync::serve::cell_name(&r, c.sync_id()).unwrap_or_default();
                        serde_json::json!({
                            "cell": name,
                            "head": head, "frontier": frontier,
                            "pending": (head - frontier).max(0),
                            "epoch": epoch,
                            "unsent": unsent.iter()
                                .filter(|(cell, ..)| *cell == name).count(),
                        })
                    })
                    .collect();
                let mut v = serde_json::json!({ "devices": devices, "cells": rows });
                if !unsent.is_empty() {
                    v["unsent_events"] = serde_json::json!(
                        unsent
                            .iter()
                            .map(|(cell, at, op_id, kind, reason)| serde_json::json!({
                                "cell": cell, "at": at, "op": op_id,
                                "kind": kind, "why": reason,
                            }))
                            .collect::<Vec<_>>()
                    );
                }
                print_json(&v);
            } else {
                println!("dispositivi {devices}");
                for c in &cells {
                    let head = engramma_sync::protocol::SyncedCell::head_seq(c);
                    let frontier = c.cell().stable_frontier().unwrap_or(0);
                    let epoch = c.current_epoch().unwrap_or(0);
                    println!(
                        "cella «{}»: testa {head}, frontiera stabile {frontier}, \
                         in attesa {}, epoca {epoch}",
                        engramma_sync::serve::cell_name(&r, c.sync_id()).unwrap_or_default(),
                        (head - frontier).max(0)
                    );
                }
            }
            Ok(())
        }
        SyncAction::Conflicts => {
            let cells = engramma_sync::sharedcell::SharedCell::all_of(&r)
                .map_err(|e| CliError::internal(e.to_string()))?;
            if cli.json {
                let rows: Vec<serde_json::Value> = cells
                    .iter()
                    .map(|c| {
                        let copies = c
                            .cell()
                            .debug_column(
                                "SELECT copy_id || ' ← ' || original_id \
                                 FROM conflict_copies ORDER BY copy_id",
                            )
                            .unwrap_or_default();
                        serde_json::json!({
                            "cell": engramma_sync::serve::cell_name(
                                &r, c.sync_id()).unwrap_or_default(),
                            "conflicts": copies,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "cells": rows }));
            } else {
                let mut any = false;
                for c in &cells {
                    let copies = c
                        .cell()
                        .debug_column(
                            "SELECT copy_id || ' ← ' || original_id \
                             FROM conflict_copies ORDER BY copy_id",
                        )
                        .unwrap_or_default();
                    for row in copies {
                        any = true;
                        println!("{row}");
                    }
                }
                if !any {
                    println!("nessuna copia di conflitto");
                }
            }
            Ok(())
        }
    }
}

fn mailbox_run(cli: &Cli, action: &MailboxAction) -> Result<(), CliError> {
    let r = root(cli);
    match action {
        MailboxAction::Serve { root: dir, relay } => {
            synccli::mailbox_serve_cmd(cli.json, dir, relay_flag(relay))
        }
        MailboxAction::Add { ticket } => synccli::mailbox_add_cmd(&r, cli.json, ticket),
        MailboxAction::Remove => synccli::mailbox_remove_cmd(&r, cli.json),
        MailboxAction::Status => synccli::mailbox_status_cmd(&r, cli.json),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engramma_core::search::{CosNorm, Fusion};

    #[test]
    fn graph_env_parsing() {
        use engramma_core::search::{GraphPolicy, GraphSeeding};
        let fixed = parse_graph_env("fixed:0,35").unwrap(/* test-only */);
        assert_eq!(fixed.policy, GraphPolicy::Fixed);
        assert!((fixed.weight - 0.35).abs() < 1e-12);
        assert_eq!(fixed.seeding, GraphSeeding::Fused);
        let ad = parse_graph_env("adaptive:0.5,0.1,0.05").unwrap(/* test-only */);
        assert_eq!(
            ad.policy,
            GraphPolicy::Adaptive {
                w_max: 0.5,
                u_min: 0.1
            }
        );
        assert!(matches!(
            ad.seeding,
            GraphSeeding::Question {
                seed_passage_weight: 0.05
            }
        ));
        // a comma decimal inside the numbers would eat a field: refused
        assert!(parse_graph_env("adaptive:0,5,0.1").is_err());
        assert!(parse_graph_env("bogus").is_err());
    }

    #[test]
    fn fusion_parsing_covers_norms_and_comma_decimals() {
        assert!(matches!(parse_fusion("rrf"), Ok(Fusion::Rrf { k: 60 })));
        assert_eq!(
            parse_fusion("convex-0,7").ok(),
            Some(Fusion::Convex {
                alpha: 0.7,
                cos_norm: CosNorm::Candidates
            })
        );
        assert_eq!(
            parse_fusion("convex-mm-0.5").ok(),
            Some(Fusion::Convex {
                alpha: 0.5,
                cos_norm: CosNorm::Candidates
            })
        );
        assert_eq!(
            parse_fusion("convex-th-0.3").ok(),
            Some(Fusion::Convex {
                alpha: 0.3,
                cos_norm: CosNorm::Theoretical
            })
        );
        assert!(parse_fusion("convex-0.9").is_err());
        assert!(parse_fusion("bm25").is_err());
    }
}
