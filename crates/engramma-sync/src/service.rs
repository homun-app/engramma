// SPDX-License-Identifier: Apache-2.0

//! The sync service (v0.6.2, S1): everything the sync CLI did, as a
//! LIBRARY type. One `SyncService` per data root, on its own thread with
//! its own runtime — the CLI parses flags and prints, the Node binding
//! answers Homun, and there is ONE copy of the cycle (the defect K1
//! showed: two cycles to keep aligned is one too many).
//!
//! What moved here from `engramma-cli`'s `synccli.rs`:
//!
//! - the pairing roads (invite with a ticket, the short code, the join),
//!   with the SAS confirmation as a PARAMETER: the service hands the six
//!   digits out and waits for the answer — the CLI reads a line, a UI
//!   answers from a button (and nothing can confirm by default: the
//!   digits must be asked for first);
//! - the one pass (`run_once`) and the continuous service (`serve_start` /
//!   `serve_stop`): the gate at acceptance, the `Write`/`Others`/`Clock`
//!   ticks, the pass worker, the per-cell write guard;
//! - sharing, unsharing, the device list, the removal with its rotation
//!   (and its courtesy call: the removed device hears it from the
//!   remover, stops its own service and never waits for keys that will
//!   not come);
//! - the events (passes, arrivals, devices, the self-removal, the unsent)
//!   out of one channel: the CLI ignores them, the binding hands them to
//!   JS.
//!
//! ONE `serve` per root: an exclusive OS lock (`<root>/sync.lock`, flock)
//! covers the continuous service and each single pass — a second service
//! on the same root, in this or in ANOTHER process, answers `SYNC_BUSY`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::OptionalExtension;
use tokio_util::sync::CancellationToken;

use crate::device::{DeviceKey, hex};
use crate::keystore::KeyStore;
use crate::pairing::{self, Side, Ticket};
use crate::pairing_net::{self, JoinReport};
use crate::protocol::SyncedCell;
use crate::registry::DeviceRegistry;
use crate::relay::{self, RelayChoice};
use crate::serve::{self, CellSummary, PassReport, PeerOutcome};
use crate::sharedcell::{self, SharedCell};
use crate::{Error, Result};

/// The exclusive root lock's file name.
const LOCK_NAME: &str = "sync.lock";

// ---------------------------------------------------------------------------
// the errors (stable codes, the same names the JS contract answers)
// ---------------------------------------------------------------------------

/// Everything the service can refuse with. The `code` is stable and
/// travels to JS verbatim (`SYNC_BUSY`, …); no message carries note text,
/// a ticket or a short code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// Another service holds the root's lock (`SYNC_BUSY`).
    Busy,
    /// The SAS was refused by one of the two sides (`PAIRING_REJECTED`).
    PairingRejected,
    /// The human never answered the SAS (`PAIRING_TIMEOUT`).
    PairingTimeout,
    /// The ticket (or the window) ran out (`PAIRING_EXPIRED`).
    PairingExpired,
    /// No blind mailbox is configured for this root (`NO_MAILBOX`).
    NoMailbox,
    /// The cell is not shared from this root (`NOT_SHARED`).
    NotShared,
    /// Removing it would orphan the root (`LAST_DEVICE`).
    LastDevice,
    /// The pairing was cancelled by the caller.
    Cancelled,
    /// Something the caller named is not there.
    NotFound(String),
    /// A wrong argument of the call.
    Invalid(String),
    /// v0.6.4 (D4): the peer speaks other materialization rules; the
    /// device named needs an upgrade (or this one does). Nothing was
    /// exchanged.
    PeerOutdated(String),
    /// Everything else (a broken root, a failed endpoint). The message is
    /// the underlying error's own, so the words a test or a human greps
    /// for (a burnt channel's «bruciato») survive the crossing.
    Io(String),
}

impl ServiceError {
    /// The stable code, the one JS reads as `e.code`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "SYNC_BUSY",
            Self::PairingRejected => "PAIRING_REJECTED",
            Self::PairingTimeout => "PAIRING_TIMEOUT",
            Self::PairingExpired => "PAIRING_EXPIRED",
            Self::NoMailbox => "NO_MAILBOX",
            Self::NotShared => "NOT_SHARED",
            Self::LastDevice => "LAST_DEVICE",
            Self::Cancelled => "CANCELLED",
            Self::NotFound(_) => "NOT_FOUND",
            Self::Invalid(_) => "INVALID_ARGUMENT",
            Self::PeerOutdated(_) => "PEER_OUTDATED",
            Self::Io(_) => "IO",
        }
    }

    /// One line, no secrets, no note text.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Busy => "another sync service is already active on this root \
                           (sync.lock)"
                .into(),
            Self::PairingRejected => "verification code refused".into(),
            Self::PairingTimeout => "no answer to the verification code in time".into(),
            Self::PairingExpired => "the ticket expired".into(),
            Self::NoMailbox => "no mailbox configured: `engramma mailbox add` first".into(),
            Self::NotShared => "the cell is not shared from this root".into(),
            Self::LastDevice => "the last device is not removed".into(),
            Self::Cancelled => "pairing cancelled".into(),
            Self::NotFound(what) => format!("{what} does not exist"),
            Self::Invalid(why) => why.clone(),
            Self::PeerOutdated(who) => format!(
                "the device {who} uses different materialization rules: update it (or update this device) before syncing"
            ),
            Self::Io(why) => why.clone(),
        }
    }

    fn io(e: impl std::fmt::Display) -> Self {
        Self::Io(e.to_string())
    }

    /// The registry's own refusals, by their words.
    fn from_registry(e: &Error) -> Self {
        let text = e.to_string();
        if text.contains("last device") || text.contains("orphan") {
            Self::LastDevice
        } else if text.contains("not a live member") {
            Self::NotFound("the device".into())
        } else {
            Self::io(text)
        }
    }

    /// A pairing flow's end, from the flags the driver kept.
    fn from_pairing(inner: &PairingInner, e: &Error) -> Self {
        if inner.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Self::Cancelled;
        }
        if inner.timed_out.load(std::sync::atomic::Ordering::Acquire) {
            return Self::PairingTimeout;
        }
        let text = e.to_string();
        if inner.sas_shown.load(std::sync::atomic::Ordering::Acquire)
            && !inner.refused.load(std::sync::atomic::Ordering::Acquire)
        {
            // the SAS was on both screens and OUR side never said no: the
            // other side refused, or vanished right after
            return Self::PairingRejected;
        }
        // v0.6.19: this side said no — a variant, never a word to find in
        // the message (the messages may change language; this does not)
        if matches!(e, Error::SasRefused(_)) {
            return Self::PairingRejected;
        }
        if text.contains("expired") || text.contains("timed out (ten minutes)") {
            return Self::PairingExpired;
        }
        Self::io(text)
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for ServiceError {}

/// Convenience alias.
pub type SResult<T> = std::result::Result<T, ServiceError>;

// ---------------------------------------------------------------------------
// the views
// ---------------------------------------------------------------------------

/// One device of the list, as `devices` answers it.
#[derive(Debug, Clone)]
pub struct DeviceView {
    /// The name its owner chose.
    pub name: String,
    /// The device id, hex (the signing public key).
    pub device: String,
    /// The first 8 characters of the device id.
    pub short_id: String,
    /// Is it THIS device?
    pub is_self: bool,
    /// The last session the address book saw (RFC 3339).
    pub last_contact: Option<String>,
    /// What the book holds: `"diretta"`, `"relay"` or nothing yet —
    /// stable values of the contract: `diretta` stays Italian.
    pub via: Option<String>,
    /// Live, or removed (the row stays for the history).
    pub live: bool,
}

/// One shared cell of the status.
#[derive(Debug, Clone)]
pub struct CellStatusView {
    /// The cell's name in this root.
    pub name: String,
    /// Shared from here (the status lists shared cells: always `true`).
    pub shared: bool,
    /// The cell's current key epoch.
    pub epoch: u64,
    /// How many live devices hold it.
    pub devices: usize,
    /// The last confirmed session on this cell (RFC 3339).
    pub last_session: Option<String>,
    /// Operations written but not yet confirmed anywhere (`head − frontier`).
    pub pending: i64,
    /// Operations that still cannot leave (R4).
    pub unsent: usize,
}

/// What `status` answers: this device, the service, the relay, the cells.
#[derive(Debug, Clone)]
pub struct StatusView {
    /// This device.
    pub device: DeviceView,
    /// Is the continuous service running?
    pub serving: bool,
    /// The relay the root chose: `"default"`, `"none"`, or the URL.
    pub relay: Option<String>,
    /// One row per shared cell.
    pub cells: Vec<CellStatusView>,
}

/// What one whole-cell share says.
#[derive(Debug, Clone)]
pub struct ShareView {
    /// The cell's name.
    pub cell: String,
    /// The adopt operation's id (the signed root of the pre-sync era).
    pub adopt: String,
    /// The epoch the share opened (0).
    pub epoch: u64,
    /// The first 8 characters of the epoch key.
    pub key: String,
    /// How many OTHER live devices received a wrapped key.
    pub wraps: usize,
}

/// What a removal says: the accepted set of the removed device's
/// operations, and the new epoch of every shared cell.
#[derive(Debug, Clone)]
pub struct RemoveView {
    /// The removed device's name.
    pub removed: String,
    /// The Merkle root over the removed device's accepted operations.
    pub accepted_root: String,
    /// How many operations that is.
    pub accepted_count: u64,
    /// One row per rotated cell.
    pub rotated: Vec<(String, u64)>,
}

/// What the continuous service published when it started.
#[derive(Debug, Clone)]
pub struct ServeInfo {
    /// The transport node id, hex.
    pub node: String,
    /// The direct socket addresses.
    pub addrs: Vec<String>,
    /// The relay URL, when one is connected.
    pub relay: Option<String>,
    /// The relay did not connect in time: the address carries none.
    pub relay_warning: Option<String>,
}

/// The pairing's shape as the UI reads it: the ticket (both forms), the
/// short code, what the relay did — none of it a secret that outlives the
/// ten minutes.
#[derive(Debug, Clone)]
pub struct PairingInfo {
    /// The ticket, textual form (invite only).
    pub ticket: Option<String>,
    /// The ticket in the QR's alphanumeric clothes (invite only).
    pub ticket_qr: Option<String>,
    /// The short code (code only).
    pub code: Option<String>,
    /// When the ticket (the window) ends, RFC 3339 (the UI's clock).
    pub expires_at: String,
    /// The same instant as Unix milliseconds (the CLI's JSON shape, kept
    /// byte for byte through the crossing).
    pub expires_ms: u64,
    /// The relay URL the ticket carries, when there is one.
    pub relay: Option<String>,
    /// The relay never connected: said, never silent.
    pub relay_warning: Option<String>,
    /// The inviter's transport node id, hex (CLI printing only).
    pub node: Option<String>,
    /// The inviter's direct addresses (CLI printing only).
    pub addrs: Vec<String>,
}

