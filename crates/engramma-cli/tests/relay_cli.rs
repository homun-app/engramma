// SPDX-License-Identifier: Apache-2.0

//! R2 (v0.6.1, revisore): the relay the documents promise — `--relay <url>`
//! and `--no-relay` on every command that binds an endpoint, the choice
//! saved in the root so `sync serve` does not ask again, an explicit flag
//! winning over what is saved.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_engramma"))
}

/// A child process that DIES with the test, even on a panic (R5).
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

/// Run the CLI to completion, bounded: a hung command fails the test with
/// a clear line instead of hanging the run.
fn run(root: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let mut child = bin()
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(/* bin */ "spawn");
    let out;
    let err;
    {
        let mut so = child.stdout.take().expect(/* piped */ "stdout");
        let mut se = child.stderr.take().expect(/* piped */ "stderr");
        let t_out = std::thread::spawn(move || {
            use std::io::Read;
            let mut s = String::new();
            let _ = so.read_to_string(&mut s);
            s
        });
        let t_err = std::thread::spawn(move || {
            use std::io::Read;
            let mut s = String::new();
            let _ = se.read_to_string(&mut s);
            s
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            match child.try_wait().expect(/* child */ "wait") {
                Some(status) => {
                    out = t_out.join().unwrap_or_default();
                    err = t_err.join().unwrap_or_default();
                    return (status.success(), out, err);
                }
                None if std::time::Instant::now() > deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("`engramma {}` non è finito in 60 s", args.join(" "));
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

/// The saved relay choice of a root (`devices.db`, meta `relay`).
fn saved_choice(root: &std::path::Path) -> Option<String> {
    let reg = rusqlite::Connection::open(root.join("devices.db")).ok()?;
    reg.query_row("SELECT value FROM meta WHERE key = 'relay'", [], |r| {
        r.get(0)
    })
    .ok()
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
}

/// The flags exist on every command that binds an endpoint, and the choice
/// they carry is SAVED in the root (a later flagless run reads it back).
#[test]
fn the_relay_flags_exist_and_save_their_choice() {
    let a = Root::new("relay-a");
    // --relay <url> on sync run: today the flag does not exist
    let (ok, _out, err) = run(
        &a.root,
        &["--json", "sync", "run", "--relay", "https://127.0.0.1:9/"],
    );
    assert!(ok, "`sync run --relay <url>` deve esistere: {err}");
    assert_eq!(
        saved_choice(&a.root).as_deref(),
        Some("own:https://127.0.0.1:9/"),
        "la scelta data esplicitamente si salva nella radice"
    );
    // --no-relay on sync run: the explicit flag wins over what is saved
    let (ok, _out, err) = run(&a.root, &["--json", "sync", "run", "--no-relay"]);
    assert!(ok, "`sync run --no-relay` deve esistere: {err}");
    assert_eq!(
        saved_choice(&a.root).as_deref(),
        Some("none"),
        "un flag esplicito vince sulla scelta salvata"
    );
    // and on the other commands that bind an endpoint
    let b = Root::new("relay-b");
    for args in [
        vec!["--json", "sync", "serve", "--no-relay"],
        vec!["--json", "mailbox", "serve", "--no-relay", "--root"],
    ] {
        // serve commands run until the signal: start, read the first json
        // line, stop — the flag must be accepted
        let child = ChildGuard {
            child: Some(
                bin()
                    .arg("--root")
                    .arg(if args[1] == "mailbox" {
                        &a.root
                    } else {
                        &b.root
                    })
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect(/* bin */ "spawn serve"),
            ),
        };
        let mut child = child;
        let mut line = String::new();
        {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(
                child
                    .child
                    .as_mut()
                    .expect(/* alive */ "serve")
                    .stdout
                    .take()
                    .expect(/* p */ "stdout"),
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let mut buf = String::new();
                let n = reader.read_line(&mut buf).expect(/* pipe */ "read");
                line.push_str(&buf);
                if n == 0 || line.contains('}') {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    drop(child);
                    panic!("`engramma {}` non ha parlato in 30 s", args.join(" "));
                }
            }
        }
        drop(child);
        assert!(
            !line.contains("error:") && !line.to_lowercase().contains("unexpected argument"),
            "`engramma {}` deve accettare il flag: {line}",
            args.join(" ")
        );
    }
    assert_eq!(saved_choice(&b.root).as_deref(), Some("none"));

    // devices invite / join / code carry the flags too: the invite with
    // --no-relay must print a ticket (bounded by the spawn-and-read below)
    let c = Root::new("relay-c");
    let mut child = ChildGuard {
        child: Some(
            bin()
                .arg("--root")
                .arg(&c.root)
                .args(["devices", "invite", "--no-relay"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect(/* bin */ "spawn invite"),
        ),
    };
    {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(
            child
                .child
                .as_mut()
                .expect(/* alive */ "invite")
                .stdout
                .take()
                .expect(/* p */ "stdout"),
        );
        let mut got = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf).expect(/* pipe */ "read");
            got.push_str(&buf);
            if got.contains("engramma-invite-v1.") || n == 0 {
                break;
            }
            if std::time::Instant::now() > deadline {
                drop(child);
                panic!("`devices invite --no-relay` non ha stampato il biglietto in 30 s");
            }
        }
        assert!(
            got.contains("engramma-invite-v1."),
            "`devices invite --no-relay` deve stampare un biglietto: {got}"
        );
        if let Some(stdin) = child
            .child
            .as_mut()
            .expect(/* alive */ "invite")
            .stdin
            .as_mut()
        {
            let _ = stdin.write_all(b"\n");
        }
    }
    drop(child);
    assert_eq!(saved_choice(&c.root).as_deref(), Some("none"));
}

// ---------------------------------------------------------------------------
// C1 (v0.6.1, la chiusura vera): the ticket carries the relay. The
// production code NEVER waited for the endpoint to reach its relay — the
// tests did, with online(), and the hole was exactly there. Now the wait
// lives in production (ticket_of, node_ticket, the serve's published
// address) and these tests go through the BINARY.
// ---------------------------------------------------------------------------

use std::io::BufRead;

/// Run iroh's LOCAL relay (loopback, self-signed certificate) on a thread
/// of its own, alive for the whole test: a current-thread runtime parks
/// its tasks the moment `block_on` returns, and the relay must KEEP
/// serving while the binary under test connects.
fn local_relay() -> String {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect(/* std */ "runtime");
        rt.block_on(async move {
            let (_map, url, _server) = iroh::test_utils::run_relay_server()
                .await
                .expect(/* loopback */ "the local relay");
            tx.send(url.to_string()).expect(/* test gone */ "the test");
            // park here forever: the runtime keeps driving the relay
            std::future::pending::<()>().await;
        });
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect(/* loopback */ "the local relay's url")
}

/// The BINARY VARIANT that trusts the local relay's self-signed
/// certificate (feature `test-relay`, never default): the production
/// binary must not carry it, and this build lives in its own target dir.
/// Built here, incrementally, once per machine.
fn test_relay_bin() -> std::path::PathBuf {
    // OUTSIDE the repo: the Linux round mounts the source read-only, and
    // a target dir inside it cannot even be created there
    let target = std::env::temp_dir().join("engramma-cli-relay");
    let status = Command::new(env!("CARGO"))
        .arg("build")
        .arg("-p")
        .arg("engramma-cli")
        .arg("--bin")
        .arg("engramma")
        .arg("--features")
        .arg("test-relay")
        .arg("--offline")
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect(/* cargo */ "cargo build");
    assert!(
        status.success(),
        "la build del binario test-relay è fallita"
    );
    target.join("debug").join("engramma")
}

/// Spawn the variant with stdin fed, watch stdout until a balanced JSON
/// object lands, bounded; the child DIES with the test.
struct JsonChild {
    /// dies with the test (R5): read by Drop, never by hand
    #[allow(dead_code)]
    guard: ChildGuard,
    stdout: std::process::ChildStdout,
}

fn spawn_json(bin_path: &std::path::Path, args: &[&str], root: &std::path::Path) -> JsonChild {
    let mut child = Command::new(bin_path)
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(/* bin */ "spawn");
    let stdout = child.stdout.take().expect(/* piped */ "stdout");
    JsonChild {
        guard: ChildGuard { child: Some(child) },
        stdout,
    }
}

/// The first balanced JSON object off a child's stdout, bounded.
fn first_json(reader: &mut dyn BufRead, what: &str, secs: u64) -> serde_json::Value {
    let mut text = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut depth = 0_i32;
    loop {
        let mut buf = String::new();
        let n = reader.read_line(&mut buf).expect(/* pipe */ "read");
        text.push_str(&buf);
        for c in buf.chars() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 && !text.trim().is_empty() {
                        return serde_json::from_str(text.trim())
                            .unwrap_or_else(|e| panic!("{what}: json non valido: {e} — {text}"));
                    }
                }
                _ => {}
            }
        }
        if n == 0 {
            panic!("{what}: stdout chiuso prima del json — {text}");
        }
        if std::time::Instant::now() > deadline {
            panic!("{what}: nessun json in {secs} s — fin qui: {text}");
        }
    }
}

/// The invite ticket, FROM THE BINARY against the local relay: it carries
/// the relay's URL (the JSON says it, and the encoded ticket too).
#[test]
fn the_invite_ticket_carries_the_relay_from_the_binary() {
    let relay_url = local_relay();
    let bin = test_relay_bin();
    let a = Root::new("c1-invite");
    let mut child = spawn_json(
        &bin,
        &["--json", "devices", "invite", "--relay", relay_url.as_str()],
        &a.root,
    );
    let v = {
        let mut reader = std::io::BufReader::new(&mut child.stdout);
        first_json(&mut reader, "devices invite", 40)
    };
    drop(child); // the invite keeps waiting for a joiner: done with it
    assert_eq!(
        v["relay"].as_str(),
        Some(relay_url.as_str()),
        "il JSON dice il relay: {v}"
    );
    // and the encoded ticket carries it too
    let encoded = v["ticket"].as_str().expect(/* json */ "the ticket");
    let ticket =
        engramma_sync::pairing::Ticket::decode(encoded).expect(/* printed */ "decode");
    assert_eq!(
        ticket.relay, relay_url,
        "il biglietto porta il relay che l'endpoint cavalca"
    );
    // no warning when the relay came
    assert!(
        v.get("relay_warning").is_none(),
        "nessun avviso quando il relay c'è: {v}"
    );
}

/// The blind node's ticket, FROM THE BINARY against the local relay.
#[test]
fn the_node_ticket_carries_the_relay_from_the_binary() {
    let relay_url = local_relay();
    let bin = test_relay_bin();
    let dir = tempfile::tempdir().unwrap(/* t */);
    let node_root = dir.path().join("node");
    std::fs::create_dir_all(&node_root).unwrap(/* t */);
    let mut child = spawn_json(
        &bin,
        &[
            "--json",
            "mailbox",
            "serve",
            "--root",
            node_root.to_str().expect(/* utf8 */ "path"),
            "--relay",
            relay_url.as_str(),
        ],
        &node_root,
    );
    let v = {
        let mut reader = std::io::BufReader::new(&mut child.stdout);
        first_json(&mut reader, "mailbox serve", 40)
    };
    drop(child);
    assert_eq!(
        v["relay"].as_str(),
        Some(relay_url.as_str()),
        "il JSON del nodo dice il relay: {v}"
    );
    // the ticket's own JSON carries the same URL
    let ticket_json = v["ticket"].to_string();
    assert!(
        ticket_json.contains(&relay_url),
        "il biglietto del nodo porta il relay: {ticket_json}"
    );
    let addr =
        engramma_sync::mailbox_net::node_ticket_parse(v["ticket"].to_string().trim_matches('"'))
            .expect(/* printed */ "parse");
    assert_eq!(
        addr.relay.as_deref(),
        Some(relay_url.as_str()),
        "chi registra il nodo impara il relay dal biglietto"
    );
}

/// An UNREACHABLE relay: the ticket comes out within the wait, WITHOUT
/// the relay, and the CLI SAYS so (stderr line, JSON null + warning).
/// The plain production binary — no TLS is ever made to an unreachable
/// relay, so the variant is not needed here.
#[test]
fn an_unreachable_relay_gives_the_ticket_within_the_wait_and_says_it() {
    let a = Root::new("c1-dead");
    let started = std::time::Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&a.root)
        .args([
            "--json",
            "devices",
            "invite",
            "--relay",
            "https://127.0.0.1:9/",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(/* bin */ "spawn invite");
    let guard = ChildGuard { child: Some(child) };
    let mut guard = guard;
    let mut stdout = guard
        .child
        .as_mut()
        .expect(/* alive */ "invite")
        .stdout
        .take()
        .expect(/* piped */ "stdout");
    let mut reader = std::io::BufReader::new(&mut stdout);
    let v = first_json(&mut reader, "devices invite (relay morto)", 25);
    let waited = started.elapsed();
    drop(guard);
    assert!(v["relay"].is_null(), "nessun relay nel biglietto: {v}");
    assert!(
        v.get("relay_warning").is_some(),
        "l'avviso c'è, mai in silenzio: {v}"
    );
    assert!(
        v["ticket"]
            .as_str()
            .is_some_and(|t| t.starts_with("engramma-invite-v1.")),
        "il biglietto esce lo stesso: {v}"
    );
    let encoded = v["ticket"].as_str().expect(/* json */ "ticket");
    let ticket =
        engramma_sync::pairing::Ticket::decode(encoded).expect(/* printed */ "decode");
    assert!(
        ticket.relay.is_empty(),
        "il biglietto non promette un relay che non c'è"
    );
    // the wait is bounded: the wait itself is 10 s, the CLI around it
    // small — 25 s is the bound of shame, 10 the honest measure
    assert!(
        waited >= Duration::from_secs(9),
        "ha aspettato il relay? {waited:?}"
    );
    assert!(
        waited <= Duration::from_secs(20),
        "il biglietto deve uscire entro il tempo massimo: {waited:?}"
    );
}

/// `--no-relay` does not wait: the ticket is out in a blink.
#[test]
fn no_relay_does_not_wait() {
    let a = Root::new("c1-fast");
    let started = std::time::Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&a.root)
        .args(["--json", "devices", "invite", "--no-relay"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(/* bin */ "spawn invite");
    let guard = ChildGuard { child: Some(child) };
    let mut guard = guard;
    let mut stdout = guard
        .child
        .as_mut()
        .expect(/* alive */ "invite")
        .stdout
        .take()
        .expect(/* piped */ "stdout");
    let mut reader = std::io::BufReader::new(&mut stdout);
    let v = first_json(&mut reader, "devices invite --no-relay", 20);
    let waited = started.elapsed();
    drop(guard);
    assert!(v["relay"].is_null(), "nessun relay chiesto: {v}");
    assert!(
        waited < Duration::from_secs(3),
        "--no-relay non aspetta: {waited:?}"
    );
}

/// The production binary does NOT carry the test-relay feature: the
/// reviewer's verification stays true after this change. The plain
/// variant builds in its OWN target dir — reading the `test-relay` one
/// (as this test did when it ran after the ticket tests) judged the
/// FEATURE binary and failed on ordering alone (found by the Linux
/// round of v0.6.7: same container, same volume, green on v0.6.6 only
/// because the order had been kinder).
#[test]
fn the_production_binary_has_no_test_utils() {
    let target = std::env::temp_dir().join("engramma-cli-plain");
    let status = Command::new(env!("CARGO"))
        .arg("build")
        .arg("-p")
        .arg("engramma-cli")
        .arg("--bin")
        .arg("engramma")
        .arg("--offline")
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect(/* cargo */ "cargo build");
    assert!(status.success(), "la build del binario plain è fallita");
    let plain = target.join("debug").join("engramma");
    let out = Command::new("strings")
        .arg(&plain)
        .output()
        .expect(/* strings */ "strings");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("insecure_skip_verify"),
        "il binario di produzione non deve fidarsi dei certificati autofirmati"
    );
}
