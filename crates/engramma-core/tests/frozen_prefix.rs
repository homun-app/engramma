// SPDX-License-Identifier: Apache-2.0

//! v0.6.12 (L1): **il prefisso congelato è immutabile.** L'istantanea al
//! taglio di compattazione è un fatto, non una cache: ciò che è nato a o
//! sotto il taglio — le righe di versione, l'ancora della
//! rimaterializzazione, lo stato al taglio — non si cancella e non si
//! riscrive mai. Una rimaterializzazione rigioca solo le operazioni
//! sopra il taglio, e la storia che il taglio ha congelato resta byte per
//! byte. Solo la purga toglie righe dal prefisso — la famiglia del
//! purgato e i riferimenti a lui, mai il contenuto proprio di un'altra
//! nota.
//!
//! Questi test sono i ROSSI della fase (vanno in verde con la
//! correzione): i semi del difetto aperto `VersionNotFound` in forma
//! deterministica, la **sonda della storia** chiesta dal revisore della
//! v0.6.11 (una purga riuscita toglieva in silenzio le versioni di
//! un'altra nota?), e l'elenco delle operazioni a due oggetti della
//! v0.6.11, una per famiglia: «compatta, poi purga l'uno» — la storia
//! dell'altro resta identica.

mod common;

use common::{cell_with, shared_init, union};
use engramma_core::cell::CompactOptions;
use engramma_core::cell::ops::{OpKind, OpParts, make_op};
use engramma_core::cell::proposals::{DecisionAction, DecisionInput, ProposalInput, ProposalMode};
use engramma_core::{Cell, Confidence, NotePatch, Status};
use std::sync::atomic::Ordering;
use ulid::Ulid;

/// The full version history of one note — numbers, recording op and body
/// — as the probe photographs it: the list a `restore` walks, the list
/// the user reads. Identical before and after a purge outside the family.
fn history_of(cell: &Cell, id: &Ulid) -> Vec<String> {
    cell.debug_column(&format!(
        "SELECT CAST(version AS TEXT) || ' @' || substr(op_id, 1, 10) \
           || ' corpo=' || json_extract(snapshot_json, '$.body')
         FROM note_versions WHERE note_id = '{id}' ORDER BY version"
    ))
    .unwrap_or_default()
}