/// One cell a pairing delivered, or received.
#[derive(Debug, Clone)]
pub struct CellArrivedView {
    /// The cell's name.
    pub name: String,
    /// The operations that crossed.
    pub ops: usize,
}

/// The other device, briefly.
#[derive(Debug, Clone)]
pub struct DeviceBrief {
    /// Its name.
    pub name: String,
    /// The first 8 characters of its device id.
    pub short_id: String,
}

/// What a completed pairing says.
#[derive(Debug, Clone)]
pub struct PairingView {
    /// The other device.
    pub peer: Option<DeviceBrief>,
    /// The cells, delivered (invite) or received (join).
    pub cells: Vec<CellArrivedView>,
    /// The registry rows applied.
    pub registry_rows: usize,
}

// ---------------------------------------------------------------------------
// the events
// ---------------------------------------------------------------------------

/// What the service tells the world while it runs. Nothing here carries
/// note text, a ticket or a short code.
#[derive(Debug, Clone)]
pub enum ServiceEvent {
    /// A pass finished (the continuous service's own passes only — a
    /// `run_once` returns its report to the caller instead).
    Pass {
        /// One row per device the pass tried.
        peers: Vec<PeerOutcome>,
        /// One row per shared cell.
        cells: Vec<CellSummary>,
    },
    /// New operations landed (a pass's or an inbound session's): the cell
    /// and how many. The UI updates its view and calls `embedPending` —
    /// the vectors do not travel, they are computed here.
    Arrived {
        /// The cell's name.
        cell: String,
        /// How many operations landed.
        count: usize,
    },
    /// A device entered or left the list, heard from another device.
    Device {
        /// Its name.
        name: String,
        /// The first 8 characters of its device id.
        short: String,
        /// `true` when it entered, `false` when it left.
        added: bool,
    },
    /// THIS device was removed by another: the service stops by itself —
    /// the new keys will not arrive.
    RemovedSelf,
    /// Operations that could not be sealed (R4): the cell and how many.
    Unsent {
        /// The cell's name.
        cell: String,
        /// How many are stuck.
        count: usize,
    },
    /// An error of the cycle that does not stop it.
    Error {
        /// A stable code (`IO`, …).
        code: String,
        /// One line, no contents.
        message: String,
    },
}

// ---------------------------------------------------------------------------
// the pairing handle
// ---------------------------------------------------------------------------

/// How long the service waits for the human's SAS answer (overridable in
/// the tests with `ENGRAMMA_SAS_TIMEOUT_MS`).
pub const SAS_TIMEOUT: Duration = Duration::from_secs(2 * 60);

fn sas_timeout() -> Duration {
    std::env::var("ENGRAMMA_SAS_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(SAS_TIMEOUT)
}

/// The human's (or the test's) answer to one SAS round, or the service's
/// own timeout/cancel word.
enum Answer {
    /// Confirmed (or refused) by the caller.
    Human(bool),
    /// `cancel()` was called.
    Cancelled,
}

/// Where one pairing stands.
enum PairingState {
    /// The SAS of the next round is not out yet.
    Waiting,
    /// The six digits are on the screen.
    SasReady(String),
    /// The round was answered; the flow goes on.
    Answered,
    /// The flow ended.
    Done(SResult<PairingView>),
}

/// A pairing in flight, from the inviter's, the code owner's or the
/// joiner's side. The staged contract a UI drives:
///
/// 1. `info` — the ticket (both forms) or the code, ready when the
///    inviter's endpoint is;
/// 2. `sas` — the six digits, when both sides hold them; with the short
///    code there are TWO rounds (the code's meeting and the ticket flow):
///    `sas` may be asked again after each `confirm`;
/// 3. `confirm` — the human's answer; before the digits are out it is
///    `INVALID_ARGUMENT`, and there is no way to confirm by default;
/// 4. `done` — the cells, the other device; or the refusal (`refused`,
///    `timeout`, `expired`, `cancelled`);
/// 5. `cancel` — any moment: the pairing endpoint closes and the other
///    side hears a clean refusal.
#[derive(Clone)]
pub struct PairingHandle {
    inner: Arc<PairingInner>,
}

/// The handle's shared half: the staged state the UI reads and answers.
struct PairingInner {
    info: std::sync::Mutex<Option<PairingInfo>>,
    info_cv: std::sync::Condvar,
    state: std::sync::Mutex<PairingState>,
    state_cv: std::sync::Condvar,
    /// The answers to the SAS rounds. v0.6.3 (C5.5): an unbounded TOKIO
    /// channel — the round AWAITS the answer instead of parking a worker,
    /// and the sender side stays the plain sync call it always was for
    /// the UI thread.
    answers: tokio::sync::mpsc::UnboundedSender<Answer>,
    /// The receiving end, owned here behind an async mutex: the rounds
    /// await it (SasConfirm), the handle never touches it.
    answers_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Answer>>,
    /// `cancel()` was called.
    cancelled: std::sync::atomic::AtomicBool,
    /// This side's human never answered.
    timed_out: std::sync::atomic::AtomicBool,
    /// This side's human said no.
    refused: std::sync::atomic::AtomicBool,
    /// The digits were on this screen at least once.
    sas_shown: std::sync::atomic::AtomicBool,
    /// Which SAS round is next.
    round: std::sync::atomic::AtomicU32,
}

impl PairingInner {
    fn set_info(&self, info: PairingInfo) {
        if let Ok(mut slot) = self.info.lock() {
            *slot = Some(info);
        }
        self.info_cv.notify_all();
    }

    fn set_state(&self, state: PairingState) {
        if let Ok(mut slot) = self.state.lock() {
            *slot = state;
        }
        self.state_cv.notify_all();
    }

    /// The SAS closure one round runs with: hand the digits out, AWAIT the
    /// answer (bounded), and remember WHY when the answer never came.
    /// v0.6.3 (C5.5): the wait is a Future — no worker is parked while
    /// the human looks at the screen, and the runtime can be one thread
    /// (before, the blocked closure froze a single-thread runtime's wire:
    /// the v0.6.2 S1 defect that two workers worked around).
    async fn confirm_round(&self, digits: &str) -> bool {
        self.sas_shown
            .store(true, std::sync::atomic::Ordering::Release);
        let _round = self.round.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.set_state(PairingState::SasReady(digits.to_string()));
        let mut answer_rx = self.answers_rx.lock().await;
        match tokio::time::timeout(sas_timeout(), answer_rx.recv()).await {
            Ok(Some(Answer::Human(true))) => {
                self.set_state(PairingState::Answered);
                true
            }
            Ok(Some(Answer::Human(false))) => {
                self.refused
                    .store(true, std::sync::atomic::Ordering::Release);
                self.set_state(PairingState::Answered);
                false
            }
            Ok(Some(Answer::Cancelled)) => false,
            Ok(None) => false,
            Err(_) => {
                self.timed_out
                    .store(true, std::sync::atomic::Ordering::Release);
                false
            }
        }
    }
}

/// v0.6.3 (C5.5): the SAS rounds of a pairing, as the pairing flow
/// awaits them — `PairingInner` IS the round gate.
impl crate::pairing_net::SasConfirm for PairingInner {
    async fn confirm(&self, digits: &str) -> bool {
        self.confirm_round(digits).await
    }
}

impl PairingHandle {
    fn from_parts(
        answers: tokio::sync::mpsc::UnboundedSender<Answer>,
        answers_rx: tokio::sync::mpsc::UnboundedReceiver<Answer>,
    ) -> Self {
        Self {
            inner: Arc::new(PairingInner {
                info: std::sync::Mutex::new(None),
                info_cv: std::sync::Condvar::new(),
                state: std::sync::Mutex::new(PairingState::Waiting),
                state_cv: std::sync::Condvar::new(),
                answers,
                answers_rx: tokio::sync::Mutex::new(answers_rx),
                cancelled: std::sync::atomic::AtomicBool::new(false),
                timed_out: std::sync::atomic::AtomicBool::new(false),
                refused: std::sync::atomic::AtomicBool::new(false),
                sas_shown: std::sync::atomic::AtomicBool::new(false),
                round: std::sync::atomic::AtomicU32::new(0),
            }),
        }
    }

    /// The ticket (both forms), the code, the relay: ready when the
    /// inviter's endpoint is (the relay wait included). `deadline` bounds
    /// the wait; join answers at once.
    pub fn info(&self, deadline: Duration) -> SResult<PairingInfo> {
        let guard = self
            .inner
            .info
            .lock()
            .map_err(|_| ServiceError::io("pairing lock poisoned"))?;
        let (guard, _left) = self
            .inner
            .info_cv
            .wait_timeout_while(guard, deadline, |info| info.is_none())
            .map_err(|_| ServiceError::io("pairing lock poisoned"))?;
        if guard.is_some() {
            return Ok(guard.clone().expect(/* just checked */ "info"));
        }
        Err(ServiceError::PairingExpired)
    }

    /// The six digits of the next round (there may be two with the short
    /// code: ask again after each `confirm`).
    pub fn sas(&self, deadline: Duration) -> SResult<String> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ServiceError::io("pairing lock poisoned"))?;
        let until = std::time::Instant::now() + deadline;
        loop {
            match &*state {
                PairingState::SasReady(digits) => return Ok(digits.clone()),
                PairingState::Done(Err(e)) => return Err(e.clone()),
                PairingState::Done(Ok(_)) => {
                    return Err(ServiceError::Invalid(
                        "the pairing is already over: no more digits".into(),
                    ));
                }
                _ => {}
            }
            let now = std::time::Instant::now();
            if now >= until {
                return Err(ServiceError::PairingTimeout);
            }
            let (guard, _left) = self
                .inner
                .state_cv
                .wait_timeout(state, until - now)
                .map_err(|_| ServiceError::io("pairing lock poisoned"))?;
            state = guard;
        }
    }

    /// The human's answer to the digits on the screen. Before `sas` hands
    /// them out it is `INVALID_ARGUMENT` — there is no confirming without
    /// comparing, and no confirming by default.
    pub fn confirm(&self, same: bool) -> SResult<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ServiceError::io("pairing lock poisoned"))?;
        match &*state {
            PairingState::SasReady(_) => {
                *state = PairingState::Answered;
                drop(state);
                self.inner.state_cv.notify_all();
                self.inner
                    .answers
                    .send(Answer::Human(same))
                    .map_err(|_| ServiceError::Cancelled)?;
                Ok(())
            }
            PairingState::Done(Err(e)) => Err(e.clone()),
            PairingState::Done(Ok(_)) => Err(ServiceError::Invalid(
                "the pairing is already over: nothing to confirm".into(),
            )),
            _ => Err(ServiceError::Invalid(
                "confirm before the digits: `sas()` first, the comparison is not skipped".into(),
            )),
        }
    }

    /// The end of the whole flow: the other device and the cells — or the
    /// refusal.
    pub fn done(&self, deadline: Duration) -> SResult<PairingView> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ServiceError::io("pairing lock poisoned"))?;
        let until = std::time::Instant::now() + deadline;
        loop {
            if let PairingState::Done(out) = &*state {
                return out.clone();
            }
            let now = std::time::Instant::now();
            if now >= until {
                return Err(ServiceError::PairingTimeout);
            }
            let (guard, _left) = self
                .inner
                .state_cv
                .wait_timeout(state, until - now)
                .map_err(|_| ServiceError::io("pairing lock poisoned"))?;
            state = guard;
        }
    }

    /// Close the pairing endpoint, now: the other side hears a clean
    /// refusal, never a hang.
    pub fn cancel(&self) {
        self.inner
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.inner.answers.send(Answer::Cancelled);
        self.inner.state_cv.notify_all();
    }
}

