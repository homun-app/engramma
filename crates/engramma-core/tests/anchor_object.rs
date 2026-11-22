// SPDX-License-Identifier: Apache-2.0

//! v0.6.13 (M1): **l'ancora è l'oggetto intero.** La v0.6.12 applicò la
//! regola del prefisso congelato solo alle righe di versione:
//! `materialize_note` cancella ancora la coda (`proposals`), il
//! registro (`decisions`), le righe `supersedes` del nuovo e gli archi
//! dichiarati senza guardare il taglio — e il rigioco riparte solo da
//! sopra, quindi una riga nata a o sotto il taglio non torna mai.
//! Questi test la chiudono: la correzione d'agente annullata
//! (`Active` + coda sparita, seme `9848…`), la `decide` che rigioca su
//! una nota senza la sua riga di coda («is not a proposal», seme
//! `8394…`), il sospetto `supersedes` di la verifica v0.6.12, e per
//! ogni tabella dell'ancora la forma «riga nata sotto il taglio,
//! modificata sopra, poi ricostruita»: dopo, la riga è uguale a quella
//! che si ottiene senza taglio.

mod common;

use common::{cell_with, shared_init};
use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, ProposalInput, ProposalMode, ProposeContext, RevertOutcome,
};
use engramma_core::format::{Note, NoteType};
use engramma_core::{Cell, CellError, Confidence, Status};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use ulid::Ulid;

use common::K;

fn nid(i: u32) -> Ulid {
    Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), i)).unwrap(/* test-only */)
}

fn note(i: u32, body: &str) -> Note {
    Note {
        id: nid(i),
        note_type: NoteType::Fact,
        title: Some("il titolo di partenza".into()),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: "test".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: common::now(),
        updated_at: common::now(),
        extensions: vec![],
    }
}

fn head(cell: &Cell) -> i64 {
    cell.debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */)
}

fn queue(cell: &Cell, id: &Ulid) -> Vec<String> {
    cell.debug_column(&format!(
        "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{id}'"
    ))
    .unwrap(/* test-only */)
}

/// La fotografia dell'oggetto intero di una cella: la coda, il
/// registro, le righe `supersedes` e le righe delle note — quello che
/// il confronto «con taglio == senza taglio» giudica.
fn whole_object_photo(cell: &Cell) -> Vec<String> {
    let mut photo = cell
        .debug_column(
            "SELECT 'q ' || note_id || ' ' || mode || ' ' || COALESCE(target, '-') || ' ' || state \
             || ' ' || COALESCE(decision_op, '-') || ' ' || COALESCE(decided_by, '-') \
             FROM proposals ORDER BY note_id",
        )
        .unwrap(/* test-only */);
    photo.extend(
        cell.debug_column(
            "SELECT 'd ' || op_id || ' ' || proposal || ' ' || decision || ' ' || actor \
             || ' ' || COALESCE(reverted_by, '-') FROM decisions ORDER BY op_id",
        )
        .unwrap(/* test-only */),
    );
    photo.extend(
        cell.debug_column(
            "SELECT 's ' || new_id || ' ' || old_id FROM supersedes ORDER BY new_id, old_id",
        )
        .unwrap(/* test-only */),
    );
    photo.extend(
        cell.debug_column(
            "SELECT 'n ' || id || ' ' || status || ' ' || COALESCE(valid_to, '-') \
             FROM notes ORDER BY id",
        )
        .unwrap(/* test-only */),
    );
    photo
}

