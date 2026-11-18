// SPDX-License-Identifier: Apache-2.0

//! S1 (v0.6.2): the sync service in the LIBRARY. The same roads the CLI
//! walked — invite and join on loopback without a relay, the confirmation
//! given by a test function instead of a stdin line, the share, the write
//! that crosses, the removal that hears back — plus the exclusive root
//! lock (`SYNC_BUSY`) and the per-cell guard.

use std::time::Duration;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use engramma_sync::relay::RelayChoice;
use engramma_sync::service::{ServiceError, SyncService};
use time::OffsetDateTime;
use ulid::Ulid;

fn note(n: u64, body: &str) -> Note {
    let t = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* fixed */);
    Note {
        id: Ulid::from_string(&format!("01F7ZQ6VTSATW2BQ5Z9R{:06X}", n)).unwrap(/* t */),
        note_type: NoteType::Fact,
        title: Some(format!("nota {n}")),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: At(t),
        updated_at: At(t),
        extensions: vec![],
    }
}

/// A root that lives as long as the service that serves it (the lesson of
/// `19d2e9e`: the folder stays beside what uses it).
struct Root {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Root {
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap(/* t */);
        let root = dir.path().join(tag);
        std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
        Self { _dir: dir, root }
    }

    fn cell(&self, name: &str) -> Cell {
        Cell::open(&self.root.join("cells").join(format!("{name}.sqlite"))).unwrap(/* t */)
    }
}

