// SPDX-License-Identifier: Apache-2.0

//! ULID helpers and per-process node identity.
//!
//! IDs are generated **when an operation is created** and travel inside the
//! operation payload: replaying a log never invents new IDs.

use std::sync::{Mutex, OnceLock};

use ulid::Ulid;

/// A per-process monotonic ULID generator (monotonic within the process, which
/// is all a single writer per cell needs; cross-machine ordering is the HLC's
/// job).
#[derive(Debug)]
pub struct Ids {
    generator: Mutex<ulid::Generator>,
}

impl Ids {
    /// A fresh generator.
    pub fn new() -> Self {
        Self {
            generator: Mutex::new(ulid::Generator::new()),
        }
    }

    /// Generate the next ULID.
    ///
    /// # Errors
    /// Only when the generator's random space overflows (practically never:
    /// 80 random bits per millisecond).
    pub fn next(&self) -> Result<Ulid, crate::error::CellError> {
        self.generator
            .lock()
            .map_err(|_| crate::error::CellError::Db("id generator mutex poisoned".into()))?
            .generate()
            .map_err(|_| {
                crate::error::CellError::Db("ulid generator overflow (80 bits exhausted)".into())
            })
    }
}

impl Default for Ids {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide shared generator.
fn shared() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(Ids::new)
}

/// Generate a ULID with the shared process generator.
///
/// # Errors
/// See [`Ids::next`].
pub fn new_ulid() -> Result<Ulid, crate::error::CellError> {
    shared().next()
}

/// A short identifier for a node (used as the HLC node id). Stable within the
/// process, derived from OS randomness sources available to `ulid`.
pub fn node_id() -> &'static str {
    static NODE: OnceLock<String> = OnceLock::new();
    NODE.get_or_init(|| {
        // Two random ULIDs hashed together: no timing information, different on
        // every process start, stable for the process lifetime.
        let a = shared().next().map(|u| u.to_string()).unwrap_or_default();
        let b = shared().next().map(|u| u.to_string()).unwrap_or_default();
        let h = blake3::hash(format!("{a}{b}").as_bytes());
        h.to_hex()[..12].to_string()
    })
}

/// The last 8 characters of a ULID (its random part), used wherever a short
/// id is shown: the first 8 are the timestamp and collide in bulk imports
/// (every ULID minted within ~256 ms shares them).
pub fn id8(u: &Ulid) -> String {
    let s = u.to_string();
    s[s.len() - 8..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulids_are_monotonic_within_a_process() {
        let ids = Ids::new();
        let mut prev = Ulid::nil();
        for _ in 0..1000 {
            let u = ids.next().unwrap(/* test-only invariant */);
            assert!(u > prev);
            prev = u;
        }
    }

    #[test]
    fn id8_is_the_random_suffix() {
        // the first 8 chars are the timestamp (they collide in bulk imports:
        // ~256 ms of ULIDs share them); the last 8 are the random part
        let u = Ulid::from_string("01GD0000000000000000000110").unwrap(/* test-only */);
        assert_eq!(id8(&u), "00000110");
        let a = new_ulid().unwrap(/* test-only */);
        let b = new_ulid().unwrap(/* test-only */);
        assert_ne!(id8(&a), id8(&b), "same-millisecond ids must differ in id8");
    }

    #[test]
    fn node_ids_are_twelve_hex_chars() {
        assert_eq!(node_id().len(), 12);
        assert!(node_id().bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
