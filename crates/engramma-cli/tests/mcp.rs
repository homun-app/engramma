// SPDX-License-Identifier: Apache-2.0

//! Integration test for `engramma mcp`: spawn the real binary and speak
//! JSON-RPC over stdio (initialize → tools/list → tools/call), including the
//! check that a cell not named in `--cells` is unreachable even when the tool
//! is called with its name.

mod common;

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
        let stdin = self
            .child
            .stdin
            .as_mut()
            .expect(/* test-only */ "stdin open");
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

fn plain_note(title: &str, body: &str) -> Note {
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */);
    Note {
        id: engramma_core::id::new_ulid().unwrap(/* t */),
        note_type: NoteType::Person,
        title: Some(title.to_string()),
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
    }
}

fn make_cell(dir: &std::path::Path, name: &str, body: &str) -> Note {
    let cell = Cell::create(&dir.join(format!("{name}.sqlite")), name, "test").unwrap(/* t */);
    let note = plain_note(name, body);
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

/// v0.6.18 (F4): the write surface — `--write <cell> --actor <name>` — for
/// the tests of the auto reviewer.
impl Mcp {
    fn spawn_with_write(root: &std::path::Path, cells: &str, actor: &str) -> Mcp {
        let child = Command::new(env!("CARGO_BIN_EXE_engramma"))
            .args(["mcp", "--root"])
            .arg(root)
            .args(["--cells", cells, "--write", cells, "--actor", actor])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(/* test-only */);
        Mcp { child }
    }
}

fn cell_root(root: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(root.join("cells")).unwrap(/* t */);
    root.join("cells")
}

#[test]
fn mcp_stdio_roundtrip_and_isolation() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    let note = make_cell(
        &cells,
        "famiglia",
        "Fabio ha traslocato a [[Milano]] con [[Giulia]].",
    );
    // a quarantined web rumour about Fabio: invisible to the whole server
    let rumour = {
        let cell = Cell::open(&cells.join("famiglia.sqlite")).unwrap(/* t */);
        let mut n = plain_note(
            "voce dal web",
            "Si dice che [[Fabio]] sia campione di crossfit.",
        );
        n.confidence = Confidence::External;
        n.status = Status::Quarantined;
        n.source = "import:web".into();
        cell.put_note(&n, "test").unwrap(/* t */);
        n
    };
    // a second cell that is NOT exposed
    make_cell(&cells, "segreta", "Nota riservata su [[Torino]].");

    let mut mcp = spawn(dir.path(), "famiglia");

    // initialize
    mcp.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0"}}}"#);
    let init = mcp.read_response(1);
    assert!(
        init["result"]["serverInfo"]["name"]
            .as_str()
            .is_some_and(|n| n == "engramma"),
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
    // v0.2.1: every search hit carries support ("strong" here: a text match
    // confirms it); the tool description tells the model what "weak" means
    assert_eq!(hits[0]["explain"]["support"].as_str(), Some("strong"));
    let search_desc = list["result"]["tools"]
        .as_array()
        .unwrap(/* t */)
        .iter()
        .find(|t| t["name"] == "memory_search")
        .and_then(|t| t["description"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        search_desc.contains("weak") && search_desc.contains("verify"),
        "the description must explain the weak signal: {search_desc}"
    );

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

    // memory_get by id (full id: two notes created in the same millisecond
    // share the time prefix — see the short-id note in REPORT v0.1.1)
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "memory_get",
            "arguments": {"cell": "famiglia", "id": note.id.to_string()}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(5);
    assert!(
        res["result"]["content"][0]["text"]
            .as_str()
            .unwrap(/* t */)
            .contains("Milano"),
        "{res}"
    );

    // quarantine: the rumour neither seeds the search nor appears as a
    // neighbour of Fabio
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "memory_search",
            "arguments": {"cell": "famiglia", "query": "campione crossfit"}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(6);
    let text = res["result"]["content"][0]["text"]
        .as_str()
        .unwrap(/* t */);
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap(/* t */);
    assert!(
        parsed["hits"].as_array().is_some_and(|h| h.is_empty()),
        "quarantine leaked through MCP search: {parsed}"
    );

    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "memory_neighbors",
            "arguments": {"cell": "famiglia", "entity": "Fabio", "depth": 1}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(7);
    let text = res["result"]["content"][0]["text"]
        .as_str()
        .unwrap(/* t */);
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap(/* t */);
    assert!(
        !text.contains(&rumour.id.to_string()),
        "quarantined edge in neighbors: {parsed}"
    );

    // memory_get on the quarantined note answers exactly like a missing note
    for (id_req, id) in [
        (8, rumour.id.to_string()),
        (9, "01AAAAAAAAAAAAAAAAAAAAAAAA".to_string()),
    ] {
        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id_req,
            "method": "tools/call",
            "params": {
                "name": "memory_get",
                "arguments": {"cell": "famiglia", "id": id}
            }
        });
        mcp.send(&call.to_string());
        let res = mcp.read_response(id_req);
        let msg = res["error"]["message"]
            .as_str()
            .or(res["result"]["isError"]
                .as_bool()
                .and_then(|_| res["result"]["content"][0]["text"].as_str()))
            .unwrap_or_default();
        assert!(
            res.get("error").is_some() && msg.contains("not found"),
            "quarantined/missing note must be an indistinguishable error: {res}"
        );
    }

    // ...and through a short id too: resolving a prefix/suffix must neither
    // reveal the hidden note's full id nor answer differently from a
    // fragment that matches nothing
    let rumour_full = rumour.id.to_string();
    let hidden_suffix = rumour_full[18..].to_string();
    let mut answers: Vec<String> = Vec::new();
    for (id_req, fragment) in [(10, hidden_suffix.clone()), (11, "ZZZZZZZZ".to_string())] {
        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id_req,
            "method": "tools/call",
            "params": {
                "name": "memory_get",
                "arguments": {"cell": "famiglia", "id": fragment}
            }
        });
        mcp.send(&call.to_string());
        let res = mcp.read_response(id_req);
        let raw = res.to_string();
        assert!(
            !raw.contains(&rumour_full),
            "short-id resolution revealed the hidden note's full id: {raw}"
        );
        answers.push(
            res["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .replace(&fragment, "<id>"),
        );
    }
    assert!(!answers[0].is_empty(), "expected an error: {answers:?}");
    assert_eq!(
        answers[0], answers[1],
        "a hidden note's fragment must answer exactly like an unknown fragment"
    );

    drop(mcp);
}

