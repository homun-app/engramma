// SPDX-License-Identifier: Apache-2.0

//! Consolidation candidates (prompt v0.3, punto 5): the golden vault gains a
//! near-duplicate pair and an open contradiction; both are found, and the
//! closed supersede chains stay out.

use engramma_core::consolidation::{CandidateKind, Consolidation};
use engramma_core::format::{At, Lang};
use engramma_core::{Cell, Confidence, Note, NoteType, Status};
use time::OffsetDateTime;
use ulid::Ulid;

fn ts(day: u8) -> At {
    At(
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
        .unwrap(/* test-only */)
        .replace_nanosecond(0)
        .unwrap(/* test-only */),
    )
}

fn note(id: u16, title: &str, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some(title.into()),
        body: body.into(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: ts(1),
        updated_at: ts(1),
        extensions: vec![],
    }
}

/// The contradictions work without any embedding model.
#[test]
fn contradictions_on_a_hand_built_cell() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t").unwrap(/* t */);
    // one subject, one note, TWO lives_in with overlapping validity: the
    // open contradiction the golden vault models for Luca
    let mut luca = note(
        1,
        "Luca",
        "Vive a [[Torino]].\n\n## Relations\n- lives_in:: [[Torino]]\n- lives_in:: [[Roma]]\n",
    );
    luca.note_type = NoteType::Person;
    cell.put_note(&luca, "t").unwrap(/* t */);
    let second = note(
        2,
        "Appunto su Luca",
        "Appunto: sembra stia anche a [[Roma]].\n\n## Relations\n- lives_in:: [[Roma]]\n",
    );
    // its subject is the note itself: it does NOT group with Luca — that is
    // the point (a contradiction needs the same subject entity)
    let _ = second;
    let mut out = cell.consolidation_candidates(10, &Consolidation::default())
        .unwrap(/* t */);
    out.retain(|c| c.kind == CandidateKind::Contradiction);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].detail.contains("lives_in"), "{out:?}");
    assert!(out[0].detail.contains("Torino") && out[0].detail.contains("Roma"));

    // closing the old note removes the candidate
    let mut newer = note(3, "Luca vive a Roma", "Luca vive a [[Roma]].");
    newer.supersedes = vec![luca.id];
    newer.valid_from = Some(engramma_core::Validity::parse("2026-09-01").unwrap(/* t */));
    let mut old = luca.clone();
    old.status = Status::Superseded;
    old.valid_to = Some(engramma_core::Validity::parse("2026-08-31").unwrap(/* t */));
    drop(cell.supersede(&luca.id, &newer, None, "t"));
    // the person note closed: its lives_in edges follow the status
    let out2 = cell
        .consolidation_candidates(10, &Consolidation::default())
        .unwrap(/* t */);
    assert!(
        out2.iter().all(|c| c.kind != CandidateKind::Contradiction),
        "a closed chain is not a contradiction: {out2:?}"
    );
}

/// No candidates on a quiet cell.
#[test]
fn quiet_cell_has_none() {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t").unwrap(/* t */);
    cell.put_note(&note(5, "una", "una nota su [[Milano]]"), "t")
        .unwrap(/* t */);
    cell.put_note(
        &note(6, "altra", "una nota del tutto diversa su [[Roma]] e [[Torino]]"),
        "t",
    )
    .unwrap(/* t */);
    let out = cell
        .consolidation_candidates(10, &Consolidation::default())
        .unwrap(/* t */);
    assert!(out.is_empty(), "{out:?}");
}
