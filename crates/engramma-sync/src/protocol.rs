// SPDX-License-Identifier: Apache-2.0

//! The sync protocol over iroh (v0.6, P5). ALPN `engramma/sync/1`. At
//! acceptance the caller's iroh id must be the TRANSPORT key of a live
//! device of the list — otherwise the connection closes before any
//! exchange. A session: protocol version, the device list reconciled
//! FIRST, then per shared cell a negentropy (RBSR) reconciliation over
//! `(hlc.physical_ms, id32)` and the missing envelopes in both
//! directions, and the ack that moves the stable frontier (P1.6).
//!
//! The wire is length-prefixed JSON frames over iroh bidirectional
//! streams: small, debuggable, and inside QUIC's own encryption — the
//! envelopes' seal exists for the RELAY and the blind mailbox, not for
//! the peer.

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use serde_json::{Value, json};

use crate::device::DeviceKey;
use crate::keystore::KeyStore;
use crate::registry::DeviceRegistry;
use crate::{Error, Result};

/// The ALPN of the sync protocol.
pub const ALPN: &[u8] = b"engramma/sync/1";
/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// A message is at most 1 MiB.
pub const MAX_MESSAGE: usize = 1024 * 1024;

fn b64(data: &[u8]) -> String {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::encode_string(data)
}

fn unb64(text: &str) -> Result<Vec<u8>> {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::decode_vec(text).map_err(|e| Error::Malformed(format!("bad base64: {e}")))
}

fn sync_id_json(id: &[u8]) -> Value {
    Value::Array(id.iter().map(|b| Value::from(*b)).collect())
}

fn sync_id_of(v: &Value) -> Result<Vec<u8>> {
    v.get("sync_id")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_u64)
                .map(|b| b as u8)
                .collect()
        })
        .ok_or_else(|| Error::Malformed("message misses sync_id".into()))
}

/// One cell this device shares: the driver's hooks. `ids` are
/// `(hlc_ms, first-16-bytes-of-content_hash)`.
pub trait SyncedCell: Send + Sync {
    /// The cell's opaque sync id.
    fn sync_id(&self) -> Vec<u8>;
    /// The local head seq (the frontier's ack).
    fn head_seq(&self) -> i64;
    /// Every local operation identity.
    fn ids(&self) -> Vec<(u64, [u8; 32])>;
    /// The sealed envelope of an id the peer needs: (epoch, nonce, ct).
    fn envelope_of(&self, id32: &[u8]) -> Option<(u64, Vec<u8>, Vec<u8>)>;
    /// Ingest a received envelope.
    fn ingest(&self, epoch: u64, nonce: &[u8], ct: &[u8]) -> Result<()>;
}

/// A session's per-cell summary.
#[derive(Debug, Clone, Default)]
pub struct CellReport {
    /// The cell's sync id.
    pub sync_id: Vec<u8>,
    /// Envelopes sent.
    pub sent: usize,
    /// Envelopes received and ingested.
    pub received: usize,
}

/// What a session did.
#[derive(Debug, Clone, Default)]
pub struct SessionReport {
    /// One row per shared cell.
    pub cells: Vec<CellReport>,
}

/// Everything a session needs.
pub struct SessionCtx<'a> {
    /// The registry (devices.db).
    pub registry: &'a DeviceRegistry,
    /// The epoch keys.
    pub keys: KeyStore<'a>,
    /// This device's key.
    pub device: &'a DeviceKey,
    /// The shared cells.
    pub cells: Vec<Box<dyn SyncedCell>>,
}

/// Bind the sync endpoint: our TRANSPORT key, loopback in tests, the
/// relay off by default (the direct path; the relay path rides the same
/// protocol through a relay map).
///
/// # Errors
/// [`Error::Io`] when iroh cannot bind.
pub async fn bind_endpoint(key: &DeviceKey, bind: &str) -> Result<Endpoint> {
    let secret = iroh::SecretKey::from_bytes(&key.transport_seed());
    let builder = iroh::Endpoint::builder(iroh::endpoint::presets::N0DisableRelay)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .bind_addr(bind)
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    builder
        .bind()
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))
}

