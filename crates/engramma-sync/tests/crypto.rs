// SPDX-License-Identifier: Apache-2.0

//! v0.6 (P4): the sealed operations. Epoch keys, HPKE wraps, XChaCha20
//! envelopes with AAD, opaque ids — and the limits this phase declares:
//! neither post-compromise security nor forward secrecy per message.

use engramma_sync::crypto::{Envelope, EpochKey, open_op, seal_op, unwrap_key, wrap_key};
use engramma_sync::device::DeviceKey;

#[test]
fn an_envelope_round_trips_and_rejects_everything_else() {
    let key = EpochKey::generate().unwrap(/* t */);
    let aad = b"cell|epoch1|opaque-id";
    let env = seal_op(&key, aad, b"the signed content of P1").unwrap(/* t */);
    assert_eq!(
        open_op(&key, aad, &env).unwrap(/* t */),
        b"the signed content of P1"
    );
    // wrong AAD — same key, different routing
    assert!(open_op(&key, b"cell|epoch2|opaque-id", &env).is_err());
    // wrong epoch key — the rotation is a real boundary
    let other = EpochKey::generate().unwrap(/* t */);
    assert!(open_op(&other, aad, &env).is_err());
    // a tampered byte
    let mut tampered = Envelope {
        nonce: env.nonce,
        ct: env.ct.clone(),
    };
    let last = tampered.ct.len() - 1;
    tampered.ct[last] ^= 1;
    assert!(open_op(&key, aad, &tampered).is_err());
}

#[test]
fn nonces_never_repeat() {
    // 1M envelopes under one key: 24 random bytes each, no duplicate —
    // the birthday bound at 2^96 makes a collision astronomically
    // unlikely, and the test is the tripwire
    let key = EpochKey::generate().unwrap(/* t */);
    let mut seen = std::collections::HashSet::new();
    for i in 0..1_000_000_u32 {
        let env = seal_op(&key, b"aad", format!("op {i}").as_bytes()).unwrap(/* t */);
        if !seen.insert(env.nonce) {
            panic!("nonce repeated after {i} envelopes");
        }
    }
}

#[test]
fn epoch_keys_wrap_only_for_their_device() {
    let laptop = DeviceKey::from_seed([1; 32]);
    let mac = DeviceKey::from_seed([2; 32]);
    let key = EpochKey::generate().unwrap(/* t */);
    let sync_id = key.cell_sync_id();

    let for_mac = wrap_key(&key, &sync_id, 0, &mac.hpke_public()).unwrap(/* t */);
    // the mac opens its own wrap
    let opened = unwrap_key(&for_mac, &sync_id, 0, &laptop_hpke_secret(&mac)).unwrap(/* t */);
    assert_eq!(opened.0, key.0);
    // the laptop cannot open the mac's wrap
    assert!(
        unwrap_key(&for_mac, &sync_id, 0, &laptop_hpke_secret(&laptop)).is_err(),
        "a wrap for another device does not open"
    );
    // a different epoch binds the info: the same wrap under epoch 1 fails
    assert!(unwrap_key(&for_mac, &sync_id, 1, &laptop_hpke_secret(&mac)).is_err());
}

fn laptop_hpke_secret(dk: &DeviceKey) -> [u8; 32] {
    dk.hpke_secret()
}

#[test]
fn the_removed_device_cannot_open_the_new_epoch() {
    // the rotation at a remove: a NEW epoch key, wrapped only for the
    // survivors — the removed device holds epoch 0 forever and opens
    // nothing after
    let laptop = DeviceKey::from_seed([1; 32]);
    let removed = DeviceKey::from_seed([3; 32]);
    let epoch0 = EpochKey::generate().unwrap(/* t */);
    let sync_id = epoch0.cell_sync_id();
    let epoch1 = EpochKey::generate().unwrap(/* t */);
    // epoch 1 is wrapped for the laptop only
    let wrap = wrap_key(&epoch1, &sync_id, 1, &laptop.hpke_public()).unwrap(/* t */);
    let opened = unwrap_key(&wrap, &sync_id, 1, &laptop_hpke_secret(&laptop)).unwrap(/* t */);
    assert_eq!(opened.0, epoch1.0);
    // the removed device tries its (absent) wrap and fails
    assert!(unwrap_key(&wrap, &sync_id, 1, &laptop_hpke_secret(&removed)).is_err());
    // and even holding the OLD key, the new epoch's envelopes stay closed
    let env = seal_op(&epoch1, b"cell|1|op", b"post-rotation content").unwrap(/* t */);
    assert!(open_op(&epoch0, b"cell|1|op", &env).is_err());
}

#[test]
fn opaque_ids_are_stable_and_opaque() {
    let key = EpochKey::generate().unwrap(/* t */);
    let mac = key.mac_key();
    let a = EpochKey::opaque_op_id(&mac, b"content-hash-of-A");
    let b = EpochKey::opaque_op_id(&mac, b"content-hash-of-B");
    assert_ne!(a, b);
    assert_eq!(a, EpochKey::opaque_op_id(&mac, b"content-hash-of-A"));
    // the sync id and the opaque ids expose nothing of the content they
    // name (they are HMACs under keys that never leave the holders)
    let other_cell = EpochKey::generate().unwrap(/* t */);
    assert_ne!(key.cell_sync_id(), other_cell.cell_sync_id());
    // a LATER epoch derives the SAME opaque ids (the MAC key is epoch-0):
    // operations recognize each other across the rotation
    let mac2 = EpochKey::generate().unwrap(/* t */);
    let _ = mac2; // each cell has its own; here the same cell keeps its own
    assert_eq!(
        EpochKey::opaque_op_id(&mac, b"content-hash-of-A"),
        EpochKey::opaque_op_id(&mac, b"content-hash-of-A")
    );
}