#[test]
fn mcp_single_cell_is_the_default() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    make_cell(
        &cells,
        "famiglia",
        "Fabio ha traslocato a [[Milano]] con [[Giulia]].",
    );
    let mut mcp = spawn(dir.path(), "famiglia");
    mcp.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0"}}}"#);
    let _ = mcp.read_response(1);
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    // no `cell` argument: with one exposed cell it is the default
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "memory_search",
            "arguments": {"query": "traslocato"}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(2);
    let text = res["result"]["content"][0]["text"]
        .as_str()
        .unwrap(/* t */);
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap(/* t */);
    assert!(
        parsed["hits"].as_array().is_some_and(|h| !h.is_empty()),
        "single-cell server must accept calls without `cell`: {parsed}"
    );

    // memory_get and memory_timeline too
    for (id_req, tool, args) in [
        (3, "memory_timeline", serde_json::json!({"entity": "Fabio"})),
        (4, "memory_tags", serde_json::json!({"prefix": ""})),
    ] {
        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id_req,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        });
        mcp.send(&call.to_string());
        let res = mcp.read_response(id_req);
        assert!(res.get("error").is_none(), "{tool} without cell: {res}");
    }
    drop(mcp);

    // with two exposed cells and no `cell`, the server must ask
    make_cell(&cells, "lavoro", "Nota di lavoro su [[Torino]].");
    let mut mcp = spawn(dir.path(), "famiglia,lavoro");
    mcp.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0"}}}"#);
    let _ = mcp.read_response(1);
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "memory_search",
            "arguments": {"query": "traslocato"}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(5);
    assert!(
        res.get("error").is_some_and(|e| {
            e["message"]
                .as_str()
                .is_some_and(|m| m.contains("which cell"))
        }),
        "two cells and no argument must be an error naming them: {res}"
    );
}

