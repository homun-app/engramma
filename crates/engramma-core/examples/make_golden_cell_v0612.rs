// SPDX-License-Identifier: Apache-2.0

//! v0.6.13 (M1): the GOLDEN CELL of the v0.6.12 base — a cell compacted
//! by the PREVIOUS engine, frozen as an artifact before this phase's
//! schema change, so every later version proves its migration opens a
//! v0.6.12 cell and keeps it green (DESIGN § 8). The story is the one
//! the v0.6.13 anchor work lives on: an agent's correction proposed in
//! supersede mode, accepted by the user, a compaction whose cut sits ON
//! the decision, and the user's CANCEL of that decision above the cut —
//! the queue row and the registry row are born at or below the cut, the
//! materializations that follow run above it.
//!
//! Run it ON THE v0.6.12 BASE with
//!
//! ```sh
//! cargo run -p engramma-core --example make_golden_cell_v0612 -- <outdir>
//! ```
//!
//! The generator refuses to run on any other schema: the artifact is
//! the point, and re-running it after the v23 migration would mint a
//! v0.6.13 cell, not the v0.6.12 one the golden test needs. The checked
//! in files under `tests/golden/` are the artifact.

use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, ProposalInput, ProposalMode, ProposeContext,
};
use engramma_core::format::{At, Note, NoteType};
use engramma_core::hlc::HlcClock;
use engramma_core::{Cell, CellError, Confidence, NotePatch, Status};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use ulid::Ulid;

/// The fixed «now» of the whole build: every HLC is chosen by the
/// script, never by the wall.
const K: u64 = 1_800_400_000_000;

fn now() -> At {
    At(OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */))
}

fn note(id: u32, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), id))
            .unwrap(/* valid Crockford */),
        note_type: NoteType::Fact,
        title: Some(format!("la nota {id}")),
        body: body.to_string(),
        tags: vec![format!("oro/{id}")],
        scope: None,
        lang: None,
        source: "golden:v0.6.12".into(),
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

/// A cell with a hand-set wall and a deterministic mint (the same idea
/// the tests' `cell_with` and the v0.6.8 golden play).
fn golden_cell(name: &str, dir: &Path) -> (Cell, Arc<AtomicU64>) {
    let wall = Arc::new(AtomicU64::new(K));
    let cell = Cell::open_bare(&dir.join(format!("{name}.sqlite"))).unwrap(/* g */);
    let w = Arc::clone(&wall);
    cell.inject_clock(HlcClock::new(format!("{name}000000000x"), move || {
        w.load(Ordering::SeqCst)
    }))
    .unwrap(/* g */);
    let n = Arc::new(Mutex::new(0_u64));
    let tag = name.to_string();
    cell.inject_minter(Arc::new(move || {
        let mut g = n.lock().map_err(|_| CellError::Db("mint".into()))?;
        *g += 1;
        let h = blake3::hash(format!("oro/{tag}/{}", *g).as_bytes());
        let b = h.as_bytes();
        let mut randomness: u128 = 0;
        for x in &b[..10] {
            randomness = (randomness << 8) | u128::from(*x);
        }
        let op_id = Ulid::from_parts(K, randomness);
        let mut salt = [0_u8; 16];
        salt.copy_from_slice(&b[10..26]);
        Ok((op_id, salt))
    }))
    .unwrap(/* g */);
    (cell, wall)
}

fn head(cell: &Cell) -> i64 {
    cell.debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* g */)[0]
        .parse()
        .unwrap(/* g */)
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap(/* g */));
    std::fs::create_dir_all(&out).expect(/* g */ "the outdir");
    // the generator is pinned to the v0.6.12 schema: on any other base
    // it would mint the wrong golden
    {
        let probe = Cell::open_bare(&out.join("probe-schema.sqlite")).unwrap(/* g */);
        let v: i64 = probe
            .debug_column("SELECT value FROM meta WHERE key = 'schema_version'")
            .expect(/* g */ "the schema version is readable")
            .first()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        drop(probe);
        std::fs::remove_file(out.join("probe-schema.sqlite")).ok();
        assert_eq!(
            v, 22,
            "this generator mints the v0.6.12 golden (schema 22); the base is {v}"
        );
    }

    let (cell, wall) = golden_cell("v0612", &out);
    let mut tick = 0_u64;
    let mut advance = |wall: &Arc<AtomicU64>| {
        tick += 1;
        wall.store(K + tick, Ordering::SeqCst);
    };

    // the cell takes proposals (the propose below refuses otherwise)
    advance(&wall);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "user:golden")
        .unwrap(/* g */);

    // A — the note the correction targets; B — a bystander
    advance(&wall);
    cell.put_note(&note(1, "il mio colore preferito è il rosso [[Roma]]"), "user:golden")
        .unwrap(/* g */);
    advance(&wall);
    cell.put_note(&note(2, "una nota qualunque con [[Milano]]"), "user:golden")
        .unwrap(/* g */);

    // the agent's correction, in supersede mode over A
    advance(&wall);
    let mut p = note(3, "il mio colore preferito è il nero [[Roma]]");
    p.status = Status::Quarantined;
    let queued = cell
        .propose(
            &ProposalInput {
                note: p,
                mode: ProposalMode::Supersede(note_id(1)),
                client: Some("golden-agent".into()),
                reason: Some("l'utente l'ha detto".into()),
            },
            "mcp:golden-agent",
            &ProposeContext::default(),
        )
        .unwrap(/* g */);
    let pid = queued.id.expect(/* g */ "queued");

    // the user accepts: P lands active, A closes
    advance(&wall);
    let decision = cell
        .decide(
            &pid,
            &DecisionInput {
                action: DecisionAction::Accept { edited: None },
                reason: Some("giusto".into()),
                review: None,
            },
            "user:golden",
        )
        .unwrap(/* g */);

    // the compaction: the cut sits ON the decision — the queue row, the
    // registry row and the closing are all born at or below it
    let cut = head(&cell);
    cell.compact(
        cut,
        &engramma_core::cell::CompactOptions { keep_snapshot: true },
    )
    .unwrap(/* g */);

    // above the cut: the user CANCELS the decision — the proposal goes
    // back to the queue, the target reopens
    advance(&wall);
    cell.revert_decision(&decision, "ci ho ripensato", "user:golden")
        .unwrap(/* g */);

    // and a plain tail write, so the tail replay has its own work
    advance(&wall);
    cell.patch_note(
        &note_id(2),
        &NotePatch {
            body: Some("una nota qualunque, ritoccata sopra il taglio [[Milano]]".into()),
            ..Default::default()
        },
        "user:golden",
    )
    .unwrap(/* g */);

    // the story must be green on the base that writes it
    let report = cell.verify().unwrap(/* g */);
    assert!(
        report.ok(),
        "the v0.6.12 golden verifies on the v0.6.12 base: {:?}",
        report.mismatch
    );
    println!(
        "v0612 golden written to {} (cut {cut}, head {}, verify ok)",
        out.display(),
        head(&cell)
    );
}

fn note_id(i: u32) -> Ulid {
    Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), i)).unwrap(/* g */)
}
