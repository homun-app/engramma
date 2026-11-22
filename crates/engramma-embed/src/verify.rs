// SPDX-License-Identifier: Apache-2.0

//! sha256 verification of the model files, with a result cache next to them
//! (invalidated by mtime and size — hashing 1.2 GB on every open would be
//! absurd). Uses `ring`'s SHA-256: the crate is already in the tree (rustls,
//! via ureq), so no new dependency enters `Cargo.lock`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use engramma_core::EmbedError;

use crate::spec::ModelSpec;

/// Name of the cache file inside a model folder.
const CACHE_NAME: &str = ".engramma-sha256.json";

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Cache {
    /// `path -> (sha256, mtime_ms, size)`.
    files: BTreeMap<String, (String, u64, u64)>,
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const TAB: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(TAB[(b >> 4) as usize] as char);
        out.push(TAB[(b & 0xf) as usize] as char);
    }
    out
}

/// Hash a whole file with ring's SHA-256.
fn sha256_file(path: &Path) -> Result<String, EmbedError> {
    let file = std::fs::File::open(path)
        .map_err(|e| EmbedError::ModelMissing(format!("{}: {e}", path.display())))?;
    sha256_reader(std::io::BufReader::new(file))
        .map_err(|e| EmbedError::BadModel(format!("{}: {e}", path.display())))
}

/// Stream a file through `update` (used by `pull` to hash while downloading).
///
/// # Errors
/// [`EmbedError::BadModel`] on read failure.
pub fn sha256_reader<R: std::io::Read>(mut r: R) -> Result<String, EmbedError> {
    use ring::digest::{Context, SHA256};
    let mut ctx = Context::new(&SHA256);
    let mut buf = vec![0_u8; 1 << 20];
    loop {
        let n = r
            .read(&mut buf)
            .map_err(|e| EmbedError::BadModel(format!("read: {e}")))?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}

fn mtime_size(path: &Path) -> Result<(u64, u64), EmbedError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| EmbedError::ModelMissing(format!("{}: {e}", path.display())))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok((mtime, meta.len()))
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join(CACHE_NAME)
}

fn load_cache(dir: &Path) -> Cache {
    std::fs::read_to_string(cache_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Verify the two big files of a model against its manifest. Results are
/// cached in `<dir>/.engramma-sha256.json`, invalidated by mtime and size.
///
/// # Errors
/// [`EmbedError::ModelMissing`] when a file is absent;
/// [`EmbedError::BadModel`] when a hash disagrees with the manifest.
pub fn verify_model(root: &Path, spec: &ModelSpec) -> Result<(), EmbedError> {
    let dir = root.join(&spec.id).join(&spec.revision);
    let mut cache = load_cache(&dir);
    let mut changed = false;
    for file in &spec.files {
        let path = spec.file_path(root, file);
        if !path.is_file() {
            return Err(EmbedError::ModelMissing(format!(
                "{} is missing (run `engramma model pull {}`)",
                path.display(),
                spec.id
            )));
        }
        let (mtime, size) = mtime_size(&path)?;
        let cached_ok = cache
            .files
            .get(&file.path)
            .is_some_and(|(h, m, s)| *h == file.sha256 && *m == mtime && *s == size);
        if cached_ok {
            continue;
        }
        let got = sha256_file(&path)?;
        if got != file.sha256 {
            return Err(EmbedError::BadModel(format!(
                "{} fails the sha256 check (expected {}, got {got}): delete the folder \
                 and run `engramma model pull {}` again",
                path.display(),
                file.sha256,
                spec.id
            )));
        }
        cache.files.insert(file.path.clone(), (got, mtime, size));
        changed = true;
    }
    if changed {
        // Best effort: a read-only model folder still works, it just hashes
        // again next time.
        if let Ok(json) = serde_json::to_string(&cache) {
            let _ = std::fs::write(cache_path(&dir), json);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ModelFile;

    #[test]
    fn sha256_known_vector() {
        // sha256("abc")
        let got = sha256_reader("abc".as_bytes()).unwrap(/* test-only */);
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_detects_missing_and_corrupt() {
        let tmp = tempfile::Builder::new().prefix("engramma-test-").tempdir().unwrap(/* test-only */);
        let mut spec = ModelSpec::granite_97m();
        // a tiny stand-in model: the manifest points at a small file
        spec.files = vec![ModelFile {
            path: "onnx/model.onnx".into(),
            sha256: spec.files[1].sha256.clone(),
            bytes: spec.files[1].bytes,
        }];
        let dir = tmp.path().join(&spec.id).join(&spec.revision).join("onnx");
        std::fs::create_dir_all(&dir).unwrap(/* test-only */);
        // missing -> ModelMissing
        assert!(matches!(
            verify_model(tmp.path(), &spec),
            Err(EmbedError::ModelMissing(_))
        ));
        // wrong content -> BadModel
        std::fs::write(dir.join("model.onnx"), b"not the model").unwrap(/* test-only */);
        assert!(matches!(
            verify_model(tmp.path(), &spec),
            Err(EmbedError::BadModel(_))
        ));
        // right content -> ok, and the cache makes the second check instant
        std::fs::write(dir.join("model.onnx"), b"modello").unwrap(/* test-only */);
        spec.files[0].sha256 = sha256_reader("modello".as_bytes()).unwrap(/* test-only */);
        spec.files[0].bytes = 7;
        verify_model(tmp.path(), &spec).unwrap(/* test-only */);
        verify_model(tmp.path(), &spec).unwrap(/* test-only */); // cache hit
        assert!(
            tmp.path()
                .join(&spec.id)
                .join(&spec.revision)
                .join(CACHE_NAME)
                .is_file()
        );
    }
}
