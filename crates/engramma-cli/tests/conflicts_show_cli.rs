// SPDX-License-Identifier: Apache-2.0

//! v0.6.5 (E5): `engramma conflicts show <cella> <copia>` — the read of
//! what the user is choosing, quarantined copies included, which `get`
//! can never show. The scene is built with the core's own API (two
//! devices, concurrent body writes, the loser's copy), then the CLI
//! reads it.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_engramma"))
}

/// A child process that DIES with the test, even on a panic.
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

fn run(root: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let mut guard = ChildGuard {
        child: Some(
            bin()
                .arg("--root")
                .arg(root)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect(/* the binary exists */ "spawn engramma"),
        ),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match guard
            .child
            .as_mut()
            .expect(/* just set */ "child")
            .try_wait()
            .expect(/* child */ "wait")
        {
            Some(_) => {
                let out = guard
                    .child
                    .take()
                    .expect(/* set */ "child")
                    .wait_with_output();
                let (ok, stdout, stderr) = match out {
                    Ok(out) => (
                        out.status.success(),
                        String::from_utf8_lossy(&out.stdout).into_owned(),
                        String::from_utf8_lossy(&out.stderr).into_owned(),
                    ),
                    Err(e) => (false, String::new(), e.to_string()),
                };
                return (ok, stdout, stderr);
            }
            None if std::time::Instant::now() > deadline => {
                panic!("`engramma {}` non è finito in 60 s", args.join(" "))
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// One note shape, for the scene.
fn note(id: u16, body: &str) -> engramma_core::format::Note {
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* test-only */);
    use engramma_core::format::{At, NoteType};
    engramma_core::format::Note {
        id: ulid::Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some(format!("nota {id}")),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: "user".into(),
        confidence: engramma_core::Confidence::Stated,
        status: engramma_core::Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: At(t),
        updated_at: At(t),
        extensions: vec![],
    }
}

/// Two cells in one root that exchange their operations by hand (the
/// CLI's conflict scene without the network).
fn sync(a: &engramma_core::Cell, b: &engramma_core::Cell) {
    let mut ops = a.ops().unwrap(/* test-only */);
    ops.extend(b.ops().unwrap(/* test-only */));
    ops.sort_by(|x, y| x.hlc.cmp(&y.hlc).then(x.op_id.cmp(&y.op_id)));
    let mut seen = std::collections::BTreeMap::new();
    for op in ops {
        seen.insert(op.op_id.to_string(), op);
    }
    for op in seen.values() {
        let _ = a.ingest_op(op);
        let _ = b.ingest_op(op);
    }
}

#[test]
fn conflicts_show_reads_the_quarantined_copy() {
    let root = common::test_dir();
    std::fs::create_dir(root.path().join("cells")).unwrap(/* t */);
    let a = engramma_core::Cell::create(
        &root.path().join("cells").join("a.sqlite"),
        "memoria",
        "user",
    )
    .unwrap(/* test-only */);
    let n = note(1, "uno\ndue\ntre");
    let id = n.id;
    a.put_note(&n, "user").unwrap(/* test-only */);
    let b =
        engramma_core::Cell::open_bare(&root.path().join("cells").join("b.sqlite")).unwrap(/* t */);
    for op in a.ops().unwrap(/* t */) {
        b.ingest_op(&op).unwrap(/* t */);
    }
    // the human hand (older) and the agent, concurrent
    a.patch_note(
        &id,
        &engramma_core::NotePatch {
            body: Some("la mano umana al lavoro".into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap(/* t */);
    b.patch_note(
        &id,
        &engramma_core::NotePatch {
            body: Some("uno\nX\ntre".into()),
            ..Default::default()
        },
        "homun:bot-1",
    )
    .unwrap(/* t */);
    sync(&a, &b);
    let copies = a.conflicts().unwrap(/* t */);
    assert_eq!(copies.len(), 1, "the agent's copy is open");
    let copy_id = copies[0].copy_id.to_string();

    // the CLI reads it: the original's body, the copy's body, the fork's
    let (ok, out, err) = run(root.path(), &["--json", "conflicts", "show", "a", &copy_id]);
    assert!(ok, "conflicts show failed: {err}");
    let v: serde_json::Value =
        serde_json::from_str(&out).expect(/* json */ "the CLI answers json");
    assert_eq!(v["conflict"]["copyId"], serde_json::json!(copy_id));
    assert_eq!(
        v["conflict"]["originalBody"],
        serde_json::json!("la mano umana al lavoro")
    );
    assert_eq!(v["conflict"]["copyBody"], serde_json::json!("uno\nX\ntre"));
    assert_eq!(
        v["conflict"]["forkBody"],
        serde_json::json!("uno\ndue\ntre")
    );
    assert_eq!(v["conflict"]["quarantined"], serde_json::json!(true));

    // an unknown copy answers NOT_FOUND
    let (ok, out, err) = run(
        root.path(),
        &["conflicts", "show", "a", "01AAAAAAAAAAAAAAAAAAAAAAAA"],
    );
    assert!(!ok, "an unknown copy must not be found: {out}");
    assert!(
        err.contains("is not an open conflict copy"),
        "the error says not-found: {err}"
    );

    // a CLOSED copy answers NOT_FOUND: resolve it, then read it again
    a.resolve_conflict(
        &ulid::Ulid::from_string(&copy_id).unwrap(/* t */),
        engramma_core::ResolveKeep::Original,
        None,
        "user",
    )
    .unwrap(/* t */);
    sync(&a, &b);
    let (ok, out, err) = run(root.path(), &["--json", "conflicts", "show", "a", &copy_id]);
    assert!(!ok, "a closed copy must not be found: {out}");
    assert!(
        err.contains("is not an open conflict copy"),
        "the error says not-found: {err}"
    );
}
