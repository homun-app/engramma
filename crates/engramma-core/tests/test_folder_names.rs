// SPDX-License-Identifier: Apache-2.0

//! v0.6.13 (M4): every temporary folder of the suite's tests carries
//! OUR name. The eight `.tmp*` of the 22nd of November were
//! attributable to no one because only the core's tests used the
//! prefix; this test walks the sources of EVERY crate — the
//! integration tests and the `#[cfg(test)]` modules inside `src` —
//! and fails at the first bare `tempfile::tempdir()` or
//! `TempDir::new()`: a folder without our prefix is a leftover no
//! sweeper may ever touch, and no future test may create one. (The
//! ONE declared foreign folder — the sweeper's own fixture in
//! `props.rs`, built with `Builder` and the `.tmp` prefix on purpose —
//! is not a bare call, and stays legal.)

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sources_under(root: &Path, dir: &str, out: &mut Vec<PathBuf>) {
    // every crate's `dir` folder (tests or src), under root/crates/*
    let Ok(crates) = root.join("crates").read_dir() else {
        return;
    };
    for c in crates.flatten() {
        walk(&c.path().join(dir), out);
    }
}

fn walk(base: &Path, out: &mut Vec<PathBuf>) {
    let mut stack = vec![base.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = d.read_dir() else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
}

#[test]
fn no_test_makes_a_folder_without_our_name() {
    let root = workspace_root();
    let mut sources = Vec::new();
    sources_under(&root, "tests", &mut sources);
    sources_under(&root, "src", &mut sources);
    assert!(
        !sources.is_empty(),
        "the walk found the workspace's sources"
    );
    let mut offenders: Vec<String> = Vec::new();
    for f in &sources {
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        // this test's own doc names the calls it forbids: it is not an
        // offender for quoting them
        if f.ends_with("test_folder_names.rs") {
            continue;
        }
        if src.contains("tempfile::tempdir()") || src.contains("TempDir::new()") {
            offenders.push(
                f.strip_prefix(&root)
                    .unwrap_or(f)
                    .display()
                    .to_string(),
            );
        }
    }
    assert!(
        offenders.is_empty(),
        "bare temp-folder calls (no `engramma-test-` prefix, no sweeper can ever \
         attribute their leftovers): {offenders:?} — use the crate's \
         `common::test_dir()` (or a Builder with our prefix, in the src \
         test modules)"
    );
}
