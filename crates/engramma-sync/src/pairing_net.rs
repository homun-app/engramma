// SPDX-License-Identifier: Apache-2.0

//! The pairing OVER THE WIRE (v0.6.1, P2): the invite flow on iroh, ALPN
//! `engramma/pairing/1`. The transport-free protocol is [`crate::pairing`];
//! this module is its turns on a real connection:
//!
//! 1. SPAKE2 over the TICKET SECRET as the password — the secret never
//!    crosses in the clear, and a man in the middle relaying two
//!    connections cannot produce the transcript MAC;
//! 2. the inviter's MAC of the secret over the transcript (the joiner
//!    authenticates WHO invited);
//! 3. RFC 9382 § 4 key confirmation, both MACs — a wrong password is
//!    discovered here, without asking the human anything;
//! 4. the SIX-DIGIT SAS on both screens, confirmed by a human on each
//!    side — only after both confirmations does the `add` land, the
//!    registry and the wrapped epoch keys travel, and the shared cells
//!    stream over as sealed operations (the new device ingests them
//!    through the same arrival gate as any session).
//!
//! Every refusal is silent about note text: the errors say which turn
//! failed, nothing else.

use engramma_core::Cell;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use serde_json::{Value, json};

use crate::device::DeviceKey;
use crate::keystore::KeyStore;
use crate::pairing::{self, Side, SpakeHalf, Ticket, six_digits, transcript_mac};
use crate::protocol::SyncedCell;
use crate::registry::DeviceRegistry;
use crate::sharedcell::SharedCell;
use crate::{Error, Result};

/// The pairing protocol's ALPN.
pub const PAIRING_ALPN: &[u8] = b"engramma/pairing/1";
/// The pairing protocol's version.
pub const PAIRING_VERSION: u64 = 1;

fn b64(data: &[u8]) -> String {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::encode_string(data)
}

fn unb64(text: &str) -> Result<Vec<u8>> {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::decode_vec(text).map_err(|e| Error::Malformed(format!("bad base64: {e}")))
}

/// One frame on one stream: write, FINISH (the transport flushes the data
/// with the FIN — a write without it may sit in the buffer), and on the
/// receiving side drain to EOF so the stream retires and the next
/// `open_bi` never waits on a stale flow-control budget.
async fn send_frame(conn: &Connection, msg: &Value) -> Result<()> {
    let bytes = msg.to_string();
    let body = bytes.as_bytes();
    if body.len() > crate::protocol::MAX_MESSAGE {
        return Err(Error::Malformed("message too large".into()));
    }
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    let (mut send, own_recv) = conn.open_bi().await.map_err(io_err)?;
    send.write_all(&frame)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    send.finish()
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    drop(own_recv);
    Ok(())
}

async fn recv_frame(conn: &Connection) -> Result<Value> {
    let (_send, mut recv) = conn.accept_bi().await.map_err(io_err)?;
    let mut len_buf = [0_u8; 4];
    recv.read_exact(&mut len_buf).await.map_err(io_err)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > crate::protocol::MAX_MESSAGE {
        return Err(Error::Malformed("message too large".into()));
    }
    let mut buf = vec![0_u8; len];
    recv.read_exact(&mut buf).await.map_err(io_err)?;
    // drain the FIN: a stream the peer never saw closed keeps the
    // open-stream budget frozen — the fifth frame of an exchange would
    // never arrive
    let _ = recv.read_to_end(1).await;
    serde_json::from_slice(&buf).map_err(|e| Error::Malformed(format!("bad frame: {e}")))
}

fn io_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> Error {
    Error::Io(std::io::Error::other(e))
}

/// Bind the pairing endpoint of one root: the one relay-aware builder of
/// the sync road (R2) — see [`crate::relay`].
///
/// # Errors
/// [`Error::Io`] when iroh cannot bind.
pub async fn bind_pairing(
    key: &DeviceKey,
    bind: &str,
    choice: &crate::relay::RelayChoice,
) -> Result<Endpoint> {
    crate::relay::bind(key, bind, PAIRING_ALPN, choice).await
}

