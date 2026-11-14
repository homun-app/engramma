// SPDX-License-Identifier: Apache-2.0

//! The end-to-end sync over iroh, on loopback (v0.6.1, P1): two devices,
//! one shared cell, real connections on 127.0.0.1 with the relay off — on
//! the whole production road: the crate's own wire form, the SharedCell
//! adapter with epoch keys from the keystore, the arrival gate (signature,
//! membership, adopt coverage) and its refusals.

use engramma_core::cell::ops::{Op, OpParts};
use engramma_core::format::{Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use engramma_sync::arrival::DeviceSigner;
use engramma_sync::crypto::{self, EpochKey};
use engramma_sync::device::{DeviceKey, hex};
use engramma_sync::keystore::KeyStore;
use engramma_sync::protocol::{
    SessionCtx, SyncedCell, authorize, bind_endpoint, connect, run_answerer, run_initiator,
};
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

/// Two roots, one registry with two live devices, one shared cell with a
/// few notes — the production shape, minus the network.
struct Fixture {
    dir_a: tempfile::TempDir,
    dir_b: tempfile::TempDir,
    key_a: DeviceKey,
    key_b: DeviceKey,
    sync_id: [u8; 32],
}

impl Fixture {
    fn new() -> Self {
        let dir_a = tempfile::tempdir().unwrap(/* t */);
        let dir_b = tempfile::tempdir().unwrap(/* t */);
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
        // B learns the list as it travels: every row with its signature
        let reg_b = DeviceRegistry::open(dir_b.path()).unwrap(/* t */);
        reg_b.arrive_rows(&reg_a.rows_json()).unwrap(/* t */);
        // the shared cell: notes first, then share — the order of
        // `sync share`
        let cell = Cell::create(&dir_a.path().join("cells").join("memoria.sqlite"), "conv", "cli")
                .unwrap(/* t */);
        for i in 1..=3_u32 {
            cell.put_note(&note(i, &format!("nota {i} di A")), "cli").unwrap(/* t */);
        }
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
        SharedCell::new(cell, reg, self.sync_id)
    }

    /// B's side: a bare cell, epoch 0 in its keystore (the pairing handed
    /// the key over), the signer armed.
    fn side_b(&self) -> SharedCell {
        let cell = Cell::open_bare(&self.dir_b.path().join("cells").join("memoria.sqlite"))
            .unwrap(/* t */);
        let reg = DeviceRegistry::open(self.dir_b.path()).unwrap(/* t */);
        let key = epoch0_of(self.dir_a.path(), self.sync_id);
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
        // the cell IS shared — the fact rides with the replicated state,
        // so every copy that joins marks it too
        cell.mark_shared().unwrap(/* t */);
        SharedCell::new(cell, reg, self.sync_id)
    }
}

fn epoch0_of(root: &std::path::Path, sync_id: [u8; 32]) -> EpochKey {
    let reg = DeviceRegistry::open(root).unwrap(/* t */);
    KeyStore::new(reg.conn())
        .key_of(&sync_id, 0)
        .unwrap(/* t */)
        .expect(/* the sharer minted it */ "epoch 0")
}

/// The envelope of one op of one cell — what the wire would carry. The op
/// is sealed exactly as the adapter would seal it.
fn envelope_of_op(cell: &SharedCell, op: &Op) -> (u64, [u8; 32], Vec<u8>, Vec<u8>) {
    let epoch = cell.current_epoch().unwrap(/* t */);
    let key = KeyStore::new(cell.registry().conn())
        .key_of(cell.sync_id(), epoch)
        .unwrap(/* t */)
        .expect(/* armed */ "the epoch key");
    let id = unhex32(&op.content_hash);
    let aad = op_aad(cell.sync_id(), epoch, &id);
    // an unsigned op travels with its adopt proof, exactly as the adapter
    // seals it
    let plaintext = if op.sig.is_none() {
        let proof = engramma_sync::arrival::adopt_proof(cell.cell(), op).unwrap(/* t */);
        wire::envelope_wire(op, Some(&proof))
    } else {
        wire::envelope_wire(op, None)
    };
    let env = crypto::seal_op(&key, &aad, plaintext.as_bytes()).unwrap(/* t */);
    (epoch, id, env.nonce.to_vec(), env.ct)
}

/// Two devices, one shared cell, real sessions over loopback: B connects
/// to A, the registry reconciles first, the cell's operations travel
/// sealed through the crate's own wire, the gate admits them, and both
/// copies converge at the same replicated digest.
#[tokio::test(flavor = "current_thread")]
async fn two_devices_converge_over_loopback() {
    let fx = Fixture::new();
    let a = fx.side_a();
    // the share: the adopt covers the pre-era set; from here on, signed
    a.cell().adopt_log("cli").unwrap(/* t */);
    a.cell().mark_shared().unwrap(/* t */);
    a.cell()
        .put_note(&note(9, "la nota dell'era firmata"), "cli")
        .unwrap(/* t */);
    let b = fx.side_b();

    let ep_a = bind_endpoint(&fx.key_a, "127.0.0.1:0").await.unwrap(/* t */);
    let ep_b = bind_endpoint(&fx.key_b, "127.0.0.1:0").await.unwrap(/* t */);
    let addr_a = ep_a.addr();

    // the SQLite handles are not Send: everything on the current thread
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = {
                let ep = ep_a.clone();
                let dir = fx.dir_a.path().to_path_buf();
                tokio::task::spawn_local(async move {
                    while let Some(incoming) = ep.accept().await {
                        let Ok(conn) = incoming.await else {
                            continue;
                        };
                        let caller = conn.remote_id();
                        let cells = SharedCell::all_of(&dir).unwrap(/* t */);
                        let reg = DeviceRegistry::open(&dir).unwrap(/* t */);
                        // the gate: closed before any exchange
                        if authorize(caller.as_bytes(), &reg).is_err() {
                            continue;
                        }
                        let dk = DeviceKey::load_or_create(&dir).unwrap(/* t */);
                        cells.iter().for_each(|c| {
                            c.cell()
                                .set_signer(std::sync::Arc::new(DeviceSigner::new(&dk)))
                                .ok();
                        });
                        let ks = KeyStore::new(reg.conn());
                        let ctx = SessionCtx {
                            registry: &reg,
                            keys: ks,
                            device: &dk,
                            cells: cells.iter().map(|c| c as &dyn SyncedCell).collect(),
                        };
                        let _ = run_answerer(conn, &ctx).await;
                    }
                })
            };
            // sessions until nothing new flows: a pre-era op the first pass
            // ordered before its adopt is refused once, then accepted
            for _ in 0..5 {
                let reg_b = DeviceRegistry::open(fx.dir_b.path()).unwrap(/* t */);
                let ks_b = KeyStore::new(reg_b.conn());
                let conn = connect(&ep_b, addr_a.clone()).await.unwrap(/* t */);
                let ctx_b = SessionCtx {
                    registry: &reg_b,
                    keys: ks_b,
                    device: &fx.key_b,
                    cells: vec![&b as &dyn SyncedCell],
                };
                let _ = run_initiator(conn, &ctx_b).await;
                if b.head_seq() >= a.head_seq() {
                    break;
                }
            }
            server.abort();
        })
        .await;

    let da = a.cell().replicated_digest().unwrap(/* t */);
    let db = b.cell().replicated_digest().unwrap(/* t */);
    if da != db {
        eprintln!("head a {} b {}", a.head_seq(), b.head_seq());
        let x = a.cell().replicated_dump().unwrap(/* t */);
        let y = b.cell().replicated_dump().unwrap(/* t */);
        let xl: Vec<&str> = x.lines().collect();
        let yl: Vec<&str> = y.lines().collect();
        for i in 0..xl.len().max(yl.len()) {
            let p = xl.get(i).copied().unwrap_or("<eof>");
            let q = yl.get(i).copied().unwrap_or("<eof>");
            if p != q {
                eprintln!("line {i}:\n  A: {p}\n  B: {q}");
            }
        }
    }
    assert_eq!(da, db, "the two devices hold the same replicated state");
    let vr = b.cell().verify().unwrap(/* t */);
    assert!(vr.ok(), "B verifies: {vr:?}");
    assert_eq!(
        b.cell().get(&note(9, "").id).unwrap(/* t */).body,
        "la nota dell'era firmata"
    );
}

