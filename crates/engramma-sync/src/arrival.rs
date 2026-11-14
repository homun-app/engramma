// SPDX-License-Identifier: Apache-2.0

//! The ARRIVAL gate (v0.6.1, P1): what the wire-to-cell road checks before
//! an operation may land, in order — the envelope is already open by the
//! time this runs:
//!
//! 1. the operation's identity is recomputed (the cell's own `ingest_op`
//!    refuses a row that claims what it is not);
//! 2. the SIGNATURE verifies against the signing key of the `device` the
//!    operation names — the device id IS that key;
//! 3. that device is LIVE in the list — or it was removed and the
//!    operation sits inside the set its `remove` accepted;
//! 4. an UNSIGNED operation enters only under a valid `adopt`: the root of
//!    the Merkle tree over the copy's held pre-era set plus the newcomer
//!    must be exactly the root the adopt recorded;
//! 5. the clock cap (the cell's own +5 min drift refusal).
//!
//! Every refusal is an EVENT WITHOUT TEXT OF THE NOTE: the error kinds
//! carry no payload, no title, no body — only which gate refused.

use engramma_core::Cell;
use engramma_core::cell::ops::{self, MerkleStep, Op};

use crate::device::DeviceKey;
use crate::registry::DeviceRegistry;
use crate::{Error, Result};

/// Why an operation was refused at the gate. No note text, by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The signature does not verify against the named device's key.
    BadSignature,
    /// The device is in no list this copy holds.
    UnknownDevice,
    /// The device was removed and the operation is outside its accepted set.
    RemovedOutsideSet,
    /// No signature, and no `adopt` covers the pre-era set it claims to
    /// belong to.
    UnsignedUncovered,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::BadSignature => "signature does not verify",
            Self::UnknownDevice => "device unknown to the list",
            Self::RemovedOutsideSet => "removed device, operation outside the accepted set",
            Self::UnsignedUncovered => "unsigned operation not covered by an adopt",
        };
        f.write_str(s)
    }
}

/// Run the gate over one rebuilt operation. `proof` is the Merkle proof
/// the envelope carried for an unsigned operation (the `adopt` cover).
///
/// # Errors
/// [`Error::Registry`] carrying the [`Rejection`] — never note text.
pub fn check(
    registry: &DeviceRegistry,
    cell: &Cell,
    op: &Op,
    proof: Option<&[MerkleStep]>,
) -> Result<()> {
    match (&op.device, &op.sig) {
        (Some(_), Some(_)) => signed_checks(registry, cell, op, proof),
        (None, None) => unsigned_checks(cell, op, proof),
        // half an author is no author: a device without its signature, or a
        // signature naming no device, is exactly as good as a bad one
        _ => Err(Error::Registry(Rejection::BadSignature.to_string())),
    }
}

fn signed_checks(
    registry: &DeviceRegistry,
    cell: &Cell,
    op: &Op,
    proof: Option<&[MerkleStep]>,
) -> Result<()> {
    let bad = || Error::Registry(Rejection::BadSignature.to_string());
    let device = op.device.as_deref().unwrap_or_default();
    // the author enters the identity (R1, v0.6.1): the signing device IS
    // the HLC node, and both live inside the content the signature covers —
    // a disagreement is a forged authorship, whatever key signed it
    if op.hlc.node != device {
        return Err(bad());
    }
    let mut key = [0_u8; 32];
    if !hex_into(device, &mut key) {
        return Err(bad());
    }
    let sig_bytes = unb64(op.sig.as_deref().unwrap_or_default())?;
    DeviceKey::verify(&key, op.content_hash.as_bytes(), &sig_bytes).map_err(|_| bad())?;
    // live, or removed with the operation inside the accepted set
    let live = registry
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM members WHERE device = ?1 AND removed_at IS NULL",
            [device],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0);
    if live > 0 {
        return Ok(());
    }
    // the HLC does not decide this (R1): the author chooses it, and dating
    // an operation before its removal was the stolen key's open door. The
    // operative set is the Merkle root the remove carries — an operation
    // of the removed device enters only if already held (a duplicate) or
    // with the PROOF of membership toward one of the recorded roots (the
    // union of concurrent removes)
    let removes = registry.accepted_sets_of(device)?;
    if removes.is_empty() {
        return Err(Error::Registry(Rejection::UnknownDevice.to_string()));
    }
    if held_here(cell, &op.content_hash) {
        return Ok(());
    }
    if let Some(path) = proof
        && let Some(walked) = ops::merkle_root_with(&op.content_hash, path)
        && removes.iter().any(|(root, _)| *root == walked)
    {
        return Ok(());
    }
    Err(Error::Registry(Rejection::RemovedOutsideSet.to_string()))
}

