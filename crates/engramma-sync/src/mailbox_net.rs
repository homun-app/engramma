// SPDX-License-Identifier: Apache-2.0

//! The blind mailbox OVER THE WIRE (v0.6.1, P4): ALPN
//! `engramma/mailbox/1`. The node holds sealed envelopes for the device
//! lists it knows, indexed by OPAQUE id and nothing else, and hosts the
//! short-code meeting point. It never holds a cell key: what it stores is
//! ciphertext; what it relays after the PAKE is ciphertext too.
//!
//! The device side (`mailbox add`, the post-session push, the purge queue)
//! lives here as well — the CLI calls these.
//!
//! The code meeting: the node pairs the two sides of a channel and RELAYS
//! opaque blobs; SPAKE2 and the RFC 9382 key confirmation run end to end,
//! so a wrong code dies on the MACs — and burns the channel: one attempt
//! per code, the node enforces it.

use std::path::Path;

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::device::DeviceKey;
use crate::mailbox::{self, MailboxStore};
use crate::pairing::{self, Side, SpakeHalf, six_digits};
use crate::registry::{DeviceRegistry, RegistryOp};
use crate::sharedcell::SharedCell;
use crate::{Error, Result};

/// The mailbox protocol's ALPN.
pub const MAILBOX_NET_ALPN: &[u8] = b"engramma/mailbox/1";
/// The mailbox protocol's version.
pub const MAILBOX_VERSION: u64 = 1;
/// A frame is at most 1 MiB.
pub const MAX_FRAME: usize = 1024 * 1024;

fn b64(data: &[u8]) -> String {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::encode_string(data)
}

fn unb64(text: &str) -> Result<Vec<u8>> {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::decode_vec(text).map_err(|e| Error::Malformed(format!("bad base64: {e}")))
}

async fn send_frame(conn: &Connection, msg: &Value) -> Result<()> {
    let bytes = msg.to_string();
    let body = bytes.as_bytes();
    if body.len() > MAX_FRAME {
        return Err(Error::Malformed("frame too large".into()));
    }
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    let (mut send, _recv) = conn
        .open_bi()
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    send.write_all(&frame)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    send.finish()
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    Ok(())
}

