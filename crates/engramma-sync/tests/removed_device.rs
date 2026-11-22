// SPDX-License-Identifier: Apache-2.0

//! R1 (v0.6.1, revisore): a REMOVED device enters only with the proof. The
//! operative set of a `remove` is the Merkle root it carries — not the HLC,
//! which the author chooses. Every test here builds the envelopes exactly
//! as the wire would carry them, and every refusal must name the gate that
//! refused, never any note text.

mod common;

use engramma_core::cell::ops::{self, MerkleStep, Op, OpKind, OpParts};
use engramma_core::format::{Note, NoteType};
use engramma_core::hlc::Hlc;
use engramma_core::{Cell, Confidence, Status};
use engramma_sync::arrival::DeviceSigner;
use engramma_sync::crypto::{self, EpochKey};
use engramma_sync::device::{DeviceKey, hex};
use engramma_sync::keystore::KeyStore;
use engramma_sync::protocol::SyncedCell;
use engramma_sync::registry::DeviceRegistry;
use engramma_sync::sharedcell::{SharedCell, op_aad, share_cell};
use engramma_sync::wire;
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

fn unhex32(s: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (chunk[1] as char).to_digit(16).unwrap_or(0) as u8;
        out[i] = (hi << 4) | lo;
    }
    out
}

/// Two roots, one list with two live devices (A and B), one shared cell —
/// the production shape, minus the network.
struct Fixture {
    dir_a: tempfile::TempDir,
    dir_b: tempfile::TempDir,
    key_a: DeviceKey,
    key_b: DeviceKey,
    sync_id: [u8; 32],
}

impl Fixture {
    fn new() -> Self {
        let dir_a = common::test_dir();
        let dir_b = common::test_dir();
        std::fs::create_dir_all(dir_a.path().join("cells")).unwrap(/* t */);
        std::fs::create_dir_all(dir_b.path().join("cells")).unwrap(/* t */);
        let key_a = DeviceKey::load_or_create(dir_a.path()).unwrap(/* t */);
        let key_b = DeviceKey::load_or_create(dir_b.path()).unwrap(/* t */);
        let reg_a = DeviceRegistry::open(dir_a.path()).unwrap(/* t */);
        reg_a.create_first(&key_a, "portatile").unwrap(/* t */);
        reg_a
            .add(
                &key_a,
                &key_b.device_id(),
                "mac",
                &hex(&key_b.transport_public()),
                &hex(&key_b.hpke_public()),
            )
            .unwrap(/* t */);
        let reg_b = DeviceRegistry::open(dir_b.path()).unwrap(/* t */);
        reg_b.arrive_rows(&reg_a.rows_json()).unwrap(/* t */);
        let cell = Cell::create(&dir_a.path().join("cells").join("memoria.sqlite"), "conv", "cli")
                .unwrap(/* t */);
        cell.put_note(&note(1, "la base di A"), "cli").unwrap(/* t */);
        let sync_id = share_cell(dir_a.path(), "memoria").unwrap(/* t */);
        Self {
            dir_a,
            dir_b,
            key_a,
            key_b,
            sync_id,
        }
    }

    /// A's side: the shared cell bound to A's registry, the signer armed.
    fn side_a(&self) -> SharedCell {
        let cell = Cell::open(&self.dir_a.path().join("cells").join("memoria.sqlite"))
            .unwrap(/* t */);
        let reg = DeviceRegistry::open(self.dir_a.path()).unwrap(/* t */);
        cell.set_signer(std::sync::Arc::new(DeviceSigner::new(&self.key_a)))
            .unwrap(/* t */);
        SharedCell::new(cell, reg, self.sync_id, self.dir_a.path())
    }

