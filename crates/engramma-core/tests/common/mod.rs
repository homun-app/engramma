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

/// v0.6.12 (L3): a test folder with OUR name. A run killed mid-flight
/// (Ctrl-C on a long hunt: SIGINT kills without unwinding, and the
/// `TempDir` drops never run) leaves behind whatever folders were in
/// flight — the three `.tmp*` of the 21st of November, with `a.sqlite`,
/// `b.sqlite` and `c.sqlite` inside, were exactly that. With a name of
/// our own the NEXT suite run can tell them from every other process's
/// folders and sweep them once they are stale (below). A test that
/// FAILS leaves nothing instead: the panic unwinds, the drops run.
pub fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

/// v0.6.12 (L3): sweep the STALE test folders a killed run left behind.
/// Only `engramma-test-*` folders older than six hours go: ours by
/// name, stale by age — a folder another test of THIS run holds is
/// seconds old, and a folder that is not ours is never touched (the
/// system's `$TMPDIR` is shared with every other process). Best effort
/// by design: a folder that refuses to go waits for the next sweep.
pub fn sweep_stale_test_dirs() -> usize {
    let tmp = std::env::temp_dir();
    let threshold = std::time::Duration::from_secs(6 * 3600);
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(&tmp) else {
        return 0;
    };
    let mut swept = 0;
    for e in entries.flatten() {
        let p = e.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("engramma-test-") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let modified = meta.modified().unwrap_or(now);
        if now.duration_since(modified).unwrap_or_default() <= threshold {
            continue;
        }
        if std::fs::remove_dir_all(&p).is_ok() {
            swept += 1;
        }
    }
    swept
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
    let dir = test_dir();
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

/// v0.6.5 (E4): the fleet's own record of every body write — the body
/// its author READ before writing (the `get` on their device, or the
/// copy's body for a hand merge), and the R4/R1 case counters the
/// REPORT's frequencies count. The judge of «no body text is lost»
/// never reads the `base` chain as its premise: these recorded bodies
/// are what "the author had seen it" means.
#[derive(Default)]
pub struct FleetModel {
    /// op_id → the bodies its author read before writing: the `get` on
    /// their device — and, for a hand merge, the copy's body too (the
    /// author of a merge has TWO texts in front of them). Each entry is
    /// (body, the op whose text that was — the containment's fork)
    pub reads: std::collections::HashMap<String, Vec<(String, Option<String>)>>,
    /// every body write ever written: op_id → (note, text, hash, key)
    pub writes: std::collections::HashMap<String, (Ulid, String, String, (String, String))>,
    /// R4: a header-only patch written right after a body write on the
    /// same device, before any sync (the reviewer's shape)
    pub r4_bursts: u64,
    /// …of those, the body writes that ended up LOSING somewhere (the
    /// shape did its work: a copy opened)
    pub r4_bursts_lost: u64,
    /// the burst body writes, to ask the copies about at the settle
    pub r4_ops: Vec<String>,
    /// R1: an agent's body write on a body a HUMAN hand authored (the
    /// older hand that wins; the follow-up the quarantine used to eat)
    pub r1_agent_on_human: u64,
    /// the body writes not yet carried anywhere (cleared at every sync
    /// round): the retouch candidates
    pub unsynced: Vec<(usize, Ulid)>,
}

impl FleetModel {
    /// Record one body write: what the author read, and the op itself.
    /// `read` is `(body, hash)` — the hash of the op whose body it was.
    pub fn record(
        &mut self,
        dev: usize,
        cell: &Cell,
        note: &Ulid,
        read: Vec<(String, Option<String>)>,
    ) -> Option<String> {
        let op = last_op_of(cell, note)?;
        if !matches!(op.kind.as_str(), "put_note" | "patch_note") {
            return None;
        }
        let text = op
            .body
            .as_ref()
            .and_then(|b| b.get("set"))
            .and_then(|s| s.get("body"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                op.body
                    .as_ref()
                    .and_then(|b| b.get("note"))
                    .and_then(|n| n.get("body"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        let text = text?;
        let key = (op.hlc.to_string(), op.op_id.to_string());
        self.reads.insert(op.op_id.to_string(), read);
        self.writes.insert(
            op.op_id.to_string(),
            (*note, text, op.content_hash.clone(), key),
        );
        self.unsynced.push((dev, *note));
        Some(op.op_id.to_string())
    }
}

/// The newest operation of one note on one device (the write just made).
pub fn last_op_of(cell: &Cell, note: &Ulid) -> Option<Op> {
    cell.ops()
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.note_id.is_some_and(|n| n == *note))
        .max_by(|a, b| a.hlc.cmp(&b.hlc).then(a.op_id.cmp(&b.op_id)))
}
/// The body THIS device shows, with its middle line replaced by the
/// writer's own — the read-modify hand (what an editor does), or the
/// empty body born full.
pub fn middle_replaced(seen: &str, dev: usize, token: &str) -> String {
    let mut lines: Vec<String> = seen.split('\n').map(str::to_string).collect();
    let mid = lines.len() / 2;
    if lines.len() < 3 {
        lines.push(format!("corpo di {dev}: {token}"));
    } else {
        lines[mid] = format!("corpo di {dev}: {token}");
    }
    lines.join("\n")
}

/// The class of an actor, the engine's own word.
use engramma_core::cell::conflicts::ActorClass;
pub fn classify(actor: &str) -> ActorClass {
    engramma_core::cell::conflicts::classify_actor(actor)
}

/// The op whose text the note SHOWS — the winner's word, not merely the
/// newest writer's (a newer write may have LOST the class, and then the
/// screen still shows someone else's body).
pub fn shown_body_hash_of(cell: &Cell, id: &Ulid, shown: &str) -> Option<String> {
    cell.debug_column(&format!(
        "SELECT o.content_hash FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         JOIN op_payloads p ON p.op_id = o.op_id
         WHERE oo.object = 'note:{id}'
           AND (json_extract(p.body, '$.set.body') = '{shown_escaped}'
                OR json_extract(p.body, '$.note.body') = '{shown_escaped}')
           AND ((o.kind = 'patch_note' AND o.note_id = '{id}')
                OR (o.kind IN ('put_note', 'supersede')
                    AND json_extract(p.body, '$.note.id') = '{id}'))
         ORDER BY o.hlc DESC, o.op_id DESC LIMIT 1",
        shown_escaped = shown.replace('\'', "''"),
    ))
    .unwrap_or_default()
    .into_iter()
    .next()
}

/// The read of an ordinary body write: the body the device shows, and
/// the op whose text that is.
pub fn read_of_shown(cell: &Cell, id: &Ulid) -> Vec<(String, Option<String>)> {
    let shown = cell.get(id).map(|n| n.body).unwrap_or_default();
    let hash = if shown.is_empty() {
        None
    } else {
        shown_body_hash_of(cell, id, &shown)
    };
    vec![(shown, hash)]
}

/// Who wrote the op a hash names.
pub fn actor_of_hash(cell: &Cell, hash: &str) -> Option<String> {
    cell.debug_column(&format!(
        "SELECT actor FROM ops WHERE content_hash = '{}'",
        hash.replace('\'', "''")
    ))
    .unwrap_or_default()
    .into_iter()
    .next()
}

// ---------------------------------------------------------------------------

/// The open copies of one device, with the loser op the derivation
/// names (the witness check's key).
fn open_copies_with_loser(cell: &Cell) -> Vec<(Ulid, Ulid, Ulid)> {
    let rows = cell
        .debug_column(
            "SELECT copy_id || ' ' || original_id || ' ' || loser_op FROM conflict_copies
             WHERE open = 1",
        )
        .unwrap(/* test-only */);
    rows.into_iter()
        .filter_map(|row| {
            let mut it = row.split(' ');
            let (Some(c), Some(o), Some(l)) = (it.next(), it.next(), it.next()) else {
                return None;
            };
            Some((
                Ulid::from_string(c).unwrap(/* test-only */),
                Ulid::from_string(o).unwrap(/* test-only */),
                Ulid::from_string(l).unwrap(/* test-only */),
            ))
        })
        .collect()
}

/// The purge dominates with a rule of its own (D1.6 has no say there):
/// a purged original takes its copies without a witness.
fn original_purged(cell: &Cell, original: &Ulid) -> bool {
    cell.debug_column(&format!(
        "SELECT CAST(COUNT(*) AS TEXT) FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         WHERE oo.object = 'note:{original}' AND o.kind = 'purge'"
    ))
    .unwrap_or_default()
    .first()
    .is_some_and(|n| n != "0")
}

/// The lapide closes without a D1.6 witness too — its rule is today's
/// own, kept by this phase (REPORT, «prima della correzione», punto 1):
/// a body write that did not see the delete wins nothing and leaves the
/// tip set; a write the rollback erased is gone; and a tip the lapide
/// held can hand the winnership to a former loser, whose copy closes
/// with it. Wherever the original holds a delete, the conflict set
/// follows the lapide, not the witnesses — the property hunts the
/// derivation on the notes the lapide never touched.
/// The original's derivation row is GONE. Only the purge ever removes
/// the row of a copy that carries operations (every other path keeps it
/// as the note's replay base): its absence says the purge took it — and
/// the purge reaches the copies of copies, transitively.
fn original_row_gone(cell: &Cell, original: &Ulid) -> bool {
    cell.debug_column(&format!(
        "SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE original_id = '{original}'"
    ))
    .unwrap_or_default()
    .first()
    .is_some_and(|n| n == "0")
        && cell
            .debug_column(&format!(
                "SELECT CAST(COUNT(*) AS TEXT) FROM ops o
                 JOIN op_objects oo ON oo.op_id = o.op_id
                 WHERE oo.object = 'note:{original}'"
            ))
            .unwrap_or_default()
            .first()
            .is_some_and(|n| n != "0")
}

fn lapide_active(cell: &Cell, original: &Ulid) -> bool {
    cell.debug_column(&format!(
        "SELECT CAST(COUNT(*) AS TEXT) FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         WHERE oo.object = 'note:{original}' AND o.kind = 'delete_note'"
    ))
    .unwrap_or_default()
    .first()
    .is_some_and(|n| n != "0")
}

// ---------------------------------------------------------------------------
// v0.6.5 (E4): the independent judge — «no body text is lost». For every
// body write a device HOLDS, at every round, at least one of: the note's
// body contains it; an open copy carries its body; an operation lives on
// its copy's id; a later write whose author READ a body containing it
// writes a text containing it; or a declared dominance (purge, lapide)
// covers it. The judge never reads the `base` chain as its premise —
// «seen» is the RECORDED body the author had in front of them, not the
// ancestry. The diff3 forks below locate TEXT (the plumbing of
// containment), never descent-as-witness.
// ---------------------------------------------------------------------------

use engramma_core::cell::conflicts::{conflict_copy_id_for_test, diff3_merge};

/// The judge's own view of a fleet: borrowed cells plus the model.
pub struct FleetView<'a> {
    pub cells: &'a [&'a Cell],
    pub model: &'a FleetModel,
}

/// The fork of the judge's containment, from the op graph, STRICT: the
/// deepest common past that is NEITHER endpoint. When the body descends
/// from the loser the fork is the loser's PARENT — descent alone never
/// counts as «contains» here (the judge's charter: the base chain is
/// not a witness premise; a body that merely REPLACED the loser's one
/// distinctive line does not «contain» it).
pub fn model_fork_strict(
    parent: &std::collections::HashMap<String, Option<String>>,
    a: &str,
    b: &str,
) -> Option<String> {
    let past_a: Vec<String> = {
        let mut out = Vec::new();
        let mut cur = Some(a.to_string());
        while let Some(h) = cur {
            out.push(h.clone());
            cur = parent.get(&h).cloned().flatten();
        }
        out
    };
    // walk b's STRICT chain; skip b's own node and a's node alike
    let mut cur = parent.get(b).cloned().flatten();
    while let Some(h) = cur {
        if h != a && past_a.contains(&h) {
            return Some(h);
        }
        if h == a {
            // a is b's ancestor: the fork is a's parent, deeper
            return parent.get(&h).cloned().flatten();
        }
        cur = parent.get(&h).cloned().flatten();
    }
    // b's chain may end before meeting a's past only if they share no
    // root — but every note's writes share the note's first op, except
    // the double imports (no base at all): then the past of a alone
    // decides nothing, and the fork is none
    None
}

/// The body the fork left: the first body write walking up from it.
fn model_body_at_fork(
    parent: &std::collections::HashMap<String, Option<String>>,
    text_of: &std::collections::HashMap<String, String>,
    fork: &Option<String>,
) -> Option<String> {
    let mut cur = fork.clone();
    while let Some(h) = cur {
        if let Some(t) = text_of.get(&h) {
            return Some(t.clone());
        }
        cur = parent.get(&h).cloned().flatten();
    }
    None
}

/// Does `text` contain `loser` from their fork? The new diff3, the
/// engine's own word (the cap included: too big to judge «contains»).
pub fn model_contains(
    parent: &std::collections::HashMap<String, Option<String>>,
    text_of: &std::collections::HashMap<String, String>,
    loser: &(String, String),
    text: &str,
    of_hash: &str,
) -> bool {
    if of_hash == loser.1 {
        // reading the very body: contained, trivially
        return text == loser.0;
    }
    let Some(fork) = model_fork_strict(parent, &loser.1, of_hash) else {
        return false;
    };
    let Some(fork_body) = model_body_at_fork(parent, text_of, &Some(fork)) else {
        return false;
    };
    diff3_merge(&fork_body, text, &loser.0).is_some_and(|m| m == text)
}

/// One judge pass over one fleet state. `saved` carries the writes a
/// LATER WRITE has already preserved forever (facts about recorded
/// reads and texts never change).
pub fn judge_no_text_lost(
    cells: &[&Cell],
    model: &FleetModel,
    saved: &mut [std::collections::HashSet<String>],
    label: &str,
) -> Result<(), String> {
    let fleet = FleetView { cells, model };
    for (dev, cell) in cells.iter().enumerate() {
        let cell: &Cell = cell;
        let held: std::collections::HashSet<String> = cell
            .debug_column("SELECT op_id FROM ops")
            .unwrap_or_default()
            .into_iter()
            .collect();
        // the device's held body writes, from the MODEL (not the engine)
        #[allow(clippy::type_complexity)]
        let mine: Vec<(&String, &(Ulid, String, String, (String, String)))> = fleet
            .model
            .writes
            .iter()
            .filter(|(id, _)| held.contains(*id))
            .collect();
        let notes: std::collections::HashMap<String, (String, String)> = cell
            .debug_column("SELECT id || char(1) || body || char(1) || status FROM notes")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let (id, rest) = row.split_once('\u{1}')?;
                let (body, status) = rest.split_once('\u{1}')?;
                Some((id.to_string(), (body.to_string(), status.to_string())))
            })
            .collect();
        let pending: std::collections::HashSet<String> = cell
            .debug_column("SELECT object FROM pending_objects")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|o| o.strip_prefix("note:").map(str::to_string))
            .collect();
        let lapide_notes: std::collections::HashSet<String> = cell
            .debug_column(
                "SELECT DISTINCT substr(oo.object, 6) FROM ops o
                 JOIN op_objects oo ON oo.op_id = o.op_id
                 WHERE o.kind = 'delete_note' AND oo.object LIKE 'note:%'",
            )
            .unwrap_or_default()
            .into_iter()
            .collect();
        let open_copies: std::collections::HashSet<String> = cell
            .debug_column("SELECT loser_op FROM conflict_copies WHERE open = 1")
            .unwrap_or_default()
            .into_iter()
            .collect();
        let parent: std::collections::HashMap<String, Option<String>> = cell
            .debug_column(
                "SELECT content_hash || char(1) || COALESCE(base, '') FROM ops WHERE base IS NOT NULL",
            )
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let (h, b) = row.split_once('\u{1}')?;
                Some((h.to_string(), if b.is_empty() { None } else { Some(b.to_string()) }))
            })
            .collect();
        let text_of: std::collections::HashMap<String, String> = fleet
            .model
            .writes
            .values()
            .map(|(_, t, h, _)| (h.clone(), t.clone()))
            .collect();
        for (op_id, (note, text, hash, key)) in &mine {
            let note_s = note.to_string();
            // the declared dominances step aside: the purge (the note is
            // gone here) and the lapide (a note with a tombstone past
            // follows the lapide's own rule, not the witnesses')
            if !notes.contains_key(&note_s) || pending.contains(&note_s) {
                continue;
            }
            if lapide_notes.contains(&note_s) {
                continue;
            }
            if saved[dev].contains(*op_id) {
                continue;
            }
            // (a) the note's body contains it — the containment target
            // is the op whose text the note shows (the winner, usually)
            if let Some((body, _)) = notes.get(&note_s).cloned() {
                let target = fleet
                    .model
                    .writes
                    .iter()
                    .filter(|(id, (n, t, _, _))| {
                        held.contains(id.as_str()) && *n == *note && *t == body
                    })
                    .max_by(|a, b| a.1.3.cmp(&b.1.3));
                if let Some((_, (_, _, y_hash, _))) = target
                    && model_contains(
                        &parent,
                        &text_of,
                        &(text.clone(), hash.clone()),
                        &body,
                        y_hash,
                    )
                {
                    continue;
                }
            }
            // (b) an open copy carries its body
            if open_copies.iter().any(|loser| {
                fleet
                    .model
                    .writes
                    .get(loser)
                    .is_some_and(|(_, t, _, _)| t == text)
            }) {
                continue;
            }
            // (c) an operation lives on its copy's id
            let copy = conflict_copy_id_for_test(hash, &key.0);
            let ops_on_copy: i64 = cell
                .debug_column(&format!(
                    "SELECT CAST(COUNT(*) AS TEXT) FROM op_objects WHERE object = 'note:{copy}'"
                ))
                .unwrap_or_default()
                .first()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if ops_on_copy > 0 {
                continue;
            }
            // (d) a later write whose author READ a body containing it,
            // and whose text contains it — the facts are immutable, so
            // the save is forever
            let mut preserved = false;
            for (x_id, (x_note, _x_text, _x_hash, x_key)) in &fleet.model.writes {
                if x_note != note || !held.contains(x_id) || x_key <= key {
                    continue;
                }
                let Some(reads) = fleet.model.reads.get(x_id) else {
                    continue;
                };
                let mut hit = false;
                for (read_body, read_hash) in reads {
                    let Some(read_hash) = read_hash else { continue };
                    if model_contains(
                        &parent,
                        &text_of,
                        &(text.clone(), hash.clone()),
                        read_body,
                        read_hash,
                    ) {
                        // the text was ON A SCREEN — seen; whatever the
                        // writer then did with it is editing, not loss
                        // (the spec's «e il suo testo contiene L» would
                        // condemn every first body ever rewritten; the
                        // R4 closer still fails here: its read never
                        // held the loser)
                        hit = true;
                        break;
                    }
                }
                if hit {
                    preserved = true;
                    break;
                }
            }
            if preserved {
                saved[dev].insert((*op_id).clone());
                continue;
            }
            return Err(format!(
                "{label}: device {dev} LOST the body of {op_id} ({text:?}) on note {note}"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// v0.6.8 (H4): the REFERENCE WITNESS — the monotonicity judge's own
// oracle, written from DESIGN § 4.3 from scratch. It calls NO conflict
// function of the engine: not `has_witness`, not `Ancestry`, not
// `winner_and_candidates`. It reads the LOG (a public surface) and the
// copy id's deterministic formula (a hash, not a judgement), it is
// naive and quadratic, and its diff3 is `diffy`'s — a different
// realization than the engine's own.
// ---------------------------------------------------------------------------

/// One body write of the note, as the reference sees it.
pub struct RefWrite {
    pub hash: String,
    pub key: (String, String),
    pub text: String,
    pub base: Option<String>,
}

/// The reference witness of one (original, loser op): does the SET
/// contain a closure witness for the copy the loser would open?
///
/// The rule (DESIGN § 4.3, written anew):
/// - (a) an operation on the copy's id — an edit, the resolving delete,
///   a decision on its proposal;
/// - (b) a body write SUBSEQUENT to the loser in the total order that
///   DESCENDS from it (its base chain passes through the loser);
/// - (c) a subsequent body write that CONTAINS it: the rigorous diff3
///   from F = nca(perdente, testimone), both ends included — when the
///   loser is the witness's ancestor, F is the loser itself, and (b)
///   has already answered.
///
/// Every body write the set carries counts — applied, lapide-beaten or
/// rolled back alike (the v0.6.7 rule: the witness is an existence, and
/// it does not depend on the lapide); a payload the compaction cut no
/// longer carries is beyond anyone's judgement, exactly as the engine's
/// own scan says.
///
/// Boundary with the engine's realization, declared: both merge
/// line-wise from the same fork; when the loser's and the witness's
/// changes touch the SAME lines, the engine's diff3 answers «conflict,
/// no containment» and diffy's merge answers Err — the same word. Where
/// two correct line merges could part ways (adjacent-context hunks),
/// the differential judge FAILS and the case is looked at by hand: that
/// is the point of a reference.
pub fn reference_witness(cell: &Cell, original: &Ulid, loser_op: &str) -> Result<bool, String> {
    let ops = cell.ops().map_err(|e| e.to_string())?;
    // the note's body writes and the parent map, over the WHOLE log
    let mut writes: Vec<RefWrite> = Vec::new();
    let mut parent: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut loser: Option<RefWrite> = None;
    for op in &ops {
        if op.note_id.is_some_and(|n| n == *original) {
            parent.insert(op.content_hash.clone(), op.base.clone());
        }
        let text = op
            .body
            .as_ref()
            .and_then(|b| b.get("set"))
            .and_then(|s| s.get("body"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let mine = op
                    .body
                    .as_ref()
                    .and_then(|b| b.get("note"))
                    .and_then(|n| n.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| id == original.to_string().as_str());
                mine.then(|| {
                    op.body
                        .as_ref()
                        .and_then(|b| b.get("note"))
                        .and_then(|n| n.get("body"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .flatten()
            });
        if let Some(text) = text {
            let w = RefWrite {
                hash: op.content_hash.clone(),
                key: (op.hlc.to_string(), op.op_id.to_string()),
                text,
                base: op.base.clone(),
            };
            if op.op_id.to_string() == loser_op {
                loser = Some(RefWrite {
                    hash: w.hash.clone(),
                    key: w.key.clone(),
                    text: w.text.clone(),
                    base: w.base.clone(),
                });
            }
            writes.push(w);
        }
    }
    let Some(loser) = loser else {
        return Ok(false);
    };
    // (a) an operation on the copy's id — the log's own object index
    let copy = conflict_copy_id_for_test(&loser.hash, &loser.key.0);
    let on_copy: i64 = cell
        .debug_column(&format!(
            "SELECT CAST(COUNT(*) AS TEXT) FROM op_objects WHERE object = 'note:{copy}'"
        ))
        .unwrap_or_default()
        .first()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    if on_copy > 0 {
        return Ok(true);
    }
    // (b) and (c): the subsequent body writes, in the total order
    let past = |from: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = Some(from.to_string());
        while let Some(h) = cur {
            out.push(h.clone());
            cur = parent.get(&h).cloned().flatten();
        }
        out
    };
    let loser_past = past(&loser.hash);
    for w in &writes {
        if w.key <= loser.key {
            continue;
        }
        // (b) descent: the witness's chain passes through the loser
        if past(&w.hash).contains(&loser.hash) {
            return Ok(true);
        }
        // (c) containment from the pair's own fork, both ends included
        let fork = {
            let w_chain = past(&w.hash);
            let mut meet = None;
            for h in &w_chain {
                if h == &loser.hash || loser_past.contains(h) {
                    meet = Some(h.clone());
                    break;
                }
            }
            meet
        };
        let Some(fork) = fork else { continue };
        // the fork's body: the first body write at or above it
        let mut cur = Some(fork);
        let mut fork_text: Option<&String> = None;
        while let Some(h) = cur {
            if let Some(fw) = writes.iter().find(|x| x.hash == h) {
                fork_text = Some(&fw.text);
                break;
            }
            cur = parent.get(&h).cloned().flatten();
        }
        let Some(f) = fork_text else { continue };
        if ref_diff3_contains(f, &w.text, &loser.text) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The reference's own rigorous diff3 (v0.6.8, H4), quadratic and
/// naive by design: the LCS against the fork by dynamic programming,
/// the stable points both sides still match, and the unstable chunks in
/// between merge by the RULE of DESIGN § 4.3 — one side unchanged takes
/// the other, identical changes never count as an overlap, everything
/// else is one. `true` when the witness's text CONTAINS the loser's
/// change cleanly (the merge is the witness itself).
///
/// Why not `diffy`'s merge, measured and declared: on the hunt's own
/// first disagreement (seed 11196124333436581517) the loser and the
/// witness rewrote the fork's middle line to the SAME text, and diffy
/// answered «conflict» — identical overlapping changes, which the
/// rigorous diff3 merges. diffy's merge is not a realization of this
/// rule, so the reference carries its own (a different one than the
/// engine's all the same: DP matches against the fork, not the
/// engine's walk). The declared boundary: two longest common
/// subsequences of the same length can cut the stable points
/// differently on ties; the reference's DP takes one canonical walk —
/// a disagreement born there fails the property and is read by hand.
pub fn ref_diff3_contains(fork: &str, witness: &str, loser: &str) -> bool {
    let fb: Vec<&str> = fork.split('\n').collect();
    let fw: Vec<&str> = witness.split('\n').collect();
    let fl: Vec<&str> = loser.split('\n').collect();
    let mw = ref_lcs_matches(&fb, &fw);
    let ml = ref_lcs_matches(&fb, &fl);
    let mut out: Vec<&str> = Vec::new();
    let mut bi = 0_usize;
    let mut wi = 0_usize;
    let mut li = 0_usize;
    let mut iw = 0_usize;
    let mut il = 0_usize;
    loop {
        while iw < mw.len() && mw[iw].0 < bi {
            iw += 1;
        }
        while il < ml.len() && ml[il].0 < bi {
            il += 1;
        }
        // the next fork line BOTH sides still match: the next stable point
        let mut jw = iw;
        let mut jl = il;
        let common = loop {
            match (mw.get(jw), ml.get(jl)) {
                (Some(x), Some(y)) if x.0 == y.0 => break Some((x.0, x.1, y.1)),
                (Some(x), Some(y)) if x.0 < y.0 => jw += 1,
                (Some(_), Some(_)) => jl += 1,
                _ => break None,
            }
        };
        let Some((k, sw, sl)) = common else {
            return ref_merge_chunk(&fb[bi..], &fw[wi..], &fl[li..], &mut out)
                .then_some(out.join("\n"))
                .is_some_and(|m| m == witness);
        };
        if !ref_merge_chunk(&fb[bi..k], &fw[wi..sw], &fl[li..sl], &mut out) {
            return false;
        }
        // the stable line itself — both sides kept it
        out.push(fb[k]);
        bi = k + 1;
        wi = sw + 1;
        li = sl + 1;
    }
}

/// One unstable chunk by the rule: one side unchanged takes the other,
/// identical changes never count as an overlap.
pub fn ref_merge_chunk<'a>(
    base: &[&'a str],
    a: &[&'a str],
    b: &[&'a str],
    out: &mut Vec<&'a str>,
) -> bool {
    if a == b {
        out.extend_from_slice(a);
        true
    } else if a == base {
        out.extend_from_slice(b);
        true
    } else if b == base {
        out.extend_from_slice(a);
        true
    } else {
        false
    }
}

/// The LCS matches of two line lists, by the quadratic DP: `(i, j)`
/// pairs, both strictly increasing. One canonical walk on ties — the
/// declared boundary of the reference.
pub fn ref_lcs_matches(base: &[&str], side: &[&str]) -> Vec<(usize, usize)> {
    let n = base.len();
    let m = side.len();
    // dp[i][j]: the LCS length of base[i..] and side[j..]
    let mut dp = vec![vec![0_usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if base[i] == side[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0_usize, 0_usize);
    while i < n && j < m {
        if base[i] == side[j] {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}
