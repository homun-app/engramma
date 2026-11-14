// SPDX-License-Identifier: Apache-2.0

//! R3 (v0.6.1, revisore): no quadratic cost on the wire. The regression
//! test counts the WHOLE-LOG reads of a bootstrap — a session whose peer
//! needs every operation of a 1000-op cell — and demands them LINEAR: a
//! fixed number per envelope at most, never one per operation. The
//! counter is test-only (`SharedCell::log_reads`).

use engramma_core::format::{Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use engramma_sync::arrival::DeviceSigner;
use engramma_sync::device::{DeviceKey, hex};
use engramma_sync::protocol::SyncedCell;
use engramma_sync::registry::DeviceRegistry;
use engramma_sync::sharedcell::{SharedCell, share_cell};
use time::OffsetDateTime;
use ulid::Ulid;

fn now() -> engramma_core::format::At {
    engramma_core::format::At(
        OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */),
    )
}

fn note(id: u32, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), id))
            .unwrap(/* valid Crockford */),
        note_type: NoteType::Fact,
        title: Some(format!("nota {id}")),
        body: body.into(),
        tags: vec![],
        scope: None,
        lang: None,
        source: String::new(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: now(),
        updated_at: now(),
        extensions: vec![],
    }
}

/// The bootstrap of 1000 operations: `ids()` once, then one envelope per
/// id, exactly as a session's sender side does. The whole-log reads must
/// stay a FIXED number — the tree is built once and the lookups are point
/// queries — never one read per envelope.
#[test]
fn the_bootstrap_of_a_thousand_ops_reads_the_log_a_fixed_number_of_times() {
    let dir = tempfile::tempdir().unwrap(/* t */);
    std::fs::create_dir_all(dir.path().join("cells")).unwrap(/* t */);
    let key = DeviceKey::load_or_create(dir.path()).unwrap(/* t */);
    let reg = DeviceRegistry::open(dir.path()).unwrap(/* t */);
    reg.create_first(&key, "portatile").unwrap(/* t */);
    let cell = Cell::create(&dir.path().join("cells").join("memoria.sqlite"), "conv", "cli")
        .unwrap(/* t */);
    for i in 1..=1000_u32 {
        cell.put_note(&note(i, &format!("corpo della nota {i}")), "cli")
            .unwrap(/* t */);
    }
    let sync_id = share_cell(dir.path(), "memoria").unwrap(/* t */);
    // the adopt: signed, as `sync share` does (the signer armed first)
    cell.set_signer(std::sync::Arc::new(DeviceSigner::new(&key)))
        .unwrap(/* t */);
    cell.adopt_log("cli").unwrap(/* t */);
    cell.mark_shared().unwrap(/* t */);
    drop(cell);
    let shared = SharedCell::new(
        Cell::open(&dir.path().join("cells").join("memoria.sqlite")).unwrap(/* t */),
        DeviceRegistry::open(dir.path()).unwrap(/* t */),
        sync_id,
        dir.path(),
    );
    // the sender side of a bootstrap session
    let ids = shared.local_ids();
    assert_eq!(ids.len(), 1002, "init + 1000 puts + the adopt");
    let reads_before = shared.log_reads();
    let mut sealed = 0_usize;
    for (_, id) in &ids {
        if shared.envelope_of(id).is_some() {
            sealed += 1;
        }
    }
    assert_eq!(sealed, ids.len(), "every operation seals");
    let reads = shared.log_reads() - reads_before;
    // a FIXED budget: the whole session may read the whole log a handful
    // of times (the id list, the tree), never once per envelope
    assert!(
        reads <= 8,
        "the bootstrap of {} envelopes did {} whole-log reads: \
         it must be a fixed number, not one per envelope",
        ids.len(),
        reads
    );
}
