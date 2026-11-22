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
    fn proposal_sequences_keep_the_invariants(
        seed in any::<u64>(),
        steps in proptest::collection::vec(0u8..9, 12..48),
    ) {
        let dir = test_dir();
        let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t").unwrap(/* test-only */);
        cell.set_policy(serde_json::json!({ "write": "propose" }), "t")
            .unwrap(/* test-only */);
        let mut rng = Rng(seed);
        let mut clock = 0_u64;
        let mut counter = 0_u64;

        // the model: what the sequence built
        let mut active: Vec<Ulid> = Vec::new();       // human + accepted notes
        let mut queue: Vec<Ulid> = Vec::new();        // proposals in the queue (pending/reverted)
        let mut marker_of: std::collections::HashMap<Ulid, String> = std::collections::HashMap::new();
        let mut decided: Vec<Ulid> = Vec::new();      // proposals with a live decision

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
                        format!("il fatto marcatore{} detto dall'utente", counter.saturating_sub(1).max(1))
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
                        Ok(engramma_core::cell::proposals::RevertOutcome::Reverted { proposal, .. }) => {
                            // invariant 2: no human op may sit between the
                            // decision and the revert, on the note or the target
                            let ops = cell.ops().unwrap(/* test-only */);
                            let dseq = ops.iter().find(|o| o.op_id == d.op_id).map(|o| o.seq).unwrap_or(i64::MAX);
                            let later: Vec<_> = ops
                                .iter()
                                .filter(|o| o.seq > dseq)
                                .filter(|o| o.note_id == Some(proposal) || d.target.is_some_and(|t| o.note_id == Some(t)))
                                .collect();
                            for o in later {
                                prop_assert!(!matches!(
                                    o.kind.as_str(),
                                    "put_note" | "patch_note" | "set_status" | "supersede"
                                        | "delete_note" | "restore" | "confirm"
                                ), "revert landed over a later human op: {}", o.kind.as_str());
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
                let Some(marker) = marker_of.get(id) else { continue };
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
    }
}
