// SPDX-License-Identifier: Apache-2.0

//! v0.6.11 (K1): la purga si porta via il purgato, mai il contenuto degli
//! altri. La dominanza della v0.6.10 (J1) saltava l'operazione INTERA
//! quando un suo oggetto era purgato; quella regola, scritta dal revisore
//! nella spec, faceva sparire la nota nuova di un supersede quando si
//! purgava la vecchia (le due sonde della verifica, ridotte qui in
//! `purge_own_content.rs`). La regola di oggi è per metà: la metà
//! dell'oggetto purgato non si applica, e nemmeno una metà che è solo un
//! RIFERIMENTO al purgato (la chiusura di una vecchia, di un bersaglio,
//! una riga di arco); il contenuto proprio dell'altro oggetto si applica,
//! senza quello che nomina il purgato. Le eccezioni restano la `purge`
//! stessa (il suo scrub è lo stato che porta) e `revert_decision` (che ha
//! la sua risposta `RevertOutcome::Purged`).
//!
//! Ogni operazione a due oggetti ha qui la sua prova deterministica, con la
//! purga su un lato e la scrittura che rimaterializza l'altro:
//! - `decide` (proposta e bersaglio) — il seme `14903861504949011020`
//!   ridotto a mano, il difetto dichiarato aperto nella v0.6.9;
//! - `decide` con il bersaglio purgato (la riga che nomina l'id dimenticato);
//! - `supersede` (vecchia e nuova);
//! - `link` e `unlink` (i due estremi e la nota che dichiara);
//! - `propose` con un bersaglio (v0.6.11: la proposta resta in coda, il
//!   suo testo è contenuto dell'agente — la prova nuova sta in
//!   `purge_own_content.rs`).

use engramma_core::cell::EdgeInput;
use engramma_core::cell::ops::OpKind;
use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, ProposalInput, ProposalMode, ProposeContext,
};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, NotePatch, Status};
use time::OffsetDateTime;
use ulid::Ulid;

fn at(day: u8) -> At {
    At(
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
            .unwrap(/* test-only */),
    )
}

fn note_of(id: u16, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some(format!("titolo {id}")),
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

/// One supersede-mode stage: a target `1` and a proposal `2` that corrects
/// it, accepted. The decide has closed the target and written the
/// `supersedes` row.
fn decided_stage(name: &str) -> (tempfile::TempDir, Cell, Ulid, Ulid) {
    let dir = test_dir();
    let cell = Cell::create(&dir.path().join(name), "p", "tester").unwrap(/* t */);
    let target = note_of(1, "la nota che il mondo vede");
    let mut proposal = note_of(2, "la rettifica che la corregge");
    proposal.status = Status::Quarantined;
    let (t, p) = (target.id, proposal.id);
    cell.put_note(&target, "tester").unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "tester")
        .unwrap(/* t */);
    cell.propose(
        &ProposalInput {
            note: proposal,
            mode: ProposalMode::Supersede(t),
            client: None,
            reason: None,
        },
        "mcp:tester",
        &ProposeContext::default(),
    )
    .unwrap(/* t */);
    cell.decide(
        &p,
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: Some("accettata".into()),
            review: None,
        },
        "tester",
    )
    .unwrap(/* t */);
    (dir, cell, p, t)
}

fn supersedes_rows(cell: &Cell, named: &Ulid) -> usize {
    cell.debug_column(&format!(
        "SELECT CAST(COUNT(*) AS TEXT) FROM supersedes WHERE new_id = '{named}' OR old_id = '{named}'"
    ))
    .unwrap(/* t */)
    .first()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
}

/// The declared defect (v0.6.9, seed `14903861504949011020`, reduced by
/// hand): a supersede-mode proposal is accepted, the proposal is purged,
/// and a write on the TARGET rematerializes it. The decide is an operation
/// of the target too, so its half replayed and rewrote the `supersedes`
/// row the purge's transitive closure had taken — the v0.6.7 guard asked
/// «is THIS object purged?», and the target is not. The replay diverged
/// from the live state, and verify went red on one cell alone.
///
/// The rule: once the set holds the purge of the proposal, the decide
/// replays NO half — the target is not closed by it anymore, the row does
/// not come back, and the purge of the proposal reopens what the decide
/// had closed.

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

