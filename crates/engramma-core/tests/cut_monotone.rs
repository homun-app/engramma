// SPDX-License-Identifier: Apache-2.0

//! v0.6.14 (N1): **the cut only goes forward.** The compaction cut is a
//! monotone threshold — Raft's snapshot index, Kafka's log start offset —
//! and until this phase `compact` accepted a cut BELOW the one already
//! made (the v0.6.13 verification traced it on seed
//! `11558843446952054066`, its minimal probe in the reviewer's
//! scratchpad): the payloads between the two cuts are already gone, the
//! registered cut says they are not, and the first reconstruction above
//! the new cut replays an operation whose payload is gone — a `decide`
//! that accepted a proposal lands as nothing, the queue says `pending`,
//! the registry is empty, and `verify` is blind to all of it.
//!
//! It happens in ordinary life: Homun raises `keepLast` and `head −
//! keepLast` falls below yesterday's cut; a shared cell clamps the cut to
//! the stable frontier, and the frontier is a minimum.
//!
//! The SAME cut is not back: it keeps the behavior of v0.3.2 — the
//! restart point is registered again, in the form asked (the test
//! `the_same_cut_registers_the_restart_point_again`; the v0.3.1 upgrade
//! in `forgetting.rs` leans on it).
//!
//! These tests were RED on the v0.6.13 base (commit of the red tests in
//! the REPORT): the queue came back `pending` with no registry row, the
//! registered cut went back, and `verify` stayed green on a cell whose
//! payload is removed above its cut.

mod common;

use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, ProposalInput, ProposalMode, ProposeContext,
};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use time::OffsetDateTime;
use ulid::Ulid;

fn at(day: u8) -> At {
    At(OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400).unwrap(/* t */))
}

fn note_of(id: u16, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* t */),
        note_type: NoteType::Fact,
        title: None,
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
        created_at: at(1),
        updated_at: at(1),
        extensions: vec![],
    }
}

fn head(c: &Cell) -> i64 {
    c.debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops").unwrap(/* t */)[0]
        .parse()
        .unwrap(/* t */)
}

/// The registered cut: the `seq` of the newest row of `snapshots` (the
/// one place the cut lives, v0.6.14 N1.4).
fn registered_cut(c: &Cell) -> i64 {
    c.debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM snapshots").unwrap(/* t */)[0]
        .parse()
        .unwrap(/* t */)
}

fn queue(c: &Cell, id: &Ulid) -> Vec<String> {
    c.debug_column(&format!(
        "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{id}'"
    ))
    .unwrap(/* t */)
}

fn registry(c: &Cell, id: &Ulid) -> Vec<String> {
    c.debug_column(&format!(
        "SELECT decision || ' ' || actor FROM decisions WHERE proposal = '{id}'"
    ))
    .unwrap(/* t */)
}

/// The reviewer's minimal stage: an agent's `new` proposal, the `decide`
/// that accepts it, and the head it leaves.
struct Stage {
    dir: tempfile::TempDir,
    cell: Cell,
    pid: Ulid,
}

fn accepted_proposal(prefix: &str) -> Stage {
    let dir = common::test_dir();
    let cell =
        Cell::create(&dir.path().join(format!("{prefix}.sqlite")), "p", "n1").unwrap(/* t */);
    cell.put_note(&note_of(1, "nota qualunque"), "user").unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "cli").unwrap(/* t */);
    let out = cell
        .propose(
            &ProposalInput {
                note: note_of(3, "il mio colore preferito è il nero"),
                mode: ProposalMode::New,
                client: None,
                reason: None,
            },
            "mcp:agente",
            &ProposeContext { embedder: None, reviewers: None },
        )
        .unwrap(/* t */);
    let pid = out.id.expect(/* t */ "queued");
    cell.decide(
        &pid,
        &DecisionInput { action: DecisionAction::Accept { edited: None }, reason: None, review: None },
        "user",
    )
    .unwrap(/* t */);
    Stage { dir, cell, pid }
}

