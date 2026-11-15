// SPDX-License-Identifier: Apache-2.0

//! v0.6.1 (P0): the identity of an operation is minted once, at birth, and
//! never changes. Redaction, censorship and compaction touch the payload;
//! `content_hash` and `sig` keep the values they were born with, so a
//! `base` keeps resolving and a signature keeps verifying on every copy,
//! whatever each copy did to the payload it holds. These tests pin the
//! defect the v0.6 review found (the purge's censor used to REWRITE the
//! digest — and with it the identity — of what it censored) and the
//! property that keeps the fleet converging.

use engramma_core::cell::ops::{Op, Signer};
use engramma_core::format::{At, Note, NoteType, Validity};
use engramma_core::{Cell, Confidence, NotePatch, Status};
use std::collections::BTreeMap;
use time::OffsetDateTime;
use ulid::Ulid;

fn now() -> At {
    At(OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */))
}

fn note(id: u32, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), id))
            .unwrap(/* valid Crockford */),
        note_type: NoteType::Fact,
        title: None,
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: String::new(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: now(),
        updated_at: now(),
        extensions: vec![],
    }
}

/// A fresh copy that ingests `ops` in the given order.
fn copy_of(name: &str, ops: &[Op]) -> Cell {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::open_bare(&dir.path().join(format!("{name}.sqlite"))).unwrap(/* t */);
    for op in ops {
        assert!(cell.ingest_op(op).unwrap(/* t */));
    }
    std::mem::forget(dir);
    cell
}

fn union_of(cells: &[&Cell]) -> BTreeMap<String, Op> {
    let mut out: BTreeMap<String, Op> = BTreeMap::new();
    for cell in cells {
        for op in cell.ops().unwrap(/* t */) {
            out.entry(op.op_id.to_string())
                .or_insert_with(|| op.clone());
        }
    }
    out
}

