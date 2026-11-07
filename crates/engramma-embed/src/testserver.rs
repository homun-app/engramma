// SPDX-License-Identifier: Apache-2.0

//! A tiny fake HTTP server on 127.0.0.1 for the embedder tests: no network
//! beyond the loopback interface, no external crate. It records every
//! request and answers through a closure.

#![cfg(test)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One recorded request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Lowercased `name: value` lines.
    pub headers: Vec<String>,
    pub body: String,
}

/// A running fake server.
pub struct FakeServer {
    pub port: u16,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    /// Stop signal: set when the accept loop should wind down.
    stop: Arc<Mutex<bool>>,
    _stop_rx: Receiver<()>,
}

impl FakeServer {
    /// Start a server that answers each request with `(status, body)` from
    /// `respond`. The closure may sleep (timeout tests) or panic (the thread
    /// dies, the client sees a connection error).
    pub fn start<F>(respond: F) -> FakeServer
    where
        F: Fn(&Recorded) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect(/* test-only */ "bind");
        let port = listener.local_addr().expect(/* test-only */ "addr").port();
        let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Mutex::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let recorded2 = Arc::clone(&recorded);
        let stop2 = Arc::clone(&stop);
        std::thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect(/* test-only */ "nonblocking");
            loop {
                if *stop2.lock().unwrap_or_else(|p| p.into_inner()) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        // macOS/BSD: the socket returned by `accept`
                        // INHERITS O_NONBLOCK from the listener (Linux does
                        // not). Without the reset, a request still in flight
                        // makes `handle`'s first read answer WouldBlock,
                        // which was treated as an error: the connection
                        // closed with no response (the ~1-in-10 flake of
                        // the bearer-token and download tests; the slow
                        // client test makes the window deterministic).
                        let _ = stream.set_nonblocking(false);
                        let respond = &respond;
                        handle(stream, respond, &recorded2);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(());
        });
        FakeServer {
            port,
            recorded,
            stop,
            _stop_rx: rx,
        }
    }

    /// The base URL (`http://127.0.0.1:<port>`).
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request seen so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.recorded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Ok(mut s) = self.stop.lock() {
            *s = true;
        }
        // Give the loop a beat to notice; the thread ends with the process.
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Read one request, answer, close. Never panics across the connection.
fn handle<F>(mut stream: TcpStream, respond: F, recorded: &Mutex<Vec<Recorded>>)
where
    F: Fn(&Recorded) -> (u16, String),
{
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    // headers
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_header_end(&buf) {
                    break pos;
                }
            }
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut method = String::new();
    let mut path = String::new();
    if let Some(first) = request_line.split_whitespace().next() {
        method = first.to_string();
    }
    if let Some(second) = request_line.split_whitespace().nth(1) {
        path = second.to_string();
    }
    let mut headers = Vec::new();
    let mut content_length = 0_usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push(format!("{k}: {v}"));
        }
    }
    // body
    let mut body_bytes = buf[header_end + 4..].to_vec();
    while body_bytes.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body_bytes.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let body = String::from_utf8_lossy(&body_bytes).to_string();
    let record = Recorded {
        method,
        path,
        headers,
        body,
    };
    if let Ok(mut r) = recorded.lock() {
        r.push(record.clone());
    }
    let (status, payload) = respond(&record);
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// macOS/BSD: the socket returned by `accept` inherits O_NONBLOCK from the
/// listener. A client that hesitates between connect and send makes the
/// window deterministic: without the reset, `handle`'s first read answers
/// WouldBlock, the connection dies with no response — the ~1-in-10 flake
/// of the bearer-token and download tests, made reproducible.
#[test]
fn a_slow_client_still_gets_its_response() {
    let server = FakeServer::start(|_| (200, "{\"data\":[]}".into()));
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", server.port))
        .expect(/* test-only */ "connect");
    // the request is LATE: 150 ms after the connection, the server has
    // already accepted and is reading
    std::thread::sleep(Duration::from_millis(150));
    let req = b"GET /v1/embeddings HTTP/1.1\r\nHost: x\r\n\r\n";
    s.write_all(req).expect(/* test-only */ "send");
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect(/* test-only */ "timeout");
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let answer = String::from_utf8_lossy(&buf);
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "the slow client must still get its answer: {answer}"
    );
    assert_eq!(server.requests().len(), 1);
}
