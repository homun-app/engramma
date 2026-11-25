// SPDX-License-Identifier: Apache-2.0

//! v0.6.8 (H1): the bin has ONE predicate. A note is in the bin when it
//! is deleted, or when it is a conflict copy of a deleted note — one
//! definition, one place, every read path speaking it. The v0.6.7 rule
//! lived in four hand-written filters and `note_ids()`/`list()` and
//! `get()` forgot the copy: the reviewer's proof (below) is that hole.
//!
//! v0.6.9 (I2) narrows the rule to the copies NOBODY took: a copy is
//! bin when the original holds a lapide AND the copy is still OPEN. A
//! copy the user TOOK (modified — the witness (a) of DESIGN § 4.3) is
//! the user's own note and lives its own life, lapide or no lapide. The
//! reviewer's second proof (below) is the hole the tag shortcut left:
//! `get()` asked the predicate only behind the user-writable
//! `conflitto` tag.
//!
//! The contract test walks EVERY public read of a Cell that speaks
//! notes — the list sits in [`READS_THE_NOTES`], one place — and the
//! gate (`every_public_read_is_classified`) fails when a new `pub fn`
//! on Cell misses a bucket: the next forgotten door closes itself.

mod common;

use common::*;
use engramma_core::embed::{Embedder, TextKind};
use engramma_core::error::EmbedError;
use engramma_core::search::{Query, Semantic};
use engramma_core::{Cell, NotePatch};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use time::OffsetDateTime;
use ulid::Ulid;