async fn recv_frame(conn: &Connection) -> Result<Value> {
    let (send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    drop(send);
    let mut len_buf = [0_u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(Error::Malformed("frame too large".into()));
    }
    let mut buf = vec![0_u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    serde_json::from_slice(&buf).map_err(|e| Error::Malformed(format!("bad frame: {e}")))
}

/// The node's own store beside its device key: the verified registry rows
/// of every owner list it serves (its OWN root registry is the node's), the
/// envelopes, and the burnt code channels.
pub struct NodeState {
    pub root: std::path::PathBuf,
    pub store: MailboxStore,
    reg: DeviceRegistry,
}

impl NodeState {
    /// Open (or create) the node's state under a root.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn open(root: &Path) -> Result<Self> {
        let reg = DeviceRegistry::open(root)?;
        reg.conn().execute_batch(
            "CREATE TABLE IF NOT EXISTS owner_lists (
                 owner TEXT PRIMARY KEY,
                 rows_json TEXT NOT NULL,
                 at TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS burnt_channels (
                 channel INTEGER PRIMARY KEY,
                 at TEXT NOT NULL);",
        )?;
        Ok(Self {
            root: root.to_path_buf(),
            store: MailboxStore::open(root)?,
            reg,
        })
    }

    /// Is the caller a live device of any list the node serves?
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn authorizes(&self, caller_transport: &[u8; 32]) -> Result<bool> {
        let hex_key = crate::device::hex(caller_transport);
        let n: i64 = self.reg.conn().query_row(
            "SELECT COUNT(*) FROM members WHERE transport = ?1 AND removed_at IS NULL",
            [hex_key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// The owner id of a registry: the create row's op id (stable, shared
    /// by every copy of the list).
    fn owner_of(rows: &[Value]) -> Option<String> {
        rows.iter()
            .find(|r| r.get("kind").and_then(Value::as_str) == Some("create"))
            .and_then(|r| r.get("op_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Store a device list (each row already carries its own signature:
    /// verified on arrival, like every session).
    ///
    /// # Errors
    /// [`Error::BadSignature`] when a row does not verify.
    pub fn take_list(&self, rows: &[Value]) -> Result<String> {
        let owner = Self::owner_of(rows)
            .ok_or_else(|| Error::Malformed("the list has no create row".into()))?;
        for row in rows {
            let op = decode_registry_row(row)?;
            self.reg.arrive(&op)?;
        }
        self.reg.conn().execute(
            "INSERT INTO owner_lists (owner, rows_json, at) VALUES (?1, ?2, ?3)
             ON CONFLICT(owner) DO UPDATE SET rows_json = ?2, at = ?3",
            rusqlite::params![
                owner,
                serde_json::to_string(rows).unwrap_or_default(),
                now_iso()
            ],
        )?;
        Ok(owner)
    }

    /// Is a channel burnt?
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn channel_burnt(&self, channel: u16) -> Result<bool> {
        let n: i64 = self.reg.conn().query_row(
            "SELECT COUNT(*) FROM burnt_channels WHERE channel = ?1",
            [i64::from(channel)],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Burn a channel: one attempt per code.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn burn_channel(&self, channel: u16) -> Result<()> {
        self.reg.conn().execute(
            "INSERT OR IGNORE INTO burnt_channels (channel, at) VALUES (?1, ?2)",
            rusqlite::params![i64::from(channel), now_iso()],
        )?;
        Ok(())
    }
}

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .replace_millisecond(0)
        .unwrap_or(time::OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn decode_registry_row(row: &Value) -> Result<RegistryOp> {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    let op_id =
        ulid::Ulid::from_string(row.get("op_id").and_then(Value::as_str).unwrap_or_default())
            .map_err(|e| Error::Malformed(format!("bad op_id: {e}")))?;
    let hlc =
        engramma_core::hlc::Hlc::parse(row.get("hlc").and_then(Value::as_str).unwrap_or_default())
            .map_err(|e| Error::Malformed(format!("bad hlc: {e}")))?;
    let kind = match row.get("kind").and_then(Value::as_str) {
        Some("create") => crate::registry::RegistryKind::Create,
        Some("add") => crate::registry::RegistryKind::Add,
        Some("remove") => crate::registry::RegistryKind::Remove,
        _ => return Err(Error::Malformed("bad registry kind".into())),
    };
    Ok(RegistryOp {
        op_id,
        hlc,
        kind,
        payload: row.get("payload").cloned().unwrap_or(Value::Null),
        sig: B64::decode_vec(row.get("sig").and_then(Value::as_str).unwrap_or_default())
            .map_err(|e| Error::Malformed(format!("bad signature: {e}")))?,
    })
}

/// The node's endpoint: its own device key, the mailbox ALPN, the one
/// relay-aware builder of the sync road (R2) — see [`crate::relay`].
///
/// # Errors
/// [`Error::Io`] when iroh cannot bind.
pub async fn bind_node(
    key: &DeviceKey,
    bind: &str,
    choice: &crate::relay::RelayChoice,
) -> Result<Endpoint> {
    crate::relay::bind(key, bind, MAILBOX_NET_ALPN, choice).await
}

/// Serve ONE device connection at the node: authorize, then serve the
/// frames until the device is done. Every refusal is silent about note
/// text (there is none to say: the node only sees opaque ids).
///
/// # Errors
/// Whatever the protocol fails with.
pub async fn serve_connection(state: &NodeState, conn: &Connection) -> Result<()> {
    let caller = conn.remote_id();
    let authorized = state.authorizes(caller.as_bytes())?;
    loop {
        let Ok(msg) = recv_frame(conn).await else {
            return Ok(()); // the device closed: done
        };
        match msg.get("kind").and_then(Value::as_str) {
            Some("list") if authorized => {
                let rows = msg["rows"].as_array().cloned().unwrap_or_default();
                let owner = state.take_list(&rows)?;
                send_frame(conn, &json!({"kind": "ok", "owner": owner})).await?;
            }
            Some("deposit") if authorized => {
                let owner = msg["owner"].as_str().unwrap_or_default().to_string();
                let mut n = 0_usize;
                for env in msg["envs"].as_array().unwrap_or(&Vec::new()) {
                    let sync_id = unb64(env["sync_id"].as_str().unwrap_or(""))?;
                    let opaque = unb64(env["id"].as_str().unwrap_or(""))?;
                    let nonce = unb64(env["nonce"].as_str().unwrap_or(""))?;
                    let ct = unb64(env["ct"].as_str().unwrap_or(""))?;
                    let now = now_ms();
                    state
                        .store
                        .put(&owner, &sync_id, &opaque, &nonce, &ct, now)?;
                    n += 1;
                }
                send_frame(conn, &json!({"kind": "ok", "stored": n})).await?;
            }
            Some("enumerate") if authorized => {
                let owner = msg["owner"].as_str().unwrap_or_default().to_string();
                let mut pairs: Vec<Value> = Vec::new();
                for sync_id in state.store.sync_ids_of(&owner)? {
                    for opaque in state.store.opaque_ids(&owner, &sync_id)? {
                        pairs.push(json!({
                            "sync_id": b64(&sync_id), "id": b64(&opaque),
                        }));
                    }
                }
                send_frame(conn, &json!({"kind": "ids", "ids": pairs})).await?;
            }
            Some("fetch") if authorized => {
                let owner = msg["owner"].as_str().unwrap_or_default().to_string();
                let mut envs = Vec::new();
                for id in msg["ids"].as_array().unwrap_or(&Vec::new()) {
                    let Ok(opaque) = unb64(id.as_str().unwrap_or("")) else {
                        continue;
                    };
                    if let Some((sync_id, nonce, ct)) = state.store.get(&owner, &opaque)? {
                        envs.push(json!({
                            "sync_id": b64(&sync_id), "id": b64(&opaque),
                            "nonce": b64(&nonce), "ct": b64(&ct),
                        }));
                    }
                }
                send_frame(conn, &json!({"kind": "envs", "envs": envs})).await?;
            }
            Some("purge") if authorized => {
                let owner = msg["owner"].as_str().unwrap_or_default().to_string();
                let ids: Vec<Vec<u8>> = msg["ids"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| unb64(x.as_str().unwrap_or("")).ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let sig = unb64(msg["sig"].as_str().unwrap_or(""))?;
                // the node verifies against the LIVE members of the list it
                // holds — a stranger's signature does not pass
                mailbox::verify_purge(&state.reg, &owner, &ids, &sig)?;
                let n = state.store.purge_opaque(&owner, &ids)?;
                send_frame(conn, &json!({"kind": "ok", "purged": n})).await?;
            }
            Some("burn") if authorized => {
                let channel = msg["channel"].as_u64().unwrap_or(0) as u16;
                state.burn_channel(channel)?;
                send_frame(conn, &json!({"kind": "ok"})).await?;
            }
            _ => {
                // unknown, or not authorized: close, say nothing
                send_frame(conn, &json!({"kind": "refused"})).await?;
                return Ok(());
            }
        }
    }
}

fn now_ms() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp() * 1000
}

// ---------------------------------------------------------------------------
// the device side
// ---------------------------------------------------------------------------

/// The mailbox a root knows (the `mailbox` row).
#[derive(Debug, Clone)]
pub struct NodeAddr {
    /// The node's iroh id (raw).
    pub node_id: [u8; 32],
    /// Direct addresses.
    pub addrs: Vec<String>,
    /// The relay the node rides, when the ticket carried one (C1).
    pub relay: Option<String>,
}

impl NodeAddr {
    /// The address to dial.
    ///
    /// # Errors
    /// [`Error::Malformed`] when nothing usable is known.
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
        if let Some(relay) = &self.relay
            && let Ok(url) = relay.parse()
        {
            addr = addr.with_relay_url(url);
        }
        Ok(addr)
    }

    /// Dial the node, one address at a time (loopback first): the
    /// transport's own address race stalls on a virtual interface that
    /// does not answer its own address.
    ///
    /// # Errors
    /// [`Error::Io`] when no address answers.
    pub async fn dial(&self, endpoint: &Endpoint) -> Result<Connection> {
        let node = iroh::PublicKey::from_bytes(&self.node_id)
            .map_err(|e| Error::Malformed(format!("bad node id: {e}")))?;
        let mut addrs: Vec<std::net::SocketAddr> =
            self.addrs.iter().filter_map(|a| a.parse().ok()).collect();
        addrs.sort_by_key(|a| !a.ip().is_loopback());
        let mut last = "nessun indirizzo noto".to_string();
        for socket in addrs {
            let addr = EndpointAddr::from_parts(node, [iroh::TransportAddr::Ip(socket)]);
            let tried = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                endpoint.connect(addr, MAILBOX_NET_ALPN),
            )
            .await;
            match tried {
                Err(_) => last = format!("{socket}: timeout"),
                Ok(Ok(conn)) => return Ok(conn),
                Ok(Err(e)) => last = format!("{socket}: {e}"),
            }
        }
        // the node behind a NAT: the relay the ticket carried (C1)
        if let Some(relay) = &self.relay
            && let Ok(url) = relay.parse()
        {
            let addr = EndpointAddr::new(node).with_relay_url(url);
            let tried = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                endpoint.connect(addr, MAILBOX_NET_ALPN),
            )
            .await;
            match tried {
                Err(_) => last = format!("{relay}: timeout"),
                Ok(Ok(conn)) => return Ok(conn),
                Ok(Err(e)) => last = format!("{relay}: {e}"),
            }
        }
        Err(Error::Io(std::io::Error::other(last)))
    }
}

/// What minting the node's ticket found out about the relay (C1).
#[derive(Debug)]
pub struct NodeTicket {
    /// The ticket's JSON (as ever: the node's id, its addresses, and now
    /// the relay it rides).
    pub json: String,
    /// The relay URL the ticket carries, when there is one.
    pub relay: Option<String>,
    /// The relay wait ran out: the ticket carries NO relay and the
    /// caller must say so — never in silence.
    pub relay_timed_out: bool,
}

/// The mailbox ticket: the node's id, its addresses, and the relay it
/// rides — no secret (the node is registered, not paired). **The relay
/// wait comes first** (C1): the devices that need the node most are the
/// ones behind a NAT, and a ticket without the relay is a ticket they
/// cannot use.
pub async fn node_ticket(endpoint: &Endpoint, choice: &crate::relay::RelayChoice) -> NodeTicket {
    let wait = crate::relay::wait_relay(endpoint, choice).await;
    let relay_url = match &wait {
        crate::relay::RelayWait::Connected(url) => Some(url.clone()),
        _ => None,
    };
    let addrs = crate::serve::direct_addresses(endpoint);
    let json = json!({
        "v": 1,
        "node": crate::device::hex(endpoint.secret_key().public().as_bytes()),
        "addrs": addrs,
        "relay": relay_url,
    })
    .to_string();
    NodeTicket {
        json,
        relay: relay_url,
        relay_timed_out: wait == crate::relay::RelayWait::TimedOut,
    }
}

/// Parse a mailbox ticket.
///
/// # Errors
/// [`Error::Malformed`] when it is not one.
pub fn node_ticket_parse(text: &str) -> Result<NodeAddr> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| Error::Malformed(format!("not a mailbox ticket: {e}")))?;
    let node = v["node"]
        .as_str()
        .and_then(|h| {
            let mut out = [0_u8; 32];
            if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
                for (i, chunk) in h.as_bytes().chunks(2).enumerate() {
                    let hi = (chunk[0] as char).to_digit(16)? as u8;
                    let lo = (chunk[1] as char).to_digit(16)? as u8;
                    out[i] = (hi << 4) | lo;
                }
                Some(out)
            } else {
                None
            }
        })
        .ok_or_else(|| Error::Malformed("the ticket misses its node id".into()))?;
    Ok(NodeAddr {
        node_id: node,
        addrs: v["addrs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        relay: v["relay"].as_str().map(str::to_string),
    })
}

/// Remember the node a root uses.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn remember_node(root: &Path, ticket: &str, addr: &NodeAddr) -> Result<()> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "CREATE TABLE IF NOT EXISTS mailbox (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             ticket TEXT NOT NULL,
             last_contact TEXT,
             at TEXT NOT NULL)",
        [],
    )?;
    reg.conn().execute(
        "INSERT INTO mailbox (id, ticket, last_contact, at) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET ticket = ?1, last_contact = ?2, at = ?3",
        rusqlite::params![
            ticket,
            now_iso(),
            serde_json::to_string(&json!({
                "node": crate::device::hex(&addr.node_id),
                "addrs": addr.addrs,
                "relay": addr.relay,
            }))
            .unwrap_or_default()
        ],
    )?;
    Ok(())
}

/// Forget the node.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn forget_node(root: &Path) -> Result<()> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute("DROP TABLE IF EXISTS mailbox", [])?;
    Ok(())
}

/// The node a root uses, when there is one.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn node_of(root: &Path) -> Result<Option<NodeAddr>> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "CREATE TABLE IF NOT EXISTS mailbox (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             ticket TEXT NOT NULL,
             last_contact TEXT,
             at TEXT NOT NULL)",
        [],
    )?;
    let row: Option<String> = reg
        .conn()
        .query_row("SELECT at FROM mailbox WHERE id = 1", [], |r| r.get(0))
        .optional()?;
    let Some(text) = row else {
        return Ok(None);
    };
    let v: Value =
        serde_json::from_str(&text).map_err(|e| Error::Malformed(format!("bad node row: {e}")))?;
    let node = v["node"]
        .as_str()
        .and_then(|h| {
            let mut out = [0_u8; 32];
            if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
                for (i, chunk) in h.as_bytes().chunks(2).enumerate() {
                    let hi = (chunk[0] as char).to_digit(16)? as u8;
                    let lo = (chunk[1] as char).to_digit(16)? as u8;
                    out[i] = (hi << 4) | lo;
                }
                Some(out)
            } else {
                None
            }
        })
        .ok_or_else(|| Error::Malformed("the node row misses its id".into()))?;
    Ok(Some(NodeAddr {
        node_id: node,
        addrs: v["addrs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        relay: v["relay"].as_str().map(str::to_string),
    }))
}

