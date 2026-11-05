// SPDX-License-Identifier: Apache-2.0

//! Hybrid logical clock (Kulkarni et al.). A few dozen lines, as promised.
//!
//! An HLC timestamp is `(physical_ms, counter, node_id)`. It orders events
//! across machines with drifting clocks: the physical part follows the wall
//! clock when it moves forward, the counter ticks when it does not. Comparison
//! is structural (physical, then counter, then node id), so two timestamps from
//! the same node never compare equal and the order is a total order.

use std::cmp::Ordering;

/// A hybrid logical clock timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hlc {
    /// Wall-clock milliseconds at the Unix epoch.
    pub physical_ms: u64,
    /// Tick within the same millisecond (or across a clock going backwards).
    pub counter: u32,
    /// Node identity (hex string).
    pub node: String,
}

impl Hlc {
    /// The zero timestamp for a node.
    pub fn zero(node: impl Into<String>) -> Self {
        Self {
            physical_ms: 0,
            counter: 0,
            node: node.into(),
        }
    }

    /// Advance the clock with a physical-time reading and return the new
    /// timestamp (the receiver is updated in place).
    ///
    /// If the wall clock went backwards (or did not move), the counter ticks so
    /// the result is still strictly greater than the previous one.
    pub fn tick(&mut self, now_ms: u64) -> Hlc {
        if now_ms > self.physical_ms {
            self.physical_ms = now_ms;
            self.counter = 0;
        } else {
            self.counter = self.counter.saturating_add(1);
        }
        self.clone()
    }

    /// Merge a remote timestamp observed from another node (receive side).
    /// The result is greater than both the local clock and the remote one.
    pub fn observe(&mut self, other: &Hlc) -> Hlc {
        self.physical_ms = self.physical_ms.max(other.physical_ms);
        self.counter = self.counter.max(other.counter).saturating_add(1);
        self.node = self.node.clone();
        self.clone()
    }

    /// Parse the wire form produced by [`Hlc::to_string`].
    ///
    /// # Errors
    /// [`CellError::Invalid`] when the string is not a well-formed HLC.
    pub fn parse(s: &str) -> Result<Self, crate::error::CellError> {
        let bad = || crate::error::CellError::Invalid(format!("malformed HLC `{s}`"));
        let (head, node) = s.rsplit_once('.').ok_or_else(bad)?;
        if head.len() != 20 || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let physical_ms = u64::from_str_radix(&head[..12], 16).map_err(|_| bad())?;
        let counter = u32::from_str_radix(&head[12..], 16).map_err(|_| bad())?;
        if node.is_empty() || !node.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(bad());
        }
        Ok(Self {
            physical_ms,
            counter,
            node: node.to_string(),
        })
    }
}

impl PartialOrd for Hlc {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hlc {
    fn cmp(&self, other: &Self) -> Ordering {
        self.physical_ms
            .cmp(&other.physical_ms)
            .then(self.counter.cmp(&other.counter))
            .then(self.node.cmp(&other.node))
    }
}

impl std::fmt::Display for Hlc {
    /// Wire form: `"%012x%08x.%s"` — physical, counter, node. Fixed-width
    /// hex prefixes keep the string order equal to the tuple order **within a
    /// pair of nodes with equal-length ids**; structural [`Ord`] is the
    /// authoritative comparison.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:012x}{:08x}.{}", self.physical_ms, self.counter, self.node)
    }
}

/// A running HLC bound to wall time.
pub struct HlcClock {
    now: Hlc,
    now_ms: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for HlcClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HlcClock").field("now", &self.now).finish_non_exhaustive()
    }
}

impl HlcClock {
    /// Clock for a node id, reading Unix milliseconds from a closure
    /// (injectable for tests).
    pub fn new(node: impl Into<String>, now_ms: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            now: Hlc::zero(node),
            now_ms: Box::new(now_ms),
        }
    }

    /// Wall clock in Unix milliseconds.
    pub fn wall_ms(&self) -> u64 {
        (self.now_ms)()
    }

    /// Produce the next timestamp.
    pub fn tick(&mut self) -> Hlc {
        self.now.tick((self.now_ms)())
    }

    /// Observe a remote timestamp.
    pub fn observe(&mut self, other: &Hlc) -> Hlc {
        self.now.observe(other)
    }

    /// Current value without advancing.
    pub fn peek(&self) -> &Hlc {
        &self.now
    }

    /// Restore a clock state (after reopening a cell, the stored HLC must not
    /// go backwards).
    pub fn restore(&mut self, h: &Hlc) {
        if *h > self.now {
            self.now = h.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_form_round_trips() {
        let h = Hlc {
            physical_ms: 1_790_000_000_000,
            counter: 42,
            node: "abc123def456".into(),
        };
        let s = h.to_string();
        assert_eq!(Hlc::parse(&s).unwrap(/* test-only invariant */), h);
        assert!(Hlc::parse("nope").is_err());
    }

    #[test]
    fn clock_going_backwards_still_ticks_forward() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        let t = Arc::new(AtomicU64::new(10_000));
        let mut clock = {
            let t = Arc::clone(&t);
            HlcClock::new("n1", move || t.load(Ordering::Relaxed))
        };
        let a = clock.tick();
        t.store(9_000, Ordering::Relaxed); // wall clock jumps back
        let b = clock.tick();
        assert!(b > a);
        let c = clock.tick();
        assert!(c > b);
        t.store(20_000, Ordering::Relaxed); // and forward again
        let d = clock.tick();
        assert!(d > c);
        assert_eq!(d.physical_ms, 20_000);
        assert_eq!(d.counter, 0);
    }

    #[test]
    fn observe_beats_both_sides() {
        let mut local = HlcClock::new("n1", || 100);
        local.tick();
        let remote = Hlc {
            physical_ms: 500,
            counter: 7,
            node: "n2".into(),
        };
        let merged = local.observe(&remote);
        assert!(merged > remote);
        assert_eq!(merged.physical_ms, 500);
    }
}
