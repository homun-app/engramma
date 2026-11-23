// SPDX-License-Identifier: Apache-2.0

//! v0.6.14 (N2): **the restore brings back the CONTENT of a version, not
//! its state.** Until this phase `restore` copied the WHOLE version
//! photograph — `status` and `confidence` included — so one operation
//! outside the lifecycle could produce a state no lifecycle operation
//! ever makes: a note `superseded` with no closing row, a quarantined
//! note read as active (an exit from the quarantine by no decision at
//! all). Every rule that works on the closings — the purge's reopening,
//! the replay — had to guess what that meant (the v0.6.13 declaration,
//! seed `11895058361907748327`, pre-existing: the same seed falls on the
//! v0.6.12 tag).
//!
//! The lifecycle is a state machine, and only the operations made for it
//! move it: `supersede`, `decide`, `revert_decision`, `set_status`,
//! `confirm`, `delete`, `purge`. The restore is the `git revert` of a
//! file: the text comes back, the branches stay.
//!
//! These tests were RED on the base (commit of the red tests in the
//! REPORT): the status came back with the body, the confidence with the
//! title, the source with the tags, and the lists of fields did not
//! exist.

mod common;

use engramma_core::cell::ops::{RESTORE_CONTENT_FIELDS, RESTORE_KEPT_FIELDS};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, NotePatch, Status, Validity};
use time::OffsetDateTime;
use ulid::Ulid;

fn at(secs: i64) -> At {
    At(OffsetDateTime::from_unix_timestamp(1_800_000_000 + secs).unwrap(/* t */))
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
        created_at: at(0),
        updated_at: at(0),
        extensions: vec![],
    }
}

fn cell(prefix: &str) -> (tempfile::TempDir, Cell) {
    let dir = common::test_dir();
    let c = Cell::create(&dir.path().join(format!("{prefix}.sqlite")), "p", "n2").unwrap(/* t */);
    (dir, c)
}

/// `restore` of a version from the note's ACTIVE life, on a note the user
/// closed with a supersede: the state stays `superseded`, the closing
/// stays, the body of that version comes back.
#[test]
fn restore_of_an_active_version_on_a_closed_note_keeps_the_closing() {
    let (_d, c) = cell("restore-chiusa");
    let a = note_of(1, "la prima mano");
    c.put_note(&a, "user").unwrap(/* t */);
    c.patch_note(&a.id, &NotePatch { body: Some("la seconda mano".into()), ..Default::default() }, "user")
        .unwrap(/* t */);
    let mut n = note_of(2, "la nota che chiude");
    n.supersedes = vec![a.id];
    c.supersede(&a.id, &n, None, "user").unwrap(/* t */);
    let closed = c.get(&a.id).unwrap(/* t */);
    assert_eq!(closed.status, Status::Superseded);
    let closing = closed
        .valid_to
        .expect(/* t */ "a closed note carries its closing");

    c.restore(&a.id, 1, "user").unwrap(/* t */);

    let after = c.get(&a.id).unwrap(/* t */);
    assert_eq!(
        after.body, "la prima mano",
        "the content of version 1 comes back"
    );
    assert_eq!(
        after.status,
        Status::Superseded,
        "the state is the lifecycle's: still closed"
    );
    assert_eq!(
        after.valid_to,
        Some(closing),
        "the closing is the lifecycle's: it stays"
    );
    let rows = c
        .debug_column(&format!(
            "SELECT new_id || '→' || old_id FROM supersedes WHERE old_id = '{}'",
            a.id
        ))
        .unwrap(/* t */);
    assert_eq!(
        rows,
        vec![format!("{}→{}", n.id, a.id)],
        "the closing row stays"
    );
    assert!(c.verify().unwrap(/* t */).ok(), "the cell verifies");
}

