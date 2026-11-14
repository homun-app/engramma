// SPDX-License-Identifier: Apache-2.0

//! The production adapter between a shared [`Cell`] and the sync session
//! (v0.6.1, P1). What lived in the v0.6 test file, at epoch 0 fixed, is
//! here for real:
//!
//! - the epoch keys come from the root's keystore (`devices.db`), never a
//!   constant: sealing uses the CURRENT epoch, opening uses the epoch THE
//!   ENVELOPE names (a rotation must not lock out holders of older frames);
//! - the ids are `(hlc.physical_ms, content_hash)` — the immutable wire
//!   identity (v0.6.1, P0);
//! - every INGEST passes the arrival gate: signature against the device
//!   list, live-or-accepted membership, `adopt` coverage for the unsigned
//!   era — then the cell's own `ingest_op` (identity recompute, clock cap).
//!
//! The mapping cell-name → sync id lives beside the keystore (per-root,
//! never replicated: the id is a local fact about which key opens what).

use engramma_core::Cell;
use engramma_core::cell::ops::Op;
use rusqlite::OptionalExtension;

use crate::arrival;
use crate::crypto::{self, Envelope, EpochKey};
use crate::keystore::KeyStore;
use crate::protocol::SyncedCell;
use crate::registry::DeviceRegistry;
use crate::wire;
use crate::{Error, Result};

/// The AAD of one sealed operation: `cell_sync_id ‖ epoch ‖ op id` — the
/// routing an attacker cannot change without breaking the seal. The op id
/// travels in the clear beside the envelope (the receiver cannot know it
/// before opening): the seal binds envelope and id together.
#[must_use]
pub fn op_aad(sync_id: &[u8; 32], epoch: u64, id32: &[u8; 32]) -> Vec<u8> {
    let mut aad = sync_id.to_vec();
    aad.extend_from_slice(&epoch.to_be_bytes());
    aad.extend_from_slice(id32);
    aad
}

/// One shared cell this device holds: the cell, its keystore, the arrival
/// gate. The registry handle sits behind a mutex: rusqlite connections are
/// not `Sync`, and the session driver holds the adapter as a trait object.
pub struct SharedCell {
    cell: Cell,
    keys: std::sync::Mutex<DeviceRegistry>,
    sync_id: [u8; 32],
}

impl SharedCell {
    /// Bind an opened cell to a sync id (the name→id row is the caller's
    /// to write: `share_cell` for the sharer, the pairing for the joiner).
    #[must_use]
    pub fn new(cell: Cell, keys: DeviceRegistry, sync_id: [u8; 32]) -> Self {
        Self {
            cell,
            keys: std::sync::Mutex::new(keys),
            sync_id,
        }
    }

    /// The registry this adapter reads keys and members from.
    pub fn registry(&self) -> std::sync::MutexGuard<'_, DeviceRegistry> {
        self.keys
            .lock()
            .expect(/* never poisoned here */ "shared cell registry")
    }

    /// Every shared cell registered in a root. A row whose cell file or
    /// key vanished is skipped: a broken registration must not take the
    /// whole sync down with it.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL or open failure.
    pub fn all_of(root: &std::path::Path) -> Result<Vec<Self>> {
        let rows: Vec<(String, Vec<u8>)> = {
            let reg = DeviceRegistry::open(root)?;
            let mut stmt = reg
                .conn()
                .prepare("SELECT name, sync_id FROM shared_cells ORDER BY name")?;
            let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            mapped.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut out = Vec::new();
        for (name, id) in rows {
            let Ok(sync_id) = <[u8; 32]>::try_from(id.as_slice()) else {
                continue;
            };
            let Ok(cell) = Cell::open(&cell_file(root, &name)) else {
                continue;
            };
            let Ok(keys) = DeviceRegistry::open(root) else {
                continue;
            };
            out.push(Self {
                cell,
                keys: std::sync::Mutex::new(keys),
                sync_id,
            });
        }
        Ok(out)
    }

    /// The underlying cell (local writes, reads, verify).
    #[must_use]
    pub fn cell(&self) -> &Cell {
        &self.cell
    }

    /// The cell's opaque sync id.
    #[must_use]
    pub fn sync_id(&self) -> &[u8; 32] {
        &self.sync_id
    }

    /// The CURRENT epoch (the newest key this device holds for the cell).
    ///
    /// # Errors
    /// [`Error::BadKey`] when this device holds no key at all.
    pub fn current_epoch(&self) -> Result<u64> {
        let reg = self.registry();
        KeyStore::new(reg.conn())
            .epochs_of(&self.sync_id)?
            .into_iter()
            .max()
            .ok_or(Error::BadKey)
    }

    fn key_of(&self, epoch: u64) -> Result<EpochKey> {
        let reg = self.registry();
        KeyStore::new(reg.conn())
            .key_of(&self.sync_id, epoch)?
            .ok_or(Error::BadKey)
    }

    /// The key of one epoch this device holds.
    ///
    /// # Errors
    /// [`Error::BadKey`] when this device holds no key for that epoch.
    pub fn epoch_key(&self, epoch: u64) -> Result<EpochKey> {
        self.key_of(epoch)
    }

    /// The cell's stable MAC key (from epoch 0), when this device holds
    /// the epoch-0 key: what turns a content hash into its opaque id.
    #[must_use]
    pub fn epoch0_mac_key(&self) -> Option<[u8; 32]> {
        let reg = self.registry();
        KeyStore::new(reg.conn())
            .key_of(&self.sync_id, 0)
            .ok()
            .flatten()
            .map(|k| k.mac_key())
    }
}