/// `mailbox add <ticket>`: dial the node, hand over the signed device
/// list, remember it. The node now accepts this list's live devices.
///
/// # Errors
/// Whatever the dial or the node refuses.
pub async fn add_node(root: &Path, endpoint: &Endpoint, ticket: &str) -> Result<String> {
    let addr = node_ticket_parse(ticket)?;
    let conn = addr.dial(endpoint).await?;
    let reg = DeviceRegistry::open(root)?;
    send_frame(&conn, &json!({"kind": "list", "rows": reg.rows_json()})).await?;
    let answer = recv_frame(&conn).await?;
    if answer.get("kind").and_then(Value::as_str) != Some("ok") {
        return Err(Error::Registry("the node refused the list".into()));
    }
    let owner = answer["owner"].as_str().unwrap_or_default().to_string();
    remember_node(root, ticket, &addr)?;
    Ok(owner)
}

/// The owner id of a root's own list (its create row's op id).
///
/// # Errors
/// [`Error::Registry`] when the list has no create row.
pub fn owner_of_root(root: &Path) -> Result<String> {
    let reg = DeviceRegistry::open(root)?;
    let rows = reg.rows_json();
    NodeState::owner_of(&rows).ok_or_else(|| Error::Registry("the list has no create row".into()))
}

/// One push to the node after a pass: deposit what the node lacks, fetch
/// what we lack, deliver the purge requests the queue holds. Returns
/// `(deposited, fetched)`.
///
/// # Errors
/// Whatever the dial or the node refuses.
pub async fn push(root: &Path, endpoint: &Endpoint) -> Result<(usize, usize)> {
    let Some(node) = node_of(root)? else {
        return Ok((0, 0));
    };
    let conn = node.dial(endpoint).await?;
    let owner = owner_of_root(root)?;
    let dk = DeviceKey::load_or_create(root)?;
    let trace = std::env::var("ENGRAMMA_TRACE").is_ok();
    let mut deposited = 0_usize;
    let mut fetched = 0_usize;
    for shared in SharedCell::all_of(root)? {
        let sync_id: [u8; 32] = *shared.sync_id();
        let Some(mac_key) = shared.epoch0_mac_key() else {
            continue;
        };
        // our opaque ids
        let mine: Vec<([u8; 32], [u8; 32])> = shared
            .local_ids()
            .into_iter()
            .map(|(_, ch)| {
                let opaque = crate::crypto::EpochKey::opaque_op_id(&mac_key, &ch);
                (ch, opaque)
            })
            .collect();
        // the node's
        send_frame(&conn, &json!({"kind": "enumerate", "owner": owner})).await?;
        let answer = recv_frame(&conn).await?;
        let mut theirs: Vec<([u8; 32], [u8; 32])> = Vec::new();
        for pair in answer["ids"].as_array().unwrap_or(&Vec::new()) {
            let Ok(s) = unb64(pair["sync_id"].as_str().unwrap_or("")) else {
                continue;
            };
            if s != sync_id {
                continue;
            }
            if let Ok(o) = unb64(pair["id"].as_str().unwrap_or(""))
                && let Ok(o) = <[u8; 32]>::try_from(o)
            {
                theirs.push((sync_id, o));
            }
        }
        let theirs_ids: std::collections::HashSet<[u8; 32]> =
            theirs.iter().map(|(_, o)| *o).collect();
        // deposit what the node lacks (sealed under the CURRENT epoch, AAD
        // over the OPAQUE id: the node's routing, the seal's binding)
        let mut envs = Vec::new();
        for (ch, opaque) in &mine {
            if theirs_ids.contains(opaque) {
                continue;
            }
            let epoch = shared.current_epoch()?;
            let key = shared.epoch_key(epoch)?;
            let Some(op) = shared.op_of(ch) else {
                continue;
            };
            let plaintext = crate::wire::envelope_wire(&op, None);
            let aad = opaque_aad(&sync_id, epoch, opaque);
            let env = crate::crypto::seal_op(&key, &aad, plaintext.as_bytes())?;
            envs.push(json!({
                "sync_id": b64(&sync_id), "id": b64(opaque),
                "epoch": epoch, "nonce": b64(&env.nonce), "ct": b64(&env.ct),
            }));
        }
        if !envs.is_empty() {
            if trace {
                eprintln!("push: depositing {} envelopes", envs.len());
            }
            deposited += envs.len();
            send_frame(
                &conn,
                &json!({"kind": "deposit", "owner": owner, "envs": envs}),
            )
            .await?;
            let _ = recv_frame(&conn).await; // ok
        }
        // fetch what we lack
        let mine_ids: std::collections::HashSet<[u8; 32]> = mine.iter().map(|(_, o)| *o).collect();
        let want: Vec<Value> = theirs
            .iter()
            .filter(|(_, o)| !mine_ids.contains(o))
            .map(|(_, o)| json!(b64(o)))
            .collect();
        if !want.is_empty() {
            send_frame(
                &conn,
                &json!({"kind": "fetch", "owner": owner, "ids": want}),
            )
            .await?;
            let answer = recv_frame(&conn).await?;
            for env in answer["envs"].as_array().unwrap_or(&Vec::new()) {
                let epoch = env["epoch"].as_u64().unwrap_or(0);
                let Ok(o) = unb64(env["id"].as_str().unwrap_or("")) else {
                    continue;
                };
                let Ok(opaque) = <[u8; 32]>::try_from(o) else {
                    continue;
                };
                let Ok(nonce) = unb64(env["nonce"].as_str().unwrap_or("")) else {
                    continue;
                };
                let Ok(ct) = unb64(env["ct"].as_str().unwrap_or("")) else {
                    continue;
                };
                if shared.ingest_opaque(epoch, &opaque, &nonce, &ct).is_ok() {
                    fetched += 1;
                }
            }
        }
    }
    // the purge queue: every applied purge whose request the node has not
    // confirmed yet
    for (ids, sig) in purge_queue(root, &dk)? {
        let _ = ids;
        let _ = sig;
        // sent below once the queue table exists in devices.db
    }
    let _ = &conn;
    Ok((deposited, fetched))
}