/// The gate at acceptance: a caller whose transport key is not a live
/// device's is refused before any exchange.
#[tokio::test]
async fn a_stranger_is_refused_before_any_exchange() {
    let dir_a = tempfile::tempdir().unwrap(/* t */);
    let key_a = DeviceKey::load_or_create(dir_a.path()).unwrap(/* t */);
    let reg_a = DeviceRegistry::open(dir_a.path()).unwrap(/* t */);
    reg_a.create_first(&key_a, "portatile").unwrap(/* t */);
    let stranger = DeviceKey::from_seed([9; 32]);
    assert!(authorize(&stranger.transport_public(), &reg_a).is_err());
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
}

// ---------------------------------------------------------------------------
// the gate's refusals (P1.4): one test per refusal, each a sealed envelope
// exactly like the wire's, each refused without note text
// ---------------------------------------------------------------------------

/// A SIGNED op by A, a genuine one — the baseline the refusals bend.
fn signed_put(a: &SharedCell, body: &str) -> Op {
    a.cell().put_note(&note(20, body), "cli").unwrap(/* t */);
    a.cell()
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .rev()
        .find(|o| o.kind.as_str() == "put_note" && o.sig.is_some())
        .expect(/* just written */ "the signed put")
}

/// 1. an ALTERED signature does not verify: refused.
#[test]
fn an_altered_signature_is_refused() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    let mut op = signed_put(&a, "la nota autentica");
    // flip one byte of the signature
    let mut sig = op.sig.clone().unwrap_or_default();
    let flip = if sig.starts_with('A') { 'B' } else { 'A' };
    sig.replace_range(0..1, &flip.to_string());
    op.sig = Some(sig);
    let (epoch, id, nonce, ct) = envelope_of_op(&a, &op);
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* t */);
    assert!(
        err.to_string().contains("signature"),
        "a tampered signature must be refused: {err}"
    );
    // and the genuine one passes
    let op = signed_put(&a, "la nota autentica");
    let (epoch, id, nonce, ct) = envelope_of_op(&a, &op);
    b.ingest(epoch, &id, &nonce, &ct).unwrap(/* t */);
}

