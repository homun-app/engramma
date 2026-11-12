// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P1): two PROCESSES writing the same cell — a `propose` from an MCP
//! server while the CLI edits. The per-process lock cannot help here: what
//! serializes the writers is `BEGIN IMMEDIATE` plus `busy_timeout` (both in
//! place since the beginning). The test proves no write is lost and no
//! "database is locked" ever reaches a caller: the child process (this same
//! binary, `--exact` this test, `ENGRAMMA_TWO_WRITERS` set) hammers the cell
//! with proposals while the parent hammers it with edits.

use std::process::Command;

use engramma_core::cell::proposals::{ProposalInput, ProposalMode, ProposalState, ProposalsFilter};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use time::OffsetDateTime;

fn note(body: String) -> Note {
    let t = At(OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */));
    Note {
        id: engramma_core::id::new_ulid().unwrap(/* t */),
        note_type: NoteType::Fact,
        title: None,
        body,
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "cli".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: t,
        updated_at: t,
        extensions: vec![],
    }
}

/// The child side: 100 proposals, no duplicate texts, every error fatal.
fn child(cell: &Cell) {
    for i in 0..100 {
        let out = cell
            .propose(
                &ProposalInput {
                    note: note(format!("proposta {i} del secondo processo")),
                    mode: ProposalMode::New,
                    client: Some("mcp-test".into()),
                    reason: Some("concorrenza".into()),
                },
                "mcp:test",
                &engramma_core::cell::proposals::ProposeContext::default(),
            )
            .unwrap(/* test-only: a locked cell must not surface here */);
        assert!(out.id.is_some(), "distinct texts cannot be duplicates");
    }
}

#[test]
fn two_processes_share_one_cell_without_lost_writes_or_locks() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let path = dir.path().join("c.sqlite");
    if let Ok(child_path) = std::env::var("ENGRAMMA_TWO_WRITERS") {
        let cell = Cell::open(std::path::Path::new(&child_path)).unwrap(/* t */);
        child(&cell);
        return;
    }
    let cell = Cell::create(&path, "t", "t").unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "t")
        .unwrap(/* t */);
    let exe = std::env::current_exe().unwrap(/* t */);
    let mut kid = Command::new(exe)
        .args([
            "--exact",
            "two_processes_share_one_cell_without_lost_writes_or_locks",
        ])
        .env("ENGRAMMA_TWO_WRITERS", &path)
        .spawn()
        .unwrap(/* t */);
    // the parent edits while the child proposes; a short pause per write
    // keeps the two actually overlapping
    for i in 0..100_u16 {
        cell.put_note(&note(format!("nota {i} del primo processo")), "cli")
            .unwrap(/* test-only: a locked cell must not surface here */);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let status = kid.wait().unwrap(/* t */);
    assert!(status.success(), "the child writer failed: {status}");

    // nothing lost: reopen (a fresh HLC and view of the file) and count
    let cell = Cell::open(&path).unwrap(/* t */);
    let ops = cell.ops().unwrap(/* t */);
    assert_eq!(
        ops.len(),
        2 + 100 + 100,
        "init + policy + 100 edits + 100 proposals"
    );
    let queued = cell
        .proposals(&ProposalsFilter {
            state: Some(ProposalState::Pending),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(queued.len(), 100);
    for row in &queued {
        assert_eq!(row.proposer, "mcp:test");
    }
    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "mismatch: {:?}", report.mismatch);
}
