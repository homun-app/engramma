// SPDX-License-Identifier: Apache-2.0

//! v0.6.10 (J3): the binary says WHICH sources built it. The commit (and
//! the `-dirty` mark when the tree was not clean) is baked at compile
//! time, next to the schema the binary supports, so a test bench — or a
//! human — can refuse a stale binary instead of measuring it. The same
//! env is baked into the Node addon (its own build.rs): the bench
//! compares the two and refuses to start when they disagree.

use std::process::Command;

fn main() {
    // rebuild when the commit moves: HEAD covers checkouts, the ref file
    // covers new commits on the same branch (a path that does not exist
    // — a tarball build — simply never triggers, and the env falls back
    // to "unknown")
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
    // the schema the binary supports, read from the one source of truth
    // (core's schema.rs — the same line the code's own const comes
    // from; a missing or moved line fails the build, not the silence)
    let schema_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engramma-core/src/cell/schema.rs");
    let src = std::fs::read_to_string(&schema_src)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", schema_src.display()));
    let schema = src
        .lines()
        .find_map(|l| l.trim().strip_prefix("pub const SCHEMA_VERSION: i64 = "))
        .and_then(|rest| rest.trim_end_matches(';').trim().parse::<i64>().ok())
        .unwrap_or_else(|| panic!("SCHEMA_VERSION not found in {}", schema_src.display()));
    println!("cargo:rustc-env=ENGRAMMA_SCHEMA={schema}");
}