/// La correzione d'agente della verifica v0.6.12: una proposta in
/// modalità supersede, accettata dall'utente, poi annullata — con e
/// senza taglio fra la decisione e l'annullamento. La purga del
/// bersaglio è la strada che ricostruisce la proposta.
fn correction_story(cut_mid: bool, revert: bool) {
    let (cell, wall) = cell_with("ac");
    cell.ingest_op(&shared_init("ancora/correzione")).unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "cli")
        .unwrap(/* t */);
    wall.store(K + 1, Ordering::SeqCst);
    cell.put_note(&note(1, "il mio colore preferito è il rosso [[Roma]]"), "user")
        .unwrap(/* t */);
    let mut proposal = note(3, "il mio colore preferito è il nero [[Roma]]");
    proposal.status = Status::Quarantined;
    wall.store(K + 2, Ordering::SeqCst);
    let out = cell
        .propose(
            &ProposalInput {
                note: proposal,
                mode: ProposalMode::Supersede(nid(1)),
                client: None,
                reason: None,
            },
            "mcp:agente",
            &ProposeContext::default(),
        )
        .unwrap(/* t */);
    let pid = out.id.unwrap(/* t */);
    let mut decision = None;
    if revert {
        wall.store(K + 3, Ordering::SeqCst);
        decision = Some(
            cell.decide(
                &pid,
                &DecisionInput {
                    action: DecisionAction::Accept { edited: None },
                    reason: None,
                    review: None,
                },
                "user",
            )
            .unwrap(/* t */),
        );
    }
    if cut_mid {
        cell.compact(
            head(&cell),
            &engramma_core::cell::CompactOptions { keep_snapshot: false },
        )
        .unwrap(/* t */);
    }
    if let Some(op) = decision {
        wall.store(K + 4, Ordering::SeqCst);
        match cell.revert_decision(&op, "ci ho ripensato", "user").unwrap(/* t */) {
            RevertOutcome::Reverted { .. } => {}
            other => panic!("l'annullamento doveva riuscire: {other:?}"),
        }
    }
    // prima della purga: la proposta è in quarantena, la coda c'è
    assert_eq!(
        cell.get(&pid).unwrap(/* t */).status,
        Status::Quarantined,
        "[taglio={cut_mid} annullata={revert}] la proposta è in quarantena prima della purga"
    );
    assert!(
        !queue(&cell, &pid).is_empty(),
        "[taglio={cut_mid} annullata={revert}] la riga di coda c'è prima della purga"
    );
    // la purga del bersaglio: deve riuscire, e la proposta resta in
    // quarantena con la sua riga di coda
    wall.store(K + 5, Ordering::SeqCst);
    cell.purge(&nid(1), "dato superato", "user").unwrap(/* t */);
    assert_eq!(
        cell.get(&pid).unwrap(/* t */).status,
        Status::Quarantined,
        "[taglio={cut_mid} annullata={revert}] dopo la purga la proposta resta in quarantena"
    );
    let expected = if revert { "reverted" } else { "pending" };
    assert_eq!(
        queue(&cell, &pid),
        vec![format!("supersede {expected}")],
        "[taglio={cut_mid} annullata={revert}] la riga di coda sopravvive alla purga"
    );
    assert!(
        cell.verify().unwrap(/* t */).ok(),
        "[taglio={cut_mid} annullata={revert}] verify verde dopo la purga"
    );
}

/// Il caso giusto senza taglio: annullata, la proposta torna in
/// quarantena e la coda dice `reverted` (il controllo della sonda del
/// revisore).
#[test]
fn correction_reverted_without_cut_stays_quarantined() {
    correction_story(false, true);
}

/// Il difetto di sicurezza della verifica v0.6.12: la stessa storia CON
/// il taglio sulla decisione. Oggi la proposta diventa `Active` e la
/// sua riga sparisce dalla coda; l'ancora dell'oggetto intero la
/// riporta.
#[test]
fn correction_reverted_across_a_cut_keeps_the_quarantine() {
    correction_story(true, true);
}

/// La proposta pendente attraversa il taglio intatta (il controllo).
#[test]
fn correction_pending_with_a_cut_keeps_the_queue() {
    correction_story(true, false);
    correction_story(false, false);
}