/// `restore` on a note in quarantine: it STAYS in quarantine. The restore
/// is not an exit — from the quarantine one leaves only by a decision.
#[test]
fn restore_on_a_quarantined_note_stays_in_quarantine() {
    let (_d, c) = cell("restore-quarantena");
    let q = note_of(3, "letto sul web");
    c.put_note(&q, "user").unwrap(/* t */);
    // into quarantine by the operation made for it: version 1 is the
    // photograph the set_status took — the note ACTIVE
    c.set_status(&q.id, Status::Quarantined, "user").unwrap(/* t */);
    assert_eq!(c.get(&q.id).unwrap(/* t */).status, Status::Quarantined);
    c.patch_note(&q.id, &NotePatch { body: Some("ritoccata in quarantena".into()), ..Default::default() }, "user")
        .unwrap(/* t */);

    c.restore(&q.id, 1, "user").unwrap(/* t */);

    let after = c.get(&q.id).unwrap(/* t */);
    assert_eq!(
        after.status,
        Status::Quarantined,
        "a restore is no exit from the quarantine"
    );
    assert_eq!(
        after.body, "letto sul web",
        "the content of version 1 comes back"
    );
    assert!(c.verify().unwrap(/* t */).ok(), "the cell verifies");
}

/// The other way round: the version of the quarantined life does not
/// bring the quarantine back over a note the user CONFIRMED.
#[test]
fn restore_does_not_demote_a_confirmed_external_note() {
    let (_d, c) = cell("restore-confermata");
    let mut q = note_of(3, "letto sul web");
    q.confidence = Confidence::External;
    q.status = Status::Quarantined;
    q.source = "import:web".into();
    c.put_note(&q, "user").unwrap(/* t */);
    c.confirm(&q.id, "user").unwrap(/* t */);
    assert_eq!(c.get(&q.id).unwrap(/* t */).status, Status::Active);

    c.restore(&q.id, 1, "user").unwrap(/* t */);

    let after = c.get(&q.id).unwrap(/* t */);
    assert_eq!(after.status, Status::Active, "the user's confirm stands");
    assert_eq!(
        after.body, "letto sul web",
        "the content of version 1 comes back"
    );
    assert_eq!(
        after.confidence,
        Confidence::External,
        "and the confidence is the lifecycle's"
    );
    assert!(c.verify().unwrap(/* t */).ok(), "the cell verifies");
}

/// `restore` of a `stated` version on a note whose confidence moved: the
/// confidence stays where the operations put it.
#[test]
fn restore_keeps_the_confidence() {
    let (_d, c) = cell("restore-confidenza");
    let s = note_of(5, "una cosa letta");
    c.put_note(&s, "user").unwrap(/* t */);
    c.patch_note(
        &s.id,
        &NotePatch { confidence: Some(Confidence::Inferred), ..Default::default() },
        "user",
    )
    .unwrap(/* t */);
    c.patch_note(&s.id, &NotePatch { title: Some(Some("un titolo".into())), ..Default::default() }, "user")
        .unwrap(/* t */);

    c.restore(&s.id, 1, "user").unwrap(/* t */);

    let after = c.get(&s.id).unwrap(/* t */);
    assert_eq!(
        after.confidence,
        Confidence::Inferred,
        "the confidence stays"
    );
    assert_eq!(
        after.title, None,
        "the content of version 1 comes back: no title"
    );
    assert!(c.verify().unwrap(/* t */).ok(), "the cell verifies");
}

