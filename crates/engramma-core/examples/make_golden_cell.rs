// SPDX-License-Identifier: Apache-2.0

//! v0.6.8 (H7): the GOLDEN CELL generator — repeatable, synthetic, never
//! real data. Run it with
//!
//! ```sh
//! cargo run -p engramma-core --example make_golden_cell -- <outdir>
//! ```
//!
//! and it writes the golden of `tests/golden/` (the live file and its
//! compaction snapshot, under the names the cell's own rows expect).
//! Every clock and every mint is injected: two runs produce the same
//! logical state (the file bytes move with SQLite's page layout; the
//! FACTS the golden test checks do not). Every phase that changes the
//! schema adds its own golden cell and never touches the old ones
//! (DESIGN § 8).

use engramma_core::cell::ops::{make_op, Op, OpKind, OpParts};
use engramma_core::embed::{Embedder, TextKind};
use engramma_core::error::EmbedError;
use engramma_core::format::{At, Note, NoteType};
use engramma_core::hlc::{Hlc, HlcClock};
use engramma_core::{Cell, CellError, Confidence, NotePatch, ResolveKeep, Status, VaultFile};
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

fn note(id: u32, note_type: NoteType, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), id))
            .unwrap(/* valid Crockford */),
        note_type,
        title: Some(format!("la nota {id}")),
        body: body.to_string(),
        tags: vec![format!("oro/{id}")],
        scope: None,
        lang: None,
        source: "golden:v0.6.8".into(),
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
/// the tests' `cell_with` plays).
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

/// The deterministic fake embedder of the suite: the text hashed into a
/// fixed-dim unit vector. The golden's vectors are DERIVED data — the
/// point is that they are there, not what they say.
struct GoldenEmbedder;

const DIM: usize = 16;

impl Embedder for GoldenEmbedder {
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
        "golden-16"
    }
    fn dim(&self) -> usize {
        DIM
    }
    fn max_tokens(&self) -> usize {
        512
    }
}

/// One shared init, hand-minted (the same shape the tests use).
fn shared_init(tag: &str) -> Op {
    let h = blake3::hash(format!("oro-init/{tag}").as_bytes());
    let b = h.as_bytes();
    let mut randomness: u128 = 0;
    for x in &b[..10] {
        randomness = (randomness << 8) | u128::from(*x);
    }
    let cell_id = Ulid::from_parts(K, randomness);
    let mut salt = [0_u8; 16];
    salt.copy_from_slice(&b[10..26]);
    make_op(OpParts {
        op_id: cell_id,
        hlc: Hlc {
            physical_ms: K - 1,
            counter: 0,
            node: "base00000000x".into(),
        },
        actor: "user".into(),
        kind: OpKind::Init,
        note_id: None,
        base: None,
        body: serde_json::json!({ "cell_id": cell_id.to_string(), "name": "oro" }),
        salt: salt.to_vec(),
        device: None,
        sig: None,
    })
    .unwrap(/* g */)
}

