//! Node.js (N-API) binding of the Engramma engine — v0.4 (P2.7).
//!
//! The base commit carries only the skeleton: the dependencies are in the
//! lock (offline builds) and `@napi-rs/cli` is installed in this crate's
//! `node_modules`. The contract to implement is in the v0.4 spec, point 7.

use napi_derive::napi;

/// The engine version, so the host can log what it loaded.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
