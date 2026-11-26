// SPDX-License-Identifier: Apache-2.0

//! v0.6: sync of a cell's change log between the owner's devices —
//! identity, pairing, end-to-end sealed operations, reconciliation over
//! iroh, and the blind mailbox node. Empty until the phase builds it: this
//! crate exists so its dependencies are in the lock before the offline work.

pub mod arrival;
pub mod crypto;
pub mod device;
pub mod keystore;
pub mod mailbox;
pub mod mailbox_net;
pub mod pairing;
pub mod pairing_net;
pub mod protocol;
pub mod registry;
pub mod relay;
pub mod serve;
pub mod service;
pub mod sharedcell;
pub mod wire;
pub mod words;

use thiserror::Error;

/// Everything the sync layer can fail with.
#[derive(Debug, Error)]
pub enum Error {
    /// Filesystem trouble.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite trouble.
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    /// A malformed or untrusted key or signature.
    #[error("bad key or signature")]
    BadKey,
    /// A signature that does not verify.
    #[error("signature does not verify")]
    BadSignature,
    /// A registry operation refused by the rules.
    #[error("registry: {0}")]
    Registry(String),
    /// The cell engine refused or failed.
    #[error("cella: {0}")]
    Cell(String),
    /// Malformed data on the wire.
    #[error("malformed: {0}")]
    Malformed(String),
    /// v0.6.4 (D4): the peer speaks other MATERIALIZATION RULES — its
    /// ALPN refused ours. Nothing was exchanged; the peer needs an
    /// upgrade (or this device does).
    #[error("peer outdated: {0}")]
    PeerOutdated(String),
    /// v0.6.19: the human on THIS side said no to the six digits of the
    /// SAS (`inviter` or `joiner` says who). A variant, not a message to
    /// read back: the service tells a refused SAS by this alone, whatever
    /// language the words speak.
    #[error("the SAS was refused by the {0}")]
    SasRefused(&'static str),
}

/// Does this connect failure look like the peer's ALPN refusing ours —
/// the only sound reading of a handshake that dies there, when the
/// transport itself answered (v0.6.4, D4)?
pub(crate) fn looks_like_alpn_refusal(e: &std::io::Error) -> bool {
    let t = e.to_string().to_ascii_lowercase();
    // rustls's own words for a server that offers none of our protocols
    // ("error 120: peer doesn't support any known protocol"), and the
    // shapes older stacks used for the same refusal
    t.contains("known protocol") || t.contains("no application protocol") || t.contains("alpn")
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