/// Run a test body with a deadline: a hung network dance fails the test,
/// not the run.
fn bounded<F>(name: &str, secs: u64, body: F)
where
    F: FnOnce() + Send + 'static,
{
    let name = name.to_string();
    let handle = std::thread::spawn(body);
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while !handle.is_finished() {
        if std::time::Instant::now() > deadline {
            panic!("il test «{name}» non è finito in {secs} s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
}

const NO_RELAY: Option<RelayChoice> = Some(RelayChoice::Disabled);
const LONG: Duration = Duration::from_secs(60);

/// The whole S1 road, in the library, on loopback without a relay: the
/// invite with the FIRST DEVICE'S NAME, the join, the SAS confirmed by a
/// test function, the shared cell that arrives, the write that crosses
/// while both services run, the removal that the removed device hears.
#[test]
fn invite_join_share_write_remove_roundtrip() {
    bounded("service_roundtrip", 240, || {
        let a = Root::new("svc-a");
        let b = Root::new("svc-b");
        // A holds a cell with three notes
        let cell = Cell::create(
            &a.root.join("cells").join("memoria.sqlite"),
            "memoria",
            "cli",
        )
        .unwrap(/* t */);
        for i in 1..=3_u64 {
            cell.put_note(&note(i, &format!("corpo della nota {i}")), "cli")
                .unwrap(/* t */);
        }
        drop(cell);
        let svc_a = SyncService::start(&a.root).unwrap(/* t */);
        let svc_b = SyncService::start(&b.root).unwrap(/* t */);
        let events_b = svc_b.take_events().expect(/* one */ "the events");
        // the invite FIRST: it names the list as the caller asked (a
        // share before it would have named it «portatile», the default)
        let invite = svc_a.invite("laptop", NO_RELAY).unwrap(/* t */);
        // the share, from the service
        let shared = svc_a.share("memoria").unwrap(/* t */);
        assert_eq!(shared.epoch, 0, "the share opens epoch 0");
        let info = invite.info(LONG).unwrap(/* t */);
        let ticket = info.ticket.expect(/* invite */ "the ticket");
        assert!(
            ticket.starts_with("engramma-invite-v1."),
            "the textual form is the ticket: {ticket}"
        );
        assert!(
            info.ticket_qr.as_deref().is_some_and(|qr| qr
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || "$%*+-./:".contains(c))),
            "the QR form wears the QR's alphanumeric clothes: {:?}",
            info.ticket_qr
        );
        // B enters with the TEXTUAL form; the QR form enters too (below,
        // in its own test)
        let join = svc_b.join(&ticket, "mac", NO_RELAY).unwrap(/* t */);
        // the SAS: both sides hold the same six digits, the test confirms
        let sas_a = invite.sas(LONG).unwrap(/* t */);
        let sas_b = join.sas(LONG).unwrap(/* t */);
        assert_eq!(sas_a, sas_b, "the two screens show the same digits");
        invite.confirm(true).unwrap(/* t */);
        join.confirm(true).unwrap(/* t */);
        let done_a = invite.done(LONG).unwrap(/* t */);
        let done_b = join.done(LONG).unwrap(/* t */);
        assert!(
            done_b
                .cells
                .iter()
                .any(|c| c.name == "memoria" && c.ops >= 3),
            "the cell arrived whole on B (its ops, genesis and adopt included): {:?}",
            done_b.cells
        );
        assert_eq!(
            done_a.peer.as_ref().map(|p| p.name.as_str()),
            Some("mac"),
            "the inviter learns who entered"
        );
        // the lists: A named itself «laptop», B is in, both sides see two
        let names = |v: &[engramma_sync::service::DeviceView]| {
            v.iter().map(|d| d.name.clone()).collect::<Vec<String>>()
        };
        assert_eq!(names(&svc_a.devices().unwrap(/* t */)), ["laptop", "mac"]);
        assert_eq!(names(&svc_b.devices().unwrap(/* t */)), ["laptop", "mac"]);
        // the status before the service runs
        let status = svc_b.status().unwrap(/* t */);
        assert!(!status.serving, "the continuous service is off");
        assert_eq!(status.cells.len(), 1, "one shared cell");
        // both services on: the books learn the other's live address (a
        // test on one machine writes the loopback rows by hand, as the
        // first session would)
        let sa = svc_a.serve_start(NO_RELAY).unwrap(/* t */);
        let sb = svc_b.serve_start(NO_RELAY).unwrap(/* t */);
        remember(&b.root, &device_of(&a.root, "laptop"), &sa);
        remember(&a.root, &device_of(&b.root, "mac"), &sb);
        // a SIGNED write on A crosses to B without anyone pulling
        let writer = signed(&a.root, "memoria");
        writer.put_note(&note(4, "la nota che attraversa"), "cli").unwrap(/* t */);
        drop(writer);
        let arrived = {
            let deadline = std::time::Instant::now() + Duration::from_secs(25);
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let there = b
                    .cell("memoria")
                    .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM ops")
                    .unwrap(/* t */)
                    .first()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                if there >= 4 {
                    break true;
                }
                if std::time::Instant::now() > deadline {
                    break false;
                }
            }
        };
        assert!(arrived, "the write never crossed to B");
        // and B said it as an EVENT (the note is readable MID-session; the
        // event lands when the session ends — wait for it, don't drain)
        let said_arrived = {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                match events_b.recv_timeout(Duration::from_millis(200)) {
                    Ok(engramma_sync::service::ServiceEvent::Arrived { cell, count }) => {
                        assert_eq!(cell, "memoria");
                        assert!(count >= 1, "an arrival says how many");
                        break true;
                    }
                    Ok(_) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                        if std::time::Instant::now() < deadline =>
                    {
                        continue;
                    }
                    Err(_) => break false,
                }
            }
        };
        assert!(said_arrived, "B never said the arrival out loud");
        // the digests agree
        assert_eq!(
            a.cell("memoria").replicated_digest().unwrap(/* t */),
            b.cell("memoria").replicated_digest().unwrap(/* t */),
            "the two roots hold the same state"
        );
        // --- the removal: B hears it from A, its service stops by itself
        let removed = svc_a.remove("mac").unwrap(/* t */);
        assert_eq!(removed.removed, "mac");
        assert!(
            removed.rotated.iter().any(|(c, _)| c == "memoria"),
            "the rotation names the cell: {:?}",
            removed.rotated
        );
        let (heard, stopped) = {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut heard = false;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                // drain everything that piled up, look for the removal
                while let Ok(ev) = events_b.try_recv() {
                    if matches!(ev, engramma_sync::service::ServiceEvent::RemovedSelf) {
                        heard = true;
                    }
                }
                if heard {
                    break (true, !svc_b.status().unwrap(/* t */).serving);
                }
                if std::time::Instant::now() > deadline {
                    break (false, false);
                }
            }
        };
        assert!(heard, "the removed device never heard the removal");
        assert!(stopped, "the removed device's service stopped by itself");
        // A still serves; the lock is still A's
        assert!(svc_a.status().unwrap(/* t */).serving);
        svc_a.serve_stop().unwrap(/* t */);
    });
}