/// La forma «riga nata sotto il taglio, modificata sopra, poi
/// ricostruita» per la coda e il registro: la proposta nasce sotto il
/// taglio (`pending`), la decisione la rifiuta SOPRA, e la purga del
/// bersaglio ricostruisce la proposta. Dopo, l'oggetto intero è quello
/// che la stessa storia produce senza taglio.
#[test]
fn a_rejection_above_the_cut_replays_over_the_whole_anchor() {
    let run = |cut_mid: bool| -> Vec<String> {
        let (cell, wall) = cell_with("ar");
        cell.ingest_op(&shared_init("ancora/rifiuto")).unwrap(/* t */);
        cell.set_policy(serde_json::json!({ "write": "propose" }), "cli")
            .unwrap(/* t */);
        wall.store(K + 1, Ordering::SeqCst);
        cell.put_note(&note(1, "il mio colore preferito è il rosso [[Roma]]"), "user")
            .unwrap(/* t */);
        let mut proposal = note(4, "una proposta che sarà rifiutata [[Roma]]");
        proposal.status = Status::Quarantined;
        wall.store(K + 2, Ordering::SeqCst);
        let out = cell
            .propose(
                &ProposalInput {
                    note: proposal,
                    mode: ProposalMode::Supersede(nid(1)),
                    client: None,
                    reason: None,
                },
                "mcp:agente",
                &ProposeContext::default(),
            )
            .unwrap(/* t */);
        let pid = out.id.unwrap(/* t */);
        if cut_mid {
            cell.compact(
                head(&cell),
                &engramma_core::cell::CompactOptions { keep_snapshot: false },
            )
            .unwrap(/* t */);
        }
        // la decisione SOPRA il taglio: la lapide, la coda `rejected`
        wall.store(K + 3, Ordering::SeqCst);
        cell.decide(
            &pid,
            &DecisionInput {
                action: DecisionAction::Reject,
                reason: None,
                review: None,
            },
            "user",
        )
        .unwrap(/* t */);
        // la purga del bersaglio ricostruisce la proposta (la decisione
        // la nomina come bersaglio): la coda nata sotto il taglio deve
        // esserci, o la decisione che rigioca non la trova
        wall.store(K + 4, Ordering::SeqCst);
        cell.purge(&nid(1), "dato superato", "user").unwrap(/* t */);
        assert!(
            cell.verify().unwrap(/* t */).ok(),
            "verify verde [taglio={cut_mid}]"
        );
        whole_object_photo(&cell)
    };
    let with_cut = run(true);
    let without_cut = run(false);
    assert_eq!(
        with_cut, without_cut,
        "la ricostruzione con il taglio produce l'oggetto intero che la stessa storia \
         produce senza taglio"
    );
    assert!(
        with_cut
            .iter()
            .any(|r| r.starts_with("q ") && r.contains(" rejected ")),
        "la coda del rifiuto c'è: {with_cut:?}"
    );
}

/// Il sospetto della verifica v0.6.12, provato: `restore_note` non
/// ricrea le righe `supersedes`, quindi una nota nata da un supersede
/// sotto il taglio, poi ricostruita, perde la riga — e la purga della
/// nuova non riapre più la vecchia. La storia: `supersede(A → N)` sotto
/// il taglio, un secondo supersede sopra (l'operazione a due oggetti),
/// la purga del suo estremo nuovo (ricostruisce N), poi la purga di N.
/// A deve riaprirsi, e l'oggetto intero deve essere quello della
/// stessa storia senza taglio.
#[test]
fn a_supersede_below_the_cut_survives_the_reconstruction_and_reopens() {
    let run = |cut_mid: bool| -> Vec<String> {
        let (cell, wall) = cell_with("as");
        cell.ingest_op(&shared_init("ancora/supersede")).unwrap(/* t */);
        wall.store(K + 1, Ordering::SeqCst);
        cell.put_note(&note(1, "la nota vecchia [[Roma]]"), "user")
            .unwrap(/* t */);
        // il supersede che crea N, sotto il taglio
        wall.store(K + 2, Ordering::SeqCst);
        let n = note(5, "la nota nuova [[Roma]]");
        cell.supersede(&nid(1), &n, None, "user").unwrap(/* t */);
        if cut_mid {
            cell.compact(
                head(&cell),
                &engramma_core::cell::CompactOptions { keep_snapshot: false },
            )
            .unwrap(/* t */);
        }
        // sopra il taglio: l'operazione a due oggetti, e la purga
        // dell'altro estremo — ricostruisce N
        wall.store(K + 3, Ordering::SeqCst);
        let m = note(6, "la nota nuova nuova [[Roma]]");
        cell.supersede(&nid(5), &m, None, "user").unwrap(/* t */);
        wall.store(K + 4, Ordering::SeqCst);
        cell.purge(&nid(6), "fuori strada", "user").unwrap(/* t */);
        assert!(
            cell.verify().unwrap(/* t */).ok(),
            "verify verde dopo la purga di M [taglio={cut_mid}]"
        );
        // la riga `supersedes (N, A)` deve essere ancora lì
        let rows = cell
            .debug_column(
                "SELECT CAST(COUNT(*) AS TEXT) FROM supersedes \
                 WHERE new_id = '01000000000000000000000005' \
                   AND old_id = '01000000000000000000000001'",
            )
            .unwrap(/* t */);
        assert_eq!(
            rows.first().map(String::as_str),
            Some("1"),
            "la riga supersedes di N sopravvive alla ricostruzione [taglio={cut_mid}]"
        );
        // la purga di N: A deve riaprirsi
        wall.store(K + 5, Ordering::SeqCst);
        cell.purge(&nid(5), "dato superato", "user").unwrap(/* t */);
        assert!(
            cell.verify().unwrap(/* t */).ok(),
            "verify verde dopo la purga di N [taglio={cut_mid}]"
        );
        whole_object_photo(&cell)
    };
    let with_cut = run(true);
    let without_cut = run(false);
    assert_eq!(
        with_cut, without_cut,
        "la storia con il taglio produce l'oggetto intero della stessa storia senza taglio"
    );
    let a = with_cut
        .iter()
        .find(|r| r.starts_with("n 01000000000000000000000001 "))
        .unwrap(/* la riga di A c'è */);
    assert!(a.contains(" active "), "la purga di N riapre A: {a:?}");
}

