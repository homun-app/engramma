// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P1): proposals, decisions and rollback in the engine. A proposal is
//! a note born in quarantine with metadata (who proposed it, how, why); a
//! decision applies in the same operation that records it; a revert brings
//! the proposal back to the queue without ever rewriting history — and never
//! over a later human correction.

use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, DecisionState, DecisionsFilter, ProposalInput, ProposalMode,
    ProposalState, ProposalStatus, ProposalsFilter, RevertFilter, RevertOutcome,
};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::search::Query;
use engramma_core::{Cell, CellError, Confidence, NotePatch, Status};
use time::OffsetDateTime;
use ulid::Ulid;

fn t() -> At {
    At(OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */))
}

fn note(body: &str) -> Note {
    Note {
        id: engramma_core::id::new_ulid().unwrap(/* t */),
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
        created_at: t(),
        updated_at: t(),
        extensions: vec![],
    }
}

fn cell() -> (tempfile::TempDir, Cell) {
    let dir = test_dir();
    let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t").unwrap(/* t */);
    // v0.5 (P2): the default policy is closed (write: off); these tests
    // exercise the queue, so they open it
    cell.set_policy(serde_json::json!({ "write": "propose" }), "t")
        .unwrap(/* t */);
    (dir, cell)
}

fn propose(cell: &Cell, body: &str) -> engramma_core::cell::proposals::ProposalOutcome {
    cell.propose(
        &ProposalInput {
            note: note(body),
            mode: ProposalMode::New,
            client: Some("claude-desktop 1.2".into()),
            reason: Some("l'utente l'ha detto".into()),
        },
        "mcp:claude-desktop",
        &engramma_core::cell::proposals::ProposeContext::default(),
    )
    .unwrap(/* t */)
}

fn ids(cell: &Cell, text: &str) -> Vec<String> {
    cell.search(&Query::text(text).k(10))
        .unwrap(/* t */)
        .into_iter()
        .map(|h| h.note.id.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// the queue
// ---------------------------------------------------------------------------

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

#[test]
fn a_proposal_is_born_in_quarantine_and_search_never_sees_it() {
    let (_d, cell) = cell();
    let out = propose(&cell, "Fabio lavora a Milano.");
    assert!(matches!(out.status, ProposalStatus::Pending));
    let id = out.id.unwrap(/* t */);

    // the note exists, in quarantine, with its provenance
    let n = cell.get(&id).unwrap(/* t */);
    assert_eq!(n.status, Status::Quarantined);
    assert_eq!(n.source, "mcp:claude-desktop");

    // the queue, not the external quarantine: the proposals table says so
    let queue = cell.proposals(&ProposalsFilter::default()).unwrap(/* t */);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].id, id);
    assert_eq!(queue[0].proposer, "mcp:claude-desktop");
    assert_eq!(queue[0].client.as_deref(), Some("claude-desktop 1.2"));
    assert_eq!(queue[0].reason.as_deref(), Some("l'utente l'ha detto"));
    assert_eq!(queue[0].state, ProposalState::Pending);
    assert_eq!(queue[0].note.body, "Fabio lavora a Milano.");

    // search must not see it, not even as a graph seed
    assert!(ids(&cell, "Milano").is_empty());
    assert!(ids(&cell, "lavora").is_empty());
    let seeded = cell.search(&Query::text("Milano").k(10).graph_hops(1)).unwrap(/* t */);
    assert!(seeded.is_empty());

    // and the op is in the log
    assert!(
        cell
        .ops()
        .unwrap(/* t */)
        .iter()
        .any(|o| o.kind.as_str() == "propose")
    );
}

#[test]
fn the_queue_is_distinct_from_the_external_quarantine() {
    let (_d, cell) = cell();
    let mut ext = note("contenuto di terze parti");
    ext.confidence = Confidence::External;
    ext.status = Status::Quarantined;
    cell.put_note(&ext, "import").unwrap(/* t */);
    propose(&cell, "Fabio vive a Roma.");
    // one quarantined-by-external note, one proposal: only the latter queues
    let queue = cell.proposals(&ProposalsFilter::default()).unwrap(/* t */);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].note.body, "Fabio vive a Roma.");
}

