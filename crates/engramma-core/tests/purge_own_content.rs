// SPDX-License-Identifier: Apache-2.0

//! v0.6.11 (K1): la purga si porta via il purgato, mai il contenuto degli
//! altri. Una purga toglie la nota purgata, la sua famiglia (le copie di
//! conflitto) e ogni RIFERIMENTO a lei. Non toglie mai il contenuto
//! proprio di un'altra nota: corpo, intestazione, esistenza, stato. Per
//! un'operazione che tocca X e Y, con X purgato: la metà di Y si applica,
//! senza tutto quello che nomina X; la metà di X non si applica.
//!
//! Ogni operazione a due oggetti dell'elenco del DESIGN § 4.3.1 ha qui la
//! sua prova deterministica, con la purga su un lato e il contenuto
//! dell'altro fotografato prima e confrontato dopo. Le due direzioni
//! dichiarate — la riapertura della vecchia quando si purga la nuova, e
//! del bersaglio quando si purga la proposta accettata — sono l'eccezione
//! che la regola dichiara: la chiusura era opera della nota purgata.
//!
//! Le due sonde sono quelle del revisore nella verifica della v0.6.10
//! (`zz_review_v0610.rs`, nello scratchpad): la v0.6.10 le fa cadere.

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
        tags: vec![format!("tag/{id}")],
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

/// Il contenuto proprio di una nota, come la regola lo intende: corpo,
/// titolo, tag, tipo, stato — e la versione che le scritture vive le hanno
/// dato. La fotografia che la purga di UN ALTRO non può cambiare.
struct Content {
    body: String,
    title: Option<String>,
    tags: Vec<String>,
    note_type: NoteType,
    status: Status,
    version: String,
}

fn content_of(cell: &Cell, id: &Ulid) -> Content {
    let n = cell.get(id).unwrap(/* test-only */);
    let version = cell
        .debug_column(&format!(
            "SELECT CAST(version AS TEXT) FROM notes WHERE id = '{id}'"
        ))
        .unwrap(/* test-only */)
        .first()
        .cloned()
        .unwrap_or_default();
    Content {
        body: n.body,
        title: n.title,
        tags: n.tags,
        note_type: n.note_type,
        status: n.status,
        version,
    }
}

fn assert_same_note(where_: &str, before: &Content, after: &Content) {
    assert_eq!(before.body, after.body, "body changed: {where_}");
    assert_eq!(before.title, after.title, "title changed: {where_}");
    assert_eq!(before.tags, after.tags, "tags changed: {where_}");
    assert_eq!(
        before.note_type, after.note_type,
        "note type changed: {where_}"
    );
    assert_eq!(before.status, after.status, "status changed: {where_}");
    assert_eq!(before.version, after.version, "version changed: {where_}");
}

fn supersedes_rows(cell: &Cell, named: &Ulid) -> usize {
    cell.debug_column(&format!(
        "SELECT CAST(COUNT(*) AS TEXT) FROM supersedes WHERE new_id = '{named}' OR old_id = '{named}'"
    ))
    .unwrap(/* test-only */)
    .first()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
}