#[test]
fn mcp_without_cells_refuses() {
    let dir = common::test_dir();
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

// ---------------------------------------------------------------------------
// v0.5 (P3): the write surface (`--write <cell> --actor <name>`)
// ---------------------------------------------------------------------------

fn spawn_writing(root: &std::path::Path, cells: &str, write: &str, actor: &str) -> Mcp {
    let child = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .args(["mcp", "--root"])
        .arg(root)
        .args(["--cells", cells])
        .args(["--write", write])
        .args(["--actor", actor])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(/* test-only */);
    Mcp { child }
}

impl Mcp {
    /// Answer a server-initiated elicitation with `(action, text)`.
    fn answer_elicitation(&mut self, action: &str, text: &str, shows: &str) {
        // read lines until the server asks; the request has a `method` and
        // an `id` but no `result`
        let stdout = self.child.stdout.as_mut().expect(/* test-only */ "stdout");
        let mut reader = BufReader::new(stdout);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the elicitation request"
            );
            let mut line = String::new();
            let n = reader.read_line(&mut line).unwrap(/* test-only */);
            assert!(n > 0, "server closed stdout");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(trimmed).unwrap(/* t */);
            if v.get("method").and_then(|m| m.as_str()) == Some("elicitation/create") {
                let id = v.get("id").cloned().unwrap_or(serde_json::json!(1));
                let params = &v["params"];
                assert!(
                    params["message"]
                        .as_str()
                        .is_some_and(|m| m.contains(shows)),
                    "the form shows the proposal: {params}"
                );
                assert!(
                    params["requestedSchema"]["properties"]["text"].is_object(),
                    "the editable text field: {params}"
                );
                // the field is PRE-FILLED with the proposal: the human edits,
                // never retypes (v0.5 verification)
                assert!(
                    params["requestedSchema"]["properties"]["text"]["default"]
                        .as_str()
                        .is_some_and(|d| d.contains(shows)),
                    "the text field carries the proposal as its default: {params}"
                );
                let answer = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "action": action,
                        "content": { "text": text }
                    }
                });
                self.send(&answer.to_string());
                return;
            }
        }
    }
}

fn open_with_policy(cells_dir: &std::path::Path, name: &str, policy: serde_json::Value) {
    let cell = Cell::open(&cells_dir.join(format!("{name}.sqlite"))).unwrap(/* t */);
    cell.set_policy(policy, "test").unwrap(/* t */);
}

fn init(mcp: &mut Mcp, id: i64, protocol: &str, elicitation: bool) -> serde_json::Value {
    let capabilities = if elicitation {
        serde_json::json!({ "elicitation": {} })
    } else {
        serde_json::json!({})
    };
    mcp.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {
                "protocolVersion": protocol,
                "capabilities": capabilities,
                "clientInfo": { "name": "test-client", "version": "9.9" }
            }
        })
        .to_string(),
    );
    let init = mcp.read_response(id);
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    init
}

fn propose_call(id: i64, cell: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "memory_propose", "arguments": { "cell": cell, "text": text } }
    })
}