    /// B's side: a bare cell, epoch 0 in its keystore, the signer armed.
    fn side_b(&self) -> SharedCell {
        let cell = Cell::open_bare(&self.dir_b.path().join("cells").join("memoria.sqlite"))
            .unwrap(/* t */);
        let reg = DeviceRegistry::open(self.dir_b.path()).unwrap(/* t */);
        let key = KeyStore::new(
            DeviceRegistry::open(self.dir_a.path()).unwrap(/* t */).conn(),
        )
        .key_of(&self.sync_id, 0)
        .unwrap(/* t */)
        .expect(/* the sharer minted it */ "epoch 0");
        KeyStore::new(reg.conn())
            .store_key(&self.sync_id, 0, &key)
            .unwrap(/* t */);
        reg.conn()
            .execute(
                "INSERT INTO shared_cells (name, sync_id) VALUES ('memoria', ?1)
                 ON CONFLICT(name) DO UPDATE SET sync_id = ?1",
                [self.sync_id],
            )
            .unwrap(/* t */);
        cell.set_signer(std::sync::Arc::new(DeviceSigner::new(&self.key_b)))
            .unwrap(/* t */);
        cell.mark_shared().unwrap(/* t */);
        SharedCell::new(cell, reg, self.sync_id, self.dir_b.path())
    }
}

/// Build one SIGNED put by hand: the HLC is the caller's (the author
/// chooses it — the whole point of R1), the identity is minted over the
/// content, the signature is the named device's.
fn hand_signed_put(key: &DeviceKey, n: u32, body: &str, hlc: Hlc) -> Op {
    let mut op = ops::make_op(OpParts {
        op_id: Ulid::from_string(&format!("01HANDMADE{:016}", n)).unwrap(/* t */),
        hlc,
        actor: "cli".into(),
        kind: OpKind::PutNote,
        note_id: Some(note(n, body).id),
        base: None,
        body: serde_json::json!({ "note": note(n, body).to_json() }),
        salt: vec![(n % 200) as u8 + 7; 16],
        device: Some(key.device_id()),
        sig: None,
    })
    .unwrap(/* t */);
    op.sig = Some({
        use base64ct::{Base64UrlUnpadded as B64, Encoding};
        B64::encode_string(&key.sign(op.content_hash.as_bytes()))
    });
    op
}

/// Seal one op for the wire, with an optional proof (the adopt cover of an
/// unsigned op, or the membership proof of a removed device's op).
fn sealed(
    sender: &SharedCell,
    op: &Op,
    proof: Option<&[MerkleStep]>,
) -> (u64, [u8; 32], Vec<u8>, Vec<u8>) {
    let epoch = sender.current_epoch().unwrap(/* t */);
    let key = KeyStore::new(sender.registry().conn())
        .key_of(sender.sync_id(), epoch)
        .unwrap(/* t */)
        .expect(/* armed */ "the epoch key");
    let id = unhex32(&op.content_hash);
    let aad = op_aad(sender.sync_id(), epoch, &id);
    let plaintext = match (op.sig.is_none(), proof) {
        // an unsigned op travels with its adopt proof, exactly as the
        // adapter seals it
        (true, None) => {
            let p = engramma_sync::arrival::adopt_proof(sender.cell(), op).unwrap(/* t */);
            wire::envelope_wire(op, Some(&p))
        }
        _ => wire::envelope_wire(op, proof),
    };
    let env = crypto::seal_op(&key, &aad, plaintext.as_bytes()).unwrap(/* t */);
    (epoch, id, env.nonce.to_vec(), env.ct)
}

/// The accepted set of a remove as `devices remove` computes it: the
/// Merkle root over every operation of the removed device this root holds,
/// across every shared cell.
fn accepted_root_of(root: &std::path::Path, device: &str) -> (String, usize) {
    let mut hashes: Vec<String> = Vec::new();
    for shared in SharedCell::all_of(root).unwrap(/* t */) {
        hashes.extend(
            shared
                .cell()
                .debug_column(&format!(
                    "SELECT o.content_hash FROM ops o WHERE o.device = '{}'",
                    device.replace('\'', "''")
                ))
                .unwrap_or_default(),
        );
    }
    let count = hashes.len();
    (ops::merkle_root(&mut hashes), count)
}

