// SPDX-License-Identifier: Apache-2.0

//! v0.6.20: the pre-sync log migrates as a SEQUENCE. A cell written by the
//! engine of `fase-v0.4.3` (schema 12: no `base`, no signature) holds a
//! person and an assistant rewriting the same notes one after the other
//! (`examples/make_golden_cell_v043.rs`, the golden `v043.sqlite`). Opened
//! by today's engine, every rewrite used to become a CONFLICT COPY — the
//! derivation read each base-less write as a root, concurrent with the
//! others — and the human-hand rule put the user's OLD body back over the
//! bot's newer one (found by Homun's hook of v0.6.19). The migration says
//! «sequential by declaration»: this test holds it to that.

use engramma_core::{Cell, NotePatch, Status};
use ulid::Ulid;

fn note_id(i: u32) -> Ulid {
    Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), i)).unwrap(/* valid Crockford */)
}

#[test]
fn a_v043_cell_migrates_its_rewrites_as_a_sequence() {
    let dir = tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .unwrap(/* t */);
    let path = dir.path().join("v043.sqlite");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/v043.sqlite"),
        &path,
    )
    .unwrap(/* t */);
    let cell = Cell::open(&path).unwrap(/* today's engine migrates it */);

    let copies = cell.conflicts().unwrap(/* t */);
    assert!(
        copies.is_empty(),
        "sequential rewrites are not conflicts: {copies:?}"
    );
    for (id, body) in [
        (1, "Gaia abita a Milano, da settembre."),
        (2, "Il CFO di Acme resta fino a giugno."),
        (3, "L'ufficio apre alle otto."),
        (4, "Giulia lavora in Acme, nel pricing, da novembre."),
        (5, "Il gatto si chiama Pepe."),
    ] {
        let note = cell.get(&note_id(id)).unwrap(/* t */);
        assert_eq!(note.body, body, "note {id}: the LAST write stands");
        assert_eq!(note.status, Status::Active, "note {id}");
    }
    assert_eq!(
        cell.get(&note_id(1)).unwrap(/* t */).tags,
        vec!["oro/1".to_string(), "casa".to_string()],
        "the header-only patch stands too"
    );
    let report = cell.verify().unwrap(/* t */);
    assert!(report.ok(), "verify: {:?}", report.mismatch);

    // a write of today's engine on the migrated history leans on ONE
    // visible head: no copy is born from it either
    cell.patch_note(
        &note_id(4),
        &NotePatch {
            body: Some("Giulia lavora in Acme, nel pricing, da novembre 2026.".into()),
            ..Default::default()
        },
        "homun:bot-01",
    )
    .unwrap(/* t */);
    assert!(
        cell.conflicts().unwrap(/* t */).is_empty(),
        "a new write over the old sequence"
    );
    assert_eq!(
        cell.get(&note_id(4)).unwrap(/* t */).body,
        "Giulia lavora in Acme, nel pricing, da novembre 2026."
    );
    let report = cell.verify().unwrap(/* t */);
    assert!(
        report.ok(),
        "verify after the new write: {:?}",
        report.mismatch
    );
}