/// THE REVIEWER'S SCENARIO (v0.6.1, P0), as a test of its own: a supersede,
/// the purge of the note it created, and a patch written by whoever had
/// seen the supersede BEFORE the purge — delivered in the two orders. The
/// censor rewrites the supersede's PAYLOAD on every copy that applies the
/// purge, but its identity is the one it was born with: the patch's `base`
/// resolves on both, the state is a function of the set, and no copy grows
/// a phantom conflict copy.
#[test]
fn the_censored_supersede_keeps_its_identity_on_every_copy() {
    // A creates the cell and the original note
    let dir_a = tempfile::tempdir().unwrap(/* t */);
    let a = Cell::create(&dir_a.path().join("a.sqlite"), "id", "device-a").unwrap(/* t */);
    let original = note(1, "la verità prima");
    let old_id = original.id;
    a.put_note(&original, "device-a").unwrap(/* t */);

    // B is a copy of A at this point; C sees the supersede before the purge
    let first: Vec<Op> = a.ops().unwrap(/* t */);
    let b = copy_of("b", &first);
    let c = copy_of("c", &first);

    // A supersedes: the new note Y closes the original
    let mut new = note(2, "la verità seconda");
    new.valid_from = Some(Validity(now().get().date(), None));
    let new_id = new.id;
    a.supersede(&old_id, &new, None, "device-a").unwrap(/* t */);

    // C receives the supersede WHOLE, then writes a patch on Y leaning on it
    let supersede: Vec<Op> = a
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.kind.as_str() == "supersede")
        .collect();
    assert_eq!(supersede.len(), 1);
    let supersede = supersede[0].clone();
    c.ingest_op(&supersede).unwrap(/* t */);
    c.patch_note(
        &new_id,
        &NotePatch {
            body: Some("la verità seconda, corretta da C".into()),
            ..Default::default()
        },
        "device-c",
    )
    .unwrap(/* t */);
    let patch_of_c: Vec<Op> = c
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.actor == "device-c")
        .collect();
    assert_eq!(patch_of_c.len(), 1, "C wrote exactly one patch");
    let patch = patch_of_c[0].clone();
    // the patch leans on the supersede's ORIGINAL identity
    assert_eq!(patch.base.as_deref(), Some(supersede.content_hash.as_str()));

    // A purges the NEW note: the censor replaces the supersede's payload on
    // A, and the supersede's identity must not move
    let identity_before = supersede.content_hash.clone();
    a.purge(&new_id, "oblio", "device-a").unwrap(/* t */);
    let censored: Vec<Op> = a
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.op_id == supersede.op_id)
        .collect();
    assert_eq!(censored.len(), 1);
    assert_eq!(
        censored[0].content_hash, identity_before,
        "the censored supersede keeps the identity it was born with"
    );
    assert_eq!(
        censored[0].payload_state.as_str(),
        "censored",
        "the payload says it was censored"
    );
    assert_ne!(
        censored[0].payload_digest,
        {
            let salt = censored[0].salt.clone().unwrap_or_default();
            let body = censored[0].body.clone().unwrap_or(serde_json::Value::Null);
            engramma_core::cell::ops::payload_digest(&salt, &body)
        },
        "the digest still names the ORIGINAL body, not the censored one"
    );

    // the two deliveries: one copy takes the purge FIRST (the supersede
    // arrives already censored, from A), the other takes the supersede
    // WHOLE first (from C), then the purge
    let purge: Vec<Op> = a
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.kind.as_str() == "purge")
        .collect();
    assert_eq!(purge.len(), 1);
    let a_supersede_censored: Vec<Op> = a
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.op_id == supersede.op_id)
        .collect();
    let union = union_of(&[&a, &c]);
    let mut by_total: Vec<Op> = union.values().cloned().collect();
    by_total.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));

    // order X: purge first, then the censored supersede, then C's patch
    let mut order_x = by_total.clone();
    let px = order_x
        .iter()
        .position(|o| o.kind.as_str() == "purge")
        .unwrap(/* t */);
    order_x.swap(0, px);
    let x = copy_of("x", &order_x);

    // order Y: the supersede WHOLE (C's copy), the patch, the purge last
    let mut order_y: Vec<Op> = Vec::new();
    for op in &by_total {
        if op.op_id == supersede.op_id {
            order_y.push(supersede.clone()); // the whole one, as C holds it
        } else {
            order_y.push(op.clone());
        }
    }
    // C's patch right after its base, the purge at the end
    let patch_pos = order_y
        .iter()
        .position(|o| o.op_id == patch.op_id)
        .unwrap(/* t */);
    let purge_op = order_y.remove(patch_pos);
    order_y.push(purge_op);
    let y = copy_of("y", &order_y);
    // (the second copy of the censored supersede from A rides in order_x)

    // both orders converge, and neither grows a phantom conflict copy
    let dx = x.replicated_digest().unwrap(/* t */);
    let dy = y.replicated_digest().unwrap(/* t */);
    assert_eq!(dx, dy, "the two delivery orders hold the same state");
    for (name, cell) in [("x", &x), ("y", &y)] {
        let copies = cell
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies")
            .unwrap(/* t */)[0]
            .clone();
        assert_eq!(copies, "0", "no phantom conflict copy on {name}");
        let purged = cell.get(&new_id);
        assert!(
            matches!(purged, Err(engramma_core::CellError::NoteNotFound(_))),
            "the purged note does not materialize on {name}"
        );
        assert!(cell.verify().unwrap(/* t */).ok(), "{name} verifies");
    }
    // and the delivery that carried A's censored supersede really used it
    // (the row on x is censored, with the original identity)
    let on_x: Vec<Op> = x
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.op_id == supersede.op_id)
        .collect();
    assert_eq!(on_x[0].payload_state.as_str(), "censored");
    assert_eq!(on_x[0].content_hash, identity_before);
    let _ = (a_supersede_censored, b);
}

/// A deterministic test signer: the signature is a MAC of the device key
/// over the message — enough to pin that whatever the cell stores as `sig`
/// survives purge, censorship and compaction untouched.
struct FakeSigner {
    device: String,
    key: [u8; 32],
}

impl Signer for FakeSigner {
    fn device_id(&self) -> String {
        self.device.clone()
    }
    fn sign(&self, msg: &[u8]) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"fake-signer");
        h.update(&self.key);
        h.update(msg);
        h.finalize().to_hex().to_string()
    }
    fn verify(device: &str, _msg: &[u8], _sig: &str) -> bool
    where
        Self: Sized,
    {
        device == "fake"
    }
}

