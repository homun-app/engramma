// SPDX-License-Identifier: Apache-2.0

//! R3 / P5.2 (v0.6.1): the bootstrap measure, time and bytes on the wire.
//! `#[ignore]`d — it is a measurement, not a test: run it by hand with
//!
//! ```text
//! cargo test -p engramma-sync --test bootstrap_bench -- --ignored --nocapture
//! ```
//!
//! The sender side of a session whose peer needs EVERY operation of an
//! `N`-op cell: `ids()` once, one envelope per id. The number of
//! operations is the first argument of the measure (1000, 10000).

mod common;

use engramma_core::format::{Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use engramma_sync::arrival::DeviceSigner;
use engramma_sync::device::DeviceKey;
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

fn bench(n: u32) {
    let dir = common::test_dir();
    std::fs::create_dir_all(dir.path().join("cells")).unwrap(/* t */);
    let key = DeviceKey::load_or_create(dir.path()).unwrap(/* t */);
    let reg = DeviceRegistry::open(dir.path()).unwrap(/* t */);
    reg.create_first(&key, "portatile").unwrap(/* t */);
    let build_t0 = std::time::Instant::now();
    let cell = Cell::create(&dir.path().join("cells").join("memoria.sqlite"), "conv", "cli")
        .unwrap(/* t */);
    for i in 1..=n {
        cell.put_note(&note(i, &format!("corpo della nota {i}")), "cli")
            .unwrap(/* t */);
    }
    let sync_id = share_cell(dir.path(), "memoria").unwrap(/* t */);
    cell.set_signer(std::sync::Arc::new(DeviceSigner::new(&key)))
        .unwrap(/* t */);
    cell.adopt_log("cli").unwrap(/* t */);
    cell.mark_shared().unwrap(/* t */);
    drop(cell);
    let build = build_t0.elapsed();
    let shared = SharedCell::new(
        Cell::open(&dir.path().join("cells").join("memoria.sqlite")).unwrap(/* t */),
        DeviceRegistry::open(dir.path()).unwrap(/* t */),
        sync_id,
        dir.path(),
    );
    let t0 = std::time::Instant::now();
    let ids = shared.local_ids();
    let t_ids = t0.elapsed();
    let reads0 = shared.log_reads();
    let t1 = std::time::Instant::now();
    let mut bytes = 0_usize;
    let mut sealed = 0_usize;
    for (_, id) in &ids {
        if let Some((_, nonce, ct)) = shared.envelope_of(id) {
            sealed += 1;
            bytes += 32 + nonce.len() + ct.len();
        }
    }
    let t_env = t1.elapsed();
    eprintln!(
        "bootstrap n={n}: costruzione {build:?}, ids {t_ids:?}, {sealed} buste in {t_env:?} \
         ({} buste/s), {} byte sul filo, {} letture dell'intero log",
        (sealed as f64 / t_env.as_secs_f64()).floor(),
        bytes,
        shared.log_reads() - reads0,
    );
}

/// The measure of P5.2 (run by hand; the number of ops is editable here).
#[test]
#[ignore = "una misura, non un test: cargo test -- --ignored --nocapture"]
fn bootstrap_ten_thousand() {
    bench(10_000);
}

/// A smaller run for quick comparison.
#[test]
#[ignore = "una misura, non un test"]
fn bootstrap_one_thousand() {
    bench(1_000);
}