/// La forma «nata sotto il taglio, modificata sopra, poi ricostruita»
/// per i TAG e gli ARCHI DICHIARATI: sono funzioni della riga della
/// nota, e l'ancora della nota li ricrea interi. La ricostruzione la
/// chiede la purga del nuovo estremo di un supersede sopra il taglio.
#[test]
fn tags_and_declared_edges_below_the_cut_replay_equal() {
    let run = |cut_mid: bool| -> Vec<String> {
        let (cell, wall) = cell_with("at");
        cell.ingest_op(&shared_init("ancora/tag")).unwrap(/* t */);
        wall.store(K + 1, Ordering::SeqCst);
        let mut base = note(7, "il corpo di partenza con [[Roma]] e #casa/milano");
        base.tags = vec!["viaggio".into()];
        cell.put_note(&base, "user").unwrap(/* t */);
        if cut_mid {
            cell.compact(
                head(&cell),
                &engramma_core::cell::CompactOptions { keep_snapshot: false },
            )
            .unwrap(/* t */);
        }
        // sopra il taglio: il tag e il corpo cambiano
        wall.store(K + 2, Ordering::SeqCst);
        cell.patch_note(
            &nid(7),
            &engramma_core::NotePatch {
                body: Some("il corpo nuovo con [[Milano]] e #casa/roma".into()),
                tags: Some(vec!["lavoro".into()]),
                ..Default::default()
            },
            "user",
        )
        .unwrap(/* t */);
        // la ricostruzione: il supersede sopra il taglio e la purga del
        // suo estremo nuovo
        wall.store(K + 3, Ordering::SeqCst);
        let q = note(8, "la nota che chiude [[Roma]]");
        cell.supersede(&nid(7), &q, None, "user").unwrap(/* t */);
        wall.store(K + 4, Ordering::SeqCst);
        cell.purge(&nid(8), "fuori strada", "user").unwrap(/* t */);
        assert!(
            cell.verify().unwrap(/* t */).ok(),
            "verify verde [taglio={cut_mid}]"
        );
        let mut photo = cell
            .debug_column(
                "SELECT 't ' || note_id || ' ' || tag FROM note_tags WHERE note_id = '01000000000000000000000007' ORDER BY tag",
            )
            .unwrap(/* t */);
        photo.extend(
            cell.debug_column(
                "SELECT 'e ' || id || ' ' || src || ' ' || rel || ' ' || dst FROM edges \
                 WHERE note_id = '01000000000000000000000007' ORDER BY id",
            )
            .unwrap(/* t */),
        );
        photo
    };
    let with_cut = run(true);
    let without_cut = run(false);
    assert_eq!(
        with_cut, without_cut,
        "tag e archi dichiarati ricostruiti come senza taglio: {with_cut:?} vs {without_cut:?}"
    );
    assert!(
        with_cut.iter().any(|r| r.contains("lavoro")),
        "il tag nuovo c'è dopo la ricostruzione: {with_cut:?}"
    );
    assert!(
        with_cut.iter().any(|r| r.contains("Milano")),
        "l'arco dichiarato nuovo c'è dopo la ricostruzione: {with_cut:?}"
    );
}

