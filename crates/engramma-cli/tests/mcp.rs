// SPDX-License-Identifier: Apache-2.0

//! Integration test for `engramma mcp`: spawn the real binary and speak
//! JSON-RPC over stdio (initialize → tools/list → tools/call), including the
//! check that a cell not named in `--cells` is unreachable even when the tool
//! is called with its name.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};

struct Mcp {
    child: Child,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mcp {
    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect(/* test-only */ "stdin open");
        stdin.write_all(line.as_bytes()).unwrap(/* test-only */);
        stdin.write_all(b"\n").unwrap(/* test-only */);
        stdin.flush().unwrap(/* test-only */);
    }

    /// Read one JSON-RPC response with the given id (skipping notifications).
    fn read_response(&mut self, id: i64) -> serde_json::Value {
        let stdout = self.child.stdout.as_mut().expect(/* test-only */ "stdout");
        let mut reader = BufReader::new(stdout);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for response {id}"
            );
            let mut line = String::new();
            let n = reader.read_line(&mut line).unwrap(/* test-only */);
            assert!(n > 0, "server closed stdout");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(trimmed).unwrap(/* test-only */);
            if v.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
                return v;
            }
        }
    }
}

fn make_cell(dir: &std::path::Path, name: &str, body: &str) -> Note {
    let cell = Cell::create(&dir.join(format!("{name}.sqlite")), name, "test").unwrap(/* t */);
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */);
    let note = Note {
        id: engramma_core::id::new_ulid().unwrap(/* t */),
        note_type: NoteType::Person,
        title: Some(name.to_string()),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: At(t),
        updated_at: At(t),
        extensions: vec![],
    };
    cell.put_note(&note, "test").unwrap(/* t */);
    note
}

fn spawn(root: &std::path::Path, cells: &str) -> Mcp {
    let child = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .args(["mcp", "--root"])
        .arg(root)
        .args(["--cells", cells])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(/* test-only */);
    Mcp { child }
}

fn cell_root(root: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
    root.join("cells")
}

#[test]
fn mcp_stdio_roundtrip_and_isolation() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cells = cell_root(dir.path());
    let note = make_cell(&cells, "famiglia", "Fabio ha traslocato a [[Milano]] con [[Giulia]].");
    // a second cell that is NOT exposed
    make_cell(&cells, "segreta", "Nota riservata su [[Torino]].");

    let mut mcp = spawn(dir.path(), "famiglia");

    // initialize
    mcp.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0"}}}"#);
    let init = mcp.read_response(1);
    assert!(
        init["result"]["serverInfo"]["name"].as_str().is_some_and(|n| n == "engramma"),
        "{init}"
    );
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    // tools/list: exactly the five read-only tools
    mcp.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    let list = mcp.read_response(2);
    let tools: Vec<String> = list["result"]["tools"]
        .as_array()
        .unwrap(/* t */)
        .iter()
        .map(|t| t["name"].as_str().unwrap(/* t */).to_string())
        .collect();
    // tools/list returns them in the router's (alphabetical) order
    let mut sorted = tools.clone();
    sorted.sort();
    assert_eq!(tools, sorted);
    assert_eq!(
        sorted,
        vec![
            "memory_get",
            "memory_neighbors",
            "memory_search",
            "memory_tags",
            "memory_timeline"
        ]
    );

    // tools/call memory_search
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "memory_search",
            "arguments": {"cell": "famiglia", "query": "traslocato"}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(3);
    let text = res["result"]["content"][0]["text"]
        .as_str()
        .unwrap(/* t */);
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap(/* t */);
    let hits = parsed["hits"].as_array().unwrap(/* t */);
    assert!(!hits.is_empty(), "{parsed}");
    assert_eq!(
        hits[0]["note"]["id"].as_str().unwrap(/* t */),
        note.id.to_string()
    );
    assert!(hits[0]["explain"]["source"].as_str().is_some());

    // an unexposed cell is unreachable even when named explicitly
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "memory_search",
            "arguments": {"cell": "segreta", "query": "Torino"}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(4);
    assert!(
        res.get("error").is_some_and(|e| {
            e["message"]
                .as_str()
                .is_some_and(|m| m.contains("not exposed"))
        }),
        "{res}"
    );

    // memory_get by prefix
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "memory_get",
            "arguments": {"cell": "famiglia", "id": note.id.to_string()[..8]}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(5);
    assert!(res["result"]["content"][0]["text"]
        .as_str()
        .unwrap(/* t */)
        .contains("Milano"), "{res}");

    drop(mcp);
}

#[test]
fn mcp_without_cells_refuses() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    cell_root(dir.path());
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .args(["mcp", "--root"])
        .arg(dir.path())
        .output()
        .unwrap(/* test-only */);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--cells"), "{stderr}");
}
