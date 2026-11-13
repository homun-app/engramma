// SPDX-License-Identifier: Apache-2.0

//! The blind mailbox (v0.6, P6): an `engramma` endpoint that is always on
//! and holds SEALED envelopes for a device list — a post box the devices
//! empty when they come back. It has NO cell keys: it stores ciphertext,
//! opaque ids, sizes and times; the authorization rides the device list's
//! own signatures, and device NAMES never reach it (the list's rows it
//! verifies carry keys).
//!
//! `engramma mailbox serve --root <dir>`: ALPN `engramma/mailbox/1`.
//! Retention: 180 days by default, configurable; the purge request a
//! device signs deletes the opaque ids of a note's envelopes AND VACUUMS,
//! so no byte of the sealed bodies survives in the file.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::device::DeviceKey;
use crate::registry::DeviceRegistry;
use crate::{Error, Result};

/// The mailbox's ALPN.
pub const MAILBOX_ALPN: &[u8] = b"engramma/mailbox/1";
/// Default retention: 180 days.
pub const RETENTION_DAYS: i64 = 180;

fn b64(data: &[u8]) -> String {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::encode_string(data)
}

fn unb64(text: &str) -> Result<Vec<u8>> {
    use base64ct::{Base64UrlUnpadded as B64, Encoding};
    B64::decode_vec(text).map_err(|e| Error::Malformed(format!("bad base64: {e}")))
}

/// The mailbox's own store: one SQLite file, envelopes by owner list and
/// cell sync id, indexed by OPAQUE id only.
pub struct MailboxStore {
    conn: Connection,
}