/// The user closes the accepted proposal with a supersede written ABOVE
/// the cut — the first reconstruction over the cut.
/// Two compactions: the first at `h`, the second at `h - back`, each with
/// or without the copy file. The reports come back with the stage.
fn compact_twice(
    s: &Stage,
    h: i64,
    back: i64,
    first_file: bool,
    second_file: bool,
) -> (
    engramma_core::cell::CompactReport,
    engramma_core::cell::CompactReport,
) {
    let r1 = s.cell.compact(h, &engramma_core::cell::CompactOptions { keep_snapshot: first_file }).unwrap(/* t */);
    let r2 = s.cell.compact(h - back, &engramma_core::cell::CompactOptions { keep_snapshot: second_file }).unwrap(/* t */);
    (r1, r2)
}

fn supersede_above(cell: &Cell, pid: &Ulid) {
    let mut n = note_of(5, "il mio colore preferito è il blu");
    n.supersedes = vec![*pid];
    cell.supersede(pid, &n, None, "user").unwrap(/* t */);
}

/// The queue and the registry of an accepted proposal, as the user left
/// them — before and after the supersede alike.
fn assert_the_queue_stands(cell: &Cell, pid: &Ulid, status: Status, ctx: &str) {
    assert_eq!(
        queue(cell, pid),
        vec!["new accepted".to_string()],
        "{ctx}: the queue says the user accepted it"
    );
    assert_eq!(
        registry(cell, pid),
        vec!["accept user".to_string()],
        "{ctx}: the registry holds the user's decision"
    );
    assert_eq!(
        cell.get(pid).unwrap(/* t */).status,
        status,
        "{ctx}: the note's state"
    );
    assert!(cell.verify().unwrap(/* t */).ok(), "{ctx}: verify is green");
}

/// `cut_goes_back`, WITH the copy file (the reviewer's `zz_back_keep`):
/// `compact(head)` then `compact(head − 1)` keeping the snapshot, and the
/// supersede above. On the v0.6.13 engine: queue `new pending`, registry
/// empty, `verify` green.
#[test]
fn cut_goes_back_with_the_copy_file() {
    let s = accepted_proposal("taglio-copia");
    let h = head(&s.cell);
    let (r1, r2) = compact_twice(&s, h, 1, false, true);
    assert!(r1.compacted, "the first call compacted");
    assert!(
        !r2.compacted,
        "the second call, below the cut made, did nothing"
    );
    assert_eq!(r2.cut_seq, r1.cut_seq, "and answers the true cut");
    assert_the_queue_stands(&s.cell, &s.pid, Status::Active, "after the two compactions");
    supersede_above(&s.cell, &s.pid);
    assert_the_queue_stands(
        &s.cell,
        &s.pid,
        Status::Superseded,
        "after the supersede above the cut",
    );
}

/// `cut_goes_back`, WITHOUT the copy file (the reviewer's
/// `zz_back_nokeep`): two checkpoints, the second one below the first.
#[test]
fn cut_goes_back_without_the_copy_file() {
    let s = accepted_proposal("taglio-secco");
    let h = head(&s.cell);
    let (r1, r2) = compact_twice(&s, h, 1, false, false);
    assert!(r1.compacted && !r2.compacted, "the second call did nothing");
    assert_eq!(r2.cut_seq, r1.cut_seq, "and answers the true cut");
    assert_the_queue_stands(&s.cell, &s.pid, Status::Active, "after the two compactions");
    supersede_above(&s.cell, &s.pid);
    assert_the_queue_stands(
        &s.cell,
        &s.pid,
        Status::Superseded,
        "after the supersede above the cut",
    );
}