/// Connect to a peer.
///
/// # Errors
/// [`Error::Io`] on connection failure.
pub async fn connect(endpoint: &Endpoint, addr: EndpointAddr) -> Result<Connection> {
    endpoint
        .connect(addr, ALPN)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))
}

/// The gate every accepted connection passes FIRST (minaccia 3): the
/// caller's iroh id must be a live device's transport key.
///
/// # Errors
/// [`Error::Registry`] when the caller is unknown or removed.
pub fn authorize(caller_transport: &[u8; 32], registry: &DeviceRegistry) -> Result<String> {
    let caller_hex = crate::device::hex(caller_transport);
    registry
        .live_members()?
        .iter()
        .find_map(|m| {
            if m.transport == caller_hex {
                Some(m.device.clone())
            } else {
                None
            }
        })
        .ok_or_else(|| Error::Registry("the caller's transport key is not a live device's".into()))
}

async fn send_frame(conn: &Connection, msg: &Value) -> Result<()> {
    let bytes = msg.to_string();
    let body = bytes.as_bytes();
    if body.len() > MAX_MESSAGE {
        return Err(Error::Malformed("message too large".into()));
    }
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    let (mut send, _recv) = conn.open_bi().await.map_err(io_err)?;
    send.write_all(&frame)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    send.finish()
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    Ok(())
}