/// The cell's own head, as the JS binding reads it for `head - keepLast`.
fn head_of(cell: &Cell) -> i64 {
    cell.debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM ops")
        .unwrap_or_default()
        .first()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn cut_at(cell: &Cell, cut: i64) {
    cell.compact(cut, &CompactOptions { keep_snapshot: false })
        .unwrap(/* test-only: the cut is within the log */);
}

/// La sonda della storia, chiesta dal revisore della v0.6.11: una purga
/// RIUSCITA toglie in silenzio le versioni di un'altra nota? Due note
/// legate da una `supersede` (l'operazione a due oggetti), alcune
/// versioni della vecchia sotto il taglio, la compattazione, la purga
/// della nuova. La storia della vecchia — l'elenco completo, numeri e
/// corpi — deve essere identica a prima (la riapertura è l'eccezione
/// dichiarata sullo STATO, non sulla storia).
#[test]
fn purge_of_the_new_note_leaves_the_old_ones_history_whole() {
    let dir = common::test_dir();
    let cell = Cell::create(&dir.path().join("sonda.sqlite"), "p", "test")
        .unwrap(/* t */);
    let old = common::base_note(1, "la prima mano [[Roma]]");
    cell.put_note(&old, "user:test").unwrap(/* t */);
    cell.patch_note(
        &old.id,
        &NotePatch {
            body: Some("la seconda mano [[Roma]]".into()),
            ..Default::default()
        },
        "user:test",
    )
    .unwrap(/* t */);
    cell.patch_note(
        &old.id,
        &NotePatch {
            body: Some("la terza mano [[Roma]]".into()),
            ..Default::default()
        },
        "user:test",
    )
    .unwrap(/* t */);
    // the cut freezes the note's whole life so far
    cut_at(&cell, head_of(&cell));
    let before = history_of(&cell, &old.id);
    assert_eq!(before.len(), 2, "two photographs under the cut");

    // the two-object operation, born ABOVE the cut: the new note closes
    // the old one (the closing photographs the old side's pre-state)
    let mut new = common::base_note(2, "la nota nuova [[Roma]]");
    new.supersedes = vec![old.id];
    cell.supersede(&old.id, &new, None, "user:test").unwrap(/* t */);
    let shot = history_of(&cell, &old.id);
    assert_eq!(shot.len(), 3, "the closing recorded its pre-state");

    // the purge of the NEW note: the old one reopens (declared), and its
    // history must be EXACTLY what it was
    cell.purge(&new.id, "prop", "user:test").unwrap(/* t */);
    let after = history_of(&cell, &old.id);
    assert_eq!(
        before.iter().take(2).collect::<Vec<_>>(),
        after.iter().take(2).collect::<Vec<_>>(),
        "the frozen prefix lost rows"
    );
    assert_eq!(
        shot, after,
        "the purge of the new note changed the old one's history"
    );
    let live = cell.get(&old.id).unwrap(/* t */);
    assert_eq!(live.status, Status::Active, "the declared reopening");
    assert_eq!(live.body, "la terza mano [[Roma]]");
}

/// Il difetto aperto in forma chirurgica: la purga della nota NUOVA di un
/// supersede, con un `restore` sopra il taglio che nomina la versione che
/// la chiusura aveva fotografato. Sulla v0.6.11 la purga fallisce per
/// sempre (`VersionNotFound`); deve riuscire, e la cella resta leggibile.
#[test]
fn compact_then_purge_of_the_new_note_succeeds() {
    let dir = common::test_dir();
    let cell = Cell::create(&dir.path().join("chirurgo.sqlite"), "p", "test")
        .unwrap(/* t */);
    let old = common::base_note(1, "la prima mano [[Roma]]");
    cell.put_note(&old, "user:test").unwrap(/* t */);
    cell.patch_note(
        &old.id,
        &NotePatch {
            body: Some("la seconda mano [[Roma]]".into()),
            ..Default::default()
        },
        "user:test",
    )
    .unwrap(/* t */);
    cell.patch_note(
        &old.id,
        &NotePatch {
            body: Some("la terza mano [[Roma]]".into()),
            ..Default::default()
        },
        "user:test",
    )
    .unwrap(/* t */);
    // the two-object operation: the closing photographs the old side's
    // pre-state (version 3), then a restore above the closing names it
    let mut new = common::base_note(2, "la nota nuova [[Roma]]");
    new.supersedes = vec![old.id];
    cell.supersede(&old.id, &new, None, "user:test").unwrap(/* t */);
    let history = cell.history(&old.id).unwrap(/* t */);
    let v = history.last().unwrap(/* t */).version;
    cell.restore(&old.id, v, "user:test").unwrap(/* t */);
    // the cut below the note's whole life: the replay of the purge walks
    // the supersede and the restore from zero
    cut_at(&cell, 1);

    let report = cell
        .purge(&new.id, "prop", "user:test")
        .expect("the purge of the new note must succeed");
    assert!(report.scrubbed, "the scrub earned its name");
    let live = cell.get(&old.id).unwrap(/* t */);
    assert_eq!(live.status, Status::Active, "the declared reopening");
    assert_eq!(live.body, "la terza mano [[Roma]]");
    // the cell still verifies after the purge
    let verify = cell.verify().unwrap(/* t */);
    assert!(verify.ok(), "verify after the purge: {:?}", verify.mismatch);
}

/// `supersede`, l'altro verso: la purga della VECCHIA. La nuova nasce e
/// vive (la riga che la nominava va), e la sua storia resta identica.
#[test]
fn purge_of_the_old_note_leaves_the_new_ones_history_whole() {
    let dir = common::test_dir();
    let cell = Cell::create(&dir.path().join("sup-volta.sqlite"), "p", "test")
        .unwrap(/* t */);
    let old = common::base_note(1, "la vecchia [[Roma]]");
    cell.put_note(&old, "user:test").unwrap(/* t */);
    let mut new = common::base_note(2, "la nuova [[Roma]]");
    new.supersedes = vec![old.id];
    cell.supersede(&old.id, &new, None, "user:test").unwrap(/* t */);
    cell.patch_note(
        &new.id,
        &NotePatch {
            body: Some("la nuova, ritoccata [[Roma]]".into()),
            ..Default::default()
        },
        "user:test",
    )
    .unwrap(/* t */);
    cut_at(&cell, head_of(&cell));
    let before = history_of(&cell, &new.id);
    assert_eq!(before.len(), 1, "the patch recorded its pre-state");

    cell.purge(&old.id, "prop", "user:test").unwrap(/* t */);
    assert_eq!(
        before,
        history_of(&cell, &new.id),
        "the purge of the old note changed the new one's history"
    );
    let live = cell.get(&new.id).unwrap(/* t */);
    assert_eq!(live.status, Status::Active, "the new note lives");
    assert_eq!(live.body, "la nuova, ritoccata [[Roma]]");
}

/// `decide` (proposta e bersaglio), primo verso: la purga della PROPOSTA.
/// Il bersaglio si riapre (dichiarato: la chiusura era opera della
/// proposta) e la sua storia resta identica.
#[test]
fn purge_of_the_proposal_leaves_the_targets_history_whole() {
    let dir = common::test_dir();
    let cell = Cell::create(&dir.path().join("dec-p.sqlite"), "p", "test")
        .unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "test")
        .unwrap(/* t */);
    let target = common::base_note(1, "il bersaglio [[Roma]]");
    cell.put_note(&target, "user:test").unwrap(/* t */);
    cell.patch_note(
        &target.id,
        &NotePatch {
            body: Some("il bersaglio, ritoccato [[Roma]]".into()),
            ..Default::default()
        },
        "user:test",
    )
    .unwrap(/* t */);
    cut_at(&cell, head_of(&cell));

    let mut p = common::base_note(7, "la correzione proposta [[Roma]]");
    p.status = Status::Quarantined;
    let out = cell
        .propose(
            &ProposalInput {
                note: p,
                mode: ProposalMode::Supersede(target.id),
                client: None,
                reason: None,
            },
            "mcp:test",
            &Default::default(),
        )
        .unwrap(/* t */);
    let pid = out.id.unwrap(/* the queue row landed */);
    cell.decide(
        &pid,
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: None,
            review: None,
        },
        "user@test",
    )
    .unwrap(/* t */);
    let before = history_of(&cell, &target.id);
    assert_eq!(before.len(), 2, "the closing recorded its pre-state");
    assert_eq!(
        cell.get(&target.id).unwrap(/* t */).status,
        Status::Superseded,
        "the accepted proposal closed the target"
    );

    cell.purge(&pid, "prop", "user:test").unwrap(/* t */);
    assert_eq!(
        before,
        history_of(&cell, &target.id),
        "the purge of the proposal changed the target's history"
    );
    let live = cell.get(&target.id).unwrap(/* t */);
    assert_eq!(live.status, Status::Active, "the declared reopening");
    assert_eq!(live.body, "il bersaglio, ritoccato [[Roma]]");
}