/// The ticket in the QR's ALPHANUMERICAL clothes: uppercase base32 (the
/// payload already is `a-z2-7`) and a prefix of capitals and a dash —
/// every character sits in the QR alphanumeric alphabet (`0-9 A-Z $%*+-./:
/// `), so a mode-alpha QR encodes it in fewer, bigger modules than the
/// byte mode would.
#[must_use]
pub fn ticket_qr_form(ticket: &Ticket) -> String {
    let text = ticket.encode();
    match text.strip_prefix("engramma-invite-v1.") {
        Some(payload) => format!("ENG-INVITE-V1.{payload}").to_uppercase(),
        None => text.to_uppercase(),
    }
}

/// Both textual forms of a ticket enter: the lowercase original and the
/// QR's uppercase clothes.
#[must_use]
pub fn normalize_ticket_input(text: &str) -> String {
    let t = text.trim();
    if let Some(payload) = t.strip_prefix("ENG-INVITE-V1.") {
        let lowered = payload.to_lowercase();
        return format!("engramma-invite-v1.{lowered}");
    }
    // v0.6.3 (C5.1): a fully-uppercase paste of the ORIGINAL form decodes
    // too (base32 is case-insensitive here because we wrote it) — the
    // branch was dead before: its prefix had three Gs
    if let Some(payload) = t.strip_prefix("ENGRAMMA-INVITE-V1.") {
        let lowered = payload.to_lowercase();
        return format!("engramma-invite-v1.{lowered}");
    }
    t.to_string()
}

// ---------------------------------------------------------------------------
// the exclusive root lock
// ---------------------------------------------------------------------------

/// The held root lock: drop it (or die) and the next service may take it.
pub struct RootLockGuard {
    file: Option<std::fs::File>,
}

impl Drop for RootLockGuard {
    fn drop(&mut self) {
        // flock leaves with the descriptor: closing is releasing
        self.file.take();
    }
}

impl RootLockGuard {
    /// A guard that holds nothing (the lock's own tests).
    #[must_use]
    pub fn none() -> Self {
        Self { file: None }
    }
}

/// Try to take `<root>/sync.lock` exclusively (flock: released even if the
/// process dies). `Err(Busy)` when someone else — this process or another
/// — already serves this root.
///
/// # Errors
/// [`ServiceError::Busy`] when the lock is held; [`ServiceError::Io`] on
/// filesystem failure.
pub fn try_lock_root(root: &Path) -> SResult<RootLockGuard> {
    use std::io::Write;
    let path = root.join(LOCK_NAME);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| ServiceError::io(format!("{}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
        let taken = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if taken != 0 {
            // v0.6.3 (C5.2): who finds the lock busy writes NOTHING — the
            // pid in the file stays the holder's, whoever asks
            return Err(ServiceError::Busy);
        }
    }
    // the winner alone writes its pid, truncating what an earlier holder
    // left (a shorter pid after a longer one must not keep the tail)
    let _ = file.set_len(0);
    let _ = writeln!(file, "{}", std::process::id());
    #[cfg(not(unix))]
    {
        // no flock off unix: the lock is the file's existence, best effort
        // (the shipped targets — macOS, Linux — are unix)
    }
    Ok(RootLockGuard { file: Some(file) })
}

/// Is the cell shared from this root? (`dropCell` asks before deleting a
/// file the service would still sync.)
///
/// # Errors
/// [`ServiceError::Io`] on SQL failure.
pub fn cell_is_shared(root: &Path, cell: &str) -> SResult<bool> {
    let reg = DeviceRegistry::open(root).map_err(ServiceError::io)?;
    let row: Option<i64> = reg
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM shared_cells WHERE name = ?1",
            [cell],
            |r| r.get(0),
        )
        .optional()
        .map_err(ServiceError::io)?;
    Ok(row.unwrap_or(0) > 0)
}

// ---------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------

/// One command to the worker (each carries its own reply channel).
enum Command {
    Status {
        reply: std::sync::mpsc::Sender<SResult<StatusView>>,
    },
    Devices {
        reply: std::sync::mpsc::Sender<SResult<Vec<DeviceView>>>,
    },
    Invite {
        name: String,
        flag: Option<RelayChoice>,
        reply: std::sync::mpsc::Sender<SResult<PairingHandle>>,
    },
    Code {
        name: String,
        flag: Option<RelayChoice>,
        reply: std::sync::mpsc::Sender<SResult<PairingHandle>>,
    },
    Join {
        text: String,
        name: String,
        flag: Option<RelayChoice>,
        reply: std::sync::mpsc::Sender<SResult<PairingHandle>>,
    },
    Remove {
        name: String,
        reply: std::sync::mpsc::Sender<SResult<RemoveView>>,
    },
    Share {
        cell: String,
        reply: std::sync::mpsc::Sender<SResult<ShareView>>,
    },
    Unshare {
        cell: String,
        reply: std::sync::mpsc::Sender<SResult<()>>,
    },
    RunOnce {
        flag: Option<RelayChoice>,
        reply: std::sync::mpsc::Sender<SResult<PassReport>>,
    },
    ServeStart {
        flag: Option<RelayChoice>,
        reply: std::sync::mpsc::Sender<SResult<ServeInfo>>,
    },
    ServeStop {
        reply: std::sync::mpsc::Sender<SResult<()>>,
    },
    Shutdown {
        reply: std::sync::mpsc::Sender<SResult<()>>,
    },
}

/// What the worker holds while the continuous service runs.
struct Serving {
    stop: CancellationToken,
    info: ServeInfo,
    endpoint: iroh::Endpoint,
    /// the loop's own "I am done" signal
    done: tokio::sync::oneshot::Receiver<()>,
    /// the root's exclusive lock, held while the service runs (the
    /// underscore keeps the intent loud: nobody reads it, dropping it IS
    /// the release)
    _lock: RootLockGuard,
}

/// The sync service of one data root: pairing, passes, the continuous
/// loop, the list, the sharing — on its own thread. Clone the `Arc` and
/// call from anywhere; every call answers within its own bound.
pub struct SyncService {
    inner: Arc<ServiceInner>,
}

struct ServiceInner {
    root: PathBuf,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    events: std::sync::Mutex<Option<std::sync::mpsc::Receiver<ServiceEvent>>>,
    join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// v0.6.8 (H5): the service closes ITSELF when the last handle goes.
/// Whoever resets the sync — close the engine, delete the root — with an
/// invite in flight must not find the root recreated by a thread that
/// outlived the service (`device.key` and `devices.db` written into a
/// deleted folder: the `.tmp*` of the suite). The close cancels the
/// pairings and the serving loop, closes the endpoint and WAITS for the
/// threads within the declared budgets ([`SHUTDOWN_REPLY`],
/// [`THREAD_CLOSE`]); a thread that overruns is an error that SHOWS on
/// stderr, never a thread that stays alive in silence. Nothing writes
/// into the root after this returns; `device.key` is born only from an
/// explicit action (`start`, a pairing), never from a background thread.
impl Drop for ServiceInner {
    fn drop(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = self.cmd_tx.send(Command::Shutdown { reply: tx }).is_ok();
        if sent && rx.recv_timeout(SHUTDOWN_REPLY).is_err() {
            eprintln!(
                "engramma-sync: the service of {} did not answer the shutdown within {SHUTDOWN_REPLY:?} — its threads stay alive",
                self.root.display()
            );
        }
        if let Ok(mut slot) = self.join.lock()
            && let Some(handle) = slot.take()
            && !join_bounded(handle, THREAD_CLOSE)
        {
            eprintln!(
                "engramma-sync: the service thread of {} did not close within {THREAD_CLOSE:?}",
                self.root.display()
            );
        }
    }
}

impl SyncService {
    /// Start the service of a root (its worker thread, its runtime). One
    /// per root in a process; a second one on the same root answers
    /// `SYNC_BUSY` at the first `serve`/`run`, not here.
    ///
    /// # Errors
    /// [`ServiceError::Io`] when the root cannot be opened.
    pub fn start(root: &Path) -> SResult<Arc<Self>> {
        std::fs::create_dir_all(root.join("cells"))
            .map_err(|e| ServiceError::io(format!("{}: {e}", root.display())))?;
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let root_owned = root.to_path_buf();
        let worker = std::thread::Builder::new()
            .name("engramma-sync".into())
            .spawn(move || worker_main(&root_owned, cmd_rx, event_tx))
            .map_err(ServiceError::io)?;
        Ok(Arc::new(Self {
            inner: Arc::new(ServiceInner {
                root: root.to_path_buf(),
                cmd_tx,
                events: std::sync::Mutex::new(Some(event_rx)),
                join: std::sync::Mutex::new(Some(worker)),
            }),
        }))
    }