/// The QR form of the ticket enters as the textual one does.
#[test]
fn the_qr_form_of_the_ticket_joins() {
    bounded("qr_joins", 120, || {
        let a = Root::new("qr-a");
        let b = Root::new("qr-b");
        let svc_a = SyncService::start(&a.root).unwrap(/* t */);
        let svc_b = SyncService::start(&b.root).unwrap(/* t */);
        let invite = svc_a.invite("portatile", NO_RELAY).unwrap(/* t */);
        let info = invite.info(LONG).unwrap(/* t */);
        let qr = info.ticket_qr.expect(/* invite */ "the QR form");
        let join = svc_b.join(&qr, "mac", NO_RELAY).unwrap(/* t */);
        let (sas_a, sas_b) = (
            invite.sas(LONG).unwrap(/* t */),
            join.sas(LONG).unwrap(/* t */),
        );
        assert_eq!(sas_a, sas_b);
        invite.confirm(true).unwrap(/* t */);
        join.confirm(true).unwrap(/* t */);
        invite.done(LONG).unwrap(/* t */);
        join.done(LONG).unwrap(/* t */);
        assert!(svc_a
            .devices()
            .unwrap(/* t */)
            .iter()
            .any(|d| d.name == "mac"));
    });
}

/// A refused SAS on one side leaves no add on either side, and both
/// handles answer `PAIRING_REJECTED`.
#[test]
fn a_refused_sas_leaves_no_add() {
    bounded("refused", 120, || {
        let a = Root::new("ref-a");
        let b = Root::new("ref-b");
        let svc_a = SyncService::start(&a.root).unwrap(/* t */);
        let svc_b = SyncService::start(&b.root).unwrap(/* t */);
        let invite = svc_a.invite("portatile", NO_RELAY).unwrap(/* t */);
        let ticket = invite.info(LONG).unwrap(/* t */).ticket.expect(/* i */ "ticket");
        let join = svc_b.join(&ticket, "mac", NO_RELAY).unwrap(/* t */);
        let _ = invite.sas(LONG).unwrap(/* t */);
        let _ = join.sas(LONG).unwrap(/* t */);
        // THIS side says no
        invite.confirm(false).unwrap(/* t */);
        join.confirm(true).unwrap(/* t */);
        let out_a = invite.done(LONG);
        let out_b = join.done(LONG);
        assert!(
            matches!(out_a, Err(ServiceError::PairingRejected)),
            "the refuser hears it: {out_a:?}"
        );
        assert!(
            matches!(out_b, Err(ServiceError::PairingRejected)),
            "the other side hears a refusal, not a hang: {out_b:?}"
        );
        assert!(
            !svc_a
                .devices()
                .unwrap(/* t */)
                .iter()
                .any(|d| d.name == "mac"),
            "no add landed on the inviter"
        );
    });
}

/// `confirm` before the digits are out is `INVALID_ARGUMENT`, on either
/// side of the flow.
#[test]
fn confirm_before_the_digits_is_invalid() {
    bounded("confirm_early", 120, || {
        let a = Root::new("early-a");
        let svc = SyncService::start(&a.root).unwrap(/* t */);
        let invite = svc.invite("portatile", NO_RELAY).unwrap(/* t */);
        // no joiner has even knocked: no SAS can be out
        assert!(
            matches!(invite.confirm(true), Err(ServiceError::Invalid(_))),
            "confirming without comparing is the caller's mistake"
        );
    });
}

