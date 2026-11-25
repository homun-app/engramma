// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P4): the CLI surfaces of the queue — `proposals`, `decisions`,
//! `revert`, `policy`, and the enriched `history`. The engine beneath them
//! is covered by the core tests; what these prove is the plumbing: filters,
//! actors, exit codes, --json shapes, the confirmation flow of the bulk
//! revert, and the editor path of `proposals accept --edit`.

mod common;

use std::process::Command;

use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, ProposalInput, ProposalMode, ProposeContext,
};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use time::OffsetDateTime;

fn note(body: &str) -> Note {
    let t = At(OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */));
    Note {
        id: engramma_core::id::new_ulid().unwrap(/* t */),
        note_type: NoteType::Fact,
        title: None,
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "mcp:claude".into(),
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

struct Fixture {
    dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = common::test_dir();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap(/* t */);
        let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
            .arg("--root")
            .arg(&root)
            .arg("init")
            .output()
            .unwrap(/* t */);
        assert!(out.status.success());
        let f = Fixture { dir, root };
        f.policy("write=propose");
        f
    }

    fn cell(&self) -> Cell {
        Cell::open(&self.root.join("cells/default.sqlite")).unwrap(/* t */)
    }

    fn policy(&self, pairs: &str) {
        let args: Vec<&str> = std::iter::once("policy")
            .chain(std::iter::once("set"))
            .chain(pairs.split_whitespace())
            .collect();
        let out = self.run(&args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_engramma"))
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap(/* t */)
    }

    fn run_json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{}: {e}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// Queue a proposal through the engine (the CLI proposes nothing on its
    /// own — that is the assistants' job).
    fn propose(&self, body: &str) -> ulid::Ulid {
        let cell = self.cell();
        let out = cell
            .propose(
                &ProposalInput {
                    note: note(body),
                    mode: ProposalMode::New,
                    client: Some("claude-desktop 1.2".into()),
                    reason: Some("l'utente l'ha detto".into()),
                },
                "mcp:claude",
                &ProposeContext::default(),
            )
            .unwrap(/* t */);
        out.id.unwrap(/* t */)
    }

    fn accept(&self, id: &ulid::Ulid, actor: &str) {
        self.cell()
            .decide(
                id,
                &DecisionInput {
                    action: DecisionAction::Accept { edited: None },
                    reason: Some("detto dall'utente".into()),
                    review: None,
                },
                actor,
            )
            .unwrap(/* t */);
    }
}

#[test]
fn the_queue_lists_filters_and_decides() {
    let f = Fixture::new();
    let a = f.propose("Fabio parla inglese.");
    let b = f.propose("Fabio parla francese.");
    f.accept(&a, "user@cli");

    // the human listing: the queue marker, the proposer, the reason
    let out = f.run(&["proposals"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("in coda"), "{text}");
    assert!(text.contains("mcp:claude"), "{text}");
    assert!(text.contains("l'utente l'ha detto"), "{text}");

    // the state filter
    let json = f.run_json(&["proposals", "--state", "pending", "--json"]);
    let pending = json["proposals"].as_array().unwrap(/* t */);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["note"]["body"], "Fabio parla francese.");

    // --by prefix
    let json = f.run_json(&["proposals", "--by", "mcp:", "--json"]);
    assert_eq!(json["proposals"].as_array().unwrap(/* t */).len(), 2);

    // accept from the CLI: the note becomes active, the actor is user@cli
    let out = f.run(&["proposals", "accept", &b.to_string()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(f.cell().get(&b).unwrap(/* t */).status, Status::Active);

    // show one in full
    let json = f.run_json(&["proposals", "show", &a.to_string(), "--json"]);
    assert_eq!(json["proposer"], "mcp:claude");
    assert_eq!(json["client"], "claude-desktop 1.2");
    assert_eq!(json["decided_by"], "user@cli");

    // reject keeps the text under the lapide
    let c = f.propose("Fabio venderebbe la casa.");
    let out = f.run(&["proposals", "reject", &c.to_string(), "--reason", "falso"]);
    assert!(out.status.success());
    let n = f.cell().get(&c).unwrap(/* t */);
    assert_eq!(n.status, Status::Deleted);
    assert_eq!(n.body, "Fabio venderebbe la casa.");
}

#[test]
fn accept_with_edit_runs_the_editor() {
    let f = Fixture::new();
    let id = f.propose("Fabio lavora a Milano.");
    // an "editor" that rewrites the body deterministically
    let editor = f.dir.path().join("editor.sh");
    std::fs::write(
        &editor,
        // plain sh + sed without -i: the same script on macOS (BSD sed) and
        // Linux (GNU sed) — `sed -i ''` is a BSD-ism the container rejects
        "#!/bin/sh\nsed 's/Milano/Milano, Porta Romana/' \"$1\" > \"$1.new\" && mv \"$1.new\" \"$1\"\n",
    )
    .unwrap(/* t */);
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap(/* t */);
    }
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&f.root)
        .env("EDITOR", &editor)
        .args(["proposals", "accept", &id.to_string(), "--edit"])
        .output()
        .unwrap(/* t */);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let n = f.cell().get(&id).unwrap(/* t */);
    assert_eq!(n.status, Status::Active);
    assert_eq!(n.body, "Fabio lavora a Milano, Porta Romana.");
}

#[test]
fn decisions_list_filter_and_the_reverts() {
    let f = Fixture::new();
    let a = f.propose("Fabio parla inglese.");
    let b = f.propose("Fabio parla spagnolo.");
    let c = f.propose("Fabio parla tedesco.");
    f.accept(&a, "reviewer:bravo");
    f.accept(&b, "reviewer:bravo");
    f.accept(&c, "user@cli");

    // the registry: actor prefix and the reviewer metadata
    let json = f.run_json(&["decisions", "--by", "reviewer:", "--json"]);
    let rows = json["decisions"].as_array().unwrap(/* t */);
    assert_eq!(rows.len(), 2);
    for r in rows {
        assert_eq!(r["actor"], "reviewer:bravo");
    }
    let human = f.run(&["decisions"]);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("reviewer:bravo"), "{text}");

    // one conflict: a human correction after b's decision
    f.cell()
        .patch_note(
            &b,
            &engramma_core::NotePatch {
                body: Some("Fabio parla spagnolo e catalano.".into()),
                ..Default::default()
            },
            "cli",
        )
        .unwrap(/* t */);

    // the bulk revert of the reviewer: dry run first, then --yes
    let json = f.run_json(&["revert", "--by", "reviewer:", "--dry-run", "--json"]);
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["reverted"], 1);
    assert_eq!(json["conflicts"].as_array().unwrap(/* t */).len(), 1);
    // the dry run changed nothing
    assert_eq!(f.cell().get(&a).unwrap(/* t */).status, Status::Active);

    // without --yes and without a tty it reads stdin: empty answer = abort
    let out = Command::new(env!("CARGO_BIN_EXE_engramma"))
        .arg("--root")
        .arg(&f.root)
        .args(["revert", "--by", "reviewer:"])
        .output()
        .unwrap(/* t */);
    assert!(out.status.success(), "an aborted bulk revert exits 0");
    assert_eq!(f.cell().get(&a).unwrap(/* t */).status, Status::Active);

    let json = f.run_json(&["revert", "--by", "reviewer:", "--yes", "--json"]);
    assert_eq!(json["reverted"], 1);
    assert_eq!(f.cell().get(&a).unwrap(/* t */).status, Status::Quarantined);
    // the conflicted one is intact
    assert_eq!(f.cell().get(&b).unwrap(/* t */).status, Status::Active);

    // the single revert by its last-8 prefix: the human decision goes back too
    let decision = f.cell()
        .decisions(&Default::default())
        .unwrap(/* t */)
        .iter()
        .find(|d| d.proposal == c)
        .unwrap(/* t */)
        .op_id
        .to_string();
    let prefix = &decision[decision.len() - 8..];
    let json = f.run_json(&["revert", prefix, "--json"]);
    assert_eq!(json["outcome"]["reverted"], true);
    assert_eq!(f.cell().get(&c).unwrap(/* t */).status, Status::Quarantined);
}

