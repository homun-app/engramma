// SPDX-License-Identifier: Apache-2.0

//! R2 (v0.6.1, revisore): the relay really exists. iroh's LOCAL relay
//! (test-utils, loopback only) carries the whole road — the pairing AND
//! the sync — with the direct connections IMPEDITE (`clear_ip_transports`:
//! no IP transport is even bound, the relay is the only road there is).
//! The pass summary must say which road each session rode, and a root with
//! `--no-relay` must fail the same round CLEANLY — a summary row, not an
//! error.

use std::time::Duration;

use engramma_core::format::{Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use engramma_sync::device::{DeviceKey, hex};
use engramma_sync::pairing_net::{self, PAIRING_ALPN};
use engramma_sync::protocol::{self, authorize};
use engramma_sync::relay::{self, RelayChoice};
use engramma_sync::serve;
use engramma_sync::sharedcell::share_cell;
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

/// Two roots and one shared cell with a few notes on A — the production
/// shape of the pairing tests, minus the direct roads.
struct Pair {
    dir_a: tempfile::TempDir,
    dir_b: tempfile::TempDir,
    key_a: DeviceKey,
    key_b: DeviceKey,
}

impl Pair {
    fn new() -> Self {
        let dir_a = tempfile::tempdir().unwrap(/* t */);
        let dir_b = tempfile::tempdir().unwrap(/* t */);
        std::fs::create_dir_all(dir_a.path().join("cells")).unwrap(/* t */);
        std::fs::create_dir_all(dir_b.path().join("cells")).unwrap(/* t */);
        let key_a = DeviceKey::load_or_create(dir_a.path()).unwrap(/* t */);
        let key_b = DeviceKey::load_or_create(dir_b.path()).unwrap(/* t */);
        Self {
            dir_a,
            dir_b,
            key_a,
            key_b,
        }
    }

    fn cell_of_a(&self) -> Cell {
        Cell::open(&self.dir_a.path().join("cells").join("memoria.sqlite")).unwrap(/* t */)
    }

    fn cell_of_b(&self) -> Cell {
        Cell::open(&self.dir_b.path().join("cells").join("memoria.sqlite")).unwrap(/* t */)
    }
}

/// The pairing and the sync, BOTH riding the local relay only: the
/// endpoints bind with no IP transports at all, the ticket carries the
/// relay's URL, the address book carries it too, and the pass summary
/// says «relay».
#[tokio::test(flavor = "current_thread")]
async fn pairing_and_sync_ride_the_relay_and_the_summary_says_so() {
    let bound =
        tokio::time::timeout(Duration::from_secs(180), async {
            // the local relay (loopback only, self-signed certificate)
            let (_relay_map, relay_url, _server) = iroh::test_utils::run_relay_server()
                .await
                .expect(/* loopback */ "the local relay");
            let choice = RelayChoice::Own(relay_url.to_string());
            let p = Pair::new();
            // A's side: registry, cell, notes, share, adopt
            {
                let reg =
                    engramma_sync::registry::DeviceRegistry::open(p.dir_a.path()).unwrap(/* t */);
                reg.create_first(&p.key_a, "portatile").unwrap(/* t */);
                let cell = Cell::create(
                &p.dir_a.path().join("cells").join("memoria.sqlite"),
                "conv",
                "cli",
            )
            .unwrap(/* t */);
                for i in 1..=3_u32 {
                    cell.put_note(&note(i, &format!("nota {i} di A")), "cli").unwrap(/* t */);
                }
                drop(cell);
                share_cell(p.dir_a.path(), "memoria").unwrap(/* t */);
                let cell = p.cell_of_a();
                // the signer is armed BEFORE the adopt, as `sync share` does:
                // the adopt itself must be signed, or it could never enter
                cell.set_signer(std::sync::Arc::new(
                engramma_sync::arrival::DeviceSigner::new(&p.key_a),
            ))
            .unwrap(/* t */);
                cell.adopt_log("cli").unwrap(/* t */);
                cell.mark_shared().unwrap(/* t */);
            }
            tokio::task::LocalSet::new()
            .run_until(async {
                // both endpoints: the relay is the ONLY road (no IP
                // transports bound at all)
                let ep_a = relay::bind_relay_only(&p.key_a, "0.0.0.0:0", PAIRING_ALPN, &choice)
                    .await
                    .unwrap(/* t */);
                let reg = engramma_sync::registry::DeviceRegistry::open(p.dir_a.path())
                    .unwrap(/* t */);
                // the PRODUCTION wait lives in ticket_of (C1): the test
                // does not call online() — that was the hole the reviewer
                // found, production never waited
                let minted = pairing_net::ticket_of(&ep_a, &choice)
                    .await
                    .unwrap(/* t */);
                let ticket = minted.ticket;
                assert_eq!(
                    minted.relay.as_deref(),
                    Some(relay_url.as_str()),
                    "the ticket must carry the relay's URL"
                );
                assert!(
                    ticket.relay == relay_url.to_string(),
                    "the ticket carries the relay it rides: {:?}",
                    ticket.relay
                );
                assert!(
                    ticket.addrs.is_empty(),
                    "no direct address with the IP transports cleared"
                );
                let invite = {
                    let ep = ep_a.clone();
                    let reg = engramma_sync::registry::DeviceRegistry::open(p.dir_a.path())
                        .unwrap(/* t */);
                    let dir = p.dir_a.path().to_path_buf();
                    let ticket = ticket.clone();
                    tokio::task::spawn_local(async move {
                        pairing_net::invite(&ep, &reg, &dir, &ticket, |_| true).await
                    })
                };
                // the joiner rides the relay too
                let ep_b = relay::bind_relay_only(&p.key_b, "0.0.0.0:0", PAIRING_ALPN, &choice)
                    .await
                    .unwrap(/* t */);
                let join = {
                    let ep = ep_b.clone();
                    let ticket = ticket.clone();
                    let dir = p.dir_b.path().to_path_buf();
                    tokio::task::spawn_local(async move {
                        pairing_net::join(&ep, &ticket, "mac", &dir, |_| true).await
                    })
                };
                join.await.expect(/* the join */ "join").expect(/* ok */ "the join flow");
                invite.await.expect(/* the invite */ "invite").expect(/* ok */ "the invite flow");
                // the pairing moved the cell to B
                assert!(
                    p.cell_of_b().ops().unwrap(/* t */).len() >= 5,
                    "B holds the cell (init, puts, adopt, …)"
                );
                let _ = reg;

                // --- the sync: A answers, B runs the pass, all over the relay
                let sync_ep_a = relay::bind_relay_only(
                    &p.key_a,
                    "0.0.0.0:0",
                    protocol::ALPN,
                    &choice,
                )
                .await
                .unwrap(/* t */);
                let answerer = {
                    let ep = sync_ep_a.clone();
                    let dir = p.dir_a.path().to_path_buf();
                    tokio::task::spawn_local(async move {
                        while let Some(incoming) = ep.accept().await {
                            let Ok(conn) = incoming.await else {
                                continue;
                            };
                            let reg = engramma_sync::registry::DeviceRegistry::open(&dir)
                                .unwrap(/* t */);
                            if authorize(conn.remote_id().as_bytes(), &reg).is_err() {
                                continue;
                            }
                            let _ = serve::answer_session(&dir, conn).await;
                        }
                    })
                };
                // a note on A after the pairing: the pass must carry it to B
                // (the signer armed, as every local write of the era)
                {
                    let cell = p.cell_of_a();
                    cell.set_signer(std::sync::Arc::new(
                        engramma_sync::arrival::DeviceSigner::new(&p.key_a),
                    ))
                    .unwrap(/* t */);
                    cell.put_note(&note(9, "la nota dopo l'abbinamento"), "cli").unwrap(/* t */);
                }
                let sync_ep_b = relay::bind_relay_only(
                    &p.key_b,
                    "0.0.0.0:0",
                    protocol::ALPN,
                    &choice,
                )
                .await
                .unwrap(/* t */);
                let mut report = serve::run_pass(p.dir_b.path(), &sync_ep_b)
                    .await
                    .expect(/* a pass */ "the pass");
                for _ in 0..3 {
                    if p.cell_of_b().ops().unwrap(/* t */).len()
                        == p.cell_of_a().ops().unwrap(/* t */).len()
                    {
                        break;
                    }
                    report = serve::run_pass(p.dir_b.path(), &sync_ep_b)
                        .await
                        .expect(/* a pass */ "the pass");
                }
                answerer.abort();
                // the summary says WHICH road the session rode
                let peer = report
                    .peers
                    .iter()
                    .find(|x| x.name == "portatile")
                    .expect(/* the row */ "the peer's row");
                assert!(peer.reached, "why not: {:?}", peer.why_not);
                assert_eq!(peer.via, Some("relay"), "the summary must say relay");
                // and the two copies agree
                assert_eq!(
                    p.cell_of_a().replicated_digest().unwrap(/* t */),
                    p.cell_of_b().replicated_digest().unwrap(/* t */),
                    "the relay carried the whole cell"
                );
                assert_eq!(
                    p.cell_of_b().get(&note(9, "").id).unwrap(/* t */).body,
                    "la nota dopo l'abbinamento"
                );
            })
            .await;
        })
        .await;
    bound.expect("il giro via relay non è finito in 180 s");
}

/// With `--no-relay` the SAME round fails CLEANLY: a row of the summary
/// («non raggiunto», why), never an error — the relay URL in the book is
/// unusable when no relay is configured.
#[tokio::test(flavor = "current_thread")]
async fn no_relay_fails_the_same_round_cleanly() {
    let bound = tokio::time::timeout(Duration::from_secs(120), async {
        let (_relay_map, relay_url, _server) = iroh::test_utils::run_relay_server()
            .await
            .expect(/* loopback */ "the local relay");
        let p = Pair::new();
        {
            let reg = engramma_sync::registry::DeviceRegistry::open(p.dir_a.path()).unwrap(/* t */);
            reg.create_first(&p.key_a, "portatile").unwrap(/* t */);
            let key_b = DeviceKey::load_or_create(p.dir_b.path()).unwrap(/* t */);
            reg.add(
                &p.key_a,
                &key_b.device_id(),
                "mac",
                &hex(&key_b.transport_public()),
                &hex(&key_b.hpke_public()),
            )
            .unwrap(/* t */);
            engramma_sync::registry::DeviceRegistry::open(p.dir_b.path())
                .unwrap(/* t */)
                .arrive_rows(&reg.rows_json())
                .unwrap(/* t */);
            let cell = Cell::create(
                &p.dir_a.path().join("cells").join("memoria.sqlite"),
                "conv",
                "cli",
            )
            .unwrap(/* t */);
            cell.put_note(&note(1, "una nota"), "cli").unwrap(/* t */);
            drop(cell);
            share_cell(p.dir_a.path(), "memoria").unwrap(/* t */);
        }
        tokio::task::LocalSet::new()
            .run_until(async {
                // A rides the relay; B's book knows A ONLY by the relay
                let ep_a = relay::bind_relay_only(
                    &p.key_a,
                    "0.0.0.0:0",
                    protocol::ALPN,
                    &RelayChoice::Own(relay_url.to_string()),
                )
                .await
                .unwrap(/* t */);
                // the book learns from the TICKET, exactly as the join
                // does (the production wait is in ticket_of)
                let minted = pairing_net::ticket_of(&ep_a, &RelayChoice::Own(relay_url.to_string()))
                    .await
                    .unwrap(/* t */);
                let t = &minted.ticket;
                let mut addr = iroh::EndpointAddr::new(
                    iroh::PublicKey::from_bytes(&t.node_id)
                        .expect(/* just minted */ "node id"),
                );
                for a in &t.addrs {
                    if let Ok(socket) = a.parse() {
                        addr = addr.with_ip_addr(socket);
                    }
                }
                if let Ok(url) = t.relay.parse() {
                    addr = addr.with_relay_url(url);
                }
                let a_device = p.key_a.device_id();
                serve::remember_addr(p.dir_b.path(), &a_device, &addr).unwrap(/* t */);
                // B: --no-relay — the same round, without the relay (a
                // normal endpoint, direct transports, no relay configured)
                let ep_b = relay::bind(
                    &p.key_b,
                    "0.0.0.0:0",
                    protocol::ALPN,
                    &RelayChoice::Disabled,
                )
                .await
                .unwrap_or_else(|e| panic!("bind senza relay: {e}"));
                let report = serve::run_pass(p.dir_b.path(), &ep_b)
                    .await
                    .expect(/* a pass never fails on a peer */ "the pass");
                let peer = report
                    .peers
                    .iter()
                    .find(|x| x.name == "portatile")
                    .expect(/* the row */ "the peer's row");
                assert!(!peer.reached, "the round must not pass");
                assert!(
                    peer.why_not.is_some(),
                    "the failure is a summary row, not an error"
                );
                assert!(peer.via.is_none());
            })
            .await;
    })
    .await;
    bound.expect("il giro senza relay non è finito in 120 s");
}