/// What minting a ticket for someone else found out about the relay
/// (C1, v0.6.1).
#[derive(Debug)]
pub struct TicketOf {
    /// The ticket.
    pub ticket: Ticket,
    /// The relay URL the ticket carries, when there is one.
    pub relay: Option<String>,
    /// The relay wait ran out: the ticket carries NO relay and the
    /// caller must say so — never in silence.
    pub relay_timed_out: bool,
}

/// The ticket an inviter mints from its LIVE endpoint: node id, direct
/// addresses, and the RELAY the endpoint actually rides (R2) — the own
/// relay's URL when `--relay` chose one, one of n0's when the default
/// rules, none with `--no-relay`. The endpoint knows which: its address
/// carries the relay it is connected to.
///
/// **The wait comes first** (C1): with a relay configured the endpoint
/// connects to it asynchronously after the bind, and a ticket minted
/// before carries no relay — whoever is behind a NAT finds nobody. The
/// wait is bounded ([`crate::relay::RELAY_WAIT`]); when it runs out the
/// ticket goes out WITHOUT the relay and `relay_timed_out` says why.
pub async fn ticket_of(
    endpoint: &Endpoint,
    choice: &crate::relay::RelayChoice,
) -> Result<TicketOf> {
    let wait = crate::relay::wait_relay(endpoint, choice).await;
    let relay = match &wait {
        crate::relay::RelayWait::Connected(url) => url.clone(),
        _ => String::new(),
    };
    let addrs = crate::serve::direct_addresses(endpoint);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let ticket = Ticket::mint(
        endpoint
            .secret_key()
            .public()
            .as_bytes()
            .to_vec()
            .try_into()
            .unwrap_or([0; 32]),
        &relay,
        &addrs,
        now,
    )?;
    Ok(TicketOf {
        relay: (!relay.is_empty()).then_some(relay),
        relay_timed_out: wait == crate::relay::RelayWait::TimedOut,
        ticket,
    })
}

/// What a completed join did, for the CLI's summary.
#[derive(Debug, Default)]
pub struct JoinReport {
    /// The cells that arrived, with their operation counts.
    pub cells: Vec<(String, usize)>,
    /// The registry rows applied.
    pub registry_rows: usize,
}

