// SPDX-License-Identifier: Apache-2.0

//! K4 (v0.6.1-ciclo): where does a `put_note`'s extra time go? The same
//! corpus, twice into a fresh cell — once unsigned, once with the device
//! signer armed (Ed25519 over the content hash, exactly as the sync era
//! writes) — then a sustained signed phase for the sampler:
//!
//! ```text
//! samply record -- cargo run --release -p engramma-bench \
//!   --example profile_put -- 10000
//! ```

use std::time::Instant;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use time::OffsetDateTime;

fn note(i: u64, t: time::OffsetDateTime) -> Note {
    Note {
        id: ulid::Ulid::from_parts(1_800_000_000_000, u128::from(i) + 1),
        note_type: NoteType::Fact,
        title: Some(format!("nota {i}")),
        body: format!(
            "Persona{} lavora a Citta{} sul progetto {} con [[Persona{}]].",
            i % 997,
            i % 101,
            i % 313,
            (i * 7) % 997
        ),
        tags: vec![format!("arg{}", i % 37), format!("anno{}", i % 12)],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: At(t),
        updated_at: At(t),
        extensions: vec![],
    }
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(10_000);
    let dir = tempfile::tempdir().expect(/* t */ "tempdir");
    let t = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect(/* valid */ "ts");
    let corpus: Vec<Note> = (0..n).map(|i| note(i, t)).collect();

    // the SAME corpus into a fresh cell, twice: without and with the signer
    let mut rates: Vec<(&str, f64)> = Vec::new();
    for (label, signed) in [("senza firma", false), ("con firma", true)] {
        let db = dir
            .path()
            .join(format!("prof-{}.sqlite", label.replace(' ', "-")));
        let cell = Cell::create(&db, "prof", "bench").expect(/* fresh */ "create");
        if signed {
            let root = tempfile::tempdir().expect(/* t */ "tempdir");
            let key = engramma_sync::device::DeviceKey::load_or_create(root.path())
                .expect(/* t */ "key");
            cell.set_signer(std::sync::Arc::new(
                engramma_sync::arrival::DeviceSigner::new(&key),
            ))
            .expect(/* t */ "arm");
        }
        let t0 = Instant::now();
        for n in &corpus {
            cell.put_note(n, "bench").expect(/* valid */ "put");
        }
        let secs = t0.elapsed().as_secs_f64();
        rates.push((label, corpus.len() as f64 / secs));
        eprintln!(
            "insert {label}: {} note in {:.2}s → {:.0} note/s",
            corpus.len(),
            secs,
            corpus.len() as f64 / secs
        );
        if signed {
            // the sustained signed phase: the sampler's window (re-put of
            // the whole corpus, round-robin)
            let deadline = Instant::now() + std::time::Duration::from_secs(20);
            let mut done: usize = 0;
            let mut i: usize = 0;
            while Instant::now() < deadline {
                let n = &corpus[i % corpus.len()];
                cell.put_note(n, "bench").expect(/* valid */ "put");
                done += 1;
                i += 1;
            }
            let secs = 20.0_f64;
            rates.push(("sustain con firma", done as f64 / secs));
            eprintln!(
                "sustain con firma: {done} put_note in 20s → {:.0}/s",
                done as f64 / secs
            );
        }
        drop(cell);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(db.with_extension(format!("sqlite{suffix}")));
        }
    }
    let unsigned = rates[0].1;
    let signed = rates[1].1;
    let per_note_unsigned = 1.0e6 / unsigned;
    let per_note_signed = 1.0e6 / signed;
    eprintln!(
        "\nper nota: {:.0} µs senza firma, {:.0} µs con firma → +{:.0} µs ({:.0}%)",
        per_note_unsigned,
        per_note_signed,
        per_note_signed - per_note_unsigned,
        100.0 * (per_note_signed - per_note_unsigned) / per_note_unsigned,
    );
}
