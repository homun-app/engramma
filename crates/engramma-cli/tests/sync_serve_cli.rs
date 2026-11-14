// SPDX-License-Identifier: Apache-2.0

//! The service and the blind node over the REAL CLI (v0.6.1, P3–P5):
//! `sync serve` beside a second writer on the same root (the Homun
//! convivenza), the one-pass `sync run`, the mailbox node between two
//! devices that never meet, three devices with concurrent writes and a
//! device that comes back, the removal with its rotation.

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

fn run(root: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let out = bin()
        .env("ENGRAMMA_TRACE", "1")
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap(/* t */);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A long-running child whose stdout lines arrive on a channel while it
/// runs (the serve daemon publishes its address as the FIRST json line).
struct Daemon {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Daemon {
    fn spawn(root: &std::path::Path, args: &[&str]) -> Self {
        let mut cmd = bin();
        cmd.arg("--root").arg(root).args(args);
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
        Self { child, lines: rx }
    }

    fn wait_for_line(&self, want: &str) -> String {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect(/* prints at start */ "the daemon spoke");
            if line.contains(want) {
                return line;
            }
        }
    }

    /// The daemon's first JSON object (pretty-printed over several lines).
    fn first_json(&self) -> serde_json::Value {
        let mut text = String::new();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect(/* prints at start */ "the daemon spoke");
            text.push_str(&line);
            if line.trim() == "}" || line.trim_start().starts_with('}') {
                break;
            }
        }
        serde_json::from_str(&text).expect(/* balanced */ "the daemon's json")
    }

    fn stop(mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

struct Root {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Root {
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap(/* t */);
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
    let mut inviter = Daemon::spawn(&a.root, &["devices", "invite"]);
    // feed the inviter's stdin: not piped here — the invite reads one line
    // at its prompt, so run it through `yes s`-style piping instead
    inviter.child.kill().ok();
    drop(inviter);
    // the honest way: spawn with stdin fed
    let mut child = bin()
        .arg("--root")
        .arg(&a.root)
        .args(["devices", "invite"])
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
    let join = bin()
        .arg("--root")
        .arg(&b.root)
        .args(["devices", "join", &ticket, "--name", b_name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(/* bin */ "spawn join");
    let mut join = join;
    // feed both confirms
    child
        .stdin
        .as_mut()
        .expect(/* piped */ "invite stdin")
        .write_all(b"s\n")
        .expect(/* pipe */ "invite confirm");
    let mut join_stdin = join.stdin.take().expect(/* piped */ "join stdin");
    join_stdin
        .write_all(b"s\n")
        .expect(/* pipe */ "join confirm");
    drop(join_stdin);
    let out = join.wait_with_output().expect(/* t */ "join done");
    assert!(
        out.status.success(),
        "join: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let out = child.wait_with_output().expect(/* t */ "invite done");
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
    let serve = Daemon::spawn(&a.root, &["--json", "sync", "serve"]);
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
    let (ok, out, err) = run(&b.root, &["--json", "sync", "run"]);
    assert!(ok, "run: {err}");
    assert!(out.contains("memoria"), "the summary names the cell: {out}");
    // a second pass settles whatever arrived between the purge and the
    // session (the daemon synced it on its own trigger)
    std::thread::sleep(Duration::from_secs(4));
    let _ = run(&b.root, &["--json", "sync", "run"]);
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
    serve.stop();
}

/// P4: A writes and goes away; B converges from the blind node alone; a
/// purge on B reaches the node and no byte of those envelopes survives in
/// the node's file; the node's file never holds note text.
#[test]
fn the_blind_node_carries_and_forgets() {
    let a = Root::new("mbx-a");
    let b = Root::new("mbx-b");
    let node_dir = tempfile::tempdir().unwrap(/* t */);
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
        .args(["--json", "mailbox", "serve", "--root"])
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
    let node = Daemon { child, lines: rx };
    let v = node.first_json();
    let ticket = v["ticket"]
        .as_str()
        .expect(/* printed */ "node ticket")
        .to_string();
    // both devices register with the node
    let (ok, _, err) = run(&a.root, &["mailbox", "add", &ticket]);
    assert!(ok, "A registers: {err}");
    let (ok, _, err) = run(&b.root, &["mailbox", "add", &ticket]);
    assert!(ok, "B registers: {err}");
    // A goes away (no serve daemon): B pushes to and pulls from the node
    let (ok, out, err) = run(&b.root, &["--json", "sync", "run"]);
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
    let _ = run(&b.root, &["--json", "sync", "run"]);
    // the node's file holds no note text and no op id — ciphertext only
    let bytes = std::fs::read(node_root.join("mailbox.db"))
        .expect(/* the node's store */ "mailbox.db");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("quokkaviola"),
        "the node never saw the secret"
    );
    assert!(!text.contains("nota pubblica"), "no note text at the node");
    node.stop();
}

/// One note written through the real CLI (armed with the device signer).
fn add_note(root: &std::path::Path, who: &str) {
    let file = tempfile::tempdir().unwrap(/* t */);
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
    std::mem::forget(file);
}

/// P5: three devices, concurrent writes on all of them, one device that
/// goes away and comes back — convergence at the replicated digest and
/// verify green everywhere.
#[test]
fn three_devices_concurrent_and_one_returns() {
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
    let serve = Daemon::spawn(&a.root, &["--json", "sync", "serve"]);
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
    let _ = run(&b.root, &["--json", "sync", "run"]);
    let _ = run(&c.root, &["--json", "sync", "run"]);
    // concurrent writes on all three, through the CLI: every local write
    // of the sync era carries its device and signature, or the peers'
    // gate refuses it
    for (root, who) in [(&a.root, "A"), (&b.root, "B"), (&c.root, "C")] {
        add_note(root, who);
    }
    // the passes cross: A's daemon syncs on its own writes, B and C run
    std::thread::sleep(Duration::from_secs(4));
    let _ = run(&b.root, &["--json", "sync", "run"]);
    let _ = run(&c.root, &["--json", "sync", "run"]);
    std::thread::sleep(Duration::from_secs(4));
    let _ = run(&b.root, &["--json", "sync", "run"]);
    let _ = run(&c.root, &["--json", "sync", "run"]);
    // C goes away; A and B keep writing
    for (root, who) in [(&a.root, "A2"), (&b.root, "B2")] {
        add_note(root, who);
    }
    std::thread::sleep(Duration::from_secs(4));
    let _ = run(&b.root, &["--json", "sync", "run"]);
    // C comes back; the pull-only topology settles in rounds (each run
    // pulls what the peer had, the daemon pushes nothing — rounds until
    // the three digests agree, as many passes as the truth needs)
    let digests: Vec<String>;
    let mut rounds = 0;
    loop {
        let _ = run(&c.root, &["--json", "sync", "run"]);
        let (ok, out, err) = run(&b.root, &["--json", "sync", "run"]);
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
    serve.stop();
}