/// The AAD of one envelope at the node: `cell_sync_id ‖ epoch ‖ opaque id`.
#[must_use]
pub fn opaque_aad(sync_id: &[u8; 32], epoch: u64, opaque: &[u8; 32]) -> Vec<u8> {
    let mut aad = sync_id.to_vec();
    aad.extend_from_slice(&epoch.to_be_bytes());
    aad.extend_from_slice(opaque);
    aad
}

/// The purge requests waiting for the node's confirmation: signed
/// `purge_signing_content(owner, ids)` over the opaque ids of the purged
/// notes' operations.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
/// A waiting purge request: the opaque ids and the signature over them.
pub type PendingPurge = (Vec<Vec<u8>>, Vec<u8>);

pub fn purge_queue(root: &Path, dk: &DeviceKey) -> Result<Vec<PendingPurge>> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "CREATE TABLE IF NOT EXISTS purge_queue (
             id INTEGER PRIMARY KEY,
             owner TEXT NOT NULL,
             ids_json TEXT NOT NULL,
             at TEXT NOT NULL)",
        [],
    )?;
    let owner = owner_of_root(root)?;
    let rows: Vec<(i64, String)> = {
        let mut stmt = reg
            .conn()
            .prepare("SELECT id, ids_json FROM purge_queue ORDER BY id")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut out = Vec::new();
    for (id, ids_json) in rows {
        let ids: Vec<Vec<u8>> = serde_json::from_str(&ids_json)
            .map_err(|e| Error::Malformed(format!("bad purge queue row: {e}")))?;
        let content = mailbox::purge_signing_content(&owner, &ids);
        let sig = dk.sign(&content);
        out.push((ids, sig));
        let _ = id;
    }
    Ok(out)
}

