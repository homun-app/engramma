// SPDX-License-Identifier: Apache-2.0

//! The addon's build: napi's setup, plus (v0.6.10, J3) the same commit
//! mark the CLI bakes — the JS bench compares the two and refuses a
//! stale pairing.

use std::process::Command;

fn main() {
    napi_build::setup();
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/main");
    let commit = || -> String {
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let Some(head) = head.filter(|h| h.len() >= 7) else {
            return "unknown".into();
        };
        let dirty = Command::new("git")
            .args(["diff-index", "--quiet", "HEAD", "--"])
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(false);
        if dirty {
            format!("{}-dirty", &head[..7])
        } else {
            head[..7].to_string()
        }
    }();
    println!("cargo:rustc-env=ENGRAMMA_BUILD_COMMIT={commit}");
}
