// SPDX-License-Identifier: Apache-2.0

//! The service loop (v0.6.1, P3): `sync run`'s one pass and `sync serve`'s
//! long run, on the production road of P1 (the SharedCell adapter, the
//! arrival gate, the session protocol).
//!
//! `sync run`: one pass with every LIVE device the address book can reach
//! — an unreachable device is a row of the summary, never an error — then
//! the blind mailbox, when one is configured.
//!
//! `sync serve`: the endpoint accepts (the gate first: the caller's iroh
//! key must be a live device's transport key), and the copy syncs at every
//! local write (the `PRAGMA data_version` moves, with a 2 s debounce) and
//! every 5 minutes. SIGINT and SIGTERM stop it clean.

use std::path::Path;
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use rusqlite::OptionalExtension;
use serde_json::json;

use crate::device::{DeviceKey, hex};
use crate::keystore::KeyStore;
use crate::protocol::{self, SessionCtx, SyncedCell, bind_endpoint, run_answerer, run_initiator};
use crate::registry::DeviceRegistry;
use crate::sharedcell::SharedCell;
use crate::{Error, Result};

/// One device's last known address, as the address book keeps it.
#[derive(Debug, Clone)]
pub struct PeerAddr {
    /// The device id (its signing key, hex).
    pub device: String,
    /// The iroh node id (transport public key, raw).
    pub node_id: [u8; 32],
    /// The relay URL, when one was seen.
    pub relay: String,
    /// Direct socket addresses.
    pub addrs: Vec<String>,
}

impl PeerAddr {
    /// Dial the peer the way iroh dials (R2.3, K2): ONE `connect` with
    /// every direct address the book knows AND the relay, bounded at 10 s
    /// in all — the transport races the roads itself, direct first and
    /// the relay beside them. Paying each dead road one at a time (5 s
    /// each, the loopback and the VPN before the relay) cost a device
    /// behind a NAT up to twenty seconds PER PEER, with the serve loop
    /// (one thread) frozen solid.
    ///
    /// # Errors
    /// [`Error::Io`] when no road answers within the bound.
    pub async fn dial(&self, endpoint: &Endpoint) -> Result<Connection> {
        let addr = self.endpoint_addr()?;
        let tried = tokio::time::timeout(
            Duration::from_secs(10),
            endpoint.connect(addr, protocol::ALPN),
        )
        .await;
        match tried {
            Err(_) => Err(Error::Io(std::io::Error::other(
                "no path answered within 10 s",
            ))),
            Ok(Ok(conn)) => Ok(conn),
            // v0.6.4 (D4): the peer's ALPN refused ours — it speaks other
            // materialization rules, and not one operation may move. The
            // name rides up so the user knows WHICH device to upgrade.
            Ok(Err(e)) if crate::looks_like_alpn_refusal(&std::io::Error::other(e.to_string())) => {
                Err(Error::PeerOutdated(self.device.clone()))
            }
            Ok(Err(e)) => Err(Error::Io(std::io::Error::other(e.to_string()))),
        }
    }

    /// The iroh address to connect to.
    ///
    /// # Errors
    /// [`Error::Malformed`] when no usable address is known.
    pub fn endpoint_addr(&self) -> Result<EndpointAddr> {
        let node = iroh::PublicKey::from_bytes(&self.node_id)
            .map_err(|e| Error::Malformed(format!("bad node id: {e}")))?;
        let mut addr = EndpointAddr::from_parts(
            node,
            self.addrs
                .iter()
                .filter_map(|a| a.parse().ok())
                .map(iroh::TransportAddr::Ip)
                .collect::<Vec<_>>(),
        );
        if let Ok(relay) = self.relay.parse() {
            addr = addr.with_relay_url(relay);
        }
        Ok(addr)
    }
}

