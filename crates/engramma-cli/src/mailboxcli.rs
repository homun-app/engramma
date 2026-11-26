// SPDX-License-Identifier: Apache-2.0

//! The mailbox commands (v0.6.1, P4): the blind node that serves and the
//! device side that registers itself. The short-code MEETING lives in
//! the library's service since v0.6.2 (S1) — the CLI drives it from
//! `synccli`; this file keeps only the node's own commands.

use std::path::Path;

use engramma_sync::device::DeviceKey;
use engramma_sync::mailbox_net::{self, NodeState};
use engramma_sync::registry::DeviceRegistry;
use engramma_sync::relay::{self as relay_choice_mod, RelayChoice};
use engramma_sync::serve;

use crate::{CliError, print_json};

/// `engramma mailbox serve --root <dir> [--relay <url>|--no-relay]`: the
/// node runs until the signal.
pub fn serve(json: bool, dir: &Path, flag: Option<RelayChoice>) -> Result<(), CliError> {
    let dir = dir.to_path_buf();
    let key = DeviceKey::load_or_create(&dir).map_err(|e| CliError::internal(e.to_string()))?;
    let choice = relay_choice_mod::resolve(&dir, flag.as_ref())
        .map_err(|e| CliError::internal(e.to_string()))?;
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
                // the node's ticket must be reachable from whoever needs
                // the node most — the devices behind a NAT: the relay
                // wait comes first (C1)
                let minted = mailbox_net::node_ticket(&endpoint, &choice).await;
                if minted.relay_timed_out {
                    eprintln!(
                        "warning: the relay did not connect within {} s — the \
                         ticket goes out without a relay (whoever is behind NAT \
                         will not find the node)",
                        engramma_sync::relay::RELAY_WAIT.as_secs()
                    );
                }
                let ticket = minted.json;
                if json {
                    let mut out = serde_json::json!({
                        "serving": dir.display().to_string(),
                        "ticket": serde_json::from_str::<serde_json::Value>(&ticket)
                            .unwrap_or(serde_json::Value::Null),
                        "alpn": "engramma/mailbox/3",
                        "retention_days": engramma_sync::mailbox::RETENTION_DAYS,
                        "relay": minted.relay,
                    });
                    if minted.relay_timed_out {
                        out["relay_warning"] = serde_json::json!(
                            "the relay did not connect: the ticket goes out without a relay"
                        );
                    }
                    print_json(&out);
                } else {
                    println!(
                        "blind mailbox in {} (ALPN engramma/mailbox/3, \
                         keeps {} days)",
                        dir.display(),
                        engramma_sync::mailbox::RETENTION_DAYS
                    );
                    println!("node ticket:");
                    println!("{ticket}");
                    if let Some(relay) = &minted.relay {
                        println!("relay: {relay}");
                    }
                    println!("listening…");
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
                    println!("mailbox: clean stop");
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
        reg.create_first(&dk, "laptop")
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
        .map_err(|e| CliError::internal(format!("mailbox: {e}")))?;
    if json {
        print_json(&serde_json::json!({ "owner": owner }));
    } else {
        println!("mailbox registered (list {owner})");
    }
    Ok(())
}

/// `engramma mailbox remove`: forget the node.
pub fn remove(root: &Path, json: bool) -> Result<(), CliError> {
    mailbox_net::forget_node(root).map_err(|e| CliError::internal(e.to_string()))?;
    if json {
        print_json(&serde_json::json!({ "removed": true }));
    } else {
        println!("mailbox forgotten");
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
            println!("no mailbox configured");
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
        .map_err(|e| CliError::internal(format!("mailbox: {e}")))?;
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
            "mailbox {} · last contact {} · {} envelopes ({} bytes) · \
             keeps {} days",
            &engramma_sync::device::hex(&node.node_id)[..8],
            last.as_deref().unwrap_or("never"),
            stats.0,
            stats.1,
            engramma_sync::mailbox::RETENTION_DAYS
        );
    }
    Ok(())
}