/// The INVITER's side: wait for the one device that enters with this
/// ticket, run the turns, and — after BOTH confirmations — sign the `add`,
/// hand over the registry, the wrapped epoch keys and the shared cells.
/// `confirm` shows the SAS and answers whether THIS side confirms.
///
/// # Errors
/// Whatever turn fails; nothing about any note.
pub async fn invite(
    endpoint: &Endpoint,
    registry: &DeviceRegistry,
    root: &std::path::Path,
    ticket: &Ticket,
    mut confirm: impl FnMut(&str) -> bool,
) -> Result<JoinReport> {
    let Some(incoming) = tokio::time::timeout(
        std::time::Duration::from_millis(pairing::TICKET_TTL_MS),
        endpoint.accept(),
    )
    .await
    .map_err(|_| Error::Malformed("the invite timed out (ten minutes)".into()))?
    else {
        return Err(Error::Malformed("no connection arrived".into()));
    };
    let conn = incoming
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    // ALPN mismatch closes the connection itself
    let (shared, transcript) = pake_turns(&conn, Side::Inviter, &ticket.secret).await?;
    // SAS: both screens, both humans
    let sas = six_digits(&shared, &transcript);
    if !confirm(&sas) {
        // this side said no: close, the peer's next read fails, no add
        return Err(Error::Malformed(" SAS rifiutata da chi invita".into()));
    }
    send_frame(&conn, &json!({"kind": "confirmed"})).await?;
    // both sides confirmed: theirs arrives too, and only then the identity
    let theirs = recv_frame(&conn).await?;
    if theirs.get("kind").and_then(Value::as_str) != Some("confirmed") {
        return Err(Error::Malformed(
            "the joiner did not confirm the SAS".into(),
        ));
    }
    // their identity, then everything they need
    let ident = recv_frame(&conn).await?;
    if ident.get("kind").and_then(Value::as_str) != Some("identity") {
        return Err(Error::Malformed("expected the joiner's identity".into()));
    }
    let device = ident["device"].as_str().unwrap_or_default().to_string();
    let name = ident["name"].as_str().unwrap_or_default().to_string();
    let transport = ident["transport"].as_str().unwrap_or_default().to_string();
    let hpke = ident["hpke"].as_str().unwrap_or_default().to_string();
    // the address book learns the new device's endpoint addresses (the
    // pairing endpoint's — a later successful dial refreshes them)
    {
        let node = unhex32(&transport).ok_or(Error::BadKey)?;
        let addrs: Vec<std::net::SocketAddr> = ident["addrs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter_map(|s| s.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        let book = EndpointAddr::from_parts(
            iroh::PublicKey::from_bytes(&node)
                .map_err(|e| Error::Malformed(format!("bad node id: {e}")))?,
            addrs
                .into_iter()
                .map(iroh::TransportAddr::Ip)
                .collect::<Vec<_>>(),
        );
        crate::serve::remember_addr(root, &device, &book).ok();
    }
    let key = DeviceKey::load_or_create(root)?;
    registry
        .add(&key, &device, &name, &transport, &hpke)
        .map_err(|e| Error::Registry(e.to_string()))?;
    // the registry rows travel first
    send_frame(
        &conn,
        &json!({"kind": "registry", "device": key.device_id(),
                "ops": registry.rows_json()}),
    )
    .await?;
    // then every shared cell: sync id, the wrapped keys, the sealed ops
    let cells = SharedCell::all_of(root)?;
    let mut report = JoinReport::default();
    for shared in &cells {
        let sync_id: [u8; 32] = shared
            .sync_id()
            .to_vec()
            .try_into()
            .map_err(|_| Error::Malformed("bad sync id".into()))?;
        // wrap every epoch we hold for the new device
        let ks = KeyStore::new(registry.conn());
        let mut wraps = Vec::new();
        for epoch in ks.epochs_of(&sync_id)? {
            if let Some(key) = ks.key_of(&sync_id, epoch)? {
                let hpke_pub = unhex32(&hpke).ok_or(Error::BadKey)?;
                let wrapped = crate::crypto::wrap_key(&key, &sync_id, epoch, &hpke_pub)?;
                ks.store_wrap(&sync_id, epoch, &device, &wrapped)?;
                wraps.push(json!({"epoch": epoch, "wrapped": b64(&wrapped)}));
            }
        }
        let cell_name = cell_name_of(root, &sync_id)?;
        send_frame(
            &conn,
            &json!({"kind": "cell", "name": cell_name,
                    "sync_id": sync_id.iter().map(|b| Value::from(*b)).collect::<Vec<_>>(),
                    "wraps": wraps, "count": shared.ids().len()}),
        )
        .await?;
        // the sealed operations, in lots with a receipt
        let mut count = 0_usize;
        let ids: Vec<[u8; 32]> = shared.ids().into_iter().map(|(_, id)| id).collect();
        for chunk in ids.chunks(64) {
            let envs: Vec<Value> = chunk
                .iter()
                .filter_map(|id| {
                    shared.envelope_of(id).map(|(epoch, nonce, ct)| {
                        json!({"id": b64(id), "epoch": epoch,
                                   "nonce": b64(&nonce), "ct": b64(&ct)})
                    })
                })
                .collect();
            send_frame(&conn, &json!({"kind": "envelopes", "envs": envs})).await?;
            count += chunk.len();
            let _ = recv_frame(&conn).await; // the receipt
        }
        report.cells.push((cell_name, count));
    }
    send_frame(&conn, &json!({"kind": "done"})).await?;
    report.registry_rows = registry.rows_json().len();
    // the joiner's goodbye: only then may this side drop the connection
    // (a sooner drop resets the wire and the last frames in flight would
    // vanish — the lockstep test found it)
    let bye = recv_frame(&conn).await?;
    if bye.get("kind").and_then(Value::as_str) != Some("bye") {
        return Err(Error::Malformed("expected the joiner's goodbye".into()));
    }
    // acknowledge the goodbye, then hold the connection until the joiner
    // closes it — the acknowledgment must not share the wire with our own
    // exit
    send_frame(&conn, &json!({"kind": "bye-ack"})).await?;
    let _ = conn.closed().await;
    Ok(report)
}