    /// The root this serves.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The events of this service, the first caller only.
    #[must_use]
    pub fn take_events(&self) -> Option<std::sync::mpsc::Receiver<ServiceEvent>> {
        self.inner
            .events
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    fn call<T: Send + 'static>(
        &self,
        make: impl FnOnce(std::sync::mpsc::Sender<SResult<T>>) -> Command,
        bound: Duration,
    ) -> SResult<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.inner
            .cmd_tx
            .send(make(tx))
            .map_err(|_| ServiceError::Io("the service is no longer alive".into()))?;
        rx.recv_timeout(bound)
            .map_err(|_| ServiceError::Io("the service did not answer in time".into()))?
    }

    /// This device, the service, the relay, the shared cells.
    pub fn status(&self) -> SResult<StatusView> {
        self.call(|reply| Command::Status { reply }, Duration::from_secs(30))
    }

    /// The devices of the list: name, short id, is-this, last contact,
    /// what the book holds, live or removed.
    pub fn devices(&self) -> SResult<Vec<DeviceView>> {
        self.call(|reply| Command::Devices { reply }, Duration::from_secs(30))
    }

    /// Invite with a ticket (ten minutes, one use): the endpoint, the
    /// relay wait, the ticket in both forms — then the wait for the device
    /// that enters.
    pub fn invite(&self, name: &str, flag: Option<RelayChoice>) -> SResult<PairingHandle> {
        self.call(
            |reply| Command::Invite {
                name: name.to_string(),
                flag,
                reply,
            },
            Duration::from_secs(60),
        )
    }

    /// Invite with the short code (the blind mailbox must be configured:
    /// `NO_MAILBOX` otherwise).
    pub fn code(&self, name: &str, flag: Option<RelayChoice>) -> SResult<PairingHandle> {
        self.call(
            |reply| Command::Code {
                name: name.to_string(),
                flag,
                reply,
            },
            Duration::from_secs(60),
        )
    }

    /// Enter with a ticket (both forms) or a short code.
    pub fn join(
        &self,
        ticket_or_code: &str,
        name: &str,
        flag: Option<RelayChoice>,
    ) -> SResult<PairingHandle> {
        self.call(
            |reply| Command::Join {
                text: normalize_ticket_input(ticket_or_code),
                name: name.to_string(),
                flag,
                reply,
            },
            Duration::from_secs(60),
        )
    }

    /// Remove a device and rotate every shared cell's key; the removed
    /// device hears it from this one and stops its own service.
    pub fn remove(&self, name: &str) -> SResult<RemoveView> {
        self.call(
            |reply| Command::Remove {
                name: name.to_string(),
                reply,
            },
            Duration::from_secs(90),
        )
    }

    /// Share a cell (the adopt, epoch 0, the wrapped keys).
    pub fn share(&self, cell: &str) -> SResult<ShareView> {
        self.call(
            |reply| Command::Share {
                cell: cell.to_string(),
                reply,
            },
            Duration::from_secs(30),
        )
    }

    /// Stop sharing a cell from here (`NOT_SHARED` when it is not).
    pub fn unshare(&self, cell: &str) -> SResult<()> {
        self.call(
            |reply| Command::Unshare {
                cell: cell.to_string(),
                reply,
            },
            Duration::from_secs(30),
        )
    }

    /// One pass with every reachable device and the blind node. With the
    /// continuous service on, the pass rides the service's own endpoint
    /// and the same one-pass-at-a-time lock.
    pub fn run_once(&self, flag: Option<RelayChoice>) -> SResult<PassReport> {
        self.call(
            |reply| Command::RunOnce { flag, reply },
            Duration::from_secs(180),
        )
    }

    /// The continuous service: accept, sync at every local write and
    /// every five minutes. A second one on the same root (this process or
    /// another) is `SYNC_BUSY`. Starting twice changes nothing.
    pub fn serve_start(&self, flag: Option<RelayChoice>) -> SResult<ServeInfo> {
        self.call(
            |reply| Command::ServeStart { flag, reply },
            Duration::from_secs(60),
        )
    }

    /// Stop the continuous service (and nothing else): the session in
    /// flight is interrupted, the convergence holds by construction, and
    /// the lock goes. Stopping a stopped service changes nothing.
    pub fn serve_stop(&self) -> SResult<()> {
        self.call(
            |reply| Command::ServeStop { reply },
            Duration::from_secs(30),
        )
    }

    /// The end of the service: the continuous loop stops, the worker
    /// thread ends. `Engine.close` calls this; the calls after it answer
    /// `IO`.
    pub fn shutdown(&self) {
        let _ = self.call(|reply| Command::Shutdown { reply }, Duration::from_secs(20));
        // the worker ends with its reply: wait for the thread itself, so
        // the runtime and the endpoint it drives are gone when this
        // returns (Engine.close must not leave a thread behind)
        if let Ok(mut slot) = self.inner.join.lock()
            && let Some(handle) = slot.take()
        {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// the worker thread
// ---------------------------------------------------------------------------

type EventTx = std::sync::mpsc::Sender<ServiceEvent>;

fn trace() -> bool {
    std::env::var("ENGRAMMA_TRACE").is_ok()
}

fn worker_main(
    root: &Path,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
    events: EventTx,
) {
    let root = root.to_path_buf();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = events.send(ServiceEvent::Error {
                code: "IO".into(),
                message: e.to_string(),
            });
            return;
        }
    };
    rt.block_on(async {
        tokio::task::LocalSet::new()
            .run_until(async move {
                // the serve state and the one-pass-at-a-time lock, shared
                // by the loop and the one-shots
                let core = Arc::new(std::sync::Mutex::new(WorkerCore {
                    serving: None,
                    pass_lock: Arc::new(tokio::sync::Mutex::new(())),
                    pairings: Vec::new(),
                }));
                while let Some(cmd) = cmd_rx.recv().await {
                    let shutting_down = matches!(cmd, Command::Shutdown { .. });
                    handle_command(cmd, &root, &events, &core).await;
                    if shutting_down {
                        break;
                    }
                }
            })
            .await
    });
}

struct WorkerCore {
    serving: Option<Serving>,
    pass_lock: Arc<tokio::sync::Mutex<()>>,
    /// v0.6.8 (H5): the LIVE pairing threads of this service, with their
    /// cancel flags — the shutdown cancels and WAITS for them, and the
    /// `Drop` sends the shutdown when the last handle goes. A pairing
    /// thread outlives nobody: its ten-minute ticket wait must never
    /// write into a root the caller has already deleted (the `.tmp*` of
    /// the suite, found by the reviewer seven times).
    pairings: Vec<PairingThread>,
}

/// One live pairing thread: its handle to join, its flag to cancel.
struct PairingThread {
    inner: Arc<PairingInner>,
    join: std::thread::JoinHandle<()>,
}

/// v0.6.8 (H5): the DECLARED budgets of the close. The shutdown waits
/// for the worker's answer at most `SHUTDOWN_REPLY`, then for each
/// thread at most `THREAD_CLOSE` more; a thread that overruns either is
/// an error that SHOWS (stderr), never a thread that stays alive in
/// silence.
const SHUTDOWN_REPLY: Duration = Duration::from_secs(25);
const THREAD_CLOSE: Duration = Duration::from_secs(10);

/// Join one thread within `budget`: `true` when it ended, `false` when
/// the budget ran out (the caller makes the overrun visible — a thread
/// cannot be killed, only named).
fn join_bounded(join: std::thread::JoinHandle<()>, budget: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = tx.send(());
    });
    rx.recv_timeout(budget).is_ok()
}

/// v0.6.8 (H5): cancel every live pairing and WAIT for its thread. The
/// cancel flag the rounds already speak (25 ms of latency at most)
/// closes the endpoint and the ticket wait; the join makes «nothing
/// writes into the root after the close» a fact, not a hope.
fn close_pairings(core: &Arc<std::sync::Mutex<WorkerCore>>) -> SResult<()> {
    let mut live: Vec<PairingThread> = Vec::new();
    if let Ok(mut guard) = core.lock() {
        guard.pairings.retain(|p| !p.join.is_finished());
        live = std::mem::take(&mut guard.pairings);
    }
    for p in &live {
        p.inner
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
    let deadline = std::time::Instant::now() + THREAD_CLOSE;
    let mut stuck = 0_usize;
    for p in live {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if !join_bounded(p.join, left) {
            stuck += 1;
        }
    }
    if stuck > 0 {
        return Err(ServiceError::Io(format!(
            "{stuck} pairing threads did not close within {THREAD_CLOSE:?}"
        )));
    }
    Ok(())
}

async fn handle_command(
    cmd: Command,
    root: &Path,
    events: &EventTx,
    core: &Arc<std::sync::Mutex<WorkerCore>>,
) {
    match cmd {
        Command::Status { reply } => {
            let serving = core
                .lock()
                .ok()
                .and_then(|c| c.serving.as_ref().map(|s| s.info.clone()));
            let out = status_of(root, serving);
            let _ = reply.send(out);
        }
        Command::Devices { reply } => {
            let _ = reply.send(devices_of(root));
        }
        Command::Invite { name, flag, reply } => {
            // the first device creates the list HERE, in the command
            // itself: a `devices` right after the call sees the name the
            // caller chose (the driver thread would race it)
            let first = ensure_first_device(root, &name);
            let _ = reply.send(
                first.and_then(|()| spawn_pairing(root, PairingKind::Invite, flag, events, core)),
            );
        }
        Command::Code { name, flag, reply } => {
            let first = ensure_first_device(root, &name);
            let _ = reply.send(
                first.and_then(|()| spawn_pairing(root, PairingKind::Code, flag, events, core)),
            );
        }
        Command::Join {
            text,
            name,
            flag,
            reply,
        } => {
            // a joiner with an empty list creates its own first row too
            // (the pairing's own add lands on top of it later)
            let first = ensure_first_device(root, &name);
            let _ = reply.send(first.and_then(|()| {
                spawn_pairing(root, PairingKind::Join { text, name }, flag, events, core)
            }));
        }
        Command::Remove { name, reply } => {
            let out = remove_device(root, &name, events, core).await;
            let _ = reply.send(out);
        }
        Command::Share { cell, reply } => {
            let _ = reply.send(share_cell(root, &cell));
        }
        Command::Unshare { cell, reply } => {
            let _ = reply.send(unshare_cell(root, &cell));
        }
        Command::RunOnce { flag, reply } => {
            let out = run_once(root, flag, core).await;
            let _ = reply.send(out);
        }
        Command::ServeStart { flag, reply } => {
            let out = serve_start(root, flag, events, core).await;
            let _ = reply.send(out);
        }
        Command::ServeStop { reply } => {
            let out = serve_stop(core).await;
            let _ = reply.send(out);
        }
        Command::Shutdown { reply } => {
            let _ = serve_stop(core).await;
            // v0.6.8 (H5): the pairings close with the service — cancel,
            // then WAIT for the threads (bounded, declared)
            let out = close_pairings(core);
            let _ = reply.send(out);
        }
    }
}

/// The FIRST device creates the list by itself, with the CALLER's name
/// (the CLI's `--name`, the UI's `deviceName`): an empty list names
/// itself before any pairing road opens.
fn ensure_first_device(root: &Path, name: &str) -> SResult<()> {
    let dk = DeviceKey::load_or_create(root).map_err(ServiceError::io)?;
    let reg = DeviceRegistry::open(root).map_err(ServiceError::io)?;
    if reg.live_members().map_err(ServiceError::io)?.is_empty() {
        reg.create_first(&dk, name).map_err(ServiceError::io)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// status and the device list
// ---------------------------------------------------------------------------

/// One member row as the registry keeps it (live or removed).
struct MemberRow {
    name: String,
    device: String,
    live: bool,
}

fn member_rows(root: &Path) -> SResult<Vec<MemberRow>> {
    let reg = DeviceRegistry::open(root).map_err(ServiceError::io)?;
    let mut stmt = reg
        .conn()
        .prepare("SELECT name, device, removed_at IS NULL FROM members ORDER BY added_at, name")
        .map_err(ServiceError::io)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MemberRow {
                name: r.get(0)?,
                device: r.get(1)?,
                live: r.get::<_, i64>(2)? != 0,
            })
        })
        .map_err(ServiceError::io)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(ServiceError::io)?;
    Ok(rows)
}