/// An SAS that nobody answers is `PAIRING_TIMEOUT` (the test shrinks the
/// window), and the other side hears a refusal, not a hang.
#[test]
fn an_unanswered_sas_times_out() {
    bounded("sas_timeout", 120, || {
        // SAFETY: a test-only process-wide knob, set before the pairing
        // threads of THIS test read it (the other tests confirm at once)
        unsafe { std::env::set_var("ENGRAMMA_SAS_TIMEOUT_MS", "1500") };
        let a = Root::new("to-a");
        let b = Root::new("to-b");
        let svc_a = SyncService::start(&a.root).unwrap(/* t */);
        let svc_b = SyncService::start(&b.root).unwrap(/* t */);
        let invite = svc_a.invite("portatile", NO_RELAY).unwrap(/* t */);
        let ticket = invite.info(LONG).unwrap(/* t */).ticket.expect(/* i */ "ticket");
        let join = svc_b.join(&ticket, "mac", NO_RELAY).unwrap(/* t */);
        let _ = invite.sas(LONG).unwrap(/* t */);
        let _ = join.sas(LONG).unwrap(/* t */);
        // nobody answers on EITHER side: both must time out on their own
        let out_a = invite.done(Duration::from_secs(20));
        let out_b = join.done(Duration::from_secs(20));
        assert!(
            matches!(out_a, Err(ServiceError::PairingTimeout)),
            "the silent side times out: {out_a:?}"
        );
        assert!(
            matches!(out_b, Err(ServiceError::PairingTimeout)),
            "the other side does not hang: {out_b:?}"
        );
    });
}

/// `cancel` at any moment: this side answers `CANCELLED` at `done`, the
/// endpoint closes — and the other side, once ITS human answers, gets a
/// clean refusal off the dead wire (never a hang).
#[test]
fn cancel_closes_cleanly() {
    bounded("cancel", 120, || {
        let a = Root::new("cx-a");
        let b = Root::new("cx-b");
        let svc_a = SyncService::start(&a.root).unwrap(/* t */);
        let svc_b = SyncService::start(&b.root).unwrap(/* t */);
        let invite = svc_a.invite("portatile", NO_RELAY).unwrap(/* t */);
        let ticket = invite.info(LONG).unwrap(/* t */).ticket.expect(/* i */ "ticket");
        let join = svc_b.join(&ticket, "mac", NO_RELAY).unwrap(/* t */);
        let _ = invite.sas(LONG).unwrap(/* t */);
        let _ = join.sas(LONG).unwrap(/* t */);
        invite.cancel();
        let out_a = invite.done(Duration::from_secs(20));
        assert!(
            matches!(out_a, Err(ServiceError::Cancelled)),
            "the canceller hears it: {out_a:?}"
        );
        // B's human still answers yes — and the answer meets a closed
        // wire: a refusal, not a wait
        join.confirm(true).unwrap(/* t */);
        let out_b = join.done(Duration::from_secs(20));
        assert!(
            matches!(out_b, Err(ServiceError::PairingRejected)),
            "the other side hears a clean refusal: {out_b:?}"
        );
    });
}

/// One `serve` per root: a second service — in THIS process — answers
/// `SYNC_BUSY`, at `serve` and at `run`; when the first stops, the lock
/// goes.
#[test]
fn a_second_serve_on_the_same_root_is_busy() {
    bounded("busy", 90, || {
        let a = Root::new("busy-a");
        let first = SyncService::start(&a.root).unwrap(/* t */);
        first.serve_start(NO_RELAY).unwrap(/* t */);
        let second = SyncService::start(&a.root).unwrap(/* t */);
        assert!(
            matches!(second.serve_start(NO_RELAY), Err(ServiceError::Busy)),
            "a second serve is SYNC_BUSY"
        );
        assert!(
            matches!(second.run_once(NO_RELAY), Err(ServiceError::Busy)),
            "a single pass takes the same lock"
        );
        // the first lets go: the second may pass
        first.serve_stop().unwrap(/* t */);
        second.run_once(NO_RELAY).unwrap(/* t */);
    });
}

