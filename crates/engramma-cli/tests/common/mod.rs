// SPDX-License-Identifier: Apache-2.0

//! v0.6.13 (M4): the shared test stage — a folder with OUR name, so a
//! killed run's leftovers can be told from every other process's and
//! swept (the eight `.tmp*` of the 22nd of November were attributable
//! to no one). The same help the core's suite already has.

#![allow(dead_code)]

/// A test folder named `engramma-test-*`.
pub fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

/// Sweep the STALE test folders a killed run left behind: ours by
/// name, older than six hours. A fresh folder survives (another test
/// of this run may hold it), and a foreign folder is never touched.
pub fn sweep_stale_test_dirs() -> usize {
    let tmp = std::env::temp_dir();
    let threshold = std::time::Duration::from_secs(6 * 3600);
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(&tmp) else {
        return 0;
    };
    let mut swept = 0;
    for e in entries.flatten() {
        let p = e.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("engramma-test-") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let modified = meta.modified().unwrap_or(now);
        if now.duration_since(modified).unwrap_or_default() <= threshold {
            continue;
        }
        if std::fs::remove_dir_all(&p).is_ok() {
            swept += 1;
        }
    }
    swept
}

#[cfg(test)]
mod sweep_gate {
    /// v0.6.13 (M4): the sweeper rides at the start of the suite —
    /// whatever a killed run left behind goes, before the new folders
    /// multiply.
    #[test]
    fn stale_test_dirs_are_swept_at_suite_start() {
        super::sweep_stale_test_dirs();
    }
}
