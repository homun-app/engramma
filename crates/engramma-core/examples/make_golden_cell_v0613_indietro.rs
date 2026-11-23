// SPDX-License-Identifier: Apache-2.0

//! v0.6.14 (N1): the GOLDEN CELL of the v0.6.13 base — a cell the
//! PREVIOUS engine compacted with a cut BELOW the one already made, frozen
//! as an artifact before this phase's repair, so every later version
//! proves its migration opens it and mends it (DESIGN § 8). The story is
//! the v0.6.13 verification's minimal probe (seed
//! `11558843446952054066`): an agent's `new` proposal, the user's accept,
//! a compaction WITH its copy file at the head, a second compaction one
//! step BELOW the first — the v0.6.13 engine accepts it — and the user's
//! supersede of the proposal above the cut. The `decide` that accepted has
//! its payload already gone and sits above the registered cut: the
//! reconstruction replays it as nothing, the queue says `pending`, the
//! registry is empty, and the engine's `verify` is green on all of it.
//!
//! Run it ON THE v0.6.13 BASE with
//!
//! ```sh
//! cargo run -p engramma-core --example make_golden_cell_v0613_indietro -- <outdir>
//! ```
//!
//! The generator refuses to run on any other schema: the artifact is the
//! point, and re-running it after the v24 migration would refuse the
//! backward cut (the rule of this phase), not mint the damaged cell the
//! golden test needs. The checked in files under `tests/golden/` are the
//! artifact.

use engramma_core::cell::proposals::{
    DecisionAction, DecisionInput, ProposalInput, ProposalMode, ProposeContext,
};
use engramma_core::format::{At, Note, NoteType};
use engramma_core::hlc::HlcClock;
use engramma_core::{Cell, CellError, Confidence, Status};
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
        id: note_id(id),
        note_type: NoteType::Fact,
        title: Some(format!("la nota {id}")),
        body: body.to_string(),
        tags: vec![format!("oro/{id}")],
        scope: None,
        lang: None,
        source: "golden:v0.6.13-indietro".into(),
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

fn note_id(i: u32) -> Ulid {
    Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), i)).unwrap(/* g */)
}

/// A cell with a hand-set wall and a deterministic mint (the same idea
/// the tests' `cell_with` and the v0.6.8 golden play).
fn golden_cell(name: &str, dir: &Path) -> (Cell, Arc<AtomicU64>) {
    let wall = Arc::new(AtomicU64::new(K));
    let cell = Cell::open_bare(&dir.join(format!("{name}.sqlite"))).unwrap(/* g */);
    let w = Arc::clone(&wall);
    // the node id is alphanumeric: the hyphen of the cell's name goes
    let node = format!("{}000000000x", name.replace('-', ""));
    cell.inject_clock(HlcClock::new(node, move || {
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
    cell.debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops").unwrap(/* g */)[0]
        .parse()
        .unwrap(/* g */)
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap(/* g */));
    std::fs::create_dir_all(&out).expect(/* g */ "the outdir");
    // the generator is pinned to the v0.6.13 schema: on any other base
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
        let _ = std::fs::remove_file(out.join("probe-schema.sqlite"));
        assert_eq!(
            v, 23,
            "this generator mints the v0.6.13 golden (schema 23); the base is {v}"
        );
    }

    let (cell, wall) = golden_cell("v0613-indietro", &out);
    let mut tick = 0_u64;
    let mut advance = |wall: &Arc<AtomicU64>| {
        tick += 1;
        wall.store(K + tick, Ordering::SeqCst);
    };

    // the cell takes proposals (the propose below refuses otherwise)
    advance(&wall);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "user:golden")
        .unwrap(/* g */);

    // a bystander, so the cut has ordinary history below it
    advance(&wall);
    cell.put_note(&note(1, "una nota qualunque con [[Milano]]"), "user:golden")
        .unwrap(/* g */);

    // the agent's NEW proposal; the user accepts it
    advance(&wall);
    let mut p = note(3, "il mio colore preferito è il nero [[Roma]]");
    p.status = Status::Quarantined;
    let queued = cell
        .propose(
            &ProposalInput {
                note: p,
                mode: ProposalMode::New,
                client: Some("golden-agent".into()),
                reason: Some("l'utente l'ha detto".into()),
            },
            "mcp:golden-agent",
            &ProposeContext::default(),
        )
        .unwrap(/* g */);
    let pid = queued.id.expect(/* g */ "queued");
    advance(&wall);
    cell.decide(
        &pid,
        &DecisionInput {
            action: DecisionAction::Accept { edited: None },
            reason: Some("giusto".into()),
            review: None,
        },
        "user:golden",
    )
    .unwrap(/* g */);

    // the compaction at the head, with its copy file — then the SECOND
    // compaction one step BELOW the first. The v0.6.13 engine accepts
    // it: the registered cut goes back under payloads already gone.
    let cut = head(&cell);
    cell.compact(
        cut,
        &engramma_core::cell::CompactOptions { keep_snapshot: true },
    )
    .unwrap(/* g */);
    cell.compact(
        cut - 1,
        &engramma_core::cell::CompactOptions { keep_snapshot: true },
    )
    .unwrap(/* g */);

    // above the cut: the user closes the accepted proposal with a
    // supersede — the first reconstruction over the wrong cut
    advance(&wall);
    let mut n = note(5, "il mio colore preferito è il blu [[Roma]]");
    n.supersedes = vec![pid];
    cell.supersede(&pid, &n, None, "user:golden").unwrap(/* g */);

    // the damage, as the base leaves it — and the base's blind green
    let registered: String = cell
        .debug_column("SELECT CAST(seq AS TEXT) FROM snapshots")
        .unwrap(/* g */)[0]
        .clone();
    let queue = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* g */);
    let registry = cell
        .debug_column(&format!(
            "SELECT CAST(COUNT(*) AS TEXT) FROM decisions WHERE proposal = '{pid}'"
        ))
        .unwrap(/* g */)[0]
        .clone();
    let report = cell.verify().unwrap(/* g */);
    assert!(
        report.ok(),
        "the v0.6.13 base calls this cell green (it is blind here): {:?}",
        report.mismatch
    );
    println!(
        "v0613-indietro golden written to {} (cut {cut}, registered {registered}, head {}, \
         queue {queue:?}, registry rows {registry}, verify ok on the base)",
        out.display(),
        head(&cell)
    );
}
