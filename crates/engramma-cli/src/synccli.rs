// SPDX-License-Identifier: Apache-2.0

//! The sync, devices and mailbox commands (v0.6.1, P2–P4): the CLI surface
//! over the crate's production road — the pairing flows over iroh, the
//! one-pass `sync run` with its JSON summary, the `sync serve` loop (the
//! gate at acceptance, a sync at every local write and every five
//! minutes, a clean stop on the signals), the real `sync share` and
//! `devices remove`, and the blind mailbox node.

use std::io::{BufRead, Write};

use engramma_sync::device::{DeviceKey, hex};
use engramma_sync::keystore::KeyStore;
use engramma_sync::pairing::Ticket;
use engramma_sync::pairing_net;
use engramma_sync::registry::DeviceRegistry;
use engramma_sync::relay::{self, RelayChoice};
use engramma_sync::serve;
use engramma_sync::sharedcell::{self, SharedCell};

use crate::{CliError, print_json};

/// Ask the human one yes line: the SAS confirmation.
fn confirm_line(sas: &str) -> bool {
    print!("codice di verifica: {sas} — confermi che è lo stesso sull'altro schermo? [s/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(
        line.trim().to_lowercase().as_str(),
        "s" | "si" | "sì" | "y" | "yes"
    )
}

/// `engramma devices invite [--relay <url>|--no-relay]`: bind the pairing
/// endpoint, print the ticket with the REAL address, wait (ten minutes,
/// one use) for the device that enters.
pub fn devices_invite(
    root: &std::path::Path,
    json: bool,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    let root = root.to_path_buf();
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let reg = DeviceRegistry::open(&root).map_err(|e| CliError::internal(e.to_string()))?;
    if reg
        .live_members()
        .map_err(|e| CliError::internal(e.to_string()))?
        .is_empty()
    {
        // the first device creates the list by itself
        reg.create_first(&dk, "portatile")
            .map_err(|e| CliError::internal(e.to_string()))?;
    }
    let choice =
        relay::resolve(&root, flag.as_ref()).map_err(|e| CliError::internal(e.to_string()))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(e.to_string()))?;
    rt.block_on(async {
        tokio::task::LocalSet::new()
            .run_until(async {
                let endpoint = pairing_net::bind_pairing(&dk, "0.0.0.0:0", &choice)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let minted = pairing_net::ticket_of(&endpoint, &choice)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let ticket = minted.ticket;
                // a relay that never connected must be SAID, never silent
                // (C1): behind a NAT the ticket would be unusable
                if minted.relay_timed_out {
                    eprintln!(
                        "avviso: il relay non si è collegato in {} s — il \
                         biglietto esce senza relay (chi è dietro NAT non ti troverà)",
                        engramma_sync::relay::RELAY_WAIT.as_secs()
                    );
                }
                let addrs = serve::direct_addresses(&endpoint);
                if json {
                    let mut out = serde_json::json!({
                        "ticket": ticket.encode(),
                        "node": hex(serve::node_id_of(&endpoint).as_slice()),
                        "addrs": addrs,
                        "expires_at": ticket.expires_at,
                        "relay": minted.relay,
                    });
                    if minted.relay_timed_out {
                        out["relay_warning"] = serde_json::json!(
                            "il relay non si è collegato: il biglietto esce senza relay"
                        );
                    }
                    print_json(&out);
                } else {
                    println!("biglietto (dieci minuti, un uso):");
                    println!("{}", ticket.encode());
                    if let Some(relay) = &minted.relay {
                        println!("relay: {relay}");
                    }
                    println!("in attesa del dispositivo che entra…");
                }
                let root2 = root.clone();
                let report = pairing_net::invite(&endpoint, &reg, &root2, &ticket, confirm_line)
                    .await
                    .map_err(|e| CliError::internal(format!("abbinamento: {e}")))?;
                if !json {
                    for (name, ops) in &report.cells {
                        println!("cella «{name}»: {ops} operazioni consegnate");
                    }
                    println!("dispositivo aggiunto all'elenco");
                }
                Ok(())
            })
            .await
    })
}