// ---------------------------------------------------------------------------
// duplicates
// ---------------------------------------------------------------------------

#[test]
fn a_normalized_duplicate_is_refused_without_writing_an_op() {
    let (_d, cell) = cell();
    let a = note("Fabio lavora a Milano.");
    cell.put_note(&a, "cli").unwrap(/* t */);
    let before = cell.ops().unwrap(/* t */).len();

    let out = cell.propose(
        &ProposalInput {
            note: note("FABIO   lavora a Milano!"),
            mode: ProposalMode::New,
            client: None,
            reason: None,
        },
        "mcp:claude",

        &engramma_core::cell::proposals::ProposeContext::default(),
    )
    .unwrap(/* t */);
    assert!(matches!(out.status, ProposalStatus::Duplicate));
    assert_eq!(out.existing, Some(a.id));
    assert_eq!(cell.ops().unwrap(/* t */).len(), before);
    assert!(cell.proposals(&ProposalsFilter::default()).unwrap(/* t */).is_empty());
}

#[test]
fn a_duplicate_of_a_queued_proposal_is_refused_too() {
    let (_d, cell) = cell();
    let first = propose(&cell, "Fabio parla inglese e francese.");
    let id = first.id.unwrap(/* t */);
    let out = propose(&cell, "fabio parla inglese e francese");
    assert!(matches!(out.status, ProposalStatus::Duplicate));
    assert_eq!(out.existing, Some(id));
}

#[test]
fn different_text_is_not_a_duplicate() {
    let (_d, cell) = cell();
    cell.put_note(&note("Fabio lavora a Milano."), "cli").unwrap(/* t */);
    let out = propose(&cell, "Fabio lavora a Torino.");
    assert!(matches!(out.status, ProposalStatus::Pending));
}

// ---------------------------------------------------------------------------
// decisions
// ---------------------------------------------------------------------------

fn accept(cell: &Cell, id: &Ulid, actor: &str) {
    cell.decide(
        id,
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: Some("corretto".into()),
            review: None,
        },
        actor,
    )
    .unwrap(/* t */);
}

#[test]
fn accept_activates_the_note_in_the_same_operation() {
    let (_d, cell) = cell();
    let out = propose(&cell, "Fabio lavora a Milano.");
    let id = out.id.unwrap(/* t */);
    accept(&cell, &id, "user@cli");

    assert_eq!(cell.get(&id).unwrap(/* t */).status, Status::Active);
    assert_eq!(ids(&cell, "Milano"), vec![id.to_string()]);
    let queue = cell
        .proposals(&ProposalsFilter {
            state: Some(ProposalState::Accepted),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].decided_by.as_deref(), Some("user@cli"));

    let ds = cell.decisions(&DecisionsFilter::default()).unwrap(/* t */);
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].proposal, id);
    assert_eq!(ds[0].actor, "user@cli");
    assert_eq!(ds[0].decision, "accept");
    assert_eq!(ds[0].reason.as_deref(), Some("corretto"));
    assert_eq!(ds[0].state(), DecisionState::Active);
}

#[test]
fn accept_with_edit_activates_the_edited_text() {
    let (_d, cell) = cell();
    let out = propose(&cell, "Fabio lavora a Milano.");
    let id = out.id.unwrap(/* t */);
    let mut edited = cell.get(&id).unwrap(/* t */);
    edited.body = "Fabio lavora a Milano, in Porta Romana.".into();
    cell.decide(
        &id,
        &DecisionInput {
            action: DecisionAction::Accept {
                edited: Some(edited),
            },
            reason: Some("precisato dall'utente".into()),
            review: None,
        },
        "user@cli",
    )
    .unwrap(/* t */);
    let n = cell.get(&id).unwrap(/* t */);
    assert_eq!(n.status, Status::Active);
    assert_eq!(n.body, "Fabio lavora a Milano, in Porta Romana.");
    assert!(
        cell.note_version(&id).unwrap(/* t */) >= 2,
        "the edit is a new version"
    );
}