fn tool_payload(res: &serde_json::Value) -> serde_json::Value {
    let text = res["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    serde_json::from_str(text).unwrap_or(serde_json::Value::Null)
}

#[test]
fn the_tool_list_is_exact_with_and_without_write() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    make_cell(&cells, "scrivibile", "Fabio nuota in piscina.");

    // without --write: the five read tools, annotated read-only
    let mut mcp = spawn(dir.path(), "scrivibile");
    init(&mut mcp, 1, "2025-06-18", false);
    mcp.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    let list = mcp.read_response(2);
    let mut tools: Vec<String> = list["result"]["tools"]
        .as_array()
        .unwrap(/* t */)
        .iter()
        .map(|t| t["name"].as_str().unwrap(/* t */).to_string())
        .collect();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "memory_get",
            "memory_neighbors",
            "memory_search",
            "memory_tags",
            "memory_timeline"
        ]
    );
    for t in list["result"]["tools"].as_array().unwrap(/* t */) {
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
    }

    // with --write: memory_propose joins, and only it writes
    let mut mcp = spawn_writing(dir.path(), "scrivibile", "scrivibile", "claude-desktop");
    init(&mut mcp, 1, "2025-06-18", false);
    mcp.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    let list = mcp.read_response(2);
    let mut tools: Vec<String> = list["result"]["tools"]
        .as_array()
        .unwrap(/* t */)
        .iter()
        .map(|t| t["name"].as_str().unwrap(/* t */).to_string())
        .collect();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "memory_get",
            "memory_neighbors",
            "memory_propose",
            "memory_search",
            "memory_tags",
            "memory_timeline"
        ]
    );
    for t in list["result"]["tools"].as_array().unwrap(/* t */) {
        let expected = t["name"] == serde_json::json!("memory_propose");
        assert_eq!(t["annotations"]["readOnlyHint"], !expected, "{t}");
        if expected {
            assert_eq!(t["annotations"]["destructiveHint"], false, "{t}");
            assert_eq!(t["annotations"]["openWorldHint"], false, "{t}");
        }
    }
    // the instructions tell the assistant the courtesy protocol
    let _ = init; // (the instructions are on initialize; checked below)
}

#[test]
fn a_proposal_lands_in_the_queue_and_a_duplicate_is_refused() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    make_cell(&cells, "mem", "Fabio nuota in piscina.");
    open_with_policy(&cells, "mem", serde_json::json!({ "write": "propose" }));

    let mut mcp = spawn_writing(dir.path(), "mem", "mem", "claude-desktop");
    let info = init(&mut mcp, 1, "2025-06-18", false);
    // the instructions carry the proposing etiquette
    let instructions = info["result"]["instructions"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        instructions.contains("ASK the user first"),
        "{instructions}"
    );
    assert!(instructions.contains("NEVER propose"), "{instructions}");
    assert!(
        instructions.contains("mcp:claude-desktop"),
        "{instructions}"
    );

    mcp.send(&propose_call(2, "mem", "Fabio corre la mattina.").to_string());
    let res = mcp.read_response(2);
    let payload = tool_payload(&res);
    assert_eq!(payload["status"], "pending", "{payload}");
    assert!(payload["id"].as_str().is_some(), "{payload}");

    // it is in the queue, quarantined, signed by the actor
    let cell = Cell::open(&cells.join("mem.sqlite")).unwrap(/* t */);
    let queue = engramma_core::cell::proposals::ProposalsFilter::default();
    let rows = cell.proposals(&queue).unwrap(/* t */);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].proposer, "mcp:claude-desktop");
    assert_eq!(rows[0].client.as_deref(), Some("test-client 9.9"));
    assert_eq!(rows[0].note.status, Status::Quarantined);

    // the same text again: a duplicate, and nothing new queued
    mcp.send(&propose_call(3, "mem", "Fabio  corre la mattina!").to_string());
    let res = mcp.read_response(3);
    let payload = tool_payload(&res);
    assert_eq!(payload["status"], "duplicate", "{payload}");
    assert_eq!(
        cell.proposals(&queue).unwrap(/* t */).len(),
        1,
        "the duplicate wrote nothing"
    );

    // a closed cell refuses: the policy is the first word
    open_with_policy(&cells, "mem", serde_json::json!({ "write": "off" }));
    mcp.send(&propose_call(4, "mem", "Fabio mangia sushi.").to_string());
    let res = mcp.read_response(4);
    assert!(
        res.get("error").is_some_and(|e| {
            e["message"]
                .as_str()
                .is_some_and(|m| m.contains("does not accept proposals"))
        }),
        "{res}"
    );
}