/// The address book's row for a device (last contact, what it holds).
fn book_row(root: &Path, device: &str) -> SResult<(Option<String>, Option<String>)> {
    let reg = DeviceRegistry::open(root).map_err(ServiceError::io)?;
    let row: Option<String> = reg
        .conn()
        .query_row(
            "SELECT addrs FROM device_addrs WHERE device = ?1",
            [device],
            |r| r.get(0),
        )
        .optional()
        .map_err(ServiceError::io)?;
    let Some(text) = row else {
        return Ok((None, None));
    };
    let v: serde_json::Value = serde_json::from_str(&text).map_err(ServiceError::io)?;
    let relay = v["relay"].as_str().unwrap_or_default().to_string();
    let via = (!relay.is_empty())
        .then_some("relay".to_string())
        .or_else(|| {
            v["addrs"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
                .then(|| "diretta".to_string())
        });
    Ok((v["seen_at"].as_str().map(str::to_string), via))
}

fn device_views(root: &Path) -> SResult<Vec<DeviceView>> {
    let self_id = serve::self_device(root);
    let mut out = Vec::new();
    for row in member_rows(root)? {
        let (last_contact, via) = book_row(root, &row.device)?;
        out.push(DeviceView {
            name: row.name,
            device: row.device.clone(),
            short_id: row.device.chars().take(8).collect(),
            is_self: row.device == self_id,
            last_contact,
            via,
            live: row.live,
        });
    }
    Ok(out)
}

fn devices_of(root: &Path) -> SResult<Vec<DeviceView>> {
    device_views(root)
}

fn self_view(root: &Path, rows: &[MemberRow]) -> DeviceView {
    let self_id = serve::self_device(root);
    if let Some(row) = rows.iter().find(|r| r.device == self_id) {
        let (last_contact, via) = book_row(root, &row.device).unwrap_or((None, None));
        return DeviceView {
            name: row.name.clone(),
            device: self_id.clone(),
            short_id: self_id.chars().take(8).collect(),
            is_self: true,
            last_contact,
            via,
            live: row.live,
        };
    }
    // not a member yet (no invite ever ran): the device exists, the list
    // does not know it
    DeviceView {
        name: String::new(),
        device: self_id.clone(),
        short_id: self_id.chars().take(8).collect(),
        is_self: true,
        last_contact: None,
        via: None,
        live: false,
    }
}

fn status_of(root: &Path, serving: Option<ServeInfo>) -> SResult<StatusView> {
    let rows = member_rows(root)?;
    let device = self_view(root, &rows);
    let relay = if let Some(info) = &serving {
        info.relay.clone()
    } else {
        match relay::saved_choice(root).map_err(ServiceError::io)? {
            RelayChoice::Default => Some("default".into()),
            RelayChoice::Own(url) => Some(url),
            RelayChoice::Disabled => Some("none".into()),
        }
    };
    let devices = rows.iter().filter(|r| r.live).count();
    let unsent = sharedcell::unsent_events(root).map_err(ServiceError::io)?;
    let mut cells = Vec::new();
    for shared in SharedCell::all_of(root).map_err(ServiceError::io)? {
        let name = serve::cell_name(root, shared.sync_id()).unwrap_or_default();
        let head = shared.head_seq();
        let frontier = shared.cell().stable_frontier().unwrap_or(0);
        let last = shared
            .cell()
            .debug_column("SELECT COALESCE(MAX(at), '') FROM device_acks")
            .ok()
            .and_then(|v| v.first().cloned())
            .filter(|s| !s.is_empty());
        cells.push(CellStatusView {
            name: name.clone(),
            shared: true,
            epoch: shared.current_epoch().unwrap_or(0),
            devices,
            last_session: last,
            pending: (head - frontier).max(0),
            unsent: unsent.iter().filter(|(c, ..)| *c == name).count(),
        });
    }
    Ok(StatusView {
        device,
        serving: serving.is_some(),
        relay,
        cells,
    })
}

// ---------------------------------------------------------------------------
// share, unshare, remove
// ---------------------------------------------------------------------------

fn cell_file(root: &Path, cell: &str) -> PathBuf {
    root.join("cells").join(format!("{cell}.sqlite"))
}

fn share_cell(root: &Path, cell: &str) -> SResult<ShareView> {
    let path = cell_file(root, cell);
    if !path.exists() {
        return Err(ServiceError::NotFound(format!("the cell \"{cell}\"")));
    }
    let c = engramma_core::Cell::open(&path).map_err(ServiceError::io)?;
    let dk = DeviceKey::load_or_create(root).map_err(ServiceError::io)?;
    let reg = DeviceRegistry::open(root).map_err(ServiceError::io)?;
    if reg.live_members().map_err(ServiceError::io)?.is_empty() {
        // the first device creates the list by itself
        reg.create_first(&dk, "laptop").map_err(ServiceError::io)?;
    }
    // arm the signer FIRST: the adopt itself must be signed
    c.set_signer(Arc::new(crate::arrival::DeviceSigner::new(&dk)))
        .map_err(ServiceError::io)?;
    let adopt = c
        .adopt_log("cli")
        .map_err(|e| ServiceError::io(format!("cell \"{cell}\": {e}")))?;
    c.mark_shared().map_err(ServiceError::io)?;
    let sync_id = sharedcell::share_cell(root, cell).map_err(ServiceError::io)?;
    // the epoch-0 key wrapped for every OTHER live device
    let ks = KeyStore::new(reg.conn());
    let epoch0 = ks
        .key_of(&sync_id, 0)
        .map_err(ServiceError::io)?
        .ok_or_else(|| ServiceError::Io("epoch 0 is lost".into()))?;
    let mut wraps = 0_usize;
    for member in reg.live_members().map_err(ServiceError::io)? {
        if member.device == dk.device_id() {
            continue;
        }
        let Some(hpke_pub) = unhex32(&member.hpke) else {
            continue;
        };
        let wrapped =
            crate::crypto::wrap_key(&epoch0, &sync_id, 0, &hpke_pub).map_err(ServiceError::io)?;
        ks.store_wrap(&sync_id, 0, &member.device, &wrapped)
            .map_err(ServiceError::io)?;
        wraps += 1;
    }
    Ok(ShareView {
        cell: cell.to_string(),
        adopt: adopt.to_string(),
        epoch: 0,
        key: hex(&epoch0.0)[..8].to_string(),
        wraps,
    })
}

fn unshare_cell(root: &Path, cell: &str) -> SResult<()> {
    if !cell_is_shared(root, cell)? {
        return Err(ServiceError::NotShared);
    }
    sharedcell::unshare_cell(root, cell).map_err(ServiceError::io)
}

fn unhex32(text: &str) -> Option<[u8; 32]> {
    let mut out = [0_u8; 32];
    if text.len() != 64 {
        return None;
    }
    for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// The removal: the signed remove with the accepted set, the new epoch of
/// every shared cell, the wraps for whoever remains — and the courtesy
/// call that TELLS the removed device, over one session it can still
/// answer (the gate checks the CALLER, and the caller here is the
/// remover: the remove op lands as a registry row inside the exchange).
async fn remove_device(
    root: &Path,
    name: &str,
    events: &EventTx,
    core: &Arc<std::sync::Mutex<WorkerCore>>,
) -> SResult<RemoveView> {
    let dk = DeviceKey::load_or_create(root).map_err(ServiceError::io)?;
    let reg = DeviceRegistry::open(root).map_err(ServiceError::io)?;
    let target = reg
        .live_members()
        .map_err(ServiceError::io)?
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| ServiceError::NotFound(format!("no device \"{name}\"")))?;
    // the accepted set: every operation of that device this copy holds,
    // across every shared cell
    let mut hashes: Vec<String> = Vec::new();
    for shared in SharedCell::all_of(root).map_err(ServiceError::io)? {
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
        .map_err(|e| ServiceError::from_registry(&e))?;
    // the rotation: a new epoch for every shared cell, wrapped for the
    // devices that remain
    let live: Vec<_> = reg
        .live_members()
        .map_err(ServiceError::io)?
        .into_iter()
        .filter(|m| m.device != target.device)
        .collect();
    let mut rotated = Vec::new();
    for shared in SharedCell::all_of(root).map_err(ServiceError::io)? {
        let sync_id: [u8; 32] = *shared.sync_id();
        let ks = KeyStore::new(reg.conn());
        let (epoch, key) = ks.rotate(&sync_id).map_err(ServiceError::io)?;
        for member in &live {
            let Some(hpke_pub) = unhex32(&member.hpke) else {
                continue;
            };
            let wrapped = crate::crypto::wrap_key(&key, &sync_id, epoch, &hpke_pub)
                .map_err(ServiceError::io)?;
            ks.store_wrap(&sync_id, epoch, &member.device, &wrapped)
                .map_err(ServiceError::io)?;
        }
        rotated.push((
            serve::cell_name(root, &sync_id).unwrap_or_else(|_| "?".into()),
            epoch,
        ));
    }
    // the courtesy call: one session toward the removed device, so it
    // learns the removal from the remover instead of waiting for keys
    // that will never come (best effort — an unreachable device learns at
    // its own next attempt, and its gate will say no)
    notify_removed(root, &target.device, core).await;
    let _ = events.send(ServiceEvent::Device {
        name: name.to_string(),
        short: target.device.chars().take(8).collect(),
        added: false,
    });
    Ok(RemoveView {
        removed: name.to_string(),
        accepted_root: root_hash,
        accepted_count: count as u64,
        rotated,
    })
}

async fn notify_removed(root: &Path, device: &str, core: &Arc<std::sync::Mutex<WorkerCore>>) {
    let choice = match relay::resolve(root, None) {
        Ok(c) => c,
        Err(_) => return,
    };
    // the service's own endpoint when it serves (no new port to stick in
    // the peer's magicsock), a short-lived one otherwise — the guard is
    // GONE before any await (the serve task runs on this same thread)
    let serving_endpoint = core
        .lock()
        .ok()
        .and_then(|guard| guard.serving.as_ref().map(|s| s.endpoint.clone()));
    let endpoint = match serving_endpoint {
        Some(ep) => ep,
        None => {
            let dk = match DeviceKey::load_or_create(root) {
                Ok(dk) => dk,
                Err(_) => return,
            };
            match serve::bind(&dk, "0.0.0.0:0", &choice).await {
                Ok(ep) => ep,
                Err(_) => return,
            }
        }
    };
    let Some(addr) = serve::addr_of(root, device).ok().flatten() else {
        return;
    };
    let Ok(endpoint_addr) = addr.endpoint_addr() else {
        return;
    };
    if let Ok(conn) = addr.dial(&endpoint).await {
        // best effort: the exchange carries the registry rows, the remove
        // op lands, the removed side stops by itself
        let _ = serve::session_with(root, conn, device, &endpoint_addr).await;
    }
}

// ---------------------------------------------------------------------------
// the passes and the continuous service
// ---------------------------------------------------------------------------

async fn run_once(
    root: &Path,
    flag: Option<RelayChoice>,
    core: &Arc<std::sync::Mutex<WorkerCore>>,
) -> SResult<PassReport> {
    // the lock first — but only when THIS service is not the one holding
    // it: its own serve already owns the root, and a pass through the
    // same cycle is what `run_once` is for. The guard is GONE before any
    // await (the serve tasks share this thread)
    let serving_endpoint = core
        .lock()
        .map_err(|_| ServiceError::Io("service lock poisoned".into()))?
        .serving
        .as_ref()
        .map(|s| s.endpoint.clone());
    let (endpoint, lock) = match serving_endpoint {
        Some(ep) => (ep, None),
        None => {
            let lock = try_lock_root(root)?;
            let dk = DeviceKey::load_or_create(root).map_err(ServiceError::io)?;
            let choice = relay::resolve(root, flag.as_ref()).map_err(ServiceError::io)?;
            let ep = serve::bind(&dk, "0.0.0.0:0", &choice)
                .await
                .map_err(ServiceError::io)?;
            (ep, Some(lock))
        }
    };
    let pass_lock = core
        .lock()
        .ok()
        .map(|g| Arc::clone(&g.pass_lock))
        .ok_or_else(|| ServiceError::Io("service lock poisoned".into()))?;
    // one pass at a time, the same lock the serve loop's worker obeys
    let _held = pass_lock.lock().await;
    let out = serve::run_pass(root, &endpoint, &[]).await;
    drop(_held);
    drop(lock);
    out.map_err(ServiceError::io)
}

/// What the serve loop's ticks say.
enum Tick {
    /// rows that are NOT the sessions' own work (a local write): a pass
    /// toward every live device
    Write,
    /// only rows the sessions brought: a pass toward the others — never
    /// back to these sources (K1)
    Others(Vec<String>),
    /// the five-minute safety net
    Clock,
}

async fn serve_start(
    root: &Path,
    flag: Option<RelayChoice>,
    events: &EventTx,
    core: &Arc<std::sync::Mutex<WorkerCore>>,
) -> SResult<ServeInfo> {
    {
        let guard = core
            .lock()
            .map_err(|_| ServiceError::Io("service lock poisoned".into()))?;
        if let Some(s) = &guard.serving {
            // starting twice changes nothing
            return Ok(s.info.clone());
        }
    }
    let lock = try_lock_root(root)?;
    let dk = DeviceKey::load_or_create(root).map_err(ServiceError::io)?;
    let choice = relay::resolve(root, flag.as_ref()).map_err(ServiceError::io)?;
    let endpoint = serve::bind(&dk, "0.0.0.0:0", &choice)
        .await
        .map_err(ServiceError::io)?;
    // the address this service PUBLISHES must be reachable from whoever
    // reads it: the relay comes first (C1)
    let wait = relay::wait_relay(&endpoint, &choice).await;
    let relay = match &wait {
        relay::RelayWait::Connected(url) => Some(url.clone()),
        _ => None,
    };
    let relay_warning = (wait == relay::RelayWait::TimedOut).then(|| {
        format!(
            "the relay did not connect within {} s: the address goes out without a relay",
            relay::RELAY_WAIT.as_secs()
        )
    });
    let info = ServeInfo {
        node: hex(serve::node_id_of(&endpoint).as_slice()),
        addrs: serve::direct_addresses(&endpoint),
        relay,
        relay_warning,
    };
    let stop = CancellationToken::new();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let pass_lock = core
            .lock()
            .map_err(|_| ServiceError::Io("service lock poisoned".into()))?
            .pass_lock
            .clone();
        let core = Arc::clone(core);
        let root = root.to_path_buf();
        let events = events.clone();
        let stop = stop.clone();
        let endpoint = endpoint.clone();
        tokio::task::spawn_local(async move {
            serve_loop(&root, endpoint, pass_lock, stop, events).await;
            let _ = done_tx.send(());
            // the loop may have ended BY ITSELF (this device was removed:
            // the accept loop cancelled the token): the state must say so,
            // and the root's lock must go back on the shelf
            if let Ok(mut guard) = core.lock() {
                guard.serving = None;
            }
        });
    }
    core.lock()
        .map_err(|_| ServiceError::Io("service lock poisoned".into()))?
        .serving = Some(Serving {
        stop,
        info: info.clone(),
        endpoint,
        done: done_rx,
        _lock: lock,
    });
    Ok(info)
}

async fn serve_stop(core: &Arc<std::sync::Mutex<WorkerCore>>) -> SResult<()> {
    // take the serving state OUT under the lock, then let the lock go
    // BEFORE any await: the serve task ends on THIS SAME thread, and a
    // mutex held across the await would meet its own `core.lock()` —
    // a synchronous block that freezes the whole worker (the found-by-
    // deadlock defect of v0.6.2 S1: `serve_stop` never answered)
    let serving = {
        let mut guard = core
            .lock()
            .map_err(|_| ServiceError::Io("service lock poisoned".into()))?;
        guard.serving.take()
    };
    let Some(mut serving) = serving else {
        return Ok(()); // stopping a stopped service changes nothing
    };
    serving.stop.cancel();
    // the loop ends within its own bounds (a pass in flight is dropped at
    // the cancellation point); the hard bound keeps `stop()` honest
    let _ = tokio::time::timeout(Duration::from_secs(5), &mut serving.done).await;
    drop(serving); // the root's lock goes with it
    Ok(())
}

/// The service loop: the write guard (per cell), the clock, the pass
/// worker, the accept loop with the gate first — the CLI's own loop, on
/// this library's thread, speaking its events.
async fn serve_loop(
    root: &Path,
    endpoint: iroh::Endpoint,
    pass_lock: Arc<tokio::sync::Mutex<()>>,
    stop: CancellationToken,
    events: EventTx,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Tick>();
    // the write guard, shared by the watcher (it polls), the worker (it
    // settles after every pass) and the accept loop (it notes what each
    // inbound session ingested). All the tasks live on this one LocalSet
    // thread: the mutex is held for short sync spans only, never across
    // an await.
    let watch = Arc::new(std::sync::Mutex::new(
        serve::DataVersionWatch::new(root).unwrap_or_else(|e| {
            if trace() {
                eprintln!("sync serve: data_version: {e}");
            }
            // an empty watch never fires: the periodic pass still runs,
            // and the next start retries
            serve::DataVersionWatch::empty(root)
        }),
    ));
    // the watcher
    {
        let tx = tx.clone();
        let watch = Arc::clone(&watch);
        let stop = stop.clone();
        tokio::task::spawn_local(async move {
            let mut opened = std::time::Instant::now();
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(serve::WATCH_TICK) => {}
                }
                if !watch.lock().map(|w| !w.busy()).unwrap_or(false) {
                    continue;
                }
                // cells shared after the start join at the periodic refresh
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
                // let the writes settle, then decide: the cheap poll stays
                // on the file's counters, the PASS is armed by the log's
                // marker (K1)
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(serve::DEBOUNCE) => {}
                }
                let decision = watch.lock().ok().and_then(|mut w| {
                    if w.busy() || !w.moved().unwrap_or(false) {
                        None
                    } else {
                        w.evaluate().ok()
                    }
                });
                match decision {
                    Some(serve::PassDecision::All) => {
                        if trace() {
                            eprintln!("watch: tick write");
                        }
                        tx.send(Tick::Write).ok();
                    }
                    Some(serve::PassDecision::Others(skip)) => {
                        if trace() {
                            eprintln!("watch: tick write (towards the others)");
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
        let stop = stop.clone();
        tokio::task::spawn_local(async move {
            // the first tick comes AFTER the period: a pass at t=0 would
            // dial a book that is almost always stale right after the
            // start — the write watcher is the real trigger, the clock is
            // the safety net
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + serve::PERIODIC,
                serve::PERIODIC,
            );
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tick.tick() => {}
                }
                tx.send(Tick::Clock).ok();
            }
        });
    }
    // the pass worker: one pass at a time, over the ticks
    {
        let root = root.to_path_buf();
        let endpoint = endpoint.clone();
        let stop = stop.clone();
        let watch = Arc::clone(&watch);
        let events = events.clone();
        let pass_lock = pass_lock.clone();
        tokio::task::spawn_local(async move {
            loop {
                let what = tokio::select! {
                    _ = stop.cancelled() => break,
                    what = rx.recv() => {
                        let Some(what) = what else { break };
                        what
                    }
                };
                // the pass toward the OTHERS only: with no others there is
                // NO pass at all — the rows that came from the sources
                // never bounce back to them (K1)
                let (label, skip, has_targets) = match &what {
                    Tick::Others(skip) => {
                        let self_dev = serve::self_device(&root);
                        let has = DeviceRegistry::open(&root)
                            .ok()
                            .and_then(|r| r.live_members().ok())
                            .map(|ms| {
                                ms.iter()
                                    .any(|m| m.device != self_dev && !skip.contains(&m.device))
                            })
                            .unwrap_or(false);
                        ("write", skip.clone(), has)
                    }
                    Tick::Clock => ("clock", Vec::new(), true),
                    Tick::Write => ("write", Vec::new(), true),
                };
                let mut first = true;
                'rounds: loop {
                    // the first round obeys the tick (the others-only
                    // exclusions, or none at all); a RETRIGGERED round
                    // carries local rows — it goes to every live device
                    let (run, skip_now) = if first {
                        (has_targets, skip.clone())
                    } else {
                        (true, Vec::new())
                    };
                    let mut received: Vec<(String, i64)> = Vec::new();
                    if run {
                        if trace() {
                            eprintln!("worker: pass {label}");
                        }
                        let _held = pass_lock.lock().await;
                        let raced = tokio::select! {
                            _ = stop.cancelled() => true,
                            r = serve::run_pass(&root, &endpoint, &skip_now) => {
                                match r {
                                    Ok(report) => {
                                        if trace() {
                                            for p in &report.peers {
                                                eprintln!(
                                                    "pass ({label}): {} reached={} via={:?} why={:?}",
                                                    p.name, p.reached, p.via, p.why_not
                                                );
                                            }
                                        }
                                        received = report
                                            .cells
                                            .iter()
                                            .map(|c| (c.name.clone(), c.received as i64))
                                            .collect();
                                        // the events a UI reads off the
                                        // continuous service's own passes
                                        for cell in &report.cells {
                                            if cell.received > 0 {
                                                let _ = events.send(ServiceEvent::Arrived {
                                                    cell: cell.name.clone(),
                                                    count: cell.received,
                                                });
                                            }
                                            if cell.unsent > 0 {
                                                let _ = events.send(ServiceEvent::Unsent {
                                                    cell: cell.name.clone(),
                                                    count: cell.unsent,
                                                });
                                            }
                                        }
                                        let _ =
                                            events.send(ServiceEvent::Pass {
                                                peers: report.peers.clone(),
                                                cells: report.cells.clone(),
                                            });
                                        // an outbound session reconciles the
                                        // registry too: THIS device may
                                        // have been removed
                                        check_self_removed(&root, &events, &stop);
                                    }
                                    Err(e) => {
                                        if trace() {
                                            eprintln!("pass ({label}): error {e}");
                                        }
                                        let _ = events.send(ServiceEvent::Error {
                                            code: "IO".into(),
                                            message: e.to_string(),
                                        });
                                    }
                                }
                                false
                            }
                        };
                        if raced {
                            break 'rounds;
                        }
                    }
                    // the ack comes AFTER the pass (K1): the baselines take
                    // what the pass itself wrote, or its own writes would
                    // re-light the cycle. And a local write that landed
                    // MID-PASS is handed to another pass — the settle must
                    // not swallow it
                    let again = if let Ok(mut w) = watch.lock() {
                        w.finish_pass(&received).unwrap_or(false)
                    } else {
                        false
                    };
                    first = false;
                    if !again {
                        break 'rounds;
                    }
                }
            }
        });
    }
    // the accept loop: the gate before anything
    {
        let root = root.to_path_buf();
        let stop = stop.clone();
        let watch = Arc::clone(&watch);
        let events = events.clone();
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
                let watch = Arc::clone(&watch);
                let events = events.clone();
                let stop = stop.clone();
                tokio::task::spawn_local(async move {
                    let reg = match DeviceRegistry::open(&root) {
                        Ok(r) => r,
                        Err(_) => return,
                    };
                    // the gate: the caller must be a live device's
                    // transport key, or the connection closes before any
                    // exchange
                    let Ok(device) = crate::protocol::authorize(caller.as_bytes(), &reg) else {
                        return;
                    };
                    let book = serve::peer_addr_of_conn(&conn).ok();
                    let before = member_rows(&root).unwrap_or_default();
                    if let Ok(report) = serve::answer_session(&root, conn).await {
                        // the guard learns what this session ingested and
                        // from WHOM (K1), PER CELL (v0.6.2)
                        let rows: Vec<(String, i64)> = report
                            .cells
                            .iter()
                            .filter_map(|c| {
                                let id: [u8; 32] = c.sync_id.as_slice().try_into().ok()?;
                                let name = serve::cell_name(&root, &id).ok()?;
                                Some((name, c.received as i64))
                            })
                            .collect();
                        if let Ok(mut w) = watch.lock() {
                            w.note_session(&device, &rows);
                        }
                        // the arrivals a UI updates its view on
                        for (name, n) in &rows {
                            if *n > 0 {
                                let _ = events.send(ServiceEvent::Arrived {
                                    cell: name.clone(),
                                    count: *n as usize,
                                });
                            }
                        }
                        // the book keeps only what it has: the minimal row
                        // when none exists
                        if let Some(book) = book {
                            serve::remember_addr_if_absent(&root, &device, &book).ok();
                        }
                        // the registry this session brought: devices may
                        // have entered or left, and THIS device may have
                        // been removed (the removal's courtesy call lands
                        // here, on the removed side). The rows keep the
                        // removed for the history: LIVENESS decides, not
                        // presence
                        let after = member_rows(&root).unwrap_or_default();
                        let self_id = serve::self_device(&root);
                        for row in &after {
                            if row.live && !before.iter().any(|b| b.device == row.device && b.live)
                            {
                                let _ = events.send(ServiceEvent::Device {
                                    name: row.name.clone(),
                                    short: row.device.chars().take(8).collect(),
                                    added: true,
                                });
                            }
                        }
                        for row in &before {
                            if row.live
                                && row.device != self_id
                                && !after.iter().any(|a| a.device == row.device && a.live)
                            {
                                let _ = events.send(ServiceEvent::Device {
                                    name: row.name.clone(),
                                    short: row.device.chars().take(8).collect(),
                                    added: false,
                                });
                            }
                        }
                        let was_member = before.iter().any(|b| b.device == self_id && b.live);
                        let still_live = after.iter().any(|a| a.device == self_id && a.live);
                        if was_member && !still_live {
                            // THIS device was removed: the service stops by
                            // itself — the new keys will not arrive
                            if trace() {
                                eprintln!("accept: THIS device was removed — stopping");
                            }
                            let _ = events.send(ServiceEvent::RemovedSelf);
                            stop.cancel();
                        }
                    }
                });
            }
        });
    }
    stop.cancelled().await;
}