/// Is this operation already in the copy? A point query — the index of
/// `content_hash` answers it without reading the log.
fn held_here(cell: &Cell, content_hash: &str) -> bool {
    cell.holds_content_hash(content_hash).unwrap_or(false)
}

/// An unsigned operation enters only under a valid `adopt`: the sharer
/// signed the Merkle root of every operation written before the sync era,
/// and the operation arrives with the PROOF that it is a leaf of that very
/// tree. The receiver — which may hold none of the other leaves yet —
/// walks the proof and compares the root with the one the adopt recorded.
fn unsigned_checks(cell: &Cell, op: &Op, proof: Option<&[MerkleStep]>) -> Result<()> {
    let root: Option<String> = cell
        .debug_column("SELECT value FROM meta WHERE key = 'adopted_root'")
        .unwrap_or_default()
        .first()
        .cloned();
    let Some(adopted) = root else {
        return Err(Error::Registry(Rejection::UnsignedUncovered.to_string()));
    };
    // already held: the duplicate is the ingest's own business, not a gate
    // refusal
    if held_here(cell, &op.content_hash) {
        return Ok(());
    }
    let Some(path) = proof else {
        return Err(Error::Registry(Rejection::UnsignedUncovered.to_string()));
    };
    match ops::merkle_root_with(&op.content_hash, path) {
        Some(walked) if walked == adopted => Ok(()),
        _ => Err(Error::Registry(Rejection::UnsignedUncovered.to_string())),
    }
}

/// Build the adopt proof for one unsigned operation of this cell (the
/// sender's side of the cover): the path from the operation's hash to the
/// root of the tree over the cell's whole pre-era set.
///
/// # Errors
/// [`Error::Registry`] when the operation is not part of that set (it was
/// written after the adopt, or belongs to no era at all).
pub fn adopt_proof(cell: &Cell, op: &Op) -> Result<Vec<MerkleStep>> {
    let hashes = adopt_hashes(cell);
    ops::merkle_prove(&mut hashes.clone(), &op.content_hash)
        .map_err(|e| Error::Registry(Rejection::UnsignedUncovered.with(&e)))
}

/// The content hashes of the cell's pre-era set — the leaves of the adopt
/// tree. One whole-log read; the session's cache pays it once.
pub fn adopt_hashes(cell: &Cell) -> Vec<String> {
    cell.debug_column(
        "SELECT o.content_hash FROM ops o
         WHERE o.sig IS NULL AND o.kind != 'adopt'",
    )
    .unwrap_or_default()
}

/// A Merkle tree over a FIXED set of content hashes, built ONCE per
/// session and kept in memory (R3): the root and every leaf's proof come
/// out of the cached levels — no re-read of the log, no rebuild of the
/// tree, per envelope. Byte order over the hashes equals the hex-string
/// order of [`ops::merkle_root`], and the pairing rule (the odd trailing
/// leaf duplicated as its own right operand) is the same one
/// [`ops::merkle_prove`] walks.
pub struct MerkleIndex {
    /// The sorted leaves.
    leaves: Vec<[u8; 32]>,
    /// levels[0] is the leaves; the last level is the single root.
    levels: Vec<Vec<[u8; 32]>>,
}

impl MerkleIndex {
    /// Build over the set (the hashes may come in any order).
    #[must_use]
    pub fn new(hashes: Vec<String>) -> Self {
        let mut leaves: Vec<[u8; 32]> = hashes.iter().filter_map(|h| hash_bytes32(h)).collect();
        leaves.sort_unstable();
        let mut levels = vec![leaves.clone()];
        while levels.last().is_some_and(|level| level.len() > 1) {
            let level = levels.last().expect(/* just checked */ "the level");
            let mut next = Vec::with_capacity(level.len().div_ceil(2));
            let mut i = 0;
            while i < level.len() {
                let a = level[i];
                let b = if i + 1 < level.len() { level[i + 1] } else { level[i] };
                let mut h = blake3::Hasher::new();
                h.update(&a);
                h.update(&b);
                let mut out = [0_u8; 32];
                out.copy_from_slice(h.finalize().as_bytes());
                next.push(out);
                i += 2;
            }
            levels.push(next);
        }
        Self { leaves, levels }
    }

    /// The root of the tree — exactly what [`ops::merkle_root`] computes
    /// over the same set.
    #[must_use]
    pub fn root(&self) -> String {
        match (self.levels.last(), self.leaves.is_empty()) {
            (Some(top), false) => blake3::Hash::from_bytes(top[0]).to_hex().to_string(),
            // the empty set's root, as `merkle_root` defines it
            _ => blake3::Hasher::new().finalize().to_hex().to_string(),
        }
    }