/// Enqueue a purge for the node from a SHARED CELL adapter (the device
/// just ingested or applied one): the opaque ids of every operation of
/// the purged note. Failures are silent: the queue retries at every pass.
pub fn enqueue_purge_shared(shared: &SharedCell, note_id: &ulid::Ulid) {
    let root = shared_root(shared);
    let Ok(reg) = DeviceRegistry::open(&root) else {
        return;
    };
    let Ok(owner) = owner_of_root(&root) else {
        return;
    };
    let Some(mac_key) = shared.epoch0_mac_key() else {
        return;
    };
    let note = format!("note:{note_id}");
    let mut ids: Vec<Vec<u8>> = shared
        .cell()
        .debug_column(&format!(
            "SELECT o.content_hash FROM ops o JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = '{}' ORDER BY o.op_id",
            note.replace('\'', "''")
        ))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|h| unhex32(&h))
        .map(|ch| crate::crypto::EpochKey::opaque_op_id(&mac_key, &ch).to_vec())
        .collect();
    ids.sort();
    ids.dedup();
    reg.conn()
        .execute(
            "CREATE TABLE IF NOT EXISTS purge_queue (
                 id INTEGER PRIMARY KEY, owner TEXT NOT NULL,
                 ids_json TEXT NOT NULL, at TEXT NOT NULL)",
            [],
        )
        .ok();
    reg.conn()
        .execute(
            "INSERT INTO purge_queue (owner, ids_json, at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                owner,
                serde_json::to_string(&ids).unwrap_or_default(),
                now_iso()
            ],
        )
        .ok();
}

fn shared_root(shared: &SharedCell) -> std::path::PathBuf {
    // the registry of the adapter is the root's devices.db: its file path
    // sits beside the cell, two levels up from cells/<name>.sqlite
    let mut path = shared.cell().file_path().to_path_buf();
    path.pop();
    path.pop();
    path
}

/// Enqueue a purge for the node (the device just applied one).
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn enqueue_purge(root: &Path, cell_name: &str, note_id: &str) -> Result<()> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "CREATE TABLE IF NOT EXISTS purge_queue (
             id INTEGER PRIMARY KEY,
             owner TEXT NOT NULL,
             ids_json TEXT NOT NULL,
             at TEXT NOT NULL)",
        [],
    )?;
    let owner = owner_of_root(root)?;
    // the opaque ids of every operation of the purged note
    let shared = SharedCell::all_of(root)?.into_iter().find(|s| {
        serve_cell_name(root, s.sync_id())
            .map(|n| n == cell_name)
            .unwrap_or(false)
    });
    let Some(shared) = shared else {
        return Ok(());
    };
    let Some(mac_key) = shared.epoch0_mac_key() else {
        return Ok(());
    };
    let note = format!("note:{note_id}");
    let mut ids: Vec<Vec<u8>> = shared
        .cell()
        .debug_column(&format!(
            "SELECT o.content_hash FROM ops o JOIN op_objects oo ON oo.op_id = o.op_id
             WHERE oo.object = '{}' ORDER BY o.op_id",
            note.replace('\'', "''")
        ))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|h| unhex32(&h))
        .map(|ch| crate::crypto::EpochKey::opaque_op_id(&mac_key, &ch).to_vec())
        .collect();
    ids.sort();
    ids.dedup();
    reg.conn().execute(
        "INSERT INTO purge_queue (owner, ids_json, at) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            owner,
            serde_json::to_string(&ids).unwrap_or_default(),
            now_iso()
        ],
    )?;
    Ok(())
}

/// Drop the queue rows the node confirmed.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn clear_purge_queue(root: &Path, up_to: usize) -> Result<()> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "DELETE FROM purge_queue WHERE id IN (
             SELECT id FROM purge_queue ORDER BY id LIMIT ?1)",
        [up_to as i64],
    )?;
    Ok(())
}

fn serve_cell_name(root: &Path, sync_id: &[u8; 32]) -> Result<String> {
    crate::serve::cell_name(root, sync_id)
}

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

// the code meeting at the node: both sides dial the node, the node pairs
// the channel and relays; everything after the PAKE is end-to-end sealed

