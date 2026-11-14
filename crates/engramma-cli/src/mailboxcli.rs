// SPDX-License-Identifier: Apache-2.0

//! The mailbox commands (v0.6.1, P4): the blind node that serves, the
//! device side that registers itself, and the short-code meeting that
//! ends with a sealed ticket handed over the node's relay.

use std::path::Path;

use engramma_sync::device::DeviceKey;
use engramma_sync::mailbox_net::{self, NodeState};
use engramma_sync::pairing::{self, Side};
use engramma_sync::pairing_net;
use engramma_sync::registry::DeviceRegistry;
use engramma_sync::relay::{self as relay_choice_mod, RelayChoice};
use engramma_sync::serve;

use crate::{CliError, print_json};

fn confirm_line(sas: &str) -> bool {
    use std::io::{BufRead, Write};
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

/// `engramma mailbox serve --root <dir> [--relay <url>|--no-relay]`: the
/// node runs until the signal.
pub fn serve(json: bool, dir: &Path, flag: Option<RelayChoice>) -> Result<(), CliError> {
    let dir = dir.to_path_buf();
    let key = DeviceKey::load_or_create(&dir).map_err(|e| CliError::internal(e.to_string()))?;
    let choice =
        relay_choice_mod::resolve(&dir, flag.as_ref()).map_err(|e| CliError::internal(e.to_string()))?;
    // the node opens its state again per connection, inside the loop
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(e.to_string()))?;
    rt.block_on(async {
        tokio::task::LocalSet::new()
            .run_until(async {
                let endpoint = mailbox_net::bind_node(&key, "0.0.0.0:0", &choice)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let ticket = mailbox_net::node_ticket(&endpoint);
                if json {
                    print_json(&serde_json::json!({
                        "serving": dir.display().to_string(),
                        "ticket": ticket,
                        "alpn": "engramma/mailbox/1",
                        "retention_days": engramma_sync::mailbox::RETENTION_DAYS,
                    }));
                } else {
                    println!(
                        "casella postale cieca in {} (ALPN engramma/mailbox/1, \
                         conserva {} giorni)",
                        dir.display(),
                        engramma_sync::mailbox::RETENTION_DAYS
                    );
                    println!("biglietto del nodo:");
                    println!("{ticket}");
                    println!("in ascolto…");
                }
                let stop = tokio_util::sync::CancellationToken::new();
                // the accept loop: one task per connection — the code
                // meetings bypass the device gate (the code IS the gate),
                // everything else must be a live device of a known list
                {
                    let endpoint = endpoint.clone();
                    let state_dir = dir.clone();
                    let stop = stop.clone();
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
                            let state_dir = state_dir.clone();
                            tokio::task::spawn_local(async move {
                                let state = match NodeState::open(&state_dir) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        eprintln!("node: state: {e}");
                                        return;
                                    }
                                };
                                // the first frame decides: a code meeting
                                // or a device frame
                                if let Err(e) = mailbox_net::serve_or_relay(&state, &conn).await {
                                    eprintln!("node: connection: {e}");
                                }
                            });
                        }
                    });
                }
                // the signals
                {
                    let stop = stop.clone();
                    tokio::task::spawn_local(async move {
                        #[cfg(unix)]
                        {
                            let mut sigterm = tokio::signal::unix::signal(
                                tokio::signal::unix::SignalKind::terminate(),
                            )
                            .expect(/* standard signal */ "sigterm handler");
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => stop.cancel(),
                                _ = sigterm.recv() => stop.cancel(),
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = tokio::signal::ctrl_c().await;
                            stop.cancel();
                        }
                    });
                }
                stop.cancelled().await;
                if !json {
                    println!("casella postale: stop pulito");
                }
                Ok::<(), CliError>(())
            })
            .await
    })
}

/// `engramma mailbox add <ticket>`: register the node in the owner's list
/// and hand it the signed device list.
pub fn add(root: &Path, json: bool, ticket: &str) -> Result<(), CliError> {
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
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(e.to_string()))?;
    let choice =
        relay_choice_mod::resolve(&root, None).map_err(|e| CliError::internal(e.to_string()))?;
    let owner = rt
        .block_on(async {
            tokio::task::LocalSet::new()
                .run_until(async {
                    let endpoint = serve::bind(&dk, "0.0.0.0:0", &choice)
                        .await
                        .map_err(|e| CliError::internal(e.to_string()))?;
                    mailbox_net::add_node(&root, &endpoint, ticket)
                        .await
                        .map_err(|e| CliError::internal(e.to_string()))
                })
                .await
        })
        .map_err(|e| CliError::internal(format!("casella postale: {e}")))?;
    if json {
        print_json(&serde_json::json!({ "owner": owner }));
    } else {
        println!("casella postale registrata (elenco {owner})");
    }
    Ok(())
}