/// One sealed envelope as the join flow carries it.
type JoinEnvelope = (u64, [u8; 32], Vec<u8>, Vec<u8>);

/// The JOINER's side: connect with the ticket, run the turns, show the
/// SAS, and — after BOTH confirmations — hand over the identity and take
/// the registry, the keys and the cells in.
///
/// # Errors
/// Whatever turn fails; nothing about any note.
pub async fn join(
    endpoint: &Endpoint,
    ticket: &Ticket,
    name: &str,
    root: &std::path::Path,
    mut confirm: impl FnMut(&str) -> bool,
) -> Result<JoinReport> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    if !ticket.is_valid(now) {
        return Err(Error::Malformed("the ticket has expired".into()));
    }
    let node = iroh::PublicKey::from_bytes(&ticket.node_id)
        .map_err(|e| Error::Malformed(format!("bad node id: {e}")))?;
    let mut addr = EndpointAddr::from_parts(
        node,
        ticket
            .addrs
            .iter()
            .filter_map(|a| a.parse().ok())
            .map(iroh::TransportAddr::Ip)
            .collect::<Vec<_>>(),
    );
    if let Ok(relay) = ticket.relay.parse() {
        addr = addr.with_relay_url(relay);
    }
    let conn = endpoint
        .connect(addr, PAIRING_ALPN)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    let (shared, transcript) = pake_turns(&conn, Side::Joiner, &ticket.secret).await?;
    let sas = six_digits(&shared, &transcript);
    if !confirm(&sas) {
        return Err(Error::Malformed(" SAS rifiutata da chi entra".into()));
    }
    send_frame(&conn, &json!({"kind": "confirmed"})).await?;
    // the inviter's confirmation arrives before we say who we are
    let theirs = recv_frame(&conn).await?;
    if theirs.get("kind").and_then(Value::as_str) != Some("confirmed") {
        return Err(Error::Malformed(
            "the inviter did not confirm the SAS".into(),
        ));
    }
    // our identity (the key of this root IS the device)
    let key = DeviceKey::load_or_create(root)?;
    send_frame(
        &conn,
        &json!({
            "kind": "identity", "device": key.device_id(), "name": name,
            "transport": crate::device::hex(&key.transport_public()),
            "hpke": crate::device::hex(&key.hpke_public()),
            "addrs": crate::serve::direct_addresses(endpoint),
        }),
    )
    .await?;
    // the registry rows (each carries its own signature; the local one,
    // if any, reconciles)
    let reg_frame = recv_frame(&conn).await?;
    if reg_frame.get("kind").and_then(Value::as_str) != Some("registry") {
        return Err(Error::Malformed("expected the registry".into()));
    }
    let registry = DeviceRegistry::open(root)?;
    let before = registry.rows_json().len();
    registry.arrive_rows(reg_frame["ops"].as_array().unwrap_or(&Vec::new()))?;
    // the address book learns the inviter's endpoint (the address we
    // dialed is the best there is) — the ticket's relay rides with it
    // (R2: without the relay URL, a device behind NAT would never be
    // reachable again)
    if let Some(inviter) = reg_frame.get("device").and_then(Value::as_str)
        && let Ok(node) = iroh::PublicKey::from_bytes(&ticket.node_id)
    {
        {
            let mut addr = EndpointAddr::new(node);
            for a in &ticket.addrs {
                if let Ok(ip) = a.parse() {
                    addr = addr.with_ip_addr(ip);
                }
            }
            if let Ok(relay) = ticket.relay.parse() {
                addr = addr.with_relay_url(relay);
            }
            crate::serve::remember_addr(root, inviter, &addr).ok();
        }
    }
    let mut report = JoinReport {
        registry_rows: registry.rows_json().len() - before,
        cells: Vec::new(),
    };
    // the cells: sync id, wrapped keys, then the sealed operations
    let dk = DeviceKey::load_or_create(root)?;
    let mut pending: Option<Value> = None;
    loop {
        let msg = match pending.take() {
            Some(m) => m,
            None => recv_frame(&conn).await?,
        };
        match msg.get("kind").and_then(Value::as_str) {
            Some("cell") => {
                let cell_name = msg["name"].as_str().unwrap_or_default().to_string();
                let sync_id: Vec<u8> = msg["sync_id"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_u64)
                            .map(|b| b as u8)
                            .collect()
                    })
                    .unwrap_or_default();
                let sync_id: [u8; 32] = sync_id
                    .try_into()
                    .map_err(|_| Error::Malformed("bad sync id".into()))?;
                // unwrap the keys THIS device can open
                let ks = KeyStore::new(registry.conn());
                for wrap in msg["wraps"].as_array().unwrap_or(&Vec::new()) {
                    let epoch = wrap["epoch"].as_u64().unwrap_or(0);
                    let wrapped = unb64(wrap["wrapped"].as_str().unwrap_or(""))?;
                    if let Ok(k) =
                        crate::crypto::unwrap_key(&wrapped, &sync_id, epoch, &dk.hpke_secret())
                    {
                        ks.store_wrap(&sync_id, epoch, &dk.device_id(), &wrapped)?;
                        ks.store_key(&sync_id, epoch, &k)?;
                    }
                }
                // the bare cell of this copy
                std::fs::create_dir_all(root.join("cells"))?;
                let cell = Cell::open_bare(&root.join("cells").join(format!("{cell_name}.sqlite")))
                    .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
                registry.conn().execute(
                    "INSERT INTO shared_cells (name, sync_id) VALUES (?1, ?2)
                     ON CONFLICT(name) DO UPDATE SET sync_id = ?2",
                    rusqlite::params![cell_name, sync_id],
                )?;
                cell.mark_shared().ok();
                cell.set_signer(std::sync::Arc::new(crate::arrival::DeviceSigner::new(&dk)))
                    .ok();
                let shared = SharedCell::new(cell, registry_open(root)?, sync_id, root);
                let mut count = 0_usize;
                let mut batch: Vec<JoinEnvelope> = Vec::new();
                // the sealed operations, in lots with a receipt each, until
                // a different frame ends the stream
                loop {
                    let env_frame = recv_frame(&conn).await?;
                    if env_frame.get("kind").and_then(Value::as_str) != Some("envelopes") {
                        pending = Some(env_frame);
                        break;
                    }
                    for env in env_frame["envs"].as_array().unwrap_or(&Vec::new()) {
                        let epoch = env["epoch"].as_u64().unwrap_or(0);
                        let id: [u8; 32] = unb64(env["id"].as_str().unwrap_or(""))?
                            .try_into()
                            .map_err(|_| Error::Malformed("bad id".into()))?;
                        let nonce = unb64(env["nonce"].as_str().unwrap_or(""))?;
                        let ct = unb64(env["ct"].as_str().unwrap_or(""))?;
                        // the same gate as any session: a refusal counts,
                        // the flow goes on
                        let _ = shared.ingest(epoch, &id, &nonce, &ct);
                        count += 1;
                        batch.push((epoch, id, nonce, ct));
                    }
                    send_frame(&conn, &json!({"kind": "receipt"})).await?;
                }
                // the second pass: the lot arrives in ARBITRARY order (the
                // ids are content hashes), and an unsigned op that came
                // before its adopt was refused once — the adopt has landed
                // by now, and the ingest is idempotent by op id
                for (epoch, id, nonce, ct) in &batch {
                    let _ = shared.ingest(*epoch, id, nonce, ct);
                }
                report.cells.push((cell_name, count));
            }
            Some("done") => break,
            _ => return Err(Error::Malformed("unexpected frame in the join".into())),
        }
    }
    // everything arrived: say goodbye, wait for its acknowledgment, and
    // only then let the connection go (a bye dropped in flight is reset)
    send_frame(&conn, &json!({"kind": "bye"})).await?;
    let ack = recv_frame(&conn).await?;
    if ack.get("kind").and_then(Value::as_str) != Some("bye-ack") {
        return Err(Error::Malformed(
            "expected the goodbye's acknowledgment".into(),
        ));
    }
    Ok(report)
}

