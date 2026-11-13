// SPDX-License-Identifier: Apache-2.0

//! v0.6 (P2): the device identity and the signed device list. One seed on
//! disk (0600), three keys derived from it — never the same key for two
//! uses — and a registry whose every operation carries an Ed25519
//! signature that names its author by construction. The revocation
//! dominates: a removed device never comes back through the door it left
//! by, and the last device cannot be removed at all.

use engramma_sync::device::DeviceKey;
use engramma_sync::registry::{DeviceRegistry, RegistryKind};

fn key_of(seed: u8) -> DeviceKey {
    DeviceKey::from_seed([seed; 32])
}

#[test]
fn the_seed_file_is_created_once_with_strict_permissions() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let a = DeviceKey::load_or_create(dir.path()).unwrap(/* test-only */);
    let b = DeviceKey::load_or_create(dir.path()).unwrap(/* test-only */);
    assert_eq!(
        a.device_id(),
        b.device_id(),
        "the seed is loaded, not re-made"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("device.key"))
            .unwrap(/* test-only */)
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "device.key is 0600");
    }
}

#[test]
fn the_three_keys_never_coincide_and_the_id_is_the_signing_key() {
    let dk = key_of(7);
    assert_ne!(dk.transport_public(), dk.signing_public());
    assert_ne!(dk.transport_public(), dk.hpke_public());
    assert_ne!(dk.signing_public(), dk.hpke_public());
    assert_eq!(dk.device_id(), hex_of(&dk.signing_public()));
    // different seeds, different everything
    let other = key_of(8);
    assert_ne!(dk.device_id(), other.device_id());
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_list_grows_and_shrinks_under_signatures() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let reg = DeviceRegistry::open(dir.path()).unwrap(/* test-only */);
    let laptop = key_of(1);
    let mac = key_of(2);
    let phone = key_of(3);

    reg.create_first(&laptop, "portatile").unwrap(/* t */);
    assert_eq!(reg.live_members().unwrap(/* t */).len(), 1);
    // the create cannot happen twice
    assert!(reg.create_first(&laptop, "di nuovo").is_err());

    reg.add(
        &laptop,
        &mac.device_id(),
        "mac",
        &hex_of(&mac.transport_public()),
        &hex_of(&mac.hpke_public()),
    )
    .unwrap(/* t */);
    reg.add(
        &mac,
        &phone.device_id(),
        "telefono",
        &hex_of(&phone.transport_public()),
        &hex_of(&phone.hpke_public()),
    )
    .unwrap(/* t */);
    assert_eq!(reg.live_members().unwrap(/* t */).len(), 3);

    // a stranger cannot add or remove
    let stranger = key_of(9);
    let err = reg.add(
        &stranger,
        &key_of(4).device_id(),
        "intruso",
        &hex_of(&[0; 32]),
        &hex_of(&[0; 32]),
    );
    assert!(matches!(err, Err(engramma_sync::Error::Registry(_))));

    // remove one of three, then of two — but never the last
    reg.remove(&mac, &phone.device_id(), "root", 12)
        .unwrap(/* t */);
    assert_eq!(reg.live_members().unwrap(/* t */).len(), 2);
    let (root, count) = reg
        .accepted_set_of(&phone.device_id())
        .unwrap(/* t */)
        .expect(/* t */ "the remove recorded its accepted set");
    assert_eq!(root, "root");
    assert_eq!(count, 12);
    reg.remove(&laptop, &mac.device_id(), "root2", 4)
        .unwrap(/* t */);
    assert_eq!(reg.live_members().unwrap(/* t */).len(), 1);
    assert!(
        reg.remove(&laptop, &laptop.device_id(), "root3", 0)
            .is_err(),
        "the last device cannot be removed"
    );
    // and the removed device cannot sign anything anymore
    let err = reg.remove(&mac, &laptop.device_id(), "x", 0);
    assert!(matches!(err, Err(engramma_sync::Error::Registry(_))));
}

#[test]
fn a_forged_or_late_arrival_is_refused() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let reg = DeviceRegistry::open(dir.path()).unwrap(/* test-only */);
    let laptop = key_of(1);
    let mac = key_of(2);
    reg.create_first(&laptop, "portatile").unwrap(/* t */);

    // a FORGED add: the signature is the laptop's but the payload claims
    // another signer
    let mut op = engramma_sync::registry::RegistryOp {
        op_id: engramma_core::id::new_ulid().unwrap(/* t */),
        hlc: engramma_core::hlc::Hlc::zero("x"),
        kind: RegistryKind::Add,
        payload: serde_json::json!({
            "device": key_of(3).device_id(), "name": "falso",
            "transport": "", "hpke": "", "by": mac.device_id(),
        }),
        sig: Vec::new(),
    };
    // signed by the laptop over the true content, but "by" names the mac
    let content = serde_json::json!({
        "hlc": op.hlc.to_string(),
        "kind": "add",
        "op_id": op.op_id.to_string(),
        "payload": op.payload,
    })
    .to_string();
    op.sig = laptop.sign(content.as_bytes());
    assert!(
        matches!(reg.arrive(&op), Err(engramma_sync::Error::BadSignature)),
        "a signature by anyone but the named signer is refused"
    );

    // a remove that dominates a later re-add
    reg.add(
        &laptop,
        &mac.device_id(),
        "mac",
        &hex_of(&mac.transport_public()),
        &hex_of(&mac.hpke_public()),
    )
    .unwrap(/* t */);
    reg.remove(&laptop, &mac.device_id(), "root", 3)
        .unwrap(/* t */);
    let err = reg.add(
        &laptop,
        &mac.device_id(),
        "mac di nuovo",
        &hex_of(&mac.transport_public()),
        &hex_of(&mac.hpke_public()),
    );
    assert!(
        matches!(err, Err(engramma_sync::Error::Registry(_))),
        "the revocation dominates: no way back in"
    );
}

#[test]
fn the_registry_travels_between_devices() {
    // two roots, one registry: the ops of one arrive at the other, and the
    // members agree
    let dir_a = tempfile::tempdir().unwrap(/* t */);
    let dir_b = tempfile::tempdir().unwrap(/* t */);
    let a = DeviceRegistry::open(dir_a.path()).unwrap(/* t */);
    let b = DeviceRegistry::open(dir_b.path()).unwrap(/* t */);
    let laptop = key_of(1);
    let mac = key_of(2);
    let op0 = a.create_first(&laptop, "portatile").unwrap(/* t */);
    let op1 = a
        .add(
            &laptop,
            &mac.device_id(),
            "mac",
            &hex_of(&mac.transport_public()),
            &hex_of(&mac.hpke_public()),
        )
        .unwrap(/* t */);
    for op in [&op0, &op1] {
        assert!(b.arrive(op).unwrap(/* t */));
        assert!(!b.arrive(op).unwrap(/* t */), "idempotent by op_id");
    }
    let ids = |r: &DeviceRegistry| {
        let mut v: Vec<String> = r
            .live_members()
            .unwrap(/* t */)
            .into_iter()
            .map(|m| m.device)
            .collect();
        v.sort();
        v
    };
    assert_eq!(ids(&a), ids(&b));
}
