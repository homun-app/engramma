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
    let mut hashes: Vec<String> = cell
        .debug_column(
            "SELECT o.content_hash FROM ops o
             WHERE o.sig IS NULL AND o.kind != 'adopt'",
        )
        .unwrap_or_default();
    ops::merkle_prove(&mut hashes, &op.content_hash)
        .map_err(|e| Error::Registry(Rejection::UnsignedUncovered.with(&e)))
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