/// Read one device's address from the book.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn addr_of(root: &Path, device: &str) -> Result<Option<PeerAddr>> {
    let reg = DeviceRegistry::open(root)?;
    let row: Option<String> = reg
        .conn()
        .query_row(
            "SELECT addrs FROM device_addrs WHERE device = ?1",
            [device],
            |r| r.get(0),
        )
        .optional()?;
    let Some(text) = row else {
        return Ok(None);
    };
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Malformed(format!("bad address row: {e}")))?;
    let node_id: [u8; 32] = v["node_id"]
        .as_str()
        .and_then(unhex32)
        .ok_or_else(|| Error::Malformed("the address row misses its node id".into()))?;
    Ok(Some(PeerAddr {
        device: device.to_string(),
        node_id,
        relay: v["relay"].as_str().unwrap_or_default().to_string(),
        addrs: v["addrs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    }))
}

/// Record the address a successful session saw.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn remember_addr(root: &Path, device: &str, addr: &EndpointAddr) -> Result<()> {
    let addrs: Vec<String> = addr
        .addrs
        .iter()
        .filter_map(|a| match a {
            iroh::TransportAddr::Ip(socket) => Some(socket.to_string()),
            _ => None,
        })
        .collect();
    let relay = addr
        .addrs
        .iter()
        .find_map(|a| match a {
            iroh::TransportAddr::Relay(url) => Some(url.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let row = json!({
        "node_id": hex(addr.id.as_bytes()),
        "relay": relay,
        "addrs": addrs,
    });
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "INSERT INTO device_addrs (device, addrs, seen_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(device) DO UPDATE SET addrs = ?2, seen_at = ?3",
        rusqlite::params![device, row.to_string(), now_iso()],
    )?;
    Ok(())
}

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .replace_millisecond(0)
        .unwrap_or(time::OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Seed the book with a minimal row when the device has NONE (K1): an
/// inbound session proves the node id is alive but shows only the
/// caller's source port — dead the moment the caller is a one-shot `sync
/// run` — so it must never OVERWRITE a richer row (the overwrite once
/// left every daemon-side book undialable: a bare node id has no road).
/// A later successful DIAL refreshes the row with what actually worked.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn remember_addr_if_absent(root: &Path, device: &str, addr: &EndpointAddr) -> Result<()> {
    let row = json!({
        "node_id": hex(addr.id.as_bytes()),
        "relay": String::new(),
        "addrs": Vec::<String>::new(),
    });
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "INSERT OR IGNORE INTO device_addrs (device, addrs, seen_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![device, row.to_string(), now_iso()],
    )?;
    Ok(())
}

/// One device's row of the pass summary.
#[derive(Debug, Clone)]
pub struct PeerOutcome {
    /// The device name.
    pub name: String,
    /// The device id.
    pub device: String,
    /// Did a session run?
    pub reached: bool,
    /// Why not, when it did not (one line, no note text).
    pub why_not: Option<String>,
    /// Which road the session rode (R2): `"direct"` or `"relay"`.
    pub via: Option<&'static str>,
}

/// One cell's row of the pass summary.
#[derive(Debug, Clone, Default)]
pub struct CellSummary {
    /// The cell's name in this root.
    pub name: String,
    /// Operations sent this pass.
    pub sent: usize,
    /// Operations received and ingested this pass.
    pub received: usize,
    /// Refusals at the gate this pass.
    pub refused: usize,
    /// Operations this pass could NOT seal or prove (R4): each is an
    /// event in the root, never silence.
    pub unsent: usize,
    /// Conflict copies this copy holds now.
    pub conflicts: usize,
    /// The head seq now.
    pub head: i64,
    /// The stable frontier now.
    pub frontier: i64,
}

/// What one whole pass did.
#[derive(Debug, Clone, Default)]
pub struct PassReport {
    /// One row per device the pass tried.
    pub peers: Vec<PeerOutcome>,
    /// One row per shared cell.
    pub cells: Vec<CellSummary>,
}

/// Bind this root's sync endpoint (the transport key of the device key),
/// with the root's relay choice (R2).
///
/// # Errors
/// [`Error::Io`] when iroh cannot bind.
pub async fn bind(
    key: &DeviceKey,
    bind_addr: &str,
    choice: &crate::relay::RelayChoice,
) -> Result<Endpoint> {
    bind_endpoint(key, bind_addr, choice).await
}

/// What one session did, for the pass summary: the protocol's own report
/// and which road the connection rode (R2).
#[derive(Debug)]
pub struct SessionOutcome {
    /// The protocol's per-cell report.
    pub report: protocol::SessionReport,
    /// `"direct"` or `"relay"`.
    pub via: &'static str,
}

/// Run ONE session as the initiator against one peer, over an already
/// connected transport, and record the addresses.
///
/// # Errors
/// Whatever the session fails with — the caller turns it into a summary
/// row, not an error.
pub async fn session_with(
    root: &Path,
    conn: Connection,
    peer_device: &str,
    peer_addr: &EndpointAddr,
) -> Result<SessionOutcome> {
    let registry = DeviceRegistry::open(root)?;
    let dk = DeviceKey::load_or_create(root)?;
    let cells = SharedCell::all_of(root)?;
    for shared in &cells {
        shared
            .cell()
            .set_signer(std::sync::Arc::new(crate::arrival::DeviceSigner::new(&dk)))
            .ok();
    }
    let ks = KeyStore::new(registry.conn());
    let ctx = SessionCtx {
        registry: &registry,
        keys: ks,
        device: &dk,
        cells: cells.iter().map(|c| c as &dyn SyncedCell).collect(),
    };
    // a bounded session: a wedged exchange must not hang the pass (the
    // one-shot run above all — nothing else would ever unblock it)
    let report = tokio::time::timeout(Duration::from_secs(90), run_initiator(conn.clone(), &ctx))
        .await
        .map_err(|_| Error::Malformed("the session did not end within 90 s".into()))??;
    // which road the session rode (the connection outlives the exchange)
    let via = via_of_conn(&conn);
    // the session ended: both sides hold the same sets — the acks move the
    // frontier, ours for them
    for shared in &cells {
        shared.cell().record_session_ack(peer_device).ok();
    }
    remember_addr(root, peer_device, peer_addr)?;
    Ok(SessionOutcome { report, via })
}

/// Serve ONE inbound session (the gate already passed).
///
/// # Errors
/// Whatever the session fails with.
pub async fn answer_session(root: &Path, conn: Connection) -> Result<protocol::SessionReport> {
    let registry = DeviceRegistry::open(root)?;
    let dk = DeviceKey::load_or_create(root)?;
    let cells = SharedCell::all_of(root)?;
    for shared in &cells {
        shared
            .cell()
            .set_signer(std::sync::Arc::new(crate::arrival::DeviceSigner::new(&dk)))
            .ok();
    }
    let ks = KeyStore::new(registry.conn());
    let ctx = SessionCtx {
        registry: &registry,
        keys: ks,
        device: &dk,
        cells: cells.iter().map(|c| c as &dyn SyncedCell).collect(),
    };
    run_answerer(conn, &ctx).await
}

/// Which road a connection rode (R2): the SELECTED path says it — a relay
/// transport or a direct IP one. The summary's per-device line carries it.
#[must_use]
pub fn via_of_conn(conn: &Connection) -> &'static str {
    let paths = conn.paths();
    if let Some(path) = paths.iter().find(|p| p.is_selected()) {
        return if path.is_relay() { "relay" } else { "direct" };
    }
    if paths.iter().any(|p| p.is_relay()) {
        "relay"
    } else {
        "direct"
    }
}

/// The device id this root is.
#[must_use]
pub fn self_device(root: &Path) -> String {
    DeviceKey::load_or_create(root)
        .map(|k| k.device_id())
        .unwrap_or_default()
}

/// One pass with every live device the book can reach: an unreachable
/// device is a row, never an error. The summary carries the per-device
/// outcomes and the per-cell deltas of the pass (operations, refusals,
/// conflicts, frontier). `skip` names devices this pass must not dial
/// (K1: the rows that came FROM them never bounce back to them — the
/// clock pass skips nobody).
///
/// # Errors
/// Only infrastructure failures (a broken root), never a peer.
pub async fn run_pass(root: &Path, endpoint: &Endpoint, skip: &[String]) -> Result<PassReport> {
    let dk = DeviceKey::load_or_create(root)?;
    let registry = DeviceRegistry::open(root)?;
    let mut report = PassReport::default();
    let conflicts_now = conflict_counts(root)?;
    let mut per_cell: std::collections::HashMap<Vec<u8>, (usize, usize, usize, usize)> =
        std::collections::HashMap::new();
    // R4: the pass starts by forgetting the cells' unsent events — what
    // still cannot leave will be named again below
    for shared in SharedCell::all_of(root)? {
        if let Ok(name) = cell_name(root, shared.sync_id()) {
            crate::sharedcell::clear_unsent(root, &name).ok();
        }
    }
    // the blind node, when one is configured: deposit what it lacks,
    // fetch what we lack, deliver the purge queue (a failure is a trace
    // line under ENGRAMMA_TRACE, never a failed pass)
    if let Err(e) = crate::mailbox_net::push(root, endpoint).await
        && std::env::var("ENGRAMMA_TRACE").is_ok()
    {
        eprintln!("mailbox push: {e}");
    }
    for member in registry.live_members()? {
        if member.device == dk.device_id() || skip.contains(&member.device) {
            continue;
        }
        let Some(addr) = addr_of(root, &member.device)? else {
            report.peers.push(PeerOutcome {
                name: member.name.clone(),
                device: member.device.clone(),
                reached: false,
                why_not: Some("no known address: an invite or a session is needed".into()),
                via: None,
            });
            continue;
        };
        let endpoint_addr = addr.endpoint_addr()?;
        // one address at a time (loopback first), each bounded: an
        // unreachable device must not stall the pass (the serve loop is
        // one thread — a hanging dial would freeze the accepts too)
        let dialed = addr.dial(endpoint).await;
        let mut via = None;
        let outcome = match dialed {
            Err(e) => Err(e.to_string()),
            Ok(conn) => match session_with(root, conn, &member.device, &endpoint_addr).await {
                Ok(session) => {
                    for cell in &session.report.cells {
                        let entry = per_cell.entry(cell.sync_id.clone()).or_insert((0, 0, 0, 0));
                        entry.0 += cell.sent;
                        entry.1 += cell.received;
                        entry.2 += cell.refused;
                        entry.3 += cell.unsent;
                    }
                    via = Some(session.via);
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            },
        };
        report.peers.push(PeerOutcome {
            name: member.name.clone(),
            device: member.device.clone(),
            reached: outcome.is_ok(),
            why_not: outcome.err(),
            via,
        });
    }
    for shared in SharedCell::all_of(root)? {
        let name = cell_name(root, shared.sync_id())?;
        let delta = per_cell
            .remove(shared.sync_id().as_slice())
            .unwrap_or((0, 0, 0, 0));
        let path_key = shared.cell().file_path().display().to_string();
        report.cells.push(CellSummary {
            name,
            sent: delta.0,
            received: delta.1,
            refused: delta.2,
            unsent: delta.3,
            conflicts: conflicts_now
                .iter()
                .find(|(p, _)| *p == path_key)
                .map(|(_, n)| *n)
                .unwrap_or(0),
            head: shared.head_seq(),
            frontier: shared.cell().stable_frontier().unwrap_or(0),
        });
    }
    Ok(report)
}

/// The cell name a root binds to a sync id.
pub fn cell_name(root: &Path, sync_id: &[u8; 32]) -> Result<String> {
    let reg = DeviceRegistry::open(root)?;
    let name: Option<String> = reg
        .conn()
        .query_row(
            "SELECT name FROM shared_cells WHERE sync_id = ?1",
            [sync_id],
            |r| r.get(0),
        )
        .optional()?;
    name.ok_or_else(|| Error::Registry("the shared cell is not registered".into()))
}

fn conflict_counts(root: &Path) -> Result<Vec<(String, usize)>> {
    let mut out = Vec::new();
    for shared in SharedCell::all_of(root)? {
        let n = shared
            .cell()
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies")
            .unwrap_or_default()
            .first()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        out.push((shared.cell().file_path().display().to_string(), n));
    }
    Ok(out)
}

/// The cells' `PRAGMA data_version`, read on PERSISTENT connections: the
/// serve loop's CHEAP poll (C4). The counter is PER-CONNECTION — it only
/// moves for commits made by OTHER connections while THIS one stays open
/// — so a watch that opens a fresh connection per poll never sees another
/// process's write (the Homun daemon, the CLI beside the serve: found by
/// the C4 measure, which waited forever). The handles stay open for the
/// watch's whole life; `refresh` re-opens them (a cell shared after the
/// serve started gets watched at the next refresh).
///
/// But the FILE's counter is only the doorbell: when it moves, the guard
/// compares the LOG's marker — `(count, max rowid)` of `ops`, per cell —
/// with its baseline, and the rows our own sessions ingested with what
/// the sessions' reports say (v0.6.1-ciclo, K1):
///
/// - rows beyond the sessions' own work are LOCAL writes (another
///   process committed): a pass toward EVERY live device;
/// - rows that are all the sessions' work came FROM their devices: a
///   pass toward the OTHERS, never back to the sources alone;
/// - no rows at all: the file moved for bookkeeping that is not
///   operations — no pass, and the baselines settle.
///
/// The baselines settle AFTER the pass (`settle`), never before: the
/// pass's own writes are not news. A watch with a pass armed or running
/// is `busy` and answers no poll until that pass settles.
pub struct DataVersionWatch {
    root: std::path::PathBuf,
    cells: Vec<SharedCell>,
    last: Vec<(String, i64)>,
    marker: Vec<(String, (i64, i64))>,
    /// The rows the serve's own sessions ingested since the last settle,
    /// per device that fed them, PER CELL (v0.6.2, S1) — the sums of the
    /// v0.6.1 guard let one cell's purge cover another cell's write, and
    /// a refused-but-counted envelope in one cell read a local write in
    /// another as session work.
    fed: std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    /// A pass is armed or running: the baselines settle when it ends.
    busy: bool,
    /// The log's marker as the armed pass BEGAN: what the pass may count
    /// as its own, and the line beyond which rows that arrived mid-pass
    /// are NEW local work the settled baselines must not swallow.
    pass_start: Option<Vec<(String, (i64, i64))>>,
}

/// What the guard decided when the file's counter moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassDecision {
    /// Local rows (another process wrote): a pass toward every live
    /// device — the sources included, they have not seen these rows.
    All,
    /// Only rows the sessions brought: a pass toward the others, never
    /// again toward these devices alone — they hold what they sent.
    Others(Vec<String>),
    /// The file moved, the log did not: bookkeeping, not operations.
    /// No pass; the baselines have already settled.
    None,
}

impl DataVersionWatch {
    /// Open every shared cell of a root and read the counters once.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn new(root: &Path) -> Result<Self> {
        let cells = SharedCell::all_of(root)?;
        let last = Self::read(&cells, root)?;
        let marker = Self::read_markers(&cells, root)?;
        Ok(Self {
            root: root.to_path_buf(),
            cells,
            last,
            marker,
            fed: std::collections::HashMap::new(),
            busy: false,
            pass_start: None,
        })
    }

    /// An empty watch (the degraded start path: it never fires, the
    /// periodic pass still runs, the next start retries).
    #[must_use]
    pub fn empty(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            cells: Vec::new(),
            last: Vec::new(),
            marker: Vec::new(),
            fed: std::collections::HashMap::new(),
            busy: false,
            pass_start: None,
        }
    }

    fn read(cells: &[SharedCell], root: &Path) -> Result<Vec<(String, i64)>> {
        let mut out = Vec::new();
        for shared in cells {
            out.push((
                cell_name(root, shared.sync_id())?,
                shared
                    .cell()
                    .data_version()
                    .map_err(|e| Error::Cell(e.to_string()))?,
            ));
        }
        Ok(out)
    }

    fn read_markers(cells: &[SharedCell], root: &Path) -> Result<Vec<(String, (i64, i64))>> {
        let mut out = Vec::new();
        for shared in cells {
            out.push((
                cell_name(root, shared.sync_id())?,
                shared
                    .cell()
                    .ops_marker()
                    .map_err(|e| Error::Cell(e.to_string()))?,
            ));
        }
        Ok(out)
    }

    /// The cheap poll on the SAME connections: `true` when any file's
    /// counter moved (another process's commit included — that is the
    /// point). The baselines do NOT move here.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn moved(&self) -> Result<bool> {
        let now = Self::read(&self.cells, &self.root)?;
        Ok(now != self.last)
    }

    /// Is a pass armed or running? While it is, the guard answers no
    /// poll: its own writes are not news, and the baselines settle when
    /// the pass ends.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Note what one of the serve's own sessions ingested (K1): the rows
    /// the session's report counts, PER CELL, and the device that fed
    /// them. The next evaluation subtracts them from each cell's own
    /// delta — what is left is local work, and the sources are the pass's
    /// exclusions.
    pub fn note_session(&mut self, device: &str, cell_rows: &[(String, i64)]) {
        let per = self.fed.entry(device.to_string()).or_default();
        for (cell, rows) in cell_rows {
            if *rows > 0 {
                *per.entry(cell.clone()).or_insert(0) += *rows;
            }
        }
    }

    /// Decide, now that the file's counter moved and the debounce is
    /// over: compare the LOG's marker with its baseline and the sessions'
    /// ledger, **per cell** (v0.6.2, S1). A cell's rows are judged
    /// against the rows THIS cell's sessions brought — one cell's story
    /// (a ledger inflated by a refused envelope, rows that vanished)
    /// can no longer decide another cell's:
    ///
    /// - any cell with rows beyond its sessions' work has LOCAL writes:
    ///   a pass toward EVERY live device (they have not seen them);
    /// - cells whose rows are all the sessions' own work name their
    ///   sources: a pass toward the OTHERS, never back to them (K1);
    /// - no rows anywhere: bookkeeping, not operations — no pass, and
    ///   the baselines settle.
    ///
    /// When the answer is a pass, the baselines stay where they are —
    /// `settle` moves them when the pass is done (K1: the ack comes AFTER
    /// the pass, never before, or the pass's own writes re-light the
    /// cycle). When the answer is nothing, the baselines settle here.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn evaluate(&mut self) -> Result<PassDecision> {
        let now = Self::read_markers(&self.cells, &self.root)?;
        let mut any_local = false;
        let mut sources: Vec<String> = Vec::new();
        for (cell, base) in &self.marker {
            let Some(current) = now.iter().find(|(name, _)| name == cell) else {
                continue;
            };
            let delta = current.1.0 - base.0;
            let fed_cell: i64 = self
                .fed
                .values()
                .map(|per| per.get(cell).copied().unwrap_or(0))
                .sum();
            if delta > fed_cell {
                any_local = true;
            } else if delta > 0 && fed_cell > 0 {
                for (device, per) in &self.fed {
                    if per.get(cell).copied().unwrap_or(0) > 0 && !sources.contains(device) {
                        sources.push(device.clone());
                    }
                }
            }
        }
        let decision = if any_local {
            PassDecision::All
        } else if sources.is_empty() {
            PassDecision::None
        } else {
            PassDecision::Others(sources)
        };
        match decision {
            PassDecision::None => self.settle()?,
            _ => {
                self.busy = true;
                // where the pass begins: rows beyond this line that are
                // not the sessions' own work arrived MID-PASS, and the
                // settle must hand them to the next pass, not swallow
                self.pass_start = Some(now);
            }
        }
        Ok(decision)
    }

    /// Take the current counters as the new baselines, forget the
    /// sessions' ledger, and lower the busy flag: the pass (if any) is
    /// done, and what it wrote is not news anymore.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn settle(&mut self) -> Result<()> {
        self.last = Self::read(&self.cells, &self.root)?;
        self.marker = Self::read_markers(&self.cells, &self.root)?;
        self.fed.clear();
        self.busy = false;
        self.pass_start = None;
        Ok(())
    }

    /// Close an armed pass (K1), PER CELL (v0.6.2, S1): settle the
    /// baselines, and say whether rows that are NOT the sessions' own
    /// work arrived WHILE the pass ran — a local write that lands mid-pass
    /// must not be swallowed by the settle (its baseline would take it as
    /// already seen), and a receipt earned in ONE cell settles only that
    /// cell. The caller answers `true` with another pass.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn finish_pass(&mut self, pass_received: &[(String, i64)]) -> Result<bool> {
        let now = Self::read_markers(&self.cells, &self.root)?;
        let start = self.pass_start.clone().unwrap_or_else(|| now.clone());
        let mut again = false;
        for (cell, base) in &start {
            let Some(current) = now.iter().find(|(name, _)| name == cell) else {
                continue;
            };
            let delta = current.1.0 - base.0;
            // the sessions' own work during the pass, in THIS cell: what
            // the inbound sessions noted in the ledger, and what the pass
            // itself received here
            let receipt: i64 = pass_received
                .iter()
                .find(|(name, _)| name == cell)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            let fed_cell: i64 = self
                .fed
                .values()
                .map(|per| per.get(cell).copied().unwrap_or(0))
                .sum();
            if delta > fed_cell + receipt {
                again = true;
            }
        }
        self.last = Self::read(&self.cells, &self.root)?;
        self.marker = now;
        self.fed.clear();
        self.busy = false;
        self.pass_start = None;
        Ok(again)
    }

    /// Re-open the handles (a newly shared cell joins the watch). Never
    /// while a pass is armed or running: the re-ack would swallow the
    /// pass's own writes mid-flight.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn refresh(&mut self) -> Result<()> {
        if self.busy {
            return Ok(());
        }
        let watch = Self::new(&self.root)?;
        self.cells = watch.cells;
        self.last = watch.last;
        self.marker = watch.marker;
        self.fed.clear();
        Ok(())
    }
}

