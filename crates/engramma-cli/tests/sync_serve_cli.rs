// SPDX-License-Identifier: Apache-2.0

//! The service and the blind node over the REAL CLI (v0.6.1, P3–P5):
//! `sync serve` beside a second writer on the same root (the Homun
//! convivenza), the one-pass `sync run`, the mailbox node between two
//! devices that never meet, three devices with concurrent writes and a
//! device that comes back, the removal with its rotation.

mod common;

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_engramma"))
}

/// Run a test body with a DEADLINE (R5): a hung network dance fails the
/// test with a clear line instead of hanging the whole run.
fn bounded<F>(name: &str, secs: u64, body: F)
where
    F: FnOnce() + Send + 'static,
{
    let name = name.to_string();
    let handle = std::thread::spawn(body);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
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

/// A child process that DIES with the test, even on a panic (R5): a leaked
/// `mailbox serve` once lived three hours and a half.
struct ChildGuard {
    child: Option<Child>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Wait for a child WITH a deadline: the child is consumed and its output
/// returned; a hung child is killed and the test fails with a clear line.
fn wait_bounded(mut child: Child, name: &str, secs: u64) -> std::process::Output {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match child.try_wait().expect(/* child */ "wait") {
            Some(_) => return child.wait_with_output().expect(/* t */ "the output"),
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("«{name}» non è finito in {secs} s");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Run to completion feeding stdin first (the SAS confirms).
fn run_with_stdin(root: &std::path::Path, args: &[&str], stdin: &[u8]) -> (bool, String, String) {
    let mut child = bin()
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(/* t */);
    child
        .stdin
        .as_mut()
        .expect(/* piped */ "stdin")
        .write_all(stdin)
        .expect(/* pipe */ "stdin write");
    let out = wait_bounded(child, &args.join(" "), 180);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn run(root: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let child = bin()
        .env("ENGRAMMA_TRACE", "1")
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(/* t */);
    let out = wait_bounded(child, &args.join(" "), 120);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A long-running child whose stdout lines arrive on a channel while it
/// runs (the serve daemon publishes its address as the FIRST json line).
/// The STDERR is drained in its own thread and kept (K3, v0.6.1-ciclo): a
/// piped-but-unread stderr fills its pipe and the daemon blocks — with
/// the trace on, in seconds.
struct Daemon {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
    err: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Daemon {
    fn spawn(root: &std::path::Path, args: &[&str]) -> Self {
        Self::spawn_opt(root, args, false)
    }

    /// The traced way (K1, v0.6.1-ciclo): `ENGRAMMA_TRACE=1`, so the
    /// daemon's own lines name every pass — the drained stderr keeps
    /// them for the test to count.
    fn spawn_traced(root: &std::path::Path, args: &[&str]) -> Self {
        Self::spawn_opt(root, args, true)
    }

    fn spawn_opt(root: &std::path::Path, args: &[&str], trace: bool) -> Self {
        let mut cmd = bin();
        cmd.arg("--root").arg(root).args(args);
        if trace {
            cmd.env("ENGRAMMA_TRACE", "1");
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect(/* bin */ "spawn");
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(out).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        let err = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(se) = child.stderr.take() {
            let sink = std::sync::Arc::clone(&err);
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(se).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    let mut buf = sink.lock().expect(/* test */ "stderr buf");
                    buf.push_str(&line);
                    buf.push('\n');
                }
            });
        }
        Self {
            child,
            lines: rx,
            err,
        }
    }

    /// How many passes of one kind the daemon ran («write» or «clock»):
    /// the trace's own `worker: pass <kind>` lines, one per pass.
    fn passes(&self, kind: &str) -> usize {
        self.err
            .lock()
            .expect(/* test */ "stderr buf")
            .matches(&format!("worker: pass {kind}"))
            .count()
    }

    /// The last lines of the drained stderr (a failure's evidence).
    fn trace_tail(&self) -> String {
        let buf = self.err.lock().expect(/* test */ "stderr buf");
        buf.lines()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The daemon's first JSON object (pretty-printed over several lines,
    /// with NESTED objects inside since C1: the ticket is one — so the
    /// balance is counted, not guessed from a lone `}` line).
    fn first_json(&self) -> serde_json::Value {
        let mut text = String::new();
        let mut depth = 0_i32;
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect(/* prints at start */ "the daemon spoke");
            text.push('\n');
            text.push_str(&line);
            for c in line.chars() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return serde_json::from_str(&text)
                                .expect(/* balanced */ "the daemon's json");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

impl Drop for Daemon {
    /// the daemon dies with the test, even on a panic (R5)
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        run(&root, &["init"]);
        Self { _dir: dir, root }
    }

    fn cell_path(&self, name: &str) -> std::path::PathBuf {
        self.root.join("cells").join(format!("{name}.sqlite"))
    }
}

/// The pairing helper: A invites (waiting), B joins, both SAS confirmed.
fn pair(a: &Root, b: &Root, b_name: &str) {
    // the honest way: spawn with stdin fed, both children guarded (R5:
    // a leaked invite waits its ten minutes, a leaked serve its life)
    let mut child = bin()
        .arg("--root")
        .arg(&a.root)
        .args(["devices", "invite", "--no-relay"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(/* bin */ "spawn invite");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if line.starts_with("engramma-invite-v1.") {
                    tx.send(line).ok();
                }
            }
        });
    }
    let ticket = rx
        .recv_timeout(Duration::from_secs(30))
        .expect(/* printed */ "the ticket");
    // the invite lives until the pairing ends: guarded (R5)
    let mut child = ChildGuard { child: Some(child) };
    let join = bin()
        .arg("--root")
        .arg(&b.root)
        .args(["devices", "join", "--no-relay", &ticket, "--name", b_name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(/* bin */ "spawn join");
    let mut join = ChildGuard { child: Some(join) };
    // feed both confirms
    child
        .child
        .as_mut()
        .expect(/* piped */ "invite stdin")
        .stdin
        .as_mut()
        .expect(/* piped */ "invite stdin")
        .write_all(b"s\n")
        .expect(/* pipe */ "invite confirm");
    let mut join_stdin = join
        .child
        .as_mut()
        .expect(/* alive */ "join")
        .stdin
        .take()
        .expect(/* piped */ "join stdin");
    join_stdin
        .write_all(b"s\n")
        .expect(/* pipe */ "join confirm");
    drop(join_stdin);
    let out = wait_bounded(join.child.take().expect(/* alive */ "join"), "join", 120);
    assert!(
        out.status.success(),
        "join: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let out = wait_bounded(
        child.child.take().expect(/* alive */ "invite"),
        "invite",
        120,
    );
    assert!(
        out.status.success(),
        "invite: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Write the serve daemon's address into a root's address book (the
/// production road is the pairing and every successful session; a test
/// sets it by hand, exactly like the first session would).
fn remember_all(root: &std::path::Path, device_hex: &str, node_hex: &str, addrs: &[String]) {
    let list = addrs
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(",");
    let reg = rusqlite::Connection::open(root.join("devices.db")).unwrap(/* t */);
    reg.execute(
        "CREATE TABLE IF NOT EXISTS device_addrs (device TEXT PRIMARY KEY, addrs TEXT NOT NULL, seen_at TEXT NOT NULL)",
        [],
    )
    .unwrap(/* t */);
    reg.execute(
        "INSERT OR REPLACE INTO device_addrs (device, addrs, seen_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            device_hex,
            format!("{{\"node_id\":\"{node_hex}\",\"relay\":\"\",\"addrs\":[{list}]}}"),
            "test",
        ],
    )
    .unwrap(/* t */);
}

/// P3 + la convivenza: `sync serve` on A receives B's run AND a second
/// process writing on A's own root (the Homun shape); at the end the
/// three copies of the truth agree and verify.
#[test]
fn serve_receives_and_converges_beside_a_second_writer() {
    bounded("serve_beside_writer", 600, || {
        let a = Root::new("srv-a");
        let b = Root::new("srv-b");
        // a cell on A, shared
        let cell = Cell::create(&a.cell_path("memoria"), "memoria", "cli").unwrap(/* t */);
        for i in 1..=3_u16 {
            cell.put_note(&note(i, &format!("corpo {i}")), "cli").unwrap(/* t */);
        }
        drop(cell);
        let (ok, _, err) = run(&a.root, &["sync", "share", "memoria"]);
        assert!(ok, "share: {err}");
        pair(&a, &b, "mac");
        // A serves; its address reaches B's book
        let serve = Daemon::spawn(&a.root, &["--json", "sync", "serve", "--no-relay"]);
        let v = serve.first_json();
        let addrs: Vec<String> = v["addrs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let node = v["node"]
            .as_str()
            .expect(/* printed */ "the node id")
            .to_string();
        // A's device id from its registry (the create row's device)
        let reg = rusqlite::Connection::open(a.root.join("devices.db")).unwrap(/* t */);
        let a_device: String = reg
        .query_row(
            "SELECT json_extract(payload_json, '$.device') FROM registry WHERE kind='create'",
            [],
            |r| r.get(0),
        )
        .unwrap(/* t */);
        remember_all(&b.root, &a_device, &node, &addrs);
        // a second process writes on A's root WHILE serve runs (Homun)
        let (ok, _, err) = run(
            &a.root,
            &["--cell", "memoria", "purge", "0001", "--reason", "oblio"],
        );
        let _ = (ok, err);
        // B's one pass reaches A
        let (ok, out, err) = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        assert!(ok, "run: {err}");
        assert!(out.contains("memoria"), "the summary names the cell: {out}");
        // a second pass settles whatever arrived between the purge and the
        // session (the daemon synced it on its own trigger)
        std::thread::sleep(Duration::from_secs(4));
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        // the three copies of the truth agree: A, B, and A's own verify
        let ca = Cell::open(&a.cell_path("memoria")).unwrap(/* t */);
        let cb = Cell::open(&b.cell_path("memoria")).unwrap(/* t */);
        assert_eq!(
            ca.replicated_digest().unwrap(/* t */),
            cb.replicated_digest().unwrap(/* t */),
            "serve and run converge"
        );
        assert!(ca.verify().unwrap(/* t */).ok());
        assert!(cb.verify().unwrap(/* t */).ok());
        drop(serve);
    });
}

/// P4: A writes and goes away; B converges from the blind node alone; a
/// purge on B reaches the node and no byte of those envelopes survives in
/// the node's file; the node's file never holds note text.
#[test]
fn the_blind_node_carries_and_forgets() {
    bounded("the_blind_node", 600, || {
        let a = Root::new("mbx-a");
        let b = Root::new("mbx-b");
        let node_dir = common::test_dir();
        let node_root = node_dir.path().join("node");
        std::fs::create_dir_all(&node_root).unwrap(/* t */);
        // the cell with a SECRET body (the needle the node must never hold)
        let cell = Cell::create(&a.cell_path("memoria"), "memoria", "cli").unwrap(/* t */);
        let secret = note(7, "il segreto quokkaviola del nodo cieco");
        let secret_id = secret.id;
        for n in [note(5, "nota pubblica"), secret, note(9, "altra nota")] {
            cell.put_note(&n, "cli").unwrap(/* t */);
        }
        drop(cell);
        run(&a.root, &["sync", "share", "memoria"]);
        pair(&a, &b, "mac");
        // the node serves
        let mut node_cmd = bin();
        node_cmd
            .args(["--json", "mailbox", "serve", "--no-relay", "--root"])
            .arg(&node_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = node_cmd.spawn().expect(/* bin */ "node spawn");
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let out = child.stdout.take().expect(/* piped */ "node stdout");
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    tx.send(line).ok();
                }
            });
        }
        // the stderr is drained too (K3): a piped-but-unread pipe fills
        // and the node blocks
        let err_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(se) = child.stderr.take() {
            let sink = std::sync::Arc::clone(&err_buf);
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(se).lines() {
                    let Ok(line) = line else { break };
                    let mut buf = sink.lock().expect(/* test */ "stderr buf");
                    buf.push_str(&line);
                    buf.push('\n');
                }
            });
        }
        // the node dies with the test, even on a panic (R5 — a leaked node
        // once lived three hours and a half)
        let node = Daemon {
            child,
            lines: rx,
            err: err_buf,
        };
        let v = node.first_json();
        // the ticket travels as a nested JSON object (C1): back to the
        // textual form `mailbox add` takes
        let ticket = v["ticket"].to_string();
        // both devices register with the node
        let (ok, _, err) = run(&a.root, &["mailbox", "add", &ticket]);
        assert!(ok, "A registers: {err}");
        let (ok, _, err) = run(&b.root, &["mailbox", "add", &ticket]);
        assert!(ok, "B registers: {err}");
        // A goes away (no serve daemon): B pushes to and pulls from the node
        let (ok, _out, err) = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        assert!(ok, "B's pass (node only): {err}");
        // B purges the secret note, then tells the node at the next pass
        let (ok, _, err) = run(
            &b.root,
            &[
                "--cell",
                "memoria",
                "purge",
                &secret_id.to_string(),
                "--reason",
                "oblio",
            ],
        );
        assert!(ok, "B purges: {err}");
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        // the node's file holds no note text and no op id — ciphertext only
        let bytes = std::fs::read(node_root.join("mailbox.db"))
            .expect(/* the node's store */ "mailbox.db");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("quokkaviola"),
            "the node never saw the secret"
        );
        assert!(!text.contains("nota pubblica"), "no note text at the node");
        drop(node);
    });
}

/// One note written through the real CLI (armed with the device signer).
fn add_note(root: &std::path::Path, who: &str) {
    let file = common::test_dir();
    let path = file.path().join("nota.md");
    std::fs::write(&path, format!("scritta concorrente di {who}")).unwrap(/* t */);
    let (ok, _, err) = run(
        root,
        &[
            "--cell",
            "memoria",
            "add",
            path.to_str().unwrap(/* utf8 */),
            "--source",
            "user",
            "--confidence",
            "stated",
            "--title",
            &format!("nota di {who}"),
        ],
    );
    assert!(ok, "add su {who}: {err}");
}

/// P5: three devices, concurrent writes on all of them, one device that
/// goes away and comes back — convergence at the replicated digest and
/// verify green everywhere.
#[test]
fn three_devices_concurrent_and_one_returns() {
    bounded("three_devices", 900, || {
        let a = Root::new("tri-a");
        let b = Root::new("tri-b");
        let c = Root::new("tri-c");
        let cell = Cell::create(&a.cell_path("memoria"), "memoria", "cli").unwrap(/* t */);
        cell.put_note(&note(1, "la base comune"), "cli").unwrap(/* t */);
        drop(cell);
        run(&a.root, &["sync", "share", "memoria"]);
        pair(&a, &b, "mac");
        pair(&a, &c, "telefono");
        // A serves; B and C know its address and run passes
        let serve = Daemon::spawn(&a.root, &["--json", "sync", "serve", "--no-relay"]);
        let v = serve.first_json();
        let addrs: Vec<String> = v["addrs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let node = v["node"].as_str().expect(/* printed */ "node").to_string();
        let reg = rusqlite::Connection::open(a.root.join("devices.db")).unwrap(/* t */);
        let a_device: String = reg
        .query_row(
            "SELECT json_extract(payload_json, '$.device') FROM registry WHERE kind='create'",
            [],
            |r| r.get(0),
        )
        .unwrap(/* t */);
        for r in [&b.root, &c.root] {
            remember_all(r, &a_device, &node, &addrs);
        }
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        let _ = run(&c.root, &["--json", "sync", "run", "--no-relay"]);
        // concurrent writes on all three, through the CLI: every local write
        // of the sync era carries its device and signature, or the peers'
        // gate refuses it
        for (root, who) in [(&a.root, "A"), (&b.root, "B"), (&c.root, "C")] {
            add_note(root, who);
        }
        // the passes cross: A's daemon syncs on its own writes, B and C run
        std::thread::sleep(Duration::from_secs(4));
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        let _ = run(&c.root, &["--json", "sync", "run", "--no-relay"]);
        std::thread::sleep(Duration::from_secs(4));
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        let _ = run(&c.root, &["--json", "sync", "run", "--no-relay"]);
        // C goes away; A and B keep writing
        for (root, who) in [(&a.root, "A2"), (&b.root, "B2")] {
            add_note(root, who);
        }
        std::thread::sleep(Duration::from_secs(4));
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        // C comes back; the pull-only topology settles in rounds (each run
        // pulls what the peer had, the daemon pushes nothing — rounds until
        // the three digests agree, as many passes as the truth needs)
        let digests: Vec<String>;
        let mut rounds = 0;
        loop {
            let _ = run(&c.root, &["--json", "sync", "run", "--no-relay"]);
            let (ok, out, err) = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
            eprintln!("B RUN ok={ok} err={err} out={out}");
            std::thread::sleep(Duration::from_secs(2));
            let d: Vec<String> = [&a, &b, &c]
                .iter()
                .map(|r| {
                    Cell::open(&r.cell_path("memoria"))
                    .unwrap(/* t */)
                    .replicated_digest()
                    .unwrap(/* t */)
                })
                .collect();
            rounds += 1;
            if (d[0] == d[1] && d[1] == d[2]) || rounds > 6 {
                digests = d;
                break;
            }
        }
        if digests[0] != digests[1] {
            use std::collections::BTreeSet;
            let ids_of = |root: &std::path::PathBuf| -> BTreeSet<String> {
                let c = Cell::open(&root.join("cells").join("memoria.sqlite")).unwrap(/* t */);
                c.debug_column("SELECT op_id FROM ops")
                    .unwrap_or_default()
                    .into_iter()
                    .collect()
            };
            let (ia, ib) = (ids_of(&a.root), ids_of(&b.root));
            eprintln!("A-only: {:?}", ia.difference(&ib).collect::<Vec<_>>());
            eprintln!("B-only: {:?}", ib.difference(&ia).collect::<Vec<_>>());
        }
        assert_eq!(digests[0], digests[1], "A and B agree (rounds {rounds})");
        assert_eq!(digests[1], digests[2], "C caught up (rounds {rounds})");
        for r in [&a, &b, &c] {
            assert!(
                Cell::open(&r.cell_path("memoria"))
            .unwrap(/* t */)
            .verify()
            .unwrap(/* t */)
            .ok()
            );
        }
        drop(serve);
    });
}

// ---------------------------------------------------------------------------
// the removal with its rotation, end to end (v0.6.1): what the spec
// promises and the reviewer found missing — the removed device cannot open
// the new envelopes (the epoch turned), its copy stays behind, and no
// session of its own passes the gate anymore
// ---------------------------------------------------------------------------

/// The digests of every root's cell, in order.
fn digests_of(roots: &[&Root]) -> Vec<String> {
    roots
        .iter()
        .map(|r| {
            Cell::open(&r.cell_path("memoria"))
                .unwrap(/* t */)
                .replicated_digest()
                .unwrap(/* t */)
        })
        .collect()
}

#[test]
fn the_removal_rotates_and_the_removed_stays_out() {
    bounded("the_removal_rotates", 900, || {
        let a = Root::new("rem-a");
        let b = Root::new("rem-b");
        let c = Root::new("rem-c");
        let cell = Cell::create(&a.cell_path("memoria"), "memoria", "cli").unwrap(/* t */);
        cell.put_note(&note(1, "la base comune"), "cli").unwrap(/* t */);
        drop(cell);
        run(&a.root, &["sync", "share", "memoria"]);
        pair(&a, &b, "mac");
        pair(&a, &c, "telefono");
        // A serves; B and C know its address and run passes
        let serve = Daemon::spawn(&a.root, &["--json", "sync", "serve", "--no-relay"]);
        let v = serve.first_json();
        let addrs: Vec<String> = v["addrs"]
            .as_array()
            .map(|x| {
                x.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let node = v["node"].as_str().expect(/* printed */ "node").to_string();
        let reg = rusqlite::Connection::open(a.root.join("devices.db")).unwrap(/* t */);
        let a_device: String = reg
            .query_row(
                "SELECT json_extract(payload_json, '$.device') FROM registry WHERE kind='create'",
                [],
                |r| r.get(0),
            )
            .unwrap(/* t */);
        for r in [&b.root, &c.root] {
            remember_all(r, &a_device, &node, &addrs);
        }
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        let _ = run(&c.root, &["--json", "sync", "run", "--no-relay"]);
        // C writes a note of its own, and the three converge
        add_note(&c.root, "C");
        let mut rounds = 0;
        let digests;
        loop {
            let _ = run(&c.root, &["--json", "sync", "run", "--no-relay"]);
            let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
            std::thread::sleep(Duration::from_secs(2));
            let d = digests_of(&[&a, &b, &c]);
            rounds += 1;
            if (d[0] == d[1] && d[1] == d[2]) || rounds > 6 {
                digests = d;
                break;
            }
        }
        assert_eq!(
            digests[0], digests[2],
            "C's note reached everyone (rounds {rounds})"
        );

        // --- A removes C: the signed remove with the accepted set, the new
        // epoch of every shared cell, the wraps for the survivors
        let (ok, out, err) = run(&a.root, &["--json", "devices", "remove", "telefono"]);
        assert!(ok, "remove: {err}");
        assert!(
            out.contains("\"rotated\"") || out.contains("rotated"),
            "the removal reports the rotation: {out}"
        );
        // the pass carries the remove and the new epoch to B
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        let (ok, out, _) = run(&b.root, &["--json", "sync", "status"]);
        assert!(ok);
        assert!(
            out.contains("\"epoch\": 1"),
            "B now holds the rotated epoch: {out}"
        );
        // A writes a NEW note, sealed under the new epoch
        add_note(&a.root, "A-dopo-la-rimozione");
        let _ = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        std::thread::sleep(Duration::from_secs(3));
        let (ok, out, _) = run(&b.root, &["--json", "sync", "run", "--no-relay"]);
        assert!(ok, "B's pass: {out}");

        // --- the removed device stays out
        // 1. its own sessions are refused at the gate: no device of the
        //    list answers, and the pass says so — a row, not an error
        let (ok, out, _) = run(&c.root, &["--json", "sync", "run", "--no-relay"]);
        assert!(ok, "a pass never fails on an unreachable peer");
        assert!(
            out.contains("\"reached\": false"),
            "C reaches nobody anymore: {out}"
        );
        // 2. its copy stays behind: A and B moved on without it
        let [da, db, dc] = digests_of(&[&a, &b, &c]).try_into().unwrap(/* three */);
        assert_eq!(da, db, "A and B agree after the rotation");
        assert_ne!(db, dc, "the removed device's copy stays behind");
        // 3. it cannot OPEN the new envelopes: the epoch turned, and no
        //    wrap of it exists — the very seal, tried on its own adapter
        let sync_id: [u8; 32] = {
            let reg = rusqlite::Connection::open(a.root.join("devices.db")).unwrap(/* t */);
            let raw: Vec<u8> = reg
                .query_row(
                    "SELECT sync_id FROM shared_cells WHERE name = 'memoria'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(/* t */);
            raw.try_into().expect(/* 32 */ "the sync id")
        };
        let a_shared = engramma_sync::sharedcell::SharedCell::new(
            Cell::open(&a.cell_path("memoria")).unwrap(/* t */),
            engramma_sync::registry::DeviceRegistry::open(&a.root).unwrap(/* t */),
            sync_id,
            &a.root,
        );
        let c_shared = engramma_sync::sharedcell::SharedCell::new(
            Cell::open(&c.cell_path("memoria")).unwrap(/* t */),
            engramma_sync::registry::DeviceRegistry::open(&c.root).unwrap(/* t */),
            sync_id,
            &c.root,
        );
        use engramma_sync::protocol::SyncedCell;
        let new_note_op = a_shared
            .cell()
            .ops()
            .unwrap(/* t */)
            .into_iter()
            .find(|o| o.actor == "cli" && o.hlc.physical_ms > 0 && {
                // the note written after the removal: the highest put of A
                o.kind.as_str() == "put_note"
            })
            .expect(/* just written */ "A's new note");
        let mut id32 = [0_u8; 32];
        for (i, chunk) in new_note_op.content_hash.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16).unwrap_or(0) as u8;
            let lo = (chunk[1] as char).to_digit(16).unwrap_or(0) as u8;
            id32[i] = (hi << 4) | lo;
        }
        let (epoch, nonce, ct) = a_shared
            .envelope_of(&id32)
            .expect(/* A seals */ "A's envelope");
        assert!(epoch >= 1, "the new note is sealed under the rotated epoch");
        let opened = c_shared.ingest(epoch, &id32, &nonce, &ct);
        assert!(
            opened.is_err(),
            "the removed device must not open the new envelopes: {opened:?}"
        );
        drop(serve);
    });
}

// ---------------------------------------------------------------------------
// the short code with its burnt channel, end to end (v0.6.1): the meeting
// at the blind node, the SAS on both screens, the cells that arrive — and
// the SECOND attempt with the same code refused by the burnt channel
// ---------------------------------------------------------------------------

#[test]
fn the_short_code_pairs_and_the_second_attempt_burns() {
    bounded("the_short_code", 600, || {
        let a = Root::new("code-a");
        let b = Root::new("code-b");
        let stranger = Root::new("code-s");
        let node_dir = common::test_dir();
        let node_root = node_dir.path().join("node");
        std::fs::create_dir_all(&node_root).unwrap(/* t */);
        // the cell on A, shared
        let cell = Cell::create(&a.cell_path("memoria"), "memoria", "cli").unwrap(/* t */);
        cell.put_note(&note(3, "la nota del codice"), "cli").unwrap(/* t */);
        drop(cell);
        run(&a.root, &["sync", "share", "memoria"]);
        // the blind node serves (its --root is the subcommand's own)
        let node = {
            let mut node_cmd = bin();
            node_cmd
                .args(["--json", "mailbox", "serve", "--no-relay", "--root"])
                .arg(&node_root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = node_cmd.spawn().expect(/* bin */ "node spawn");
            let (tx, rx) = std::sync::mpsc::channel();
            if let Some(out) = child.stdout.take() {
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(out).lines() {
                        let Ok(line) = line else { break };
                        tx.send(line).ok();
                    }
                });
            }
            // the stderr is drained too (K3): a piped-but-unread pipe
            // fills and the node blocks
            let err_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            if let Some(se) = child.stderr.take() {
                let sink = std::sync::Arc::clone(&err_buf);
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(se).lines() {
                        let Ok(line) = line else { break };
                        let mut buf = sink.lock().expect(/* test */ "stderr buf");
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                });
            }
            Daemon {
                child,
                lines: rx,
                err: err_buf,
            }
        };
        let v = node.first_json();
        let ticket = v["ticket"].to_string();
        let (ok, _, err) = run(&a.root, &["mailbox", "add", &ticket]);
        assert!(ok, "A registers: {err}");
        let (ok, _, err) = run(&b.root, &["mailbox", "add", &ticket]);
        assert!(ok, "B registers: {err}");
        let (ok, _, err) = run(&stranger.root, &["mailbox", "add", &ticket]);
        assert!(ok, "the stranger registers too: {err}");
        // A opens the channel and prints the code (SAS confirmed on stdin)
        let mut code_proc = bin()
            .arg("--root")
            .arg(&a.root)
            .args(["devices", "code", "--no-relay"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect(/* bin */ "spawn code");
        // the inviter's SAS confirms (one for the code meeting, one for
        // the ticket flow that follows) ride the pipe from the start
        {
            use std::io::Write;
            code_proc
                .stdin
                .as_mut()
                .expect(/* piped */ "code stdin")
                .write_all(b"s\ns\n")
                .expect(/* pipe */ "code confirm");
        }
        {
            use std::io::BufRead;
            let mut reader =
                std::io::BufReader::new(code_proc.stdout.take().expect(/* p */ "stdout"));
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).expect(/* pipe */ "read");
                // the code is NUMBER-WORD-WORD (the words are Italian:
                // accents included)
                let trimmed = line.trim();
                let parts: Vec<&str> = trimmed.split('-').collect();
                let word_ok = |w: &str| {
                    w.chars()
                        .all(|c| c.is_alphanumeric() && !c.is_ascii_uppercase())
                };
                if parts.len() == 3
                    && parts[0].len() <= 4
                    && parts[0].bytes().all(|b| b.is_ascii_digit())
                    && word_ok(parts[1])
                    && word_ok(parts[2])
                {
                    CODE.with(|c| c.borrow_mut().clone_from(&trimmed.to_string()));
                    // keep draining the inviter's stdout: closing the read
                    // end would break its next print (the SAS prompt)
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        for _ in reader.lines() {}
                    });
                    break;
                }
                if n == 0 || std::time::Instant::now() > deadline {
                    panic!("`devices code` non ha stampato il codice in 30 s");
                }
            }
        }
        let code = CODE.with(|c| c.borrow().clone());
        assert!(!code.is_empty(), "the code was printed");
        // B enters with the code (SAS confirmed on stdin)
        // B enters with the code (two SAS confirms on stdin: the code
        // meeting's and the ticket flow's)
        let (ok, out, err) = run_with_stdin(
            &b.root,
            &["devices", "join", "--no-relay", &code, "--name", "mac"],
            b"s\ns\n",
        );
        assert!(ok, "join col codice: {err} {out}");
        // the inviter side finishes (its confirm was already in the pipe)
        let code_out = wait_bounded(code_proc, "devices code", 180);
        assert!(
            code_out.status.success(),
            "devices code: {} {}",
            String::from_utf8_lossy(&code_out.stdout),
            String::from_utf8_lossy(&code_out.stderr),
        );
        // B is in the list and holds the cell
        let (ok, out, _) = run(&a.root, &["--json", "devices", "list"]);
        assert!(ok && out.contains("mac"), "B entered: {out}");
        assert!(
            Cell::open(&b.cell_path("memoria"))
                .unwrap(/* t */)
                .replicated_digest()
                .unwrap(/* t */)
                == Cell::open(&a.cell_path("memoria"))
                    .unwrap(/* t */)
                    .replicated_digest()
                    .unwrap(/* t */),
            "the cell arrived whole through the code"
        );
        // --- the burnt channel: the SECOND attempt with the same code is
        // refused — one attempt per code, the second fails even with the
        // right code in hand
        let (ok, out, err) = run_with_stdin(
            &stranger.root,
            &["devices", "join", "--no-relay", &code, "--name", "estraneo"],
            b"s\n",
        );
        assert!(
            !ok,
            "the second attempt with the same code must fail: {ok} {out}"
        );
        let all = format!("{out}{err}");
        assert!(
            all.contains("burnt"),
            "the refusal names the burnt channel: {all}"
        );
        let (ok, out, _) = run(&a.root, &["--json", "devices", "list"]);
        assert!(
            ok && !out.contains("estraneo"),
            "the stranger did not enter: {out}"
        );
        drop(node);
    });
}

thread_local! {
    static CODE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

// ---------------------------------------------------------------------------
// C4 (v0.6.1, la chiusura vera): the latency the spec asks for — from a
// WRITE on A to the note READABLE on B, with B (and A) running the REAL
// `sync serve` cycle: `data_version` + the 2 s debounce. The debounce is
// IN the number; the no-debounce view is reported beside it. A
// measurement, not a test:
//
// ```text
// cargo test --release -p engramma-cli --test sync_serve_cli \
//   the_write_to_read_latency -- --ignored --nocapture
// ```
// ---------------------------------------------------------------------------

/// Poll B's cell (read-only) until the note with this body is readable.
fn b_has_note(b_root: &std::path::Path, body: &str) -> bool {
    let conn = rusqlite::Connection::open_with_flags(
        b_root.join("cells").join("memoria.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect(/* exists */ "B's cell");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM notes WHERE body = ?1", [body], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    n > 0
}

#[test]
#[ignore = "una misura, non un test: cargo test --release -- --ignored --nocapture"]
fn the_write_to_read_latency() {
    let a = Root::new("lat-a");
    let b = Root::new("lat-b");
    // the shared cell, paired the real way
    let cell = Cell::create(&a.cell_path("memoria"), "memoria", "cli").unwrap(/* t */);
    cell.put_note(&note(1, "la base"), "cli").unwrap(/* t */);
    drop(cell);
    run(&a.root, &["sync", "share", "memoria"]);
    pair(&a, &b, "mac");
    // B serves (the receiving side); its address reaches A's book. On
    // loopback without a relay the first contact after the pairing needs
    // the live address (the pairing's own endpoints die with the join) —
    // in production the relay carries the first contact with the node id
    // alone, and every successful session refreshes the row. The key is
    // B's DEVICE id as A's registry knows it (the member «mac»).
    let serve_b = Daemon::spawn(&b.root, &["--json", "sync", "serve", "--no-relay"]);
    let vb = serve_b.first_json();
    let addrs: Vec<String> = vb["addrs"]
        .as_array()
        .map(|x| {
            x.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let node = vb["node"].as_str().expect(/* printed */ "node").to_string();
    let reg = rusqlite::Connection::open(a.root.join("devices.db")).unwrap(/* t */);
    let b_device: String = reg
        .query_row(
            "SELECT device FROM members WHERE name = 'mac'",
            [],
            |r| r.get(0),
        )
        .unwrap(/* t */);
    drop(reg);
    remember_all(&a.root, &b_device, &node, &addrs);
    // A serves too: the WRITE side of the cycle (its watcher sees the
    // write, debounces, and runs the pass that pushes to B)
    let serve_a = Daemon::spawn(&a.root, &["--json", "sync", "serve", "--no-relay"]);
    let _ = serve_a.first_json();
    // let both daemons settle their watchers
    std::thread::sleep(Duration::from_secs(2));

    // the fifty writes, each one timed to READABLE on B, spaced 3 s so
    // each pays its own debounce
    let mut latencies: Vec<std::time::Duration> = Vec::with_capacity(50);
    let started_all = std::time::Instant::now();
    for i in 0..50_u32 {
        let body = format!("la nota misurata numero {i}");
        // the write is SIGNED, as every local write of the sync era (the
        // CLI arms the signer; an unsigned write would be refused by B's
        // gate — and rightly so)
        let cell = Cell::open(&a.cell_path("memoria")).unwrap(/* t */);
        cell.set_signer(std::sync::Arc::new(engramma_sync::arrival::DeviceSigner::new(
            &engramma_sync::device::DeviceKey::load_or_create(&a.root).unwrap(/* t */),
        )))
        .unwrap(/* t */);
        let mut n = note((10 + i) as u16, "segnaposto");
        n.body = body.clone();
        cell.put_note(&n, "cli").unwrap(/* t */);
        drop(cell);
        let t0 = std::time::Instant::now();
        loop {
            if b_has_note(&b.root, &body) {
                break;
            }
            if t0.elapsed() > Duration::from_secs(30) {
                panic!("la nota {i} non è mai arrivata su B in 30 s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        latencies.push(t0.elapsed());
        // distanziate: the next write starts its own debounce cycle
        std::thread::sleep(Duration::from_secs(3));
    }
    drop(serve_a);
    drop(serve_b);

    // mediana e p95 (with the debounce in, and the no-debounce view)
    let mut ms: Vec<f64> = latencies.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|x, y| x.partial_cmp(y).expect(/* finite */ "cmp"));
    let pick =
        |q: f64| -> f64 { ms[((q * (ms.len() - 1) as f64).round() as usize).min(ms.len() - 1)] };
    let median = pick(0.5);
    let p95 = pick(0.95);
    let debounce_ms = 2000.0_f64;
    eprintln!(
        "latenza scritta→leggibile (50 scritture distanziate 3 s, ciclo reale):\n  \
         mediana {median:.0} ms, p95 {p95:.0} ms (il rimbalzo di 2 s è dentro)\n  \
         senza rimbalzo: mediana {:.0} ms, p95 {:.0} ms\n  \
         totale del giro: {:?}",
        (median - debounce_ms).max(0.0),
        (p95 - debounce_ms).max(0.0),
        started_all.elapsed(),
    );
    // K1 (v0.6.1-ciclo): a median BELOW the debounce would not be latency
    // — it would be the cycle's PHASE (the write catching a pass that was
    // already running for another reason). The measure fails rather than
    // lie: the expected shape is debounce + one watch tick + one session.
    assert!(
        median >= debounce_ms,
        "la mediana {median:.0} ms è sotto il rimbalzo di {debounce_ms:.0} ms: \
         la misura non sta misurando la latenza, ma la fase del ciclo"
    );
}

/// C4 (v0.6.1): the serve cycle PUSHES a write made by a SECOND process on
/// the same root (the Homun shape) — nobody pulls. `PRAGMA data_version`
/// polled on FRESH connections never sees another process's commit (the
/// counter is per-connection), so the daemon's watcher was blind to
/// cross-process writes: the cycle must watch on PERSISTENT connections.
#[test]
fn the_serve_cycle_pushes_a_second_process_write() {
    let a = Root::new("push-a");
    let b = Root::new("push-b");
    let cell = Cell::create(&a.cell_path("memoria"), "memoria", "cli").unwrap(/* t */);
    cell.put_note(&note(1, "la base"), "cli").unwrap(/* t */);
    drop(cell);
    run(&a.root, &["sync", "share", "memoria"]);
    pair(&a, &b, "mac");
    // B serves (the receiving side); A's book gets B's live address
    let serve_b = Daemon::spawn(&b.root, &["--json", "sync", "serve", "--no-relay"]);
    let vb = serve_b.first_json();
    let addrs: Vec<String> = vb["addrs"]
        .as_array()
        .map(|x| {
            x.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let node = vb["node"].as_str().expect(/* printed */ "node").to_string();
    let reg = rusqlite::Connection::open(a.root.join("devices.db")).unwrap(/* t */);
    let b_device: String = reg
        .query_row("SELECT device FROM members WHERE name = 'mac'", [], |r| {
            r.get(0)
        })
        .unwrap(/* t */);
    drop(reg);
    remember_all(&a.root, &b_device, &node, &addrs);
    // A serves: ITS cycle must see the write and push — nobody pulls
    let serve_a = Daemon::spawn(&a.root, &["--json", "sync", "serve", "--no-relay"]);
    let _ = serve_a.first_json();
    std::thread::sleep(Duration::from_secs(2));
    // the write, from a SECOND process on A's root (the CLI itself)
    let file = common::test_dir();
    let path = file.path().join("nota.md");
    std::fs::write(&path, "la nota scritta da un altro processo").unwrap(/* t */);
    let (ok, _, err) = run(
        &a.root,
        &[
            "--cell",
            "memoria",
            "add",
            path.to_str().expect(/* utf8 */ "path"),
            "--source",
            "user",
            "--confidence",
            "stated",
        ],
    );
    assert!(ok, "add: {err}");
    // B receives WITHOUT ever pulling: the watch (≤250 ms) + debounce
    // (2 s) + the pass — 20 s is the bound of shame
    let mut arrived = false;
    for _ in 0..900 {
        std::thread::sleep(Duration::from_millis(100));
        let conn = rusqlite::Connection::open_with_flags(
            b.root.join("cells").join("memoria.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap(/* t */);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE body = 'la nota scritta da un altro processo'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if n > 0 {
            arrived = true;
            break;
        }
    }
    drop(serve_a);
    drop(serve_b);
    assert!(
        arrived,
        "il ciclo di serve doveva spingere la scrittura di un secondo processo fino a B"
    );
}

// ---------------------------------------------------------------------------
// K1 (v0.6.1-ciclo): the cycle RESTS. Two daemons already synced, thirty
// seconds of silence: ZERO «write» passes — the daemon works for nobody
// when nothing moved. And ONE write on A is ONE write pass on A, none on
// B: the operations that came FROM A never bounce back to A (the pass
// they trigger goes toward the OTHERS; with none, it does not go).
// ---------------------------------------------------------------------------

/// A device id as the root's registry holds it (`members` by name).
fn device_of(root: &std::path::Path, name: &str) -> String {
    let reg = rusqlite::Connection::open(root.join("devices.db")).unwrap(/* t */);
    reg.query_row("SELECT device FROM members WHERE name = ?1", [name], |r| r.get(0))
        .unwrap(/* t */)
}

/// Loopback-only addresses for a loopback test (K2): the daemon binds
/// `0.0.0.0`, so every published port answers on 127.0.0.1 — and a test
/// that runs on one machine must not lean on the machine's real
/// interfaces (a VPN's 100.64/10 among them).
fn to_loopback(addrs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in addrs {
        let Some(port) = a.rsplit(':').next() else {
            continue;
        };
        let lb = format!("127.0.0.1:{port}");
        if !out.contains(&lb) {
            out.push(lb);
        }
    }
    out
}

/// Two roots, one shared cell, both daemons traced and already synced:
/// the scene the K1 measures rest on.
struct ServedPair {
    a: Root,
    b: Root,
    serve_a: Daemon,
    serve_b: Daemon,
}

impl ServedPair {
    fn new(tag_a: &str, tag_b: &str) -> Self {
        let a = Root::new(tag_a);
        let b = Root::new(tag_b);
        let cell = Cell::create(&a.cell_path("memoria"), "memoria", "cli").unwrap(/* t */);
        cell.put_note(&note(1, "la base"), "cli").unwrap(/* t */);
        drop(cell);
        run(&a.root, &["sync", "share", "memoria"]);
        pair(&a, &b, "mac");
        let serve_a = Daemon::spawn_traced(&a.root, &["--json", "sync", "serve", "--no-relay"]);
        let serve_b = Daemon::spawn_traced(&b.root, &["--json", "sync", "serve", "--no-relay"]);
        let va = serve_a.first_json();
        let vb = serve_b.first_json();
        let addrs_of = |v: &serde_json::Value| -> Vec<String> {
            let published: Vec<String> = v["addrs"]
                .as_array()
                .map(|x| {
                    x.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            to_loopback(&published)
        };
        let node_of = |v: &serde_json::Value| -> String {
            v["node"]
                .as_str()
                .expect(/* printed */ "the node id")
                .to_string()
        };
        // each book learns the other daemon's live address (loopback only:
        // same machine, same test)
        remember_all(
            &a.root,
            &device_of(&a.root, "mac"),
            &node_of(&vb),
            &addrs_of(&vb),
        );
        remember_all(
            &b.root,
            &device_of(&b.root, "laptop"),
            &node_of(&va),
            &addrs_of(&va),
        );
        // NOTE: no `sync run` here on purpose. The run's endpoint shares
        // the root's transport KEY with the daemon: the peer's magicsock
        // would stick to the run's ephemeral port, which dies with the
        // run — the daemon's later dial to the same node id times out
        // against the dead route. The pair is already synced (the join
        // carried the cell); the books are set by hand above.
        // the watchers settle (their baselines take whatever the setup
        // wrote): the count starts from the quiet
        std::thread::sleep(Duration::from_secs(4));
        Self {
            a,
            b,
            serve_a,
            serve_b,
        }
    }
}

/// K1: after a write has crossed (the cycle is HOT), thirty seconds of
/// silence: zero «write» passes on both sides. The rest state is the one
/// that matters — a cycle that only stops when nothing ever happened is
/// not a cycle that rests.
#[test]
fn the_serve_cycle_rests_when_idle() {
    bounded("rests_when_idle", 420, || {
        let scene = ServedPair::new("rest-a", "rest-b");
        // one write first: it crosses, the cycle runs — and then it must
        // STOP. The silence is measured from the quiet after the crossing.
        add_note(&scene.a.root, "A");
        let deadline = std::time::Instant::now() + Duration::from_secs(25);
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if b_has_note(&scene.b.root, "scritta concorrente di A") {
                break;
            }
            if std::time::Instant::now() > deadline {
                let tail_a = scene.serve_a.trace_tail();
                panic!("la nota di A non è mai arrivata su B:\n{tail_a}");
            }
        }
        // let the last pass of the crossing finish
        std::thread::sleep(Duration::from_secs(4));
        let before = (scene.serve_a.passes("write"), scene.serve_b.passes("write"));
        std::thread::sleep(Duration::from_secs(30));
        let after = (scene.serve_a.passes("write"), scene.serve_b.passes("write"));
        let tail_a = scene.serve_a.trace_tail();
        let tail_b = scene.serve_b.trace_tail();
        drop(scene);
        assert_eq!(
            after.0 - before.0,
            0,
            "A ha fatto {} passate write in 30 s di silenzio:\n{tail_a}",
            after.0 - before.0
        );
        assert_eq!(
            after.1 - before.1,
            0,
            "B ha fatto {} passate write in 30 s di silenzio:\n{tail_b}",
            after.1 - before.1
        );
    });
}

/// K1: one write on A is ONE write pass on A — and NONE on B: what came
/// from A never bounces back to A.
#[test]
fn one_write_is_one_pass_and_none_back() {
    bounded("one_write_one_pass", 420, || {
        let scene = ServedPair::new("one-a", "one-b");
        let before = (scene.serve_a.passes("write"), scene.serve_b.passes("write"));
        // the write, from a SECOND process on A's root (the CLI)
        add_note(&scene.a.root, "A");
        // it must reach B with nobody pulling: the cycle pushes it
        let arrived = {
            let deadline = std::time::Instant::now() + Duration::from_secs(25);
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if b_has_note(&scene.b.root, "scritta concorrente di A") {
                    break true;
                }
                if std::time::Instant::now() > deadline {
                    break false;
                }
            }
        };
        // any spurious re-trigger shows in the next seconds
        std::thread::sleep(Duration::from_secs(6));
        let after = (scene.serve_a.passes("write"), scene.serve_b.passes("write"));
        let tail_a = scene.serve_a.trace_tail();
        let tail_b = scene.serve_b.trace_tail();
        drop(scene);
        assert!(
            arrived,
            "la nota di A non è mai arrivata su B\n— traccia di A:\n{tail_a}\n— traccia di B:\n{tail_b}"
        );
        assert_eq!(
            after.0 - before.0,
            1,
            "A doveva fare UNA passata write, non {}:\n{tail_a}",
            after.0 - before.0
        );
        assert_eq!(
            after.1 - before.1,
            0,
            "B non doveva fare passate write (niente rimbalzo verso A), non {}:\n{tail_b}",
            after.1 - before.1
        );
    });
}
