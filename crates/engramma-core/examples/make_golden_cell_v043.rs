// SPDX-License-Identifier: Apache-2.0

//! v0.6.20: the GOLDEN CELL of the pre-sync log. Written by the engine of
//! `fase-v0.4.3` (schema 12, before v0.6: no `base`, no signature), the
//! one Homun shipped with: every operation is sequential by declaration.
//! The story is a person and an assistant REWRITING the same notes one
//! after the other — the shape the migration must never read as
//! concurrent. All synthetic.
//!
//! - note 1: the user writes it, the bot rewrites the body (patch), the
//!   user retags it (header-only patch);
//! - note 2: the bot writes it, the user rewrites it whole (put);
//! - note 3: the user writes it and rewrites the body twice;
//! - note 4: user, bot, user — interleaved rewrites of the body;
//! - note 5: written once, never touched.
//!
//! Run it from a worktree of the tag (the API it uses is the same today,
//! but the artifact is the point: only that engine writes schema 12):
//!
//! ```sh
//! git worktree add target/wt-v043 fase-v0.4.3
//! cd target/wt-v043
//! cargo run -p engramma-core --example make_golden_cell_v043 -- <outdir>
//! ```

use engramma_core::format::At;
use engramma_core::{Cell, Confidence, Note, NotePatch, NoteType, Status};
use std::path::PathBuf;
use time::OffsetDateTime;
use ulid::Ulid;

fn note_id(i: u32) -> Ulid {
    Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), i)).unwrap(/* valid Crockford */)
}

fn note(id: u32, body: &str) -> Note {
    let now = At(OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(/* fixed */));
    Note {
        id: note_id(id),
        note_type: NoteType::Fact,
        title: Some(format!("la nota {id}")),
        body: body.to_string(),
        tags: vec![format!("oro/{id}")],
        scope: None,
        lang: None,
        source: "golden:v0.4.3".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: now,
        updated_at: now,
        extensions: vec![],
    }
}

fn body(text: &str) -> NotePatch {
    NotePatch {
        body: Some(text.to_string()),
        ..Default::default()
    }
}

fn main() {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: make_golden_cell_v043 <outdir>"),
    );
    std::fs::create_dir_all(&out).unwrap(/* the outdir */);
    let path = out.join("v043.sqlite");
    let _ = std::fs::remove_file(&path);
    let cell = Cell::create(&path, "oro", "golden").unwrap(/* a fresh cell */);
    let user = "homun:user";
    let bot = "homun:bot-01";

    cell.put_note(&note(1, "Gaia abita a Torino."), user)
        .unwrap();
    cell.patch_note(
        &note_id(1),
        &body("Gaia abita a Milano, da settembre."),
        bot,
    )
    .unwrap();
    cell.patch_note(
        &note_id(1),
        &NotePatch {
            tags: Some(vec!["oro/1".into(), "casa".into()]),
            ..Default::default()
        },
        user,
    )
    .unwrap();

    cell.put_note(&note(2, "Il CFO di Acme lascia a marzo."), bot)
        .unwrap();
    cell.put_note(&note(2, "Il CFO di Acme resta fino a giugno."), user)
        .unwrap();

    cell.put_note(&note(3, "L'ufficio apre alle nove."), user)
        .unwrap();
    cell.patch_note(
        &note_id(3),
        &body("L'ufficio apre alle otto e mezza."),
        user,
    )
    .unwrap();
    cell.patch_note(&note_id(3), &body("L'ufficio apre alle otto."), user)
        .unwrap();

    cell.put_note(&note(4, "Giulia lavora in Acme."), user)
        .unwrap();
    cell.patch_note(
        &note_id(4),
        &body("Giulia lavora in Acme, nel pricing."),
        bot,
    )
    .unwrap();
    cell.patch_note(
        &note_id(4),
        &body("Giulia lavora in Acme, nel pricing, da novembre."),
        user,
    )
    .unwrap();

    cell.put_note(&note(5, "Il gatto si chiama Pepe."), user)
        .unwrap();

    let report = cell.verify().unwrap(/* the engine of the tag verifies its own cell */);
    assert!(report.ok(), "verify: {:?}", report.mismatch);
    drop(cell);
    println!("{}", path.display());
}
