// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P1): property test over random sequences of `propose`, `decide`,
//! `revert`, human corrections (`patch`, `supersede`, `restore`) and `purge`.
//! Invariants (the spec's own list):
//! 1. a queued or reverted proposal never shows up in the search — not even
//!    as a graph seed;
//! 2. a revert never lands on a later human correction;
//! 3. the from-zero replay gives the same state as the live cell (`verify`);
//! 4. `verify` stays green.

use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, DecisionsFilter, ProposalInput, ProposalMode, RevertFilter,
};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::search::Query;
use engramma_core::{Cell, Confidence, NotePatch, Status};
use proptest::prelude::*;
use time::OffsetDateTime;
use ulid::Ulid;
// v0.6.14 (N3): `current` is a trait method of the value trees
use proptest::strategy::ValueTree as _;

/// v0.6.14 (N3): a runner seeded by the property's OWN seed. The body of
/// every property takes that one seed and derives its inputs from it
/// through the same strategies the property always drew — so a failing
/// seed replays in ONE case (`scripts/seme-sul-tag.sh <proprietà> <seme>
/// <tag>`), on any tree that holds the body.
fn seeded_runner(seed: u64) -> proptest::test_runner::TestRunner {
    // ChaCha (the default) wants a 32-byte seed: the u64 is laid four times
    let block = seed.to_le_bytes();
    let mut bytes = [0_u8; 32];
    for k in 0..4 {
        bytes[k * 8..k * 8 + 8].copy_from_slice(&block);
    }
    proptest::test_runner::TestRunner::new_with_rng(
        proptest::test_runner::Config::default(),
        proptest::test_runner::TestRng::from_seed(
            proptest::test_runner::RngAlgorithm::default(),
            &bytes,
        ),
    )
}

/// A tiny deterministic generator (SplitMix64): the sequence must be a pure
/// function of the seed, or a failing case cannot be replayed by hand.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n as u64)).unwrap_or(0)
    }
}

fn t(secs: u64) -> At {
    At(
        OffsetDateTime::from_unix_timestamp(1_800_000_000_i64 + (secs as i64))
        .unwrap(/* test-only invariant: bounded by the strategy */),
    )
}

fn base_note(body: String, at: At) -> Note {
    Note {
        id: engramma_core::id::new_ulid().unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: None,
        body,
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: at,
        updated_at: at,
        extensions: vec![],
    }
}

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]


#[test]
    fn proposal_sequences_keep_the_invariants(seed in any::<u64>()) {
        proposal_sequences_keep_the_invariants_body(seed)?;
    }

// v0.6.18 (F3): the invariant the double-accept broke — in every chain of
// `supersede` (direct or through the queue), at most ONE note stands
// active: a correction is worth against the version it corrected.
#[test]
    fn a_supersede_chain_keeps_at_most_one_active_note(seed in any::<u64>()) {
        a_supersede_chain_keeps_at_most_one_active_note_body(seed)?;
    }
}

/// v0.6.14 (N3): the body of `proposal_sequences_keep_the_invariants` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn proposal_sequences_keep_the_invariants_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (seed, steps) = ((any::<u64>()), (proptest::collection::vec(0u8..9, 12..48)))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    proposal_sequences_keep_the_invariants_with(seed, steps)
}

