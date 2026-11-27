// SPDX-License-Identifier: Apache-2.0

//! v0.6.17 (F1): the released version is a promise — the installer, the
//! GitHub Release and the docs all say `0.6.20-beta.1`, and the binary
//! must say the same. The test hardcodes the string on purpose: a bump
//! that forgets a manifest turns the suite red instead of shipping a
//! binary that answers with the workspace's old placeholder.

use std::process::Command;

#[test]
fn version_prints_the_released_number() {
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--version")
        .output()
        .expect(/* test-only */ "the binary runs");
    assert!(out.status.success(), "`--version` exits 0");
    let said = String::from_utf8_lossy(&out.stdout);
    // v0.6.10 (J3): the long version keeps going with the commit and the
    // supported schema — the promise is the FIRST line's beginning.
    assert!(
        said.starts_with("engramma 0.6.20-beta.1"),
        "the binary says who it is: {said}"
    );
}
