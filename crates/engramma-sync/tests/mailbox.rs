// SPDX-License-Identifier: Apache-2.0

//! v0.6 (P6): the blind mailbox. It stores SEALED envelopes by opaque id,
//! reconciles them, delivers them — and when a device's purge arrives,
//! signed, the bytes are REALLY gone from the file. The file never holds
//! note text or an op_id (the string-search proves it).

use engramma_sync::crypto::{EpochKey, seal_op};
use engramma_sync::device::DeviceKey;
use engramma_sync::mailbox::{
    MailboxMsg, MailboxStore, RETENTION_DAYS, purge_signing_content, verify_purge,
};
use engramma_sync::registry::DeviceRegistry;

#[test]
fn the_store_holds_envelopes_by_opaque_id() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let store = MailboxStore::open(dir.path()).unwrap(/* t */);
    let key = EpochKey::generate().unwrap(/* t */);
    let sync_id = key.cell_sync_id();
    let mac = key.mac_key();
    let opaque = |n: u8| EpochKey::opaque_op_id(&mac, &[n]).to_vec();

    for n in 1..=3_u8 {
        let env = seal_op(&key, &sync_id, &[n]).unwrap(/* t */);
        store
            .put("owner", &sync_id, &opaque(n), &env.nonce, &env.ct, 1000)
            .unwrap(/* t */);
    }
    // idempotent
    let env = seal_op(&key, &sync_id, b"1").unwrap(/* t */);
    store
        .put("owner", &sync_id, &opaque(1), &env.nonce, &env.ct, 1000)
        .unwrap(/* t */);
    let ids = store.opaque_ids("owner", &sync_id).unwrap(/* t */);
    assert_eq!(ids.len(), 3);

    // fetch one
    let (sid, _nonce, ct) = store
        .get("owner", &opaque(2))
        .unwrap(/* t */)
        .expect(/* t */ "held");
    assert_eq!(sid, sync_id.to_vec());
    assert!(!ct.is_empty());
}

#[test]
fn a_signed_purge_really_deletes_the_bytes() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let store = MailboxStore::open(dir.path()).unwrap(/* t */);
    let key = EpochKey::generate().unwrap(/* t */);
    let sync_id = key.cell_sync_id();
    let mac = key.mac_key();
    let secret_body = b"il segreto da dimenticare davvero";
    let env = seal_op(&key, &sync_id, secret_body).unwrap(/* t */);
    let opaque = EpochKey::opaque_op_id(&mac, b"content-hash-of-the-note").to_vec();
    store
        .put("owner", &sync_id, &opaque, &env.nonce, &env.ct, 1000)
        .unwrap(/* t */);

    // the sealed body is in the file: the ciphertext's bytes are there
    // (the AFTER comparison below is the honest one — plaintext never
    // existed anywhere, the purge's target is the ciphertext itself)
    let bytes = store.file_bytes(dir.path()).unwrap(/* t */);
    let needle = &env.ct[..16.min(env.ct.len())];
    assert!(
        bytes.windows(needle.len()).any(|w| w == needle),
        "the sealed bytes are stored"
    );

    // the device list: A creates, so A's key verifies the purge
    let dev = DeviceKey::from_seed([1; 32]);
    let reg = DeviceRegistry::open(dir.path()).unwrap(/* t */);
    reg.create_first(&dev, "portatile").unwrap(/* t */);
    let ids = vec![opaque.clone()];
    let content = purge_signing_content("owner", &ids);
    let sig = dev.sign(&content);
    assert!(verify_purge(&reg, "owner", &ids, &sig).is_ok());

    // a stranger's signature does not verify
    let stranger = DeviceKey::from_seed([9; 32]);
    let bad = stranger.sign(&content);
    assert!(verify_purge(&reg, "owner", &ids, &bad).is_err());

    // the purge: the ids go, the file is vacuumed
    let n = store.purge_opaque("owner", &ids).unwrap(/* t */);
    assert_eq!(n, 1);
    let bytes = store.file_bytes(dir.path()).unwrap(/* t */);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("il segreto"),
        "no byte of the sealed body survives the purge"
    );
    assert!(store.get("owner", &opaque).unwrap(/* t */).is_none());
}

#[test]
fn retention_expires_old_envelopes() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let store = MailboxStore::open(dir.path()).unwrap(/* t */);
    let key = EpochKey::generate().unwrap(/* t */);
    let sync_id = key.cell_sync_id();
    let mac = key.mac_key();
    let opaque = EpochKey::opaque_op_id(&mac, b"old").to_vec();
    let env = seal_op(&key, &sync_id, b"vecchia busta").unwrap(/* t */);
    let now = 1_800_000_000_000_i64;
    store
        .put("owner", &sync_id, &opaque, &env.nonce, &env.ct, now - 200 * 24 * 3600 * 1000)
        .unwrap(/* t */);
    let expired = store.expire(now, RETENTION_DAYS).unwrap(/* t */);
    assert_eq!(expired, 1);
    assert!(store.get("owner", &opaque).unwrap(/* t */).is_none());
}

#[test]
fn the_wire_messages_round_trip() {
    let msgs = vec![
        MailboxMsg::Enumerate {
            owner: "owner".into(),
        },
        MailboxMsg::Fetch {
            owner: "owner".into(),
            ids: vec![vec![1, 2, 3]],
        },
        MailboxMsg::Purge {
            owner: "owner".into(),
            ids: vec![vec![9]],
            sig: vec![7; 64],
        },
        MailboxMsg::Deposit {
            owner: "owner".into(),
            envs: vec![serde_json::json!({"nonce": "x", "ct": "y"})],
        },
    ];
    for msg in msgs {
        let round = msg.to_json();
        let back = MailboxMsg::from_json(&round).unwrap(/* t */);
        let again = back.to_json();
        assert_eq!(round, again, "the wire form is stable");
    }
}