impl SyncedCell for SharedCell {
    fn sync_id(&self) -> Vec<u8> {
        self.sync_id.to_vec()
    }

    fn head_seq(&self) -> i64 {
        self.cell
            .debug_column("SELECT CAST(COALESCE(MAX(seq),0) AS TEXT) FROM ops")
            .unwrap_or_default()
            .first()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn ids(&self) -> Vec<(u64, [u8; 32])> {
        self.cell
            .ops()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|op| {
                let id = unhex32(&op.content_hash)?;
                Some((op.hlc.physical_ms, id))
            })
            .collect()
    }

    fn envelope_of(&self, id32: &[u8; 32]) -> Option<(u64, Vec<u8>, Vec<u8>)> {
        let op = self.find_op(id32)?;
        let epoch = self.current_epoch().ok()?;
        let key = self.key_of(epoch).ok()?;
        // an unsigned operation travels with its adopt proof; everything
        // else travels alone
        let plaintext = if op.sig.is_none() && op.kind != engramma_core::cell::ops::OpKind::Adopt {
            let proof = arrival::adopt_proof(&self.cell, &op).ok()?;
            wire::envelope_wire(&op, Some(&proof))
        } else {
            wire::envelope_wire(&op, None)
        };
        let aad = op_aad(&self.sync_id, epoch, id32);
        let env = crypto::seal_op(&key, &aad, plaintext.as_bytes()).ok()?;
        Some((epoch, env.nonce.to_vec(), env.ct))
    }

    fn ingest(&self, epoch: u64, id32: &[u8; 32], nonce: &[u8], ct: &[u8]) -> Result<()> {
        let nonce: [u8; 24] = nonce
            .try_into()
            .map_err(|_| Error::Malformed("bad nonce length".into()))?;
        let key = self.key_of(epoch)?;
        let aad = op_aad(&self.sync_id, epoch, id32);
        let env = Envelope {
            nonce,
            ct: ct.to_vec(),
        };
        let plaintext = crypto::open_op(&key, &aad, &env)?;
        let (op, proof) = wire::envelope_from_wire(&plaintext)?;
        // the seal can only have opened for the id the frame named: the
        // identity inside must be that very id, or the frame lies
        let expect = crate::device::hex(id32);
        if op.content_hash != expect {
            return Err(Error::Malformed(
                "the envelope's id and its content disagree".into(),
            ));
        }
        // the gate: signature, membership, adopt coverage — then the
        // cell's own arrival (identity recompute, clock cap)
        {
            let reg = self.registry();
            arrival::check(&reg, &self.cell, &op, proof.as_deref())?;
        }
        let was_new = self
            .cell
            .ingest_op(&op)
            .map_err(|e| Error::Malformed(format!("ingest refused the op: {e}")))?;
        // a purge that LANDED (here or arriving) queues its request for
        // the blind node: the right to be forgotten reaches its store
        if was_new
            && op.kind == engramma_core::cell::ops::OpKind::Purge
            && let Some(note) = op.note_id
        {
            crate::mailbox_net::enqueue_purge_shared(self, &note);
        }
        Ok(())
    }
}

impl SharedCell {
    fn find_op(&self, id32: &[u8; 32]) -> Option<Op> {
        let wanted = crate::device::hex(id32);
        self.cell
            .ops()
            .ok()?
            .into_iter()
            .find(|op| op.content_hash == wanted)
    }