// v0.6.18 (F1): `revert <id> --dry-run` is a PREVIEW — the single-id path
// ran the real revert and ignored the flag (found by the site capture, S12:
// the proposal was back in the queue after a "dry run"). The bulk path
// already honoured it; the preview must say what the real revert WOULD do,
// from the same classifier, and write nothing.
#[test]
fn revert_dry_run_on_a_single_id_writes_nothing() {
    let f = Fixture::new();
    let id = f.propose("Fabio parla inglese.");
    f.accept(&id, "user@cli");
    let decision = f
        .cell()
        .decisions(&Default::default())
        .unwrap(/* t */)
        .iter()
        .find(|d| d.proposal == id)
        .unwrap(/* t */)
        .op_id
        .to_string();
    let ops_before = f.run_json(&["verify", "--json"])["ops"]
        .as_i64()
        .unwrap(/* t */);

    // the dry run answers, and changes nothing
    let json = f.run_json(&["revert", &decision, "--dry-run", "--json"]);
    assert_eq!(json["dry_run"], true);
    let ops_after = f.run_json(&["verify", "--json"])["ops"]
        .as_i64()
        .unwrap(/* t */);
    assert_eq!(ops_after, ops_before, "a dry run writes no operation");

    // `decisions` still shows the decision active
    let json = f.run_json(&["decisions", "--json"]);
    let row = json["decisions"]
        .as_array()
        .unwrap(/* t */)
        .iter()
        .find(|r| r["proposal"] == id.to_string())
        .unwrap(/* t */);
    assert_eq!(row["state"], "active", "the dry run must not cancel it");
    assert_eq!(row["reverted_by"], serde_json::Value::Null);
    // the note is still active: the queue kept its accepted proposal
    assert_eq!(f.cell().get(&id).unwrap(/* t */).status, Status::Active);

    // a revert right after undoes it for real
    let json = f.run_json(&["revert", &decision, "--json"]);
    assert_eq!(json["outcome"]["reverted"], true);
    assert_eq!(f.cell().get(&id).unwrap(/* t */).status, Status::Quarantined);
}