/// `restore` keeps the provenance: the source and the birth date belong
/// to the NOTE, not to the version; the content fields come back whole.
#[test]
fn restore_keeps_the_provenance_and_bring_back_the_content() {
    let (_d, c) = cell("restore-provenienza");
    let mut s = note_of(7, "il corpo di partenza");
    s.tags = vec!["casa/milano".into()];
    s.note_type = NoteType::Fact;
    s.valid_from = Validity::parse("2024-01-01").ok();
    s.valid_to = Validity::parse("2024-12-31").ok();
    s.extensions = vec![(
        "x-origine".into(),
        engramma_core::format::yaml::YamlValue::Str("a".into()),
    )];
    c.put_note(&s, "user").unwrap(/* t */);
    c.patch_note(
        &s.id,
        &NotePatch {
            body: Some("il corpo ritoccato".into()),
            tags: Some(vec!["lavoro".into()]),
            title: Some(Some("un titolo".into())),
            scope: Some(Some("privato".into())),
            source: Some("import:web".into()),
            valid_from: Some(None),
            valid_to: Some(None),
            extensions: Some(vec![]),
            ..Default::default()
        },
        "user",
    )
    .unwrap(/* t */);

    c.restore(&s.id, 1, "user").unwrap(/* t */);

    let after = c.get(&s.id).unwrap(/* t */);
    // the content of version 1
    assert_eq!(after.body, "il corpo di partenza");
    assert_eq!(after.title, None);
    assert_eq!(after.tags, vec!["casa/milano".to_string()]);
    assert_eq!(after.scope, None);
    assert_eq!(
        after.valid_from.map(|v| v.to_db_string()),
        Some("2024-01-01".to_string())
    );
    assert_eq!(
        after.valid_to.map(|v| v.to_db_string()),
        Some("2024-12-31".to_string())
    );
    assert_eq!(
        after.extensions.len(),
        1,
        "the extensions of version 1 come back"
    );
    // the provenance of the note
    assert_eq!(
        after.source, "import:web",
        "the source is the note's provenance: it stays"
    );
    assert_eq!(
        after.created_at,
        at(0),
        "the birth date belongs to the note"
    );
    assert!(c.verify().unwrap(/* t */).ok(), "the cell verifies");
}

/// The two lists are the whole of `Note`: every field of the struct is in
/// exactly one — the content the restore brings back, or the lifecycle
/// and provenance it leaves — each with its reason. The lists are the
/// engine's own constants (the single place they are written, beside the
/// restore); the fields are read from the struct's DEFINITION, so a
/// field `Note` gains is in neither until someone says which it is (the
/// same idea as `anchor_tables`: a hand-written list held honest by the
/// thing it lists).
#[test]
fn every_field_of_note_is_content_or_lifecycle() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let format_src = std::fs::read_to_string(root.join("src/format/mod.rs")).unwrap(/* t */);
    // the fields of `pub struct Note { ... }`
    let start = format_src
        .find("pub struct Note {")
        .expect(/* t */ "Note is defined in format/mod.rs");
    // the fields begin on the line after the opening one
    let body_start = format_src[start..]
        .find('\n')
        .expect(/* t */ "the struct opens")
        + start;
    let end = format_src[body_start..]
        .find("\n}")
        .expect(/* t */ "the struct closes")
        + body_start;
    let fields: Vec<String> = format_src[body_start..end]
        .lines()
        .filter_map(|l| l.trim().strip_prefix("pub "))
        .filter_map(|l| l.split(':').next())
        .map(str::to_string)
        .collect();
    assert!(
        fields.len() >= 16,
        "the walk reads the struct's fields: {fields:?}"
    );
    let content: Vec<&str> = RESTORE_CONTENT_FIELDS.iter().map(|(f, _)| *f).collect();
    let kept: Vec<&str> = RESTORE_KEPT_FIELDS.iter().map(|(f, _)| *f).collect();
    for (name, list) in [
        ("RESTORE_CONTENT_FIELDS", &content),
        ("RESTORE_KEPT_FIELDS", &kept),
    ] {
        for f in list.iter() {
            assert!(
                fields.iter().any(|x| x == f),
                "{name} names `{f}`, which Note does not have"
            );
        }
    }
    for f in &content {
        assert!(!kept.contains(f), "`{f}` is in both lists");
    }
    let unlisted: Vec<&String> = fields
        .iter()
        .filter(|f| !content.contains(&f.as_str()) && !kept.contains(&f.as_str()))
        .collect();
    assert!(
        unlisted.is_empty(),
        "fields of Note in neither list — the restore would not know what to do with them: \
         {unlisted:?} (add each to RESTORE_CONTENT_FIELDS or RESTORE_KEPT_FIELDS in ops.rs, \
         with its reason)"
    );
    // each entry carries its reason
    for (f, reason) in RESTORE_CONTENT_FIELDS.iter().chain(RESTORE_KEPT_FIELDS) {
        assert!(
            reason.len() >= 12,
            "`{f}` has no reason worth the name: {reason:?}"
        );
    }
}