/// La purga di una nota legata a una proposta riesce anche quando il
/// bersaglio non c'è più da leggere: la forma minima del seme
/// `8394…`, senza generatore — la `decide` sopra il taglio trova la
/// proposta senza la sua riga di coda e la purga fallisce per sempre.
#[test]
fn the_purge_of_a_proposal_target_succeeds_after_the_cut() {
    let (cell, wall) = cell_with("ap");
    cell.ingest_op(&shared_init("ancora/purga")).unwrap(/* t */);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "cli")
        .unwrap(/* t */);
    wall.store(K + 1, Ordering::SeqCst);
    cell.put_note(&note(1, "il mio colore preferito è il rosso [[Roma]]"), "user")
        .unwrap(/* t */);
    let mut proposal = note(9, "la correzione che aspetta [[Roma]]");
    proposal.status = Status::Quarantined;
    wall.store(K + 2, Ordering::SeqCst);
    let out = cell
        .propose(
            &ProposalInput {
                note: proposal,
                mode: ProposalMode::Supersede(nid(1)),
                client: None,
                reason: None,
            },
            "mcp:agente",
            &ProposeContext::default(),
        )
        .unwrap(/* t */);
    let pid = out.id.unwrap(/* t */);
    cell.compact(
        head(&cell),
        &engramma_core::cell::CompactOptions { keep_snapshot: false },
    )
    .unwrap(/* t */);
    // sopra il taglio: la decisione dell'utente — e poi la purga del
    // bersaglio, che ricostruisce la proposta sopra la sua ancora
    wall.store(K + 3, Ordering::SeqCst);
    cell.decide(
        &pid,
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: None,
            review: None,
        },
        "user",
    )
    .unwrap(/* t */);
    wall.store(K + 4, Ordering::SeqCst);
    match cell.purge(&nid(1), "dato superato", "user") {
        Ok(_) => {}
        Err(CellError::Invalid(msg)) => panic!("la purga riesce: {msg}"),
        Err(e) => panic!("la purga riesce, non quest'errore: {e}"),
    }
    assert!(
        cell.verify().unwrap(/* t */).ok(),
        "verify verde dopo la purga"
    );
    assert_eq!(
        queue(&cell, &pid),
        vec!["supersede accepted".to_string()],
        "la coda della proposta accettata sopravvive alla purga"
    );
}