#[test]
fn a_cell_outside_the_write_list_refuses_without_revealing_more() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    make_cell(&cells, "aperta", "Fabio nuota in piscina.");
    make_cell(&cells, "sola_lettura", "Nota su [[Torino]].");
    make_cell(&cells, "segreta", "Nota riservata.");
    for name in ["aperta", "sola_lettura"] {
        open_with_policy(&cells, name, serde_json::json!({ "write": "propose" }));
    }

    let mut mcp = spawn_writing(dir.path(), "aperta,sola_lettura", "aperta", "claude");
    init(&mut mcp, 1, "2025-06-18", false);
    // exposed but not writable: the answer names only the writable set
    mcp.send(&propose_call(2, "sola_lettura", "Fabio corre.").to_string());
    let res = mcp.read_response(2);
    assert!(
        res.get("error").is_some_and(|e| {
            e["message"].as_str().is_some_and(|m| {
                m.contains("does not accept proposals through this server")
                    && m.contains("writable cells: aperta")
            })
        }),
        "{res}"
    );
    // not exposed at all: the same answer the read tools give, nothing more
    mcp.send(&propose_call(3, "segreta", "Fabio corre.").to_string());
    let res = mcp.read_response(3);
    assert!(
        res.get("error").is_some_and(|e| e["message"]
            .as_str()
            .is_some_and(|m| m.contains("not exposed"))),
        "{res}"
    );
}

#[test]
fn confirm_without_elicitation_capability_stays_pending_with_a_pointer() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    make_cell(&cells, "mem", "Fabio nuota in piscina.");
    open_with_policy(&cells, "mem", serde_json::json!({ "write": "confirm" }));

    let mut mcp = spawn_writing(dir.path(), "mem", "mem", "claude");
    init(&mut mcp, 1, "2025-06-18", false); // NO elicitation capability
    mcp.send(&propose_call(2, "mem", "Fabio sciaga.").to_string());
    let res = mcp.read_response(2);
    let payload = tool_payload(&res);
    assert_eq!(payload["status"], "pending", "{payload}");
    assert!(
        payload["note"]
            .as_str()
            .is_some_and(|n| n.contains("engramma proposals")),
        "the answer says where to confirm: {payload}"
    );
}

#[test]
fn confirm_with_the_server_requested_form_accepts_rejects_and_cancels() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    make_cell(&cells, "mem", "Fabio nuota in piscina.");
    open_with_policy(&cells, "mem", serde_json::json!({ "write": "confirm" }));

    let mut mcp = spawn_writing(dir.path(), "mem", "mem", "claude");
    init(&mut mcp, 1, "2025-06-18", true);

    // accept with the text EDITED by the human
    mcp.send(&propose_call(2, "mem", "Fabio sciaga.").to_string());
    mcp.answer_elicitation("accept", "Fabio sciaga in Dolomiti.", "sciaga");
    let res = mcp.read_response(2);
    let payload = tool_payload(&res);
    assert_eq!(payload["status"], "accepted", "{payload}");
    assert_eq!(payload["decided_by"], "user@mcp:claude", "{payload}");
    let id = payload["id"].as_str().unwrap(/* t */).to_string();
    let cell = Cell::open(&cells.join("mem.sqlite")).unwrap(/* t */);
    let note = cell
        .get(&ulid::Ulid::from_string(&id).unwrap(/* t */))
        .unwrap(/* t */);
    assert_eq!(note.status, Status::Active);
    assert_eq!(note.body, "Fabio sciaga in Dolomiti.", "the edit landed");

    // a plain accept keeps the proposed text
    mcp.send(&propose_call(3, "mem", "Fabio gioca a scacchi.").to_string());
    mcp.answer_elicitation("accept", "Fabio gioca a scacchi.", "scacchi");
    let res = mcp.read_response(3);
    let payload = tool_payload(&res);
    assert_eq!(payload["status"], "accepted");
    // the unchanged text is a plain accept, not an accept-with-edit
    assert_eq!(payload["reason"], "confermato dal modulo", "{payload}");

    // the RIFIUTA button rejects: the lapide keeps the text
    mcp.send(&propose_call(4, "mem", "Fabio vende la casa.").to_string());
    mcp.answer_elicitation("decline", "Fabio vende la casa.", "casa");
    let res = mcp.read_response(4);
    let payload = tool_payload(&res);
    assert_eq!(payload["status"], "rejected", "{payload}");
    let id = payload["id"].as_str().unwrap(/* t */).to_string();
    let note = cell
        .get(&ulid::Ulid::from_string(&id).unwrap(/* t */))
        .unwrap(/* t */);
    assert_eq!(note.status, Status::Deleted);
    assert_eq!(note.body, "Fabio vende la casa.");

    // ANNULLA leaves it pending, in the queue
    mcp.send(&propose_call(5, "mem", "Fabio cucina il pesce.").to_string());
    mcp.answer_elicitation("cancel", "", "pesce");
    let res = mcp.read_response(5);
    let payload = tool_payload(&res);
    assert_eq!(payload["status"], "pending", "{payload}");
}

