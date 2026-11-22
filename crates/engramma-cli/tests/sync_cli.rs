// SPDX-License-Identifier: Apache-2.0

//! The pairing over the REAL CLI (v0.6.1, P2): two roots, one invite that
//! waits, one join that enters, the SAS confirmed (or refused) on both
//! sides — and the shared cell that arrives whole. Plus the refusals: the
//! expired ticket, the reused ticket, the man in the middle with someone
//! else's ticket, and the code way in with its burnt channel (P4).

mod common;

use std::io::Write;
use std::process::{Child, Command, Stdio};

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};

fn note(n: u16, body: &str) -> Note {
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* fixed */);
    Note {
        id: ulid::Ulid::from_string(&format!("01F7ZQ6VTSATW2BQ5Z9R0P{n:04}")).unwrap(/* t */),
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

struct Root {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Root {
    fn new(tag: &str) -> Self {
        let dir = common::test_dir();
        let root = dir.path().join(tag);
        std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
        run(&root, &["init"], b"");
        Self { _dir: dir, root }
    }

    fn cell(&self, name: &str) -> Cell {
        Cell::open(&self.root.join("cells").join(format!("{name}.sqlite"))).unwrap(/* t */)
    }
}

fn run(root: &std::path::Path, args: &[&str], stdin: &[u8]) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engramma"));
    cmd.arg("--root")
        .arg(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !stdin.is_empty() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().unwrap(/* t */);
    if !stdin.is_empty() {
        child
            .stdin
            .as_mut()
            .expect(/* piped */ "stdin")
            .write_all(stdin)
            .expect(/* pipe */ "stdin write");
    }
    let out = wait_bounded(child, &args.join(" "), 60);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Wait for a child WITH a deadline (R5): a hung command fails the test
/// with a clear line instead of hanging the run.
fn wait_bounded(mut child: Child, name: &str, secs: u64) -> std::process::Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match child.try_wait().expect(/* child */ "wait") {
            Some(_) => return child.wait_with_output().expect(/* t */ "the output"),
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("«{name}» non è finito in {secs} s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// Spawn a command whose stdin we feed and whose stdout we read while it
/// runs (the invite waits for the joiner).
struct Running {
    child: Option<Child>,
}

impl Running {
    /// Spawn with stdin fed and stdout watched: the ticket line (if any)
    /// arrives on the channel while the command runs.
    fn spawn(
        root: &std::path::Path,
        args: &[&str],
        stdin: &[u8],
    ) -> (Self, std::sync::mpsc::Receiver<String>) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_engramma"));
        cmd.arg("--root").arg(root).args(args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect(/* bin */ "spawn");
        child
            .stdin
            .as_mut()
            .expect(/* piped */ "stdin")
            .write_all(stdin)
            .expect(/* pipe */ "stdin write");
        let mut child = Some(child);
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(out) = child.as_mut().expect(/* alive */ "spawn").stdout.take() {
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(out).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if line.starts_with("engramma-invite-v1.") {
                        tx.send(line).ok();
                    }
                }
            });
        }
        (Self { child }, rx)
    }

    fn wait(mut self) -> (bool, String, String) {
        let out = wait_bounded(
            self.child.take().expect(/* alive */ "running"),
            "running",
            120,
        );
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }
}

/// The whole invite→join→cell-arrives flow, on the real CLI, on loopback.
#[test]
fn invite_and_join_share_the_cells() {
    let a = Root::new("root-a");
    let b = Root::new("root-b");
    // A has a cell with notes, shared
    let cell = Cell::create(
        &a.root.join("cells").join("memoria.sqlite"),
        "memoria",
        "cli",
    )
    .unwrap(/* t */);
    for i in 1..=5_u16 {
        cell.put_note(&note(i, &format!("corpo della nota {i}")), "cli")
            .unwrap(/* t */);
    }
    drop(cell);
    let (ok, _, err) = run(&a.root, &["sync", "share", "memoria"], b"");
    assert!(ok, "sync share: {err}");
    // A invites (waits); B joins with the ticket; both confirm the SAS
    let (inviter, tickets) = Running::spawn(&a.root, &["devices", "invite", "--no-relay"], b"s\n");
    let ticket = tickets
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect(/* printed */ "the invite printed its ticket");
    let (ok, out, err) = run(
        &b.root,
        &["devices", "join", "--no-relay", &ticket, "--name", "mac"],
        b"s\n",
    );
    assert!(ok, "join: {err} {out}");
    let (ok, out, err) = inviter.wait();
    assert!(ok, "invite: {err} {out}");
    // the cell arrived whole on B: same replicated digest, verify green
    let ca = a.cell("memoria");
    let cb = b.cell("memoria");
    assert_eq!(
        ca.replicated_digest().unwrap(/* t */),
        cb.replicated_digest().unwrap(/* t */),
        "the two devices hold the same replicated state"
    );
    assert!(cb.verify().unwrap(/* t */).ok());
    // and B is in A's list
    let (ok, out, _) = run(&a.root, &["--json", "devices", "list"], b"");
    assert!(ok);
    assert!(out.contains("mac"), "the joiner is in the list: {out}");
}

/// A REFUSED SAS on one side leaves no add anywhere.
#[test]
fn a_refused_sas_leaves_no_add() {
    let a = Root::new("root-c");
    let b = Root::new("root-d");
    let (inviter, tickets) = Running::spawn(&a.root, &["devices", "invite", "--no-relay"], b"n\n");
    let ticket = tickets
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect(/* printed */ "the invite printed its ticket");
    let (ok, _, err) = run(
        &b.root,
        &["devices", "join", "--no-relay", &ticket, "--name", "mac"],
        b"s\n",
    );
    // the joiner fails too: the inviter said no and closed
    assert!(!ok, "a refused SAS must fail the join: {err}");
    let (ok, _, _) = inviter.wait();
    assert!(!ok, "the inviter fails as well");
    // no add on either side
    let (ok, out, _) = run(&a.root, &["--json", "devices", "list"], b"");
    assert!(ok);
    assert!(!out.contains("mac"), "no add landed: {out}");
    let (ok, out, _) = run(&b.root, &["--json", "devices", "list"], b"");
    assert!(ok);
    assert!(!out.contains("portatile"), "B learned nothing: {out}");
}

/// An EXPIRED ticket is refused at the door.
#[test]
fn an_expired_ticket_is_refused() {
    let b = Root::new("root-e");
    // a ticket that expired a minute ago, pointing anywhere
    let past = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        - 60_000;
    let ticket = engramma_sync::pairing::Ticket {
        node_id: [1; 32],
        relay: String::new(),
        addrs: vec!["127.0.0.1:1".into()],
        secret: [2; 16],
        expires_at: past,
    };
    let (ok, _, err) = run(
        &b.root,
        &[
            "devices",
            "join",
            "--no-relay",
            &ticket.encode(),
            "--name",
            "mac",
        ],
        b"s\n",
    );
    assert!(!ok, "an expired ticket must be refused: {err}");
}

/// A REUSED ticket finds nobody listening: the invite was one use, the
/// inviter is gone.
#[test]
fn a_reused_ticket_finds_nobody() {
    let a = Root::new("root-f");
    let b = Root::new("root-g");
    let (inviter, tickets) = Running::spawn(&a.root, &["devices", "invite", "--no-relay"], b"s\n");
    let ticket = tickets
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect(/* printed */ "the invite printed its ticket");
    let (ok, _, err) = run(
        &b.root,
        &["devices", "join", "--no-relay", &ticket, "--name", "mac"],
        b"s\n",
    );
    assert!(ok, "the first join works: {err}");
    let (ok, _, _) = inviter.wait();
    assert!(ok);
    // the second use: the inviter exited, nobody listens at that address
    let (ok, _, err) = run(
        &b.root,
        &["devices", "join", "--no-relay", &ticket, "--name", "mac"],
        b"s\n",
    );
    assert!(!ok, "a reused ticket must fail: {err}");
}

/// A MAN IN THE MIDDLE with someone else's ticket: the inviter's MAC over
/// the ticket secret does not verify — the join dies before any exchange.
#[test]
fn a_stolen_ticket_fails_the_mac() {
    let a = Root::new("root-h");
    let other = Root::new("root-i");
    let b = Root::new("root-l");
    // a real inviter (never joined: nobody has its secret)
    let (inviter, tickets) = Running::spawn(&a.root, &["devices", "invite", "--no-relay"], b"s\n");
    let ticket_text = tickets
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect(/* printed */ "the invite printed its ticket");
    let mut ticket = engramma_sync::pairing::Ticket::decode(&ticket_text)
        .expect(/* just printed */ "decode");
    // the man in the middle substitutes ITS OWN inviter (the address of
    // `other`'s pairing endpoint), keeping the victim's secret — the MITM
    // does not know it, so the inviter-side MAC cannot verify
    let (other_inviter, other_tickets) =
        Running::spawn(&other.root, &["devices", "invite", "--no-relay"], b"s\n");
    let other_ticket = engramma_sync::pairing::Ticket::decode(
        &other_tickets
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect(/* printed */ "the other invite printed its ticket"),
    )
    .expect(/* just printed */ "decode");
    ticket.node_id = other_ticket.node_id;
    ticket.addrs = other_ticket.addrs;
    let (ok, _, err) = run(
        &b.root,
        &[
            "devices",
            "join",
            "--no-relay",
            &ticket.encode(),
            "--name",
            "mac",
        ],
        b"s\n",
    );
    assert!(!ok, "a stolen ticket must fail the MAC: {err}");

    // the victim's invite never sees a joiner: kill it (it would wait its
    // ten minutes); the other side failed its own key confirmation
    let mut victim = inviter;
    if let Some(mut child) = victim.child.take() {
        kill(&mut child);
    }
    let (ok, _, _) = other_inviter.wait();
    assert!(!ok, "the other inviter fails its own confirmation");
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
