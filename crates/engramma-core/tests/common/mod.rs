// SPDX-License-Identifier: Apache-2.0

//! The shared stage of the v0.6.4 conflict tests: fixed clocks,
//! deterministic mints, a shared init, and the open-copies reader.

#![allow(dead_code)]

use engramma_core::cell::ops::Op;
use engramma_core::format::{At, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use ulid::Ulid;

/// A fixed «now» far from the real clock, so every HLC in the case is
/// chosen by the test, never by the wall.
pub const K: u64 = 1_800_400_000_000;

pub fn now() -> At {
    At(OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */))
}

pub fn base_note(id: u32, body: &str) -> Note {

    Note {
        id: Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), id))
            .unwrap(/* test-only: valid Crockford */),
        note_type: NoteType::Fact,
        title: Some("il titolo di partenza".into()),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: "test".into(),
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

/// A copy and the folder that holds its file (the folder outlives the
/// cell: fields drop in declaration order).
pub struct Kept {
    cell: Cell,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for Kept {
    type Target = Cell;
    fn deref(&self) -> &Cell {
        &self.cell
    }
}

/// A cell with a hand-set wall clock (the test moves it between writes)
/// and a deterministic mint, so every HLC and every id is chosen by the
/// case, not by the machine.
pub fn cell_with(name: &str) -> (Kept, Arc<AtomicU64>) {
    let wall = Arc::new(AtomicU64::new(K));
    let dir = tempfile::tempdir().unwrap(/* test-only */);
    let cell = Cell::open_bare(&dir.path().join(format!("{name}.sqlite"))).unwrap(/* t */);
    let w = Arc::clone(&wall);
    cell.inject_clock(engramma_core::hlc::HlcClock::new(
        format!("{name}000000000x"),
        move || w.load(Ordering::SeqCst),
    ))
    .unwrap(/* t */);
    let n = Arc::new(Mutex::new(0_u64));
    let tag = name.to_string();
    cell.inject_minter(Arc::new(move || {
        let mut g = n.lock().map_err(|_| engramma_core::CellError::Db("mint".into()))?;
        *g += 1;
        let h = blake3::hash(format!("mint/{tag}/{}", *g).as_bytes());
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
    .unwrap(/* t */);
    (Kept { cell, _dir: dir }, wall)
}

/// One shared init, hand-minted (a `Cell::create` would mint from the
/// process CSPRNG and the case would not reproduce).
pub fn shared_init(tag: &str) -> Op {
    let h = blake3::hash(format!("init/{tag}").as_bytes());
    let b = h.as_bytes();
    let mut randomness: u128 = 0;
    for x in &b[..10] {
        randomness = (randomness << 8) | u128::from(*x);
    }
    let cell_id = Ulid::from_parts(K, randomness);
    let mut salt = [0_u8; 16];
    salt.copy_from_slice(&b[10..26]);
    engramma_core::cell::ops::make_op(engramma_core::cell::ops::OpParts {
        op_id: cell_id,
        hlc: engramma_core::hlc::Hlc {
            physical_ms: K - 1,
            counter: 0,
            node: "base00000000x".into(),
        },
        actor: "user".into(),
        kind: engramma_core::cell::ops::OpKind::Init,
        note_id: None,
        base: None,
        body: serde_json::json!({ "cell_id": cell_id.to_string(), "name": "conv" }),
        salt: salt.to_vec(),
        device: None,
        sig: None,
    })
    .unwrap(/* t */)
}

/// The union of two logs, deduplicated by op id (first occurrence wins).
pub fn union(a: &Cell, b: &Cell) -> Vec<Op> {
    let mut out: BTreeMap<String, Op> = BTreeMap::new();
    for op in a.ops().unwrap(/* t */).iter().chain(b.ops().unwrap(/* t */).iter()) {
        out.entry(op.op_id.to_string())
            .or_insert_with(|| op.clone());
    }
    let mut v: Vec<Op> = out.into_values().collect();
    v.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
    v
}

/// Every OPEN conflict copy of `original` as a notes row (the user's
/// eye: what the cell shows, tagged `conflitto`), as
/// `(id, body, status)`.
pub fn open_copies(cell: &Cell, original: &Ulid) -> Vec<(String, String, String)> {
    cell.debug_column(&format!(
        "SELECT n.id || '' || n.body || '' || n.status FROM notes n
         JOIN note_tags t ON t.note_id = n.id
         WHERE t.tag = 'conflitto' AND n.id != '{}' AND n.status != 'deleted'
         ORDER BY n.id",
        original
    ))
    .unwrap(/* t */)
    .into_iter()
    .map(|row| {
        let (id, rest) = row.split_once('').unwrap(/* t */);
        let (body, status) = rest.split_once('').unwrap(/* t */);
        (id.to_string(), body.to_string(), status.to_string())
    })
    .collect()
}