#[test]
fn confirm_with_the_2026_input_required_round() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    make_cell(&cells, "mem", "Fabio nuota in piscina.");
    open_with_policy(&cells, "mem", serde_json::json!({ "write": "confirm" }));

    let mut mcp = spawn_writing(dir.path(), "mem", "mem", "claude");
    // 2026-07-28 speaks the discover flow: no initialize handshake — every
    // request carries its own `_meta` (version + capabilities)
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": { "elicitation": {} }
    });

    // the first round answers input_required, with the form inside
    mcp.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "memory_propose",
                "arguments": { "cell": "mem", "text": "Fabio va in barca." },
                "_meta": meta
            }
        })
        .to_string(),
    );
    let res = mcp.read_response(2);
    assert_eq!(res["result"]["resultType"], "input_required", "{res}");
    let request = &res["result"]["inputRequests"]["confirm"];
    assert!(request.is_object(), "{res}");
    assert!(
        request["params"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("barca")),
        "{res}"
    );
    let state = res["result"]["requestState"]
        .as_str()
        .unwrap(/* t */)
        .to_string();

    // the proposal is ALREADY queued: a client that never retries loses
    // nothing
    let cell = Cell::open(&cells.join("mem.sqlite")).unwrap(/* t */);
    assert_eq!(
        cell.proposals(&engramma_core::cell::proposals::ProposalsFilter::default())
            .unwrap(/* t */)
            .len(),
        1
    );

    // the retry: the same call with the answers and the echoed state
    mcp.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "memory_propose",
                "arguments": { "cell": "mem", "text": "Fabio va in barca." },
                "_meta": meta,
                "inputResponses": {
                    "confirm": { "action": "accept", "content": { "text": "Fabio va in barca a vela." } }
                },
                "requestState": state
            }
        })
        .to_string(),
    );
    let res = mcp.read_response(3);
    let payload = tool_payload(&res);
    assert_eq!(payload["status"], "accepted", "{res}");
    let id = payload["id"].as_str().unwrap(/* t */).to_string();
    let note = cell
        .get(&ulid::Ulid::from_string(&id).unwrap(/* t */))
        .unwrap(/* t */);
    assert_eq!(note.body, "Fabio va in barca a vela.", "the edit landed");

    // an unknown requestState is refused: the state is one-shot
    mcp.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {
                "name": "memory_propose",
                "arguments": { "cell": "mem", "text": "Fabio dorme." },
                "_meta": meta,
                "inputResponses": {
                    "confirm": { "action": "accept", "content": { "text": "Fabio dorme." } }
                },
                "requestState": state
            }
        })
        .to_string(),
    );
    let res = mcp.read_response(4);
    assert!(res.get("error").is_some(), "{res}");
}

