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
//!   answers from a button;
//! - the one pass (`run_once`) and the continuous service (`serve_start` /
//!   `serve_stop`): the gate at acceptance, the `Write`/`Others`/`Clock`
//!   ticks, the pass worker, the per-cell write guard;
//! - sharing, unsharing, the device list, the removal with its rotation
//!   (and its courtesy call: the removed device hears it from the remover,
//!   stops its own service and never waits for keys that will not come);
//! - the events (passes, arrivals, devices, the self-removal, the unsent)
//!   out of one channel: the CLI ignores them, the binding hands them to
//!   JS.
//!
//! ONE `serve` per root: an exclusive OS lock (`<root>/sync.lock`, flock)
//! covers the continuous service and each single pass — a second service
//! on the same root, in this or in ANOTHER process, answers `SYNC_BUSY`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::relay::RelayChoice;
use crate::serve::{CellSummary, PassReport, PeerOutcome};

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
    /// Everything else (a broken root, a failed endpoint).
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
            Self::Io(_) => "IO",
        }
    }

    /// One line, no secrets, no note text.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Busy => "un altro servizio di sincronizzazione è già \
                           attivo su questa radice (sync.lock)"
                .into(),
            Self::PairingRejected => "codice di verifica rifiutato".into(),
            Self::PairingTimeout => "nessuna risposta al codice di verifica in tempo".into(),
            Self::PairingExpired => "il biglietto è scaduto".into(),
            Self::NoMailbox => "nessuna casella postale configurata: \
                                `engramma mailbox add` prima"
                .into(),
            Self::NotShared => "la cella non è condivisa da questa radice".into(),
            Self::LastDevice => "l'ultimo dispositivo non si rimuove".into(),
            Self::Cancelled => "abbinamento annullato".into(),
            Self::NotFound(what) => format!("{what} non esiste"),
            Self::Invalid(why) => why.clone(),
            Self::Io(why) => why.clone(),
        }
    }

    fn io(e: impl std::fmt::Display) -> Self {
        Self::Io(e.to_string())
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for ServiceError {}

/// Convenience alias.
pub type SResult<T> = Result<T, ServiceError>;

// ---------------------------------------------------------------------------
// the views
// ---------------------------------------------------------------------------

/// One device of the list, as `devices` answers it.
#[derive(Debug, Clone)]
pub struct DeviceView {
    /// The name its owner chose.
    pub name: String,
    /// The first 8 characters of the device id.
    pub short_id: String,
    /// Is it THIS device?
    pub is_self: bool,
    /// The last session the address book saw (RFC 3339).
    pub last_contact: Option<String>,
    /// What the book holds: `"diretta"`, `"relay"` or nothing yet.
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
    /// The relay the root chose (`None` with `--no-relay`, before a choice).
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
    /// When the ticket (the window) ends, RFC 3339.
    pub expires_at: String,
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
        /// `true` when it entered.
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
pub struct PairingHandle {
    pub(crate) inner: Arc<PairingInner>,
}

/// The handle's shared state (see the driver below).
pub(crate) struct PairingInner {
    pub(crate) root: std::path::PathBuf,
    pub(crate) info: std::sync::Mutex<Option<PairingInfo>>,
    pub(crate) info_cv: std::sync::Condvar,
    pub(crate) state: std::sync::Mutex<PairingState>,
    pub(crate) state_cv: std::sync::Condvar,
    /// The answers to the SAS rounds; the DRIVER owns the receiving end
    /// (a `Receiver` is not `Sync`, and the handle crosses threads).
    pub(crate) answers: std::sync::mpsc::Sender<Answer>,
    pub(crate) cancelled: std::sync::atomic::AtomicBool,
}

/// The human's (or the test's) answer to one SAS round, or the service's
/// own timeout/cancel word.
pub(crate) enum Answer {
    /// Confirmed (or refused) by the caller.
    Human(bool),
    /// `cancel()` was called.
    Cancelled,
}

/// Where one pairing stands.
#[derive(Debug)]
pub(crate) enum PairingState {
    /// The SAS of the next round is not out yet.
    Waiting,
    /// The six digits of round `n` are on the screen.
    SasReady(String, u32),
    /// Round `n` was answered; the flow goes on.
    Answered(u32),
    /// The flow ended.
    Done(SResult<PairingView>),
}

impl PairingHandle {
    /// The ticket (both forms), the code, the relay: ready when the
    /// inviter's endpoint is (the relay wait included). `deadline` bounds
    /// the wait; join answers at once.
    pub fn info(&self, deadline: Duration) -> SResult<PairingInfo> {
        let _guard = self
            .inner
            .state_cv
            .wait_timeout_while(
                self.inner.info.lock().map_err(|_| ServiceError::io("lock"))?,
                deadline,
                |info| info.is_none(),
            )
            .map_err(|_| ServiceError::io("lock"))?
            .0;
        self.inner
            .info
            .lock()
            .map_err(|_| ServiceError::io("lock"))?
            .clone()
            .ok_or_else(|| ServiceError::PairingExpired)
    }

    /// The six digits of the next round (there may be two with the short
    /// code: ask again after each `confirm`).
    pub fn sas(&self, deadline: Duration) -> SResult<String> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ServiceError::io("lock"))?;
        let deadline_instant = std::time::Instant::now() + deadline;
        loop {
            match &*state {
                PairingState::SasReady(digits, _) => return Ok(digits.clone()),
                PairingState::Done(Err(e)) => return Err(e.clone()),
                PairingState::Done(Ok(_)) => {
                    return Err(ServiceError::Invalid(
                        "l'abbinamento è già finito: niente altre cifre".into(),
                    ));
                }
                _ => {}
            }
            let now = std::time::Instant::now();
            if now >= deadline_instant {
                return Err(ServiceError::PairingTimeout);
            }
            let (guard, _left) = self
                .inner
                .state_cv
                .wait_timeout(state, deadline_instant - now)
                .map_err(|_| ServiceError::io("lock"))?;
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
            .map_err(|_| ServiceError::io("lock"))?;
        match &*state {
            PairingState::SasReady(_, round) => {
                let round = *round;
                *state = PairingState::Answered(round);
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
                "l'abbinamento è già finito: niente da confermare".into(),
            )),
            _ => Err(ServiceError::Invalid(
                "confirm prima delle cifre: `sas()` prima, il confronto non si salta".into(),
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
            .map_err(|_| ServiceError::io("lock"))?;
        let deadline_instant = std::time::Instant::now() + deadline;
        loop {
            if let PairingState::Done(out) = &*state {
                return out.clone();
            }
            let now = std::time::Instant::now();
            if now >= deadline_instant {
                return Err(ServiceError::PairingTimeout);
            }
            let (guard, _left) = self
                .inner
                .state_cv
                .wait_timeout(state, deadline_instant - now)
                .map_err(|_| ServiceError::io("lock"))?;
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
    let _ = root;
    let _ = LOCK_NAME;
    Err(ServiceError::Io("not yet: v0.6.2 S1".into()))
}

// ---------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------

/// The sync service of one data root: pairing, passes, the continuous
/// loop, the list, the sharing — on its own thread. Clone the `Arc` and
/// call from anywhere; every call answers within its own bound.
pub struct SyncService {
    pub(crate) inner: Arc<ServiceInner>,
}

/// The service's shared half (the worker thread holds the other).
pub(crate) struct ServiceInner {
    /// The data root.
    pub root: std::path::PathBuf,
    /// Commands to the worker.
    pub cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    /// The events, once (`take_events`).
    pub events: std::sync::Mutex<Option<std::sync::mpsc::Receiver<ServiceEvent>>>,
    /// The worker's lifetime.
    pub join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// One command to the worker (each carries its own reply channel).
pub(crate) enum Command {
    Status { reply: std::sync::mpsc::Sender<SResult<StatusView>> },
    Devices { reply: std::sync::mpsc::Sender<SResult<Vec<DeviceView>>> },
    Invite { name: String, flag: Option<RelayChoice>, reply: std::sync::mpsc::Sender<SResult<PairingHandle>> },
    Code { name: String, flag: Option<RelayChoice>, reply: std::sync::mpsc::Sender<SResult<PairingHandle>> },
    Join { text: String, name: String, flag: Option<RelayChoice>, reply: std::sync::mpsc::Sender<SResult<PairingHandle>> },
    Remove { name: String, reply: std::sync::mpsc::Sender<SResult<RemoveView>> },
    Share { cell: String, reply: std::sync::mpsc::Sender<SResult<ShareView>> },
    Unshare { cell: String, reply: std::sync::mpsc::Sender<SResult<()>> },
    RunOnce { flag: Option<RelayChoice>, reply: std::sync::mpsc::Sender<SResult<PassReport>> },
    ServeStart { flag: Option<RelayChoice>, reply: std::sync::mpsc::Sender<SResult<ServeInfo>> },
    ServeStop { reply: std::sync::mpsc::Sender<SResult<()>> },
    Shutdown { reply: std::sync::mpsc::Sender<SResult<()>> },
}

impl SyncService {
    /// Start the service of a root (its worker thread, its runtime). One
    /// per root in a process; a second one on the same root answers
    /// `SYNC_BUSY` at the first `serve`/`run`, not here.
    ///
    /// # Errors
    /// [`ServiceError::Io`] when the root cannot be opened.
    pub fn start(root: &Path) -> SResult<Arc<Self>> {
        let _ = root;
        Err(ServiceError::Io("not yet: v0.6.2 S1".into()))
    }

    /// The events of this service, the first caller only.
    #[must_use]
    pub fn take_events(&self) -> Option<std::sync::mpsc::Receiver<ServiceEvent>> {
        self.inner
            .events
            .lock()
            .ok()
            .and_then(|mut e| e.take())
    }

    fn call<T: Send + 'static>(
        &self,
        make: impl FnOnce(std::sync::mpsc::Sender<SResult<T>>) -> Command,
        bound: Duration,
    ) -> SResult<T> {
        let _ = (make, bound);
        Err(ServiceError::Io("not yet: v0.6.2 S1".into()))
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
            |reply| Command::Invite { name: name.to_string(), flag, reply },
            Duration::from_secs(60),
        )
    }

    /// Invite with the short code (the blind mailbox must be configured:
    /// `NO_MAILBOX` otherwise).
    pub fn code(&self, name: &str, flag: Option<RelayChoice>) -> SResult<PairingHandle> {
        self.call(
            |reply| Command::Code { name: name.to_string(), flag, reply },
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
                text: ticket_or_code.to_string(),
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
            |reply| Command::Remove { name: name.to_string(), reply },
            Duration::from_secs(60),
        )
    }

    /// Share a cell (the adopt, epoch 0, the wrapped keys).
    pub fn share(&self, cell: &str) -> SResult<ShareView> {
        self.call(
            |reply| Command::Share { cell: cell.to_string(), reply },
            Duration::from_secs(30),
        )
    }

    /// Stop sharing a cell from here (`NOT_SHARED` when it is not).
    pub fn unshare(&self, cell: &str) -> SResult<()> {
        self.call(
            |reply| Command::Unshare { cell: cell.to_string(), reply },
            Duration::from_secs(30),
        )
    }

    /// One pass with every reachable device and the blind node. With the
    /// continuous service on, the pass runs through the same cycle (one
    /// pass at a time, always).
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
        self.call(|reply| Command::ServeStop { reply }, Duration::from_secs(30))
    }

    /// The end of the service: the continuous loop stops, the worker
    /// thread ends. `Engine.close` calls this; the calls after it answer
    /// `IO`.
    pub fn shutdown(&self) {
        let _ = self.call(|reply| Command::Shutdown { reply }, Duration::from_secs(20));
    }
}