/// The short code without a mailbox is `NO_MAILBOX`.
#[test]
fn the_code_without_a_mailbox_is_refused() {
    let a = Root::new("nomb-a");
    let svc = SyncService::start(&a.root).unwrap(/* t */);
    assert!(
        matches!(
            svc.code("portatile", NO_RELAY),
            Err(ServiceError::NoMailbox)
        ),
        "the code needs the blind node"
    );
}

/// Unsharing a cell that is not shared is `NOT_SHARED`; sharing twice is
/// fine (the same cell, the same epoch line).
#[test]
fn unsharing_a_stranger_is_not_shared() {
    let a = Root::new("nsh-a");
    Cell::create(
        &a.root.join("cells").join("solitaria.sqlite"),
        "solitaria",
        "cli",
    )
    .unwrap(/* t */);
    let svc = SyncService::start(&a.root).unwrap(/* t */);
    assert!(matches!(
        svc.unshare("solitaria"),
        Err(ServiceError::NotShared)
    ));
    svc.share("solitaria").unwrap(/* t */);
    svc.unshare("solitaria").unwrap(/* t */);
    assert!(matches!(
        svc.unshare("solitaria"),
        Err(ServiceError::NotShared)
    ));
}

/// Removing the last device is `LAST_DEVICE`.
#[test]
fn the_last_device_stays() {
    let a = Root::new("last-a");
    let svc = SyncService::start(&a.root).unwrap(/* t */);
    let invite = svc.invite("portatile", NO_RELAY).unwrap(/* t */);
    drop(invite);
    assert!(
        matches!(svc.remove("portatile"), Err(ServiceError::LastDevice)),
        "the root would be orphaned"
    );
}

// helpers -------------------------------------------------------------------

/// A cell handle armed with the device's signer: every local write of the
/// sync era carries its signature, or the peers' gate refuses it.
fn signed(root: &std::path::Path, name: &str) -> Cell {
    let cell = Cell::open(&root.join("cells").join(format!("{name}.sqlite"))).unwrap(/* t */);
    let dk = engramma_sync::device::DeviceKey::load_or_create(root).unwrap(/* t */);
    cell.set_signer(std::sync::Arc::new(engramma_sync::arrival::DeviceSigner::new(&dk)))
        .unwrap(/* t */);
    cell
}

/// The device id a root's registry holds under a name.
fn device_of(root: &std::path::Path, name: &str) -> String {
    let reg = rusqlite::Connection::open(root.join("devices.db")).unwrap(/* t */);
    reg.query_row("SELECT device FROM members WHERE name = ?1", [name], |r| r.get(0))
        .unwrap(/* t */)
}

/// Write a daemon's live address into a root's address book (the pairing
/// and every successful session do this in production; a loopback test
/// writes the row by hand, on the loopback form of the published ports).
fn remember(root: &std::path::Path, device: &str, info: &engramma_sync::service::ServeInfo) {
    let loopback = info
        .addrs
        .iter()
        .filter_map(|a| {
            let port = a.rsplit(':').next()?;
            Some(format!("127.0.0.1:{port}"))
        })
        .collect::<Vec<_>>();
    let reg = rusqlite::Connection::open(root.join("devices.db")).unwrap(/* t */);
    let node = info.node.trim_start_matches("0x").to_string();
    reg.execute(
        "INSERT OR REPLACE INTO device_addrs (device, addrs, seen_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            device,
            format!(
                "{{\"node_id\":\"{node}\",\"relay\":\"\",\"addrs\":{}}}",
                serde_json::to_string(&loopback).unwrap(/* t */)
            ),
            "test",
        ],
    )
    .unwrap(/* t */);
}

// the pairing state machine's own shape (the handle answers what it must,
// before any wire is involved)
#[test]
fn the_handle_refuses_what_it_must() {
    let a = Root::new("state-a");
    let svc = SyncService::start(&a.root).unwrap(/* t */);
    let invite = svc.invite("portatile", NO_RELAY).unwrap(/* t */);
    // the SAS never comes (no joiner): asking with a short bound answers
    // PairingTimeout, not a hang
    assert!(matches!(
        invite.sas(Duration::from_millis(200)),
        Err(ServiceError::PairingTimeout)
    ));
    invite.cancel();
}