/// v0.5.3 (P1.4): the indistinguishability control at the MCP surface. Two
/// symmetric notes answer «gatto»; a quarantined note and a tombstone that
/// match the question join the cell from another process (the server keeps
/// running). The wire answer of `memory_search` must be EXACTLY what it was
/// before — ids, order, scores, ranks — with no `2 and 3 without 1` and no
/// displaced answer: over HTTP that would be an oracle on hidden notes.
///
/// v0.6.15 (F1): the search asks `as_of` — a bitemporal question, where
/// the recency factor is exactly 1. The wall-clock score carries
/// `recency_of(now)`, quantised to WHOLE SECONDS: when the two searches
/// straddle a second boundary (a slower runner: the Linux arm64 CI met
/// it) the raw score moves in the eighth decimal and the byte equality
/// fails WITH THE ENGINE INTACT. With `as_of` the answer is a function of
/// the index alone, and the bytes can be equal on every platform.
#[test]
fn mcp_search_is_indistinguishable_with_hidden_notes() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    {
        let cell = Cell::create(&cells.join("gatti.sqlite"), "gatti", "test").unwrap(/* t */);
        for (title, body) in [
            ("nero", "Il gatto nero dorme tutta la giornata sul divano."),
            ("rosso", "Il gatto rosso gioca con la palla ogni sera."),
            ("cena", "Stasera cuciniamo le melanzane alla parmigiana."),
        ] {
            cell.put_note(&plain_note(title, body), "test").unwrap(/* t */);
        }
    }
    let mut mcp = spawn(dir.path(), "gatti");
    mcp.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0"}}}"#);
    let _ = mcp.read_response(1);
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let search = |id: i64| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "memory_search",
                "arguments": {"cell": "gatti", "query": "gatto", "as_of": "2027-02-01"}
            }
        })
    };
    mcp.send(&search(2).to_string());
    let before = mcp.read_response(2);
    let hits = before["result"]["content"][0]["text"].as_str().unwrap(/* t */);
    let before_parsed: serde_json::Value = serde_json::from_str(hits).unwrap(/* t */);
    let hits_before = before_parsed["hits"].as_array().unwrap(/* t */).clone();
    assert_eq!(hits_before.len(), 2, "{hits_before:?}");
    // the ranks are dense from 1: no hidden note above them
    assert_eq!(
        hits_before[0]["explain"]["bm25_rank"].as_i64(),
        Some(1),
        "{hits_before:?}"
    );
    assert_eq!(
        hits_before[1]["explain"]["bm25_rank"].as_i64(),
        Some(2),
        "{hits_before:?}"
    );

    // from ANOTHER process (the server keeps its cell open): a quarantined
    // note and a tombstone, both matching the question
    {
        let cell = Cell::open(&cells.join("gatti.sqlite")).unwrap(/* t */);
        let mut hidden = plain_note("web", "Il gatto del web vince tutti i premi.");
        hidden.confidence = Confidence::External;
        hidden.status = Status::Quarantined;
        hidden.source = "import:web".into();
        cell.put_note(&hidden, "test").unwrap(/* t */);
        let dead = plain_note("morto", "Il gatto di prova non c'è più.");
        let id = dead.id;
        cell.put_note(&dead, "test").unwrap(/* t */);
        cell.delete_note(&id, "test").unwrap(/* t */);
    }

    mcp.send(&search(3).to_string());
    let after = mcp.read_response(3);
    let text_after = after["result"]["content"][0]["text"].as_str().unwrap(/* t */);
    // the SAME bytes as before: field for field, hidden notes never moved
    // anything (the two visible notes match the same stem symmetrically, so
    // even SQLite's idf wobble cancels in the normalised answer)
    assert_eq!(hits, text_after, "the wire answer changed");
}

