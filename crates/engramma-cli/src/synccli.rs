// SPDX-License-Identifier: Apache-2.0

//! The sync, devices and mailbox commands (v0.6.2, S1): a THIN CLI over
//! the library's own service (`engramma_sync::service`). The flags parse
//! here, the road lives there — the Node binding answers Homun from the
//! very same cycle. The textual and JSON shapes are the v0.6.1 ones; the
//! SAS confirmation still reads one line, but through the service's
//! staged handle.

use std::io::{BufRead, Write};
use std::time::Duration;

use engramma_sync::relay::RelayChoice;
use engramma_sync::service::{PairingHandle, ServiceError, SyncService};

use crate::output::said_yes;
use crate::{CliError, print_json};

/// The default first-device name, as it always was.
const FIRST_NAME: &str = "laptop";

/// The service's own refusals, in the CLI's terms: `SYNC_BUSY` keeps its
/// own exit code, everything the caller named is a usage or not-found,
/// the rest is internal with the original words (a burnt channel's
/// «burnt» survives the crossing).
fn svc_err(e: ServiceError) -> CliError {
    match e {
        ServiceError::Busy => CliError::busy(e.message()),
        ServiceError::NotFound(what) => CliError::not_found(what),
        ServiceError::Invalid(why) => CliError::usage(why),
        other => CliError::internal(other.to_string()),
    }
}

/// Ask the human one yes line: the SAS confirmation, the same prompt the
/// service's staged handle asks the UI to answer.
fn confirm_line(sas: &str) -> bool {
    print!("verification code: {sas} — is it the same on the other screen? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    said_yes(&line)
}

/// Drive one SAS round the CLI way: wait for the digits, prompt, read the
/// line, hand the answer to the service.
fn confirm_round(p: &PairingHandle) -> Result<(), CliError> {
    let sas = p.sas(Duration::from_secs(11 * 60)).map_err(svc_err)?;
    let yes = confirm_line(&sas);
    p.confirm(yes).map_err(svc_err)
}

/// `engramma devices invite [--name <name>] [--relay <url>|--no-relay]`:
/// bind the pairing endpoint, print the ticket with the REAL address,
/// wait (ten minutes, one use) for the device that enters.
pub fn devices_invite(
    root: &std::path::Path,
    json: bool,
    flag: Option<RelayChoice>,
    name: Option<String>,
) -> Result<(), CliError> {
    let name = name.unwrap_or_else(|| FIRST_NAME.to_string());
    let svc = SyncService::start(root).map_err(svc_err)?;
    let p = svc.invite(&name, flag).map_err(svc_err)?;
    let info = p.info(Duration::from_secs(60)).map_err(svc_err)?;
    if let Some(warning) = &info.relay_warning {
        eprintln!("warning: {warning}");
    }
    if json {
        let mut out = serde_json::json!({
            "ticket": info.ticket.clone().unwrap_or_default(),
            "node": info.node.clone().unwrap_or_default(),
            "addrs": info.addrs,
            "expires_at": info.expires_ms,
            "relay": info.relay,
        });
        if let Some(warning) = &info.relay_warning {
            out["relay_warning"] = serde_json::json!(warning);
        }
        print_json(&out);
    } else {
        println!("ticket (ten minutes, one use):");
        println!("{}", info.ticket.clone().unwrap_or_default());
        if let Some(relay) = &info.relay {
            println!("relay: {relay}");
        }
        println!("waiting for the device to join…");
    }
    confirm_round(&p)?;
    let report = p.done(Duration::from_secs(120)).map_err(svc_err)?;
    if !json {
        for cell in &report.cells {
            println!("cell \"{}\": {} operations delivered", cell.name, cell.ops);
        }
        println!("device added to the list");
    }
    Ok(())
}

/// `engramma devices join <ticket|code> --name <name>`: the ticket way
/// in (both textual forms) or the code's meeting at the blind node —
/// SPAKE2, the key confirmation, the SAS on both screens, then the
/// registry, the keys and the cells.
pub fn devices_join(
    root: &std::path::Path,
    json: bool,
    ticket_or_code: &str,
    name: &str,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    let svc = SyncService::start(root).map_err(svc_err)?;
    let p = svc.join(ticket_or_code, name, flag).map_err(svc_err)?;
    // the short code meets in TWO rounds: the code's own and the ticket
    // flow's — each one gets its prompt
    let rounds = if ticket_or_code.trim().starts_with("engramma-invite-v1")
        || ticket_or_code
            .trim()
            .to_uppercase()
            .starts_with("ENG-INVITE-V1.")
    {
        1
    } else {
        2
    };
    for _ in 0..rounds {
        confirm_round(&p)?;
    }
    let report = p.done(Duration::from_secs(180)).map_err(svc_err)?;
    if json {
        print_json(&serde_json::json!({
            "cells": report.cells.iter().map(|c| serde_json::json!({
                "name": c.name, "ops": c.ops,
            })).collect::<Vec<_>>(),
        }));
    } else {
        for cell in &report.cells {
            println!("cell \"{}\" received: {} operations", cell.name, cell.ops);
        }
        println!("in: `engramma sync run` for the first pass");
    }
    Ok(())
}

/// `engramma devices code [--name <name>]`: the short code, printed at
/// once, then the meeting at the blind node — TWO SAS rounds (the code's
/// and the ticket flow's).
pub fn devices_code(
    root: &std::path::Path,
    json: bool,
    flag: Option<RelayChoice>,
    name: Option<String>,
) -> Result<(), CliError> {
    let name = name.unwrap_or_else(|| FIRST_NAME.to_string());
    let svc = SyncService::start(root).map_err(svc_err)?;
    let p = svc.code(&name, flag).map_err(svc_err)?;
    let info = p.info(Duration::from_secs(60)).map_err(svc_err)?;
    if let Some(warning) = &info.relay_warning {
        eprintln!("warning: {warning}");
    }
    if json {
        print_json(&serde_json::json!({ "code": info.code.unwrap_or_default() }));
    } else {
        println!("short code (ten minutes, one attempt):");
        println!("{}", info.code.unwrap_or_default());
    }
    for _ in 0..2 {
        confirm_round(&p)?;
    }
    let report = p.done(Duration::from_secs(180)).map_err(svc_err)?;
    if !json {
        for cell in &report.cells {
            println!("cell \"{}\": {} operations delivered", cell.name, cell.ops);
        }
    }
    Ok(())
}

/// `engramma sync run [--relay <url>|--no-relay]`: one pass with every
/// live device the book reaches, then the mailbox when one is
/// configured. An unreachable device is a row of the summary, never an
/// error.
pub fn sync_run_cmd(
    root: &std::path::Path,
    json: bool,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    let svc = SyncService::start(root).map_err(svc_err)?;
    let report = svc.run_once(flag).map_err(svc_err)?;
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
                (Some(why), _) => println!("{}: unreachable — {why}", p.name),
                (None, _) => {
                    let via = p.via.unwrap_or("direct");
                    println!("{}: session done ({})", p.name, via);
                }
            }
        }
        for c in &report.cells {
            println!(
                "cell \"{}\": {} sent, {} received, {} refused, {} conflicts, {} unsent, frontier {}",
                c.name, c.sent, c.received, c.refused, c.conflicts, c.unsent, c.frontier
            );
        }
    }
    Ok(())
}