/// One-shot read on FRESH connections (diagnostics; a persistent
/// [`DataVersionWatch`] is what the serve loop needs).
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn data_versions(root: &Path) -> Result<Vec<(String, i64)>> {
    let mut out = Vec::new();
    for shared in SharedCell::all_of(root)? {
        out.push((
            cell_name(root, shared.sync_id())?,
            shared
                .cell()
                .data_version()
                .map_err(|e| Error::Cell(e.to_string()))?,
        ));
    }
    Ok(out)
}

/// How often the serve loop re-syncs on the clock.
pub const PERIODIC: Duration = Duration::from_secs(5 * 60);
/// The write debounce: local writes settle before a pass runs.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// How fast the data_version watch polls.
pub const WATCH_TICK: Duration = Duration::from_millis(250);

fn unhex32(hex: &str) -> Option<[u8; 32]> {
    let mut out = [0_u8; 32];
    if hex.len() != 64 {
        return None;
    }
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// The endpoint's direct socket addresses (what a ticket or a summary
/// shows): the CONCRETE interfaces iroh discovered, and nothing else (K2)
/// — no `0.0.0.0` (not dialable) and no invented `127.0.0.1` (loopback
/// serves the same machine only: publishing it to other devices hands
/// them a road that dies one machine away). A peer on this same machine
/// reaches the endpoint through the real interfaces, which answer
/// loopback traffic too; a test that runs on one machine writes the
/// loopback addresses in the book by hand.
#[must_use]
pub fn direct_addresses(endpoint: &Endpoint) -> Vec<String> {
    endpoint
        .addr()
        .addrs
        .iter()
        .filter_map(|a| match a {
            iroh::TransportAddr::Ip(socket) if !socket.ip().is_unspecified() => {
                Some(socket.to_string())
            }
            _ => None,
        })
        .collect()
}

/// The endpoint's node id (its transport public key), raw.
#[must_use]
pub fn node_id_of(endpoint: &Endpoint) -> [u8; 32] {
    *endpoint.secret_key().public().as_bytes()
}

/// A minimal address for a known node id (no direct addresses, no relay):
/// reachable only through lookup — the address book's fallback row.
///
/// # Errors
/// [`Error::Malformed`] on a bad id.
pub fn addr_for(node_id: &[u8; 32]) -> Result<EndpointAddr> {
    let node = iroh::PublicKey::from_bytes(node_id)
        .map_err(|e| Error::Malformed(format!("bad node id: {e}")))?;
    Ok(EndpointAddr::new(node))
}

/// The peer's minimal address from a connection (its node id): the book's
/// fallback row — a later successful dial refreshes it with what worked.
pub fn peer_addr_of_conn(conn: &Connection) -> Result<EndpointAddr> {
    addr_for(conn.remote_id().as_bytes())
}