/// The reviewer's proof of the v0.6.7 verification, verbatim in shape:
/// after the `delete` of the original, `conflicts()` is empty, but the
/// copy is still in `note_ids()` (therefore `list()`), and `get(copy)`
/// answers `Active, ["conflitto"]` with the whole body.
#[test]
fn zz_the_copy_of_a_deleted_note_is_in_note_ids_and_get() {
    let init = shared_init("zz-lapide7");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(9, "uno\ndue\ntre");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    wa.store(K + 2, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("uno\nzibellinoxa\ntre".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    wb.store(K + 5, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("uno\nDUE-di-B\ntre".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let copies = open_copies(&b, &id);
    assert_eq!(copies.len(), 1);
    let copy = Ulid::from_string(&copies[0].0).unwrap(/* t */);
    wb.store(K + 9, Ordering::SeqCst);
    b.delete_note(&id, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    assert!(
        b.conflicts().unwrap(/* t */).is_empty(),
        "v0.6.7 already hides the copy from conflicts()"
    );
    let ids = b.note_ids().unwrap(/* t */);
    assert!(
        !ids.contains(&copy),
        "the copy of a bin's note appears in note_ids (list())"
    );
    match b.get(&copy) {
        Err(engramma_core::CellError::NoteNotFound(_)) => {}
        other => panic!("get(copy) must answer like a deleted note (NotFound); got {other:?}"),
    }
}

/// v0.6.9 (I2): the reviewer's second proof, verbatim in shape
/// (`zz_review_v068.rs` of the verification). The user TAKES the open
/// copy — a patch on its id, the witness (a) that closes it — and
/// re-tags it (the tag is the user's, `conflitto` is gone). After the
/// `delete` of the original the copy is THE USER'S NOTE: it stays
/// visible, in `note_ids()` AND in `get()`. The v0.6.8 read hid it in
/// `note_ids()` (the copy arm looked at open AND closed copies) while
/// `get()` showed it `Active` behind the tag shortcut — two doors, two
/// answers.
#[test]
fn zz_the_taken_copy_of_a_deleted_note_is_visible_everywhere() {
    let init = shared_init("zz-lapide8");
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(9, "uno\ndue\ntre");
    let id = note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    wa.store(K + 2, Ordering::SeqCst);
    a.patch_note(
        &id,
        &NotePatch {
            body: Some("uno\nzibellinoxb\ntre".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    wb.store(K + 5, Ordering::SeqCst);
    b.patch_note(
        &id,
        &NotePatch {
            body: Some("uno\nDUE-di-B\ntre".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let copies = open_copies(&b, &id);
    assert_eq!(copies.len(), 1);
    let copy = Ulid::from_string(&copies[0].0).unwrap(/* t */);
    // the user takes the copy: a patch on its id (the witness (a)) that
    // also re-tags it — the tag is the user's now
    wb.store(K + 7, Ordering::SeqCst);
    b.patch_note(
        &copy,
        &NotePatch {
            tags: Some(vec!["mio".into()]),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    // the take closed the copy: the derivation says 0/1 (open/visible)
    let closed = b.debug_column(&format!(
        "SELECT open || '/' || visible FROM conflict_copies WHERE copy_id = '{copy}'"
    ))
    .unwrap(/* t */);
    assert_eq!(
        closed.first().map(String::as_str),
        Some("0/0"),
        "the user's patch is the witness (a): the copy is closed \
(open=0; the derivation's window bit goes with it)"
    );
    wb.store(K + 9, Ordering::SeqCst);
    b.delete_note(&id, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let ids = b.note_ids().unwrap(/* t */);
    assert!(
        ids.contains(&copy),
        "a taken copy is the user's note: note_ids (list()) keeps it"
    );
    match b.get(&copy) {
        Ok(n) => assert!(
            n.tags.iter().any(|t| t == "mio"),
            "get answers the taken copy with the user's tags"
        ),
        other => panic!("get(copy) must answer the user's note; got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The stage: a two-device cell, an original with an open HUMAN copy (the
// loser's body carries a searchable token, a tag, a mention and two
// functional relations), an alive control note, and then the delete.
// ---------------------------------------------------------------------------

/// A deterministic fake embedder (the same idea the indistinguishability
/// suite uses): the text hashed into a fixed-dim unit vector, no model.
struct FakeEmbedder;

const DIM: usize = 16;

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[&str], _kind: TextKind) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = [0.0_f32; DIM];
                for (i, b) in t.bytes().enumerate() {
                    v[(i + usize::from(b)) % DIM] += f32::from(b) / 255.0;
                }
                let norm = (v.iter().map(|x| x * x).sum::<f32>() + 1e-9).sqrt();
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }

    fn model_id(&self) -> &str {
        "fake-16"
    }

    fn dim(&self) -> usize {
        DIM
    }

    fn max_tokens(&self) -> usize {
        512
    }
}

/// The loser's body: a token only the copy carries, a tag only the
/// losing patch sets, a mention only the copy declares, and two
/// functional relations (the contradiction candidate the copy becomes
/// while it lives).
const LOSER_BODY: &str = "prima\nocchiello turchino verso [[Namorio]]\nterza

## Relations
- lives_in:: [[Salerno]]
- lives_in:: [[Aversa]]";

const WINNER_BODY: &str = "prima\nmanopola smeraldo\nterza";

const ALIVE_BODY: &str = "appunto vivo che fa da controllo

## Relations
- works_at:: [[Officina]]
- works_at:: [[Cantiere]]";

/// The stage both contract tests start from: `b` holds an open visible
/// copy of `original` (human loser, so ACTIVE), an alive control note,
/// and then the original's delete — the copy is bin. Returns
/// `(b, original, copy, alive)`.
fn stage_with_bin_copy(tag: &str) -> (Kept, Ulid, Ulid, Ulid) {
    let init = shared_init(tag);
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(9, "prima\nseconda\nterza");
    let original = note.id;
    let alive_note = base_note(11, ALIVE_BODY);
    let alive = alive_note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    wa.store(K + 2, Ordering::SeqCst);
    a.put_note(&alive_note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    // the loser: A's human hand, EARLIER, with the tag only it sets
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &original,
        &NotePatch {
            body: Some(LOSER_BODY.into()),
            tags: Some(vec!["spicchio".into()]),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    // the winner: B's human hand, later
    wb.store(K + 5, Ordering::SeqCst);
    b.patch_note(
        &original,
        &NotePatch {
            body: Some(WINNER_BODY.into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let list = b.conflicts().unwrap(/* t */);
    assert_eq!(list.len(), 1, "the conflict is open before the delete");
    let copy = list[0].copy_id;
    // the copy lives: vectors too, so the semantic arm has something to
    // leak before the fix
    b.embed_pending(&FakeEmbedder, 64).unwrap(/* t */);
    // the bin: the original goes, the copy follows the lapide
    wb.store(K + 9, Ordering::SeqCst);
    b.delete_note(&original, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    (b, original, copy, alive)
}

/// v0.6.9 (I2): the same stage, with the user TAKING the copy before the
/// delete — a header patch on its id (the witness (a)) that re-tags it
/// `mio`. The copy closes; when the original goes to the bin, the copy
/// is the user's note and STAYS. Returns `(b, original, copy, alive)`.
fn stage_with_taken_copy(tag: &str) -> (Kept, Ulid, Ulid, Ulid) {
    let init = shared_init(tag);
    let (a, wa) = cell_with("a");
    let (b, wb) = cell_with("b");
    a.ingest_op(&init).unwrap(/* t */);
    b.ingest_op(&init).unwrap(/* t */);
    let note = base_note(9, "prima\nseconda\nterza");
    let original = note.id;
    let alive_note = base_note(11, ALIVE_BODY);
    let alive = alive_note.id;
    wa.store(K + 1, Ordering::SeqCst);
    a.put_note(&note, "homun:user").unwrap(/* t */);
    wa.store(K + 2, Ordering::SeqCst);
    a.put_note(&alive_note, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* t */);
    }
    wa.store(K + 3, Ordering::SeqCst);
    a.patch_note(
        &original,
        &NotePatch {
            body: Some(LOSER_BODY.into()),
            tags: Some(vec!["spicchio".into()]),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    wb.store(K + 5, Ordering::SeqCst);
    b.patch_note(
        &original,
        &NotePatch {
            body: Some(WINNER_BODY.into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let list = b.conflicts().unwrap(/* t */);
    assert_eq!(list.len(), 1, "the conflict is open before the take");
    let copy = list[0].copy_id;
    // the take: the user's patch on the copy's id, with their own tag
    wb.store(K + 7, Ordering::SeqCst);
    b.patch_note(
        &copy,
        &NotePatch {
            tags: Some(vec!["mio".into()]),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    let closed = b.debug_column(&format!(
        "SELECT open || '/' || visible FROM conflict_copies WHERE copy_id = '{copy}'"
    ))
    .unwrap(/* t */);
    assert_eq!(
        closed.first().map(String::as_str),
        Some("0/0"),
        "the take closed the copy (witness (a); the derivation's \
window bit goes with it)"
    );
    // the vectors of the taken note: the semantic arm has something to say
    b.embed_pending(&FakeEmbedder, 64).unwrap(/* t */);
    // the original goes to the bin all the same
    wb.store(K + 9, Ordering::SeqCst);
    b.delete_note(&original, "homun:user").unwrap(/* t */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* t */);
        b.ingest_op(op).unwrap(/* t */);
    }
    (b, original, copy, alive)
}

/// Every public read of a Cell that SPEAKS NOTES, in one place. The
/// contract test below walks this list; the gate demands the list stays
/// complete. `conflict` is here on purpose: the read BY ID stays honest
/// and says `originalDeleted` — it is the one door that may name the
/// copy, saying it is a bin's contents.
const READS_THE_NOTES: &[&str] = &[
    "note_ids",
    "note_ids_all",
    "get",
    "purge_preview",
    "history",
    "note_version",
    "search",
    "search_with_now",
    "search_semantic",
    "search_semantic_with_now",
    "neighbors",
    "path",
    "tags",
    "entities",
    "timeline",
    "conflicts",
    "conflict",
    "proposals",
    "consolidation_candidates",
    "resolve_visible_note_id",
];

/// The contract: the copy of a deleted note appears NOWHERE. Each check
/// names itself as it runs, and the walk must cover [`READS_THE_NOTES`]
/// exactly — a check removed from the body fails the test by name.
#[test]
fn the_bin_copy_appears_in_no_public_read() {
    let (b, original, copy, alive) = stage_with_bin_copy("contratto");
    let mut walked: Vec<&'static str> = Vec::new();
    let copy_s = copy.to_string();

    // note_ids: the living-only view (list() with no status filter)
    walked.push("note_ids");
    let ids = b.note_ids().unwrap(/* t */);
    assert!(ids.contains(&alive), "control: the alive note is listed");
    assert!(!ids.contains(&copy), "note_ids lists the bin's copy");

    // note_ids_all: the tombstones view — the ORIGINAL stays (restore
    // wants it), the copy does not
    walked.push("note_ids_all");
    let all = b.note_ids_all().unwrap(/* t */);
    assert!(all.contains(&original), "control: the tombstone stays");
    assert!(!all.contains(&copy), "note_ids_all lists the bin's copy");

    // get: the closed oracle — the copy answers like a deleted note
    walked.push("get");
    assert!(
        b.get(&original).is_ok(),
        "control: a tombstone still reads by id"
    );
    match b.get(&copy) {
        Err(engramma_core::CellError::NoteNotFound(_)) => {}
        other => panic!("get(copy) must be NoteNotFound; got {other:?}"),
    }

    // history / note_version: the by-id reads answer as for a tombstone
    // (they serve restore), never as a listing
    walked.push("history");
    b.history(&copy).unwrap(/* t */);
    walked.push("note_version");
    b.note_version(&copy).unwrap(/* t */);
    // the purge preview reads the supersedes rows: the bin's copy closes
    // nothing, and its own preview says so
    walked.push("purge_preview");
    b.purge_preview(&copy).unwrap(/* t */);

    // search, both views: the copy's token answers nothing
    walked.push("search");
    let mut wide = Query::text("occhiello turchino").k(10);
    wide.filters.include_quarantined = true;
    for q in [Query::text("occhiello turchino").k(10), wide] {
        let hits = b.search(&q).unwrap(/* t */);
        assert!(
            hits.iter().all(|h| h.note.id != copy),
            "search answers the bin's copy (quarantine={:?})",
            q.filters.include_quarantined
        );
    }
    let control = b.search(&Query::text("appunto vivo").k(10)).unwrap(/* t */);
    assert!(
        control.iter().any(|h| h.note.id == alive),
        "control: the alive note is searchable"
    );
    // search_with_now: the same path, an explicit clock
    walked.push("search_with_now");
    let now = OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */);
    let hits = b
        .search_with_now(&Query::text("occhiello turchino").k(10), now)
        .unwrap(/* t */);
    assert!(hits.iter().all(|h| h.note.id != copy));

    // the semantic arm: the vectors know the copy, the read must not
    // say it
    walked.push("search_semantic");
    walked.push("search_semantic_with_now");
    let sem = Semantic::new(Arc::new(FakeEmbedder));
    let outcome = b
        .search_semantic_with_now(&Query::text("occhiello turchino").k(10), now, &sem)
        .unwrap(/* t */);
    assert!(
        outcome.hits.iter().all(|h| h.note.id != copy),
        "the semantic arm answers the bin's copy"
    );

    // neighbors: the copy is not a node to walk to, nor a declaring note
    walked.push("neighbors");
    for start in ["Namorio", copy_s.as_str()] {
        let near = b.neighbors(start, 1, &[], false).unwrap(/* t */);
        assert!(
            near.iter().all(|n| {
                n.note_id.as_deref() != Some(copy_s.as_str()) && n.entity != format!("note:{copy}")
            }),
            "neighbors names the bin's copy from {start}"
        );
    }

    // path: no walk passes THROUGH the copy
    walked.push("path");
    for other in ["Officina", "Cantiere", "Namorio"] {
        if let Some(steps) = b.path("Namorio", other, false).unwrap(/* t */) {
            assert!(
                steps
                    .iter()
                    .all(|s| s.from != format!("note:{copy}") && s.to != format!("note:{copy}")),
                "path walks through the bin's copy (Namorio → {other})"
            );
        }
    }

    // tags: the count of a tag only the copy carries is zero
    walked.push("tags");
    for include in [false, true] {
        let counted = b.tags("spicchio", include).unwrap(/* t */);
        let zero: usize = counted
            .iter()
            .map(|t| usize::try_from(t.note_count).unwrap_or(0))
            .sum();
        assert_eq!(zero, 0, "tags counts the bin's copy (quarantine={include})");
    }

    // entities: the listing may know Namorio (the row stays, like a
    // tombstone's), but the copy is not an entity's note
    walked.push("entities");
    let ents = b.entities("Namorio").unwrap(/* t */);
    assert!(
        ents.iter()
            .all(|e| e.note_id.as_deref() != Some(copy_s.as_str())),
        "entities binds the bin's copy as an entity's note"
    );

    // timeline: the entity's story has no chapter from the copy
    walked.push("timeline");
    for include in [false, true] {
        let hits = b.timeline("Namorio", include).unwrap(/* t */);
        assert!(
            hits.iter().all(|h| h.note.id != copy),
            "timeline shows the bin's copy (quarantine={include})"
        );
    }
    let control = b.timeline("Officina", false).unwrap(/* t */);
    assert!(
        control.iter().any(|h| h.note.id == alive),
        "control: the alive note answers the timeline"
    );

    // conflicts: the listing is empty (v0.6.7's own rule, kept)
    walked.push("conflicts");
    assert!(b.conflicts()
            .unwrap(/* t */)
            .iter()
            .all(|c| c.copy_id != copy));

    // conflict: the read BY ID stays honest — originalDeleted: true
    walked.push("conflict");
    let detail = b.conflict(&copy).unwrap(/* t */);
    assert!(detail.original_deleted, "the by-id read names the lapide");

    // the queue of proposals: no pending row of the copy survives the
    // lapide (the agent's copy rides this queue while it lives)
    walked.push("proposals");
    let queue = b
        .proposals(&engramma_core::ProposalsFilter::default())
        .unwrap(/* t */);
    assert!(
        queue.iter().all(|p| p.id != copy),
        "the proposals queue names the bin's copy"
    );

    // consolidation: the copy is nobody's duplicate nor contradiction
    walked.push("consolidation_candidates");
    let cfg = engramma_core::Consolidation::default();
    let candidates = b.consolidation_candidates(50, &cfg).unwrap(/* t */);
    assert!(
        candidates.iter().all(|c| c.a != copy && c.b != copy),
        "consolidation proposes the bin's copy"
    );

    // resolve_visible_note_id: the copy does not exist for surfaces that
    // must not reveal hidden notes
    walked.push("resolve_visible_note_id");
    let suffix: String = copy_s.chars().skip(copy_s.len() - 8).collect();
    if let Ok(resolved) = b.resolve_visible_note_id(&suffix) {
        panic!("resolve_visible_note_id answers the bin's copy ({resolved})");
    }

    // the export: no file of the folder carries the copy
    let dir = common::test_dir();
    let summary = engramma_core::port::export(&b, dir.path()).unwrap(/* t */);
    assert_eq!(
        summary.written, 1,
        "only the alive control note is written ({summary:?})"
    );
    assert!(
        summary.tombstones >= 1,
        "the deleted original counts as a tombstone ({summary:?})"
    );
    for file in std::fs::read_dir(dir.path()).unwrap(/* t */).flatten() {
        let text = std::fs::read_to_string(file.path()).unwrap_or_default();
        assert!(
            !text.contains("occhiello turchino"),
            "the export writes the bin's copy: {}",
            file.path().display()
        );
    }

    // verify stays green: hiding is a read rule, not a state change
    assert!(b.verify().unwrap(/* t */).ok(), "verify after the delete");

    // the walk covered the whole list
    let missing: Vec<&str> = READS_THE_NOTES
        .iter()
        .filter(|n| !walked.contains(n))
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "the contract forgot to walk: {missing:?} — walk them or take \
         them out of READS_THE_NOTES (and say why in the REPORT)"
    );
}

/// The whole of it comes back together when the note reopens: one
/// predicate means one flip.
#[test]
fn the_reopen_brings_the_copy_back_whole() {
    let (b, original, copy, _alive) = stage_with_bin_copy("riapertura");
    // the reopen: a body write whose base IS the lapide (the local
    // writer's base is the closed line, by the v0.6.5 rule)
    b.patch_note(
        &original,
        &NotePatch {
            body: Some("prima\nriaperta per prova\nterza".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* t */);
    let ids = b.note_ids().unwrap(/* t */);
    assert!(ids.contains(&copy), "the copy returns to note_ids");
    let listed = b.conflicts().unwrap(/* t */);
    assert!(
        listed.iter().any(|c| c.copy_id == copy),
        "the copy returns to conflicts()"
    );
    assert!(
        b.get(&copy)
            .is_ok_and(|n| n.tags.iter().any(|t| t == "conflitto")),
        "the copy reads again, whole"
    );
    let hits = b
        .search(&Query::text("occhiello turchino").k(10))
        .unwrap(/* t */);
    assert!(
        hits.iter().any(|h| h.note.id == copy),
        "the copy is searchable again"
    );
}

/// v0.6.9 (I2): the same walk as the bin's, on the copy the user TOOK
/// before the delete — every read answers it like the living note it
/// now is. One predicate means one flip, in the other direction.
#[test]
fn the_taken_copy_answers_every_read_like_a_living_note() {
    let (b, original, copy, alive) = stage_with_taken_copy("contratto-v069");
    let mut walked: Vec<&'static str> = Vec::new();
    let copy_s = copy.to_string();

    // note_ids: the taken copy IS a living note
    walked.push("note_ids");
    let ids = b.note_ids().unwrap(/* t */);
    assert!(ids.contains(&alive), "control: the alive note is listed");
    assert!(
        ids.contains(&copy),
        "note_ids hides the taken copy (the user's note)"
    );

    // note_ids_all: living notes are there, tombstones included
    walked.push("note_ids_all");
    let all = b.note_ids_all().unwrap(/* t */);
    assert!(all.contains(&original), "control: the tombstone stays");
    assert!(all.contains(&copy), "note_ids_all hides the taken copy");

    // get: the copy reads, with the user's tag
    walked.push("get");
    let got = b.get(&copy).unwrap(/* t */);
    assert!(
        got.tags.iter().any(|t| t == "mio"),
        "get answers the taken copy with the user's tags"
    );

    // history / note_version: as ever
    walked.push("history");
    b.history(&copy).unwrap(/* t */);
    walked.push("note_version");
    b.note_version(&copy).unwrap(/* t */);
    // and the preview reads it like any living note
    walked.push("purge_preview");
    b.purge_preview(&copy).unwrap(/* t */);

    // search, both views: the copy's token answers
    walked.push("search");
    let mut wide = Query::text("occhiello turchino").k(10);
    wide.filters.include_quarantined = true;
    for q in [Query::text("occhiello turchino").k(10), wide] {
        let hits = b.search(&q).unwrap(/* t */);
        assert!(
            hits.iter().any(|h| h.note.id == copy),
            "search does not answer the taken copy (quarantine={:?})",
            q.filters.include_quarantined
        );
    }
    walked.push("search_with_now");
    let now = OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */);
    let hits = b
        .search_with_now(&Query::text("occhiello turchino").k(10), now)
        .unwrap(/* t */);
    assert!(hits.iter().any(|h| h.note.id == copy));

    // the semantic arm: the vectors say the taken note
    walked.push("search_semantic");
    walked.push("search_semantic_with_now");
    let sem = Semantic::new(Arc::new(FakeEmbedder));
    let outcome = b
        .search_semantic_with_now(&Query::text("occhiello turchino").k(10), now, &sem)
        .unwrap(/* t */);
    assert!(
        outcome.hits.iter().any(|h| h.note.id == copy),
        "the semantic arm does not answer the taken copy"
    );

    // neighbors: the copy is a node again
    walked.push("neighbors");
    let near = b.neighbors("Namorio", 1, &[], false).unwrap(/* t */);
    assert!(
        near.iter()
            .any(|n| n.note_id.as_deref() == Some(copy_s.as_str())),
        "neighbors does not name the taken copy from Namorio"
    );

    // path: a walk may pass through the copy (it is the only Namorio)
    walked.push("path");
    if let Some(steps) = b.path("Namorio", "Salerno", false).unwrap(/* t */) {
        assert!(
            steps
                .iter()
                .any(|s| s.from == format!("note:{copy}") || s.to == format!("note:{copy}")),
            "a path out of Namorio does not pass through the taken copy"
        );
    }

    // tags: the user's tag counts one (the copy carries it)
    walked.push("tags");
    for include in [false, true] {
        let counted = b.tags("mio", include).unwrap(/* t */);
        let total: usize = counted
            .iter()
            .map(|t| usize::try_from(t.note_count).unwrap_or(0))
            .sum();
        assert_eq!(
            total, 1,
            "tags counts the taken copy (quarantine={include})"
        );
    }

    // entities: the mention binds no title — nothing to assert beyond
    // the call itself (the original's title is not Namorio)
    walked.push("entities");
    b.entities("Namorio").unwrap(/* t */);

    // timeline: the entity's story has the copy's chapter again
    walked.push("timeline");
    for include in [false, true] {
        let hits = b.timeline("Namorio", include).unwrap(/* t */);
        assert!(
            hits.iter().any(|h| h.note.id == copy),
            "timeline hides the taken copy (quarantine={include})"
        );
    }

    // conflicts: the listing of OPEN copies does not name it (the take
    // closed it — that is what a take IS)
    walked.push("conflicts");
    assert!(b
        .conflicts()
        .unwrap(/* t */)
        .iter()
        .all(|c| c.copy_id != copy));

    // conflict: the read BY ID — the TAKEN copy is closed, there is no
    // conflict left to describe: `NotFound` is the honest word (the
    // open-in-bin copy keeps answering with `originalDeleted: true`,
    // the walk above covers that side)
    walked.push("conflict");
    match b.conflict(&copy) {
        Err(engramma_core::CellError::NoteNotFound(_)) => {}
        other => panic!("conflict(copy) on a taken copy: {other:?}"),
    }

    // the queue: no pending row (the copy is not a proposal)
    walked.push("proposals");
    let queue = b
        .proposals(&engramma_core::ProposalsFilter::default())
        .unwrap(/* t */);
    assert!(queue.iter().all(|p| p.id != copy));

    // consolidation: the copy's own contradiction (two lives_in) is a
    // candidate again — it was hidden with the copy
    walked.push("consolidation_candidates");
    let cfg = engramma_core::Consolidation::default();
    let candidates = b.consolidation_candidates(50, &cfg).unwrap(/* t */);
    assert!(
        candidates.iter().any(|c| c.a == copy || c.b == copy),
        "consolidation does not propose the taken copy's contradiction"
    );

    // resolve_visible_note_id: the copy exists for the surfaces
    walked.push("resolve_visible_note_id");
    let suffix: String = copy_s.chars().skip(copy_s.len() - 8).collect();
    let resolved = b
        .resolve_visible_note_id(&suffix)
        .expect("resolve_visible_note_id hides the taken copy");
    assert_eq!(resolved, copy);

    // the export: the copy's file is written
    let dir = common::test_dir();
    let summary = engramma_core::port::export(&b, dir.path()).unwrap(/* t */);
    assert_eq!(
        summary.written, 2,
        "the alive note AND the taken copy are written ({summary:?})"
    );
    assert!(
        summary.tombstones >= 1,
        "the deleted original counts as a tombstone ({summary:?})"
    );

    // verify stays green: hiding is a read rule, not a state change
    assert!(b.verify().unwrap(/* t */).ok(), "verify after the delete");

    // the walk covered the whole list
    let missing: Vec<&str> = READS_THE_NOTES
        .iter()
        .filter(|n| !walked.contains(n))
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "the contract forgot to walk: {missing:?} — walk them or take \
         them out of READS_THE_NOTES (and say why in the REPORT)"
    );
}

// ---------------------------------------------------------------------------
// The gate: every `pub fn …(&self` of Cell, in the sources, is in exactly
// one bucket. The parser is deliberately simple, like the properties
// inventory's: it walks the files that hold `impl Cell` blocks and
// records every `pub fn name(` whose parameters include `&self`. What it
// does NOT see (declared in the REPORT): functions of other modules,
// associated functions (`Cell::create`), and impl blocks of other types.
// ---------------------------------------------------------------------------

/// Reads that do not speak notes (each with its reason).
const READS_OTHER: &[(&str, &str)] = &[
    ("policy_of", "legge la politica, non le note"),
    ("policy", "la politica grezza, non le note"),
    ("decisions", "lo storico delle decisioni, non le note"),
    (
        "revert_preview",
        "v0.6.18 (F1): l'anteprima di un annullamento, non le note",
    ),
    ("meta", "i metadati del file, non le note"),
    ("models", "i modelli di embedding, non le note"),
    ("file_path", "il percorso del file"),
    ("scrub_state", "lo stato della purga, bookkeeping"),
    (
        "vault_files_of_view",
        "i legami locali della vista markdown, non le note",
    ),
    (
        "vault_file_of_note",
        "il legame locale di un file della vista (la VISTA legge note_ids e get, che già filtrano)",
    ),
    ("vault_views", "le viste legate, bookkeeping locale"),
    ("vault_view_by_root", "come sopra"),
    ("vault_view_by_id", "come sopra"),
    ("vault_views_of_note", "come sopra"),
    (
        "vault_conflicts_of",
        "i conflitti di file della vista, locali",
    ),
    ("vault_conflicts_of_view", "come sopra"),
];

/// Internals and diagnostics that see the tables BY DESIGN (tests,
/// verify, the sync): the bin rule is a rule of the READS, not of the
/// dumps.
const INTERNAL: &[(&str, &str)] = &[
    (
        "verify",
        "l'integrità legge tutto per nome, e deve restare verde",
    ),
    (
        "state_digest",
        "il digest dello stato, per i test e la sync",
    ),
    ("replicated_digest", "come sopra, lato insieme"),
    ("replicated_dump", "il dump delle tabelle logiche"),
    ("debug_column", "la lente dei test"),
    ("rematerialize_for_test", "l'oracolo differenziale"),
    ("conflict_deserved_for_test", "il gancio dei test"),
    ("conflict_witness_for_test", "il gancio dei test"),
    ("ops", "il registro, superficie della sync"),
    ("changes_since", "il registro, superficie della sync"),
    ("holds_content_hash", "il registro, superficie della sync"),
    ("op_of_hash", "il registro, superficie della sync"),
    ("ops_marker", "bookkeeping del registro"),
    ("data_version", "bookkeeping della cache"),
    ("stable_frontier", "bookkeeping della sync"),
];

/// Writes and local bookkeeping that mutate: no reason needed, but the
/// NAME sits here all the same — a new `pub fn` on Cell fails the gate
/// until someone says what it is, and the next forgotten door closes
/// itself.
const WRITES: &[&str] = &[
    "retire",
    // v0.6.10 (J2): the explicit repair — a write, the diagnosis stays
    // read-only (verify)
    "repair",
    "checkpoint",
    "set_query_cache_capacity",
    "build_graph",
    "set_signer",
    "set_signer_gate",
    "inject_clock",
    "inject_minter",
    "ingest_op",
    "purge",
    "put_note",
    "patch_note",
    "set_status",
    "supersede",
    "resolve_conflict",
    "delete_note",
    "restore",
    "link",
    "unlink",
    "confirm",
    "add_model",
    "embed_pending",
    "embed_all",
    "vault_file_put",
    "vault_view_put",
    "vault_unbind",
    "vault_conflict_put",
    "vault_conflict_delete",
    "vault_file_delete",
    "record_session_ack",
    "adopt_log",
    "mark_shared",
    "compact",
    "resolve_note_id",
    "propose",
    "decide",
    "revert_decision",
    "revert_decisions",
    "review_failed",
    "set_policy",
];

/// The files that hold `impl Cell` blocks.
const IMPL_FILES: &[&str] = &[
    "crates/engramma-core/src/cell/mod.rs",
    "crates/engramma-core/src/cell/policy.rs",
    "crates/engramma-core/src/cell/proposals.rs",
    "crates/engramma-core/src/search.rs",
    "crates/engramma-core/src/consolidation.rs",
];

/// Every `pub fn name(&self` of `impl Cell` in the sources: the gate's
/// own eyes, on the same brace-counting idea as the properties
/// inventory. The FLOOR of each open `impl Cell` block is the depth the
/// line after its opening brace sits at; a `pub fn` line is inside while
/// its starting depth is at or above the floor. A `//`-comment line is
/// never counted (it can NAME `impl Cell {`; block comments and braces
/// inside strings are a declared limit, as in the properties parser).
fn cell_methods_of(src: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut floors: Vec<usize> = Vec::new();
    let mut depth = 0_usize;
    let mut offset = 0_usize;
    for line in src.lines() {
        if line.trim_start().starts_with("//") {
            offset += line.len() + 1;
            continue;
        }
        let opens = line.matches('{').count();
        let closes = line.matches('}').count();
        if line.contains("impl Cell") && line.contains('{') {
            floors.push(depth + opens - closes);
        }
        let inside = !floors.is_empty();
        let trimmed = line.trim_start();
        if inside
            && trimmed.starts_with("pub fn ")
            && let Some(rest) = trimmed.strip_prefix("pub fn ")
            && let Some(name) = rest.split(['(', ':']).next()
        {
            // the parameters sit on this line or the ones right after:
            // read a window from the line's own offset, stop at the
            // first `)`
            let ahead = &src[offset..(offset + 250).min(src.len())];
            let sig_end = ahead.find(')').unwrap_or(ahead.len());
            let sig = &ahead[..sig_end];
            let name = name.trim();
            if sig.contains("&self") && !name.is_empty() && !found.iter().any(|f| f == name) {
                found.push(name.to_string());
            }
        }
        depth = (depth + opens).saturating_sub(closes);
        floors.retain(|&f| depth >= f);
        offset += line.len() + 1;
    }
    found
}

#[test]
fn every_public_read_is_classified() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.ancestors().nth(2).expect(
        /* test-only */ "the workspace root sits two levels above engramma-core",
    );
    let mut methods: Vec<String> = Vec::new();
    for rel in IMPL_FILES {
        let text = std::fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("lettura di {rel} fallita: {e}"));
        for name in cell_methods_of(&text) {
            if !methods.contains(&name) {
                methods.push(name);
            }
        }
    }
    assert!(
        methods.len() > 40,
        "the parser's eyes are shut ({} methods) — fix it before trusting the gate",
        methods.len()
    );
    let reads: Vec<&str> = READS_THE_NOTES.to_vec();
    let other: Vec<&str> = READS_OTHER.iter().map(|(n, _)| *n).collect();
    let internal: Vec<&str> = INTERNAL.iter().map(|(n, _)| *n).collect();
    let writes: Vec<&str> = WRITES.to_vec();
    let classified: Vec<&str> = reads
        .iter()
        .chain(other.iter())
        .chain(internal.iter())
        .chain(writes.iter())
        .copied()
        .collect();
    // every method of Cell is in exactly one bucket
    let unclassified: Vec<&String> = methods
        .iter()
        .filter(|m| !classified.contains(&m.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "pub fn on Cell without a bucket: {unclassified:?}\n\
         a read that speaks notes goes in READS_THE_NOTES (and the contract \
         walks it); a read of something else in READS_OTHER; an internal in \
         INTERNAL; a write needs no bucket"
    );
    // every bucket name still exists (a removed read leaves a dead rule)
    let gone: Vec<&str> = classified
        .into_iter()
        .filter(|n| !methods.iter().any(|m| m == n))
        .collect();
    assert!(
        gone.is_empty(),
        "classified reads that are no longer methods of Cell: {gone:?}"
    );
    // the buckets do not overlap
    let mut all: Vec<&str> = Vec::new();
    for n in reads
        .iter()
        .chain(other.iter())
        .chain(internal.iter())
        .chain(writes.iter())
    {
        assert!(!all.contains(n), "{n} is in two buckets");
        all.push(n);
    }
    let _ = Cell::open_bare; // the associated functions are not reads
}
