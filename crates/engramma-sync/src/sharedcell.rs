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
    root: std::path::PathBuf,
    /// How many WHOLE-LOG reads the adapter's sync roads have performed
    /// (a test-only counter: the regression test of the bootstrap wants
    /// them LINEAR — a fixed number per envelope, never one per op).
    whole_log_reads: std::sync::atomic::AtomicU64,
    /// The adopt tree over the pre-era set, built ONCE per session (R3):
    /// every unsigned envelope's proof comes out of it without re-reading
    /// the log or rebuilding the tree.
    adopt_tree: std::sync::OnceLock<arrival::MerkleIndex>,
    /// The membership trees of the REMOVED devices' sets (R1), one per
    /// device, built once per session over the whole tree of cells.
    removal_trees: std::sync::Mutex<
        std::collections::HashMap<String, Option<std::sync::Arc<arrival::MerkleIndex>>>,
    >,
}

impl SharedCell {
    /// Bind an opened cell to a sync id (the name→id row is the caller's
    /// to write: `share_cell` for the sharer, the pairing for the joiner).
    /// The root names the tree this cell lives in — the membership proofs
    /// of a removed device's operations are built over the whole tree, as
    /// its `remove` computed them.
    #[must_use]
    pub fn new(
        cell: Cell,
        keys: DeviceRegistry,
        sync_id: [u8; 32],
        root: &std::path::Path,
    ) -> Self {
        Self {
            cell,
            keys: std::sync::Mutex::new(keys),
            sync_id,
            root: root.to_path_buf(),
            whole_log_reads: std::sync::atomic::AtomicU64::new(0),
            adopt_tree: std::sync::OnceLock::new(),
            removal_trees: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The test-only counter: how many whole-log reads the sync roads have
    /// performed on this adapter so far (`Cell::ops()` or a scan of the
    /// `ops` table). Point queries do not count.
    #[must_use]
    pub fn log_reads(&self) -> u64 {
        self.whole_log_reads
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn count_log_read(&self) {
        self.whole_log_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                root: root.to_path_buf(),
                whole_log_reads: std::sync::atomic::AtomicU64::new(0),
                adopt_tree: std::sync::OnceLock::new(),
                removal_trees: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        // only `hlc` and `content_hash` — no payload is deserialized for
        // an identity list (R3: the whole log was parsed per bootstrap)
        self.count_log_read();
        self.cell
            .debug_column("SELECT hlc || ' ' || content_hash FROM ops")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let (hlc, content_hash) = row.split_once(' ')?;
                // the physical part is the fixed-width 12-hex prefix
                let physical_ms = u64::from_str_radix(&hlc[..12], 16).ok()?;
                let id = unhex32(content_hash)?;
                Some((physical_ms, id))
            })
            .collect()
    }

    fn envelope_of(&self, id32: &[u8; 32]) -> Option<(u64, Vec<u8>, Vec<u8>)> {
        let op = match self.find_op(id32) {
            Some(op) => op,
            None => {
                self.record_unsent(
                    &crate::device::hex(id32)[..16],
                    "?",
                    "not found in this copy",
                );
                return None;
            }
        };
        let epoch = match self.current_epoch() {
            Ok(epoch) => epoch,
            Err(_) => {
                self.record_unsent(&op.op_id.to_string()[..16], op.kind.as_str(), "no epoch");
                return None;
            }
        };
        let Ok(key) = self.key_of(epoch) else {
            self.record_unsent(
                &op.op_id.to_string()[..16],
                op.kind.as_str(),
                "epoch key missing",
            );
            return None;
        };
        // an unsigned operation travels with its adopt proof; a SIGNED
        // operation of a REMOVED device travels with its membership proof
        // (R1) — whoever sends it must be able to prove it sits inside a
        // root its remove recorded; everything else travels alone. A
        // failure is NEVER silent (R4): the operation stays, the event
        // names it — id short, kind, reason; no note text by construction.
        let plaintext = if op.sig.is_none() && op.kind != engramma_core::cell::ops::OpKind::Adopt {
            let proof = match self.adopt_tree().prove(&op.content_hash) {
                Some(proof) => proof,
                None => {
                    self.record_unsent(
                        &op.op_id.to_string()[..16],
                        op.kind.as_str(),
                        "unsigned and outside the adopt tree",
                    );
                    return None;
                }
            };
            wire::envelope_wire(&op, Some(&proof))
        } else if let Some(device) = op.device.as_deref()
            && op.sig.is_some()
            && self.device_is_removed(device)
        {
            let proof = match self.removal_proof(device, &op.content_hash) {
                Some(proof) => proof,
                None => {
                    self.record_unsent(
                        &op.op_id.to_string()[..16],
                        op.kind.as_str(),
                        "of a removed device, without the proof of the set",
                    );
                    return None;
                }
            };
            wire::envelope_wire(&op, Some(&proof))
        } else {
            wire::envelope_wire(&op, None)
        };
        let aad = op_aad(&self.sync_id, epoch, id32);
        let env = match crypto::seal_op(&key, &aad, plaintext.as_bytes()) {
            Ok(env) => env,
            Err(_) => {
                self.record_unsent(
                    &op.op_id.to_string()[..16],
                    op.kind.as_str(),
                    "sealing failed",
                );
                return None;
            }
        };
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
        self.cell.op_of_hash(&wanted).ok().flatten()
    }

    /// R4: name an operation that could not leave — an event without
    /// note text (op id SHORT, kind, reason) in the root's own store. The
    /// pass clears what it re-tries, so `sync status` shows what still
    /// cannot leave.
    fn record_unsent(&self, op_id_short: &str, kind: &str, reason: &str) {
        let reg = self.registry();
        let cell = crate::serve::cell_name(&self.root, &self.sync_id)
            .unwrap_or_else(|_| crate::device::hex(&self.sync_id)[..8].to_string());
        let at = time::OffsetDateTime::now_utc()
            .replace_millisecond(0)
            .unwrap_or(time::OffsetDateTime::now_utc())
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let _ = reg.conn().execute(
            "INSERT INTO sync_events (at, cell, op_id, kind, reason) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![at, cell, op_id_short, kind, reason],
        );
    }

    /// The adopt tree, built on first use and kept for the session (R3).
    fn adopt_tree(&self) -> &arrival::MerkleIndex {
        self.adopt_tree.get_or_init(|| {
            self.count_log_read(); // one whole-log read, once per session
            arrival::MerkleIndex::new(arrival::adopt_hashes(&self.cell))
        })
    }

    /// Is this device REMOVED in the list this root holds? (Live devices
    /// and strangers answer false: their operations carry no proof.)
    fn device_is_removed(&self, device: &str) -> bool {
        let reg = self.registry();
        let live: i64 = reg
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM members WHERE device = ?1 AND removed_at IS NULL",
                [device],
                |r| r.get(0),
            )
            .unwrap_or(0);
        live == 0 && reg.accepted_sets_of(device).is_ok_and(|r| !r.is_empty())
    }