/// 2. a device the list never knew: refused.
#[test]
fn an_unknown_device_is_refused() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    let stranger = DeviceKey::from_seed([3; 32]);
    let op = signed_put(&a, "la nota di nessuno");
    // re-sign with the stranger's key, naming the stranger's device
    let mut op = op;
    op.device = Some(stranger.device_id());
    op.sig = Some({
        use base64ct::{Base64UrlUnpadded as B64, Encoding};
        B64::encode_string(&stranger.sign(op.content_hash.as_bytes()))
    });
    let (epoch, id, nonce, ct) = envelope_of_op(&a, &op);
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* t */);
    assert!(
        err.to_string().contains("unknown") || err.to_string().contains("signature"),
        "an unknown device must be refused: {err}"
    );
}

/// 3. a REMOVED device whose op is outside the set its remove accepted
///    (at or after the remove in total order): refused.
#[test]
fn a_removed_device_outside_its_set_is_refused() {
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
    reg_a.remove(&fx.key_a, &doomed.device_id(), "root", 0).unwrap(/* t */);
    // B learns the add and the remove
    let reg_b = DeviceRegistry::open(fx.dir_b.path()).unwrap(/* t */);
    reg_b.arrive_rows(&reg_a.rows_json()).unwrap(/* t */);
    // the removed device writes AFTER its removal — outside the set
    let mut op = signed_put(&a, "la nota del rimosso");
    op.device = Some(doomed.device_id());
    op.sig = Some({
        use base64ct::{Base64UrlUnpadded as B64, Encoding};
        B64::encode_string(&doomed.sign(op.content_hash.as_bytes()))
    });
    let (epoch, id, nonce, ct) = envelope_of_op(&a, &op);
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* t */);
    assert!(
        err.to_string().contains("removed"),
        "a removed device's late op must be refused: {err}"
    );
}