impl MailboxStore {
    /// Open (or create) the store under a root.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn open(root: &std::path::Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let conn = Connection::open(root.join("mailbox.db"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS envelopes (
                owner TEXT NOT NULL,          -- the owner list's id (its registry hash)
                sync_id BLOB NOT NULL,        -- the cell's opaque id
                opaque_id BLOB NOT NULL,      -- HMAC(content_hash): what reconciles
                nonce BLOB NOT NULL,
                ct BLOB NOT NULL,             -- the sealed operation
                stored_at INTEGER NOT NULL,
                PRIMARY KEY (owner, sync_id, opaque_id));
             CREATE INDEX IF NOT EXISTS idx_env_time ON envelopes(stored_at);",
        )?;
        Ok(Self { conn })
    }

    /// Store one envelope (idempotent by opaque id).
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn put(
        &self,
        owner: &str,
        sync_id: &[u8],
        opaque_id: &[u8],
        nonce: &[u8],
        ct: &[u8],
        now_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO envelopes (owner, sync_id, opaque_id, nonce, ct, stored_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![owner, sync_id, opaque_id, nonce, ct, now_ms],
        )?;
        Ok(())
    }

    /// The opaque ids of one owner+cell (for the reconciliation).
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn opaque_ids(&self, owner: &str, sync_id: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT opaque_id FROM envelopes WHERE owner = ?1 AND sync_id = ?2")?;
        let rows = stmt.query_map(rusqlite::params![owner, sync_id], |r| {
            r.get::<_, Vec<u8>>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The envelope of one opaque id.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn get(&self, owner: &str, opaque_id: &[u8]) -> Result<Option<StoredEnvelope>> {
        let row = self
            .conn
            .query_row(
                "SELECT sync_id, nonce, ct FROM envelopes WHERE owner = ?1 AND opaque_id = ?2",
                rusqlite::params![owner, opaque_id],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Delete the envelopes named by their opaque ids (a purge, signed by
    /// the device that applied it) — then VACUUM, so the freed pages
    /// really go: no byte of the sealed bodies survives in the file.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn purge_opaque(&self, owner: &str, opaque_ids: &[Vec<u8>]) -> Result<usize> {
        let mut n = 0;
        for id in opaque_ids {
            n += self.conn.execute(
                "DELETE FROM envelopes WHERE owner = ?1 AND opaque_id = ?2",
                rusqlite::params![owner, id],
            )?;
        }
        self.conn.execute_batch("VACUUM;")?;
        Ok(n)
    }

    /// Drop everything older than the retention.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn expire(&self, now_ms: i64, retention_days: i64) -> Result<usize> {
        let horizon = now_ms - retention_days * 24 * 3600 * 1000;
        let n = self
            .conn
            .execute("DELETE FROM envelopes WHERE stored_at < ?1", [horizon])?;
        if n > 0 {
            self.conn.execute_batch("VACUUM;")?;
        }
        Ok(n)
    }

    /// The whole file's bytes (the test's string-search target).
    ///
    /// # Errors
    /// [`Error::Io`] on read failure.
    pub fn file_bytes(&self, root: &std::path::Path) -> Result<Vec<u8>> {
        Ok(std::fs::read(root.join("mailbox.db"))?)
    }
}

/// One stored envelope: the cell it belongs to, the seal's nonce and
/// ciphertext.
pub type StoredEnvelope = (Vec<u8>, Vec<u8>, Vec<u8>);

/// One mailbox message on the wire.
#[derive(Debug, Clone)]
pub enum MailboxMsg {
    /// A device deposits envelopes.
    Deposit { owner: String, envs: Vec<Value> },
    /// A device asks for the opaque ids of its cells.
    Enumerate { owner: String },
    /// A device fetches envelopes by opaque id.
    Fetch { owner: String, ids: Vec<Vec<u8>> },
    /// A signed purge: delete these opaque ids, for real.
    Purge {
        owner: String,
        ids: Vec<Vec<u8>>,
        sig: Vec<u8>,
    },
    /// The answer to enumerate/fetch/deposit.
    Reply {
        ids: Vec<Vec<u8>>,
        envs: Vec<Value>,
        purged: usize,
    },
}

impl MailboxMsg {
    /// Encode for the wire.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Deposit { owner, envs } => {
                json!({"kind": "deposit", "owner": owner, "envs": envs})
            }
            Self::Enumerate { owner } => json!({"kind": "enumerate", "owner": owner}),
            Self::Fetch { owner, ids } => json!({"kind": "fetch", "owner": owner,
                "ids": ids.iter().map(|i| b64(i)).collect::<Vec<_>>()}),
            Self::Purge { owner, ids, sig } => json!({"kind": "purge", "owner": owner,
                "ids": ids.iter().map(|i| b64(i)).collect::<Vec<_>>(),
                "sig": b64(sig)}),
            Self::Reply { ids, envs, purged } => json!({"kind": "reply",
                "ids": ids.iter().map(|i| b64(i)).collect::<Vec<_>>(),
                "envs": envs, "purged": purged}),
        }
    }

    /// Decode from the wire.
    ///
    /// # Errors
    /// [`Error::Malformed`] when the frame is not a mailbox message.
    pub fn from_json(v: &Value) -> Result<Self> {
        let owner = || {
            v.get("owner")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let ids = || -> Result<Vec<Vec<u8>>> {
            v.get("ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| unb64(x.as_str().unwrap_or("")).ok())
                        .collect()
                })
                .ok_or_else(|| Error::Malformed("message misses ids".into()))
        };
        match v.get("kind").and_then(Value::as_str) {
            Some("deposit") => Ok(Self::Deposit {
                owner: owner(),
                envs: v
                    .get("envs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            }),
            Some("enumerate") => Ok(Self::Enumerate { owner: owner() }),
            Some("fetch") => Ok(Self::Fetch {
                owner: owner(),
                ids: ids()?,
            }),
            Some("purge") => Ok(Self::Purge {
                owner: owner(),
                ids: ids()?,
                sig: unb64(v.get("sig").and_then(Value::as_str).unwrap_or(""))?,
            }),
            _ => Err(Error::Malformed("unknown mailbox message".into())),
        }
    }
}

/// The purge request's canonical content: what a device signs so the
/// mailbox deletes for real.
#[must_use]
pub fn purge_signing_content(owner: &str, ids: &[Vec<u8>]) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"engramma/mailbox/purge/v1");
    hasher.update(owner.as_bytes());
    for id in ids {
        hasher.update(&id.len().to_be_bytes());
        hasher.update(id);
    }
    hasher.finalize().as_bytes().to_vec()
}

/// Verify a purge request against the owner list's live members.
///
/// # Errors
/// [`Error::BadSignature`] when no live member signed it.
pub fn verify_purge(
    registry: &DeviceRegistry,
    owner: &str,
    ids: &[Vec<u8>],
    sig: &[u8],
) -> Result<()> {
    let content = purge_signing_content(owner, ids);
    for member in registry.live_members()? {
        // the member's device id IS its signing public key
        let mut key = [0_u8; 32];
        let hex = &member.device;
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            for i in 0..32 {
                key[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
            }
            if DeviceKey::verify(&key, &content, sig).is_ok() {
                return Ok(());
            }
        }
    }
    Err(Error::BadSignature)
}