#[test]
fn accept_of_a_supersede_closes_the_target() {
    let (_d, cell) = cell();
    let old = note("Fabio lavora a Milano.");
    cell.put_note(&old, "cli").unwrap(/* t */);
    let mut new = note("Fabio lavora a Torino.");
    let out = cell
        .propose(
            &ProposalInput {
                note: new.clone(),
                mode: ProposalMode::Supersede(old.id),
                client: None,
                reason: Some("l'utente ha corretto".into()),
            },
            "mcp:claude",
            &engramma_core::cell::proposals::ProposeContext::default(),
        )
        .unwrap(/* t */);
    let id = out.id.unwrap(/* t */);
    new.id = id;
    accept(&cell, &id, "user@cli");

    let closed = cell.get(&old.id).unwrap(/* t */);
    assert_eq!(closed.status, Status::Superseded);
    assert!(closed.valid_to.is_some(), "the closing carries a valid_to");
    assert_eq!(cell.get(&id).unwrap(/* t */).status, Status::Active);
    let sup = cell
        .debug_column(&format!(
            "SELECT new_id FROM supersedes WHERE old_id = '{}'",
            old.id
        ))
        .unwrap(/* t */);
    assert!(sup.contains(&id.to_string()));
}

#[test]
fn reject_tombstones_but_the_text_stays_for_the_revert() {
    let (_d, cell) = cell();
    let out = propose(&cell, "Fabio venderebbe la casa.");
    let id = out.id.unwrap(/* t */);
    cell.decide(
        &id,
        &DecisionInput {
            action: DecisionAction::Reject,
            reason: Some("falso".into()),
            review: None,
        },
        "user@cli",
    )
    .unwrap(/* t */);
    let n = cell.get(&id).unwrap(/* t */);
    assert_eq!(n.status, Status::Deleted);
    assert_eq!(
        n.body, "Fabio venderebbe la casa.",
        "the lapide keeps the text"
    );
    assert!(ids(&cell, "casa").is_empty());
    let queue = cell
        .proposals(&ProposalsFilter {
            state: Some(ProposalState::Rejected),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(queue.len(), 1);
}

#[test]
fn a_decision_cannot_be_taken_twice_without_a_revert() {
    let (_d, cell) = cell();
    let out = propose(&cell, "Fabio corre la maratona.");
    let id = out.id.unwrap(/* t */);
    accept(&cell, &id, "user@cli");
    let again = cell.decide(
        &id,
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: None,
            review: None,
        },
        "user@cli",
    );
    assert!(again.is_err());
}

#[test]
fn deciding_an_unknown_note_is_refused() {
    let (_d, cell) = cell();
    let err = cell.decide(
        &Ulid::nil(),
        &DecisionInput {
            action: DecisionAction::Reject,
            reason: None,
            review: None,
        },
        "user@cli",
    );
    assert!(matches!(err, Err(CellError::Invalid(_))));
}

// ---------------------------------------------------------------------------
// revert
// ---------------------------------------------------------------------------

#[test]
fn revert_returns_the_proposal_to_the_queue_and_reopens_the_target() {
    let (_d, cell) = cell();
    let old = note("Fabio lavora a Milano.");
    cell.put_note(&old, "cli").unwrap(/* t */);
    let out = cell
        .propose(
            &ProposalInput {
                note: note("Fabio lavora a Torino."),
                mode: ProposalMode::Supersede(old.id),
                client: None,
                reason: None,
            },
            "mcp:claude",
            &engramma_core::cell::proposals::ProposeContext::default(),
        )
        .unwrap(/* t */);
    let id = out.id.unwrap(/* t */);
    accept(&cell, &id, "user@cli");
    let decision = cell.decisions(&DecisionsFilter::default()).unwrap(/* t */)[0].op_id;

    let outcome = cell.revert_decision(&decision, "errore", "user@cli").unwrap(/* t */);
    match outcome {
        RevertOutcome::Reverted { proposal, .. } => assert_eq!(proposal, id),
        RevertOutcome::Purged { .. } => panic!("nothing was purged"),
    }

    // the proposal is back in the queue, hidden again
    assert_eq!(cell.get(&id).unwrap(/* t */).status, Status::Quarantined);
    assert!(ids(&cell, "Torino").is_empty());
    let queue = cell
        .proposals(&ProposalsFilter {
            state: Some(ProposalState::Reverted),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(queue.len(), 1);

    // the target is alive again, exactly as it was
    let reopened = cell.get(&old.id).unwrap(/* t */);
    assert_eq!(reopened.status, Status::Active);
    assert!(reopened.valid_to.is_none());
    assert!(ids(&cell, "Milano") == vec![old.id.to_string()]);

    // the history says the decision was cancelled, it was not rewritten
    let ds = cell.decisions(&DecisionsFilter::default()).unwrap(/* t */);
    assert_eq!(ds[0].state(), DecisionState::Reverted);

    // and the proposal can be decided again
    accept(&cell, &id, "user@cli");
    assert_eq!(cell.get(&id).unwrap(/* t */).status, Status::Active);
}

#[test]
fn revert_of_a_reject_untombstones_the_proposal() {
    let (_d, cell) = cell();
    let out = propose(&cell, "Fabio venderebbe la casa.");
    let id = out.id.unwrap(/* t */);
    cell.decide(
        &id,
        &DecisionInput {
            action: DecisionAction::Reject,
            reason: None,
            review: None,
        },
        "user@cli",
    )
    .unwrap(/* t */);
    let decision = cell.decisions(&DecisionsFilter::default()).unwrap(/* t */)[0].op_id;
    cell.revert_decision(&decision, "ripenso", "user@cli").unwrap(/* t */);
    assert_eq!(cell.get(&id).unwrap(/* t */).status, Status::Quarantined);
    let queue = cell
        .proposals(&ProposalsFilter {
            state: Some(ProposalState::Reverted),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(queue.len(), 1);
}

#[test]
fn a_revert_never_lands_on_a_later_human_correction() {
    let (_d, cell) = cell();
    // case 1: the accepted note itself was edited afterwards
    let out = propose(&cell, "Fabio corre la maratona.");
    let id = out.id.unwrap(/* t */);
    accept(&cell, &id, "user@cli");
    cell.patch_note(
        &id,
        &NotePatch {
            body: Some("Fabio corre la mezza maratona.".into()),
            ..Default::default()
        },
        "cli",
    )
    .unwrap(/* t */);
    let decision = cell.decisions(&DecisionsFilter::default()).unwrap(/* t */)[0].op_id;
    let err = cell.revert_decision(&decision, "troppo tardi", "user@cli").unwrap_err(/* t */);
    assert!(matches!(err, CellError::Conflict(_)), "got: {err}");
    // the human edit is intact and the note is still active
    assert_eq!(
        cell.get(&id).unwrap(/* t */).body,
        "Fabio corre la mezza maratona."
    );
    assert_eq!(cell.get(&id).unwrap(/* t */).status, Status::Active);

    // case 2: the TARGET of the supersede was corrected afterwards
    let old = note("Fabio lavora a Milano.");
    cell.put_note(&old, "cli").unwrap(/* t */);
    let out = cell
        .propose(
            &ProposalInput {
                note: note("Fabio lavora a Torino."),
                mode: ProposalMode::Supersede(old.id),
                client: None,
                reason: None,
            },
            "mcp:claude",
            &engramma_core::cell::proposals::ProposeContext::default(),
        )
        .unwrap(/* t */);
    let id2 = out.id.unwrap(/* t */);
    accept(&cell, &id2, "user@cli");
    cell.patch_note(
        &old.id,
        &NotePatch {
            title: Some(Some("lavoro".into())),
            ..Default::default()
        },
        "cli",
    )
    .unwrap(/* t */);
    let decision2 = cell
        .decisions(&DecisionsFilter {
            ..Default::default()
        })
        .unwrap(/* t */)
        .iter()
        .find(|d| d.proposal == id2)
        .unwrap(/* t */)
        .op_id;
    let err = cell.revert_decision(&decision2, "no", "user@cli").unwrap_err(/* t */);
    assert!(matches!(err, CellError::Conflict(_)), "got: {err}");
}

#[test]
fn the_revert_of_purged_content_says_so_and_cannot_return_it() {
    let (_d, cell) = cell();
    let out = propose(&cell, "Fabio venderebbe la casa.");
    let id = out.id.unwrap(/* t */);
    accept(&cell, &id, "user@cli");
    let decision = cell.decisions(&DecisionsFilter::default()).unwrap(/* t */)[0].op_id;
    cell.purge(&id, "diritto all'oblio", "cli").unwrap(/* t */);

    let outcome = cell.revert_decision(&decision, "troppo tardi", "user@cli").unwrap(/* t */);
    assert!(matches!(outcome, RevertOutcome::Purged { .. }));
    // nothing came back
    assert!(cell.get(&id).is_err());
    assert!(cell.proposals(&ProposalsFilter::default()).unwrap(/* t */).is_empty());
}

// ---------------------------------------------------------------------------
// filters
// ---------------------------------------------------------------------------

#[test]
fn proposals_and_decisions_filters() {
    let (_d, cell) = cell();
    let a = propose(&cell, "Fabio parla inglese.");
    let b = propose(&cell, "Fabio parla francese.");
    let c = propose(&cell, "Fabio parla tedesco.");
    accept(&cell, &a.id.unwrap(/* t */), "reviewer:strict");
    cell.decide(
        &b.id.unwrap(/* t */),
        &DecisionInput {
            action: DecisionAction::Reject,
            reason: None,
            review: None,
        },
        "reviewer:strict",
    )
    .unwrap(/* t */);
    let _ = c; // still pending

    let pending = cell
        .proposals(&ProposalsFilter {
            state: Some(ProposalState::Pending),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].note.body, "Fabio parla tedesco.");

    let by = cell
        .proposals(&ProposalsFilter {
            by: Some("mcp:claude".into()),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(by.len(), 3, "the proposer filter matches by prefix");

    let reviewer = cell
        .decisions(&DecisionsFilter {
            by: Some("reviewer:".into()),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(reviewer.len(), 2);
    let human = cell
        .decisions(&DecisionsFilter {
            by: Some("user@".into()),
            ..Default::default()
        })
        .unwrap(/* t */);
    assert_eq!(human.len(), 0);
}

// ---------------------------------------------------------------------------
// bulk revert
// ---------------------------------------------------------------------------

#[test]
fn bulk_revert_by_actor_with_a_conflict_and_a_purge_in_the_middle() {
    let (_d, cell) = cell();
    // three decisions by the reviewer, one by the human
    let a = propose(&cell, "Fabio parla inglese.").id.unwrap(/* t */);
    let b = propose(&cell, "Fabio parla francese.").id.unwrap(/* t */);
    let c = propose(&cell, "Fabio parla tedesco.").id.unwrap(/* t */);
    let d = propose(&cell, "Fabio parla spagnolo.").id.unwrap(/* t */);
    accept(&cell, &a, "reviewer:strict");
    accept(&cell, &b, "reviewer:strict");
    accept(&cell, &c, "reviewer:strict");
    accept(&cell, &d, "user@cli");
    // a later human op on b: conflict
    cell.patch_note(
        &b,
        &NotePatch {
            body: Some("Fabio parla francese e catalano.".into()),
            ..Default::default()
        },
        "cli",
    )
    .unwrap(/* t */);
    // c was purged after its decision
    cell.purge(&c, "oblio", "cli").unwrap(/* t */);

    let filter = RevertFilter {
        by: Some("reviewer:".into()),
        ..Default::default()
    };
    let report = cell.revert_decisions(&filter, true).unwrap(/* t */);
    assert_eq!(report.reverted, 1, "only a is revertible");
    assert_eq!(report.conflicts.len(), 1, "b has a later human op");
    assert_eq!(report.purged, 1, "c is gone");

    // dry run changed nothing
    assert_eq!(cell.get(&a).unwrap(/* t */).status, Status::Active);

    let report = cell.revert_decisions(&filter, false).unwrap(/* t */);
    assert_eq!(report.reverted, 1);
    assert_eq!(cell.get(&a).unwrap(/* t */).status, Status::Quarantined);
    // the human decision is untouched
    assert_eq!(cell.get(&d).unwrap(/* t */).status, Status::Active);
}

// ---------------------------------------------------------------------------
// the log: replay, verify, purge
// ---------------------------------------------------------------------------

#[test]
fn verify_covers_the_new_operations_and_the_replay_matches() {
    let (_d, cell) = cell();
    let old = note("Fabio lavora a Milano.");
    cell.put_note(&old, "cli").unwrap(/* t */);
    let out = cell
        .propose(
            &ProposalInput {
                note: note("Fabio lavora a Torino."),
                mode: ProposalMode::Supersede(old.id),
                client: Some("claude".into()),
                reason: Some("correzione".into()),
            },
            "mcp:claude",
            &engramma_core::cell::proposals::ProposeContext::default(),
        )
        .unwrap(/* t */);
    accept(&cell, &out.id.unwrap(/* t */), "user@cli");
    propose(&cell, "Fabio corre la maratona.");
    let decision = cell.decisions(&DecisionsFilter::default()).unwrap(/* t */)[0].op_id;
    cell.revert_decision(&decision, "ripensamento", "user@cli").unwrap(/* t */);

    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "mismatch: {:?}", report.mismatch);
    assert_eq!(report.ops, cell.ops().unwrap(/* t */).len());
}

#[test]
fn a_purge_forgets_the_proposal_its_decisions_and_stays_verifiable() {
    let (_d, cell) = cell();
    let out = propose(&cell, "Fabio venderebbe la casa.");
    let id = out.id.unwrap(/* t */);
    accept(&cell, &id, "user@cli");
    assert_eq!(
        cell.decisions(&DecisionsFilter::default()).unwrap(/* t */).len(),
        1
    );
    cell.purge(&id, "oblio", "cli").unwrap(/* t */);
    assert!(cell.proposals(&ProposalsFilter::default()).unwrap(/* t */).is_empty());
    assert!(cell.decisions(&DecisionsFilter::default()).unwrap(/* t */).is_empty());
    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "mismatch: {:?}", report.mismatch);
}

/// v0.6.10 (J1) rewrote this replay equality: purging an ACCEPTED
/// supersede proposal dominates the whole DECIDE — the target's closing
/// goes with the proposal, and the from-zero replay of the redacted log
/// must agree with the live state. (v0.5–v0.6.9 kept the target closed:
/// the censored decide still replayed the closing. That was the hole the
/// reviewer's seed `14903861504949011020` walked through.)
#[test]
fn purging_an_accepted_supersede_reopens_the_target_and_stays_verifiable() {
    let (_d, cell) = cell();
    let old = note("Fabio lavora a Milano.");
    cell.put_note(&old, "cli").unwrap(/* t */);
    let out = cell
        .propose(
            &ProposalInput {
                note: note("Fabio lavora a Torino."),
                mode: ProposalMode::Supersede(old.id),
                client: None,
                reason: Some("correzione".into()),
            },
            "mcp:claude",
            &engramma_core::cell::proposals::ProposeContext::default(),
        )
        .unwrap(/* t */);
    let id = out.id.unwrap(/* t */);
    accept(&cell, &id, "reviewer:bravo");
    cell.purge(&id, "la proposta era avvelenata", "cli").unwrap(/* t */);

    // the proposal is gone, its decision with it…
    assert!(cell.get(&id).is_err());
    assert!(cell.proposals(&ProposalsFilter::default()).unwrap(/* t */).is_empty());
    assert!(cell.decisions(&DecisionsFilter::default()).unwrap(/* t */).is_empty());
    // …and the TARGET is open again: the dominated decide closed nothing
    let reopened = cell.get(&old.id).unwrap(/* t */);
    assert_eq!(reopened.status, Status::Active);
    assert!(reopened.valid_to.is_none());
    assert!(
        !ids(&cell, "Milano").is_empty(),
        "the reopened note is the current truth again"
    );
    // and the redacted replay (from zero: no compaction) reproduces it
    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "mismatch: {:?}", report.mismatch);
}
