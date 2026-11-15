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
use crate::protocol::{
    self, SessionCtx, SyncedCell, bind_endpoint, connect, run_answerer, run_initiator,
};
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
    /// Which road the session rode (R2): `"diretta"` or `"relay"`.
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
    /// `"diretta"` or `"relay"`.
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
        .map_err(|_| Error::Malformed("la sessione non è finita in 90 s".into()))??;
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
        return if path.is_relay() { "relay" } else { "diretta" };
    }
    if paths.iter().any(|p| p.is_relay()) {
        "relay"
    } else {
        "diretta"
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
/// conflicts now, frontier).
///
/// # Errors
/// Only infrastructure failures (a broken root), never a peer.
pub async fn run_pass(root: &Path, endpoint: &Endpoint) -> Result<PassReport> {
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
        if member.device == dk.device_id() {
            continue;
        }
        let Some(addr) = addr_of(root, &member.device)? else {
            report.peers.push(PeerOutcome {
                name: member.name.clone(),
                device: member.device.clone(),
                reached: false,
                why_not: Some("nessun indirizzo noto: servono un invito o una sessione".into()),
                via: None,
            });
            continue;
        };
        let endpoint_addr = match addr.endpoint_addr() {
            Ok(a) => a,
            Err(e) => {
                report.peers.push(PeerOutcome {
                    name: member.name.clone(),
                    device: member.device.clone(),
                    reached: false,
                    why_not: Some(format!("indirizzo inutilizzabile: {e}")),
                    via: None,
                });
                continue;
            }
        };
        // a bounded dial: an unreachable device must not stall the pass
        // (the serve loop is one thread — a hanging dial would freeze the
        // accepts too)
        let dialed = tokio::time::timeout(
            Duration::from_secs(10),
            connect(endpoint, endpoint_addr.clone()),
        )
        .await;
        let mut via = None;
        let outcome = match dialed {
            Err(_) => Err("timeout della connessione (10 s)".to_string()),
            Ok(Ok(conn)) => match session_with(root, conn, &member.device, &endpoint_addr).await {
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
            Ok(Err(e)) => Err(e.to_string()),
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

/// The cells' `PRAGMA data_version` (moves on every committed write — ours
/// or another process's on the same file): the serve loop's write trigger.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn data_versions(root: &Path) -> Result<Vec<(String, i64)>> {
    let mut out = Vec::new();
    for shared in SharedCell::all_of(root)? {
        let v = shared
            .cell()
            .debug_column("PRAGMA data_version")
            .unwrap_or_default()
            .first()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        out.push((cell_name(root, shared.sync_id())?, v));
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
/// shows). A wildcard bind publishes as loopback: the address must be
/// DIALABLE, and `0.0.0.0` is not.
#[must_use]
pub fn direct_addresses(endpoint: &Endpoint) -> Vec<String> {
    endpoint
        .addr()
        .addrs
        .iter()
        .filter_map(|a| match a {
            iroh::TransportAddr::Ip(socket) => {
                if socket.ip().is_unspecified() {
                    Some(
                        std::net::SocketAddr::new(
                            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                            socket.port(),
                        )
                        .to_string(),
                    )
                } else {
                    Some(socket.to_string())
                }
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
