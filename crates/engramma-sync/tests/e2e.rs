// SPDX-License-Identifier: Apache-2.0

//! The end-to-end sync over iroh, on loopback (v0.6, P5): two devices,
//! one shared cell, real connections on 127.0.0.1 with the relay off —
//! and the gate: a caller whose transport key is not a live device's is
//! refused before any exchange.

use engramma_core::cell::ops::Op;
use engramma_core::{Cell, Confidence, Note, NoteType, Status};
use engramma_sync::crypto::{EpochKey, open_op, seal_op};
use engramma_sync::device::{DeviceKey, hex};
use engramma_sync::keystore::KeyStore;
use engramma_sync::protocol::{
    SessionCtx, SyncedCell, authorize, bind_endpoint, connect, run_answerer, run_initiator,
};
use engramma_sync::registry::{DeviceRegistry, RegistryKind};
use time::OffsetDateTime;
use ulid::Ulid;

fn now() -> engramma_core::format::At {
    engramma_core::format::At(
        OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */),
    )
}

fn note(id: u32, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), id))
            .unwrap(/* valid Crockford */),
        note_type: NoteType::Fact,
        title: None,
        body: body.into(),
        tags: vec![],
        scope: None,
        lang: None,
        source: String::new(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: now(),
        updated_at: now(),
        extensions: vec![],
    }
}

/// One device's shared cell: the cell, its epoch key, the seal.
struct SharedCell {
    cell: Cell,
    key: EpochKey,
}

impl SyncedCell for SharedCell {
    fn sync_id(&self) -> Vec<u8> {
        self.key.cell_sync_id().to_vec()
    }

    fn head_seq(&self) -> i64 {
        self.cell
            .debug_column("SELECT CAST(COALESCE(MAX(seq),0) AS TEXT) FROM ops")
            .unwrap(/* t */)[0]
            .parse()
            .unwrap_or(0)
    }

    fn ids(&self) -> Vec<(u64, [u8; 32])> {
        self.cell
            .debug_column("SELECT hlc || ' ' || content_hash FROM ops")
            .unwrap(/* t */)
            .into_iter()
            .filter_map(|row| {
                let (hlc_s, hash) = row.split_once(' ')?;
                let hlc = engramma_core::hlc::Hlc::parse(hlc_s).ok()?;
                let mut id = [0_u8; 32];
                let hash_bytes = hex_to_bytes(hash);
                id.copy_from_slice(&hash_bytes[..32]);
                Some((hlc.physical_ms, id))
            })
            .collect()
    }

    fn envelope_of(&self, id32: &[u8]) -> Option<(u64, Vec<u8>, Vec<u8>)> {
        // find the op whose content_hash starts with id32
        let hashes = self
            .cell
            .debug_column("SELECT content_hash FROM ops")
            .unwrap(/* t */);
        let wanted = hex(id32);
        let hash = hashes.into_iter().find(|h| h == &wanted)?;
        let op = self
            .cell
            .ops()
            .unwrap(/* t */)
            .into_iter()
            .find(|op| op.content_hash == hash)?;
        let plaintext = op_wire(&op);
        let aad = aad_of(self.key.cell_sync_id(), 0);
        let env = seal_op(&self.key, &aad, plaintext.as_bytes()).ok()?;
        Some((0, env.nonce.to_vec(), env.ct))
    }

    fn ingest(&self, _epoch: u64, nonce: &[u8], ct: &[u8]) -> engramma_sync::Result<()> {
        let aad = aad_of(self.key.cell_sync_id(), 0);
        let env = engramma_sync::crypto::Envelope {
            nonce: nonce
                .try_into()
                .map_err(|_| engramma_sync::Error::Malformed("bad nonce length".into()))?,
            ct: ct.to_vec(),
        };
        let plaintext = open_op(&self.key, &aad, &env)?;
        let op = op_from_wire(&plaintext)?;
        self.cell
            .ingest_op(&op)
            .map_err(|e| engramma_sync::Error::Malformed(format!("ingest refused the op: {e}")))?;
        Ok(())
    }
}

fn aad_of(sync_id: [u8; 32], epoch: u64) -> Vec<u8> {
    let mut aad = sync_id.to_vec();
    aad.extend_from_slice(&epoch.to_be_bytes());
    aad
}