/// `engramma sync serve [--relay <url>|--no-relay]`: the service's own
/// continuous loop, announced and stopped by the CLI — the address is
/// printed as it always was, Ctrl-C and SIGTERM stop it clean.
pub fn sync_serve_cmd(
    root: &std::path::Path,
    json: bool,
    flag: Option<RelayChoice>,
) -> Result<(), CliError> {
    let svc = SyncService::start(root).map_err(svc_err)?;
    let info = svc.serve_start(flag).map_err(svc_err)?;
    if let Some(warning) = &info.relay_warning {
        eprintln!("warning: {warning}");
    }
    if json {
        let mut out = serde_json::json!({
            "serving": true,
            "node": info.node,
            "addrs": info.addrs,
            "relay": info.relay,
        });
        if let Some(warning) = &info.relay_warning {
            out["relay_warning"] = serde_json::json!(warning);
        }
        print_json(&out);
    } else {
        println!(
            "sync serve listening ({})",
            info.addrs.first().cloned().unwrap_or_default()
        );
    }
    // the signals: Ctrl-C and SIGTERM stop clean
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::internal(e.to_string()))?;
    rt.block_on(async {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect(/* standard signal */ "sigterm handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    });
    svc.serve_stop().map_err(svc_err)?;
    if !json {
        println!("sync serve: clean stop");
    }
    Ok(())
}

/// `engramma sync share <cell>`: the adopt (signed), epoch 0, the wrapped
/// keys for every live device, the shared registration.
pub fn sync_share_cmd(
    root: &std::path::Path,
    json: bool,
    cell: &str,
    _cell_path: &std::path::Path,
) -> Result<(), CliError> {
    let svc = SyncService::start(root).map_err(svc_err)?;
    let view = svc.share(cell).map_err(svc_err)?;
    if json {
        print_json(&serde_json::json!({
            "cell": view.cell, "adopt": view.adopt, "epoch": view.epoch,
            "key": view.key, "wraps": view.wraps,
        }));
    } else {
        println!(
            "cell \"{}\" shared: adopt {}, epoch 0, key {}, {} devices wrapped",
            view.cell, view.adopt, view.key, view.wraps
        );
    }
    Ok(())
}

/// `engramma sync unshare <cell>`: this root stops syncing the cell; the
/// copies elsewhere stay.
pub fn sync_unshare_cmd(root: &std::path::Path, json: bool, cell: &str) -> Result<(), CliError> {
    let svc = SyncService::start(root).map_err(svc_err)?;
    svc.unshare(cell).map_err(svc_err)?;
    if json {
        print_json(&serde_json::json!({ "cell": cell }));
    } else {
        println!("\"{cell}\" no longer syncs from here; the copies elsewhere stay");
    }
    Ok(())
}

/// `engramma devices remove <name>`: the signed remove with the accepted
/// set, the NEW epoch for every shared cell, the wrapped keys for
/// whoever remains — and the courtesy call that tells the removed
/// device.
pub fn devices_remove_cmd(root: &std::path::Path, json: bool, name: &str) -> Result<(), CliError> {
    let svc = SyncService::start(root).map_err(svc_err)?;
    let view = svc.remove(name).map_err(svc_err)?;
    if json {
        print_json(&serde_json::json!({
            "removed": view.removed, "accepted_root": view.accepted_root,
            "accepted_count": view.accepted_count,
            "rotated": view.rotated.iter().map(|(c, e)| serde_json::json!({
                "cell": c, "epoch": e,
            })).collect::<Vec<_>>(),
        }));
    } else {
        println!(
            "removed {}: {} operations accepted, root {}",
            view.removed,
            view.accepted_count,
            &view.accepted_root[..16.min(view.accepted_root.len())]
        );
        for (cell, epoch) in &view.rotated {
            println!("cell \"{cell}\": new epoch {epoch}");
        }
        println!("the rotation reaches the others at the next session");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the blind mailbox (P4): the node itself stays a CLI concern
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