/// v0.6.14 (N3): the property's story on its inputs EXPLICITLY — the shape
/// it drew until this phase, the one in which its persisted counterexamples
/// and its declared defect were written down. The body above derives the
/// inputs from one seed (so a failing seed replays in one case); this takes
/// them as they are, for the cases history recorded.
fn proposal_sequences_keep_the_invariants_with(
    seed: u64,
    steps: Vec<u8>,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let dir = test_dir();
    let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t").unwrap(/* test-only */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "t")
        .unwrap(/* test-only */);
    let mut rng = Rng(seed);
    let mut clock = 0_u64;
    let mut counter = 0_u64;

    // the model: what the sequence built
    let mut active: Vec<Ulid> = Vec::new(); // human + accepted notes
    let mut queue: Vec<Ulid> = Vec::new(); // proposals in the queue (pending/reverted)
    let mut marker_of: std::collections::HashMap<Ulid, String> = std::collections::HashMap::new();
    let mut decided: Vec<Ulid> = Vec::new(); // proposals with a live decision

    for step in steps {
        counter += 1;
        clock += 1 + rng.below(3) as u64;
        let at = t(clock);
        let marker = format!("marcatore{counter}");
        match step {
            0 | 1 => {
                // human write
                let body = format!("il fatto {marker} detto dall'utente");
                let n = base_note(body.clone(), at);
                cell.put_note(&n, "cli").unwrap(/* test-only */);
                marker_of.insert(n.id, marker.clone());
                active.push(n.id);
            }
            2 => {
                // propose something new (a fifth of the times the SAME
                // text as an active note: the duplicate refusal fires too)
                let body = if !active.is_empty() && rng.below(5) == 0 {
                    format!(
                        "il fatto marcatore{} detto dall'utente",
                        counter.saturating_sub(1).max(1)
                    )
                } else {
                    format!("la proposta {marker} sostenuta dall'assistente")
                };
                let note = base_note(body, at);
                let id = note.id;
                let out = cell.propose(
                    &ProposalInput {
                        note,
                        mode: ProposalMode::New,
                        client: Some("test".into()),
                        reason: Some("perché".into()),
                    },
                    "mcp:test",
                    &engramma_core::cell::proposals::ProposeContext::default(),
                )
                .unwrap(/* test-only */);
                if out.id.is_some() {
                    marker_of.insert(id, marker);
                    queue.push(id);
                }
            }
            3 => {
                // propose a supersede of a living note
                if let Some(&target) = active.last() {
                    let note = base_note(format!("la rettifica {marker} dell'assistente"), at);
                    let id = note.id;
                    let out = cell.propose(
                        &ProposalInput {
                            note,
                            mode: ProposalMode::Supersede(target),
                            client: None,
                            reason: None,
                        },
                        "mcp:test",

                        &engramma_core::cell::proposals::ProposeContext::default(),
                    )
                    .unwrap(/* test-only */);
                    if out.id.is_some() {
                        marker_of.insert(id, marker);
                        queue.push(id);
                    }
                }
            }
            4 | 5 => {
                // decide a queued proposal (accept or reject)
                if !queue.is_empty()
                    && let Some(id) = queue.get(rng.below(queue.len())).copied()
                {
                    let accept = step == 4;
                    let r = cell.decide(
                        &id,
                        &DecisionInput {
                            action: if accept {
                                DecisionAction::Accept { edited: None }
                            } else {
                                DecisionAction::Reject
                            },
                            reason: Some("perché".into()),
                            review: None,
                        },
                        "user@cli",
                    );
                    if r.is_ok() {
                        queue.retain(|x| *x != id);
                        if accept {
                            active.push(id);
                        }
                        decided.push(id);
                    }
                }
            }
            6 => {
                // a human correction of an active note
                if let Some(&id) = active.last() {
                    let body = format!("il fatto {marker} corretto a mano");
                    cell.patch_note(
                        &id,
                        &NotePatch { body: Some(body), ..Default::default() },
                        "cli",
                    )
                    .unwrap(/* test-only */);
                    marker_of.insert(id, marker);
                }
            }
            7 => {
                // a human supersede of an active note
                if let Some(&old) = active.last() {
                    let n = base_note(format!("il fatto {marker} sostituito a mano"), at);
                    cell.supersede(&old, &n, None, "cli").unwrap(/* test-only */);
                    marker_of.insert(n.id, marker);
                    active.retain(|x| *x != old);
                    active.push(n.id);
                }
            }
            _ => {}
        }
        // reverts: after any step, try reverting a live decision
        if rng.below(3) == 0 && !decided.is_empty() {
            let ds = cell.decisions(&DecisionsFilter::default()).unwrap(/* test-only */);
            if let Some(d) = ds.get(rng.below(ds.len().max(1))) {
                let outcome = cell.revert_decision(&d.op_id, "ripensamento", "user@cli");
                match outcome {
                    Ok(engramma_core::cell::proposals::RevertOutcome::Reverted {
                        proposal,
                        ..
                    }) => {
                        // invariant 2: no human op may sit between the
                        // decision and the revert, on the note or the target
                        let ops = cell.ops().unwrap(/* test-only */);
                        let dseq = ops
                            .iter()
                            .find(|o| o.op_id == d.op_id)
                            .map(|o| o.seq)
                            .unwrap_or(i64::MAX);
                        let later: Vec<_> = ops
                            .iter()
                            .filter(|o| o.seq > dseq)
                            .filter(|o| {
                                o.note_id == Some(proposal)
                                    || d.target.is_some_and(|t| o.note_id == Some(t))
                            })
                            .collect();
                        for o in later {
                            prop_assert!(
                                !matches!(
                                    o.kind.as_str(),
                                    "put_note"
                                        | "patch_note"
                                        | "set_status"
                                        | "supersede"
                                        | "delete_note"
                                        | "restore"
                                        | "confirm"
                                ),
                                "revert landed over a later human op: {}",
                                o.kind.as_str()
                            );
                        }
                        queue.push(proposal);
                        active.retain(|x| *x != proposal);
                    }
                    Ok(engramma_core::cell::proposals::RevertOutcome::Purged { .. }) => {}
                    Err(_) => { /* conflict: allowed, expected over later ops */ }
                }
            }
        }

        // invariant 1 (checked continuously): every queued proposal is
        // invisible to the default search, text and graph alike
        for id in &queue {
            let Some(marker) = marker_of.get(id) else {
                continue;
            };
            let hits = cell
                .search(&Query::text(marker).k(10))
                .unwrap(/* test-only */);
            prop_assert!(
                hits.iter().all(|h| h.note.id != *id),
                "queued proposal {id} leaked into the text search"
            );
            let graphed = cell
                .search(&Query::text(marker).k(10).graph_hops(1))
                .unwrap(/* test-only */);
            prop_assert!(
                graphed.iter().all(|h| h.note.id != *id),
                "queued proposal {id} leaked through the graph"
            );
        }
    }

    // invariants 3 and 4: the from-zero replay equals the live cell
    let report = cell.verify().unwrap(/* test-only */);
    prop_assert!(report.ok(), "verify: {:?}", report.mismatch);
    prop_assert!(matches!(report.replay, engramma_core::cell::Replay::Full));

    // and the bulk revert over everything the test decided stays honest:
    // every ACTIVE decision is either revertable or conflicting (the
    // purged ones have no registry row; the already-reverted ones are
    // skipped, not recounted)
    let report = cell
        .revert_decisions(
            &RevertFilter { by: Some("user@cli".into()), ..Default::default() },
            true,
        )
        .unwrap(/* test-only */);
    let active = cell
        .decisions(&DecisionsFilter {
            state: Some(engramma_core::cell::proposals::DecisionState::Active),
            ..Default::default()
        })
        .unwrap(/* test-only */)
        .len();
    prop_assert_eq!(report.reverted + report.conflicts.len(), active);
    Ok(())
}