/// Sonda 1 del revisore: `put` di A, `supersede(A → N)`, `purge(A)`. N è
/// nata dal supersede: la sua esistenza è contenuto PROPRIO, e la purga
/// della vecchia non può toglierla. Vanno via la chiusura di A (A stessa)
/// e la riga `supersedes` che la nominava. Nella v0.6.10 `get(N)`
/// rispondeva `NoteNotFound` con `verify` verde.
#[test]
fn the_new_note_of_a_supersede_survives_the_purge_of_the_old() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("nuova-vive.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let a = note_of(1, "il mio colore preferito è il rosso");
    let mut n = note_of(3, "il mio colore preferito è il nero");
    n.supersedes = vec![a.id];
    cell.put_note(&a, "user").unwrap(/* test-only */);
    cell.supersede(&a.id, &n, None, "user").unwrap(/* test-only */);
    let before = content_of(&cell, &n.id);
    assert_eq!(before.status, Status::Active);

    cell.purge(&a.id, "dato superato", "user").unwrap(/* t */);

    let after = content_of(&cell, &n.id);
    assert_same_note("la nuova sopravvive alla purga della vecchia", &before, &after);
    assert_eq!(
        supersedes_rows(&cell, &a.id),
        0,
        "nessuna riga nomina la vecchia purgata"
    );
    assert!(cell.get(&a.id).is_err(), "la purgata non c'è più");

    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// Sonda 2 del revisore: il flusso principale di Homun. L'agente propone
/// la correzione di A in modalità supersede, l'utente l'accetta, P è
/// `Active`; poi `purge(A)`. P deve restare: la sua esistenza e il suo
/// testo sono contenuto dell'agente, approvato dall'utente. Nella v0.6.10
/// P diventava `NoteNotFound` e anche la `patch` cadeva.
#[test]
fn the_accepted_correction_survives_the_purge_of_its_target() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("correzione-vive.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let a = note_of(1, "il mio colore preferito è il rosso");
    let mut p = note_of(3, "il mio colore preferito è il nero");
    p.status = Status::Quarantined;
    cell.put_note(&a, "user").unwrap(/* test-only */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "cli")
        .unwrap(/* test-only */);
    cell.propose(
        &ProposalInput {
            note: p,
            mode: ProposalMode::Supersede(a.id),
            client: None,
            reason: None,
        },
        "mcp:agente",
        &ProposeContext {
            embedder: None,
            reviewers: None,
        },
    )
    .unwrap(/* test-only */);
    cell.decide(
        &a_id(3),
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: None,
            review: None,
        },
        "user",
    )
    .unwrap(/* test-only */);
    let before = content_of(&cell, &a_id(3));
    assert_eq!(before.status, Status::Active);

    cell.purge(&a.id, "dato superato", "user").unwrap(/* t */);

    let after = content_of(&cell, &a_id(3));
    assert_same_note("la correzione accettata sopravvive alla purga del bersaglio", &before, &after);
    assert_eq!(
        supersedes_rows(&cell, &a.id),
        0,
        "nessuna riga nomina il bersaglio purgato"
    );
    // la patch di P deve poter continuare a correggerla
    cell.patch_note(
        &a_id(3),
        &NotePatch {
            body: Some("nero, anzi blu".into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap(/* test-only: la nota accettata è viva e scrivibile */);

    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// La direzione dichiarata: la purga della NOTA NUOVA riapre la vecchia,
/// perché la chiusura era opera della nuova. Il corpo della vecchia non
/// cambia; lo stato torna `Active` (la riapertura è l'eccezione che la
/// regola dichiara, non un cambiamento di contenuto).
#[test]
fn the_old_note_reopens_unchanged_when_the_new_is_purged() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("vecchia-riapre.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let old = note_of(1, "la nota vecchia");
    let mut new = note_of(2, "la nota nuova");
    new.supersedes = vec![old.id];
    cell.put_note(&old, "tester").unwrap(/* test-only */);
    cell.supersede(&old.id, &new, None, "tester").unwrap(/* t */);
    let closed = content_of(&cell, &old.id);
    assert_eq!(closed.status, Status::Superseded);

    cell.purge(&new.id, "diritto all'oblio", "tester").unwrap(/* t */);

    let reopened = content_of(&cell, &old.id);
    assert_eq!(reopened.body, closed.body, "il corpo non cambia");
    assert_eq!(reopened.title, closed.title, "il titolo non cambia");
    assert_eq!(reopened.tags, closed.tags, "i tag non cambiano");
    assert_eq!(
        reopened.status,
        Status::Active,
        "la riapertura dichiarata: la chiusura era opera della purgata"
    );
    assert_eq!(supersedes_rows(&cell, &new.id), 0);

    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// La direzione dichiarata, lato `decide`: la purga della PROPOSTA
/// accettata riapre il bersaglio. Il suo contenuto non cambia.
#[test]
fn the_target_reopens_unchanged_when_the_proposal_is_purged() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("bersaglio-riapre.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let target = note_of(1, "la nota che il mondo vede");
    let mut proposal = note_of(2, "la rettifica che la corregge");
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
    cell.decide(
        &a_id(2),
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: Some("accettata".into()),
            review: None,
        },
        "tester",
    )
    .unwrap(/* test-only */);
    let closed = content_of(&cell, &target.id);
    assert_eq!(closed.status, Status::Superseded);

    cell.purge(&a_id(2), "diritto all'oblio", "tester").unwrap(/* t */);

    let reopened = content_of(&cell, &target.id);
    assert_eq!(reopened.body, closed.body, "il corpo non cambia");
    assert_eq!(reopened.title, closed.title, "il titolo non cambia");
    assert_eq!(reopened.tags, closed.tags, "i tag non cambiano");
    assert_eq!(
        reopened.status,
        Status::Active,
        "la riapertura dichiarata: la chiusura era opera della proposta purgata"
    );
    assert_eq!(supersedes_rows(&cell, &target.id), 0);

    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// `link`: la riga verso la nota purgata sparisce; l'altro estremo e la
/// nota che dichiara l'arco restano identici in tutto — corpo, titolo,
/// tag, tipo, stato, versione.
#[test]
fn link_the_other_endpoint_and_the_declaring_note_stay_identical() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("arco-vive.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let left = note_of(1, "un capo del legame");
    let right = note_of(2, "l'altro capo, che sarà dimenticato");
    let declared = note_of(3, "la nota che dichiara il legame");
    cell.put_note(&left, "tester").unwrap(/* test-only */);
    cell.put_note(&right, "tester").unwrap(/* test-only */);
    cell.put_note(&declared, "tester").unwrap(/* test-only */);
    cell.link(
        &EdgeInput {
            src: format!("note:{}", left.id),
            rel: "rel_k".into(),
            dst: format!("note:{}", right.id),
            note_id: Some(declared.id),
            valid_from: None,
            valid_to: None,
            confidence: Confidence::Stated,
        },
        "tester",
    )
    .unwrap(/* test-only */);
    let left_before = content_of(&cell, &left.id);
    let declared_before = content_of(&cell, &declared.id);

    cell.purge(&right.id, "diritto all'oblio", "tester").unwrap(/* t */);

    let edges = cell
        .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM edges")
        .unwrap(/* test-only */);
    assert_eq!(
        edges.first().map(String::as_str),
        Some("0"),
        "l'arco che nominava la purgata sparisce"
    );
    assert_same_note(
        "l'altro estremo non cambia",
        &left_before,
        &content_of(&cell, &left.id),
    );
    assert_same_note(
        "la nota che dichiara l'arco non cambia",
        &declared_before,
        &content_of(&cell, &declared.id),
    );

    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// `unlink`: la purga di un estremo porta via la riga; l'unlink che arriva
/// dopo la purga si registra senza errore e senza aspettare, e l'altro
/// estremo resta identico.
#[test]
fn unlink_after_the_purge_leaves_the_other_endpoint_identical() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let writer = Cell::create(&dir.path().join("scollega-scr.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let left = note_of(1, "un capo del legame che finisce");
    let right = note_of(2, "l'altro capo, che sarà dimenticato");
    writer.put_note(&left, "tester").unwrap(/* test-only */);
    writer.put_note(&right, "tester").unwrap(/* test-only */);
    let edge = EdgeInput {
        src: format!("note:{}", left.id),
        rel: "rel_k".into(),
        dst: format!("note:{}", right.id),
        note_id: None,
        valid_from: None,
        valid_to: None,
        confidence: Confidence::Stated,
    };
    let edge_id = writer.link(&edge, "tester").unwrap(/* test-only */);
    writer.unlink(&edge_id, "tester").unwrap(/* test-only */);

    let reader = Cell::create(&dir.path().join("scollega-leg.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    for op in writer.ops().unwrap(/* test-only */).iter().skip(1) {
        if matches!(op.kind, OpKind::PutNote | OpKind::Link) {
            reader.ingest_op(op).unwrap(/* test-only */);
        }
    }
    let left_before = content_of(&reader, &left.id);
    reader.purge(&right.id, "diritto all'oblio", "tester").unwrap(/* t */);

    let unlink_op = writer
        .ops()
        .unwrap(/* test-only */)
        .iter()
        .find(|op| op.kind == OpKind::Unlink)
        .cloned()
        .unwrap(/* the stage wrote one */);
    reader.ingest_op(&unlink_op).unwrap(/* test-only: registrato, senza errore */);

    let pending: Vec<String> = reader
        .debug_column("SELECT object FROM pending_objects WHERE object LIKE 'edge:%'")
        .unwrap(/* test-only */);
    assert!(pending.is_empty(), "l'unlink non aspetta: {pending:?}");
    assert_same_note(
        "l'altro estremo non cambia",
        &left_before,
        &content_of(&reader, &left.id),
    );

    let report = reader.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// `propose` con bersaglio (K1, strada scelta: resta in coda come
/// proposta di nota nuova): la purga del bersaglio non fa sparire il testo
/// dell'agente. La proposta resta leggibile nella coda, e l'utente può
/// ancora rifiutarla — o accettarla: atterrerà come nota NUOVA, senza
/// chiusura (non c'è più niente da chiudere).
#[test]
fn a_correction_of_a_purged_target_keeps_its_text_in_the_queue() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("proposta-vive.sqlite"), "p", "tester")
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

    cell.purge(&target.id, "diritto all'oblio", "tester").unwrap(/* t */);

    let body = cell
        .debug_column(&format!(
            "SELECT body FROM notes WHERE id = '{}'",
            a_id(2)
        ))
        .unwrap(/* test-only */);
    assert_eq!(
        body.first().map(String::as_str),
        Some("la rettifica orfana"),
        "il testo della proposta non sparisce senza traccia"
    );
    let queue = cell
        .debug_column(&format!(
            "SELECT state FROM proposals WHERE note_id = '{}'",
            a_id(2)
        ))
        .unwrap(/* test-only */);
    assert_eq!(
        queue.first().map(String::as_str),
        Some("pending"),
        "la proposta resta in coda"
    );

    // l'utente può rifiutarla: la lapide tiene il testo
    cell.decide(
        &a_id(2),
        &DecisionInput {
            action: DecisionAction::Reject,
            reason: Some("il bersaglio è stato dimenticato".into()),
            review: None,
        },
        "tester",
    )
    .unwrap(/* test-only: la proposta è ancora decidibile */);

    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// La stessa coda, accettata: la proposta atterra come nota NUOVA —
/// attiva, senza chiusura e senza riga (il bersaglio purgato non c'è
/// più, e nessuna metà può chiuderlo).
#[test]
fn a_correction_of_a_purged_target_lands_as_a_new_note_when_accepted() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("proposta-atterra.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let target = note_of(1, "la nota che il mondo correggeva");
    let mut proposal = note_of(2, "la rettifica che diventa nota");
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

    cell.purge(&target.id, "diritto all'oblio", "tester").unwrap(/* t */);

    cell.decide(
        &a_id(2),
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: None,
            review: None,
        },
        "tester",
    )
    .unwrap(/* test-only: la proposta è ancora decidibile */);

    let landed = content_of(&cell, &a_id(2));
    assert_eq!(landed.status, Status::Active, "atterra come nota nuova");
    assert_eq!(landed.body, "la rettifica che diventa nota");
    assert_eq!(
        supersedes_rows(&cell, &target.id),
        0,
        "nessuna riga nomina il bersaglio purgato"
    );

    let report = cell.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// La forma a due dispositivi della sonda 1: la purga ARRIVA per sync su
/// una copia che aveva già il supersede. La nota nuova deve sopravvivere
/// anche lì — il giudizio vale sul ricevente.
#[test]
fn the_new_note_survives_the_purge_arriving_by_sync() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let writer = Cell::create(&dir.path().join("scrittore.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    let a = note_of(1, "il mio colore preferito è il rosso");
    let mut n = note_of(3, "il mio colore preferito è il nero");
    n.supersedes = vec![a.id];
    writer.put_note(&a, "user").unwrap(/* test-only */);
    writer.supersede(&a.id, &n, None, "user").unwrap(/* t */);
    let before = content_of(&writer, &n.id);

    let reader = Cell::create(&dir.path().join("lettore.sqlite"), "p", "tester")
        .unwrap(/* test-only */);
    for op in writer.ops().unwrap(/* test-only */).iter().skip(1) {
        if matches!(op.kind, OpKind::PutNote | OpKind::Supersede) {
            reader.ingest_op(op).unwrap(/* test-only */);
        }
    }
    assert_eq!(
        content_of(&reader, &n.id).status,
        Status::Active,
        "la fase: la nuova è nata anche sul ricevente"
    );

    // la purga nasce sullo scrittore e arriva al ricevente
    writer.purge(&a.id, "dato superato", "user").unwrap(/* t */);
    let purge_op = writer
        .ops()
        .unwrap(/* test-only */)
        .iter()
        .find(|op| op.kind == OpKind::Purge)
        .cloned()
        .unwrap(/* the stage wrote one */);
    reader.ingest_op(&purge_op).unwrap(/* test-only */);

    assert_same_note(
        "la nuova sopravvive alla purga arrivata per sync",
        &before,
        &content_of(&reader, &n.id),
    );
    assert_eq!(supersedes_rows(&reader, &a.id), 0);

    let report = reader.verify().unwrap(/* test-only */);
    assert!(report.ok(), "{:?}", report.mismatch);
}

/// L'id che `note_of` conia (lettura condivisa fra le fasi del test).
fn a_id(n: u16) -> Ulid {
    Ulid::from_string(&format!("01{:024}", n)).unwrap(/* test-only */)
}
