// SPDX-License-Identifier: Apache-2.0

//! v0.4.1 (P0.1): a note's STATE changes only through `set_status`,
//! `confirm`, `delete` (and `restore`). A rewrite — `put_note` with an
//! existing id, or `patch_note` — PRESERVES the state: a quarantined note
//! cannot be laundered into `active` by rewriting it, and rewriting an
//! active note with `external` confidence quarantines it (never the other
//! way). A tombstone is not resurrected by a `put`: `CONFLICT`, restore it
//! from the history instead.

use engramma_core::format::{At, Note, NoteType};
use engramma_core::search::Query;
use engramma_core::{Cell, CellError, Confidence, NotePatch, Status};

fn t() -> At {
    At(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* test-only */))
}

fn note(id: u16, body: &str, confidence: Confidence, status: Status) -> Note {
    Note {
        id: ulid::Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some(format!("nota {id}")),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: "user".into(),
        confidence,
        status,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: t(),
        updated_at: t(),
        extensions: vec![],
    }
}

fn cell() -> (tempfile::TempDir, Cell) {
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t").unwrap(/* test-only */);
    (dir, cell)
}

fn found(cell: &Cell, text: &str) -> Vec<String> {
    cell.search(&Query::text(text).k(10))
        .unwrap(/* test-only */)
        .into_iter()
        .map(|h| h.note.id.to_string())
        .collect()
}

/// The dangerous rewrite of the point: a quarantined note rewritten with
/// `stated` confidence stays quarantined, out of the search; asking for
/// `active` explicitly is refused.
#[test]
fn a_rewrite_preserves_quarantine() {
    let (_d, cell) = cell();
    let q = note(
        1,
        "Il PIN di Fabio è 4321",
        Confidence::External,
        Status::Quarantined,
    );
    cell.put_note(&q, "t").unwrap(/* test-only */);
    assert!(found(&cell, "PIN Fabio").is_empty());

    // the rewrite: same id, stated confidence — the state survives it
    let mut rewrite = note(
        1,
        "Il PIN di Fabio è 4321",
        Confidence::Stated,
        Status::Quarantined,
    );
    rewrite.created_at = At(time::OffsetDateTime::now_utc());
    cell.put_note(&rewrite, "t").unwrap(/* test-only */);
    let after = cell.get(&q.id).unwrap(/* test-only */);
    assert_eq!(after.status, Status::Quarantined, "the rewrite launders it");
    assert!(found(&cell, "PIN Fabio").is_empty(), "it must stay hidden");

    // the same rewrite that ASKS for active is a conflict
    let mut laundering = note(
        1,
        "Il PIN di Fabio è 4321",
        Confidence::Stated,
        Status::Active,
    );
    laundering.created_at = At(time::OffsetDateTime::now_utc());
    let err = cell.put_note(&laundering, "t").unwrap_err(/* test-only */);
    assert!(
        matches!(err, CellError::Invalid(ref m) if m.contains("a rewrite preserves")),
        "{err}"
    );
    assert_eq!(
        cell.get(&q.id).unwrap(/* test-only */).status,
        Status::Quarantined
    );

    // confirm is the one door to active — and it works after all this
    cell.confirm(&q.id, "t").unwrap(/* test-only */);
    assert_eq!(cell.get(&q.id).unwrap(/* t */).status, Status::Active);
    assert!(found(&cell, "PIN Fabio").contains(&q.id.to_string()));
}