/// The wire form of one operation: everything the receiver needs to
/// rebuild and verify it.
fn op_wire(op: &Op) -> String {
    serde_json::json!({
        "op_id": op.op_id.to_string(),
        "hlc": op.hlc.to_string(),
        "actor": op.actor,
        "kind": op.kind.as_str(),
        "note_id": op.note_id.map(|n| n.to_string()),
        "base": op.base,
        "payload_digest": op.payload_digest,
        "content_hash": op.content_hash,
        "device": op.device,
        "sig": op.sig,
        "body": op.body,
        "salt": op.salt.as_deref().map(hex),
    })
    .to_string()
}

fn op_from_wire(plaintext: &[u8]) -> engramma_sync::Result<Op> {
    let v: serde_json::Value = serde_json::from_slice(plaintext)
        .map_err(|e| engramma_sync::Error::Malformed(format!("bad op wire: {e}")))?;
    let get_s = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let op_id = Ulid::from_string(&get_s("op_id"))
        .map_err(|e| engramma_sync::Error::Malformed(format!("bad op_id: {e}")))?;
    let hlc = engramma_core::hlc::Hlc::parse(&get_s("hlc"))
        .map_err(|e| engramma_sync::Error::Malformed(format!("bad hlc: {e}")))?;
    let kind = engramma_core::cell::ops::OpKind::parse(&get_s("kind"))
        .map_err(|e| engramma_sync::Error::Malformed(format!("bad kind: {e}")))?;
    let note_id = v
        .get("note_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Ulid::from_string(s).ok());
    let mut op = engramma_core::cell::ops::make_op(engramma_core::cell::ops::OpParts {
        op_id,
        hlc,
        actor: get_s("actor"),
        kind,
        note_id,
        base: v
            .get("base")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        body: v.get("body").cloned().unwrap_or(serde_json::Value::Null),
        salt: v
            .get("salt")
            .and_then(serde_json::Value::as_str)
            .map(hex_to_bytes)
            .unwrap_or_default(),
        device: v
            .get("device")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        sig: v
            .get("sig")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
    })
    .map_err(|e| engramma_sync::Error::Malformed(format!("op rebuild: {e}")))?;
    op.payload_digest = get_s("payload_digest");
    op.content_hash = get_s("content_hash");
    Ok(op)
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

/// Two devices, one shared cell, a real session: B connects to A, the
/// registry reconciles, the cell's operations travel sealed, and both
/// copies converge at the same replicated digest.
#[tokio::test(flavor = "current_thread")]
async fn two_devices_converge_over_loopback() {
    let dir_a = tempfile::tempdir().unwrap(/* t */);
    let dir_b = tempfile::tempdir().unwrap(/* t */);

    // the devices
    let key_a = DeviceKey::load_or_create(dir_a.path()).unwrap(/* t */);
    let key_b = DeviceKey::load_or_create(dir_b.path()).unwrap(/* t */);

    // the registry: A first, B added by A; B learns the create+add
    let reg_a = DeviceRegistry::open(dir_a.path()).unwrap(/* t */);
    reg_a.create_first(&key_a, "portatile").unwrap(/* t */);
    let add = reg_a
        .add(
            &key_a,
            &key_b.device_id(),
            "mac",
            &hex(&key_b.transport_public()),
            &hex(&key_b.hpke_public()),
        )
        .unwrap(/* t */);
    let reg_b = DeviceRegistry::open(dir_b.path()).unwrap(/* t */);
    // B learns the whole list as it traveled: every row with its own
    // signature (the create and the add of B itself)
    reg_b.arrive_rows(&reg_a.rows_json()).unwrap(/* t */);
    assert_eq!(reg_b.live_members().unwrap(/* t */).len(), 2);
    let _ = &add;

    // A's cell, with three notes
    let cell_a = Cell::create(&dir_a.path().join("p.sqlite"), "conv", "device-a").unwrap(/* t */);
    for i in 1..=3_u32 {
        cell_a.put_note(&note(i, &format!("nota {i} di A")), "device-a").unwrap(/* t */);
    }
    // share: epoch 0
    let key_store_keys_a = EpochKey::generate().unwrap(/* t */);
    let ks_a = KeyStore::new(reg_a.conn());
    ks_a.store_key(&key_store_keys_a.cell_sync_id(), 0, &key_store_keys_a).unwrap(/* t */);
    let _shared_a = SharedCell {
        cell: cell_a.clone(),
        key: key_store_keys_a.clone(),
    };

    // B's bare copy of the same cell, same key (the pairing handed it over)
    let cell_b = Cell::open_bare(&dir_b.path().join("p.sqlite")).unwrap(/* t */);
    let ks_b = KeyStore::new(reg_b.conn());
    ks_b.store_key(&key_store_keys_a.cell_sync_id(), 0, &key_store_keys_a).unwrap(/* t */);
    let shared_b = SharedCell {
        cell: cell_b.clone(),
        key: key_store_keys_a.clone(),
    };

    // the endpoints, on loopback
    let ep_a = bind_endpoint(&key_a, "127.0.0.1:0").await.unwrap(/* t */);
    let ep_b = bind_endpoint(&key_b, "127.0.0.1:0").await.unwrap(/* t */);
    let addr_a = ep_a.addr();

    // A serves; B initiates. The registry's SQLite handle is not Send:
    // everything runs on the current thread's LocalSet
    let report = tokio::task::LocalSet::new()
        .run_until(async {
            let server = {
                let ep_a2 = ep_a.clone();
                let cell_a = cell_a.clone();
                let key_a2 = key_a.clone_seed();
                tokio::task::spawn_local(async move {
                    let reg_a2 = DeviceRegistry::open(dir_a.path()).unwrap(/* t */);
                    let ks_a2 = KeyStore::new(reg_a2.conn());
                    let key_a = DeviceKey::from_seed(key_a2);
                    let conn = {
                        let Some(incoming) = ep_a2.accept().await else {
                            return;
                        };
                        match incoming.await {
                            Ok(conn) => conn,
                            Err(_) => return,
                        }
                    };
                    let caller = conn.remote_id();
                    if authorize(caller.as_bytes(), &reg_a2).is_err() {
                        // minaccia 3: closed before any exchange
                        return;
                    }
                    let cell = SharedCell {
                        cell: cell_a.clone(),
                        key: key_store_keys_a.clone(),
                    };
                    let ctx = SessionCtx {
                        registry: &reg_a2,
                        keys: ks_a2,
                        device: &key_a,
                        cells: vec![Box::new(cell)],
                    };
                    let _ = run_answerer(conn, &ctx).await;
                })
            };

            let conn = connect(&ep_b, addr_a.clone()).await.unwrap(/* t */);
            let ctx_b = SessionCtx {
                registry: &reg_b,
                keys: ks_b,
                device: &key_b,
                cells: vec![Box::new(shared_b)],
            };
            let report = run_initiator(conn, &ctx_b).await.unwrap(/* t */);
            let _ = server.await;
            report
        })
        .await;

    // B received the three notes and the init: the digests agree
    assert!(report.cells[0].received >= 4, "{report:?}");
    let da = cell_a.replicated_digest().unwrap(/* t */);
    let db = cell_b.replicated_digest().unwrap(/* t */);
    assert_eq!(da, db, "the two devices hold the same replicated state");
    assert!(cell_b.verify().unwrap(/* t */).ok());
}

/// The gate: a caller whose transport key is not a live device's is
/// refused before any exchange.
#[tokio::test]
async fn a_stranger_is_refused_before_any_exchange() {
    let dir_a = tempfile::tempdir().unwrap(/* t */);
    let key_a = DeviceKey::load_or_create(dir_a.path()).unwrap(/* t */);
    let reg_a = DeviceRegistry::open(dir_a.path()).unwrap(/* t */);
    reg_a.create_first(&key_a, "portatile").unwrap(/* t */);

    // the stranger is not in the list
    let stranger = DeviceKey::from_seed([9; 32]);
    assert!(authorize(&stranger.transport_public(), &reg_a).is_err());

    // and a live member passes
    let member = DeviceKey::from_seed([1; 32]);
    reg_a
        .add(
            &key_a,
            &member.device_id(),
            "m",
            &hex(&member.transport_public()),
            &hex(&member.hpke_public()),
        )
        .unwrap(/* t */);
    assert!(authorize(&member.transport_public(), &reg_a).is_ok());
    let _ = RegistryKind::Create; // the import stays honest
}
