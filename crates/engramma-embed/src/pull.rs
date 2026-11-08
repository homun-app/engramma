// SPDX-License-Identifier: Apache-2.0

//! `engramma model pull`: download a model's files from a fixed revision and
//! verify each sha256 **before** the temporary file is renamed into place —
//! a wrong hash leaves no residue (DESIGN §7).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use engramma_core::EmbedError;

use crate::spec::ModelSpec;

/// What one `pull` did.
#[derive(Debug, Default, Clone)]
pub struct PullReport {
    /// Files downloaded and verified.
    pub downloaded: Vec<String>,
    /// Files already present with the right hash.
    pub skipped: Vec<String>,
}

/// The default revision host.
pub const HF_BASE: &str = "https://huggingface.co";

/// Download the model's files into `<root>/<id>/<revision>/…`. `base`
/// overrides `https://huggingface.co` (tests point it at a local server).
///
/// # Errors
/// [`EmbedError::Remote`] on transport failures or a non-200 status;
/// [`EmbedError::BadModel`] when the downloaded bytes fail the sha256 check
/// (the partial file is removed).
pub fn pull(
    root: &Path,
    spec: &ModelSpec,
    base: Option<&str>,
    timeout: Duration,
) -> Result<PullReport, EmbedError> {
    // no progress interest: the callback is free when unused
    pull_progress(root, spec, base, timeout, &mut |_, _| {})
}