#[test]
fn the_target_half_of_a_decide_does_not_replay_over_the_purge_of_the_proposal_seed_14903() {
    let (_dir, cell, proposal, target) = decided_stage("decide-purga.sqlite");

    // the decide closed the target and wrote the row
    assert_eq!(cell.get(&target).unwrap(/* t */).status, Status::Superseded);
    assert_eq!(supersedes_rows(&cell, &target), 1);

    // the purge of the PROPOSAL: the row goes (its new side is the purged
    // id), and now the decide is dominated — the target's closing must go
    // with it
    cell.purge(&proposal, "diritto all'oblio", "tester").unwrap(/* t */);
    assert_eq!(
        supersedes_rows(&cell, &target),
        0,
        "the purge's closure took the row"
    );
    assert_eq!(
        cell.get(&target).unwrap(/* t */).status,
        Status::Active,
        "the dominated decide closes nothing: the target is open again"
    );

    // the write on the OTHER side — the defect's own road, as the seed
    // walked it: a CONCURRENT patch on the target (an author that had not
    // seen the decide: its base is the put) arrives after the purge, the
    // object rematerializes from the whole set, and the decide's half
    // must not replay in it
    let put_op = cell
        .ops()
        .unwrap(/* t */)
        .iter()
        .find(|op| op.kind == OpKind::PutNote)
        .cloned()
        .unwrap(/* the stage wrote one */);
    let writer = Cell::create(
        &(_dir.path().join("scrittore-14903.sqlite")),
        "p",
        "tester",
    )
    .unwrap(/* t */);
    writer.ingest_op(&put_op).unwrap(/* t */);
    writer
        .patch_note(
            &target,
            &NotePatch {
                body: Some("la nota riscritta dopo l'oblio della rettifica".into()),
                ..Default::default()
            },
            "tester",
        )
        .unwrap(/* t */);
    let concurrent = writer
        .ops()
        .unwrap(/* t */)
        .iter()
        .find(|op| op.kind == OpKind::PatchNote)
        .cloned()
        .unwrap(/* the writer wrote one */);
    cell.ingest_op(&concurrent).unwrap(/* t */);

    assert_eq!(
        supersedes_rows(&cell, &target),
        0,
        "the rematerialization does not rewrite the row"
    );

    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// The decide with the TARGET purged: the same rule, the other side. The
/// row the accept wrote names the purged id, and a write on the PROPOSAL
/// rematerializes it — the decide's half on the proposal re-creates the
/// row that names the forgotten id.
#[test]
fn the_proposal_half_of_a_decide_does_not_replay_over_the_purge_of_the_target() {
    let (_dir, cell, proposal, target) = decided_stage("bersaglio-purga.sqlite");

    cell.purge(&target, "diritto all'oblio", "tester").unwrap(/* t */);
    assert_eq!(
        supersedes_rows(&cell, &target),
        0,
        "the purge took the row that names it"
    );

    // the write on the OTHER side: a CONCURRENT patch on the (accepted,
    // living) proposal — an author that had not seen the decide — makes
    // the object rematerialize from the whole set, and the decide's half
    // must not rewrite the row that names the purged target
    let propose_op = cell
        .ops()
        .unwrap(/* t */)
        .iter()
        .find(|op| op.kind == OpKind::Propose)
        .cloned()
        .unwrap(/* the stage wrote one */);
    let writer = Cell::create(
        &(_dir.path().join("scrittore-bersaglio.sqlite")),
        "p",
        "tester",
    )
    .unwrap(/* t */);
    writer.ingest_op(&propose_op).unwrap(/* t */);
    writer
        .patch_note(
            &proposal,
            &NotePatch {
                body: Some("la rettifica riscritta dopo l'oblio del bersaglio".into()),
                ..Default::default()
            },
            "tester",
        )
        .unwrap(/* t */);
    let concurrent = writer
        .ops()
        .unwrap(/* t */)
        .iter()
        .find(|op| op.kind == OpKind::PatchNote)
        .cloned()
        .unwrap(/* the writer wrote one */);
    cell.ingest_op(&concurrent).unwrap(/* t */);

    assert_eq!(
        supersedes_rows(&cell, &target),
        0,
        "no row may name a purged id, whatever half replays"
    );

    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// `supersede` (old and new): the purge of the NEW note dominates the whole
/// operation — the OLD note's closing was the old half, and it does not
/// stay. The v0.5–v0.6.9 derivation kept the closing replayable (the
/// censored body carried it); the per-operation rule takes it back.
#[test]
fn the_old_half_of_a_supersede_does_not_replay_over_the_purge_of_the_new() {
    let dir = test_dir();
    let cell = Cell::create(&dir.path().join("supersede-purga.sqlite"), "p", "tester")
        .unwrap(/* t */);
    let old = note_of(1, "la nota vecchia");
    let mut new = note_of(2, "la nota nuova");
    new.supersedes = vec![old.id];
    cell.put_note(&old, "tester").unwrap(/* t */);
    cell.supersede(&old.id, &new, None, "tester").unwrap(/* t */);
    assert_eq!(cell.get(&old.id).unwrap(/* t */).status, Status::Superseded);

    cell.purge(&new.id, "diritto all'oblio", "tester").unwrap(/* t */);

    // the dominated supersede closes nothing: the old note is open again,
    // and no row names the purged new id
    assert_eq!(
        cell.get(&old.id).unwrap(/* t */).status,
        Status::Active,
        "the purge of the new note reopens the old one"
    );
    assert_eq!(supersedes_rows(&cell, &new.id), 0);

    // and the write on the surviving side changes nothing about that
    cell.patch_note(
        &old.id,
        &NotePatch {
            body: Some("la vecchia riscritta dopo l'oblio della nuova".into()),
            ..Default::default()
        },
        "tester",
    )
    .unwrap(/* t */);
    assert_eq!(cell.get(&old.id).unwrap(/* t */).status, Status::Active);
    assert_eq!(supersedes_rows(&cell, &new.id), 0);

    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// `link` (the two endpoints and the note it declares): the purge of an
/// endpoint dominates the link — the edge that names the forgotten id does
/// not come back, not even when the link op itself ARRIVES after the purge
/// (the local write is already refused by the v0.6.9 guard; the arrival
/// is the hole this closes).
#[test]
fn a_link_arriving_after_the_purge_of_an_endpoint_does_not_recreate_the_edge() {
    let dir = test_dir();
    let writer = Cell::create(&dir.path().join("scrittore.sqlite"), "p", "tester")
        .unwrap(/* t */);
    // the cited note: the edge's `note_id` — the note that declares it.
    // The endpoints stay ENTITIES (an edge between entities checks no
    // note's existence: the cited note is the purge-relevant object here)
    let cited = note_of(1, "la nota che dichiara il legame");
    writer.put_note(&cited, "tester").unwrap(/* t */);
    let edge = EdgeInput {
        src: "Alfa".into(),
        rel: "rel_x".into(),
        dst: "Beta".into(),
        note_id: Some(cited.id),
        valid_from: None,
        valid_to: None,
        confidence: Confidence::Stated,
    };
    writer.link(&edge, "tester").unwrap(/* t */);

    // the reader holds the note, purges it, and only then receives the
    // link
    let reader = Cell::create(&dir.path().join("lettore.sqlite"), "p", "tester")
        .unwrap(/* t */);
    for op in writer.ops().unwrap(/* t */).iter().skip(1) {
        if op.kind == OpKind::PutNote {
            reader.ingest_op(op).unwrap(/* t */);
        }
    }
    reader.purge(&cited.id, "diritto all'oblio", "tester").unwrap(/* t */);
    let link_op = writer
        .ops()
        .unwrap(/* t */)
        .iter()
        .find(|op| op.kind == OpKind::Link)
        .cloned()
        .unwrap(/* the stage wrote one */);
    reader.ingest_op(&link_op).unwrap(/* t */);

    let edges: Vec<String> = reader
        .debug_column("SELECT CAST(id AS TEXT) FROM edges WHERE note_id IS NOT NULL")
        .unwrap(/* t */);
    assert!(
        edges.is_empty(),
        "no edge may cite a purged note: {edges:?}"
    );
    let pending: Vec<String> = reader
        .debug_column("SELECT object FROM pending_objects WHERE object LIKE 'edge:%'")
        .unwrap(/* t */);
    assert!(
        pending.is_empty(),
        "a dominated link does not wait: {pending:?}"
    );

    let report = reader.verify().unwrap(/* t */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// `unlink`: an unlink whose edge cites a purged endpoint is dominated —
/// it is recorded, it does not wait (the edge it names will never
/// materialize again).
#[test]
fn an_unlink_of_an_edge_that_cited_a_purged_note_does_not_wait() {
    let dir = test_dir();
    let writer = Cell::create(&dir.path().join("scollega-scr.sqlite"), "p", "tester")
        .unwrap(/* t */);
    let left = note_of(1, "un capo del legame che finisce");
    let right = note_of(2, "l'altro capo del legame che finisce");
    writer.put_note(&left, "tester").unwrap(/* t */);
    writer.put_note(&right, "tester").unwrap(/* t */);
    let edge = EdgeInput {
        src: format!("note:{}", left.id),
        rel: "rel_x".into(),
        dst: format!("note:{}", right.id),
        note_id: None,
        valid_from: None,
        valid_to: None,
        confidence: Confidence::Stated,
    };
    let edge_id = writer.link(&edge, "tester").unwrap(/* t */);
    writer.unlink(&edge_id, "tester").unwrap(/* t */);

    let reader = Cell::create(&dir.path().join("scollega-leg.sqlite"), "p", "tester")
        .unwrap(/* t */);
    for op in writer.ops().unwrap(/* t */).iter().skip(1) {
        if op.kind == OpKind::PutNote {
            reader.ingest_op(op).unwrap(/* t */);
        }
    }
    // the realistic order: the reader holds the LINK (the edge exists),
    // purges the endpoint, and only then receives the unlink
    let link_op = writer
        .ops()
        .unwrap(/* t */)
        .iter()
        .find(|op| op.kind == OpKind::Link)
        .cloned()
        .unwrap(/* the stage wrote one */);
    reader.ingest_op(&link_op).unwrap(/* t */);
    assert_eq!(
        reader
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM edges")
            .unwrap(/* t */),
        vec!["1".to_string()],
        "the stage created the edge"
    );
    reader.purge(&right.id, "diritto all'oblio", "tester").unwrap(/* t */);
    let unlink_op = writer
        .ops()
        .unwrap(/* t */)
        .iter()
        .find(|op| op.kind == OpKind::Unlink)
        .cloned()
        .unwrap(/* the stage wrote one */);
    reader.ingest_op(&unlink_op).unwrap(/* t */);

    let pending: Vec<String> = reader
        .debug_column("SELECT object FROM pending_objects WHERE object LIKE 'edge:%'")
        .unwrap(/* t */);
    assert!(
        pending.is_empty(),
        "a dominated unlink does not wait: {pending:?}"
    );

    let report = reader.verify().unwrap(/* t */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// `propose` with a target — v0.6.11 (K1), CORRECTED: the v0.6.10 rule
/// («the dominated propose materializes nothing: the proposal goes»)
/// encoded the wrong behavior this file was born from. The proposal's
/// text is the agent's content, and the user must be able to see it and
/// refuse it: the purge of the target leaves it in the queue, and no
/// rematerialization may lose it. The full new behavior (the queue stays
/// decidable, an accept lands a new note) is in `purge_own_content.rs`;
/// what stays HERE is the purge-dominance half this file guards: no row
/// and no edge may name the purged target, whatever half replays.
#[test]
fn a_propose_for_a_purged_target_keeps_its_queue_row_and_names_nothing_purged() {
    let dir = test_dir();
    let cell = Cell::create(&dir.path().join("proposta-purga.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let target = note_of(1, "la nota che il mondo correggeva");
    let mut proposal = note_of(2, "la rettifica orfana");
    proposal.status = Status::Quarantined;
    cell.put_note(&target, "tester").unwrap(/* test-only */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "tester")
        .unwrap(/* test-only */);
    cell.propose(
        &ProposalInput {
            note: proposal,
            mode: ProposalMode::Supersede(target.id),
            client: None,
            reason: None,
        },
        "mcp:tester",
        &ProposeContext::default(),
    )
    .unwrap(/* test-only */);
    assert!(
        cell.note_ids_all().unwrap(/* test-only */).contains(&proposal_id(&cell, 2)),
        "the stage wrote the proposal"
    );

    cell.purge(&target.id, "diritto all'oblio", "tester").unwrap(/* test-only */);

    let pid = proposal_id(&cell, 2);
    assert!(
        cell.note_ids_all().unwrap(/* test-only */).contains(&pid),
        "the proposal's text survives the purge of its target"
    );
    let queue: Vec<String> = cell
        .debug_column(&format!(
            "SELECT CAST(COUNT(*) AS TEXT) FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* test-only */);
    assert_eq!(
        queue.first().map(String::as_str),
        Some("1"),
        "the queue still holds the proposal for the purged target"
    );
    let rows: Vec<String> = cell
        .debug_column(&format!(
            "SELECT CAST(COUNT(*) AS TEXT) FROM supersedes WHERE new_id = '{pid}' OR old_id = '{}'",
            target.id
        ))
        .unwrap(/* test-only */);
    assert_eq!(
        rows.first().map(String::as_str),
        Some("0"),
        "no row may name the purged target"
    );

    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// The proposal's id as the note_of(n) helper mints it (the row is gone
/// after the purge, so it cannot be read from the cell).
fn proposal_id(_cell: &Cell, n: u16) -> Ulid {
    Ulid::from_string(&format!("01{:024}", n)).unwrap(/* test-only */)
}