/// v0.6.18 (F3): the body of `a_supersede_chain_keeps_at_most_one_active_note`
/// in ONE case — the seed derives every input through the same strategies
/// (`seeded_runner`); a failing seed replays here.
fn a_supersede_chain_keeps_at_most_one_active_note_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (seed, steps) = ((any::<u64>()), (proptest::collection::vec(0u8..7, 12..40)))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    a_supersede_chain_keeps_at_most_one_active_note_with(seed, steps)
}

/// v0.6.14 (N3): the property's story on its inputs EXPLICITLY, for the
/// cases history records.
fn a_supersede_chain_keeps_at_most_one_active_note_with(
    seed: u64,
    steps: Vec<u8>,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let dir = test_dir();
    let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t").unwrap(/* test-only */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "t")
        .unwrap(/* test-only */);
    let mut rng = Rng(seed);
    let mut clock = 0_u64;
    let mut counter = 0_u64;

    // the model: what the sequence built
    let mut active: Vec<Ulid> = Vec::new();
    // the queue: (proposal, its supersede target, when any)
    let mut queue: Vec<(Ulid, Option<Ulid>)> = Vec::new();
    // the live decisions: (proposal, its supersede target, whether THIS
    // decision had closed it, whether the target stood ACTIVE then). A
    // reject keeps the target alive and the proposal pointing at it; the
    // revert of an accept restores the target AS IT WAS — active only if
    // it was active then (a quarantined proposal in the queue may be
    // corrected, v0.6.11 K1, and its revert puts it back in the queue)
    let mut decided: Vec<(Ulid, Option<Ulid>, bool, bool)> = Vec::new();
    let mut purged: std::collections::HashSet<Ulid> = std::collections::HashSet::new();

    for step in steps {
        counter += 1;
        clock += 1 + rng.below(3) as u64;
        let at = t(clock);
        let marker = format!("catena{counter}");
        match step {
            0 | 1 => {
                let n = base_note(format!("il fatto {marker} detto dall'utente"), at);
                cell.put_note(&n, "cli").unwrap(/* test-only */);
                active.push(n.id);
            }
            2 => {
                // propose a supersede against a RANDOM active note: the
                // same target collects corrections while it stands
                if !active.is_empty() {
                    let target = active[rng.below(active.len())];
                    let note = base_note(format!("la rettifica {marker} dell'assistente"), at);
                    let id = note.id;
                    let out = cell
                        .propose(
                            &ProposalInput {
                                note,
                                mode: ProposalMode::Supersede(target),
                                client: None,
                                reason: None,
                            },
                            "mcp:test",
                            &engramma_core::cell::proposals::ProposeContext::default(),
                        )
                        .unwrap(/* test-only */);
                    if let Some(id) = out.id {
                        queue.push((id, Some(target)));
                    }
                }
            }
            3 => {
                // accept: the engine refuses it only against a closed
                // target (v0.6.18, F3) and lands it as a NEW note over a
                // purged one (v0.6.11, K1). WHETHER each refusal is
                // justified is the unit tests' job (they pin the
                // trajectories: closed → StaleTarget, active → goes,
                // quarantined → goes, purged → K1, reverted → goes again);
                // this property only keeps the sequence moving — the
                // ORACLE is the invariant below, checked on the REAL cell,
                // which the reopen semantics make exact and the model
                // cannot chase
                if !queue.is_empty() {
                    let i = rng.below(queue.len());
                    let (id, target) = queue[i];
                    if cell
                        .decide(
                            &id,
                            &DecisionInput {
                                action: DecisionAction::Accept { edited: None },
                                reason: Some("perché".into()),
                                review: None,
                            },
                            "user@cli",
                        )
                        .is_ok()
                    {
                        queue.remove(i);
                        active.push(id);
                        let closed = match target {
                            Some(t) if !purged.contains(&t) => {
                                active.retain(|x| *x != t);
                                true
                            }
                            _ => false,
                        };
                        let was_active =
                            target.is_some_and(|t| !purged.contains(&t));
                        decided.push((id, target, closed, was_active));
                    }
                }
            }
            4 => {
                // reject: the lapide, never refused — the proposal keeps
                // pointing at its target, which never moved
                if !queue.is_empty() {
                    let i = rng.below(queue.len());
                    let (id, target) = queue[i];
                    if cell
                        .decide(
                            &id,
                            &DecisionInput {
                                action: DecisionAction::Reject,
                                reason: None,
                                review: None,
                            },
                            "user@cli",
                        )
                        .is_ok()
                    {
                        queue.remove(i);
                        // a revert had made the proposal live again? the
                        // reject tombstones it: it is not active either way
                        active.retain(|x| *x != id);
                        decided.push((id, target, false, false));
                    }
                }
            }
            5 => {
                // revert an ACCEPT decision: the target it had closed
                // reopens. The revert of a REJECT rides the OPEN DEFECT of
                // v0.6.14 (open-defects.txt: it reopens the target another
                // decision had closed) — the declared defect is not this
                // property's oracle, so the reverts here cancel accepts only
                if !decided.is_empty() {
                    let ds = cell
                        .decisions(&DecisionsFilter::default())
                        .unwrap(/* test-only */);
                    let candidate = ds.iter().find(|d| {
                        d.reverted_by.is_none()
                            && decided
                                .iter()
                                .any(|(p, _, closed, _)| *closed && *p == d.proposal)
                    });
                    if let Some(d) = candidate {
                        let proposal = d.proposal;
                        let (target, was_active) = decided
                            .iter()
                            .find(|(p, _, _, _)| *p == proposal)
                            .map(|(_, t, _, was)| (*t, *was))
                            .unwrap_or((None, false));
                        match cell.revert_decision(&d.op_id, "ripensamento", "user@cli") {
                            Ok(engramma_core::cell::proposals::RevertOutcome::Reverted { .. }) => {
                                decided.retain(|(p, _, _, _)| *p != proposal);
                                queue.push((proposal, target));
                                active.retain(|x| *x != proposal);
                                // the revert restores the target AS IT WAS:
                                // active only if it stood active then — a
                                // proposal the accept had corrected goes
                                // back to the queue (quarantined), not live
                                if was_active
                                    && let Some(t) = target
                                    && !purged.contains(&t)
                                    && !active.contains(&t)
                                {
                                    active.push(t);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            6 => {
                // a purge: the chain loses a member — and when the purged
                // note was an ACCEPTED supersede, its closing goes with it
                // and the target goes back to WHAT IT WAS when it was
                // closed (v0.6.10, J1): active only if it stood active —
                // a quarantined proposal the accept had corrected goes
                // back to the queue, and a proposal rejected after the
                // closing stays a tombstone (the lapide wins)
                if !active.is_empty() {
                    let i = rng.below(active.len());
                    let id = active[i];
                    let reopen = decided
                        .iter()
                        .find(|(p, _, _, _)| *p == id)
                        .map(|(_, t, closed, was)| (closed.then_some(*t).flatten(), *was))
                        .unwrap_or((None, false));
                    cell.purge(&id, "oblio", "cli").unwrap(/* test-only */);
                    active.remove(i);
                    purged.insert(id);
                    queue.retain(|(p, _)| *p != id);
                    decided.retain(|(p, _, _, _)| *p != id);
                    if let (Some(t), true) = reopen
                        && !purged.contains(&t)
                        && !active.contains(&t)
                    {
                        active.push(t);
                    }
                }
            }
            _ => {}
        }

        // THE INVARIANT (checked after every step): for every pair of
        // active notes, neither carries a chain that reaches the other
        for b in &active {
            let chain = chain_of(&cell, b);
            for a in &active {
                if a != b {
                    prop_assert!(
                        !chain.contains(a),
                        "two active notes in one supersede chain: {b} → {a}"
                    );
                }
            }
        }
    }

    let report = cell.verify().unwrap(/* test-only */);
    prop_assert!(report.ok(), "verify: {:?}", report.mismatch);
    prop_assert!(matches!(report.replay, engramma_core::cell::Replay::Full));
    Ok(())
}

/// The transitive `supersedes` chain of a note, through the living notes —
/// a purged member ends the walk (its rows are gone, it cannot be active).
fn chain_of(cell: &Cell, root: &Ulid) -> Vec<Ulid> {
    let mut seen = vec![*root];
    let mut i = 0;
    while i < seen.len() && i < 256 {
        let Ok(n) = cell.get(&seen[i]) else {
            break; // purged or unknown: the chain stops here
        };
        for old in &n.supersedes {
            if !seen.contains(old) {
                seen.push(*old);
            }
        }
        i += 1;
    }
    seen
}