// v0.6.18 (F2): decisions are signed by WHO DECIDES (`user@cli`,
// `reviewer:<nome>`), never by who proposed — `revert --by mcp:<actor>` was
// accepted and silently undid nothing (found by the site capture). It is a
// usage error now; the undo of what an assistant filled the queue with is
// `--proposed-by`, on `revert` and on `decisions`.
#[test]
fn revert_by_mcp_actor_is_a_usage_error_and_proposed_by_selects_the_proposals() {
    let f = Fixture::new();
    let a = f.propose("Fabio parla inglese.");
    let b = f.propose("Fabio parla francese.");
    // one proposal from another actor, accepted by the same reviewer
    let c = {
        let cell = f.cell();
        let out = cell
            .propose(
                &ProposalInput {
                    note: note("Fabio parla tedesco."),
                    mode: ProposalMode::New,
                    client: None,
                    reason: None,
                },
                "mcp:altro",
                &ProposeContext::default(),
            )
            .unwrap(/* t */);
        out.id.unwrap(/* t */)
    };
    f.accept(&a, "reviewer:bravo");
    f.accept(&b, "user@cli");
    f.accept(&c, "reviewer:bravo");

    // `--by mcp:…` cannot select anything: decisions never carry that actor
    let out = f.run(&["revert", "--by", "mcp:claude-code"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a usage error, not silence: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("--proposed-by mcp:claude-code"),
        "the error suggests the new filter: {said}"
    );

    // the preview of the undo: every decision taken on mcp:claude-code's
    // proposals — the reviewer's one on a AND the human's one on b
    let json = f.run_json(&[
        "revert",
        "--proposed-by",
        "mcp:claude",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["reverted"], 2);
    // the dry run changed nothing
    let d = f
        .cell()
        .decisions(&Default::default())
        .unwrap(/* t */)
        .into_iter()
        .find(|d| d.proposal == a)
        .unwrap(/* t */);
    assert!(d.reverted_by.is_none(), "a dry run writes nothing");

    // the registry answers the same question before the undo
    let json = f.run_json(&["decisions", "--proposed-by", "mcp:claude", "--json"]);
    let rows = json["decisions"].as_array().unwrap(/* t */);
    assert_eq!(rows.len(), 2);

    // the real undo: a and b go back to the queue, c stays decided
    let json = f.run_json(&["revert", "--proposed-by", "mcp:claude", "--yes", "--json"]);
    assert_eq!(json["reverted"], 2);
    assert_eq!(f.cell().get(&a).unwrap(/* t */).status, Status::Quarantined);
    assert_eq!(f.cell().get(&b).unwrap(/* t */).status, Status::Quarantined);
    assert_eq!(f.cell().get(&c).unwrap(/* t */).status, Status::Active);
}

#[test]
fn the_policy_reads_and_changes_from_the_cli_only() {
    let f = Fixture::new();
    // show reflects the stored document
    let json = f.run_json(&["policy", "show", "--json"]);
    assert_eq!(json["policy"]["write"], "propose");
    assert_eq!(json["policy"]["min_confidence"], 0.8);
    assert_eq!(json["set"], true);

    // set merges and refuses what it cannot read back
    f.policy("write=auto reviewer=bravo max_auto_per_hour=5 kinds=fact,preference allow_supersede=inferred");
    let json = f.run_json(&["policy", "show", "--json"]);
    assert_eq!(json["policy"]["reviewer"], "bravo");
    assert_eq!(json["policy"]["max_auto_per_hour"], 5);
    let bare = Fixture::new(); // write=propose, no reviewer
    let out = bare.run(&["policy", "set", "write=auto"]);
    assert!(
        !out.status.success(),
        "auto without reviewer must be refused"
    );
    let out = f.run(&["policy", "set", "min_confidnce=0.9"]);
    assert!(!out.status.success(), "unknown fields are refused");
    // and the policy is an operation: the log says who changed what
    assert!(
        f
        .cell()
        .ops()
        .unwrap(/* t */)
        .iter()
        .any(|o| o.kind.as_str() == "set_policy")
    );
}

#[test]
fn history_shows_the_story_and_the_diff() {
    let f = Fixture::new();
    let id = f.propose("Fabio corre la maratona.");
    f.accept(&id, "user@cli");
    f.cell()
        .patch_note(
            &id,
            &engramma_core::NotePatch {
                body: Some("Fabio corre la mezza maratona.\nOgni domenica.".into()),
                ..Default::default()
            },
            "cli",
        )
        .unwrap(/* t */);

    // the human history tells the story: who proposed, who decided
    let out = f.run(&["history", &id.to_string()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("proposta da mcp:claude"), "{text}");
    assert!(text.contains("decisa da user@cli"), "{text}");
    assert!(text.contains("[decide"), "{text}");
    assert!(text.contains("[patch_note"), "{text}");

    // the diff: what changed between consecutive versions
    let out = f.run(&["history", &id.to_string(), "--diff"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("- Fabio corre la maratona."), "{text}");
    assert!(text.contains("+ Fabio corre la mezza maratona."), "{text}");
    assert!(text.contains("+ Ogni domenica."), "{text}");

    // --json keeps the story and the op of every version
    let json = f.run_json(&["history", &id.to_string(), "--json"]);
    let versions = json["versions"].as_array().unwrap(/* t */);
    assert!(versions.len() >= 2);
    assert!(versions.iter().any(|v| v["op"]["kind"] == "decide"));
    assert_eq!(json["proposal"]["proposer"], "mcp:claude");
    assert_eq!(json["proposal"]["decided_by"], "user@cli");
}