/// `engramma devices join <ticket> --name <nome>`: the ticket way in —
/// SPAKE2 over the ticket secret, the inviter's MAC, the key confirmation,
/// the SAS on both screens, then the registry, the keys and the cells.
pub fn devices_join(
    root: &std::path::Path,
    json: bool,
    ticket_or_code: &str,
    name: &str,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    // the code way in meets at the blind node (P4)
    if !ticket_or_code.starts_with("engramma-invite-v1") {
        let root = root.to_path_buf();
        return mailbox_join_with_code(&root, json, ticket_or_code, name, flag);
    }
    let root = root.to_path_buf();
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let ticket = Ticket::decode(ticket_or_code)
        .map_err(|e| CliError::usage(format!("biglietto non valido: {e}")))?;
    let choice =
        relay::resolve(&root, flag.as_ref()).map_err(|e| CliError::internal(e.to_string()))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(e.to_string()))?;
    rt.block_on(async {
        tokio::task::LocalSet::new()
            .run_until(async {
                let endpoint = pairing_net::bind_pairing(&dk, "0.0.0.0:0", &choice)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let report = pairing_net::join(&endpoint, &ticket, name, &root, confirm_line)
                    .await
                    .map_err(|e| CliError::internal(format!("ingresso: {e}")))?;
                if json {
                    print_json(&serde_json::json!({
                        "cells": report.cells.iter().map(|(n, c)| serde_json::json!({
                            "name": n, "ops": c,
                        })).collect::<Vec<_>>(),
                    }));
                } else {
                    for (cell, ops) in &report.cells {
                        println!("cella «{cell}» ricevuta: {ops} operazioni");
                    }
                    println!("dentro: `engramma sync run` per la prima passata");
                }
                Ok(())
            })
            .await
    })
}

/// `engramma sync run [--relay <url>|--no-relay]`: one pass with every live
/// device the book reaches, then the mailbox when one is configured. An
/// unreachable device is a row of the summary, never an error.
pub fn sync_run_cmd(
    root: &std::path::Path,
    json: bool,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    let root = root.to_path_buf();
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let choice =
        relay::resolve(&root, flag.as_ref()).map_err(|e| CliError::internal(e.to_string()))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(e.to_string()))?;
    let report = rt.block_on(async {
        tokio::task::LocalSet::new()
            .run_until(async {
                let endpoint = serve::bind(&dk, "0.0.0.0:0", &choice)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                serve::run_pass(&root, &endpoint, &[])
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))
            })
            .await
    })?;
    if json {
        print_json(&serde_json::json!({
            "devices": report.peers.iter().map(|p| serde_json::json!({
                "name": p.name, "device": &p.device[..16.min(p.device.len())],
                "reached": p.reached, "why_not": p.why_not,
                "via": p.via,
            })).collect::<Vec<_>>(),
            "cells": report.cells.iter().map(|c| serde_json::json!({
                "cell": c.name, "sent": c.sent, "received": c.received,
                "refused": c.refused, "conflicts": c.conflicts, "unsent": c.unsent,
                "head": c.head, "frontier": c.frontier,
            })).collect::<Vec<_>>(),
        }));
    } else {
        for p in &report.peers {
            match (&p.why_not, p.reached) {
                (Some(why), _) => println!("{}: non raggiunto — {why}", p.name),
                (None, _) => {
                    let via = p.via.unwrap_or("diretta");
                    println!("{}: sessione fatta ({})", p.name, via);
                }
            }
        }
        for c in &report.cells {
            println!(
                "cella «{}»: {} inviate, {} ricevute, {} rifiuti, {} conflitti, {} non speditibili, frontiera {}",
                c.name, c.sent, c.received, c.refused, c.conflicts, c.unsent, c.frontier
            );
        }
    }
    Ok(())
}