    /// The proof that one content hash is a leaf of the tree, with the
    /// same shape [`ops::merkle_prove`] produces.
    #[must_use]
    pub fn prove(&self, target: &str) -> Option<Vec<MerkleStep>> {
        let leaf = hash_bytes32(target)?;
        let mut idx = self.leaves.partition_point(|x| *x < leaf);
        if self.leaves.get(idx) != Some(&leaf) {
            return None; // not a leaf of this set
        }
        let mut path = Vec::new();
        for level in &self.levels[..self.levels.len() - 1] {
            let sibling_idx = if idx % 2 == 0 {
                (idx + 1).min(level.len() - 1)
            } else {
                idx - 1
            };
            path.push(MerkleStep {
                sibling: level[sibling_idx],
                right: sibling_idx >= idx,
            });
            idx /= 2;
        }
        Some(path)
    }
}

fn hash_bytes32(hex: &str) -> Option<[u8; 32]> {
    let parsed = blake3::Hash::from_hex(hex).ok()?;
    let mut out = [0_u8; 32];
    out.copy_from_slice(parsed.as_bytes());
    Some(out)
}

#[cfg(test)]
mod merkle_index_tests {
    use super::MerkleIndex;
    use engramma_core::cell::ops::{merkle_prove, merkle_root, merkle_root_with};

    /// The cached tree IS the root-and-proof pair the core computes: same
    /// root, same path, for every set size around the powers of two.
    #[test]
    fn the_cached_tree_equals_merkle_root_and_prove() {
        for n in [0_usize, 1, 2, 3, 5, 8, 9, 16, 17, 100, 1000] {
            let hashes: Vec<String> = (0..n)
                .map(|i| blake3::hash(&(i as u64).to_le_bytes()).to_hex().to_string())
                .collect();
            let index = MerkleIndex::new(hashes.clone());
            let mut reference = hashes.clone();
            let root = merkle_root(&mut reference);
            assert_eq!(index.root(), root, "n={n}: the root must be the same");
            for target in &hashes {
                let fast = index.prove(target);
                let slow = merkle_prove(&mut hashes.clone(), target);
                assert_eq!(fast.is_some(), slow.is_ok(), "n={n}: membership agrees");
                let (fast, slow) = (fast.unwrap_or_default(), slow.unwrap_or_default());
                assert_eq!(fast.len(), slow.len(), "n={n}: the path length");
                for (f, s) in fast.iter().zip(slow.iter()) {
                    assert_eq!(f.sibling, s.sibling, "n={n}: the sibling");
                    assert_eq!(f.right, s.right, "n={n}: the side");
                }
                assert_eq!(
                    merkle_root_with(target, &fast).as_deref(),
                    Some(root.as_str()),
                    "n={n}: the fast proof walks to the root"
                );
            }
            // and an outsider is no leaf of the set
            let outsider = blake3::hash(&[255_u8; 8]).to_hex().to_string();
            assert!(index.prove(&outsider).is_none(), "n={n}: the outsider");
        }
    }
}

impl Rejection {
    /// A rejection kind with a short, note-free detail attached.
    fn with(self, e: &engramma_core::CellError) -> String {
        format!("{self}: {e}")
    }
}

fn hex_into(hex: &str, out: &mut [u8; 32]) -> bool {
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

fn unb64(text: &str) -> Result<Vec<u8>> {
    use base64ct::{Base64UrlUnpadded, Encoding};
    Base64UrlUnpadded::decode_vec(text).map_err(|e| Error::Malformed(format!("bad signature: {e}")))
}

/// The device signer the CLI and the serve loop arm the cell with (v0.6.1,
/// P1): every LOCAL operation from here on carries the device id and the
/// Ed25519 signature over its `content_hash`.
pub struct DeviceSigner {
    device: String,
    key: DeviceKey,
}

impl DeviceSigner {
    /// Arm with the device's key.
    #[must_use]
    pub fn new(key: &DeviceKey) -> Self {
        Self {
            device: key.device_id(),
            key: DeviceKey::from_seed(key.clone_seed()),
        }
    }
}

impl ops::Signer for DeviceSigner {
    fn device_id(&self) -> String {
        self.device.clone()
    }
    fn sign(&self, msg: &[u8]) -> String {
        use base64ct::{Base64UrlUnpadded, Encoding};
        Base64UrlUnpadded::encode_string(&self.key.sign(msg))
    }
    fn verify(device: &str, msg: &[u8], sig: &str) -> bool
    where
        Self: Sized,
    {
        use base64ct::{Base64UrlUnpadded, Encoding};
        let mut key = [0_u8; 32];
        let Ok(sig_bytes) = Base64UrlUnpadded::decode_vec(sig) else {
            return false;
        };
        hex_into(device, &mut key) && DeviceKey::verify(&key, msg, &sig_bytes).is_ok()
    }
}