/// `engramma mailbox remove`: forget the node.
pub fn remove(root: &Path, json: bool) -> Result<(), CliError> {
    mailbox_net::forget_node(root).map_err(|e| CliError::internal(e.to_string()))?;
    if json {
        print_json(&serde_json::json!({ "removed": true }));
    } else {
        println!("casella postale dimenticata");
    }
    Ok(())
}

/// `engramma mailbox status`: last contact, envelopes kept (count and
/// bytes), retention — the numbers come FROM the node.
pub fn status(root: &Path, json: bool) -> Result<(), CliError> {
    let root = root.to_path_buf();
    let Some(node) = mailbox_net::node_of(&root).map_err(|e| CliError::internal(e.to_string()))?
    else {
        if json {
            print_json(&serde_json::json!({ "mailbox": false }));
        } else {
            println!("nessuna casella postale configurata");
        }
        return Ok(());
    };
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(e.to_string()))?;
    let choice =
        relay_choice_mod::resolve(&root, None).map_err(|e| CliError::internal(e.to_string()))?;
    let stats = rt
        .block_on(async {
            tokio::task::LocalSet::new()
                .run_until(async {
                    let endpoint = serve::bind(&dk, "0.0.0.0:0", &choice)
                        .await
                        .map_err(|e| CliError::internal(e.to_string()))?;
                    mailbox_net::node_stats(&root, &endpoint, &node)
                        .await
                        .map_err(|e| CliError::internal(e.to_string()))
                })
                .await
        })
        .map_err(|e| CliError::internal(format!("casella postale: {e}")))?;
    let last = mailbox_net::last_contact(&root).map_err(|e| CliError::internal(e.to_string()))?;
    if json {
        print_json(&serde_json::json!({
            "mailbox": true,
            "node": engramma_sync::device::hex(&node.node_id)[..8].to_string(),
            "last_contact": last,
            "envelopes": stats.0,
            "bytes": stats.1,
            "retention_days": engramma_sync::mailbox::RETENTION_DAYS,
        }));
    } else {
        println!(
            "casella postale {} · ultimo contatto {} · {} buste ({} byte) · \
             conserva {} giorni",
            &engramma_sync::device::hex(&node.node_id)[..8],
            last.as_deref().unwrap_or("mai"),
            stats.0,
            stats.1,
            engramma_sync::mailbox::RETENTION_DAYS
        );
    }
    Ok(())
}