/// The control: the same cut twice is the story with no defect, on every
/// engine (`zz_same_keep`).
#[test]
fn the_same_cut_twice_is_the_control() {
    let s = accepted_proposal("taglio-uguale");
    let h = head(&s.cell);
    let (r1, r2) = compact_twice(&s, h, 0, false, true);
    assert!(
        r1.compacted && r2.compacted,
        "the same cut registers the restart point again"
    );
    assert_eq!(r2.cut_seq, r1.cut_seq, "and the cut does not move");
    assert_the_queue_stands(&s.cell, &s.pid, Status::Active, "after the two compactions");
    supersede_above(&s.cell, &s.pid);
    assert_the_queue_stands(
        &s.cell,
        &s.pid,
        Status::Superseded,
        "after the supersede above the cut",
    );
}

/// A cut BELOW the one already made does NOTHING: the report answers the
/// true cut, and the registered cut does not move. It is not an error —
/// Homun's automatic call must not fail when `keepLast` grows.
#[test]
fn a_cut_below_the_made_one_changes_nothing() {
    let s = accepted_proposal("taglio-fermo");
    let h = head(&s.cell);
    let first = s.cell
        .compact(h, &engramma_core::cell::CompactOptions { keep_snapshot: false })
        .unwrap(/* t */);
    let below = s.cell
        .compact(h - 1, &engramma_core::cell::CompactOptions { keep_snapshot: false })
        .unwrap(/* t */);
    assert!(
        first.compacted && !below.compacted,
        "the first call compacted; the one below the cut made did nothing"
    );
    assert_eq!(
        below.cut_seq, first.cut_seq,
        "the report answers the TRUE cut, not the one asked"
    );
    assert_eq!(below.head_seq, h, "the report speaks of the head of now");
    assert!(
        below.bytes_before == below.bytes_after,
        "a call that does nothing moves no byte"
    );
    assert_eq!(
        registered_cut(&s.cell),
        first.cut_seq,
        "the registered cut did not go back"
    );
    // and a forward cut still works
    supersede_above(&s.cell, &s.pid);
    let h2 = head(&s.cell);
    let forward = s.cell
        .compact(h2, &engramma_core::cell::CompactOptions { keep_snapshot: false })
        .unwrap(/* t */);
    assert!(forward.compacted, "a cut above the made one lands");
    assert_eq!(forward.cut_seq, h2, "a cut above the made one lands");
    assert_eq!(registered_cut(&s.cell), h2);
}

/// The same cut, asked again, is not a cut that goes back: the restart
/// point takes the form asked (a file snapshot becomes a checkpoint, and
/// the other way round — the v0.3.2 conversion) and the cut does not move.
#[test]
fn the_same_cut_registers_the_restart_point_again() {
    let s = accepted_proposal("taglio-forma");
    let h = head(&s.cell);
    let with_file = s.cell
        .compact(h, &engramma_core::cell::CompactOptions { keep_snapshot: true })
        .unwrap(/* t */);
    assert!(
        with_file.compacted && with_file.snapshot.is_some(),
        "a compaction with its file"
    );
    let as_checkpoint = s.cell
        .compact(h, &engramma_core::cell::CompactOptions { keep_snapshot: false })
        .unwrap(/* t */);
    assert!(
        as_checkpoint.snapshot.is_none(),
        "the same cut turns the file into a checkpoint (v0.3.2)"
    );
    assert_eq!(as_checkpoint.cut_seq, h, "the cut did not move");
    assert_eq!(registered_cut(&s.cell), h);
    assert_the_queue_stands(&s.cell, &s.pid, Status::Active, "after the two compactions");
    supersede_above(&s.cell, &s.pid);
    assert_the_queue_stands(
        &s.cell,
        &s.pid,
        Status::Superseded,
        "after the supersede above the cut",
    );
}