/// What changed in the list, as an outbound session may have brought it:
/// only the one thing the INITIATOR must notice — its own removal (the
/// adds and the other removals are the accept loop's before/after).
fn check_self_removed(root: &Path, events: &EventTx, stop: &CancellationToken) {
    let rows = match member_rows(root) {
        Ok(rows) => rows,
        Err(_) => return,
    };
    let self_id = serve::self_device(root);
    if rows.is_empty() || rows.iter().any(|r| r.device == self_id && r.live) {
        return;
    }
    let _ = events.send(ServiceEvent::RemovedSelf);
    stop.cancel();
}

// ---------------------------------------------------------------------------
// the pairing driver (its own thread, its own runtime)
// ---------------------------------------------------------------------------

/// Which pairing road a handle drives.
#[derive(PartialEq, Eq)]
enum PairingKind {
    /// Invite with a ticket.
    Invite,
    /// Invite with the short code.
    Code,
    /// Enter with a ticket or a code; `name` is THIS device's name.
    Join { text: String, name: String },
}

fn spawn_pairing(
    root: &Path,
    kind: PairingKind,
    flag: Option<RelayChoice>,
    events: &EventTx,
    core: &Arc<std::sync::Mutex<WorkerCore>>,
) -> SResult<PairingHandle> {
    // a first refusal that needs no thread at all
    if kind == PairingKind::Code
        && crate::mailbox_net::node_of(root)
            .map_err(ServiceError::io)?
            .is_none()
    {
        return Err(ServiceError::NoMailbox);
    }
    let (answer_tx, answer_rx) = tokio::sync::mpsc::unbounded_channel::<Answer>();
    let handle = PairingHandle::from_parts(answer_tx, answer_rx);
    let inner = Arc::clone(&handle.inner);
    let tracked = Arc::clone(&handle.inner);
    let root = root.to_path_buf();
    let events = events.clone();
    let spawn_result = std::thread::Builder::new()
        .name("engramma-pairing".into())
        .spawn(move || {
            // v0.6.3 (C5.5): ONE thread. The SAS round AWAITS its answer
            // (a tokio channel, a timeout future) instead of blocking the
            // worker, so the endpoint's wire keeps moving while the human
            // looks at the screen — the two-worker runtime was the v0.6.2
            // workaround for the closure that parked a whole worker
            // (S1's found-by-measure defect: B's PAKE completed while A's
            // never did, the moment the closures started blocking).
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    inner.set_state(PairingState::Done(Err(ServiceError::io(e))));
                    return;
                }
            };
            rt.block_on(async move {
                let out = drive_pairing(&inner, &root, &kind, flag, &events).await;
                inner.set_state(PairingState::Done(out));
            });
        });
    match spawn_result {
        Ok(join) => {
            // v0.6.8 (H5): the thread is LIVE until the service closes it
            // — the shutdown cancels and waits for it, so a dropped
            // service leaves nothing that writes into the root
            if let Ok(mut guard) = core.lock() {
                guard.pairings.retain(|p| !p.join.is_finished());
                guard.pairings.push(PairingThread {
                    inner: tracked,
                    join,
                });
            }
            Ok(handle)
        }
        Err(e) => Err(ServiceError::io(e)),
    }
}