/// `decide`, secondo verso: la purga del BERSAGLIO. La proposta resta
/// accettata e attiva (contenuto suo) e la sua storia resta identica.
#[test]
fn purge_of_the_target_leaves_the_proposals_history_whole() {
    let dir = common::test_dir();
    let cell = Cell::create(&dir.path().join("dec-t.sqlite"), "p", "test")
        .unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "test")
        .unwrap(/* t */);
    let target = common::base_note(1, "il bersaglio [[Roma]]");
    cell.put_note(&target, "user:test").unwrap(/* t */);
    let mut p = common::base_note(7, "la correzione proposta [[Roma]]");
    p.status = Status::Quarantined;
    let out = cell
        .propose(
            &ProposalInput {
                note: p,
                mode: ProposalMode::Supersede(target.id),
                client: None,
                reason: None,
            },
            "mcp:test",
            &Default::default(),
        )
        .unwrap(/* t */);
    let pid = out.id.unwrap(/* the queue row landed */);
    cell.decide(
        &pid,
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: None,
            review: None,
        },
        "user@test",
    )
    .unwrap(/* t */);
    cut_at(&cell, head_of(&cell));
    let before = history_of(&cell, &pid);

    cell.purge(&target.id, "prop", "user:test").unwrap(/* t */);
    assert_eq!(
        before,
        history_of(&cell, &pid),
        "the purge of the target changed the proposal's history"
    );
    let live = cell.get(&pid).unwrap(/* t */);
    assert_eq!(live.status, Status::Active, "the proposal stays active");
    assert_eq!(live.body, "la correzione proposta [[Roma]]");
}

