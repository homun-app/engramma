// SPDX-License-Identifier: Apache-2.0

//! R2 (v0.6.1, revisore): the relay the documents promise — `--relay <url>`
//! and `--no-relay` on every command that binds an endpoint, the choice
//! saved in the root so `sync serve` does not ask again, an explicit flag
//! winning over what is saved.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_engramma"))
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
    reg.query_row("SELECT value FROM meta WHERE key = 'relay'", [], |r| r.get(0))
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
        let mut child = bin()
            .arg("--root")
            .arg(if args[1] == "mailbox" { &a.root } else { &b.root })
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect(/* bin */ "spawn serve");
        let mut line = String::new();
        {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(child.stdout.take().expect(/* p */ "stdout"));
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let mut buf = String::new();
                let n = reader
                    .read_line(&mut buf)
                    .expect(/* pipe */ "read");
                line.push_str(&buf);
                if n == 0 || line.contains('}') {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("`engramma {}` non ha parlato in 30 s", args.join(" "));
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
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
    let mut child = bin()
        .arg("--root")
        .arg(&c.root)
        .args(["devices", "invite", "--no-relay"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(/* bin */ "spawn invite");
    {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(child.stdout.take().expect(/* p */ "stdout"));
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
                let _ = child.kill();
                let _ = child.wait();
                panic!("`devices invite --no-relay` non ha stampato il biglietto in 30 s");
            }
        }
        assert!(
            got.contains("engramma-invite-v1."),
            "`devices invite --no-relay` deve stampare un biglietto: {got}"
        );
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(b"\n");
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(saved_choice(&c.root).as_deref(), Some("none"));
}