/// Drive one pairing road to its end, mapping every failure to its code.
async fn drive_pairing(
    inner: &Arc<PairingInner>,
    root: &Path,
    kind: &PairingKind,
    flag: Option<RelayChoice>,
    events: &EventTx,
) -> SResult<PairingView> {
    let choice = relay::resolve(root, flag.as_ref()).map_err(ServiceError::io)?;
    let dk = DeviceKey::load_or_create(root).map_err(ServiceError::io)?;
    let reg = DeviceRegistry::open(root).map_err(ServiceError::io)?;
    let result: std::result::Result<JoinReport, Error> = match kind {
        PairingKind::Invite => {
            let endpoint = pairing_net::bind_pairing(&dk, "0.0.0.0:0", &choice)
                .await
                .map_err(ServiceError::io)?;
            let minted = pairing_net::ticket_of(&endpoint, &choice)
                .await
                .map_err(ServiceError::io)?;
            inner.set_info(PairingInfo {
                ticket: Some(minted.ticket.encode()),
                ticket_qr: Some(ticket_qr_form(&minted.ticket)),
                code: None,
                expires_at: rfc3339_of(minted.ticket.expires_at),
                expires_ms: minted.ticket.expires_at,
                relay: minted.relay.clone(),
                relay_warning: minted.relay_timed_out.then(|| {
                    "the relay did not connect: the ticket goes out without a relay".to_string()
                }),
                node: Some(hex(serve::node_id_of(&endpoint).as_slice())),
                addrs: serve::direct_addresses(&endpoint),
            });
            let out = race_cancel(
                inner,
                pairing_net::invite(&endpoint, &reg, root, &minted.ticket, inner.as_ref()),
            )
            .await;
            // cancel closes the endpoint itself: the peer must hear the
            // connection end NOW, not at the idle timeout
            let _ = endpoint.close().await;
            out
        }
        PairingKind::Join { text, name } => {
            if text.starts_with("engramma-invite-v1") {
                let ticket = Ticket::decode(text)
                    .map_err(|e| ServiceError::Invalid(format!("invalid ticket: {e}")))?;
                let expires = ticket.expires_at;
                inner.set_info(PairingInfo {
                    ticket: Some(text.clone()),
                    ticket_qr: Some(ticket_qr_form(&ticket)),
                    code: None,
                    expires_at: rfc3339_of(expires),
                    expires_ms: expires,
                    relay: (!ticket.relay.is_empty()).then(|| ticket.relay.clone()),
                    relay_warning: None,
                    node: None,
                    addrs: ticket.addrs.clone(),
                });
                let endpoint = pairing_net::bind_pairing(&dk, "0.0.0.0:0", &choice)
                    .await
                    .map_err(ServiceError::io)?;
                let out = race_cancel(
                    inner,
                    pairing_net::join(&endpoint, &ticket, name, root, inner.as_ref()),
                )
                .await;
                let _ = endpoint.close().await;
                out
            } else {
                join_with_code(inner, root, &dk, &choice, text, name).await
            }
        }
        PairingKind::Code => code_invite(inner, root, &dk, &reg, &choice).await,
    };
    match result {
        Ok(report) => {
            let view = PairingView {
                peer: report.peer.as_ref().map(|(name, device)| DeviceBrief {
                    name: name.clone(),
                    short_id: device.chars().take(8).collect(),
                }),
                cells: report
                    .cells
                    .iter()
                    .map(|(name, ops)| CellArrivedView {
                        name: name.clone(),
                        ops: *ops,
                    })
                    .collect(),
                registry_rows: report.registry_rows,
            };
            if let Some((name, device)) = &report.peer {
                let _ = events.send(ServiceEvent::Device {
                    name: name.clone(),
                    short: device.chars().take(8).collect(),
                    added: true,
                });
            }
            Ok(view)
        }
        Err(e) => Err(ServiceError::from_pairing(inner, &e)),
    }
}