/// A rewrite with `external` confidence quarantines an active note (never
/// the opposite), and a patch keeps the state too.
#[test]
fn external_rewrite_quarantines_and_patch_preserves() {
    let (_d, cell) = cell();
    let a = note(
        2,
        "Fabio lavora alla Acme",
        Confidence::Stated,
        Status::Active,
    );
    cell.put_note(&a, "t").unwrap(/* test-only */);

    // external rewrite of an ACTIVE note: it goes to quarantine
    let mut ext = note(
        2,
        "Fabio lavora alla Acme",
        Confidence::External,
        Status::Quarantined,
    );
    ext.created_at = At(time::OffsetDateTime::now_utc());
    cell.put_note(&ext, "t").unwrap(/* test-only */);
    assert_eq!(cell.get(&a.id).unwrap(/* t */).status, Status::Quarantined);
    assert!(found(&cell, "Acme").is_empty());

    // a patch does not launder it either (stated confidence, same state)
    cell.patch_note(
        &a.id,
        &NotePatch {
            body: Some("Fabio lavora alla Beta".into()),
            confidence: Some(Confidence::Stated),
            ..Default::default()
        },
        "t",
    )
    .unwrap(/* test-only */);
    assert_eq!(cell.get(&a.id).unwrap(/* t */).status, Status::Quarantined);
    // while a patch of an ACTIVE note with external confidence quarantines
    let b = note(
        3,
        "Giulia suona il piano",
        Confidence::Stated,
        Status::Active,
    );
    cell.put_note(&b, "t").unwrap(/* test-only */);
    cell.patch_note(
        &b.id,
        &NotePatch {
            confidence: Some(Confidence::External),
            ..Default::default()
        },
        "t",
    )
    .unwrap(/* test-only */);
    assert_eq!(cell.get(&b.id).unwrap(/* t */).status, Status::Quarantined);
}

/// v0.6 (P1): a rewrite by a writer who had SEEN the lapide reopens the
/// note — their base IS the tombstone. (A rewrite that did not see it never
/// reaches `apply`: the materializer's tombstone rule holds it back, and
/// the lapide wins — the convergence tests own that half.)
#[test]
fn a_tombstone_reopens_for_a_writer_who_saw_it() {
    let (_d, cell) = cell();
    let n = note(
        4,
        "nota che finirà lapide",
        Confidence::Stated,
        Status::Active,
    );
    cell.put_note(&n, "t").unwrap(/* test-only */);
    cell.delete_note(&n.id, "t").unwrap(/* test-only */);

    // a local patch: the writer had the tombstone in front of them
    cell.patch_note(
        &n.id,
        &NotePatch {
            body: Some("riscritta dopo la lapide".into()),
            ..Default::default()
        },
        "t",
    )
    .unwrap(/* test-only */);
    let back = cell.get(&n.id).unwrap(/* test-only */);
    assert_eq!(back.status, Status::Active, "the note reopens");
    assert!(back.body.contains("dopo la lapide"));

    // and a full rewrite reopens it too, preserving the birth date
    cell.delete_note(&n.id, "t").unwrap(/* test-only */);
    let mut same_id = note(
        4,
        "tentativo di resurrezione",
        Confidence::Stated,
        Status::Active,
    );
    same_id.created_at = At(time::OffsetDateTime::now_utc());
    cell.put_note(&same_id, "t").unwrap(/* test-only */);
    let back = cell.get(&n.id).unwrap(/* test-only */);
    assert_eq!(back.status, Status::Active, "the rewrite reopens");
    assert_eq!(back.created_at, n.created_at, "the birth date stays");
    let vr = cell.verify().unwrap(/* test-only */);
    assert!(vr.ok(), "{vr:?}");
}

/// v0.4.1 (P1.4): the birth date belongs to the NOTE, not to the version —
/// a rewrite keeps it (the edit time is `updated_at`).
#[test]
fn a_rewrite_preserves_the_birth_date() {
    let (_d, cell) = cell();
    let mut a = note(7, "nota migrata", Confidence::Stated, Status::Active);
    let birth = At(time::OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap(/* t */));
    a.created_at = birth;
    a.updated_at = birth;
    cell.put_note(&a, "t").unwrap(/* test-only */);

    let mut b = note(7, "nota riscritta", Confidence::Stated, Status::Active);
    b.created_at = At(time::OffsetDateTime::now_utc());
    b.updated_at = At(time::OffsetDateTime::now_utc());
    cell.put_note(&b, "t").unwrap(/* test-only */);

    let back = cell.get(&a.id).unwrap(/* test-only */);
    assert_eq!(
        back.created_at, birth,
        "the rewrite moved the birth date: {:?}",
        back.created_at
    );
    assert!(
        back.updated_at > birth,
        "the edit time moved: {:?}",
        back.updated_at
    );
}
