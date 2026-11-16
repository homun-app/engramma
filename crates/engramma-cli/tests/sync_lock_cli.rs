// SPDX-License-Identifier: Apache-2.0

//! S1 (v0.6.2): the exclusive root lock, BETWEEN PROCESSES. The CLI's
//! `sync serve` holds `<root>/sync.lock`; a service in ANOTHER process on
//! the same root answers `SYNC_BUSY` — and the CLI answers its own line
//! and exit code when the lock is someone else's.

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spawn `sync serve` and wait for its first JSON line (the address).
fn serve_json(root: &std::path::Path) -> (std::process::Child, serde_json::Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(root)
        .args(["--json", "sync", "serve", "--no-relay"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(/* t */);
    let out = child.stdout.take().expect(/* piped */ "stdout");
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    std::thread::spawn(move || {
        let mut text = String::new();
        let mut depth = 0_i32;
        for line in std::io::BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            text.push('\n');
            text.push_str(&line);
            for c in line.chars() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            if let Ok(v) = serde_json::from_str(&text) {
                                let _ = tx.send(v);
                            }
                            return;
                        }
                    }
                    _ => {}
                }
            }
        }
    });
    let v = rx
        .recv_timeout(Duration::from_secs(30))
        .expect(/* prints at start */ "the daemon's json");
    (child, v)
}

/// The CLI's serve holds the lock; the library's service, in THIS other
/// process, is told `SYNC_BUSY` — and told so at a single pass too.
#[test]
fn the_cli_serve_and_the_library_service_share_one_lock() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let root = dir.path().join("r");
    std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
    let (mut child, v) = serve_json(&root);
    assert!(v["serving"].as_bool().unwrap_or(false), "the daemon is up");
    // the library, from another process on the same root
    let svc = engramma_sync::service::SyncService::start(&root).unwrap(/* t */);
    assert!(
        matches!(
            svc.serve_start(Some(engramma_sync::relay::RelayChoice::Disabled)),
            Err(engramma_sync::service::ServiceError::Busy)
        ),
        "the CLI's serve owns the root"
    );
    assert!(
        matches!(
            svc.run_once(Some(engramma_sync::relay::RelayChoice::Disabled)),
            Err(engramma_sync::service::ServiceError::Busy)
        ),
        "a single pass takes the same lock"
    );
    let _ = child.kill();
    let _ = child.wait();
    // the daemon is gone: the lock went with it (flock dies with the fd)
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let free = svc
            .run_once(Some(engramma_sync::relay::RelayChoice::Disabled))
            .is_ok();
        if free || Instant::now() > deadline {
            assert!(
                free,
                "the lock outlived the daemon that held it"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The other way around: the library serves, and the CLI's OWN serve
/// answers a dedicated line and a dedicated exit code.
#[test]
fn the_cli_refuses_a_root_the_library_serves() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let root = dir.path().join("r");
    std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
    let svc = engramma_sync::service::SyncService::start(&root).unwrap(/* t */);
    svc.serve_start(Some(engramma_sync::relay::RelayChoice::Disabled))
        .unwrap(/* t */);
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&root)
        .args(["sync", "serve", "--no-relay"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap(/* t */);
    assert!(
        !out.status.success(),
        "a second serve must not start: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("sync.lock") || err.contains("SYNC_BUSY"),
        "a dedicated line names the lock: {err}"
    );
    assert_eq!(
        out.status.code(),
        Some(7),
        "a dedicated exit code (7), not the generic internal"
    );
    svc.serve_stop().unwrap(/* t */);
}