// ---------------------------------------------------------------------------
// v0.6.3 (C5): the minors — every textual form of a ticket enters, and
// the root lock's second taker writes nothing.
// ---------------------------------------------------------------------------

/// v0.6.3 (C5.1): a ticket pasted in ANY of its textual forms enters —
/// the original lowercase, the QR's uppercase clothes, and the ORIGINAL
/// form pasted all in caps (base32 is case-insensitive: we wrote it).
#[test]
fn every_textual_form_of_a_ticket_normalizes() {
    use engramma_sync::service::normalize_ticket_input as norm;
    let original = "engramma-invite-v1.abcdefghij234567";
    assert_eq!(norm(original), original, "the original stays");
    assert_eq!(
        norm("  engramma-invite-v1.abcdefghij234567  "),
        original,
        "the trim is part of the deal"
    );
    assert_eq!(
        norm("ENG-INVITE-V1.ABCDEFGHIJ234567"),
        original,
        "the QR's uppercase clothes come off"
    );
    assert_eq!(
        norm("ENGRAMMA-INVITE-V1.ABCDEFGHIJ234567"),
        original,
        "the ORIGINAL pasted all in caps comes off too"
    );
}

/// v0.6.3 (C5.2): who finds the root lock busy writes NOTHING — the pid
/// in `sync.lock` stays the holder's — and who TAKES it truncates what an
/// earlier holder left before writing its own.
#[test]
fn the_second_lock_taker_leaves_the_holders_pid_alone() {
    use engramma_sync::service::try_lock_root;

    let dir = tempfile::tempdir().unwrap(/* t */);
    let holder = try_lock_root(dir.path()).unwrap(/* t */);
    // the holder's pid is in the file (here: staged by hand, so the test
    // does not read its own pid back)
    std::fs::write(dir.path().join("sync.lock"), "424242\n").unwrap(/* t */);
    assert!(
        matches!(try_lock_root(dir.path()), Err(ServiceError::Busy)),
        "the second taker finds it busy"
    );
    let content = std::fs::read_to_string(dir.path().join("sync.lock")).unwrap(/* t */);
    assert_eq!(
        content.trim(),
        "424242",
        "who finds the lock busy writes nothing: {content:?}"
    );
    drop(holder);
    // and the next winner TRUNCATES: no leftovers of a shorter pid after
    // a longer one
    std::fs::write(dir.path().join("sync.lock"), "999999999999\n").unwrap(/* t */);
    let winner = try_lock_root(dir.path()).unwrap(/* t */);
    let content = std::fs::read_to_string(dir.path().join("sync.lock")).unwrap(/* t */);
    assert_eq!(
        content.trim(),
        format!("{}", std::process::id()),
        "the winner writes its own pid, and only that"
    );
    drop(winner);
}

/// v0.6.8 (H5): the service closes ITSELF when the last handle goes.
/// A dropped `SyncService` — no explicit `shutdown`, the case of whoever
/// resets the sync by closing the engine and deleting the root — must
/// cancel the open invite, close the endpoint and WAIT for its threads:
/// nothing may write into the root afterwards. The reviewer's proof:
/// `confirm_before_the_digits_is_invalid` left the invite's thread
/// alive, and it recreated `device.key` and `devices.db` in the deleted
/// root (the `.tmp*` of the suite, seven times).
#[test]
fn a_dropped_service_closes_its_threads_and_the_root_stays_gone() {
    bounded("chiusa-drop", 120, || {
        let a = Root::new("early-drop");
        let root = a.root.clone();
        let svc = SyncService::start(&root).unwrap(/* t */);
        let invite = svc.invite("portatile", NO_RELAY).unwrap(/* t */);
        drop(invite);
        // the service goes FIRST, the root after: whoever drops both must
        // not leave a thread that recreates what the removal took away
        drop(svc);
        drop(a);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            !root.exists(),
            "la radice cancellata è ricomparsa: un filo del servizio scrive dopo la chiusura"
        );
    });
}