/// `verify` checks the threshold: an operation whose payload is removed
/// ABOVE the registered cut is a broken cell, and the message names it.
/// The cell is built BY HAND — the v0.6.13 engine's own damage, frozen as
/// a shape: a compaction at the head, then the registered cut pushed one
/// step back under the same log.
#[test]
fn verify_names_a_payload_removed_above_the_cut() {
    let s = accepted_proposal("taglio-verifica");
    let h = head(&s.cell);
    s.cell
        .compact(h, &engramma_core::cell::CompactOptions { keep_snapshot: false })
        .unwrap(/* t */);
    assert!(
        s.cell.verify().unwrap(/* t */).ok(),
        "the healthy cell verifies"
    );
    // the operation the pushed-back record leaves above the cut: the
    // one AT the cut the compaction really made (the head, the `decide`)
    let offender = s.cell.debug_column(&format!(
        "SELECT op_id FROM ops WHERE seq = {h} AND payload_state = 'redacted'"
    ))
    .unwrap(/* t */)[0]
        .clone();
    // the hand-made damage: the record says one step less than the
    // payloads the compaction took
    let raw =
        rusqlite::Connection::open(s.dir.path().join("taglio-verifica.sqlite")).unwrap(/* t */);
    raw.execute("UPDATE snapshots SET seq = seq - 1", []).unwrap(/* t */);
    drop(raw);
    let report = s.cell.verify().unwrap(/* t */);
    assert!(
        !report.ok(),
        "a payload removed above the cut is a broken cell"
    );
    let mismatch = report.mismatch.expect(/* t */ "the report names the break");
    assert!(
        mismatch.contains(&offender) && mismatch.contains(&format!("seq {h}")),
        "the message names the operation above the cut (`{offender}`, seq {h}): {mismatch}"
    );
}

/// The frontier that goes down. A shared cell compacts under its stable
/// frontier (the minimum over the devices' acknowledgements); the first
/// compaction lands, a new device enters with an acknowledgement BELOW
/// that cut, and the next compaction — asked above the frontier — must
/// change nothing: the cut does not go back with the frontier.
///
/// The device row is written directly: with `record_session_ack` as it is
/// (an acknowledgement is the head of its own session) no public call
/// lowers the frontier today, and the rule guards the shape all the same.
#[test]
fn the_frontier_that_goes_down_changes_nothing() {
    let s = accepted_proposal("taglio-frontiera");
    let cell = &s.cell;
    for i in 6..10 {
        cell.put_note(&note_of(i, "nota di riempimento"), "user").unwrap(/* t */);
    }
    let h = head(cell);
    cell.mark_shared().unwrap(/* t */);
    cell.record_session_ack("portatile").unwrap(/* t */);
    assert_eq!(
        cell.stable_frontier().unwrap(/* t */),
        h,
        "one device: the frontier is the head"
    );
    let first = cell
        .compact(h - 2, &engramma_core::cell::CompactOptions { keep_snapshot: false })
        .unwrap(/* t */);
    assert_eq!(
        first.cut_seq,
        h - 2,
        "the first compaction lands under the frontier"
    );
    // a new device enters and the frontier falls below the cut
    let raw =
        rusqlite::Connection::open(s.dir.path().join("taglio-frontiera.sqlite")).unwrap(/* t */);
    raw.execute(
        "INSERT INTO device_acks (device, ack_seq, at) VALUES ('nuovo', ?1, '')",
        [h - 4],
    )
    .unwrap(/* t */);
    drop(raw);
    assert_eq!(
        cell.stable_frontier().unwrap(/* t */),
        h - 4,
        "the frontier is below the cut already made"
    );
    let second = cell
        .compact(h, &engramma_core::cell::CompactOptions { keep_snapshot: false })
        .unwrap(/* t */);
    assert!(
        !second.compacted,
        "the frontier does not drag the cut back: nothing was compacted"
    );
    assert_eq!(
        second.cut_seq, first.cut_seq,
        "the frontier does not drag the cut back"
    );
    assert_eq!(
        registered_cut(cell),
        first.cut_seq,
        "the registered cut stands"
    );
    assert!(cell.verify().unwrap(/* t */).ok(), "the cell verifies");
}