/// The reviewer's proof, as a test: the device is removed, then an
/// operation built by hand — HLC one day BEFORE the remove, identity
/// recomputed, signature of the stolen key — must NOT pass the gate. The
/// HLC is the author's choice; the clock cap only stops the future.
#[test]
fn a_backdated_op_of_a_removed_device_is_refused() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    let doomed = DeviceKey::from_seed([5; 32]);
    // A adds the third device, then removes it
    let reg_a = DeviceRegistry::open(fx.dir_a.path()).unwrap(/* t */);
    reg_a
        .add(
            &fx.key_a,
            &doomed.device_id(),
            "vecchio",
            &hex(&doomed.transport_public()),
            &hex(&doomed.hpke_public()),
        )
        .unwrap(/* t */);
    let (root, count) = accepted_root_of(fx.dir_a.path(), &doomed.device_id());
    reg_a
        .remove(&fx.key_a, &doomed.device_id(), &root, count as u64)
        .unwrap(/* t */);
    // B learns the add and the remove
    DeviceRegistry::open(fx.dir_b.path())
        .unwrap(/* t */)
        .arrive_rows(&reg_a.rows_json())
        .unwrap(/* t */);
    // the stolen key writes, dating the operation one day back
    let day = 24 * 60 * 60 * 1000_u64;
    let backdated = Hlc {
        physical_ms: OffsetDateTime::now_utc().unix_timestamp() as u64 * 1000 - day,
        counter: 0,
        node: doomed.device_id(),
    };
    let op = hand_signed_put(&doomed, 41, "la nota retrodatata del rimosso", backdated);
    let (epoch, id, nonce, ct) = sealed(&a, &op, None);
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* must refuse */);
    assert!(
        err.to_string().contains("removed"),
        "a backdated op of a removed device must be refused as outside the \
         accepted set: {err}"
    );
}

/// A TRUE operation of the removed device — inside the set its remove
/// accepted, not yet arrived at B — enters WITH the membership proof, and
/// is refused without it.
#[test]
fn a_true_op_of_a_removed_device_needs_its_proof() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    let doomed = DeviceKey::from_seed([6; 32]);
    let reg_a = DeviceRegistry::open(fx.dir_a.path()).unwrap(/* t */);
    reg_a
        .add(
            &fx.key_a,
            &doomed.device_id(),
            "vecchio",
            &hex(&doomed.transport_public()),
            &hex(&doomed.hpke_public()),
        )
        .unwrap(/* t */);
    // A adopts, then the doomed device writes two true operations (A's gate
    // admits them: the device is live), and A removes it
    a.cell().adopt_log("cli").unwrap(/* t */);
    a.cell().mark_shared().unwrap(/* t */);
    let hlc_of = |ms: u64| Hlc {
        physical_ms: ms,
        counter: 0,
        node: doomed.device_id(),
    };
    let base_ms = OffsetDateTime::now_utc().unix_timestamp() as u64 * 1000 - 60_000;
    for (i, off) in [10_000_u64, 20_000].into_iter().enumerate() {
        let op = hand_signed_put(
            &doomed,
            50 + i as u32,
            "la nota vera del rimosso",
            hlc_of(base_ms + off),
        );
        let (epoch, id, nonce, ct) = sealed(&a, &op, None);
        a.ingest(epoch, &id, &nonce, &ct)
            .unwrap_or_else(|e| panic!("the live device's op {} must enter A: {e}", op.op_id));
    }
    let (root, count) = accepted_root_of(fx.dir_a.path(), &doomed.device_id());
    reg_a
        .remove(&fx.key_a, &doomed.device_id(), &root, count as u64)
        .unwrap(/* t */);
    let rows = reg_a.rows_json();
    DeviceRegistry::open(fx.dir_b.path())
        .unwrap(/* t */)
        .arrive_rows(&rows)
        .unwrap(/* t */);
    // the last true op of the doomed device, held by A, unknown to B
    let held: Vec<Op> = a
        .cell()
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.device.as_deref() == Some(doomed.device_id().as_str()))
        .collect();
    assert_eq!(held.len(), 2, "A holds both true ops");
    let target = held.last().cloned().unwrap(/* checked */);
    // without the proof: refused
    let (epoch, id, nonce, ct) = sealed(&a, &target, None);
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* needs proof */);
    assert!(
        err.to_string().contains("removed"),
        "a removed device's op without its proof must be refused: {err}"
    );
    // with the membership proof toward the remove's root: accepted
    let mut set: Vec<String> = a
        .cell()
        .debug_column("SELECT content_hash FROM ops WHERE device IS NOT NULL")
        .unwrap(/* t */)
        .into_iter()
        .filter(|h| {
            // the set is the removed device's ops, as the remove computed it
            a.cell()
                .ops()
                .unwrap_or_default()
                .iter()
                .any(|o| o.device.as_deref() == Some(doomed.device_id().as_str())
                    && &o.content_hash == h)
        })
        .collect();
    assert_eq!(
        ops::merkle_root(&mut set),
        root,
        "the set rebuilds the root"
    );
    let proof = ops::merkle_prove(&mut set.clone(), &target.content_hash).unwrap(/* t */);
    assert_eq!(
        ops::merkle_root_with(&target.content_hash, &proof).as_deref(),
        Some(root.as_str())
    );
    let (epoch, id, nonce, ct) = sealed(&a, &target, Some(&proof));
    b.ingest(epoch, &id, &nonce, &ct)
        .unwrap_or_else(|e| panic!("the true op with its proof must enter B: {e}"));
}