/// `engramma sync serve [--relay <url>|--no-relay]`: accept (the gate
/// first), sync at every local write (data_version, 2 s debounce) and
/// every five minutes, stop clean on SIGINT/SIGTERM. The relay choice is
/// saved in the root: it is asked once, not at every start.
pub fn sync_serve_cmd(
    root: &std::path::Path,
    json: bool,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    let root = root.to_path_buf();
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let choice =
        relay::resolve(&root, flag.as_ref()).map_err(|e| CliError::internal(e.to_string()))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(e.to_string()))?;
    rt.block_on(async {
        tokio::task::LocalSet::new()
            .run_until(async {
                let endpoint = serve::bind(&dk, "0.0.0.0:0", &choice)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                // the address this daemon PUBLISHES must be reachable from
                // whoever reads it: the relay comes first (C1)
                let wait = engramma_sync::relay::wait_relay(&endpoint, &choice).await;
                let relay = match &wait {
                    engramma_sync::relay::RelayWait::Connected(url) => Some(url.clone()),
                    _ => None,
                };
                if wait == engramma_sync::relay::RelayWait::TimedOut {
                    eprintln!(
                        "avviso: il relay non si è collegato in {} s — chi è \
                         dietro NAT non raggiungerà questo servizio",
                        engramma_sync::relay::RELAY_WAIT.as_secs()
                    );
                }
                let addrs = serve::direct_addresses(&endpoint);
                if json {
                    let mut out = serde_json::json!({
                        "serving": true,
                        "node": hex(serve::node_id_of(&endpoint).as_slice()),
                        "addrs": addrs,
                        "relay": relay,
                    });
                    if wait == engramma_sync::relay::RelayWait::TimedOut {
                        out["relay_warning"] = serde_json::json!(
                            "il relay non si è collegato: l'indirizzo esce senza relay"
                        );
                    }
                    print_json(&out);
                } else {
                    println!(
                        "sync serve in ascolto ({})",
                        addrs.first().cloned().unwrap_or_default()
                    );
                }
                // the pass worker: one pass at a time, over the ticks
                enum Tick {
                    /// rows that are NOT the sessions' own work (a local
                    /// write): a pass toward every live device
                    Write,
                    /// only rows the sessions brought: a pass toward the
                    /// others — never back to these sources (K1)
                    Others(Vec<String>),
                    /// the five-minute safety net
                    Clock,
                }
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Tick>();
                // the write guard, shared by the watcher (it polls), the
                // worker (it settles after every pass) and the accept loop
                // (it notes what each inbound session ingested). All the
                // tasks live on this one LocalSet thread: the mutex is
                // held for short sync spans only, never across an await.
                let watch = std::sync::Arc::new(std::sync::Mutex::new(
                    serve::DataVersionWatch::new(&root).unwrap_or_else(|e| {
                        eprintln!("sync serve: data_version: {e}");
                        // an empty watch never fires: the periodic pass
                        // still runs, and the next start retries
                        serve::DataVersionWatch::empty(&root)
                    }),
                ));
                {
                    let tx = tx.clone();
                    let watch = std::sync::Arc::clone(&watch);
                    tokio::task::spawn_local(async move {
                        let mut opened = std::time::Instant::now();
                        loop {
                            tokio::time::sleep(serve::WATCH_TICK).await;
                            if !watch
                                .lock()
                                .map(|w| !w.busy())
                                .unwrap_or(false)
                            {
                                continue;
                            }
                            // cells shared after the start join at the
                            // periodic refresh
                            if opened.elapsed() >= serve::PERIODIC {
                                opened = std::time::Instant::now();
                                if let Ok(mut w) = watch.lock() {
                                    let _ = w.refresh();
                                }
                                continue;
                            }
                            let moved = watch
                                .lock()
                                .ok()
                                .and_then(|w| w.moved().ok())
                                .unwrap_or(false);
                            if !moved {
                                continue;
                            }
                            // let the writes settle, then decide: the
                            // cheap poll stays on the file's counters, the
                            // PASS is armed by the log's marker (K1)
                            tokio::time::sleep(serve::DEBOUNCE).await;
                            let decision = watch.lock().ok().and_then(|mut w| {
                                if w.busy() || !w.moved().unwrap_or(false) {
                                    None
                                } else {
                                    w.evaluate().ok()
                                }
                            });
                            match decision {
                                Some(serve::PassDecision::All) => {
                                    if std::env::var("ENGRAMMA_TRACE").is_ok() {
                                        eprintln!("watch: tick write");
                                    }
                                    tx.send(Tick::Write).ok();
                                }
                                Some(serve::PassDecision::Others(skip)) => {
                                    if std::env::var("ENGRAMMA_TRACE").is_ok() {
                                        eprintln!("watch: tick write (verso gli altri)");
                                    }
                                    tx.send(Tick::Others(skip)).ok();
                                }
                                _ => {}
                            }
                        }
                    });
                }
                // the clock: every five minutes
                {
                    let tx = tx.clone();
                    tokio::task::spawn_local(async move {
                        // the first tick comes AFTER the period: a pass at
                        // t=0 would dial a book that is almost always
                        // stale right after the start (the pairing-era
                        // addresses die with the join) and pile a dead
                        // wait on top of the startup — the write watcher
                        // is the real trigger, the clock is the safety net
                        let mut tick = tokio::time::interval_at(
                            tokio::time::Instant::now() + serve::PERIODIC,
                            serve::PERIODIC,
                        );
                        loop {
                            tick.tick().await;
                            tx.send(Tick::Clock).ok();
                        }
                    });
                }
                // the worker
                let stop = tokio_util::sync::CancellationToken::new();
                {
                    let root = root.clone();
                    let endpoint = endpoint.clone();
                    let stop = stop.clone();
                    let watch = std::sync::Arc::clone(&watch);
                    tokio::task::spawn_local(async move {
                        loop {
                            tokio::select! {
                                _ = stop.cancelled() => break,
                                what = rx.recv() => {
                                    let Some(what) = what else { break };
                                    // the pass toward the OTHERS only: with
                                    // no others there is NO pass at all —
                                    // the rows that came from the sources
                                    // never bounce back to them (K1)
                                    let (label, skip, has_targets) = match &what {
                                        Tick::Others(skip) => {
                                            let self_dev = serve::self_device(&root);
                                            let has = DeviceRegistry::open(&root)
                                                .ok()
                                                .and_then(|r| r.live_members().ok())
                                                .map(|ms| {
                                                    ms.iter().any(|m| {
                                                        m.device != self_dev
                                                            && !skip.contains(&m.device)
                                                    })
                                                })
                                                .unwrap_or(false);
                                            ("write", skip.clone(), has)
                                        }
                                        Tick::Clock => ("clock", Vec::new(), true),
                                        Tick::Write => ("write", Vec::new(), true),
                                    };
                                    let mut received: Vec<(String, i64)>;
                                    let mut first = true;
                                    loop {
                                        // the first round obeys the tick
                                        // (the others-only exclusions, or
                                        // none at all); a RETRIGGERED round
                                        // carries local rows — it goes to
                                        // every live device
                                        let (run, skip_now) = if first {
                                            (has_targets, skip.clone())
                                        } else {
                                            (true, Vec::new())
                                        };
                                        received = Vec::new();
                                        if run {
                                            if std::env::var("ENGRAMMA_TRACE").is_ok() {
                                                eprintln!("worker: passo {label}");
                                            }
                                            match serve::run_pass(&root, &endpoint, &skip_now).await {
                                                Ok(report) => {
                                                    received = report
                                                        .cells
                                                        .iter()
                                                        .map(|c| {
                                                            (c.name.clone(), c.received as i64)
                                                        })
                                                        .collect();
                                                    if std::env::var("ENGRAMMA_TRACE").is_ok() {
                                                        for p in &report.peers {
                                                            eprintln!(
                                                                "passo ({label}): {} reached={} via={:?} why={:?}",
                                                                p.name, p.reached, p.via, p.why_not
                                                            );
                                                        }
                                                    }
                                                }
                                                Err(e) if std::env::var("ENGRAMMA_TRACE").is_ok() => {
                                                    eprintln!("passo ({label}): errore {e}");
                                                }
                                                Err(_) => {}
                                            }
                                        }
                                        // the ack comes AFTER the pass (K1):
                                        // the baselines take what the pass
                                        // itself wrote, or its own writes
                                        // would re-light the cycle. And a
                                        // local write that landed MID-PASS
                                        // is handed to another pass — the
                                        // settle must not swallow it
                                        let again = if let Ok(mut w) = watch.lock() {
                                            w.finish_pass(&received).unwrap_or(false)
                                        } else {
                                            false
                                        };
                                        first = false;
                                        if !again {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
                // the accept loop: the gate before anything
                {
                    let root = root.clone();
                    let stop = stop.clone();
                    let watch = std::sync::Arc::clone(&watch);
                    tokio::task::spawn_local(async move {
                        loop {
                            let incoming = tokio::select! {
                                _ = stop.cancelled() => break,
                                i = endpoint.accept() => match i {
                                    Some(i) => i,
                                    None => break,
                                },
                            };
                            let Ok(conn) = incoming.await else {
                                continue;
                            };
                            let caller = conn.remote_id();
                            let root = root.clone();
                            let watch = std::sync::Arc::clone(&watch);
                            tokio::task::spawn_local(async move {
                                let reg = match DeviceRegistry::open(&root) {
                                    Ok(r) => r,
                                    Err(_) => return,
                                };
                                // the gate: the caller must be a live
                                // device's transport key, or the connection
                                // closes before any exchange
                                let Ok(device) =
                                    engramma_sync::protocol::authorize(caller.as_bytes(), &reg)
                                else {
                                    return;
                                };
                                let book = serve::peer_addr_of_conn(&conn).ok();
                                if let Ok(report) = serve::answer_session(&root, conn).await {
                                    // the guard learns what this session
                                    // ingested and from WHOM (K1), PER CELL
                                    // (v0.6.2): those rows are the sessions'
                                    // own work, and the pass they may
                                    // trigger goes toward the others, never
                                    // back to this device
                                    let rows: Vec<(String, i64)> = report
                                        .cells
                                        .iter()
                                        .filter_map(|c| {
                                            let id: [u8; 32] =
                                                c.sync_id.as_slice().try_into().ok()?;
                                            let name =
                                                serve::cell_name(&root, &id).ok()?;
                                            Some((name, c.received as i64))
                                        })
                                        .collect();
                                    if let Ok(mut w) = watch.lock() {
                                        w.note_session(&device, &rows);
                                    }
                                    // the book keeps only what it has: the
                                    // minimal row when none exists
                                    if let Some(book) = book {
                                        serve::remember_addr_if_absent(&root, &device, &book)
                                            .ok();
                                    }
                                }
                            });
                        }
                    });
                }
                // the signals: Ctrl-C and SIGTERM stop clean
                #[cfg(unix)]
                {
                    let stop = stop.clone();
                    tokio::task::spawn_local(async move {
                        let mut sigterm = tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::terminate(),
                        )
                        .expect(/* standard signal */ "sigterm handler");
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => stop.cancel(),
                            _ = sigterm.recv() => stop.cancel(),
                        }
                    });
                }
                #[cfg(not(unix))]
                {
                    let stop = stop.clone();
                    tokio::task::spawn_local(async move {
                        let _ = tokio::signal::ctrl_c().await;
                        stop.cancel();
                    });
                }
                stop.cancelled().await;
                if !json {
                    println!("sync serve: stop pulito");
                }
                Ok::<(), CliError>(())
            })
            .await
    })
}

/// `engramma sync share <cell>`: the adopt (signed), epoch 0, the wrapped
/// keys for every live device, the shared registration. The cell starts
/// syncing at the next session.
pub fn sync_share_cmd(
    root: &std::path::Path,
    json: bool,
    cell: &str,
    cell_path: &std::path::Path,
) -> Result<(), CliError> {
    let c = engramma_core::Cell::open(cell_path)
        .map_err(|e| CliError::internal(format!("cella «{cell}»: {e}")))?;
    let root = root.to_path_buf();
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let reg = DeviceRegistry::open(&root).map_err(|e| CliError::internal(e.to_string()))?;
    if reg
        .live_members()
        .map_err(|e| CliError::internal(e.to_string()))?
        .is_empty()
    {
        reg.create_first(&dk, "portatile")
            .map_err(|e| CliError::internal(e.to_string()))?;
    }
    // arm the signer FIRST: the adopt itself must be signed
    c.set_signer(std::sync::Arc::new(
        engramma_sync::arrival::DeviceSigner::new(&dk),
    ))
    .map_err(|e| CliError::internal(e.to_string()))?;
    let adopt = c
        .adopt_log("cli")
        .map_err(|e| CliError::internal(e.to_string()))?;
    c.mark_shared()
        .map_err(|e| CliError::internal(e.to_string()))?;
    let sync_id =
        sharedcell::share_cell(&root, cell).map_err(|e| CliError::internal(e.to_string()))?;
    // the epoch-0 key wrapped for every OTHER live device: they will hold
    // it from the next session's keys frame, and right now if they are
    // reachable — the wrap lands in the keystore either way
    let ks = KeyStore::new(reg.conn());
    let epoch0 = ks
        .key_of(&sync_id, 0)
        .map_err(|e| CliError::internal(e.to_string()))?
        .ok_or_else(|| CliError::internal("l'epoca 0 è andata perduta"))?;
    let mut wraps = 0_usize;
    for member in reg
        .live_members()
        .map_err(|e| CliError::internal(e.to_string()))?
    {
        if member.device == dk.device_id() {
            continue;
        }
        let mut hpke_pub = [0_u8; 32];
        if member.hpke.len() == 64 && member.hpke.bytes().all(|b| b.is_ascii_hexdigit()) {
            for (i, chunk) in member.hpke.as_bytes().chunks(2).enumerate() {
                let hi = (chunk[0] as char).to_digit(16).unwrap_or(0) as u8;
                let lo = (chunk[1] as char).to_digit(16).unwrap_or(0) as u8;
                hpke_pub[i] = (hi << 4) | lo;
            }
        } else {
            continue;
        }
        let wrapped = engramma_sync::crypto::wrap_key(&epoch0, &sync_id, 0, &hpke_pub)
            .map_err(|e| CliError::internal(e.to_string()))?;
        ks.store_wrap(&sync_id, 0, &member.device, &wrapped)
            .map_err(|e| CliError::internal(e.to_string()))?;
        wraps += 1;
    }
    if json {
        print_json(&serde_json::json!({
            "cell": cell, "adopt": adopt.to_string(), "epoch": 0,
            "key": hex(&epoch0.0)[..8].to_string(), "wraps": wraps,
        }));
    } else {
        println!(
            "cella «{cell}» condivisa: adopt {}, epoca 0, chiave {}, {} dispositivi avvolti",
            adopt,
            &hex(&epoch0.0)[..8],
            wraps
        );
    }
    Ok(())
}

/// `engramma sync unshare <cell>`: this root stops syncing the cell; the
/// copies elsewhere stay.
pub fn sync_unshare_cmd(root: &std::path::Path, json: bool, cell: &str) -> Result<(), CliError> {
    sharedcell::unshare_cell(root, cell).map_err(|e| CliError::internal(e.to_string()))?;
    if json {
        print_json(&serde_json::json!({ "cell": cell }));
    } else {
        println!("«{cell}» non si sincronizza più da qui; le copie altrove restano");
    }
    Ok(())
}

/// `engramma devices remove <name>`: the signed remove with the accepted
/// set (the Merkle root and count over the removed device's operations
/// this copy holds), the NEW epoch for every shared cell, the wrapped keys
/// for whoever remains. Everything travels at the next session.
pub fn devices_remove_cmd(root: &std::path::Path, json: bool, name: &str) -> Result<(), CliError> {
    let root = root.to_path_buf();
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let reg = DeviceRegistry::open(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let target = reg
        .live_members()
        .map_err(|e| CliError::internal(e.to_string()))?
        .into_iter()
        .find(|m| m.name == *name)
        .ok_or_else(|| CliError::usage(format!("nessun dispositivo «{name}»")))?;
    // the accepted set: every operation of that device this copy holds,
    // across every shared cell — the root and the count ride with the
    // remove, the operative rule stays the total order
    let mut hashes: Vec<String> = Vec::new();
    for shared in SharedCell::all_of(&root).map_err(|e| CliError::internal(e.to_string()))? {
        hashes.extend(
            shared
                .cell()
                .debug_column(&format!(
                    "SELECT o.content_hash FROM ops o WHERE o.device = '{}'",
                    target.device.replace('\'', "''")
                ))
                .unwrap_or_default(),
        );
    }
    let count = hashes.len();
    let root_hash = engramma_core::cell::ops::merkle_root(&mut hashes);
    reg.remove(&dk, &target.device, &root_hash, count as u64)
        .map_err(|e| CliError::internal(e.to_string()))?;
    // the rotation: a new epoch for every shared cell, wrapped for the
    // devices that remain
    let mut rotated = Vec::new();
    let live: Vec<_> = reg
        .live_members()
        .map_err(|e| CliError::internal(e.to_string()))?
        .into_iter()
        .filter(|m| m.device != target.device)
        .collect();
    for shared in SharedCell::all_of(&root).map_err(|e| CliError::internal(e.to_string()))? {
        let sync_id: [u8; 32] = *shared.sync_id();
        let ks = KeyStore::new(reg.conn());
        let (epoch, key) = ks
            .rotate(&sync_id)
            .map_err(|e| CliError::internal(e.to_string()))?;
        for member in &live {
            let mut hpke_pub = [0_u8; 32];
            if member.hpke.len() == 64 && member.hpke.bytes().all(|b| b.is_ascii_hexdigit()) {
                for (i, chunk) in member.hpke.as_bytes().chunks(2).enumerate() {
                    let hi = (chunk[0] as char).to_digit(16).unwrap_or(0) as u8;
                    let lo = (chunk[1] as char).to_digit(16).unwrap_or(0) as u8;
                    hpke_pub[i] = (hi << 4) | lo;
                }
            } else {
                continue;
            }
            let wrapped = engramma_sync::crypto::wrap_key(&key, &sync_id, epoch, &hpke_pub)
                .map_err(|e| CliError::internal(e.to_string()))?;
            ks.store_wrap(&sync_id, epoch, &member.device, &wrapped)
                .map_err(|e| CliError::internal(e.to_string()))?;
        }
        rotated.push((
            serve::cell_name(&root, &sync_id).unwrap_or_else(|_| "?".into()),
            epoch,
        ));
    }
    if json {
        print_json(&serde_json::json!({
            "removed": name, "accepted_root": root_hash, "accepted_count": count,
            "rotated": rotated.iter().map(|(c, e)| serde_json::json!({
                "cell": c, "epoch": e,
            })).collect::<Vec<_>>(),
        }));
    } else {
        println!(
            "rimosso {name}: {count} operazioni accettate, root {}",
            &root_hash[..16]
        );
        for (cell, epoch) in &rotated {
            println!("cella «{cell}»: epoca nuova {epoch}");
        }
        println!("la rotazione raggiunge gli altri alla sessione successiva");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the blind mailbox (P4)
// ---------------------------------------------------------------------------

/// `engramma mailbox serve --root <dir> [--relay <url>|--no-relay]`: the
/// node that stores sealed envelopes for the device lists it knows,
/// indexes by opaque id and nothing else, and meets the short codes.
pub fn mailbox_serve_cmd(
    json: bool,
    dir: &std::path::Path,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    crate::mailboxcli::serve(json, dir, flag)
}

/// `engramma mailbox add <ticket>`: register the node in the owner's list
/// and hand it the signed device list.
pub fn mailbox_add_cmd(root: &std::path::Path, json: bool, ticket: &str) -> Result<(), CliError> {
    crate::mailboxcli::add(root, json, ticket)
}

/// `engramma mailbox remove`: forget the node.
pub fn mailbox_remove_cmd(root: &std::path::Path, json: bool) -> Result<(), CliError> {
    crate::mailboxcli::remove(root, json)
}

/// `engramma mailbox status`: last contact, envelopes kept, retention.
pub fn mailbox_status_cmd(root: &std::path::Path, json: bool) -> Result<(), CliError> {
    crate::mailboxcli::status(root, json)
}

/// the code way in: the meeting at the blind node (P4)
fn mailbox_join_with_code(
    root: &std::path::Path,
    json: bool,
    code: &str,
    name: &str,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    crate::mailboxcli::join_with_code(root, json, code, name, flag)
}