/// `propose` con bersaglio: la purga del bersaglio lascia la proposta IN
/// CODA (il suo testo è contenuto dell'agente, v0.6.11 K1) con la storia
/// identica.
#[test]
fn purge_of_the_target_leaves_the_queued_proposal_whole() {
    let dir = common::test_dir();
    let cell = Cell::create(&dir.path().join("prop-t.sqlite"), "p", "test")
        .unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "test")
        .unwrap(/* t */);
    let target = common::base_note(1, "il bersaglio [[Roma]]");
    cell.put_note(&target, "user:test").unwrap(/* t */);
    cut_at(&cell, head_of(&cell));
    let mut p = common::base_note(7, "la proposta in coda [[Roma]]");
    p.status = Status::Quarantined;
    let out = cell
        .propose(
            &ProposalInput {
                note: p,
                mode: ProposalMode::Supersede(target.id),
                client: None,
                reason: None,
            },
            "mcp:test",
            &Default::default(),
        )
        .unwrap(/* t */);
    let pid = out.id.unwrap(/* the queue row landed */);
    let before = history_of(&cell, &pid);

    cell.purge(&target.id, "prop", "user:test").unwrap(/* t */);
    assert_eq!(
        before,
        history_of(&cell, &pid),
        "the purge of the target changed the queued proposal's history"
    );
    let queued: Vec<String> = cell
        .debug_column("SELECT note_id FROM proposals WHERE state IN ('pending', 'reverted')")
        .unwrap_or_default();
    assert!(
        queued.contains(&pid.to_string()),
        "the proposal stays in the queue: {queued:?}"
    );
}

/// `link`/`unlink`: l'arco è TUTTO riferimento — la purga di un estremo
/// porta via la riga, e né l'altro estremo né la nota che dichiara
/// cambiano storia.
#[test]
fn purge_of_an_endpoint_leaves_the_other_notes_whole() {
    let dir = common::test_dir();
    let cell = Cell::create(&dir.path().join("link.sqlite"), "p", "test")
        .unwrap(/* t */);
    let left = common::base_note(1, "l'estremo di sinistra [[Roma]]");
    let right = common::base_note(2, "l'estremo di destra [[Roma]]");
    let citing = common::base_note(3, "la nota che dichiara [[Roma]]");
    cell.put_note(&left, "user:test").unwrap(/* t */);
    cell.put_note(&right, "user:test").unwrap(/* t */);
    cell.put_note(&citing, "user:test").unwrap(/* t */);
    cell.patch_note(
        &right.id,
        &NotePatch {
            body: Some("l'estremo di destra, ritoccato [[Roma]]".into()),
            ..Default::default()
        },
        "user:test",
    )
    .unwrap(/* t */);
    let edge = engramma_core::cell::EdgeInput {
        src: format!("note:{}", left.id),
        rel: "riguarda".into(),
        dst: format!("note:{}", right.id),
        note_id: Some(citing.id),
        valid_from: None,
        valid_to: None,
        confidence: Confidence::Stated,
    };
    let edge_id = cell.link(&edge, "user:test").unwrap(/* t */);
    cut_at(&cell, head_of(&cell));
    let right_before = history_of(&cell, &right.id);
    let citing_before = history_of(&cell, &citing.id);

    cell.purge(&left.id, "prop", "user:test").unwrap(/* t */);
    let gone: Vec<String> = cell
        .debug_column(&format!(
            "SELECT id FROM edges WHERE src = 'note:{}' OR dst = 'note:{}'",
            left.id, left.id
        ))
        .unwrap_or_default();
    assert!(gone.is_empty(), "the edge row named the purged note");
    let _ = edge_id;
    assert_eq!(
        right_before,
        history_of(&cell, &right.id),
        "the purge of an endpoint changed the other endpoint's history"
    );
    assert_eq!(
        citing_before,
        history_of(&cell, &citing.id),
        "the purge of an endpoint changed the citing note's history"
    );
    assert_eq!(
        cell.get(&right.id).unwrap(/* t */).body,
        "l'estremo di destra, ritoccato [[Roma]]"
    );
}