async fn recv_frame(conn: &Connection) -> Result<Value> {
    let (send, mut recv) = conn.accept_bi().await.map_err(io_err)?;
    drop(send);
    let mut len_buf = [0_u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE {
        return Err(Error::Malformed("message too large".into()));
    }
    let mut buf = vec![0_u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    serde_json::from_slice(&buf).map_err(|e| Error::Malformed(format!("bad frame: {e}")))
}

fn io_err(e: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}

/// Run one full session as the INITIATOR (we connected out).
///
/// # Errors
/// Any protocol error.
pub async fn run_initiator(conn: Connection, ctx: &SessionCtx<'_>) -> Result<SessionReport> {
    // 1. hello: version, device, our registry rows
    send_frame(
        &conn,
        &json!({
            "kind": "hello", "version": PROTOCOL_VERSION,
            "device": ctx.device.device_id(),
            "registry": ctx.registry.rows_json(),
        }),
    )
    .await?;
    // 2. their registry rows (the answerer validates nothing of ours: every
    //    row carries its signature)
    let answer = recv_frame(&conn).await?;
    if answer.get("kind").and_then(Value::as_str) != Some("registry") {
        return Err(Error::Malformed("expected the registry answer".into()));
    }
    ctx.registry
        .arrive_rows(answer["ops"].as_array().unwrap_or(&Vec::new()))?;
    // 3. every shared cell: offer, reconcile, exchange envelopes
    let mut report = SessionReport::default();
    for cell in &ctx.cells {
        report
            .cells
            .push(cell_exchange_initiator(&conn, cell.as_ref()).await?);
    }
    // 4. ack: the heads we now hold (the frontier's ack)
    send_frame(
        &conn,
        &json!({
            "kind": "ack",
            "cells": ctx.cells.iter().map(|c| json!({
                "sync_id": sync_id_json(&c.sync_id()),
                "head": c.head_seq(),
            })).collect::<Vec<_>>(),
        }),
    )
    .await?;
    let _ = recv_frame(&conn).await; // their ack, or the stream's end
    Ok(report)
}

/// Run one full session as the ANSWERER: the caller was authorized at
/// acceptance.
///
/// # Errors
/// Any protocol error.
pub async fn run_answerer(conn: Connection, ctx: &SessionCtx<'_>) -> Result<SessionReport> {
    let hello = recv_frame(&conn).await?;
    if hello.get("kind").and_then(Value::as_str) != Some("hello") {
        return Err(Error::Malformed("expected hello first".into()));
    }
    let version = hello.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version != PROTOCOL_VERSION {
        return Err(Error::Malformed(format!(
            "protocol version {version} is not {PROTOCOL_VERSION}"
        )));
    }
    ctx.registry
        .arrive_rows(hello["registry"].as_array().unwrap_or(&Vec::new()))?;
    send_frame(
        &conn,
        &json!({"kind": "registry", "ops": ctx.registry.rows_json()}),
    )
    .await?;
    let mut report = SessionReport::default();
    for cell in &ctx.cells {
        report
            .cells
            .push(cell_exchange_answerer(&conn, cell.as_ref()).await?);
    }
    // our ack closes the session; theirs closes ours
    send_frame(
        &conn,
        &json!({
            "kind": "ack",
            "cells": ctx.cells.iter().map(|c| json!({
                "sync_id": sync_id_json(&c.sync_id()),
                "head": c.head_seq(),
            })).collect::<Vec<_>>(),
        }),
    )
    .await?;
    let _ = recv_frame(&conn).await; // their ack, or the stream's end
    Ok(report)
}

async fn cell_exchange_initiator(conn: &Connection, cell: &dyn SyncedCell) -> Result<CellReport> {
    let mut_cell = cell; // one exchange, one driver
    let mut neg = Reconciler::initiator(mut_cell.ids())?;
    let query = neg.initiate()?;
    send_frame(
        conn,
        &json!({"kind": "cell_offer", "sync_id": sync_id_json(&mut_cell.sync_id()),
                 "epoch": 0, "query": b64(&query)}),
    )
    .await?;
    let answer = recv_frame(conn).await?;
    let reply = unb64(answer["reply"].as_str().unwrap_or(""))?;
    let outcome = neg.reconcile_as_initiator(&reply)?;
    let second = outcome.second;
    let mut have = outcome.have;
    let mut my_need = outcome.need;
    // the rare second round folds into the need list (64 KiB frames make
    // it unnecessary at our scale; correctness does not depend on skipping)
    if let Some(second_query) = second {
        send_frame(
            conn,
            &json!({"kind": "cell_query", "sync_id": sync_id_json(&mut_cell.sync_id()),
                     "query": b64(&second_query)}),
        )
        .await?;
        let second_answer = recv_frame(conn).await?;
        let reply2 = unb64(second_answer["reply"].as_str().unwrap_or(""))?;
        let more = neg.reconcile_as_initiator(&reply2)?;
        have.extend(more.have);
        my_need.extend(more.need);
        // fall through with the extended lists
        return finish_initiator_exchange(conn, mut_cell, have, my_need).await;
    }
    finish_initiator_exchange(conn, mut_cell, have, my_need).await
}

/// The initiator's tail after the RBSR math: ask for what we lack, send
/// what they lack, one lot at a time with a receipt each.
async fn finish_initiator_exchange(
    conn: &Connection,
    mut_cell: &dyn SyncedCell,
    have: Vec<[u8; 32]>,
    my_need: Vec<[u8; 32]>,
) -> Result<CellReport> {
    let mut received = 0;
    for chunk in my_need.chunks(64) {
        send_frame(
            conn,
            &json!({"kind": "need", "sync_id": sync_id_json(&mut_cell.sync_id()),
                     "ids": chunk.iter().map(|id| b64(id)).collect::<Vec<_>>()}),
        )
        .await?;
        let lot = recv_frame(conn).await?;
        if let Some(envs) = lot["envs"].as_array() {
            for env in envs {
                let epoch = env["epoch"].as_u64().unwrap_or(0);
                let nonce = unb64(env["nonce"].as_str().unwrap_or(""))?;
                let ct = unb64(env["ct"].as_str().unwrap_or(""))?;
                mut_cell.ingest(epoch, &nonce, &ct)?;
                received += 1;
            }
        }
    }
    let mut sent = 0;
    for chunk in have.chunks(64) {
        let envs: Vec<Value> = chunk
            .iter()
            .filter_map(|id| mut_cell.envelope_of(id))
            .map(|(epoch, nonce, ct)| json!({"epoch": epoch, "nonce": b64(&nonce), "ct": b64(&ct)}))
            .collect();
        send_frame(
            conn,
            &json!({"kind": "envelopes", "sync_id": sync_id_json(&mut_cell.sync_id()),
                     "envs": envs}),
        )
        .await?;
        sent += chunk.len();
        let _ = recv_frame(conn).await; // the receipt
    }
    Ok(CellReport {
        sync_id: mut_cell.sync_id(),
        sent,
        received,
    })
}

async fn cell_exchange_answerer(conn: &Connection, cell: &dyn SyncedCell) -> Result<CellReport> {
    let mut_cell = cell;
    let offer = recv_frame(conn).await?;
    if offer.get("kind").and_then(Value::as_str) != Some("cell_offer") {
        return Err(Error::Malformed("expected a cell offer".into()));
    }
    let query = unb64(offer["query"].as_str().unwrap_or(""))?;
    // the honest RBSR answer: negentropy's reply over OUR items — the
    // initiator's math on it decides who sends what
    let mut neg = Reconciler::responder(mut_cell.ids())?;
    let reply = neg.reconcile_as_responder(&query)?;
    send_frame(
        conn,
        &json!({"kind": "cell_answer", "sync_id": sync_id_of(&offer)?,
                 "reply": b64(&reply)}),
    )
    .await?;
    // the initiator may run a second round
    if let Ok(next) = recv_frame(conn).await {
        if next.get("kind").and_then(Value::as_str) == Some("cell_query") {
            let q2 = unb64(next["query"].as_str().unwrap_or(""))?;
            let r2 = neg.reconcile_as_responder(&q2)?;
            send_frame(
                conn,
                &json!({"kind": "cell_answer", "sync_id": sync_id_of(&offer)?,
                         "reply": b64(&r2)}),
            )
            .await?;
        } else if next.get("kind").and_then(Value::as_str) == Some("need") {
            serve_need(conn, mut_cell, &next).await?;
        } else if next.get("kind").and_then(Value::as_str) == Some("envelopes") {
            ingest_envelopes(conn, mut_cell, &next).await?;
        }
    }
    let mut sent = 0;
    let mut received = 0;
    loop {
        let Ok(msg) = recv_frame(conn).await else {
            break;
        };
        match msg.get("kind").and_then(Value::as_str) {
            Some("need") => sent += serve_need(conn, mut_cell, &msg).await?,
            Some("envelopes") => received += ingest_envelopes(conn, mut_cell, &msg).await?,
            _ => break, // the ack closes the session
        }
    }
    Ok(CellReport {
        sync_id: mut_cell.sync_id(),
        sent,
        received,
    })
}

/// Serve one need lot: the envelopes of the ids we hold.
async fn serve_need(conn: &Connection, cell: &dyn SyncedCell, msg: &Value) -> Result<usize> {
    let ids: Vec<[u8; 32]> = msg["ids"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|x| unb64(x.as_str()?).ok())
        .filter_map(|v| <[u8; 32]>::try_from(v).ok())
        .collect();
    let envs: Vec<Value> = ids
        .iter()
        .filter_map(|id| cell.envelope_of(id))
        .map(|(epoch, nonce, ct)| json!({"epoch": epoch, "nonce": b64(&nonce), "ct": b64(&ct)}))
        .collect();
    let count = envs.len();
    send_frame(
        conn,
        &json!({"kind": "envelopes", "sync_id": msg["sync_id"].clone(), "envs": envs}),
    )
    .await?;
    Ok(count)
}

/// Ingest one envelope lot, with the receipt.
async fn ingest_envelopes(conn: &Connection, cell: &dyn SyncedCell, msg: &Value) -> Result<usize> {
    let mut n = 0;
    if let Some(envs) = msg["envs"].as_array() {
        for env in envs {
            let epoch = env["epoch"].as_u64().unwrap_or(0);
            let nonce = unb64(env["nonce"].as_str().unwrap_or(""))?;
            let ct = unb64(env["ct"].as_str().unwrap_or(""))?;
            cell.ingest(epoch, &nonce, &ct)?;
            n += 1;
        }
    }
    send_frame(conn, &json!({"kind": "receipt"})).await?;
    Ok(n)
}

/// negentropy (RBSR) over `(hlc_ms, id32)` items, frame limit 64 KiB.
pub struct Reconciler {
    inner: negentropy::Negentropy<'static, negentropy::NegentropyStorageVector>,
}

impl Reconciler {
    /// The initiator's side.
    ///
    /// # Errors
    /// [`Error::Malformed`] on storage failure.
    pub fn initiator(items: Vec<(u64, [u8; 32])>) -> Result<Self> {
        let mut storage = negentropy::NegentropyStorageVector::new();
        for (ts, id) in items {
            storage
                .insert(ts, negentropy::Id::from_byte_array(id))
                .map_err(|e| Error::Malformed(format!("negentropy: {e}")))?;
        }
        storage
            .seal()
            .map_err(|e| Error::Malformed(format!("negentropy: {e}")))?;
        let inner = negentropy::Negentropy::owned(storage, 64 * 1024)
            .map_err(|e| Error::Malformed(format!("negentropy: {e}")))?;
        Ok(Self { inner })
    }

    /// The responder's side.
    ///
    /// # Errors
    /// [`Error::Malformed`] on storage failure.
    pub fn responder(items: Vec<(u64, [u8; 32])>) -> Result<Self> {
        let mut storage = negentropy::NegentropyStorageVector::new();
        for (ts, id) in items {
            storage
                .insert(ts, negentropy::Id::from_byte_array(id))
                .map_err(|e| Error::Malformed(format!("negentropy: {e}")))?;
        }
        storage
            .seal()
            .map_err(|e| Error::Malformed(format!("negentropy: {e}")))?;
        let inner = negentropy::Negentropy::owned(storage, 64 * 1024)
            .map_err(|e| Error::Malformed(format!("negentropy: {e}")))?;
        Ok(Self { inner })
    }

    /// The initial message.
    ///
    /// # Errors
    /// [`Error::Malformed`] on protocol failure.
    pub fn initiate(&mut self) -> Result<Vec<u8>> {
        self.inner
            .initiate()
            .map_err(|e| Error::Malformed(format!("negentropy: {e}")))
    }

    /// The responder's answer.
    ///
    /// # Errors
    /// [`Error::Malformed`] on protocol failure.
    pub fn reconcile_as_responder(&mut self, query: &[u8]) -> Result<Vec<u8>> {
        self.inner
            .reconcile(query)
            .map_err(|e| Error::Malformed(format!("negentropy: {e}")))
    }

    /// The initiator's math over the responder's reply: which ids do I
    /// HAVE that the responder needs (send them) and which do I NEED (ask).
    ///
    /// # Errors
    /// [`Error::Malformed`] on protocol failure.
    pub fn reconcile_as_initiator(&mut self, reply: &[u8]) -> Result<ReconcileOutcome> {
        let mut have: Vec<negentropy::Id> = Vec::new();
        let mut need: Vec<negentropy::Id> = Vec::new();
        let out = self
            .inner
            .reconcile_with_ids(reply, &mut have, &mut need)
            .map_err(|e| Error::Malformed(format!("negentropy: {e}")))?;
        let to32 = |v: Vec<negentropy::Id>| -> Vec<[u8; 32]> {
            v.into_iter()
                .filter_map(|id| <[u8; 32]>::try_from(id.as_ref().to_vec()).ok())
                .collect()
        };
        Ok(ReconcileOutcome {
            second: out,
            have: to32(have),
            need: to32(need),
        })
    }
}

/// What the initiator's RBSR math produced: the optional second-round
/// query, the ids the responder needs (we send them), the ids we need.
pub struct ReconcileOutcome {
    /// The second-round query, when one round did not settle it.
    pub second: Option<Vec<u8>>,
    /// The ids the responder lacks: we send these envelopes.
    pub have: Vec<[u8; 32]>,
    /// The ids we lack: we ask for these.
    pub need: Vec<[u8; 32]>,
}
