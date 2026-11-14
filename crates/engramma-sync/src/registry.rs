// SPDX-License-Identifier: Apache-2.0

//! The owner's device list (v0.6, P2): a small SIGNED registry per
//! data-root, in `<root>/devices.db`. It is the root of every other trust
//! decision — which connections to accept, whose signatures to honor,
//! whose sealed boxes to open — so it reconciles FIRST, before any cell.
//!
//! The registry is a log with the cell's own discipline: an append-only
//! chain of operations, a total order `(hlc, op_id)`, signatures over the
//! canonical content. Three kinds: `create` (the first device, signed by
//! itself), `add` (a member vouches for a new device), `remove` (a member
//! revokes one). A `remove` concurrent with an `add` of the same device
//! WINS — the revocation dominates: a device that was ever removed cannot
//! come back through the door it left by.
//!
//! The `remove` carries the Merkle root and the count of the operations of
//! the removed device that the REMOVER had accepted: from then on every
//! copy accepts from that device only those, and nothing else (the cell
//! log enforces its side in P1's terms).

use rusqlite::Connection;
use serde_json::{Value, json};
use ulid::Ulid;

use crate::device::DeviceKey;
use crate::{Error, Result};

/// One registry operation, as it travels between devices.
#[derive(Debug, Clone)]
pub struct RegistryOp {
    /// Operation id.
    pub op_id: Ulid,
    /// The registry's own HLC.
    pub hlc: engramma_core::hlc::Hlc,
    /// create | add | remove.
    pub kind: RegistryKind,
    /// The canonical payload (see [`RegistryKind`]).
    pub payload: Value,
    /// The Ed25519 signature over the canonical content.
    pub sig: Vec<u8>,
}

/// The registry operation kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryKind {
    /// The first device. Payload: `{device, name, transport, hpke}`.
    Create,
    /// A new device. Payload: `{device, name, transport, hpke}`.
    Add,
    /// A revoked device. Payload: `{device, accepted_root, accepted_count}`.
    Remove,
}