/// Il punto 5: nelle celle condivise un arrivo che cade DENTRO il
/// prefisso congelato (sotto la testa d'ordine totale che il taglio ha
/// inghiottito) non si rigioca — si rifiuta con un errore leggibile, e il
/// dispositivo si riallinea dall'istantanea. Un'operazione così può
/// nascere solo da un dispositivo rimasto indietro (un backup vecchio
/// che risincronizza): l'HLC di chi ha visto la frontiera non torna mai
/// indietro. La forma SANA — il ritardo scritto offline sopra la
/// frontiera, sotto la testa — resta la strada di sempre
/// (`shared_verify.rs`).
#[test]
fn a_shared_cell_refuses_an_arrival_under_the_cut() {
    let init = shared_init("prefisso-congelato");
    let (a, wa) = cell_with("a");
    let (b, _wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = common::base_note(9, "la nota della frontiera [[Roma]]");
    wa.store(common::K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    // the shared frontier: both devices confirmed everything
    a.mark_shared().unwrap(/* t */);
    b.mark_shared().unwrap(/* t */);
    a.record_session_ack("b000000000x").unwrap(/* t */);
    b.record_session_ack("a000000000x").unwrap(/* t */);
    let head = head_of(&a);
    cut_at(&a, head);

    // the behind device's op: minted with an HLC BELOW the frontier's own
    // put — a clock that never observed it. Its base resolves (the device
    // held the put), so today it would replay INTO the frozen prefix
    let put_op = a
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .find(|o| o.kind == OpKind::PutNote)
        .unwrap(/* t */);
    let stale = make_op(OpParts {
        op_id: Ulid::from_string(&format!("01{}{:010}", "0".repeat(14), 42)).unwrap(/* t */),
        hlc: engramma_core::hlc::Hlc {
            physical_ms: common::K, // below the put's K+1
            counter: 0,
            node: "b0000000000x".into(),
        },
        actor: "homun:user".into(),
        kind: OpKind::PatchNote,
        note_id: Some(note.id),
        base: Some(put_op.content_hash.clone()),
        body: serde_json::json!({
            "id": note.id.to_string(),
            "set": { "body": "la mano che torna dal passato [[Roma]]" },
        }),
        salt: vec![1_u8; 16],
        device: None,
        sig: None,
    })
    .unwrap(/* t */);
    let refuse = a.ingest_op(&stale);
    let Err(e) = refuse else {
        panic!("an arrival under the cut of a shared cell must be refused");
    };
    assert!(
        e.to_string().contains("taglio"),
        "the refusal must say what it refused and the way out: {e}"
    );
    // and the prefix is what it was: the note reads the frontier's body
    let live = a.get(&note.id).unwrap(/* t */);
    assert_eq!(live.body, "la nota della frontiera [[Roma]]");
}