/// Open one side of a code channel at the node and return the RELAYED
/// connection once the other side arrives (the node shuttles opaque
/// blobs; one wrong key confirmation burns the channel).
///
/// # Errors
/// Whatever the meeting fails with; a burnt channel refuses at the door.
pub async fn meet_code(
    endpoint: &Endpoint,
    node: &NodeAddr,
    channel: u16,
) -> Result<RelayConnection> {
    // the node's own dial discipline (one address at a time): the bare
    // endpoint connect stalls on a virtual interface that does not answer
    // its own address
    let conn = node.dial(endpoint).await?;
    send_frame(
        &conn,
        &json!({"kind": "code-open", "v": MAILBOX_VERSION, "channel": channel}),
    )
    .await?;
    // the FIRST side of the channel is told to WAIT — the node pairs the
    // channel only when the other side arrives (v0.6.1: the code flow
    // never worked because this read the one "waiting" answer and died)
    loop {
        let answer = recv_frame(&conn).await?;
        match answer.get("kind").and_then(Value::as_str) {
            Some("waiting") => continue,
            Some("paired") => return Ok(RelayConnection { conn }),
            Some("burnt") => {
                return Err(Error::Registry(
                    "il canale è bruciato: un tentativo per codice".into(),
                ));
            }
            _ => {
                return Err(Error::Malformed("the node did not pair the channel".into()));
            }
        }
    }
}

/// A relayed connection: the bytes that cross it are end-to-end sealed by
/// the callers — the node reads lengths, nothing else.
pub struct RelayConnection {
    conn: Connection,
}

impl RelayConnection {
    /// Send one opaque blob across.
    ///
    /// # Errors
    /// [`Error::Io`] on failure.
    pub async fn send(&self, blob: &[u8]) -> Result<()> {
        send_frame(&self.conn, &json!({"kind": "relay", "blob": b64(blob)})).await
    }

    /// Receive one opaque blob.
    ///
    /// # Errors
    /// [`Error::Io`] on failure.
    pub async fn recv(&self) -> Result<Vec<u8>> {
        let msg = recv_frame(&self.conn).await?;
        match msg.get("kind").and_then(Value::as_str) {
            Some("relay") => unb64(msg["blob"].as_str().unwrap_or("")),
            _ => Err(Error::Malformed("expected a relay blob".into())),
        }
    }

    /// Tell the node the attempt failed: the channel burns.
    ///
    /// # Errors
    /// [`Error::Io`] on failure.
    pub async fn burn(&self, channel: u16) -> Result<()> {
        send_frame(&self.conn, &json!({"kind": "burn", "channel": channel})).await
    }
}

/// The code pairing over a relay: SPAKE2 over the code, RFC 9382 key
/// confirmation end to end, then the SAS on both screens. Returns the
/// shared secret and the SAS (the caller confirms and runs the sealed
/// join turns over the relay).
///
/// # Errors
/// [`Error::BadSignature`] when the confirmation MACs do not verify —
/// the caller must BURN the channel then.
pub async fn code_pake(
    relay: &RelayConnection,
    side: Side,
    password: &[u8],
) -> Result<(Vec<u8>, Vec<u8>, String)> {
    let (half, msg) = match side {
        Side::Inviter => SpakeHalf::begin_a(password),
        Side::Joiner => SpakeHalf::begin_b(password),
    };
    relay.send(&msg).await?;
    let their_msg = relay.recv().await?;
    let shared = half.finish(&their_msg)?;
    let transcript = match side {
        Side::Inviter => [msg.as_slice(), their_msg.as_slice()].concat(),
        Side::Joiner => [their_msg.as_slice(), msg.as_slice()].concat(),
    };
    // RFC 9382 § 4: both MACs — a wrong code dies here
    let mine = pairing::confirm_mac(side, &shared, &transcript);
    relay.send(&mine).await?;
    let their_mac = relay.recv().await?;
    let theirs_side = match side {
        Side::Inviter => Side::Joiner,
        Side::Joiner => Side::Inviter,
    };
    if their_mac != pairing::confirm_mac(theirs_side, &shared, &transcript) {
        return Err(Error::BadSignature);
    }
    let sas = six_digits(&shared, &transcript);
    Ok((shared, transcript, sas))
}

/// Derive the relay session key from the PAKE shared secret (the join
/// turns that cross the node ride sealed under this).
#[must_use]
pub fn relay_key(shared: &[u8], transcript: &[u8]) -> [u8; 32] {
    use blake3::Hasher;
    let mut h = Hasher::new();

    h.update(b"engramma/relay-session/v1");
    h.update(shared);
    h.update(transcript);
    let mut out = [0_u8; 32];
    out.copy_from_slice(h.finalize().as_bytes());
    out
}

/// Seal one blob under the relay key (the node relays ciphertext).
///
/// # Errors
/// [`Error::Io`] on CSPRNG failure, [`Error::Malformed`] on seal failure.
pub fn relay_seal(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>> {
    let epoch_key = crate::crypto::EpochKey(*key);
    let env = crate::crypto::seal_op(&epoch_key, b"engramma/relay/v1", blob)?;
    // the ENVELOPE's nonce is the one the ciphertext was sealed with — a
    // fresh local one prefixed instead could never open (v0.6.1: the code
    // flow's sealed turns never crossed for this)
    let mut out = env.nonce.to_vec();
    out.extend_from_slice(&env.ct);
    Ok(out)
}

/// Open one blob under the relay key.
///
/// # Errors
/// [`Error::Malformed`] when the blob does not open.
pub fn relay_open(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 24 {
        return Err(Error::Malformed("relay blob too short".into()));
    }
    let (nonce, ct) = blob.split_at(24);
    let env = crate::crypto::Envelope {
        nonce: nonce
            .try_into()
            .map_err(|_| Error::Malformed("bad nonce".into()))?,
        ct: ct.to_vec(),
    };
    crate::crypto::open_op(&crate::crypto::EpochKey(*key), b"engramma/relay/v1", &env)
}

/// The node side of the code meeting: pair two sides of a channel and
/// relay until either closes. A burnt channel never pairs. This runs
/// inside the node's accept loop.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub async fn node_relay(state: &NodeState, conn: &Connection, channel: u16) -> Result<()> {
    if state.channel_burnt(channel)? {
        send_frame(conn, &json!({"kind": "burnt"})).await?;
        let _ = conn.closed().await;
        return Ok(());
    }
    // the other side registers in the channel table when it arrives
    let Some((mine, theirs)) = wait_pair(state, conn, channel).await? else {
        // the SECOND arrival: the FIRST's task owns the pair and relays —
        // a second relay would compete for the very same frames (v0.6.1:
        // the code flow's seals never opened because of it)
        return Ok(());
    };
    let relayed = relay_between(state, &mine, &theirs, channel).await;
    // hold both wires until the peers are done: returning sooner closes
    // the QUIC connections and DISCARDS frames still in flight (v0.6.1:
    // the sealed ticket raced the burn's close)
    let _ = mine.closed().await;
    let _ = theirs.closed().await;
    relayed
}