/// `engramma devices code`: the short code meets at the blind node. The
/// owner opens the channel, runs the PAKE with whoever enters, shows the
/// SAS, and hands over a FRESH invite ticket — sealed, over the relay: the
/// join then runs the ticket flow directly, device to device.
pub fn devices_code(root: &Path, json: bool, flag: Option<RelayChoice>) -> Result<(), CliError> {
    let root = root.to_path_buf();
    let Some(node) = mailbox_net::node_of(&root).map_err(|e| CliError::internal(e.to_string()))?
    else {
        return Err(CliError::usage(
            "nessuna casella postale configurata: `engramma mailbox add <biglietto>` prima",
        ));
    };
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let code = pairing::ShortCode::generate().map_err(|e| CliError::internal(e.to_string()))?;
    if json {
        print_json(&serde_json::json!({ "code": code.encode() }));
    } else {
        println!("codice breve (dieci minuti, un tentativo):");
        println!("{}", code.encode());
    }
    let choice =
        relay_choice_mod::resolve(&root, flag.as_ref()).map_err(|e| CliError::internal(e.to_string()))?;
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
                let relay = mailbox_net::meet_code(&endpoint, &node, code.channel)
                    .await
                    .map_err(|e| CliError::internal(format!("canale: {e}")))?;
                // SPAKE2 over the code + the RFC 9382 confirmation: a
                // wrong code dies here and burns the channel
                let pake = mailbox_net::code_pake(&relay, Side::Inviter, &code.password()).await;
                let (shared, transcript, sas) = match pake {
                    Ok(x) => x,
                    Err(e) => {
                        relay.burn(code.channel).await.ok();
                        return Err(CliError::internal(format!("conferma di chiave: {e}")));
                    }
                };
                if !confirm_line(&sas) {
                    relay.burn(code.channel).await.ok();
                    return Err(CliError::internal("SAS rifiutata".to_string()));
                }
                // the sealed confirmation, then the fresh ticket — the
                // join runs the ticket flow directly
                let rkey = mailbox_net::relay_key(&shared, &transcript);
                let yes = mailbox_net::relay_seal(&rkey, b"engramma/pair/yes")
                    .map_err(|e| CliError::internal(e.to_string()))?;
                relay
                    .send(&yes)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let their_blob = relay
                    .recv()
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                mailbox_net::relay_open(&rkey, &their_blob)
                    .map_err(|e| CliError::internal(e.to_string()))?;
                // the pairing endpoint and its fresh ticket
                let pair_endpoint = pairing_net::bind_pairing(&dk, "0.0.0.0:0", &choice)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let ticket = pairing_net::ticket_of(&pair_endpoint)
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let blob = mailbox_net::relay_seal(&rkey, ticket.encode().as_bytes())
                    .map_err(|e| CliError::internal(e.to_string()))?;
                relay
                    .send(&blob)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                // the ticket is handed over: the channel burns (one
                // attempt per code — the joiner burns too, whoever is
                // first)
                relay.burn(code.channel).await.ok();
                // and the invite flow runs on the pairing endpoint
                let reg =
                    DeviceRegistry::open(&root).map_err(|e| CliError::internal(e.to_string()))?;
                let report = pairing_net::invite(&pair_endpoint, &reg, &root, &ticket, |sas| {
                    confirm_line(sas)
                })
                .await
                .map_err(|e| CliError::internal(format!("abbinamento: {e}")))?;
                if !json {
                    for (name, ops) in &report.cells {
                        println!("cella «{name}»: {ops} operazioni consegnate");
                    }
                }
                Ok(())
            })
            .await
    })
}

/// `engramma devices join <code> --name`: the code way in — the meeting at
/// the blind node, then the ticket flow directly.
pub fn join_with_code(
    root: &Path,
    json: bool,
    code_text: &str,
    name: &str,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    let root = root.to_path_buf();
    let Some(node) = mailbox_net::node_of(&root).map_err(|e| CliError::internal(e.to_string()))?
    else {
        return Err(CliError::usage(
            "nessuna casella postale configurata: `engramma mailbox add <biglietto>` prima",
        ));
    };
    let code = pairing::ShortCode::decode(code_text)
        .map_err(|e| CliError::usage(format!("codice non valido: {e}")))?;
    let dk = DeviceKey::load_or_create(&root).map_err(|e| CliError::internal(e.to_string()))?;
    let choice =
        relay_choice_mod::resolve(&root, flag.as_ref()).map_err(|e| CliError::internal(e.to_string()))?;
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
                let relay = mailbox_net::meet_code(&endpoint, &node, code.channel)
                    .await
                    .map_err(|e| CliError::internal(format!("canale: {e}")))?;
                let pake = mailbox_net::code_pake(&relay, Side::Joiner, &code.password()).await;
                let (shared, transcript, sas) = match pake {
                    Ok(x) => x,
                    Err(e) => {
                        relay.burn(code.channel).await.ok();
                        return Err(CliError::internal(format!("conferma di chiave: {e}")));
                    }
                };
                if !confirm_line(&sas) {
                    relay.burn(code.channel).await.ok();
                    return Err(CliError::internal("SAS rifiutata".to_string()));
                }
                let rkey = mailbox_net::relay_key(&shared, &transcript);
                // their sealed confirmation, then ours, then the ticket
                let their_blob = relay
                    .recv()
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                mailbox_net::relay_open(&rkey, &their_blob)
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let yes = mailbox_net::relay_seal(&rkey, b"engramma/pair/yes")
                    .map_err(|e| CliError::internal(e.to_string()))?;
                relay
                    .send(&yes)
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let blob = relay
                    .recv()
                    .await
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let ticket_bytes = mailbox_net::relay_open(&rkey, &blob)
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let ticket_text = String::from_utf8(ticket_bytes)
                    .map_err(|e| CliError::internal(e.to_string()))?;
                let ticket = pairing::Ticket::decode(&ticket_text)
                    .map_err(|e| CliError::internal(format!("biglietto: {e}")))?;
                // ONE attempt per code: the channel that carried the
                // ticket burns, the second arrival finds it burnt
                relay.burn(code.channel).await.ok();
                // the direct ticket flow
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
                }
                Ok(())
            })
            .await
    })
}