/// La migrazione v23 costruisce le ancore che una cella compattata con
/// un CHECKPOINT (senza file di copia) non ha mai preso: la storia
/// della correzione annullata viene costruita qui, poi fatta tornare
/// indietro allo schema della v0.6.12 (la tabella delle ancore via, la
/// versione via) e riaperta. La purga del bersaglio deve riuscire e la
/// coda restare — la migrazione ha ricostruito lo stato al taglio
/// (rollback degli effetti sopra il taglio) senza il file che lo
/// fotografò.
#[test]
fn the_v23_migration_rebuilds_the_anchor_of_a_checkpoint_cell() {
    use engramma_core::hlc::HlcClock;
    let dir = common::test_dir();
    let path = dir.path().join("migrazione.sqlite");
    {
        let cell = Cell::open_bare(&path).unwrap(/* t */);
        let wall = Arc::new(AtomicU64::new(common::K));
        let w = Arc::clone(&wall);
        cell.inject_clock(HlcClock::new("am0000000000x".to_string(), move || {
            w.load(Ordering::SeqCst)
        }))
        .unwrap(/* t */);
        let n = Arc::new(std::sync::Mutex::new(0_u64));
        cell.inject_minter(Arc::new(move || {
            let mut g = n.lock().map_err(|_| CellError::Db("mint".into()))?;
            *g += 1;
            let h = blake3::hash(format!("ancora/mig/{}", *g).as_bytes());
            let b = h.as_bytes();
            let mut randomness: u128 = 0;
            for x in &b[..10] {
                randomness = (randomness << 8) | u128::from(*x);
            }
            let op_id = Ulid::from_parts(common::K, randomness);
            let mut salt = [0_u8; 16];
            salt.copy_from_slice(&b[10..26]);
            Ok((op_id, salt))
        }))
        .unwrap(/* t */);
        cell.ingest_op(&shared_init("ancora/migrazione")).unwrap(/* t */);
        cell.set_policy(serde_json::json!({ "write": "propose" }), "cli")
            .unwrap(/* t */);
        wall.store(common::K + 1, Ordering::SeqCst);
        cell.put_note(&note(1, "il mio colore preferito è il rosso [[Roma]]"), "user")
            .unwrap(/* t */);
        let mut proposal = note(10, "la correzione della migrazione [[Roma]]");
        proposal.status = Status::Quarantined;
        wall.store(common::K + 2, Ordering::SeqCst);
        let out = cell
            .propose(
                &ProposalInput {
                    note: proposal,
                    mode: ProposalMode::Supersede(nid(1)),
                    client: None,
                    reason: None,
                },
                "mcp:agente",
                &ProposeContext::default(),
            )
            .unwrap(/* t */);
        let pid = out.id.unwrap(/* t */);
        wall.store(common::K + 3, Ordering::SeqCst);
        let decision = cell
            .decide(
                &pid,
                &DecisionInput {
                    action: DecisionAction::Accept { edited: None },
                    reason: None,
                    review: None,
                },
                "user",
            )
            .unwrap(/* t */);
        // il taglio come CHECKPOINT: nessun file di copia
        cell.compact(
            head(&cell),
            &engramma_core::cell::CompactOptions { keep_snapshot: false },
        )
        .unwrap(/* t */);
        wall.store(common::K + 4, Ordering::SeqCst);
        cell.revert_decision(&decision, "ci ho ripensato", "user")
            .unwrap(/* t */);
        assert!(
            cell.verify().unwrap(/* t */).ok(),
            "verify verde prima del ritorno indietro"
        );
    }
    // il ritorno indietro allo schema v0.6.12: le ancore via, la
    // versione via — quello che una cella compattata dalla v0.6.12 sa
    // di questa storia
    {
        let conn = rusqlite::Connection::open(&path).unwrap(/* t */);
        conn.execute_batch(
            "DROP TABLE object_anchors;
             UPDATE meta SET value = '22' WHERE key = 'schema_version';",
        )
        .unwrap(/* t */);
    }
    // la riapertura: la migrazione v23 gira sul checkpoint (rollback,
    // nessun file) e la purga del bersaglio riesce con la coda intatta
    let cell = Cell::open(&path).unwrap(/* t */);
    let pid = nid(10);
    assert_eq!(
        cell.get(&pid).unwrap(/* t */).status,
        Status::Quarantined,
        "la proposta annullata è in quarantena dopo la migrazione"
    );
    let anchors: Vec<String> = cell
        .debug_column(&format!(
            "SELECT anchor_json FROM object_anchors WHERE note_id = '{pid}'"
        ))
        .unwrap(/* t */);
    assert_eq!(
        anchors.len(),
        1,
        "la migrazione ha scritto un'ancora: {anchors:?}"
    );
    assert!(
        anchors[0].contains("accepted"),
        "l'ancora fotografa lo stato AL TAGLIO (la decisione accettata), non il vivo: {}",
        anchors[0]
    );
    cell.purge(&nid(1), "dato superato", "user").unwrap(/* t */);
    assert_eq!(
        cell.get(&pid).unwrap(/* t */).status,
        Status::Quarantined,
        "la proposta resta in quarantena dopo la purga"
    );
    assert_eq!(
        queue(&cell, &pid),
        vec!["supersede reverted".to_string()],
        "la coda sopravvive alla purga grazie all'ancora migrata"
    );
    assert!(
        cell.verify().unwrap(/* t */).ok(),
        "verify verde dopo la purga"
    );
}