/// Two CONCURRENT removes of the same device, by two different members,
/// with different accepted sets: the union rules — a proof toward EITHER
/// root admits the operation.
#[test]
fn concurrent_removes_admit_a_proof_toward_either_root() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    let doomed = DeviceKey::from_seed([7; 32]);
    let reg_a = DeviceRegistry::open(fx.dir_a.path()).unwrap(/* t */);
    reg_a
        .add(
            &fx.key_a,
            &doomed.device_id(),
            "vecchio",
            &hex(&doomed.transport_public()),
            &hex(&doomed.hpke_public()),
        )
        .unwrap(/* t */);
    a.cell().adopt_log("cli").unwrap(/* t */);
    a.cell().mark_shared().unwrap(/* t */);
    let hlc_of = |ms: u64| Hlc {
        physical_ms: ms,
        counter: 0,
        node: doomed.device_id(),
    };
    let base_ms = OffsetDateTime::now_utc().unix_timestamp() as u64 * 1000 - 60_000;
    let mut held: Vec<Op> = Vec::new();
    for (i, off) in [10_000_u64, 20_000, 30_000].into_iter().enumerate() {
        let op = hand_signed_put(
            &doomed,
            60 + i as u32,
            "la nota del rimosso",
            hlc_of(base_ms + off),
        );
        let (epoch, id, nonce, ct) = sealed(&a, &op, None);
        a.ingest(epoch, &id, &nonce, &ct).unwrap(/* live */);
        held.push(op);
    }
    // the two removes are CONCURRENT: each happens before its author learns
    // the other's, so B takes the list WITHOUT any remove first
    let reg_b = DeviceRegistry::open(fx.dir_b.path()).unwrap(/* t */);
    reg_b.arrive_rows(&reg_a.rows_json()).unwrap(/* t */);
    // remove 1: by B — B had accepted only {h1} when it removed
    let mut first_only: Vec<String> = vec![held[0].content_hash.clone()];
    let root_first = ops::merkle_root(&mut first_only);
    reg_b
        .remove(&fx.key_b, &doomed.device_id(), &root_first, 1)
        .unwrap(/* t */);
    // remove 2: by A, over the whole set {h1, h2, h3}
    let mut whole: Vec<String> = held.iter().map(|o| o.content_hash.clone()).collect();
    let root_whole = ops::merkle_root(&mut whole);
    reg_a
        .remove(&fx.key_a, &doomed.device_id(), &root_whole, 3)
        .unwrap(/* t */);
    // each side learns the other's remove: both lists hold BOTH
    let reg_a = DeviceRegistry::open(fx.dir_a.path()).unwrap(/* t */);
    reg_a.arrive_rows(&reg_b.rows_json()).unwrap(/* t */);
    let reg_b = DeviceRegistry::open(fx.dir_b.path()).unwrap(/* t */);
    reg_b.arrive_rows(&reg_a.rows_json()).unwrap(/* t */);
    // the last op (h3) proves toward the WHOLE root (remove 1's set)
    let mut set: Vec<String> = held.iter().map(|o| o.content_hash.clone()).collect();
    let proof_whole = ops::merkle_prove(&mut set, &held[2].content_hash).unwrap(/* t */);
    let (epoch, id, nonce, ct) = sealed(&a, &held[2], Some(&proof_whole));
    b.ingest(epoch, &id, &nonce, &ct)
        .unwrap_or_else(|e| panic!("a proof toward the first remove's root must enter: {e}"));
    // the first op (h1) proves toward the FIRST-ONLY root (remove 2's set)
    let mut set: Vec<String> = vec![held[0].content_hash.clone()];
    let proof_first = ops::merkle_prove(&mut set, &held[0].content_hash).unwrap(/* t */);
    let (epoch, id, nonce, ct) = sealed(&a, &held[0], Some(&proof_first));
    b.ingest(epoch, &id, &nonce, &ct)
        .unwrap_or_else(|e| panic!("a proof toward the second remove's root must enter: {e}"));
    // and h2 — in neither single set... wait: it IS in the whole set. A
    // proof toward a set that does not rebuild any recorded root refuses.
    let mut alien: Vec<String> = vec![
        held[1].content_hash.clone(),
        blake3::hash(&[9; 8]).to_hex().to_string(),
    ];
    let proof_alien = ops::merkle_prove(&mut alien, &held[1].content_hash).unwrap(/* t */);
    let (epoch, id, nonce, ct) = sealed(&a, &held[1], Some(&proof_alien));
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* alien */);
    assert!(
        err.to_string().contains("removed"),
        "a proof toward no recorded root must be refused: {err}"
    );
}