    /// The operation of one content-hash id.
    #[must_use]
    pub fn op_of(&self, id32: &[u8; 32]) -> Option<Op> {
        self.find_op(id32)
    }

    /// Every local operation identity (the trait's `ids`, as a method).
    #[must_use]
    pub fn local_ids(&self) -> Vec<(u64, [u8; 32])> {
        <Self as SyncedCell>::ids(self)
    }

    /// Ingest one envelope addressed by its OPAQUE id (the mailbox road):
    /// the seal's AAD binds the opaque id, and the operation inside must
    /// HMAC to exactly that id under the cell's stable MAC key.
    ///
    /// # Errors
    /// [`Error::BadKey`] when this device holds no epoch-0 key; the gate's
    /// refusals; the seal's own refusal.
    pub fn ingest_opaque(
        &self,
        epoch: u64,
        opaque: &[u8; 32],
        nonce: &[u8],
        ct: &[u8],
    ) -> Result<()> {
        let nonce: [u8; 24] = nonce
            .try_into()
            .map_err(|_| Error::Malformed("bad nonce length".into()))?;
        let key = self.key_of(epoch)?;
        let aad = crate::mailbox_net::opaque_aad(&self.sync_id, epoch, opaque);
        let env = Envelope {
            nonce,
            ct: ct.to_vec(),
        };
        let plaintext = crypto::open_op(&key, &aad, &env)?;
        let (op, proof) = wire::envelope_from_wire(&plaintext)?;
        // the opaque id must be the HMAC of the identity inside
        let reg = self.registry();
        let mac_key = KeyStore::new(reg.conn())
            .key_of(&self.sync_id, 0)?
            .ok_or(Error::BadKey)?
            .mac_key();
        if EpochKey::opaque_op_id(
            &mac_key,
            unhex32(&op.content_hash)
                .ok_or_else(|| Error::Malformed("bad content hash".into()))?
                .as_slice(),
        ) != *opaque
        {
            return Err(Error::Malformed(
                "the opaque id and the operation disagree".into(),
            ));
        }
        arrival::check(&reg, &self.cell, &op, proof.as_deref())?;
        let was_new = self
            .cell
            .ingest_op(&op)
            .map_err(|e| Error::Malformed(format!("ingest refused the op: {e}")))?;
        if was_new
            && op.kind == engramma_core::cell::ops::OpKind::Purge
            && let Some(note) = op.note_id
        {
            crate::mailbox_net::enqueue_purge_shared(self, &note);
        }
        Ok(())
    }
}

/// Where a root keeps its cell files.
#[must_use]
pub fn cell_file(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    root.join("cells").join(format!("{name}.sqlite"))
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

/// Register a cell as shared in a root: the name→sync-id row and epoch 0,
/// created by the sharer (`sync share`). Idempotent — a cell already
/// shared keeps its key.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn share_cell(root: &std::path::Path, name: &str) -> Result<[u8; 32]> {
    let reg = DeviceRegistry::open(root)?;
    let existing: Option<Vec<u8>> = reg
        .conn()
        .query_row(
            "SELECT sync_id FROM shared_cells WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing
        && let Ok(sync_id) = <[u8; 32]>::try_from(id.as_slice())
    {
        return Ok(sync_id);
    }
    // epoch 0 is minted here and names its own cell: the sync id IS the
    // HMAC of this key
    let key = EpochKey::generate()?;
    let sync_id = key.cell_sync_id();
    KeyStore::new(reg.conn()).store_key(&sync_id, 0, &key)?;
    reg.conn().execute(
        "INSERT INTO shared_cells (name, sync_id) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET sync_id = ?2",
        rusqlite::params![name, sync_id],
    )?;
    Ok(sync_id)
}

/// The sync id a root knows for a cell name.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn sync_id_of(root: &std::path::Path, name: &str) -> Result<Option<[u8; 32]>> {
    let reg = DeviceRegistry::open(root)?;
    let id: Option<Vec<u8>> = reg
        .conn()
        .query_row(
            "SELECT sync_id FROM shared_cells WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    Ok(id.and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok()))
}

/// Forget a cell's registration (`sync unshare`): the row goes, the key
/// rows stay (a re-share mints a fresh epoch 0 and a fresh id anyway).
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn unshare_cell(root: &std::path::Path, name: &str) -> Result<()> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn()
        .execute("DELETE FROM shared_cells WHERE name = ?1", [name])?;
    Ok(())
}
