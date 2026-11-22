
// SPDX-License-Identifier: Apache-2.0

//! R4 (v0.6.1, revisore): no envelope is lost in silence. An operation
//! this copy holds but CANNOT seal or prove — here: an unsigned operation
//! outside the adopt tree, written by an unarmed hand after the era —
//! must leave a trace: a row of the pass summary (`unsent`) and an EVENT
//! in the root that carries the op id (short), the kind and the reason,
//! and NOTHING of the note's text.

mod common;

use engramma_core::cell::ops::{self, OpKind, OpParts};
use engramma_core::format::{Note, NoteType};
use engramma_core::hlc::Hlc;
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

/// An unsigned operation OUTSIDE the adopt tree, ingested by hand — the
/// shape of a write an unarmed process left after the era.
fn orphan_unsigned_put(body: &str) -> engramma_core::cell::ops::Op {
    ops::make_op(OpParts {
        op_id: Ulid::from_string("01HANDMADE0000000000000099").unwrap(/* t */),
        hlc: Hlc {
            physical_ms: 1_790_000_000_000,
            counter: 0,
            node: "orphan000000".into(),
        },
        actor: "cli".into(),
        kind: OpKind::PutNote,
        note_id: Some(note(99, body).id),
        base: None,
        body: serde_json::json!({ "note": note(99, body).to_json() }),
        salt: vec![9_u8; 16],
        device: None,
        sig: None,
    })
    .unwrap(/* t */)
}