/// Race a flow against `cancel()`: the future drops at the cancellation
/// point (the endpoint closes, the peer hears a refusal); the SAS closure
/// inside unblocks through its own channel.
async fn race_cancel<F: std::future::Future<Output = Result<JoinReport>>>(
    inner: &Arc<PairingInner>,
    flow: F,
) -> Result<JoinReport> {
    tokio::select! {
        _ = cancel_watcher(inner) => Err(Error::Malformed("cancelled".into())),
        out = flow => out,
    }
}

async fn cancel_watcher(inner: &Arc<PairingInner>) {
    loop {
        if inner.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The code window's end: ten minutes from now, like a ticket's.
fn window_end() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
        + pairing::TICKET_TTL_MS
}

fn rfc3339_of(unix_ms: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_ms) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .replace_millisecond(0)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The short code's owner: the meeting at the blind node, the FIRST SAS,
/// the sealed ticket — then the ticket's own flow with its SECOND SAS.
async fn code_invite(
    inner: &Arc<PairingInner>,
    root: &Path,
    dk: &DeviceKey,
    reg: &DeviceRegistry,
    choice: &RelayChoice,
) -> Result<JoinReport> {
    let Some(node) = crate::mailbox_net::node_of(root)? else {
        return Err(Error::Malformed("no mailbox".into()));
    };
    let code = pairing::ShortCode::generate()?;
    inner.set_info(PairingInfo {
        ticket: None,
        ticket_qr: None,
        code: Some(code.encode()),
        expires_at: rfc3339_of(window_end()),
        expires_ms: window_end(),
        relay: None,
        relay_warning: None,
        node: None,
        addrs: Vec::new(),
    });
    let endpoint = serve::bind(dk, "0.0.0.0:0", choice).await?;
    let relay = crate::mailbox_net::meet_code(&endpoint, &node, code.channel).await?;
    // SPAKE2 over the code + the RFC 9382 confirmation: a wrong code dies
    // here and burns the channel
    let pake = crate::mailbox_net::code_pake(&relay, Side::Inviter, &code.password()).await;
    let (shared, transcript, sas) = match pake {
        Ok(x) => x,
        Err(e) => {
            relay.burn(code.channel).await.ok();
            return Err(e);
        }
    };
    if !inner.confirm_round(&sas).await {
        relay.burn(code.channel).await.ok();
        return Err(Error::SasRefused("inviter"));
    }
    // the sealed confirmation, then the fresh ticket
    let rkey = crate::mailbox_net::relay_key(&shared, &transcript);
    let yes = crate::mailbox_net::relay_seal(&rkey, b"engramma/pair/yes")?;
    relay.send(&yes).await?;
    let their_blob = relay.recv().await?;
    crate::mailbox_net::relay_open(&rkey, &their_blob)?;
    // the pairing endpoint and its fresh ticket
    let pair_endpoint = pairing_net::bind_pairing(dk, "0.0.0.0:0", choice).await?;
    let minted = pairing_net::ticket_of(&pair_endpoint, choice).await?;
    let blob = crate::mailbox_net::relay_seal(&rkey, minted.ticket.encode().as_bytes())?;
    relay.send(&blob).await?;
    // the ticket is handed over: the channel burns
    relay.burn(code.channel).await.ok();
    // and the invite flow runs on the pairing endpoint (its own SAS round)
    pairing_net::invite(&pair_endpoint, reg, root, &minted.ticket, inner.as_ref()).await
}

/// The code way in: the meeting, the FIRST SAS, the sealed ticket — then
/// the ticket flow with its SECOND SAS.
async fn join_with_code(
    inner: &Arc<PairingInner>,
    root: &Path,
    dk: &DeviceKey,
    choice: &RelayChoice,
    code_text: &str,
    name: &str,
) -> Result<JoinReport> {
    let Some(node) = crate::mailbox_net::node_of(root)? else {
        return Err(Error::Malformed("no mailbox".into()));
    };
    let code = pairing::ShortCode::decode(code_text)?;
    inner.set_info(PairingInfo {
        ticket: None,
        ticket_qr: None,
        code: Some(code.encode()),
        expires_at: rfc3339_of(window_end()),
        expires_ms: window_end(),
        relay: None,
        relay_warning: None,
        node: None,
        addrs: Vec::new(),
    });
    let endpoint = serve::bind(dk, "0.0.0.0:0", choice).await?;
    let relay = crate::mailbox_net::meet_code(&endpoint, &node, code.channel).await?;
    let pake = crate::mailbox_net::code_pake(&relay, Side::Joiner, &code.password()).await;
    let (shared, transcript, sas) = match pake {
        Ok(x) => x,
        Err(e) => {
            relay.burn(code.channel).await.ok();
            return Err(e);
        }
    };
    if !inner.confirm_round(&sas).await {
        relay.burn(code.channel).await.ok();
        return Err(Error::SasRefused("joiner"));
    }
    let rkey = crate::mailbox_net::relay_key(&shared, &transcript);
    let their_blob = relay.recv().await?;
    crate::mailbox_net::relay_open(&rkey, &their_blob)?;
    let yes = crate::mailbox_net::relay_seal(&rkey, b"engramma/pair/yes")?;
    relay.send(&yes).await?;
    let blob = relay.recv().await?;
    let ticket_bytes = crate::mailbox_net::relay_open(&rkey, &blob)?;
    let ticket_text = String::from_utf8(ticket_bytes)
        .map_err(|e| Error::Malformed(format!("bad ticket bytes: {e}")))?;
    let ticket = pairing::Ticket::decode(&ticket_text)?;
    // ONE attempt per code: the channel that carried the ticket burns
    relay.burn(code.channel).await.ok();
    // the direct ticket flow (its own SAS round)
    pairing_net::join(&endpoint, &ticket, name, root, inner.as_ref()).await
}

#[cfg(test)]
mod sas_tests {
    use super::*;

    /// v0.6.19: a refused SAS is known by its variant, never by its
    /// words. The service used to look for «rifiutata» in the message: a
    /// message in another language would have turned a refusal into an
    /// internal error.
    #[test]
    fn a_refused_sas_is_known_by_its_variant_not_by_its_words() {
        let (answers, answers_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = PairingHandle::from_parts(answers, answers_rx);
        let inner = &handle.inner;
        for side in ["inviter", "joiner"] {
            assert!(
                matches!(
                    ServiceError::from_pairing(inner, &Error::SasRefused(side)),
                    ServiceError::PairingRejected
                ),
                "a refusal by the {side} is PAIRING_REJECTED"
            );
        }
        // the old spelling, alone, convinces nobody
        let spelled = Error::Malformed(" SAS rifiutata da chi invita".into());
        assert!(
            matches!(
                ServiceError::from_pairing(inner, &spelled),
                ServiceError::Io(_)
            ),
            "no message is read for its words"
        );
        // and the other families keep their codes
        let late = Error::Malformed("the invite timed out (ten minutes)".into());
        assert!(matches!(
            ServiceError::from_pairing(inner, &late),
            ServiceError::PairingExpired
        ));
        assert!(matches!(
            ServiceError::from_pairing(inner, &Error::BadKey),
            ServiceError::Io(_)
        ));
    }
}