/// v0.5.3 (P2): `--compat chatgpt` without `--http` is a usage error —
/// over stdio there is no grant to bind the wrappers to (and no ChatGPT
/// either); it was silently ignored before.
#[test]
fn compat_chatgpt_over_stdio_is_a_usage_error() {
    let dir = common::test_dir();
    cell_root(dir.path());
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(dir.path())
        .args(["mcp", "--cells", "famiglia", "--compat", "chatgpt"])
        .output()
        .unwrap(/* t */);
    assert!(
        !out.status.success(),
        "--compat chatgpt over stdio must fail, it did not"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--compat chatgpt` goes with `--http"), "{err}");
}

// v0.6.18 (F4): after the automatic reviewer accepts, `memory_search` finds
// the note by MEANING, with no word in common — the vector is computed in
// the same process, after the decision and outside the write lock. Before
// this the accepted note waited for `engramma embed` (found by the site
// capture: a question paraphrased answered nothing until that ran).
#[test]
fn the_auto_reviewers_acceptance_is_found_by_meaning_right_away() {
    let fake = common::fake_embedder_server();
    let dir = common::test_dir();
    let root = dir.path();
    let cells = cell_root(root);
    // an empty cell, its target model, the auto policy and the reviewer
    {
        let cell = Cell::create(&cells.join("c.sqlite"), "c", "test").unwrap(/* t */);
        cell.add_model("http:fake", 8).unwrap(/* t */);
        cell.set_policy(
            serde_json::json!({ "write": "auto", "reviewer": "r", "min_confidence": 0.9 }),
            "test",
        )
        .unwrap(/* t */);
    }
    std::fs::write(
        root.join("reviewers.yaml"),
        "reviewers:\n  r:\n    command:\n      argv: [\"/bin/echo\", \"{\\\"decision\\\":\\\"accept\\\",\\\"reason\\\":\\\"ok\\\",\\\"confidence\\\":0.9}\"]\n      timeout_secs: 10\n",
    )
    .unwrap(/* t */);
    std::fs::write(
        root.join("config.json"),
        format!(
            r#"{{"embedder": {{"kind": "http", "model": "fake", "url": "http://127.0.0.1:{}", "dim": 8}}}}"#,
            fake.port
        ),
    )
    .unwrap(/* t */);

    let mut mcp = Mcp::spawn_with_write(root, "c", "claude-code");
    mcp.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0"}}}"#);
    mcp.read_response(1);
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    // the propose: the auto reviewer decides INSIDE the call
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "memory_propose",
            "arguments": {"cell": "c", "text": "Fabio lavora a Torino."}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(2);
    let text = res["result"]["content"][0]["text"].as_str().unwrap(/* t */);
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap(/* t */);
    assert_eq!(
        parsed["status"], "accepted",
        "the reviewer decided: {parsed}"
    );
    let note_id = parsed["id"].as_str().unwrap(/* t */).to_string();

    // and the meaning search finds it with no word in common
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "memory_search",
            "arguments": {"cell": "c", "query": "chi è impiegato?"}
        }
    });
    mcp.send(&call.to_string());
    let res = mcp.read_response(3);
    let text = res["result"]["content"][0]["text"].as_str().unwrap(/* t */);
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap(/* t */);
    assert_eq!(
        parsed["mode"], "hybrid",
        "the vector branch joins: {parsed}"
    );
    assert!(
        parsed["hits"]
            .as_array()
            .unwrap(/* t */)
            .iter()
            .any(|h| h["note"]["id"] == note_id),
        "the accepted note is found by meaning right away: {parsed}"
    );
}

// v0.6.18 (F5): `mcp --actor` without `--write` died INSIDE the server —
// the CLI wrapped it as an internal error (`mcp: …`, exit 4) and the MCP
// client only saw a server that dies. The check belongs to the CLI usage
// rules, next to «`--write` needs `--actor`»: exit 2, same message.
#[test]
fn actor_without_write_is_a_usage_error_of_the_cli() {
    let dir = common::test_dir();
    let cells = cell_root(dir.path());
    Cell::create(&cells.join("famiglia.sqlite"), "famiglia", "test").unwrap(/* t */);
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(dir.path())
        .args(["mcp", "--cells", "famiglia", "--actor", "claude-code"])
        .output()
        .unwrap(/* t */);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a usage error, not a dead server: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("`--actor` goes with `--write <cells>`"),
        "{err}"
    );
}
