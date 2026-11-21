// SPDX-License-Identifier: Apache-2.0

//! v0.6.4 (D4): the rules version rides the ALPN. A peer that speaks
//! other materialization rules is refused BEFORE a single operation
//! moves, and the dialer says it plainly: `PeerOutdated`, the device to
//! upgrade — `PEER_OUTDATED` on the JS side.
//!
//! v0.6.11 (K1): the rules are 6 now — the purge takes the purged note,
//! its family and every REFERENCE to it, never another note's own
//! content (the v0.6.10 rule let the purge of an old note take away the
//! new one a supersede had created). A derivation change: rules 5
//! against rules 6 derive different states from the same operations,
//! and must refuse each other.

use engramma_sync::device::DeviceKey;
use engramma_sync::protocol::{self, PROTOCOL_VERSION, connect};
use engramma_sync::relay::RelayChoice;
use engramma_sync::serve::PeerAddr;

/// The old rules' ALPN — what a v0.6.10 device still speaks (its purge
/// dominates whole operations: rules 5 against rules 6 derive different
/// states from the same operations, and must refuse each other).
const V5_ALPN: &[u8] = b"engramma/sync/5";

fn unhex32(s: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (chunk[1] as char).to_digit(16).unwrap_or(0) as u8;
        out[i] = (hi << 4) | lo;
    }
    out
}

/// Two endpoints on loopback, one per rules version: the v6 dialer
/// against a v5 answerer. The handshake dies at the ALPN, the error is
/// `PeerOutdated` naming the device, and not one frame moved.
#[tokio::test]
async fn a_peer_of_the_old_rules_is_refused_before_any_exchange() {
    assert_eq!(
        protocol::ALPN,
        b"engramma/sync/6",
        "this build speaks rules 6"
    );
    assert_eq!(PROTOCOL_VERSION, 6);

    let key_old = DeviceKey::from_seed([7_u8; 32]);
    let key_new = DeviceKey::from_seed([9_u8; 32]);
    // the OLD rules' endpoint: the ALPN of the v0.6.10 era, nothing else
    let ep_old = engramma_sync::relay::bind(
        &key_old,
        "127.0.0.1:0",
        V5_ALPN,
        &RelayChoice::Disabled,
    )
    .await
    .unwrap(/* test-only */);
    let ep_new = engramma_sync::relay::bind(
        &key_new,
        "127.0.0.1:0",
        protocol::ALPN,
        &RelayChoice::Disabled,
    )
    .await
    .unwrap(/* test-only */);

    // the OLD side answers its own era's handshakes: the refusal below
    // is about the rules, not about a dead endpoint
    let accept_old = {
        let ep = ep_old.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                // the handshake itself dies on the ALPN mismatch — the
                // error lands on BOTH sides
                let _ = incoming.await;
            }
        })
    };
    let old_addr = ep_old.addr();

    // the protocol's own connect: the handshake dies at the ALPN
    let refused = connect(&ep_new, old_addr.clone()).await;
    assert!(refused.is_err(), "a v5 answerer refuses a v6 dialer");

    // the service's dial says WHO: PeerOutdated names the device
    let peer = PeerAddr {
        device: "il-mac-del-2026".into(),
        node_id: *old_addr.id.as_bytes(),
        relay: String::new(),
        addrs: old_addr
            .addrs
            .iter()
            .map(|sa| sa.to_string())
            .collect::<Vec<_>>(),
    };
    let err = peer.dial(&ep_new).await.unwrap_err(/* test-only: must fail */);
    match &err {
        engramma_sync::Error::PeerOutdated(who) => {
            assert_eq!(who, "il-mac-del-2026", "the device to upgrade is named");
        }
        other => panic!("the ALPN refusal must be PeerOutdated, got {other:?}"),
    }
    // and the same text the JS side will see
    assert!(err.to_string().contains("peer outdated"));
    accept_old.abort();
}

/// A v6 answerer receives a v6 dialer and the session proceeds past the
/// hello: the version check holds when the rules agree (the full
/// exchange is e2e's own; here the handshake itself is the point).
#[tokio::test]
async fn the_same_rules_still_shake_hands() {
    let key_a = DeviceKey::from_seed([1_u8; 32]);
    let key_b = DeviceKey::from_seed([2_u8; 32]);
    let ep_a = engramma_sync::relay::bind(
        &key_a,
        "127.0.0.1:0",
        protocol::ALPN,
        &RelayChoice::Disabled,
    )
    .await
    .unwrap(/* test-only */);
    let ep_b = engramma_sync::relay::bind(
        &key_b,
        "127.0.0.1:0",
        protocol::ALPN,
        &RelayChoice::Disabled,
    )
    .await
    .unwrap(/* test-only */);
    let addr_a = ep_a.addr();
    let accept_a = {
        let ep = ep_a.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                // hold the connection; the exchange is e2e's own business
                let _ = incoming.await;
            }
        })
    };
    // B connects to A: the handshake succeeds — same rules
    let conn = connect(&ep_b, addr_a).await;
    assert!(conn.is_ok(), "two v6 endpoints shake hands: {conn:?}");
    let _ = unhex32("00");
    accept_a.abort();
}