/// The author enters the identity: a LOCAL operation of an armed cell has
/// `hlc.node` equal to the device id (the machine id is not the author).
#[test]
fn a_local_signed_op_carries_the_device_as_its_hlc_node() {
    let fx = Fixture::new();
    let a = fx.side_a();
    a.cell().adopt_log("cli").unwrap(/* t */);
    a.cell().mark_shared().unwrap(/* t */);
    a.cell().put_note(&note(70, "dell'era firmata"), "cli").unwrap(/* t */);
    let signed: Vec<Op> = a
        .cell()
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.sig.is_some())
        .collect();
    assert!(!signed.is_empty(), "the signer was armed");
    for op in &signed {
        assert_eq!(
            op.hlc.node,
            fx.key_a.device_id(),
            "op {}: the HLC node must name the device, not the machine",
            op.op_id
        );
    }
}

/// The gate demands `device == hlc.node` on every signed operation — both
/// are covered by the content hash the signature signs, so a disagreement
/// is a forged authorship.
#[test]
fn a_signed_op_whose_device_disagrees_with_its_hlc_node_is_refused() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    a.cell().adopt_log("cli").unwrap(/* t */);
    a.cell().mark_shared().unwrap(/* t */);
    // A signs an op whose HLC names ANOTHER node (the machine id, as the
    // old code wrote): the identity is recomputed over the altered HLC, so
    // the signature is genuine — the disagreement is the forgery
    let mut parts_hlc = Hlc {
        physical_ms: OffsetDateTime::now_utc().unix_timestamp() as u64 * 1000,
        counter: 0,
        node: "0123456789ab".into(), // the machine id, not the device
    };
    parts_hlc.counter = 3;
    let op = hand_signed_put(&fx.key_a, 71, "la nota dal nodo sbagliato", parts_hlc);
    let (epoch, id, nonce, ct) = sealed(&a, &op, None);
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* must refuse */);
    assert!(
        err.to_string().contains("signature"),
        "device != hlc.node must be refused as a bad signature: {err}"
    );
}

/// A live device cannot sign FOR another: the op names A, the signature is
/// B's — the verification against A's key fails.
#[test]
fn a_live_device_cannot_sign_for_another() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    let hlc = Hlc {
        physical_ms: OffsetDateTime::now_utc().unix_timestamp() as u64 * 1000,
        counter: 1,
        node: fx.key_a.device_id(),
    };
    // names A, signed by B
    let mut op = hand_signed_put(&fx.key_a, 72, "la nota di A firmata da B", hlc);
    op.sig = Some({
        use base64ct::{Base64UrlUnpadded as B64, Encoding};
        B64::encode_string(&fx.key_b.sign(op.content_hash.as_bytes()))
    });
    let (epoch, id, nonce, ct) = sealed(&a, &op, None);
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* must refuse */);
    assert!(
        err.to_string().contains("signature"),
        "one device's signature over another's op must be refused: {err}"
    );
}

/// The epoch key of a fixture (kept for the sender's sealing).
#[allow(dead_code)]
fn epoch0_of(root: &std::path::Path, sync_id: [u8; 32]) -> EpochKey {
    let reg = DeviceRegistry::open(root).unwrap(/* t */);
    KeyStore::new(reg.conn())
        .key_of(&sync_id, 0)
        .unwrap(/* t */)
        .expect(/* the sharer minted it */ "epoch 0")
}