/// Two roots, one shared cell: A shares and adopts, an orphan unsigned op
/// lands on A, B pulls everything but the orphan (the gate refuses it),
/// then A's own pass must SAY it could not send it — a row of the summary
/// and an event without note text.
#[tokio::test(flavor = "current_thread")]
async fn an_unsendable_op_is_a_summary_row_and_an_event_without_note_text() {
    let bound = tokio::time::timeout(std::time::Duration::from_secs(120), async {
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
        // A: the cell, the notes, the share, the adopt (signed)
        let cell = Cell::create(
            &dir_a.path().join("cells").join("memoria.sqlite"),
            "conv",
            "cli",
        )
        .unwrap(/* t */);
        for i in 1..=3_u32 {
            cell.put_note(&note(i, &format!("nota {i} di A")), "cli").unwrap(/* t */);
        }
        let sync_id = share_cell(dir_a.path(), "memoria").unwrap(/* t */);
        cell.set_signer(std::sync::Arc::new(DeviceSigner::new(&key_a)))
            .unwrap(/* t */);
        cell.adopt_log("cli").unwrap(/* t */);
        cell.mark_shared().unwrap(/* t */);
        // the orphan: an unsigned op outside the tree, ingested by hand
        let orphan = orphan_unsigned_put("il corpo segreto dell'orfana quokkaviola");
        let orphan_id = {
            let mut bytes = [0_u8; 32];
            for (i, chunk) in orphan.content_hash.as_bytes().chunks(2).enumerate() {
                let hi = (chunk[0] as char).to_digit(16).unwrap_or(0) as u8;
                let lo = (chunk[1] as char).to_digit(16).unwrap_or(0) as u8;
                bytes[i] = (hi << 4) | lo;
            }
            bytes
        };
        cell.ingest_op(&orphan).unwrap(/* t */);
        drop(cell);
        // B: the bare cell, epoch 0, marked shared
        let cell_b = Cell::open_bare(&dir_b.path().join("cells").join("memoria.sqlite"))
            .unwrap(/* t */);
        let epoch0 = KeyStore::new(reg_a.conn())
            .key_of(&sync_id, 0)
            .unwrap(/* t */)
            .expect(/* the sharer minted it */ "epoch 0");
        KeyStore::new(reg_b.conn())
            .store_key(&sync_id, 0, &epoch0)
            .unwrap(/* t */);
        reg_b
            .conn()
            .execute(
                "INSERT INTO shared_cells (name, sync_id) VALUES ('memoria', ?1)
                 ON CONFLICT(name) DO UPDATE SET sync_id = ?1",
                [sync_id],
            )
            .unwrap(/* t */);
        cell_b.mark_shared().unwrap(/* t */);
        drop(cell_b);

        let choice = RelayChoice::Disabled;
        let ep_a = protocol::bind_endpoint(&key_a, "127.0.0.1:0", &choice)
            .await
            .unwrap(/* t */);
        let ep_b = protocol::bind_endpoint(&key_b, "127.0.0.1:0", &choice)
            .await
            .unwrap(/* t */);
        let addr_a = ep_a.addr();

        tokio::task::LocalSet::new()
            .run_until(async {
                // A answers; the first pull (B the initiator) takes the
                // good ops and refuses the orphan at the gate
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
                            // the address book learns B (a real serve loop
                            // does the same at acceptance)
                            if let Ok(book) = serve::peer_addr_of_conn(&conn) {
                                serve::remember_addr(&dir, "mac", &book).ok();
                            }
                            let _ = serve::answer_session(&dir, conn).await;
                        }
                    })
                };
                let b = SharedCell::new(
                    Cell::open_bare(&dir_b.path().join("cells").join("memoria.sqlite"))
                        .unwrap(/* t */),
                    DeviceRegistry::open(dir_b.path()).unwrap(/* t */),
                    sync_id,
                    dir_b.path(),
                );
                for _ in 0..3 {
                    let reg_b = DeviceRegistry::open(dir_b.path()).unwrap(/* t */);
                    let ks_b = KeyStore::new(reg_b.conn());
                    let conn = protocol::connect(&ep_b, addr_a.clone())
                        .await
                        .unwrap(/* t */);
                    let ctx_b = protocol::SessionCtx {
                        registry: &reg_b,
                        keys: ks_b,
                        device: &key_b,
                        cells: vec![&b as &dyn SyncedCell],
                    };
                    let _ = protocol::run_initiator(conn, &ctx_b).await;
                    let held = b
                        .cell()
                        .debug_column("SELECT op_id FROM ops")
                        .unwrap_or_default()
                        .len();
                    if held >= 5 {
                        break; // init + 3 puts + the adopt (the orphan refused)
                    }
                }
                // A's book learns B's real endpoint (a later successful
                // dial would do the same)
                serve::remember_addr(dir_a.path(), &key_b.device_id(), &ep_b.addr())
                    .unwrap(/* t */);
                // B answers A's pass (the answerer of ep_a stays too)
                let answerer_b = {
                    let ep = ep_b.clone();
                    let dir = dir_b.path().to_path_buf();
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
                // A's own pass: the only op B lacks is the orphan — the
                // one A cannot seal. The summary must SAY it.
                let report = serve::run_pass(dir_a.path(), &ep_a, &[])
                    .await
                    .expect(/* a pass */ "the pass");
                answerer.abort();
                answerer_b.abort();
                let summary = format!("{:?}", report.cells);
                assert!(
                    summary.contains("unsent: 1"),
                    "the summary must count the unsendable op: {summary}"
                );
                // and the EVENT: one row for the cell, op id short, kind,
                // reason — and nothing of the note's text (read from the
                // root's own store, as `sync status` will)
                let events: Vec<(String, String, String, String, String)> = {
                    let conn = rusqlite::Connection::open(dir_a.path().join("devices.db"))
                        .expect(/* the root */ "devices.db");
                    let mut stmt = conn
                        .prepare(
                            "SELECT cell, at, op_id, kind, reason FROM sync_events \
                             ORDER BY rowid",
                        )
                        .expect(/* the table exists */ "the sync_events table");
                    let rows = stmt
                        .query_map([], |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                            ))
                        })
                        .unwrap(/* t */);
                    rows.flatten().collect()
                };
                assert_eq!(events.len(), 1, "exactly one event: {events:?}");
                let (cell, _at, op_id, kind, reason) = &events[0];
                assert_eq!(cell, "memoria");
                assert_eq!(op_id.len(), 16, "the op id travels short: {op_id}");
                assert!(orphan.op_id.to_string().starts_with(op_id));
                assert_eq!(kind, "put_note");
                assert!(!reason.is_empty());
                for (what, text) in [
                    ("the event", format!("{cell}{op_id}{kind}{reason}")),
                    ("the summary", summary),
                ] {
                    assert!(
                        !text.contains("quokkaviola") && !text.contains("segreto"),
                        "{what} must carry no note text: {text}"
                    );
                }
                // the orphan itself is still held (it is not lost — it is
                // named, and the next pass will say it again)
                let a_again = SharedCell::all_of(dir_a.path()).unwrap(/* t */);
                assert!(
                    a_again[0]
                        .cell()
                        .holds_content_hash(&orphan.content_hash)
                        .unwrap_or(false)
                );
                let _ = orphan_id;
            })
            .await;
    })
    .await;
    bound.expect("il giro dell'orfana non è finito in 120 s");
}
