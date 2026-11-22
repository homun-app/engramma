
// SPDX-License-Identifier: Apache-2.0

//! CLI plumbing for the v0.2.1 search additions: `--strict` and the
//! `support` field in the JSON output (the ranking behaviour lives in the
//! golden tests, with the real model).

mod common;

use std::process::Command;

#[test]
fn strict_flag_and_support_field_travel_the_cli() {
    let dir = common::test_dir();
    let root = dir.path().join("root");
    let run = |args: &[&str]| -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
            .arg("--root")
            .arg(&root)
            .args(args)
            .output()
            .expect(/* test-only */ "spawn engramma");
        assert!(out.status.success(), "{:?}", out);
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    run(&["init"]);
    // one note, body piped through stdin ("-")
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&root)
        .args([
            "add",
            "-",
            "--source",
            "test",
            "--confidence",
            "stated",
            "--title",
            "Fabio abita a Milano",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect(/* test-only */ "spawn");
    child
        .stdin
        .as_mut()
        .expect(/* test-only */ "stdin")
        .write_all(b"Fabio vive a Milano.")
        .expect(/* test-only */ "write");
    let out = child.wait_with_output().expect(/* test-only */ "wait");
    assert!(out.status.success());

    // --strict on a text search: text hits are strong, nothing is dropped
    let plain = run(&["search", "Fabio Milano", "--json"]);
    let strict = run(&["search", "Fabio Milano", "--json", "--strict"]);
    let a: serde_json::Value = serde_json::from_str(&plain).unwrap(/* t */);
    let b: serde_json::Value = serde_json::from_str(&strict).unwrap(/* t */);
    assert_eq!(
        a["hits"].as_array().map(Vec::len),
        b["hits"].as_array().map(Vec::len)
    );
    assert_eq!(
        a["hits"][0]["explain"]["support"].as_str(),
        Some("strong"),
        "{plain}"
    );
    let human = run(&["search", "Fabio Milano", "--strict"]);
    assert!(human.contains("supporto: strong"), "{human}");
}
