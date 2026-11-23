// SPDX-License-Identifier: Apache-2.0

//! v0.6.14: the GOLDEN CELL of this phase — the same repeatable, synthetic
//! stage of the v0.6.8 and v0.6.13 ones, plus the shapes this phase fixed,
//! frozen as a v0.6.14 engine wrote them (schema 24):
//!
//! - a note the user closed with a supersede, then RESTORED to a version
//!   of its active life: the content comes back, the closing stays (N2);
//! - an `external` note the user confirmed, put back in quarantine, then
//!   RESTORED to a version of its confirmed life: it stays in quarantine
//!   — the restore is no exit (N2);
//! - an agent's proposal the user accepted, a compaction WITH its copy
//!   file on the decision, then a compaction asked one step BELOW: it
//!   does nothing, the cut does not go back (N1);
//! - above the cut, the user's supersede of the proposal: the queue keeps
//!   the decision.
//!
//! Run it with
//!
//! ```sh
//! cargo run -p engramma-core --example make_golden_cell_v0614 -- <outdir>
//! ```
//! Every clock and every mint is injected: two runs produce the same
//! logical state. Every phase that changes the schema adds its own golden
//! cell and never touches the old ones (DESIGN § 8). The generator refuses
//! to run on any other schema: the artifact is the point.

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
        id: note_id(id),
        note_type: NoteType::Fact,
        title: Some(format!("la nota {id}")),
        body: body.to_string(),
        tags: vec![format!("oro/{id}")],
        scope: None,
        lang: None,
        source: "golden:v0.6.14".into(),
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
    let node = format!("{}000000000x", name.replace('.', ""));
    cell.inject_clock(HlcClock::new(node, move || w.load(Ordering::SeqCst)))
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
    // the generator is pinned to this phase's schema: on any other base it
    // would mint the wrong golden
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
            v, 24,
            "this generator mints the v0.6.14 golden (schema 24); the base is {v}"
        );
    }

    let (cell, wall) = golden_cell("v0614", &out);
    let mut tick = 0_u64;
    let mut advance = |wall: &Arc<AtomicU64>| {
        tick += 1;
        wall.store(K + tick, Ordering::SeqCst);
    };

    // the cell takes proposals (the propose below refuses otherwise)
    advance(&wall);
    cell.set_policy(serde_json::json!({ "write": "propose" }), "user:golden")
        .unwrap(/* g */);

    // 1 — a bystander
    advance(&wall);
    cell.put_note(&note(1, "una nota qualunque con [[Milano]]"), "user:golden")
        .unwrap(/* g */);

    // 2 — the note the user will close and RESTORE: three versions, then
    // the supersede that closes it
    advance(&wall);
    cell.put_note(&note(2, "la prima mano [[Roma]]"), "user:golden")
        .unwrap(/* g */);
    advance(&wall);
    cell.patch_note(
        &note_id(2),
        &NotePatch {
            body: Some("la seconda mano [[Roma]]".into()),
            ..Default::default()
        },
        "user:golden",
    )
    .unwrap(/* g */);
    advance(&wall);
    let mut closing = note(3, "la nota che chiude [[Roma]]");
    closing.supersedes = vec![note_id(2)];
    cell.supersede(&note_id(2), &closing, None, "user:golden")
        .unwrap(/* g */);
    // the restore of version 1 (the active life): the content comes
    // back, the closing stays
    advance(&wall);
    cell.restore(&note_id(2), 1, "user:golden").unwrap(/* g */);
    let closed = cell.get(&note_id(2)).unwrap(/* g */);
    assert_eq!(
        closed.status,
        Status::Superseded,
        "a restore does not reopen"
    );
    assert_eq!(
        closed.body, "la prima mano [[Roma]]",
        "the content of version 1"
    );

    // 4 — an `external` note: confirmed by the user, put back in
    // quarantine, edited, then RESTORED to a version of its confirmed
    // life: it stays in quarantine
    advance(&wall);
    let mut ext = note(4, "letto sul web [[Milano]]");
    ext.confidence = Confidence::External;
    ext.status = Status::Quarantined;
    ext.source = "import:web".into();
    cell.put_note(&ext, "user:golden").unwrap(/* g */);
    advance(&wall);
    cell.confirm(&note_id(4), "user:golden").unwrap(/* g */);
    advance(&wall);
    cell.set_status(&note_id(4), Status::Quarantined, "user:golden")
        .unwrap(/* g */);
    advance(&wall);
    cell.patch_note(
        &note_id(4),
        &NotePatch {
            body: Some("ritoccata in quarantena [[Milano]]".into()),
            ..Default::default()
        },
        "user:golden",
    )
    .unwrap(/* g */);
    advance(&wall);
    cell.restore(&note_id(4), 2, "user:golden").unwrap(/* g */);
    let quarantined = cell.get(&note_id(4)).unwrap(/* g */);
    assert_eq!(
        quarantined.status,
        Status::Quarantined,
        "the restore is no exit from the quarantine"
    );
    assert_eq!(
        quarantined.body, "letto sul web [[Milano]]",
        "the content of version 2"
    );

    // 5 — the agent's NEW proposal; the user accepts it
    advance(&wall);
    let mut p = note(5, "il mio colore preferito è il nero [[Roma]]");
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

    // the compaction WITH its copy file, the cut ON the decision — then
    // the compaction asked one step BELOW: it does nothing, the cut
    // stays where the first one put it
    let cut = head(&cell);
    let first = cell
        .compact(
            cut,
            &engramma_core::cell::CompactOptions { keep_snapshot: true },
        )
        .unwrap(/* g */);
    assert!(first.compacted, "the first compaction lands");
    let second = cell
        .compact(
            cut - 1,
            &engramma_core::cell::CompactOptions { keep_snapshot: true },
        )
        .unwrap(/* g */);
    assert!(
        !second.compacted && second.cut_seq == cut,
        "the cut only goes forward: {:?}",
        (second.compacted, second.cut_seq)
    );

    // above the cut: the user closes the accepted proposal with a
    // supersede, and retouches the new note — the reconstruction over the
    // cut keeps the user's decision
    advance(&wall);
    let mut n = note(6, "il mio colore preferito è il blu [[Roma]]");
    n.supersedes = vec![pid];
    cell.supersede(&pid, &n, None, "user:golden").unwrap(/* g */);
    advance(&wall);
    cell.patch_note(
        &note_id(6),
        &NotePatch {
            title: Some(Some("il blu, ritoccato sopra il taglio".into())),
            ..Default::default()
        },
        "user:golden",
    )
    .unwrap(/* g */);

    // the story must be green on the engine that writes it
    let report = cell.verify().unwrap(/* g */);
    assert!(
        report.ok(),
        "the v0.6.14 golden verifies on the v0.6.14 engine: {:?}",
        report.mismatch
    );
    assert!(report.declared_corners.is_empty());
    let queue = cell
        .debug_column(&format!(
            "SELECT mode || ' ' || state FROM proposals WHERE note_id = '{pid}'"
        ))
        .unwrap(/* g */);
    println!(
        "v0614 golden written to {} (cut {cut}, head {}, queue {queue:?}, verify ok)",
        out.display(),
        head(&cell)
    );
}