/// 4. an UNSIGNED op: refused without an adopt, refused when the root does
///    not cover it, admitted when it does.
#[test]
fn an_unsigned_op_needs_its_adopt_cover() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    // the pre-era ops (init, three puts) are unsigned; B holds no adopt yet
    let pre: Vec<Op> = a
        .cell()
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .take_while(|o| o.kind.as_str() != "adopt")
        .collect();
    assert!(pre.iter().all(|o| o.sig.is_none()));
    let (epoch, id, nonce, ct) = envelope_of_op(&a, &pre[0]);
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* t */);
    assert!(
        err.to_string().contains("adopt"),
        "an unsigned op without an adopt must be refused: {err}"
    );
    // A adopts; the adopt itself travels first and opens the door
    a.cell().adopt_log("cli").unwrap(/* t */);
    let adopt: Op = a
        .cell()
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .find(|o| o.kind.as_str() == "adopt")
        .expect(/* just written */ "the adopt");
    let (epoch, id, nonce, ct) = envelope_of_op(&a, &adopt);
    b.ingest(epoch, &id, &nonce, &ct).unwrap(/* t */);
    // now every pre-era op is covered, one by one, whatever the order
    for op in &pre {
        let (epoch, id, nonce, ct) = envelope_of_op(&a, op);
        b.ingest(epoch, &id, &nonce, &ct)
            .unwrap_or_else(|e| panic!("the covered op {} must pass: {e}", op.op_id));
    }
    // but a FORGED unsigned op moves the root: refused
    let forged = engramma_core::cell::ops::make_op(OpParts {
        op_id: Ulid::from_string("01F7ZQ6VTSATW2BQ5Z9R0P1S8T").unwrap(/* t */),
        hlc: engramma_core::hlc::Hlc {
            physical_ms: 1_700_000_000_000,
            counter: 0,
            node: "forged000000".into(),
        },
        actor: "cli".into(),
        kind: engramma_core::cell::ops::OpKind::PutNote,
        note_id: Some(note(31, "nota").id),
        base: None,
        body: serde_json::json!({ "note": note(31, "nota").to_json() }),
        salt: vec![1_u8; 16],
        device: None,
        sig: None,
    })
    .unwrap(/* t */);
    // the forger sends no proof (or a bogus one — either walks nowhere)
    let id = unhex32(&forged.content_hash);
    let reg_key = KeyStore::new(a.registry().conn())
        .key_of(a.sync_id(), 0)
        .unwrap(/* t */)
        .expect(/* armed */ "epoch 0");
    let aad = op_aad(a.sync_id(), 0, &id);
    let env = crypto::seal_op(
        &reg_key,
        &aad,
        wire::envelope_wire(&forged, None).as_bytes(),
    )
    .unwrap(/* t */);
    let err = b.ingest(0, &id, &env.nonce, &env.ct).unwrap_err(/* t */);
    assert!(
        err.to_string().contains("adopt"),
        "a forged unsigned op must be refused: {err}"
    );
    // and with a proof built over a DIFFERENT set, the walk misses the root
    let mut alien_set: Vec<String> = (0..4)
        .map(|i| blake3::hash(&[i; 8]).to_hex().to_string())
        .chain(std::iter::once(forged.content_hash.clone()))
        .collect();
    let alien_path = engramma_core::cell::ops::merkle_prove(&mut alien_set, &forged.content_hash)
            .unwrap(/* t */);
    let env = crypto::seal_op(
        &reg_key,
        &aad,
        wire::envelope_wire(&forged, Some(&alien_path)).as_bytes(),
    )
    .unwrap(/* t */);
    let err = b.ingest(0, &id, &env.nonce, &env.ct).unwrap_err(/* t */);
    assert!(
        err.to_string().contains("adopt"),
        "an alien proof must be refused: {err}"
    );
}

/// 5. an envelope of an epoch this device holds no key for: refused.
#[test]
fn an_envelope_of_an_unknown_epoch_is_refused() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    let op = signed_put(&a, "la nota dell'epoca nuova");
    // rotate to epoch 1 on A (a removal would in production)
    let reg_a = DeviceRegistry::open(fx.dir_a.path()).unwrap(/* t */);
    KeyStore::new(reg_a.conn()).rotate(&fx.sync_id).unwrap(/* t */);
    let (epoch, id, nonce, ct) = envelope_of_op(&a, &op);
    assert_eq!(epoch, 1, "A now seals with the rotated epoch");
    let err = b.ingest(epoch, &id, &nonce, &ct).unwrap_err(/* t */);
    assert!(
        err.to_string().contains("key") || err.to_string().contains("envelope"),
        "an epoch B holds no key for must be refused: {err}"
    );
}

/// 6. an envelope sealed for ANOTHER cell (wrong sync id in the AAD): the
///    seal itself refuses it.
#[test]
fn an_envelope_of_another_cell_is_refused() {
    let fx = Fixture::new();
    let a = fx.side_a();
    let b = fx.side_b();
    let op = signed_put(&a, "la nota dell'altra cella");
    let id = unhex32(&op.content_hash);
    // seal with the SAME key but the AAD of a different cell
    let other_sync_id = EpochKey::generate().unwrap(/* t */).cell_sync_id();
    let key = KeyStore::new(a.registry().conn())
        .key_of(a.sync_id(), 0)
        .unwrap(/* t */)
        .expect(/* armed */ "epoch 0");
    let aad = op_aad(&other_sync_id, 0, &id);
    let env = crypto::seal_op(&key, &aad, wire::op_wire(&op).as_bytes()).unwrap(/* t */);
    let err = b
        .ingest(0, &id, &env.nonce, &env.ct)
        .unwrap_err(/* t */);
    assert!(
        err.to_string().contains("open"),
        "another cell's envelope must not open: {err}"
    );
}