/// The PAKE turns both sides run: SPAKE2 over the password, the inviter's
/// transcript MAC (the joiner authenticates the inviter), and the two
/// RFC 9382 § 4 confirmation MACs (a wrong password dies here). Back: the
/// shared secret and the transcript.
async fn pake_turns(conn: &Connection, side: Side, password: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let (half, msg) = match side {
        Side::Inviter => SpakeHalf::begin_a(password),
        Side::Joiner => SpakeHalf::begin_b(password),
    };
    let mut secret_arr = [0_u8; 16];
    secret_arr.copy_from_slice(password);
    send_frame(
        conn,
        &json!({"kind": "pake", "v": PAIRING_VERSION, "msg": b64(&msg)}),
    )
    .await?;
    let theirs = recv_frame(conn).await?;
    if theirs.get("kind").and_then(Value::as_str) != Some("pake") {
        return Err(Error::Malformed("expected the PAKE half".into()));
    }
    let their_msg = unb64(theirs["msg"].as_str().unwrap_or(""))?;
    let shared = half.finish(&their_msg)?;
    let transcript = theirs_msg_transcript(&msg, &their_msg, side);
    // the inviter's MAC of the ticket secret over the transcript: whoever
    // is on the other end holds the secret, or the MAC does not verify
    match side {
        Side::Inviter => {
            send_frame(
                conn,
                &json!({"kind": "ticket-mac",
                        "mac": b64(&transcript_mac(&secret_arr, &transcript))}),
            )
            .await?;
        }
        Side::Joiner => {
            let mac_frame = recv_frame(conn).await?;
            if mac_frame.get("kind").and_then(Value::as_str) != Some("ticket-mac") {
                return Err(Error::Malformed("expected the inviter's MAC".into()));
            }
            let mac = unb64(mac_frame["mac"].as_str().unwrap_or(""))?;
            let expect = transcript_mac(&secret_arr, &transcript);
            if mac != expect {
                return Err(Error::BadSignature);
            }
        }
    }
    // RFC 9382 § 4: both key-confirmation MACs, both verified — a wrong
    // password (a wrong code) is discovered here, not by the human
    let mine = pairing::confirm_mac(side, &shared, &transcript);
    let theirs_side = match side {
        Side::Inviter => Side::Joiner,
        Side::Joiner => Side::Inviter,
    };
    send_frame(conn, &json!({"kind": "confirm", "mac": b64(&mine)})).await?;
    let their_confirm = recv_frame(conn).await?;
    if their_confirm.get("kind").and_then(Value::as_str) != Some("confirm") {
        return Err(Error::Malformed("expected the key confirmation".into()));
    }
    let their_mac = unb64(their_confirm["mac"].as_str().unwrap_or(""))?;
    if their_mac != pairing::confirm_mac(theirs_side, &shared, &transcript) {
        return Err(Error::BadSignature);
    }
    Ok((shared, transcript))
}

/// The transcript the SAS and the MACs cover: both public messages, in
/// the same reading order on both sides (A‖B — the inviter starts).
fn theirs_msg_transcript(mine: &[u8], theirs: &[u8], side: Side) -> Vec<u8> {
    match side {
        Side::Inviter => {
            let mut t = mine.to_vec();
            t.extend_from_slice(theirs);
            t
        }
        Side::Joiner => {
            let mut t = theirs.to_vec();
            t.extend_from_slice(mine);
            t
        }
    }
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

fn registry_open(root: &std::path::Path) -> Result<DeviceRegistry> {
    DeviceRegistry::open(root)
}

/// The cell NAME a root binds to a sync id (the row the joiner just wrote
/// on its side; the inviter reads its own).
fn cell_name_of(root: &std::path::Path, sync_id: &[u8; 32]) -> Result<String> {
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

use rusqlite::OptionalExtension;
