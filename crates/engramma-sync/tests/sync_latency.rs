// SPDX-License-Identifier: Apache-2.0

//! The latency measure of v0.6.1 (P5.3): FIFTY local writes, then the pass
//! that carries them. `#[ignore]`d — a measurement, not a test:
//!
//! ```text
//! cargo test --release -p engramma-sync --test sync_latency -- --ignored --nocapture
//! ```
//!
//! What it reports: the cost of each LOCAL write (the signer armed — the
//! era's shape), the wall time of the pass that pulls all fifty, and the
//! incremental pass that follows ONE more write (the steady-state latency
//! a `sync serve` sees at every local write).

mod common;

use engramma_core::format::{Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use engramma_sync::arrival::DeviceSigner;
use engramma_sync::device::{DeviceKey, hex};
use engramma_sync::keystore::KeyStore;
use engramma_sync::protocol::{self, SyncedCell, authorize};
use engramma_sync::registry::DeviceRegistry;
use engramma_sync::relay::RelayChoice;
use engramma_sync::serve;
use engramma_sync::sharedcell::{SharedCell, share_cell};
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
        title: Some(format!("nota {id}")),
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

#[tokio::test(flavor = "current_thread")]
#[ignore = "una misura, non un test: cargo test -- --ignored --nocapture"]
async fn the_fifty_writes_latency() {
    let bound = tokio::time::timeout(std::time::Duration::from_secs(300), async {
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
        let cell = Cell::create(
            &dir_a.path().join("cells").join("memoria.sqlite"),
            "conv",
            "cli",
        )
        .unwrap(/* t */);
        cell.put_note(&note(1, "la base"), "cli").unwrap(/* t */);
        let sync_id = share_cell(dir_a.path(), "memoria").unwrap(/* t */);
        cell.set_signer(std::sync::Arc::new(DeviceSigner::new(&key_a)))
            .unwrap(/* t */);
        cell.adopt_log("cli").unwrap(/* t */);
        cell.mark_shared().unwrap(/* t */);
        // B: bare, epoch 0, shared
        let cell_b = Cell::open_bare(&dir_b.path().join("cells").join("memoria.sqlite"))
            .unwrap(/* t */);
        let epoch0 = KeyStore::new(reg_a.conn())
            .key_of(&sync_id, 0)
            .unwrap(/* t */)
            .expect(/* minted */ "epoch 0");
        KeyStore::new(reg_b.conn())
            .store_key(&sync_id, 0, &epoch0)
            .unwrap(/* t */);
        reg_b
            .conn()
            .execute(
                "INSERT INTO shared_cells (name, sync_id) VALUES ('memoria', ?1)",
                [sync_id],
            )
            .unwrap(/* t */);
        cell_b.mark_shared().unwrap(/* t */);

        let choice = RelayChoice::Disabled;
        let ep_a = protocol::bind_endpoint(&key_a, "127.0.0.1:0", &choice)
            .await
            .unwrap(/* t */);
        let ep_b = protocol::bind_endpoint(&key_b, "127.0.0.1:0", &choice)
            .await
            .unwrap(/* t */);
        tokio::task::LocalSet::new()
            .run_until(async {
                let answerer = {
                    let ep = ep_a.clone();
                    let dir = dir_a.path().to_path_buf();
                    tokio::task::spawn_local(async move {
                        while let Some(incoming) = ep.accept().await {
                            let Ok(conn) = incoming.await else {
                                continue;
                            };
                            let reg = DeviceRegistry::open(&dir).unwrap(/* t */);
                            if authorize(conn.remote_id().as_bytes(), &reg).is_err() {
                                continue;
                            }
                            let _ = serve::answer_session(&dir, conn).await;
                        }
                    })
                };
                serve::remember_addr(dir_b.path(), &key_a.device_id(), &ep_a.addr())
                    .unwrap(/* t */);
                // the first pull with the bare-cell adapter (the pass
                // driver skips a cell with no init op yet — the pairing
                // normally delivers it)
                let b_bare = SharedCell::new(
                    Cell::open_bare(&dir_b.path().join("cells").join("memoria.sqlite"))
                        .unwrap(/* t */),
                    DeviceRegistry::open(dir_b.path()).unwrap(/* t */),
                    sync_id,
                    dir_b.path(),
                );
                for _ in 0..3 {
                    let reg_b = DeviceRegistry::open(dir_b.path()).unwrap(/* t */);
                    let ks_b = KeyStore::new(reg_b.conn());
                    let conn = protocol::connect(&ep_b, ep_a.addr())
                        .await
                        .unwrap(/* t */);
                    let ctx_b = protocol::SessionCtx {
                        registry: &reg_b,
                        keys: ks_b,
                        device: &key_b,
                        cells: vec![&b_bare as &dyn SyncedCell],
                    };
                    let _ = protocol::run_initiator(conn, &ctx_b).await;
                    let held = b_bare
                        .cell()
                        .debug_column("SELECT op_id FROM ops")
                        .unwrap_or_default()
                        .len();
                    if held >= 2 {
                        break;
                    }
                }

                // --- the fifty writes, timed one by one
                let t0 = std::time::Instant::now();
                for i in 100..150_u32 {
                    cell.put_note(&note(i, &format!("il corpo numero {i}")), "cli")
                        .unwrap(/* t */);
                }
                let writes = t0.elapsed();
                // --- the pass that carries all fifty
                let t1 = std::time::Instant::now();
                let report = serve::run_pass(dir_b.path(), &ep_b, &[])
                    .await
                    .expect(/* a pass */ "the pass");
                let pass = t1.elapsed();
                let received: usize = report.cells.iter().map(|c| c.received).sum();
                // --- the steady state: ONE write, one pass
                cell.put_note(&note(200, "una in più"), "cli").unwrap(/* t */);
                let t2 = std::time::Instant::now();
                let _ = serve::run_pass(dir_b.path(), &ep_b, &[]).await;
                let one = t2.elapsed();
                answerer.abort();
                let per_write = writes.as_millis() as f64 / 50.0;
                let per_op = pass.as_secs_f64() / received.max(1) as f64 * 1000.0;
                eprintln!(
                    "latenza: 50 scritture locali in {writes:?} ({per_write:.2} ms/scrittura), \
                     la passata che le porta {pass:?} ({received} ricevute, \
                     {per_op:.2} ms/operazione), una scrittura in più con la sua passata {one:?}",
                );
            })
            .await;
    })
    .await;
    bound.expect("la misura di latenza non è finita in 300 s");
}
