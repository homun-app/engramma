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
pub mod serve;
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
    /// Malformed data on the wire.
    #[error("malformed: {0}")]
    Malformed(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