/// [`pull`] with a progress callback (v0.4, the Node binding): called
/// after every verified byte chunk with `(done_bytes, total_bytes)` over
/// the WHOLE model — already-present files count as done from the start.
/// The revision and the sha256 of each file stay pinned, exactly as `pull`.
///
/// # Errors
/// See [`pull`].
pub fn pull_progress(
    root: &Path,
    spec: &ModelSpec,
    base: Option<&str>,
    timeout: Duration,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PullReport, EmbedError> {
    let base = base.unwrap_or(HF_BASE);
    let mut report = PullReport::default();
    let total: u64 = spec.files.iter().map(|f| f.bytes).sum();
    let mut done: u64 = 0;
    for file in &spec.files {
        let target = spec.file_path(root, file);
        if target.is_file() && verify_one(&target, &file.sha256)? {
            report.skipped.push(file.path.clone());
            done += file.bytes;
            progress(done, total);
            continue;
        }
        let url = format!(
            "{}/{}/resolve/{}/{}",
            base, spec.repo, spec.revision, file.path
        );
        download_verified_progress(&url, &target, &file.sha256, timeout, &mut |n| {
            progress(done + n, total)
        })?;
        report.downloaded.push(file.path.clone());
        done += file.bytes;
        progress(done, total);
    }
    Ok(report)
}

/// Hash `path` and compare (no cache: this is the rare, explicit pull).
fn verify_one(path: &Path, sha256: &str) -> Result<bool, EmbedError> {
    let f = std::fs::File::open(path)
        .map_err(|e| EmbedError::BadModel(format!("{}: {e}", path.display())))?;
    let got = crate::verify::sha256_reader(std::io::BufReader::new(f))?;
    Ok(got == sha256)
}

/// Stream `url` into a sibling temporary file, hashing while downloading;
/// rename over `target` only when the hash matches. On any failure the
/// temporary file is removed — nothing partial is ever left behind.
/// Stream `url` into a sibling temporary file, hashing while downloading
/// and reporting the downloaded-so-far bytes; rename over `target` only
/// when the hash matches. On any failure the temporary file is removed —
/// nothing partial is ever left behind.
fn download_verified_progress(
    url: &str,
    target: &Path,
    sha256: &str,
    timeout: Duration,
    progress: &mut dyn FnMut(u64),
) -> Result<(), EmbedError> {
    use ring::digest::{Context, SHA256};

    let Some(parent) = target.parent() else {
        return Err(EmbedError::BadModel(format!(
            "{} has no parent directory",
            target.display()
        )));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| EmbedError::BadModel(format!("{}: {e}", parent.display())))?;
    let temp: PathBuf = parent.join(format!(
        ".{}.part-{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("model"),
        std::process::id()
    ));
    let cleanup = |t: &Path| {
        let _ = std::fs::remove_file(t);
    };

    let response = ureq::get(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|e| EmbedError::Remote(format!("{url}: {e}")))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(EmbedError::Remote(format!(
            "{url} answered {status} (the revision or file does not exist?)"
        )));
    }
    let mut reader = response.into_body().into_reader();
    let mut file = match std::fs::File::create(&temp) {
        Ok(f) => f,
        Err(e) => {
            cleanup(&temp);
            return Err(EmbedError::BadModel(format!("{}: {e}", temp.display())));
        }
    };
    let mut ctx = Context::new(&SHA256);
    let mut buf = vec![0_u8; 1 << 20];
    let mut so_far: u64 = 0;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                cleanup(&temp);
                return Err(EmbedError::Remote(format!(
                    "{url}: download interrupted: {e}"
                )));
            }
        };
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
        if let Err(e) = file.write_all(&buf[..n]) {
            cleanup(&temp);
            return Err(EmbedError::BadModel(format!("{}: {e}", temp.display())));
        }
        so_far += n as u64;
        progress(so_far);
    }
    let got = crate::verify::hex(ctx.finish().as_ref());
    if got != sha256 {
        cleanup(&temp);
        return Err(EmbedError::BadModel(format!(
            "{url}: sha256 mismatch (expected {sha256}, got {got}); the file was discarded"
        )));
    }
    if let Err(e) = std::fs::rename(&temp, target) {
        cleanup(&temp);
        return Err(EmbedError::BadModel(format!(
            "renaming into {}: {e}",
            target.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ModelFile;
    use crate::testserver::FakeServer;

    fn spec_with(sha256: String) -> ModelSpec {
        let mut spec = ModelSpec::granite_97m();
        spec.repo = "test/repo".into();
        spec.files = vec![ModelFile {
            path: "tokenizer.json".into(),
            sha256,
            bytes: 5,
        }];
        spec
    }

    #[test]
    fn downloads_verifies_and_skips_when_present() {
        let payload = "hello";
        let sha = crate::verify::sha256_reader(payload.as_bytes()).unwrap(/* test-only */);
        let server = FakeServer::start(|req| {
            assert!(req.path.starts_with("/test/repo/resolve/"), "{}", req.path);
            (200, payload.to_string())
        });
        let tmp = tempfile::tempdir().unwrap(/* test-only */);
        let spec = spec_with(sha);
        let report = pull(tmp.path(), &spec, Some(&server.url()), Duration::from_secs(5))
            .unwrap(/* test-only */);
        assert_eq!(report.downloaded, vec!["tokenizer.json".to_string()]);
        let file = spec.file_path(tmp.path(), &spec.files[0]);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(/* test-only */),
            payload
        );
        // no partial residue
        let leftovers: Vec<_> = std::fs::read_dir(file.parent().unwrap(/* test-only */))
            .unwrap(/* test-only */)
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".part"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // a second pull skips: the file is there and verifies
        let report2 = pull(tmp.path(), &spec, Some(&server.url()), Duration::from_secs(5))
            .unwrap(/* test-only */);
        assert_eq!(report2.skipped, vec!["tokenizer.json".to_string()]);
        assert_eq!(
            server.requests().len(),
            1,
            "the second pull hit the network"
        );
    }

    #[test]
    fn wrong_sha256_leaves_no_residue() {
        let server = FakeServer::start(|_| (200, "totally not the model".to_string()));
        let tmp = tempfile::tempdir().unwrap(/* test-only */);
        let spec = spec_with("00".repeat(32));
        let err = pull(tmp.path(), &spec, Some(&server.url()), Duration::from_secs(5))
            .unwrap_err(/* test-only */);
        assert!(matches!(err, EmbedError::BadModel(_)), "{err}");
        assert!(err.to_string().contains("sha256"), "{err}");
        let dir = tmp.path().join(&spec.id).join(&spec.revision);
        // nothing landed, nothing partial stayed behind
        assert!(!dir.join("tokenizer.json").exists());
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap(/* test-only */)
            .flatten()
            .collect();
        assert!(entries.is_empty(), "residue: {entries:?}");
    }

    #[test]
    fn http_404_is_a_remote_error() {
        let server = FakeServer::start(|_| (404, "gone".to_string()));
        let tmp = tempfile::tempdir().unwrap(/* test-only */);
        let spec = spec_with("00".repeat(32));
        let err = pull(tmp.path(), &spec, Some(&server.url()), Duration::from_secs(5))
            .unwrap_err(/* test-only */);
        assert!(matches!(err, EmbedError::Remote(_)), "{err}");
        assert!(err.to_string().contains("404"), "{err}");
    }
}