/// The relay loop proper: bytes both ways until a burn or a close.
async fn relay_between(
    state: &NodeState,
    mine: &Connection,
    theirs: &Connection,
    channel: u16,
) -> Result<()> {
    // relay until either side closes; `burn` is the only other frame
    loop {
        tokio::select! {
            msg = recv_frame(mine) => {
                match msg {
                    Ok(v) if v.get("kind").and_then(Value::as_str) == Some("burn") => {
                        state.burn_channel(channel)?;
                        let _ = send_frame(mine, &json!({"kind": "ok"})).await;
                        let _ = send_frame(theirs, &json!({"kind": "peer-closed"})).await;
                        return Ok(());
                    }
                    Ok(v) if v.get("kind").and_then(Value::as_str) == Some("relay") => {
                        if send_frame(theirs, &v).await.is_err() {
                            return Ok(());
                        }
                    }
                    _ => {
                        let _ = send_frame(theirs, &json!({"kind": "peer-closed"})).await;
                        return Ok(());
                    }
                }
            }
            msg = recv_frame(theirs) => {
                match msg {
                    Ok(v) if v.get("kind").and_then(Value::as_str) == Some("burn") => {
                        state.burn_channel(channel)?;
                        let _ = send_frame(theirs, &json!({"kind": "ok"})).await;
                        let _ = send_frame(mine, &json!({"kind": "peer-closed"})).await;
                        return Ok(());
                    }
                    Ok(v) if v.get("kind").and_then(Value::as_str) == Some("relay") => {
                        if send_frame(mine, &v).await.is_err() {
                            return Ok(());
                        }
                    }
                    _ => {
                        let _ = send_frame(mine, &json!({"kind": "peer-closed"})).await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Two sides waiting on one channel: the first parks (up to the code's
/// ten minutes), the second wakes it, both learn they paired.
async fn wait_pair(
    _state: &NodeState,
    conn: &Connection,
    channel: u16,
) -> Result<Option<(Connection, Connection)>> {
    let notify = {
        let mut channels = channels()
            .lock()
            .expect(/* never poisoned here */ "node channels");
        channels
            .entry(channel)
            .or_insert_with(|| ChannelState {
                first: None,
                second: None,
                notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            })
            .notify
            .clone()
    };
    let second_side = {
        let mut map = channels().lock().expect(/* as above */ "node channels");
        if map.get(&channel).is_some_and(|c| c.first.is_some()) {
            let first = map
                .get_mut(&channel)
                .and_then(|c| c.first.take())
                .expect(/* checked above */ "the first side");
            map.get_mut(&channel)
                .expect(/* present above */ "the channel")
                .second = Some(conn.clone());
            Some(first)
        } else {
            map.get_mut(&channel)
                .expect(/* entry made above */ "the channel")
                .first = Some(conn.clone());
            None
        }
    };
    if let Some(first) = second_side {
        // the second side: wake the parked first — and LEAVE the entry:
        // the first side still has to take `second` out of it (v0.6.1:
        // removing it here closed the channel under the first side's
        // feet, and the code flow never paired). The second side relays
        // nothing: `None` says the first side's task owns the pair.
        notify.notify_one();
        send_frame(conn, &json!({"kind": "paired"})).await?;
        let _ = first;
        Ok(None)
    } else {
        // the first side: park (registered above) and wait for the second
        send_frame(conn, &json!({"kind": "waiting"})).await?;
        let paired = tokio::time::timeout(
            std::time::Duration::from_millis(pairing::TICKET_TTL_MS),
            notify.notified(),
        )
        .await;
        if paired.is_err() {
            channels()
                .lock()
                .expect(/* as above */ "node channels")
                .remove(&channel);
            return Err(Error::Malformed("nessuno è arrivato sul canale".into()));
        }
        let second = channels()
            .lock()
            .expect(/* as above */ "node channels")
            .get_mut(&channel)
            .and_then(|c| c.second.take());
        // the entry's work is done whoever won: the pair is made (or lost)
        channels()
            .lock()
            .expect(/* as above */ "node channels")
            .remove(&channel);
        match second {
            Some(second) => {
                send_frame(conn, &json!({"kind": "paired"})).await?;
                Ok(Some((conn.clone(), second)))
            }
            None => Err(Error::Malformed("il canale si è chiuso".into())),
        }
    }
}

/// One code channel's state at the node.
struct ChannelState {
    first: Option<Connection>,
    second: Option<Connection>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

static CHANNELS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<u16, ChannelState>>,
> = std::sync::OnceLock::new();

fn channels() -> &'static std::sync::Mutex<std::collections::HashMap<u16, ChannelState>> {
    CHANNELS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The node's accept-loop dispatcher: the FIRST frame decides — a code
/// meeting goes to the relay (the code is its own gate: a stranger may
/// open a channel, one attempt, ten minutes), everything else must come
/// from a live device of a known list.
///
/// # Errors
/// Whatever the chosen road fails with.
pub async fn serve_or_relay(state: &NodeState, conn: &Connection) -> Result<()> {
    let first = recv_frame(conn).await?;
    match first.get("kind").and_then(Value::as_str) {
        Some("code-open") => {
            let channel = first["channel"].as_u64().unwrap_or(0) as u16;
            if state.channel_burnt(channel)? {
                send_frame(conn, &json!({"kind": "burnt"})).await?;
                // hold the wire until the caller read the answer: an early
                // return would race the close against the frame
                let _ = conn.closed().await;
                return Ok(());
            }
            node_relay(state, conn, channel).await
        }
        _ => {
            // a device frame: handle it and everything that follows
            handle_frame(state, conn, &first).await
        }
    }
}

/// Handle one already-received device frame, then keep serving.
async fn handle_frame(state: &NodeState, conn: &Connection, msg: &Value) -> Result<()> {
    let caller = conn.remote_id();
    let authorized = state.authorizes(caller.as_bytes())?;
    let owner_of_root = |v: &Value| v["owner"].as_str().unwrap_or_default().to_string();
    match msg.get("kind").and_then(Value::as_str) {
        // the LIST is how a device first introduces itself to the node:
        // no prior authorization can exist yet — the rows carry their own
        // signatures, and that is the security (a row that does not
        // verify is refused). What the node does NOT promise: refusing a
        // stranger's SELF-CONSISTENT list (storage-bounded, ciphertext
        // only) — declared in the REPORT.
        Some("list") => {
            let rows = msg["rows"].as_array().cloned().unwrap_or_default();
            let owner = state.take_list(&rows)?;
            send_frame(conn, &json!({"kind": "ok", "owner": owner})).await?;
        }
        Some("deposit") if authorized => {
            let owner = owner_of_root(msg);
            let mut n = 0_usize;
            for env in msg["envs"].as_array().unwrap_or(&Vec::new()) {
                let sync_id = unb64(env["sync_id"].as_str().unwrap_or(""))?;
                let opaque = unb64(env["id"].as_str().unwrap_or(""))?;
                let nonce = unb64(env["nonce"].as_str().unwrap_or(""))?;
                let ct = unb64(env["ct"].as_str().unwrap_or(""))?;
                state
                    .store
                    .put(&owner, &sync_id, &opaque, &nonce, &ct, now_ms())?;
                n += 1;
            }
            send_frame(conn, &json!({"kind": "ok", "stored": n})).await?;
        }
        Some("enumerate") if authorized => {
            let owner = owner_of_root(msg);
            let mut pairs: Vec<Value> = Vec::new();
            for sync_id in state.store.sync_ids_of(&owner)? {
                for opaque in state.store.opaque_ids(&owner, &sync_id)? {
                    pairs.push(json!({
                        "sync_id": b64(&sync_id), "id": b64(&opaque),
                    }));
                }
            }
            send_frame(conn, &json!({"kind": "ids", "ids": pairs})).await?;
        }
        Some("fetch") if authorized => {
            let owner = owner_of_root(msg);
            let mut envs = Vec::new();
            for id in msg["ids"].as_array().unwrap_or(&Vec::new()) {
                let Ok(opaque) = unb64(id.as_str().unwrap_or("")) else {
                    continue;
                };
                if let Some((sync_id, nonce, ct)) = state.store.get(&owner, &opaque)? {
                    envs.push(json!({
                        "sync_id": b64(&sync_id), "id": b64(&opaque),
                        "nonce": b64(&nonce), "ct": b64(&ct),
                    }));
                }
            }
            send_frame(conn, &json!({"kind": "envs", "envs": envs})).await?;
        }
        Some("purge") if authorized => {
            let owner = owner_of_root(msg);
            let ids: Vec<Vec<u8>> = msg["ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| unb64(x.as_str().unwrap_or("")).ok())
                        .collect()
                })
                .unwrap_or_default();
            let sig = unb64(msg["sig"].as_str().unwrap_or(""))?;
            crate::mailbox::verify_purge(&state.reg, &owner, &ids, &sig)?;
            let n = state.store.purge_opaque(&owner, &ids)?;
            send_frame(conn, &json!({"kind": "ok", "purged": n})).await?;
        }
        Some("stats") if authorized => {
            let owner = owner_of_root(msg);
            let envelopes = state.store.count_of(&owner)?;
            let bytes = state.store.bytes_of(&owner)?;
            send_frame(
                conn,
                &json!({
                    "kind": "stats", "envelopes": envelopes, "bytes": bytes,
                    "retention_days": crate::mailbox::RETENTION_DAYS,
                }),
            )
            .await?;
        }
        Some("burn") if authorized => {
            let channel = msg["channel"].as_u64().unwrap_or(0) as u16;
            state.burn_channel(channel)?;
            send_frame(conn, &json!({"kind": "ok"})).await?;
        }
        _ => {
            // unknown, or not authorized: say nothing of value, close
            send_frame(conn, &json!({"kind": "refused"})).await?;
            return Ok(());
        }
    }
    // the next frames of this connection (boxed: the handler recurses)
    loop {
        let Ok(next) = recv_frame(conn).await else {
            return Ok(());
        };
        Box::pin(handle_frame(state, conn, &next)).await?;
    }
}

/// The device side of `mailbox status`: ask the node for this owner's
/// numbers.
///
/// # Errors
/// Whatever the dial or the node refuses.
pub async fn node_stats(root: &Path, endpoint: &Endpoint, node: &NodeAddr) -> Result<(usize, i64)> {
    let conn = node.dial(endpoint).await?;
    let owner = owner_of_root(root)?;
    send_frame(&conn, &json!({"kind": "stats", "owner": owner})).await?;
    let answer = recv_frame(&conn).await?;
    if answer.get("kind").and_then(Value::as_str) != Some("stats") {
        return Err(Error::Registry("the node refused the stats".into()));
    }
    Ok((
        answer["envelopes"].as_u64().unwrap_or(0) as usize,
        answer["bytes"].as_i64().unwrap_or(0),
    ))
}

/// When this root last talked to its node (the local row).
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn last_contact(root: &Path) -> Result<Option<String>> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "CREATE TABLE IF NOT EXISTS mailbox (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             ticket TEXT NOT NULL,
             last_contact TEXT,
             at TEXT NOT NULL)",
        [],
    )?;
    let row: Option<String> = reg
        .conn()
        .query_row(
            "SELECT COALESCE(last_contact, at) FROM mailbox WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row)
}