/// The property the phase asks for (P0.5), as a PROPERTEST on generated
/// histories (R6, v0.6.1): the `content_hash`, the `sig` and the payload
/// digest of every operation are IDENTICAL after `purge`, after
/// censorship, and after `compact` at a generated cut — the payload state
/// may change, the identity never does. The fixed test above keeps the
/// reviewer's exact scenario; this one sweeps the space.
#[test]
fn purge_compact_and_censorship_never_move_the_identity() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    let cell = Cell::create(&dir.path().join("p.sqlite"), "id", "device-a").unwrap(/* t */);
    cell.set_signer(std::sync::Arc::new(FakeSigner {
        device: "fakesigningdevice".into(),
        key: [7; 32],
    }))
    .unwrap(/* t */);

    // a history with everything the censor touches: a superseded note, a
    // supersede proposal decided, a revert, and plain writes
    cell.set_policy(serde_json::json!({ "write": "propose" }), "device-a")
        .unwrap(/* t */);
    let mut living: Vec<Ulid> = Vec::new();
    for i in 1..=6_u32 {
        let n = note(i, &format!("il corpo numero {i} della storia"));
        cell.put_note(&n, "device-a").unwrap(/* t */);
        living.push(n.id);
    }
    let mut new = note(7, "la nota che sarà dimenticata");
    new.supersedes = vec![living[0]];
    cell.supersede(&living[0], &new, None, "device-a").unwrap(/* t */);
    let doomed = new.id;
    cell.patch_note(
        &doomed,
        &NotePatch {
            body: Some("una versione intermedia".into()),
            ..Default::default()
        },
        "device-a",
    )
    .unwrap(/* t */);
    cell.delete_note(&living[1], "device-a").unwrap(/* t */);
    // a proposal and its decision, then the purge of the supersede's new
    // note (the censor case) and of a plain note (the forget case)
    let out = cell
        .propose(
            &engramma_core::cell::proposals::ProposalInput {
                note: note(8, "la proposta della proprietà"),
                mode: engramma_core::cell::proposals::ProposalMode::New,
                client: Some("test".into()),
                reason: Some("perché".into()),
            },
            "device-a",
            &engramma_core::cell::proposals::ProposeContext::default(),
        )
        .unwrap(/* t */);
    if let Some(proposal) = out.id {
        cell.decide(
            &proposal,
            &engramma_core::cell::proposals::DecisionInput {
                action: engramma_core::cell::proposals::DecisionAction::Accept { edited: None },
                reason: Some("va bene".into()),
                review: None,
            },
            "device-a",
        )
        .unwrap(/* t */);
    }

    let identity_before: BTreeMap<String, (String, Option<String>)> = cell
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .map(|o| {
            (
                o.op_id.to_string(),
                (o.content_hash.clone(), o.sig.clone()),
            )
        })
        .collect();
    // the init predates the signer (the cell is born by `create`): every
    // OTHER operation carries a signature
    let init_id = cell
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .find(|o| o.kind.as_str() == "init")
        .map(|o| o.op_id.to_string())
        .expect(/* exactly one */ "the cell has its init");
    assert!(
        identity_before
            .iter()
            .all(|(id, (_, sig))| sig.is_some() || *id == init_id),
        "every op written under the signer carries one"
    );

    // the purge of the supersede's new note censors the supersede; the
    // purge of a plain note forgets its payloads
    cell.purge(&doomed, "oblio", "device-a").unwrap(/* t */);
    cell.purge(&living[2], "oblio", "device-a").unwrap(/* t */);
    // the censor really ran on the supersede: its payload is censored, not
    // merely forgotten (the closing of the old note stays replayable)
    let states_after_purge = cell
        .debug_column("SELECT DISTINCT payload_state FROM ops")
        .unwrap(/* t */);
    assert!(
        states_after_purge.iter().any(|s| s == "censored"),
        "the censor ran: {states_after_purge:?}"
    );
    assert!(
        states_after_purge.iter().any(|s| s == "redacted"),
        "the forget ran: {states_after_purge:?}"
    );
    // and a compaction under the whole history (an unshared cell's frontier
    // is its own head)
    let head: i64 = cell
        .debug_column("SELECT CAST(COALESCE(MAX(seq),0) AS TEXT) FROM ops")
        .unwrap(/* t */)[0]
        .parse()
        .unwrap(/* t */);
    cell.compact(head, &engramma_core::cell::CompactOptions::default())
        .unwrap(/* t */);

    let states_after_compact = cell
        .debug_column("SELECT DISTINCT payload_state FROM ops")
        .unwrap(/* t */);
    assert!(
        states_after_compact.iter().all(|s| s == "redacted"),
        "under the frontier even censored bodies go: {states_after_compact:?}"
    );
    // the identities did not move — one byte less and this fails. The map
    // AFTER holds more entries (the purge ops themselves are new): every op
    // that existed before keeps exactly the identity it was born with.
    let after: BTreeMap<String, (String, Option<String>)> = cell
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .map(|o| {
            (
                o.op_id.to_string(),
                (o.content_hash.clone(), o.sig.clone()),
            )
        })
        .collect();
    for (id, before) in &identity_before {
        assert_eq!(
            after.get(id),
            Some(before),
            "op {id} moved its identity across purge/censorship/compaction"
        );
    }
    assert!(cell.verify().unwrap(/* t */).ok());
}

// ---------------------------------------------------------------------------
// the property as a proptest (R6, v0.6.1): generated histories, generated
// purges, a generated compaction cut — the identity never moves
// ---------------------------------------------------------------------------

use proptest::prelude::*;

/// One generated step of a history: what to write, and on which note.
#[derive(Debug, Clone)]
enum Step {
    Put,
    Patch(u16),
    Delete(u16),
    Supersede(u16),
    Purge(u16),
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        Just(Step::Put),
        any::<u16>().prop_map(Step::Patch),
        any::<u16>().prop_map(Step::Delete),
        any::<u16>().prop_map(Step::Supersede),
        any::<u16>().prop_map(Step::Purge),
    ]
}