    /// The membership proof of one operation of a REMOVED device (R1): the
    /// path from its `content_hash` to a root one of the device's removes
    /// recorded. The set is the removed device's operations across the
    /// WHOLE tree — exactly what its `remove` computed. A copy whose set
    /// rebuilds no recorded root cannot prove, and does not send.
    fn removal_proof(
        &self,
        device: &str,
        target_hash: &str,
    ) -> Option<Vec<engramma_core::cell::ops::MerkleStep>> {
        let removes = self.registry().accepted_sets_of(device).ok()?;
        if removes.is_empty() {
            return None;
        }
        // the tree of this device's set, built once per session and kept
        // (R3): every envelope's proof comes out of the cached levels
        let cached = {
            let mut trees = self
                .removal_trees
                .lock()
                .expect(/* never poisoned */ "trees");
            trees
                .entry(device.to_string())
                .or_insert_with(|| {
                    let mut hashes: Vec<String> = Vec::new();
                    for shared in SharedCell::all_of(&self.root).ok()? {
                        self.count_log_read(); // one scan per cell, once
                        hashes.extend(
                            shared
                                .cell()
                                .debug_column(&format!(
                                    "SELECT o.content_hash FROM ops o WHERE o.device = '{}'",
                                    device.replace('\'', "''")
                                ))
                                .ok()?,
                        );
                    }
                    Some(std::sync::Arc::new(arrival::MerkleIndex::new(hashes)))
                })
                .clone()
        };
        let tree = cached?;
        if !removes.iter().any(|(recorded, _)| *recorded == tree.root()) {
            return None; // our set is not any remove's set: no proof exists
        }
        tree.prove(target_hash)
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

/// One unsent event, as `sync status` shows it: `(cell, at, op_id short,
/// kind, reason)` — no note text by construction.
pub type UnsentEvent = (String, String, String, String, String);

/// R4: the unsent events a root still holds, no note text by construction.
/// `sync status` shows them until a pass can send what they name.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn unsent_events(root: &std::path::Path) -> Result<Vec<UnsentEvent>> {
    let reg = DeviceRegistry::open(root)?;
    let mut stmt = reg
        .conn()
        .prepare("SELECT cell, at, op_id, kind, reason FROM sync_events ORDER BY seq")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// R4: forget one cell's unsent events — what a pass does when it starts,
/// before the envelopes that fail are named again.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn clear_unsent(root: &std::path::Path, cell: &str) -> Result<()> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn()
        .execute("DELETE FROM sync_events WHERE cell = ?1", [cell])?;
    Ok(())
}