/// The union of two logs, deduplicated by op id, in total order.
fn union(a: &Cell, b: &Cell) -> Vec<Op> {
    let mut out: std::collections::BTreeMap<String, Op> = std::collections::BTreeMap::new();
    for op in a.ops().unwrap(/* g */).iter().chain(b.ops().unwrap(/* g */).iter()) {
        out.entry(op.op_id.to_string()).or_insert_with(|| op.clone());
    }
    let mut v: Vec<Op> = out.into_values().collect();
    v.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    v
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "crates/engramma-core/tests/golden".to_string());
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out).unwrap(/* g */);
    let work = std::env::temp_dir().join(format!("engramma-oro-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap(/* g */);
    let init = shared_init("v068");

    // TWO devices, so the conflict copy is born the true way
    let (a, wa) = golden_cell("a", &work);
    let (b, wb) = golden_cell("b", &work);
    for cell in [&a, &b] {
        cell.ingest_op(&init).unwrap(/* g */);
    }

    // notes of every type, with mentions and a typed relation
    let mut tick = 0_u64;
    let mut next = |wall: &Arc<AtomicU64>| {
        tick += 1;
        wall.store(K + tick, Ordering::SeqCst);
    };
    let mut ids: Vec<Ulid> = Vec::new();
    for (i, note_type, body) in [
        (1_u32, NoteType::Person, "Giulia lavora con [[Fabio]] a [[Milano]]"),
        (2, NoteType::Place, "Milano, la città della [[Bicicletta]]"),
        (3, NoteType::Project, "il progetto oro raccoglie le prove"),
        (
            4,
            NoteType::Event,
            "la verifica del 2026-11-18\n\n## Relations\n- lives_in:: [[Milano]]",
        ),
        (5, NoteType::Preference, "preferisce il treno all'aereo"),
        (6, NoteType::Note, "appunto qualunque"),
    ] {
        next(&wa);
        let n = note(i, note_type, body);
        a.put_note(&n, "homun:user").unwrap(/* g */);
        ids.push(n.id);
    }
    let (person, _place, _project, _event, preference, plain) =
        (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);

    // an external note is BORN in quarantine; a confirm promotes it
    let mut external = note(7, NoteType::Fact, "voci di corridoio su [[Milano]]");
    external.confidence = Confidence::External;
    external.status = Status::Quarantined;
    next(&wa);
    a.put_note(&external, "import:web").unwrap(/* g */);
    let external_id = external.id;

    // a supersede closes the preference with a new note
    let newer = note(8, NoteType::Preference, "adesso preferisce il tram");
    next(&wa);
    a.supersede(&preference, &newer, None, "homun:user").unwrap(/* g */);

    // a lapide: the plain note goes to the bin
    next(&wa);
    a.delete_note(&plain, "homun:user").unwrap(/* g */);

    // the first sync; the confirm of the quarantined note
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* g */);
    }
    next(&wa);
    a.confirm(&external_id, "homun:user").unwrap(/* g */);

    // the OPEN conflict: two concurrent human bodies on one fact
    let fact = note(9, NoteType::Fact, "prima riga\nseconda riga\nterza riga");
    let fact = fact.id;
    next(&wa);
    a.put_note(
        &note(9, NoteType::Fact, "prima riga\nseconda riga\nterza riga"),
        "homun:user",
    )
    .unwrap(/* g */);
    for op in &union(&a, &b) {
        b.ingest_op(op).unwrap(/* g */);
    }
    next(&wa);
    a.patch_note(
        &fact,
        &NotePatch {
            body: Some("prima riga\nocchiello turchino verso [[Namorio]]\nterza".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* g */);
    next(&wb);
    b.patch_note(
        &fact,
        &NotePatch {
            body: Some("prima riga\nmanopola smeraldo\nterza".into()),
            ..Default::default()
        },
        "homun:user",
    )
    .unwrap(/* g */);

    // the CLOSED conflict: two concurrent agent bodies, then a resolve
    let closer = note(10, NoteType::Fact, "chiusa per prova\ncon due righe");
    let closer_id = closer.id;
    next(&wa);
    a.put_note(&closer, "homun:user").unwrap(/* g */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* g */);
        b.ingest_op(op).unwrap(/* g */);
    }
    next(&wa);
    a.patch_note(
        &closer_id,
        &NotePatch {
            body: Some("chiusa per prova\ncon una riga diversa".into()),
            ..Default::default()
        },
        "homun:bot",
    )
    .unwrap(/* g */);
    next(&wb);
    b.patch_note(
        &closer_id,
        &NotePatch {
            body: Some("chiusa per prova\ncon un'altra riga".into()),
            ..Default::default()
        },
        "homun:bot",
    )
    .unwrap(/* g */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* g */);
        b.ingest_op(op).unwrap(/* g */);
    }
    if let Some(first) = b
        .conflicts()
        .unwrap(/* g */)
        .iter()
        .find(|c| c.original_id == closer_id)
        .map(|c| c.copy_id)
    {
        next(&wb);
        b.resolve_conflict(&first, ResolveKeep::Original, None, "homun:user")
            .unwrap(/* g */);
    }
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* g */);
        b.ingest_op(op).unwrap(/* g */);
    }

    // the PURGE: a note and its whole family go
    let purged = note(11, NoteType::Fact, "il segreto da dimenticare");
    let purged_id = purged.id;
    next(&wa);
    a.put_note(&purged, "homun:user").unwrap(/* g */);
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* g */);
        b.ingest_op(op).unwrap(/* g */);
    }
    next(&wa);
    let report = a
        .purge(&purged_id, "diritto all'oblio", "homun:user")
        .unwrap(/* g */);
    assert!(report.scrubbed, "the golden's purge completes: {report:?}");
    for op in &union(&a, &b) {
        a.ingest_op(op).unwrap(/* g */);
        b.ingest_op(op).unwrap(/* g */);
    }

    // the vectors (derived, deterministic) — on B, the golden's device
    b.add_model(GoldenEmbedder.model_id(), DIM).unwrap(/* g */);
    b.embed_pending(&GoldenEmbedder, 64).unwrap(/* g */);

    // a vault view, bound to one file (local state)
    let view_id = Ulid::from_string(&format!("01{}{:08}", "1".repeat(16), 99))
        .unwrap(/* valid Crockford */);
    b.vault_view_put(&view_id.to_string(), "/tmp/oro-vista")
        .unwrap(/* g */);
    b.vault_file_put(
        &view_id.to_string(),
        &VaultFile {
            path: "fact/la-nota-9.md".into(),
            note_id: fact.to_string(),
            file_hash: "oro".into(),
            note_version: 1,
            mtime: 1,
            size: 1,
        },
    )
    .unwrap(/* g */);

    // B is the golden: the compaction with a SNAPSHOT freezes it
    let head: i64 = b
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* g */)[0]
        .parse()
        .unwrap(/* g */);
    let compacted = b
        .compact(
            head,
            &engramma_core::cell::CompactOptions { keep_snapshot: true },
        )
        .unwrap(/* g */);
    let snap = compacted.snapshot.clone().expect(/* asked */ "the snapshot file");
    let snap_name = snap.file_name().unwrap(/* our own */).to_string_lossy().into_owned();

    // close the cells, fold the WAL in, copy the two files
    drop(compacted);
    drop(b);
    drop(a);
    let cell_file = work.join("b.sqlite");
    {
        let conn = rusqlite::Connection::open(&cell_file).unwrap(/* g */);
        conn.pragma_update(None, "journal_mode", "DELETE").ok();
    }
    let golden_cell_path = out.join("v068.sqlite");
    let golden_snap_path = out.join(&snap_name);
    std::fs::remove_file(&golden_cell_path).ok();
    std::fs::remove_file(&golden_snap_path).ok();
    std::fs::copy(&cell_file, &golden_cell_path).unwrap(/* g */);
    std::fs::copy(&snap, &golden_snap_path).unwrap(/* g */);
    // the snapshot row names the file by basename: same name, new home
    {
        let conn = rusqlite::Connection::open(&golden_cell_path).unwrap(/* g */);
        conn.execute("UPDATE snapshots SET file = ?1 WHERE file IS NOT NULL", [&snap_name])
            .unwrap(/* g */);
    }
    let _ = person;
    std::fs::remove_dir_all(&work).ok();
    println!(
        "cella d'oro scritta: {} (+ {snap_name})",
        golden_cell_path.display()
    );
}