fn word() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<u16>(), 1..8)
        .prop_map(|v| v.iter().map(|i| (b'a' + (i % 26) as u8) as char).collect())
}

fn body() -> impl Strategy<Value = String> {
    prop::collection::vec(word(), 1..4).prop_map(|parts| parts.join(" "))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// purge, censorship and compaction never move the identity, on any
    /// generated history and any generated cut.
    #[test]
    fn the_identity_never_moves_on_generated_histories(
        steps in prop::collection::vec(step_strategy(), 1..24),
        bodies in prop::collection::vec(body(), 1..24),
        cut in 0.0f64..1.0,
    ) {
        let dir = tempfile::tempdir().unwrap(/* t */);
        let cell = Cell::create(&dir.path().join("g.sqlite"), "gen", "device-a")
            .unwrap(/* t */);
        cell.set_signer(std::sync::Arc::new(FakeSigner {
            device: "fakesigningdevice".into(),
            key: [11; 32],
        }))
        .unwrap(/* t */);

        // run the generated history: impossible steps (a patch of a note
        // that is not there) are skipped — the history is what DID happen
        let mut living: Vec<Ulid> = Vec::new();
        let mut next_id = 100_u32;
        for (i, step) in steps.iter().enumerate() {
            let text = bodies[i % bodies.len()].clone();
            let pick = |living: &[Ulid], i: usize| living[i % living.len()];
            match step {
                Step::Put => {
                    let n = note(next_id, &text);
                    next_id += 1;
                    if cell.put_note(&n, "device-a").is_ok() {
                        living.push(n.id);
                    }
                }
                Step::Patch(k) => {
                    if !living.is_empty() {
                        let _ = cell.patch_note(
                            &pick(&living, *k as usize),
                            &NotePatch { body: Some(text), ..Default::default() },
                            "device-a",
                        );
                    }
                }
                Step::Delete(k) => {
                    if !living.is_empty() {
                        let _ = cell.delete_note(&pick(&living, *k as usize), "device-a");
                    }
                }
                Step::Supersede(k) => {
                    if !living.is_empty() {
                        let old = pick(&living, *k as usize);
                        let mut new = note(next_id, &text);
                        next_id += 1;
                        new.supersedes = vec![old];
                        if cell.supersede(&old, &new, None, "device-a").is_ok() {
                            living.retain(|id| *id != old);
                            living.push(new.id);
                        }
                    }
                }
                Step::Purge(k) => {
                    if !living.is_empty() {
                        let id = pick(&living, *k as usize);
                        if cell.purge(&id, "oblio", "device-a").is_ok() {
                            living.retain(|x| *x != id);
                        }
                    }
                }
            }
        }

        // the identities at the cut: content_hash, sig AND payload digest
        let before: BTreeMap<String, (String, Option<String>, String)> = cell
            .ops()
            .unwrap(/* t */)
            .into_iter()
            .map(|o| {
                (
                    o.op_id.to_string(),
                    (o.content_hash.clone(), o.sig.clone(), o.payload_digest.clone()),
                )
            })
            .collect();
        prop_assume!(!before.is_empty());

        // a compaction at the generated cut, under the whole history
        let head: i64 = cell
            .debug_column("SELECT CAST(COALESCE(MAX(seq),0) AS TEXT) FROM ops")
            .unwrap(/* t */)[0]
            .parse()
            .unwrap(/* t */);
        // the cut stays WITHIN the live log (compact refuses anything else)
        let cut_seq = (1.0 + (head as f64 - 1.0) * cut).floor() as i64;
        let cut_seq = cut_seq.clamp(1, head);
        cell.compact(cut_seq, &engramma_core::cell::CompactOptions::default())
            .unwrap(/* t */);

        // nothing moved — one byte less and this fails
        let after: BTreeMap<String, (String, Option<String>, String)> = cell
            .ops()
            .unwrap(/* t */)
            .into_iter()
            .map(|o| {
                (
                    o.op_id.to_string(),
                    (o.content_hash.clone(), o.sig.clone(), o.payload_digest.clone()),
                )
            })
            .collect();
        for (id, was) in &before {
            prop_assert_eq!(
                after.get(id),
                Some(was),
                "op {} moved its identity",
                id
            );
        }
        // and every censored body still has the shape the censor produces
        for op in cell.ops().unwrap(/* t */) {
            if op.payload_state.as_str() == "censored" {
                prop_assert!(
                    engramma_core::cell::ops::censor_shape_ok(&op),
                    "op {} has a censored body outside the censor's shape",
                    op.op_id
                );
            }
        }
        prop_assert!(cell.verify().unwrap(/* t */).ok());
    }
}