impl RegistryKind {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// A live (or once-live) member as the registry sees it.
#[derive(Debug, Clone)]
pub struct Member {
    /// The device id (its signing public key, hex).
    pub device: String,
    /// The name its owner chose.
    pub name: String,
    /// The iroh transport public key (hex).
    pub transport: String,
    /// The HPKE public key (hex).
    pub hpke: String,
    /// Who added it, when.
    pub added_by: String,
    /// When it was removed, and by whom (`None` while it lives).
    pub removed: Option<(String, String)>,
}

/// The signed device registry: `<root>/devices.db`.
pub struct DeviceRegistry {
    conn: Connection,
    #[allow(dead_code)] // the path is kept for the CLI's status output
    path: std::path::PathBuf,
}

impl DeviceRegistry {
    /// The underlying connection (the keystore shares it).
    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Every registry row as JSON, for the session's first exchange: each
    /// row carries the signature it was born with, so the peer validates
    /// on arrival, not on trust.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn rows_json(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        if let Ok(rows) = self
            .conn
            .prepare("SELECT op_id, hlc, kind, payload_json, sig FROM registry ORDER BY seq")
            .and_then(|mut stmt| {
                stmt.query_map([], |r| {
                    let payload: String = r.get(3)?;
                    let sig: Vec<u8> = r.get(4)?;
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        payload,
                        sig,
                    ))
                })
                .map(|rows| rows.collect::<Vec<_>>())
            })
        {
            for row in rows.into_iter().flatten() {
                let (op_id, hlc, kind, payload, sig) = row;
                out.push(serde_json::json!({
                    "op_id": op_id,
                    "hlc": hlc,
                    "kind": kind,
                    "payload": serde_json::from_str::<serde_json::Value>(&payload)
                        .unwrap_or(serde_json::Value::Null),
                    "sig": base64_url(&sig),
                }));
            }
        }
        out
    }

    /// Apply the rows a session brought: each is decoded, verified and
    /// applied through [`DeviceRegistry::arrive`] — signature first,
    /// rules second.
    ///
    /// # Errors
    /// Whatever `arrive` rejects with; a bad row aborts the exchange.
    pub fn arrive_rows(&self, rows: &[serde_json::Value]) -> Result<()> {
        for row in rows {
            let op = decode_row(row)?;
            self.arrive(&op)?;
        }
        Ok(())
    }

    /// Open (or create) the registry of a data-root.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn open(root: &std::path::Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let path = root.join("devices.db");
        let conn = Connection::open(&path)?;
        // several handles share one devices.db (the registry, every
        // SharedCell adapter, the keystore): a write must wait, not fail
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS registry (
                seq INTEGER PRIMARY KEY, op_id TEXT UNIQUE, hlc TEXT, kind TEXT,
                payload_json TEXT, sig BLOB, prev_hash TEXT, hash TEXT);
             CREATE TABLE IF NOT EXISTS members (
                device TEXT PRIMARY KEY, name TEXT NOT NULL, transport TEXT NOT NULL,
                hpke TEXT NOT NULL, added_by TEXT NOT NULL, added_at TEXT NOT NULL,
                removed_at TEXT, removed_by TEXT);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE IF NOT EXISTS shared_cells (
                name TEXT PRIMARY KEY,          -- the cell file's name (under cells/)
                sync_id BLOB NOT NULL UNIQUE);  -- HMAC of the epoch-0 key
             CREATE TABLE IF NOT EXISTS device_addrs (
                device TEXT PRIMARY KEY,        -- the device id
                addrs TEXT NOT NULL,            -- JSON: {node_id, relay, addrs}
                seen_at TEXT NOT NULL);         -- last session that updated it
             CREATE TABLE IF NOT EXISTS cell_keys (
                sync_id BLOB NOT NULL, epoch INTEGER NOT NULL,
                key BLOB NOT NULL,        -- the raw epoch key, for holders
                wrapped_for TEXT,         -- device id when this row is a wrap
                wrapped BLOB,             -- the HPKE wrap (when wrapped_for)
                created_at TEXT NOT NULL,
                PRIMARY KEY (sync_id, epoch, wrapped_for));",
        )?;
        Ok(Self { conn, path })
    }

    /// The canonical content a registry signature covers.
    fn content(
        op_id: &Ulid,
        hlc: &engramma_core::hlc::Hlc,
        kind: RegistryKind,
        payload: &Value,
    ) -> String {
        json!({
            "hlc": hlc.to_string(),
            "kind": kind.as_str(),
            "op_id": op_id.to_string(),
            "payload": payload,
        })
        .to_string()
    }

    fn tick(&self) -> engramma_core::hlc::Hlc {
        // the registry's own clock: the max ever seen, then a tick
        let logged: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'hlc'", [], |r| r.get(0))
            .ok()
            .flatten();
        let wall = self
            .conn
            .query_row(
                "SELECT CAST(strftime('%s','now') AS INTEGER) * 1000",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64;
        let mut clock =
            engramma_core::hlc::HlcClock::new(crate::device::registry_node(), move || wall);
        if let Some(s) = logged
            && let Ok(h) = engramma_core::hlc::Hlc::parse(&s)
        {
            clock.observe(&h);
        }
        let t = clock.tick();
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('hlc', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = ?1",
                [t.to_string()],
            )
            .ok();
        t
    }

    fn append(&self, op: &RegistryOp) -> Result<()> {
        let (prev_seq, prev_hash): (i64, String) = self
            .conn
            .query_row(
                "SELECT seq, hash FROM registry ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, "0".repeat(64)));
        let mut hasher = blake3::Hasher::new();
        hasher.update(prev_hash.as_bytes());
        hasher.update(
            json!({
                "hlc": op.hlc.to_string(),
                "kind": op.kind.as_str(),
                "op_id": op.op_id.to_string(),
                "payload": op.payload,
            })
            .to_string()
            .as_bytes(),
        );
        hasher.update(&op.sig);
        let hash = hasher.finalize().to_hex().to_string();
        self.conn.execute(
            "INSERT INTO registry (seq, op_id, hlc, kind, payload_json, sig, prev_hash, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                prev_seq + 1,
                op.op_id.to_string(),
                op.hlc.to_string(),
                op.kind.as_str(),
                op.payload.to_string(),
                op.sig,
                prev_hash,
                hash
            ],
        )?;
        Ok(())
    }

    /// The FIRST device: signed by itself. Refused if anything is already
    /// there.
    ///
    /// # Errors
    /// [`Error::Registry`] when the registry is not empty.
    pub fn create_first(&self, dk: &DeviceKey, name: &str) -> Result<RegistryOp> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM registry", [], |r| r.get(0))?;
        if n > 0 {
            return Err(Error::Registry("the registry already exists".into()));
        }
        let payload = json!({
            "device": dk.device_id(),
            "name": name,
            "transport": crate::device::hex(&dk.transport_public()),
            "hpke": crate::device::hex(&dk.hpke_public()),
        });
        let op_id = engramma_core::id::new_ulid().map_err(|e| Error::Malformed(e.to_string()))?;
        let hlc = self.tick();
        let content = Self::content(&op_id, &hlc, RegistryKind::Create, &payload);
        let op = RegistryOp {
            op_id,
            hlc,
            kind: RegistryKind::Create,
            payload,
            sig: dk.sign(content.as_bytes()),
        };
        self.append(&op)?;
        self.apply_member_row(&op)?;
        Ok(op)
    }

    /// Add a device, signed by THIS device (a member).
    ///
    /// # Errors
    /// [`Error::Registry`] when the signer is not a live member or the
    /// device is already known-and-removed (the revocation dominates).
    pub fn add(
        &self,
        dk: &DeviceKey,
        device: &str,
        name: &str,
        transport: &str,
        hpke: &str,
    ) -> Result<RegistryOp> {
        self.require_live_member(&dk.device_id())?;
        if self.was_ever_removed(device)? {
            return Err(Error::Registry(format!(
                "device {device} was removed: the revocation dominates, it cannot be re-added"
            )));
        }
        let live: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE device = ?1 AND removed_at IS NULL",
            [device],
            |r| r.get(0),
        )?;
        if live > 0 {
            return Err(Error::Registry(format!(
                "device {device} is already a member"
            )));
        }
        let payload = json!({
            "device": device, "name": name, "transport": transport, "hpke": hpke,
            "by": dk.device_id(),
        });
        let op_id = engramma_core::id::new_ulid().map_err(|e| Error::Malformed(e.to_string()))?;
        let hlc = self.tick();
        let content = Self::content(&op_id, &hlc, RegistryKind::Add, &payload);
        let op = RegistryOp {
            op_id,
            hlc,
            kind: RegistryKind::Add,
            payload,
            sig: dk.sign(content.as_bytes()),
        };
        self.append(&op)?;
        self.apply_member_row(&op)?;
        Ok(op)
    }

    /// Remove a device, signed by THIS device. `accepted_root` and
    /// `accepted_count` name the operations of the removed device this
    /// signer had accepted (the cell log's own reconciliation uses them).
    ///
    /// # Errors
    /// [`Error::Registry`] for the last live device, an unknown member, or
    /// a non-member signer.
    pub fn remove(
        &self,
        dk: &DeviceKey,
        device: &str,
        accepted_root: &str,
        accepted_count: u64,
    ) -> Result<RegistryOp> {
        self.require_live_member(&dk.device_id())?;
        let live: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE removed_at IS NULL",
            [],
            |r| r.get(0),
        )?;
        if live <= 1 {
            return Err(Error::Registry(
                "removing the last device would orphan the data root".into(),
            ));
        }
        let known: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE device = ?1 AND removed_at IS NULL",
            [device],
            |r| r.get(0),
        )?;
        if known == 0 {
            return Err(Error::Registry(format!(
                "device {device} is not a live member"
            )));
        }
        let payload = json!({
            "device": device,
            "accepted_root": accepted_root,
            "accepted_count": accepted_count,
            "by": dk.device_id(),
        });
        let op_id = engramma_core::id::new_ulid().map_err(|e| Error::Malformed(e.to_string()))?;
        let hlc = self.tick();
        let content = Self::content(&op_id, &hlc, RegistryKind::Remove, &payload);
        let op = RegistryOp {
            op_id,
            hlc,
            kind: RegistryKind::Remove,
            payload,
            sig: dk.sign(content.as_bytes()),
        };
        self.append(&op)?;
        self.apply_member_row(&op)?;
        Ok(op)
    }

    /// A registry operation arriving from another device: verified, then
    /// applied in the registry's total order. Idempotent by `op_id`.
    /// Returns `false` when the operation was already held.
    ///
    /// # Errors
    /// [`Error::BadSignature`] for a forged or unknown signer;
    /// [`Error::Registry`] when the rules refuse it.
    pub fn arrive(&self, op: &RegistryOp) -> Result<bool> {
        let seen: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM registry WHERE op_id = ?1",
            [op.op_id.to_string()],
            |r| r.get(0),
        )?;
        if seen > 0 {
            return Ok(false);
        }
        self.verify(op)?;
        self.append(op)?;
        self.apply_member_row(op)?;
        Ok(true)
    }

    /// The signature must verify against the SIGNING key of a live member
    /// (the create signs itself; its payload names the very key).
    fn verify(&self, op: &RegistryOp) -> Result<()> {
        let content = Self::content(&op.op_id, &op.hlc, op.kind, &op.payload);
        let device = op
            .payload
            .get("device")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match op.kind {
            RegistryKind::Create => {
                // the create covers itself: the payload's key must verify
                let mut key = [0_u8; 32];
                let ok = hex_decode(&device, &mut key)
                    && DeviceKey::verify(&key, content.as_bytes(), &op.sig).is_ok();
                if !ok {
                    return Err(Error::BadSignature);
                }
            }
            RegistryKind::Add | RegistryKind::Remove => {
                let signer = op
                    .payload
                    .get("by")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(mut key) = self.live_signing_key(signer)? else {
                    return Err(Error::BadSignature);
                };
                if DeviceKey::verify(&key, content.as_bytes(), &op.sig).is_err() {
                    return Err(Error::BadSignature);
                }
                let _ = &mut key;
            }
        }
        Ok(())
    }

    fn live_signing_key(&self, device: &str) -> Result<Option<[u8; 32]>> {
        let live: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE device = ?1 AND removed_at IS NULL",
            [device],
            |r| r.get(0),
        )?;
        if live == 0 {
            return Ok(None);
        }
        // the device id IS the signing public key
        let mut key = [0_u8; 32];
        if !hex_decode(device, &mut key) {
            return Ok(None);
        }
        Ok(Some(key))
    }

    fn require_live_member(&self, device: &str) -> Result<()> {
        if self.live_signing_key(device)?.is_some() {
            Ok(())
        } else {
            Err(Error::Registry(format!(
                "device {device} is not a live member"
            )))
        }
    }

    fn was_ever_removed(&self, device: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE device = ?1 AND removed_at IS NOT NULL",
            [device],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// The state side: the members table follows the operation, whatever
    /// order the operations arrived in (the total order rules).
    fn apply_member_row(&self, op: &RegistryOp) -> Result<()> {
        let device = op
            .payload
            .get("device")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match op.kind {
            RegistryKind::Create | RegistryKind::Add => {
                self.conn.execute(
                    "INSERT INTO members (device, name, transport, hpke, added_by, added_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(device) DO UPDATE SET
                        removed_at = NULL, removed_by = NULL",
                    rusqlite::params![
                        device,
                        op.payload.get("name").and_then(Value::as_str).unwrap_or(""),
                        op.payload
                            .get("transport")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        op.payload.get("hpke").and_then(Value::as_str).unwrap_or(""),
                        op.payload
                            .get("by")
                            .and_then(Value::as_str)
                            .unwrap_or(device),
                        op.hlc.to_string(),
                    ],
                )?;
            }
            RegistryKind::Remove => {
                self.conn.execute(
                    "UPDATE members SET removed_at = ?2, removed_by = ?3 WHERE device = ?1",
                    rusqlite::params![
                        device,
                        op.hlc.to_string(),
                        op.payload.get("by").and_then(Value::as_str).unwrap_or(""),
                    ],
                )?;
            }
        }
        Ok(())
    }

    /// Every live member.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn members(&self) -> Result<Vec<Member>> {
        let mut stmt = self.conn.prepare(
            "SELECT device, name, transport, hpke, added_by, removed_at, removed_by
             FROM members ORDER BY device",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Member {
                device: r.get(0)?,
                name: r.get(1)?,
                transport: r.get(2)?,
                hpke: r.get(3)?,
                added_by: r.get(4)?,
                removed: match r.get::<_, Option<String>>(5)? {
                    Some(at) => Some((at, r.get::<_, String>(6)?)),
                    None => None,
                },
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The live members only.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn live_members(&self) -> Result<Vec<Member>> {
        Ok(self
            .members()?
            .into_iter()
            .filter(|m| m.removed.is_none())
            .collect())
    }

    /// The Merkle root + count a remove recorded for a device, when any.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn accepted_set_of(&self, device: &str) -> Result<Option<(String, u64)>> {
        Ok(self.accepted_sets_of(device)?.into_iter().next_back())
    }

    /// EVERY remove of a device this list holds, oldest first: the union of
    /// the accepted sets (R1, v0.6.1) — concurrent removes by different
    /// members name different sets, and an operation of the removed device
    /// enters with a proof toward ANY of the recorded roots.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn accepted_sets_of(&self, device: &str) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload_json FROM registry
             WHERE kind = 'remove' AND json_extract(payload_json, '$.device') = ?1
             ORDER BY hlc, op_id",
        )?;
        let rows: Vec<String> = stmt
            .query_map([device], |r| r.get(0))?
            .flatten()
            .collect();
        let mut out = Vec::with_capacity(rows.len());
        for payload in rows {
            let v: Value = serde_json::from_str(&payload)
                .map_err(|e| Error::Malformed(format!("bad remove payload: {e}")))?;
            out.push((
                v.get("accepted_root")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                v.get("accepted_count").and_then(Value::as_u64).unwrap_or(0),
            ));
        }
        Ok(out)
    }
}

fn decode_row(row: &serde_json::Value) -> Result<RegistryOp> {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    let op_id = ulid::Ulid::from_string(
        row.get("op_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    )
    .map_err(|e| Error::Malformed(format!("bad op_id: {e}")))?;
    let hlc = engramma_core::hlc::Hlc::parse(
        row.get("hlc")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    )
    .map_err(|e| Error::Malformed(format!("bad hlc: {e}")))?;
    let kind = match row
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
    {
        "create" => RegistryKind::Create,
        "add" => RegistryKind::Add,
        "remove" => RegistryKind::Remove,
        other => return Err(Error::Malformed(format!("unknown registry kind `{other}`"))),
    };
    let sig = B64::decode_vec(
        row.get("sig")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    )
    .map_err(|e| Error::Malformed(format!("bad signature: {e}")))?;
    Ok(RegistryOp {
        op_id,
        hlc,
        kind,
        payload: row
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        sig,
    })
}

fn base64_url(data: &[u8]) -> String {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::encode_string(data)
}

fn hex_decode(hex: &str, out: &mut [u8; 32]) -> bool {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (chunk[1] as char).to_digit(16).unwrap_or(0) as u8;
        out[i] = (hi << 4) | lo;
    }
    true
}
