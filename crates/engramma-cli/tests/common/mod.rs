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

// ---------------------------------------------------------------------------
// v0.6.18 (F4): a fake OpenAI-compatible embeddings endpoint
// ---------------------------------------------------------------------------

/// A fake `POST /v1/embeddings` server on the loopback: one tiny thread,
/// deterministic vectors, and a fixed synonym map — so a question with no
/// word in common with the note still points at its meaning. The vectors
/// are bags of words over 8 dimensions; `impiegato`/`impiego`/`impiega`
/// map onto `lavora`.
pub struct FakeEmbedder {
    pub port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for FakeEmbedder {
    fn drop(&mut self) {
        self.stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn fake_embedder_server() -> FakeEmbedder {
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(/* test-only */);
    let port = listener.local_addr().unwrap(/* test-only */).port();
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    std::thread::spawn(move || {
        listener.set_nonblocking(true).ok();
        while !stop2.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // macOS: a socket accepted from a NON-blocking listener
                    // inherits the flag — a read before the client's bytes
                    // arrive would return WouldBlock, which answer_one
                    // would read as EOF, and the client would see the peer
                    // disconnect instead of its embeddings
                    stream.set_nonblocking(false).ok();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                        .ok();
                    answer_one(&mut stream);
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
    });
    FakeEmbedder { port, stop }
}

fn answer_one(stream: &mut std::net::TcpStream) {
    use std::io::{Read as _, Write as _};
    let mut data = Vec::new();
    let mut buf = [0_u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            return;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let body_start = head_end + 4;
    while data.len() < body_start + content_length {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&data[body_start..]).into_owned();
    let answer = embeddings_answer(&body);
    let http = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{answer}",
        answer.len()
    );
    let _ = stream.write_all(http.as_bytes());
    let _ = stream.flush();
}

fn embeddings_answer(body: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let inputs = parsed
        .get("input")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let data: Vec<serde_json::Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let text = v.as_str().unwrap_or_default();
            serde_json::json!({ "index": i, "embedding": fake_vector(text) })
        })
        .collect();
    serde_json::json!({ "object": "list", "data": data }).to_string()
}

fn fake_vector(text: &str) -> Vec<f32> {
    let mut v = [0.0_f32; 8];
    for word in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let word = synonym(word);
        if word.chars().count() < 2 {
            continue;
        }
        let h = word.bytes().fold(5381_u64, |h, b| {
            h.wrapping_mul(33).wrapping_add(u64::from(b))
        });
        v[(h % 8) as usize] += 1.0;
    }
    let mut out = v.to_vec();
    if out.iter().all(|x| *x == 0.0) {
        out.fill(1.0); // an empty text still points somewhere
    }
    let norm = out.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    for x in &mut out {
        *x /= norm;
    }
    out
}

fn synonym(word: &str) -> &str {
    let lower = word.to_lowercase();
    if matches!(lower.as_str(), "impiegato" | "impiego" | "impiega") {
        "lavora"
    } else {
        word
    }
}
