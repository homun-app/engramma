// SPDX-License-Identifier: Apache-2.0

//! v0.6.4 (D1/D2): the body-conflict rule, derived from the SET of a
//! note's operations — never from the order they arrived in.
//!
//! - A **body write** is a `put` of the note or a `patch` whose `set`
//!   carries `body`. Header-only patches never create conflicts; the
//!   header stays last-write-per-field in the total order, as ever.
//! - **Ancestry** walks the `base` chain through EVERY operation of the
//!   note, header writes included. Two body writes are CONCURRENT when
//!   neither is an ancestor of the other; a write without `base` is a
//!   root (the same import on two machines never saw the note).
//! - The **winner** among the concurrent tips is the key
//!   `(class of the author, hlc, op_id)` — the human hand beats the
//!   agent even when older.
//! - A **loser** is an applied body write that is neither the winner
//!   nor one of its ancestors. Its copy is DURABLE: it closes only with
//!   a WITNESS in the set — an operation on the copy's id, a body write
//!   that descends from the loser (its author had seen it), or a later
//!   body write that CONTAINS it (a rigorous diff3 from the fork
//!   returns the winner without overlaps).
//! - The **cap**: at most three open copies per note are VISIBLE (the
//!   most recent in the total order); the others stay open and appear
//!   when a visible one closes.
//!
//! The module is pure derivation: every copy of the cell walks the same
//! graph over the same set and lands on the same rows, whatever order
//! the operations arrived in. The arrival fast path and the local write
//! path pay the same derivation the full replay pays (`after_replay`),
//! through `after_body_write` — the v0.6.3 shortcut that emptied the
//! loser set is gone.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Transaction};
use ulid::Ulid;

use crate::error::CellError;
use crate::format::Note;

use super::material;
use super::ops::{self, Op, OpKind};

/// How many open copies of one note are VISIBLE at most (D1.7).
pub const VISIBLE_COPIES_CAP: usize = 3;

// ---------------------------------------------------------------------------
// the class of the author (D2)
// ---------------------------------------------------------------------------

/// Who wrote an operation: a human hand or an agent. On two concurrent
/// tips the human wins even when older; a prudent default — whoever is
/// not recognized as human is an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActorClass {
    /// An agent: bots, stories, MCP labels, reviewers, anyone unknown.
    Agent,
    /// A human hand: `user`, every actor ending in `:user`
    /// (`homun:user`), every actor starting with `user@`
    /// (`user@mcp:<label>`).
    Human,
}

/// The class of an actor. Declared, not signed — enough for the
/// personal sync, where every writer lives on the owner's devices
/// (organizations will bind the class to a signature, v0.7).
#[must_use]
pub fn classify_actor(actor: &str) -> ActorClass {
    if actor == "user" || actor.ends_with(":user") || actor.starts_with("user@") {
        ActorClass::Human
    } else {
        ActorClass::Agent
    }
}

// ---------------------------------------------------------------------------
// the rigorous diff3 (a line merge with no approximations)
// ---------------------------------------------------------------------------

/// The alignment cap (v0.6.5, E3): beyond this many lines, or this many
/// bytes, on either side the containment answers «does not contain».
/// The prudent choice — a copy too big to judge stays open, and text is
/// never lost. Under the cap Myers' linear alignment keeps a 20 000-line
/// merge in the milliseconds (the numbers are in the REPORT); the cap
/// keeps a hostile import bounded whatever its shape.
pub const DIFF3_MAX_LINES: usize = 32_768;
/// The byte twin of [`DIFF3_MAX_LINES`] — long lines pay in bytes what
/// many lines pay in rows. Measured on the R3 shape: 20 000 lines cost
/// 1,4 ms and 5,6 MB (v0.6.4: 1,4 s and 1,68 GB); the worst honest
/// shape — two unrelated sides — stays linear in memory and bounded in
/// time by the cap (≈2,6 s at 32 768). The numbers are in the REPORT.
pub const DIFF3_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The aligned pairs of an LCS of two line slices, increasing on both
/// sides — the same pairs the v0.6.4 quadratic table walked (the
/// equality property of `tests/diff3_linear.rs` freezes that contract),
/// found in LINEAR SPACE with Myers' middle-snake divide and conquer:
/// O((n+m)·D) time, O(n+m) memory (v0.6.5, E3 — the reviewer's R3: the
/// quadratic table cost 1,4 s and 1,68 GB for ONE 20 000-line merge).
fn lcs_matches(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    myers_box(a, b, 0, 0, &mut out);
    out
}

/// (v0.6.5, tests only) the deterministic id of one loser's conflict
/// copy — the E4 judge asks whether an operation lives on it.
#[must_use]
pub fn conflict_copy_id_for_test(loser_hash: &str, loser_hlc: &str) -> Ulid {
    material::conflict_copy_id(
        loser_hash,
        crate::hlc::Hlc::parse(loser_hlc)
            .map(|h| h.physical_ms)
            .unwrap_or(0),
    )
}

/// (v0.6.5, tests only) the alignment itself — the long-text property
/// checks it is a longest common subsequence, valid and maximal, above
/// the walk's base where Myers rules.
#[must_use]
pub fn lcs_matches_for_test(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    lcs_matches(a, b)
}

/// The base-case side (v0.6.5, E3): a box this small takes the walk of
/// the v0.6.4 table itself — BOUNDED now (512²·4 B ≈ 1 MB, a leaf of
/// the recursion, never the unbounded whole of the reviewer's R3). The
/// ties of Myers' canonical path and of the table's walk are two
/// different optimums among equals, and the merge DECIDES with them
/// (which lines are stable); short texts keep the table's own decisions
/// — the equality property's contract — while the big ones, where no
/// contract binds, take Myers' O((n+m)·D) under the cap.
const MYERS_BASE: usize = 512;

/// One box of the divide and conquer: the common head matches greedily
/// (the walk's own first move — equal fronts always match), then the
/// middle snake splits the rest and the halves recurse.
fn myers_box(a: &[&str], b: &[&str], a0: usize, b0: usize, out: &mut Vec<(usize, usize)>) {
    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        out.push((a0 + head, b0 + head));
        head += 1;
    }
    let a = &a[head..];
    let b = &b[head..];
    let (a0, b0) = (a0 + head, b0 + head);
    if a.is_empty() || b.is_empty() {
        return;
    }
    if a.len() <= MYERS_BASE && b.len() <= MYERS_BASE {
        walk_box(a, b, a0, b0, out);
        return;
    }
    if let Some((sx, sy, ex, ey)) = middle_snake(a, b) {
        myers_box(&a[..sx], &b[..sy], a0, b0, out);
        for t in 0..ex.saturating_sub(sx) {
            out.push((a0 + sx + t, b0 + sy + t));
        }
        myers_box(&a[ex..], &b[ey..], a0 + ex, b0 + ey, out);
    }
    // else: the box shares no line at all — pure edits, nothing to align
}

/// The walk of the v0.6.4 table over one bounded box: match when the
/// fronts are equal, else advance the first side whenever both lead to
/// a maximum — the decisions every short text has always had.
fn walk_box(a: &[&str], b: &[&str], a0: usize, b0: usize, out: &mut Vec<(usize, usize)>) {
    let mut table = vec![vec![0_u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push((a0 + i, b0 + j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
}

/// The middle snake (Myers 1986, §4b): the diagonal run an optimal path
/// crosses at the center of the box, as its forward coordinates
/// `(start_x, start_y, end_x, end_y)`. `None` when no split makes
/// progress (the box shares no line). The ties follow the table's own
/// rule — on equal reach the path consumes the FIRST side, and equal
/// fronts always match — which is what keeps the alignment equal to the
/// quadratic walk's on the texts the property generates.
fn middle_snake(a: &[&str], b: &[&str]) -> Option<(usize, usize, usize, usize)> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let delta = n - m;
    let odd = delta.rem_euclid(2) == 1;
    let max_d = (n + m) / 2 + 1;
    let off = max_d;
    let mut vf = vec![0_isize; (2 * max_d + 1) as usize];
    let mut vr = vec![0_isize; (2 * max_d + 1) as usize];
    for d in 0..=max_d {
        // the forward D-paths; k walks DOWN so vf[k+1] is this round's
        let mut k = d;
        while k >= -d {
            let mut x =
                if k == -d || (k != d && vf[(off + k - 1) as usize] < vf[(off + k + 1) as usize]) {
                    vf[(off + k + 1) as usize]
                } else {
                    vf[(off + k - 1) as usize] + 1
                };
            let mut y = x - k;
            let (xs, ys) = (x, y);
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            vf[(off + k) as usize] = x;
            if odd {
                let rk = delta - k;
                if -(d - 1) <= rk
                    && rk < d
                    && x + vr[(off + rk) as usize] >= n
                    && (xs != x || (xs != 0 && ys != 0) || (xs != n && ys != m))
                {
                    return Some((xs as usize, ys as usize, x as usize, y as usize));
                }
            }
            k -= 2;
        }
        // the reverse D-paths, measured from the far corner
        let mut k = d;
        while k >= -d {
            let mut x =
                if k == -d || (k != d && vr[(off + k - 1) as usize] < vr[(off + k + 1) as usize]) {
                    vr[(off + k + 1) as usize]
                } else {
                    vr[(off + k - 1) as usize] + 1
                };
            let mut y = x - k;
            let (xs, ys) = (x, y);
            while x < n && y < m && a[(n - 1 - x) as usize] == b[(m - 1 - y) as usize] {
                x += 1;
                y += 1;
            }
            vr[(off + k) as usize] = x;
            if !odd {
                let fk = delta - k;
                if -d <= fk && fk <= d && x >= n - vf[(off + fk) as usize] {
                    // the reverse snake, back in forward coordinates,
                    // runs from (n-x, m-y) to (n-xs, m-ys)
                    let (sx, sy, ex, ey) = (
                        (n - x) as usize,
                        (m - y) as usize,
                        (n - xs) as usize,
                        (m - ys) as usize,
                    );
                    if (sx, sy) != (0, 0) || (ex, ey) != (0, 0) {
                        return Some((sx, sy, ex, ey));
                    }
                }
            }
            k -= 2;
        }
    }
    None
}

/// One unstable chunk of the merge: what each side did to the same
/// stretch of the base. Identical changes from both sides are ONE
/// change (not an overlap); a chunk only one side touched is that
/// side's; a chunk both touched differently is `None` — the overlap.
fn merge_chunk<'a>(base: &[&'a str], a: &[&'a str], b: &[&'a str], out: &mut Vec<&'a str>) -> bool {
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

/// The rigorous three-way merge of `a` and `b` over `base`, line by
/// line: `Some(merged)` when no chunk overlaps, `None` when one does.
/// Identical changes from both sides never count as an overlap.
#[must_use]
pub fn diff3_merge(base: &str, a: &str, b: &str) -> Option<String> {
    if base.len() > DIFF3_MAX_BYTES
        || a.len() > DIFF3_MAX_BYTES
        || b.len() > DIFF3_MAX_BYTES
        || base.lines().count() > DIFF3_MAX_LINES
        || a.lines().count() > DIFF3_MAX_LINES
        || b.lines().count() > DIFF3_MAX_LINES
    {
        // over the cap (E3): «does not contain» — the copy stays open
        return None;
    }
    let fb: Vec<&str> = base.split('\n').collect();
    let fa: Vec<&str> = a.split('\n').collect();
    let fbb: Vec<&str> = b.split('\n').collect();
    let ma = lcs_matches(&fb, &fa);
    let mb = lcs_matches(&fb, &fbb);
    let mut out: Vec<&str> = Vec::new();
    let mut bi = 0_usize;
    let mut ai = 0_usize;
    let mut ci = 0_usize;
    let mut ia = 0_usize;
    let mut ib = 0_usize;
    loop {
        // the next base line BOTH sides still match is the next stable
        // point; everything before it is one unstable chunk
        while ia < ma.len() && ma[ia].0 < bi {
            ia += 1;
        }
        while ib < mb.len() && mb[ib].0 < bi {
            ib += 1;
        }
        let mut ja = ia;
        let mut jb = ib;
        let common = loop {
            match (ma.get(ja), mb.get(jb)) {
                (Some(x), Some(y)) if x.0 == y.0 => break Some((x.0, x.1, y.1)),
                (Some(x), Some(y)) if x.0 < y.0 => ja += 1,
                (Some(_), Some(_)) => jb += 1,
                _ => break None,
            }
        };
        let Some((k, sa, sb)) = common else {
            return merge_chunk(&fb[bi..], &fa[ai..], &fbb[ci..], &mut out).then(|| out.join("\n"));
        };
        if !merge_chunk(&fb[bi..k], &fa[ai..sa], &fbb[ci..sb], &mut out) {
            return None;
        }
        out.push(fb[k]);
        bi = k + 1;
        ai = sa + 1;
        ci = sb + 1;
        ia += 1;
        ib += 1;
    }
}

// ---------------------------------------------------------------------------
// the derivation, from the set
// ---------------------------------------------------------------------------

/// One operation of the note, as the derivation sees it: the graph
/// fields (they survive redaction and compaction — the skeletons keep
/// hash, hlc and base) plus the body-write facts when the payload
/// lives.
struct NoteOpRow {
    hash: String,
    op_id: Ulid,
    hlc: String,
    actor: String,
    kind: OpKind,
    parent: Option<usize>,
    /// `Some` iff this op is a body write whose payload is here.
    body: Option<BodyFacts>,
}

struct BodyFacts {
    text: String,
    updated_at: String,
}

/// The plan the derivation applies: the winning tip, and the open
/// copies with everything their rows need.
struct Plan {
    /// The winner's body text, when a body write is applied at all.
    winner_body: Option<String>,
    /// A restore newer than every body write set the body by its own
    /// hand: the correction yields.
    yield_to_restore: bool,
    /// One entry per OPEN copy.
    copies: Vec<PlannedCopy>,
}

struct PlannedCopy {
    loser_op: Ulid,
    loser_hlc: String,
    loser_actor: String,
    copy_id: Ulid,
    /// The loser's applied state — the copy's content.
    note: Note,
    /// The stored derivation row omits the body (the loser's payload
    /// is its home); `note` still carries it for the notes row.
    state_json_without_body: bool,
    quarantined: bool,
    visible: bool,
    /// False for a closed-with-operations loser: the row stays as the
    /// birth base of a note that lives on, listed by no conflict.
    open: bool,
}

/// One raw scan row: `(op_id, hlc, actor, kind, base, content_hash,
/// payload body)`.
type ScanRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
);

/// Read every operation of the note — above the compaction cut AND
/// below it (the skeletons still carry hash, hlc and base, which is all
/// the graph asks of them) — in total order.
fn scan(tx: &Transaction<'_>, original: &Ulid) -> Result<Vec<NoteOpRow>, CellError> {
    let mut stmt = tx.prepare(
        "SELECT o.op_id, o.hlc, o.actor, o.kind, o.base, o.content_hash, p.body
         FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         LEFT JOIN op_payloads p ON p.op_id = o.op_id
         WHERE oo.object = ?1
         ORDER BY o.hlc, o.op_id",
    )?;
    let raw: Vec<ScanRow> = stmt
        .query_map([format!("note:{original}")], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let by_hash: HashMap<String, usize> = raw
        .iter()
        .enumerate()
        .map(|(i, r)| (r.5.clone(), i))
        .collect();
    let mapped = raw
        .into_iter()
        .map(|(op_id, hlc, actor, kind, base, hash, body_json)| {
            let op_id = Ulid::from_string(&op_id)
                .map_err(|_| CellError::Db(format!("bad op id {op_id}")))?;
            let kind = OpKind::parse(&kind)?;
            let body = body_json
                .filter(|_| matches!(kind, OpKind::PutNote | OpKind::PatchNote))
                .and_then(|b| body_facts(&kind, &b));
            let parent = base.as_ref().and_then(|b| by_hash.get(b).copied());
            Ok(NoteOpRow {
                hash,
                op_id,
                hlc,
                actor,
                kind,
                parent,
                body,
            })
        })
        .collect::<Result<Vec<_>, CellError>>()?;
    Ok(mapped)
}

/// The `(body, updated_at)` a body write's payload carries: a put's
/// whole note, a patch's `set.body`.
fn body_facts(kind: &OpKind, body_json: &str) -> Option<BodyFacts> {
    let v: serde_json::Value = serde_json::from_str(body_json).ok()?;
    match kind {
        OpKind::PatchNote => {
            let text = v.get("set")?.get("body")?.as_str()?.to_string();
            Some(BodyFacts {
                text,
                updated_at: v.get("updated_at")?.as_str()?.to_string(),
            })
        }
        OpKind::PutNote => {
            let note = v.get("note")?;
            Some(BodyFacts {
                text: note.get("body")?.as_str()?.to_string(),
                updated_at: note.get("updated_at")?.as_str()?.to_string(),
            })
        }
        _ => None,
    }
}

/// Every row's strict ancestors as bitsets — one pass, no allocation
/// in the loops that follow (v0.6.5, E3: the containment product
/// (losers × subsequent writes) paid these walks at every pass; the
/// measured 9,4 ms of a ten-copy note were mostly here, not in the
/// diff3). Pure set state: it never crosses a derivation.
struct Ancestry {
    anc: Vec<Vec<u64>>,
}

impl Ancestry {
    fn of(rows: &[NoteOpRow]) -> Self {
        let words = rows.len().div_ceil(64);
        let mut anc = vec![vec![0_u64; words]; rows.len()];
        let mut done = vec![false; rows.len()];
        for start in 0..rows.len() {
            if done[start] {
                continue;
            }
            let mut path: Vec<usize> = Vec::new();
            let mut cur = Some(start);
            while let Some(i) = cur {
                if done[i] {
                    let bit = 1u64 << (i % 64);
                    let done_rows = anc[i].clone();
                    for &p in &path {
                        anc[p][i / 64] |= bit;
                        for (a_row, done_row) in anc[p].iter_mut().zip(&done_rows) {
                            *a_row |= *done_row;
                        }
                    }
                    break;
                }
                path.push(i);
                cur = rows[i].parent;
            }
            // each path node's ancestors are the path nodes AFTER it
            for (pos, &node) in path.iter().enumerate() {
                for &later in &path[pos + 1..] {
                    let bit = 1u64 << (later % 64);
                    anc[node][later / 64] |= bit;
                }
            }
            for &p in &path {
                done[p] = true;
            }
        }
        Self { anc }
    }

    /// Is `what` a strict ancestor of `of`?
    fn contains(&self, of: usize, what: usize) -> bool {
        self.anc[of]
            .get(what / 64)
            .is_some_and(|w| w >> (what % 64) & 1_u64 == 1)
    }

    /// The containment's fork (E2, both ends included): when one of the
    /// two is the other's ancestor, that ancestor IS the fork; otherwise
    /// the first of `from`'s strict chain that walks `of`'s past.
    fn fork_of(&self, rows: &[NoteOpRow], from: usize, of: usize) -> Option<usize> {
        if self.contains(from, of) {
            return Some(of);
        }
        if self.contains(of, from) {
            return Some(from);
        }
        let mut cur = rows[from].parent;
        while let Some(j) = cur {
            if self.contains(of, j) {
                return Some(j);
            }
            cur = rows[j].parent;
        }
        None
    }
}

/// Walk a row's parent chain, collecting every ancestor index.
fn ancestors_of(rows: &[NoteOpRow], from: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut cur = rows[from].parent;
    while let Some(i) = cur {
        out.push(i);
        cur = rows[i].parent;
    }
    out
}

/// The class-and-order key: the human class first, then today's total
/// order `(hlc, op_id)`. The greater key wins.
fn key_of(row: &NoteOpRow) -> (ActorClass, String, String) {
    (
        classify_actor(&row.actor),
        row.hlc.clone(),
        row.op_id.to_string(),
    )
}

/// The losing body write's candidate set and the winner, from the rows
/// and the set of APPLIED body writes (the replay's or the guard's
/// word): the tips are the applied body writes with no applied body
/// write descending from them; the winner is the greatest key among
/// them; the candidates are the applied body writes the winner's author
/// never saw — the winner itself and its ancestors excepted.
fn winner_and_candidates(rows: &[NoteOpRow], applied: &[usize]) -> (Option<usize>, Vec<usize>) {
    let applied_set: Vec<usize> = {
        let mut v = applied.to_vec();
        v.sort_unstable();
        v
    };
    let mut descendants: Vec<bool> = vec![false; rows.len()];
    for &i in &applied_set {
        for a in ancestors_of(rows, i) {
            descendants[a] = true;
        }
    }
    let tips: Vec<usize> = applied_set
        .iter()
        .copied()
        .filter(|&i| !descendants[i])
        .collect();
    let winner = tips.iter().copied().max_by(|&a, &b| {
        key_of(&rows[a])
            .cmp(&key_of(&rows[b]))
            .then_with(|| a.cmp(&b))
    });
    let Some(winner) = winner else {
        return (None, Vec::new());
    };
    let winner_past: std::collections::HashSet<usize> =
        ancestors_of(rows, winner).into_iter().collect();
    let candidates = applied_set
        .into_iter()
        .filter(|&i| i != winner && !winner_past.contains(&i))
        .collect();
    (Some(winner), candidates)
}

/// The nearest common ancestor of two rows: the first row of `from`'s
/// parent chain that also walks `of`'s.
fn nearest_common_ancestor(rows: &[NoteOpRow], from: usize, of: usize) -> Option<usize> {
    let of_past: std::collections::HashSet<usize> = ancestors_of(rows, of).into_iter().collect();
    ancestors_of(rows, from)
        .into_iter()
        .find(|a| of_past.contains(a))
}

/// The body the fork left: the first body write walking up from the
/// fork point (the text both branches inherited). `None` when the chain
/// dives below the compaction cut — the payload is gone, and the
/// filters that need it step aside (a copy too deep to judge stays).
fn body_at_fork(rows: &[NoteOpRow], fork: Option<usize>) -> Option<String> {
    let mut cur = fork;
    while let Some(i) = cur {
        if let Some(b) = &rows[i].body {
            return Some(b.text.clone());
        }
        cur = rows[i].parent;
    }
    None
}

// ---------------------------------------------------------------------------
// the visible head (v0.6.5, E1): what a local write leans on
// ---------------------------------------------------------------------------

/// `qualifies[i] = seeds[i] || qualifies[parent(i)]`, for every row, in
/// one walk: each parent edge is visited once, whatever order the rows
/// sit in (a base may name an operation that sorts after its child when
/// two nodes share a physical millisecond).
fn inherits_flag(rows: &[NoteOpRow], seeds: &[bool]) -> Vec<bool> {
    let mut memo: Vec<Option<bool>> = vec![None; rows.len()];
    for start in 0..rows.len() {
        if memo[start].is_some() {
            continue;
        }
        let mut path = Vec::new();
        let mut cur = start;
        let value = loop {
            if seeds[cur] {
                break true;
            }
            match rows[cur].parent {
                Some(p) => match memo[p] {
                    Some(v) => break v,
                    None => {
                        path.push(cur);
                        cur = p;
                    }
                },
                None => break false,
            }
        };
        memo[start] = Some(value);
        for i in path {
            memo[i] = Some(value);
        }
    }
    memo.into_iter().map(|m| m.unwrap_or(false)).collect()
}

/// The index of the VISIBLE HEAD: the newest operation in the total
/// order that DESCENDS from the winner of the body (or is the winner) —
/// the line the note shows, header patches above the winner included,
/// the losing branches out. A note closed by a lapide leans on the
/// lapide, or on whoever after it left it closed (its own rule, kept).
/// No applied body write at all (the degenerate note, or a lapide that
/// erased them): today's rule, the newest operation of the note.
fn visible_head_index(rows: &[NoteOpRow], winner: Option<usize>, deleted: bool) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let qualifies: Vec<bool> = if deleted {
        let seeds: Vec<bool> = rows.iter().map(|r| r.kind == OpKind::DeleteNote).collect();
        inherits_flag(rows, &seeds)
    } else if let Some(w) = winner {
        let mut seeds = vec![false; rows.len()];
        seeds[w] = true;
        inherits_flag(rows, &seeds)
    } else {
        vec![true; rows.len()]
    };
    rows.iter()
        .enumerate()
        .filter(|(i, _)| qualifies[*i])
        .max_by(|(ia, a), (ib, b)| {
            (a.hlc.as_str(), a.op_id.to_string())
                .cmp(&(b.hlc.as_str(), b.op_id.to_string()))
                .then(ia.cmp(ib))
        })
        .map(|(i, _)| i)
}

/// Write the note's visible head (schema 22): the derivation's word,
/// one indexed row the local write reads. The purge takes the row with
/// the note (nothing leans on a purged note ever again).
fn write_note_head(
    tx: &Transaction<'_>,
    original: &Ulid,
    rows: &[NoteOpRow],
    winner: Option<usize>,
) -> Result<(), CellError> {
    let deleted = ops::read_note_row(tx, original)?
        .is_some_and(|row| row.note.status == crate::format::Status::Deleted);
    match visible_head_index(rows, winner, deleted) {
        Some(h) => {
            tx.execute(
                "INSERT INTO note_heads (note_id, content_hash) VALUES (?1, ?2)
                 ON CONFLICT(note_id) DO UPDATE SET content_hash = ?2",
                rusqlite::params![original.to_string(), rows[h].hash],
            )?;
        }
        None => {
            tx.execute(
                "DELETE FROM note_heads WHERE note_id = ?1",
                [original.to_string()],
            )?;
        }
    }
    Ok(())
}

/// Does the SET hold a closure witness for this loser? (D1.6, E2) An
/// operation on the copy's id; an APPLIED body write after the loser
/// in the total order that DESCENDS from it (its author had seen the
/// loser — the containment's special case with the fork taken at the
/// loser itself, kept its own arm because it needs no payload); or an
/// applied later body write that CONTAINS it — the rigorous diff3 from
/// the fork between the LOSER and THAT WRITER returns the writer
/// without overlaps. The fork never names the winner: a late arrival
/// can move it, and a witness is a fact about the pair and their past
/// (v0.6.5, E2 — the reopened copies of the reviewer's R2). A later
/// write with the very same text contains it (two identical changes
/// are one change), which is how equal-bodied losers collapse into a
/// single copy.
fn has_witness(
    tx: &Transaction<'_>,
    rows: &[NoteOpRow],
    ancestry: &Ancestry,
    loser: usize,
    applied: &[usize],
    copy_id: &Ulid,
) -> Result<bool, CellError> {
    // (a) an operation on the copy's id
    let on_copy: i64 = tx.query_row(
        "SELECT COUNT(*) FROM op_objects WHERE object = ?1",
        [format!("note:{copy_id}")],
        |r| r.get(0),
    )?;
    if on_copy > 0 {
        return Ok(true);
    }
    // every APPLIED body write that descends from the loser: its author
    // had seen it (the base chain walks through it)
    if applied.iter().any(|&i| ancestry.contains(i, loser)) {
        return Ok(true);
    }
    // the witness is a LATER write, in the TOTAL ORDER — not in the
    // class key: a HUMAN ancestor sorts above a later AGENT write there,
    // and «contains» its own descendant by the inclusive fork, closing
    // the copy of the very author who had read the text (found by the
    // E4 judge: the equal-bodied follow-up). The class decides WHO
    // WINS; the order of time decides who comes after.
    let loser_key = (rows[loser].hlc.as_str(), rows[loser].op_id.to_string());
    let loser_text = rows[loser].body.as_ref().map(|b| b.text.as_str());
    let Some(loser_text) = loser_text else {
        return Ok(false);
    };
    for &i in applied {
        let key = (rows[i].hlc.as_str(), rows[i].op_id.to_string());
        if key <= loser_key {
            continue;
        }
        // a descendant saw the loser, whatever the diff says of its tail
        if ancestry.contains(i, loser) {
            return Ok(true);
        }
        let Some(text) = rows[i].body.as_ref().map(|b| b.text.as_str()) else {
            continue;
        };
        // the fork is the pair's own, both ends included (E2)
        let fork = ancestry.fork_of(rows, loser, i);
        let fork_body = body_at_fork(rows, fork);
        if let Some(f) = fork_body.as_deref()
            && let Some(merged) = diff3_merge(f, text, loser_text)
            && merged == text
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The whole derivation of one note, applied to the derived rows: the
/// open copies (with the cap's visibility), the closures, the
/// quarantined copies' queue rows, and the winner's body — the note's
/// body is the WINNER's, even when a later write applied over it and
/// lost the class comparison.
pub(crate) fn after_replay(
    tx: &Transaction<'_>,
    original: &Ulid,
    applied_hashes: &std::collections::HashSet<String>,
    states: &HashMap<String, Note>,
) -> Result<(), CellError> {
    let rows = scan(tx, original)?;
    // the applied body writes: the events the replay recorded (or, on
    // the fast path, every body write — the guard saw to it)
    let applied: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.body.is_some() && applied_hashes.contains(&r.hash))
        .map(|(i, _)| i)
        .collect();
    let (winner, candidates) = winner_and_candidates(&rows, &applied);
    // v0.6.5 (E1): the visible head — the local writer's `base` — rides
    // with the same derivation, from the same set
    write_note_head(tx, original, &rows, winner)?;
    let ancestry = Ancestry::of(&rows);
    let winner_body = winner.and_then(|w| rows[w].body.as_ref().map(|b| b.text.clone()));
    let mut plan = Plan {
        winner_body,
        yield_to_restore: newer_restore(&rows, &applied),
        copies: Vec::new(),
    };
    // BIRTH ROWS: any note whose id is a copy's id and carries
    // operations of its own (an edit, the resolving delete, a decision)
    // needs its derivation row — it is the replay base. That includes a
    // loser that later became the WINNER (or the winner's ancestor): the
    // incremental path kept its row from when it was a loser, and a
    // rebuild from the log alone must land on the same row or the copy's
    // own ops park forever (found by the oracle on the phased fleet)
    let mut planned: Vec<Ulid> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let _ = i;
        // a birth row is a BASE, not a standing: the loser may even be
        // one the lapide erased (its state arrives from the replay's
        // erased_states) — the copy lived, its note replays over the row
        if row.body.is_none() {
            continue;
        }
        let copy_id = material::conflict_copy_id(&row.hash, hlc_ms_of(&row.hlc));
        let has_ops: i64 = tx.query_row(
            "SELECT COUNT(*) FROM op_objects WHERE object = ?1",
            [format!("note:{copy_id}")],
            |r| r.get(0),
        )?;
        if has_ops == 0 {
            continue;
        }
        planned.push(copy_id);
        if plan.copies.iter().any(|c| c.copy_id == copy_id) {
            continue;
        }
        let quarantined = classify_actor(&row.actor) == ActorClass::Agent;
        if let Some(state) = states.get(&row.hash) {
            let mut note = state.clone();
            note.id = copy_id;
            // the same content every path writes: the class decides the
            // state (a divergence here left one copy active on one device
            // and quarantined on another — found by the phased fleet)
            note.status = if quarantined {
                crate::format::Status::Quarantined
            } else {
                crate::format::Status::Active
            };
            if let Some(facts) = row.body.as_ref()
                && let Ok(at) = crate::format::At::parse(&facts.updated_at)
            {
                note.updated_at = at;
            }
            if !note.tags.iter().any(|t| t == "conflitto") {
                note.tags.push("conflitto".into());
            }
            plan.copies.push(PlannedCopy {
                loser_op: row.op_id,
                loser_hlc: row.hlc.clone(),
                loser_actor: row.actor.clone(),
                copy_id,
                note,
                state_json_without_body: true,
                quarantined,
                visible: false,
                open: false,
            });
        }
    }
    for loser in candidates {
        let facts = rows[loser]
            .body
            .as_ref()
            .expect(/* just checked */ "applied body write");
        // the filters (D1.5): no copy for a body equal to the winner's,
        // none for a body the fork already had. A filtered loser the
        // user made theirs keeps its birth row, like every closed one.
        let fork_same = match winner.and_then(|w| nearest_common_ancestor(&rows, loser, w)) {
            Some(fork) => body_at_fork(&rows, Some(fork)).as_deref() == Some(facts.text.as_str()),
            None => false,
        };
        let filtered = plan.winner_body.as_deref() == Some(facts.text.as_str()) || fork_same;
        let copy_id = material::conflict_copy_id(&rows[loser].hash, hlc_ms_of(&rows[loser].hlc));
        let witnessed =
            winner.is_some() && has_witness(tx, &rows, &ancestry, loser, &applied, &copy_id)?;
        let mut open = true;
        if filtered || witnessed {
            // closed. But a copy the user made THEIRS (an edit, the
            // resolving delete, a decision) keeps its BIRTH ROW — it is
            // the replay base of a note that lives on, and a rebuild
            // from the log alone must land on it
            let has_ops: i64 = tx.query_row(
                "SELECT COUNT(*) FROM op_objects WHERE object = ?1",
                [format!("note:{copy_id}")],
                |r| r.get(0),
            )?;
            if has_ops == 0 {
                continue;
            }
            open = false;
        }
        let Some(state) = states.get(&rows[loser].hash) else {
            continue;
        };
        let quarantined = classify_actor(&rows[loser].actor) == ActorClass::Agent;
        let mut note = state.clone();
        note.id = copy_id;
        note.status = if quarantined {
            crate::format::Status::Quarantined
        } else {
            crate::format::Status::Active
        };
        note.updated_at = crate::format::At::parse(&facts.updated_at).unwrap_or(state.updated_at);
        if !note.tags.iter().any(|t| t == "conflitto") {
            note.tags.push("conflitto".into());
        }
        if !planned.contains(&copy_id) {
            plan.copies.push(PlannedCopy {
                loser_op: rows[loser].op_id,
                loser_hlc: rows[loser].hlc.clone(),
                loser_actor: rows[loser].actor.clone(),
                copy_id,
                note,
                // the derivation row keeps everything but the BODY —
                // measured (D5): the body twice was a fourth of a copy's
                // cost, and the loser's payload is the body's home
                state_json_without_body: true,
                quarantined,
                visible: false,
                open,
            });
        } else if open {
            // the birth pass wrote it closed; an OPEN loser upgrades it
            if let Some(slot) = plan.copies.iter_mut().find(|c| c.copy_id == copy_id) {
                slot.note = note;
                slot.open = true;
                slot.quarantined = quarantined;
            }
        }
    }
    // the cap: the three most recent OPEN copies (total order) are
    // visible; the others stay open and wait for a window
    plan.copies.sort_by(|a, b| {
        (a.loser_hlc.clone(), a.loser_op.to_string())
            .cmp(&(b.loser_hlc.clone(), b.loser_op.to_string()))
    });
    let open_count = plan.copies.iter().filter(|c| c.open).count();
    let mut open_seen = 0_usize;
    for c in plan.copies.iter_mut() {
        if !c.open {
            continue;
        }
        open_seen += 1;
        c.visible = open_seen + VISIBLE_COPIES_CAP > open_count;
    }
    apply(tx, original, &plan)
}

/// Is a RESTORE the newest body-setting event? A restore rewrites the
/// body from a version snapshot without being a body write; the
/// winner-body correction (and its guard) yield to the newest one.
fn newer_restore(rows: &[NoteOpRow], applied: &[usize]) -> bool {
    let newest_write = applied.iter().copied().max_by(|&a, &b| {
        (rows[a].hlc.as_str(), rows[a].op_id.to_string())
            .cmp(&(rows[b].hlc.as_str(), rows[b].op_id.to_string()))
    });
    let Some(newest_write) = newest_write else {
        return false;
    };
    rows.iter().any(|r| {
        r.kind == OpKind::Restore
            && (r.hlc.as_str(), r.op_id.to_string())
                > (
                    rows[newest_write].hlc.as_str(),
                    rows[newest_write].op_id.to_string(),
                )
    })
}

/// The `physical_ms` of an hlc string (the copy id's time part).
fn hlc_ms_of(hlc: &str) -> u64 {
    crate::hlc::Hlc::parse(hlc)
        .map(|h| h.physical_ms)
        .unwrap_or(0)
}

/// Write the plan: refresh the open copies' rows (and the notes rows of
/// the visible ones — with the queue row of every quarantined one),
/// close the ones the plan no longer wants (a copy the user made theirs
/// with edits lives on as its own note: its derivation row stays as the
/// replay base), and set the note's body to the winner's.
fn apply(tx: &Transaction<'_>, original: &Ulid, plan: &Plan) -> Result<(), CellError> {
    let desired: Vec<Ulid> = plan.copies.iter().map(|c| c.copy_id).collect();
    let existing: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT copy_id FROM conflict_copies WHERE original_id = ?1 ORDER BY copy_id",
        )?;
        let rows = stmt.query_map([original.to_string()], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for copy_id in existing {
        let ulid = Ulid::from_string(&copy_id)
            .map_err(|_| CellError::Db(format!("bad conflict copy id {copy_id}")))?;
        if desired.contains(&ulid) {
            continue;
        }
        let has_ops: i64 = tx.query_row(
            "SELECT COUNT(*) FROM op_objects WHERE object = ?1",
            [format!("note:{ulid}")],
            |r| r.get(0),
        )?;
        if has_ops > 0 {
            // the user made the copy theirs: the note lives on, the row
            // stays as its replay base — and the conflict is CLOSED
            tx.execute(
                "UPDATE conflict_copies SET open = 0, visible = 0 WHERE copy_id = ?1",
                [&copy_id],
            )?;
            // the queue row of a decided quarantined copy is the copy's
            // own replay's business; an undecided one goes
            tx.execute(
                "DELETE FROM proposals WHERE note_id = ?1 AND state = 'pending' AND reason = 'conflitto'",
                [&copy_id],
            )?;
            continue;
        }
        material::delete_note_state(tx, &ulid)?;
        super::vectors::delete_note_vectors(tx, &ulid)?;
        tx.execute("DELETE FROM conflict_copies WHERE copy_id = ?1", [&copy_id])?;
        tx.execute(
            "DELETE FROM proposals WHERE note_id = ?1 AND state = 'pending'",
            [&copy_id],
        )?;
    }
    for copy in &plan.copies {
        let stored = if copy.state_json_without_body {
            let mut lean = copy.note.clone();
            lean.body.clear();
            lean
        } else {
            copy.note.clone()
        };
        tx.execute(
            "INSERT INTO conflict_copies (copy_id, original_id, loser_op, state_json,
                                          loser_actor, loser_hlc, open, visible)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?8, ?7)
             ON CONFLICT(copy_id) DO UPDATE SET
                original_id = ?2, loser_op = ?3, state_json = ?4,
                loser_actor = ?5, loser_hlc = ?6, open = ?8, visible = ?7",
            rusqlite::params![
                copy.copy_id.to_string(),
                original.to_string(),
                copy.loser_op.to_string(),
                stored.to_json().to_string(),
                copy.loser_actor,
                copy.loser_hlc,
                i64::from(copy.visible),
                i64::from(copy.open)
            ],
        )?;
        if !copy.open {
            // a closed birth row: the copy's note lives its own life
            // (an edit, the resolving delete's tombstone, a decision) —
            // its rows belong to its own materialization, not to this
            // derivation
            material::materialize(tx, &format!("note:{}", copy.copy_id))?;
            continue;
        }
        if copy.visible {
            material::restore_conflict_copy(tx, &copy.copy_id, original, &copy.note)?;
            if copy.quarantined {
                // the agent's copy sits in the queue, DERIVED — no
                // operation of its own; the row goes when the copy
                // closes, and a `decide` on it is an operation on the
                // copy's id, which closes the conflict by (a)
                tx.execute(
                    "INSERT INTO proposals (note_id, mode, target, proposer, client, reason,
                                            state, proposed_at, auto_failed)
                     VALUES (?1, 'new', NULL, ?2, NULL, 'conflitto', 'pending', ?3, 0)
                     ON CONFLICT(note_id) DO UPDATE SET
                        proposer = ?2, reason = 'conflitto',
                        proposed_at = ?3",
                    rusqlite::params![
                        copy.copy_id.to_string(),
                        copy.loser_actor,
                        copy.note.updated_at.to_db_string()
                    ],
                )?;
            }
        } else {
            // an invisible copy keeps no state rows: it appears, whole,
            // when a visible one closes and the window moves
            material::delete_note_state(tx, &copy.copy_id)?;
            tx.execute(
                "DELETE FROM proposals WHERE note_id = ?1 AND state = 'pending'",
                [copy.copy_id.to_string()],
            )?;
        }
        // a copy the user edited rematerializes over the new derivation
        let has_ops: i64 = tx.query_row(
            "SELECT COUNT(*) FROM op_objects WHERE object = ?1",
            [format!("note:{}", copy.copy_id)],
            |r| r.get(0),
        )?;
        if has_ops > 0 {
            material::materialize(tx, &format!("note:{}", copy.copy_id))?;
        }
    }
    // the winner's body: the note's body is the winner's even when a
    // later write applied over it — the human hand, older, wins. The
    // lapide keeps its own state: a deleted note is not corrected.
    // a RESTORE newer than the winner set the body by its own hand (it
    // is no body write, but it rewrites the body from a version): the
    // correction yields to the newest body-setting event, or it would
    // undo every restore of a note with a conflict past
    if let Some(body) = &plan.winner_body
        && let Some(row) = ops::read_note_row(tx, original)?
        && row.note.status != crate::format::Status::Deleted
        && row.note.body != *body
        && !plan.yield_to_restore
    {
        // the same language every writer detects: the note's own, or
        // the detection over title and body (the correction once wrote
        // body_norm unstemmed — the oracle saw it)
        let lang = row.note.lang.or_else(|| {
            crate::text::detect_lang(&format!("{}\n{}", row.note.display_title(), body))
        });
        let body_norm = crate::text::normalize_for_index(row.note.title.as_deref(), body, lang);
        tx.execute(
            "UPDATE notes SET body = ?2, body_norm = ?3 WHERE id = ?1",
            rusqlite::params![original.to_string(), body, body_norm],
        )?;
        ops::reindex_note(tx, original, Some(&row))?;
    }
    Ok(())
}

/// Does this note hold exactly ONE stored operation? The derivation of a
/// single-op note is the identity: one body write at most, the only tip,
/// the winner, no loser, no copy, no correction — every path agrees
/// without running. One indexed COUNT, the price the hot insert path
/// pays instead of the scan.
fn single_op_note(tx: &Transaction<'_>, original: &Ulid) -> Result<bool, CellError> {
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         WHERE oo.object = ?1",
        [format!("note:{original}")],
        |r| r.get(0),
    )?;
    Ok(n <= 1)
}

/// Does the note's op set above the cut need the full replay for the
/// derivation? Deletes, restores, supersedes and purges move state in
/// ways only the replay can see, and a pending object has operations
/// the state does not reflect yet.
fn needs_replay(
    tx: &Transaction<'_>,
    original: &Ulid,
    arriving: Option<&str>,
) -> Result<bool, CellError> {
    let pending: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pending_objects WHERE object = ?1",
        [format!("note:{original}")],
        |r| r.get(0),
    )?;
    if pending > 0 {
        return Ok(true);
    }
    // the winner-body correction is in effect: the live row's body is
    // NOT the body a sequential replay would hold at this point (it is
    // the class-winner's, an older write's). Any version snapshot taken
    // from the live state now — the next patch's, the next delete's —
    // would diverge from the rebuild's sequential snapshot (found by
    // the oracle, seed 12328535914020248362): replay the whole note, so
    // the snapshots and the chain land as the rebuild computes them
    if let Some(row) = ops::read_note_row(tx, original)? {
        let rows = scan(tx, original)?;
        // the newest body write in the TOTAL ORDER (not the class
        // winner): that is the body a sequential replay holds now
        let applied_all: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.body.is_some())
            .map(|(i, _)| i)
            .collect();
        if !newer_restore(&rows, &applied_all)
            && let Some(newest) = rows
                .iter()
                .filter(|r| r.body.is_some() && Some(r.hash.as_str()) != arriving)
                .enumerate()
                .max_by(|(ia, a), (ib, b)| {
                    (a.hlc.as_str(), a.op_id.to_string())
                        .cmp(&(b.hlc.as_str(), b.op_id.to_string()))
                        .then_with(|| ia.cmp(ib))
                })
                .map(|(_, r)| r)
            && newest
                .body
                .as_ref()
                .is_some_and(|b| b.text != row.note.body)
        {
            return Ok(true);
        }
    }
    let cut: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM snapshots", [], |r| {
        r.get(0)
    })?;
    let heavy: i64 = tx.query_row(
        "SELECT COUNT(*) FROM ops o
         JOIN op_objects oo ON oo.op_id = o.op_id
         WHERE oo.object = ?1 AND o.seq > ?2
           AND o.kind IN ('delete_note', 'restore', 'supersede', 'purge')",
        rusqlite::params![format!("note:{original}"), cut],
        |r| r.get(0),
    )?;
    Ok(heavy > 0)
}

/// After a body write took the ARRIVAL fast path (or was written
/// LOCALLY): the same derivation the replay pays. When the note needs
/// the replay — or the derivation wants a state only the replay has —
/// the object rematerializes from the whole set, and the derivation
/// runs there.
pub(crate) fn after_body_write(
    tx: &Transaction<'_>,
    original: &Ulid,
    arriving: &Op,
) -> Result<(), CellError> {
    // the purge dominates (see rederive_note)
    if material::note_purged_in_set(tx, original)? {
        return Ok(());
    }
    // ONE operation, the one that just applied: it is the only tip, the
    // winner by construction, no loser exists to open a copy and no
    // correction can differ (the note's body IS this write's). The bulk
    // insert path (every note fresh) paid the whole derivation here —
    // measured −14% on 10k puts against v0.6.3
    if single_op_note(tx, original)? {
        return Ok(());
    }
    // the snapshot this op just wrote holds the PRE-arrival body — the
    // state a sequential replay would have snapshot only when the
    // correction was NOT in effect. Compare THAT (the post-apply body
    // can coincide with the newest write even when the pre-body was
    // the corrected one, and the wrong snapshot would stay — found by
    // the hunt, seed 3806138527623646080)
    if arriving.kind == OpKind::PatchNote {
        let pre_body: Option<String> = tx
            .query_row(
                "SELECT json_extract(snapshot_json, '$.body') FROM note_versions
                 WHERE note_id = ?1 AND op_id = ?2",
                rusqlite::params![original.to_string(), arriving.op_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(pre) = pre_body {
            let rows = scan(tx, original)?;
            if let Some(newest) = rows
                .iter()
                .filter(|r| {
                    r.body.is_some()
                        && Some(r.hash.as_str()) != Some(arriving.content_hash.as_str())
                })
                .max_by(|a, b| {
                    (a.hlc.as_str(), a.op_id.to_string())
                        .cmp(&(b.hlc.as_str(), b.op_id.to_string()))
                })
                && let Some(facts) = newest.body.as_ref()
                && facts.text != pre
            {
                return material::materialize(tx, &format!("note:{original}"));
            }
        }
    }
    if needs_replay(tx, original, Some(&arriving.content_hash))? {
        return material::materialize(tx, &format!("note:{original}"));
    }
    // the arriving op's applied state is the live row — it applied over
    // the state the replay would have produced
    let arriving_state =
        ops::read_note_row(tx, original)?.map(|row| (arriving.content_hash.clone(), row.note));
    let mut states = HashMap::new();
    let mut known_only_partial = false;
    let rows = scan(tx, original)?;
    let applied: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.body.is_some())
        .map(|(i, _)| i)
        .collect();
    let (winner, candidates) = winner_and_candidates(&rows, &applied);
    // the fast path only ever needs the ARRIVING op's state: an older
    // candidate already open has its row, and one that is not open and
    // is not the arrival can only have lost a filter it passes now —
    // the winner moved — which the replay must judge
    if let Some((hash, note)) = arriving_state {
        states.insert(hash, note);
    }
    for loser in &candidates {
        if Some(*loser) == winner {
            continue;
        }
        let is_arrival = rows[*loser].hash == arriving.content_hash;
        let has_row: i64 = tx.query_row(
            "SELECT COUNT(*) FROM conflict_copies WHERE loser_op = ?1",
            [rows[*loser].op_id.to_string()],
            |r| r.get(0),
        )?;
        if !is_arrival && has_row == 0 {
            known_only_partial = true;
            break;
        }
    }
    // and any copy-with-ops whose birth row is missing needs the replay
    // too (the loser may have become the winner: only the replay's
    // events carry the state the birth row wants)
    for row in &rows {
        let Some(_facts) = row.body.as_ref() else {
            continue;
        };
        let copy_id = material::conflict_copy_id(&row.hash, hlc_ms_of(&row.hlc));
        let has_ops: i64 = tx.query_row(
            "SELECT COUNT(*) FROM op_objects WHERE object = ?1",
            [format!("note:{copy_id}")],
            |r| r.get(0),
        )?;
        if has_ops > 0 {
            let has_row: i64 = tx.query_row(
                "SELECT COUNT(*) FROM conflict_copies WHERE copy_id = ?1",
                [copy_id.to_string()],
                |r| r.get(0),
            )?;
            if has_row == 0 {
                known_only_partial = true;
                break;
            }
        }
    }
    if known_only_partial {
        return material::materialize(tx, &format!("note:{original}"));
    }
    // the older open copies' states come from their rows; an arriving
    // loser takes the live row it just produced
    let existing_states: HashMap<String, Note> = {
        let mut stmt = tx.prepare(
            "SELECT cc.loser_op, cc.state_json FROM conflict_copies cc
             WHERE cc.original_id = ?1 AND cc.open = 1",
        )?;
        let rows_out: Vec<(String, String)> = stmt
            .query_map([original.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let by_op: std::collections::HashMap<String, String> = rows_out
            .into_iter()
            .filter(|(op, _)| op != &arriving.op_id.to_string())
            .collect();
        let mut out = HashMap::new();
        for row in &rows {
            if let Some(json) = by_op.get(&row.op_id.to_string())
                && let Ok(mut note) = Note::from_json(
                    &serde_json::from_str::<serde_json::Value>(json)
                        .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?,
                )
            {
                // the stored state has no body: the loser's payload is
                // its home, and it is here (a loser is a body write)
                if let Some(facts) = row.body.as_ref() {
                    note.body = facts.text.clone();
                }
                out.insert(row.hash.clone(), note);
            }
        }
        out
    };
    for (hash, note) in existing_states {
        states.entry(hash).or_insert(note);
    }
    let applied_hashes: std::collections::HashSet<String> = rows
        .iter()
        .filter(|r| r.body.is_some())
        .map(|r| r.hash.clone())
        .collect();
    after_replay(tx, original, &applied_hashes, &states)
}

/// After any operation touched a note — a body write, a header patch, a
/// supersede, an operation on a COPY's id: every arrival can change the
/// derivation (a node that completes the graph moves a tip; an
/// operation on a copy's id is a closure witness). Without this, a
/// device whose last arrival for the note was not a body write kept a
/// stale conflict set until some later write fixed it — and two devices
/// of one fleet diverged (found by the phased fleet, seed
/// 1421488054822918858).
pub(crate) fn rederive_note(tx: &Transaction<'_>, original: &Ulid) -> Result<(), CellError> {
    // the purge dominates: a purged note does not materialize and has
    // no conflict set to derive (its copies went with it, purge_conflict_copies)
    if material::note_purged_in_set(tx, original)? {
        return Ok(());
    }
    if single_op_note(tx, original)? {
        return Ok(());
    }
    if needs_replay(tx, original, None)? {
        return material::materialize(tx, &format!("note:{original}"));
    }
    let rows = scan(tx, original)?;
    let applied: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.body.is_some())
        .map(|(i, _)| i)
        .collect();
    let (_, candidates) = winner_and_candidates(&rows, &applied);
    // a copy with operations of its own whose BIRTH ROW is missing
    // needs the replay: only the replay's events carry the state the
    // row wants (found by the oracle — the merged fleet, seed
    // 15192944291106605994)
    for row in &rows {
        if row.body.is_none() {
            continue;
        }
        let copy_id = material::conflict_copy_id(&row.hash, hlc_ms_of(&row.hlc));
        let has_ops: i64 = tx.query_row(
            "SELECT COUNT(*) FROM op_objects WHERE object = ?1",
            [format!("note:{copy_id}")],
            |r| r.get(0),
        )?;
        if has_ops > 0 {
            let has_row: i64 = tx.query_row(
                "SELECT COUNT(*) FROM conflict_copies WHERE copy_id = ?1",
                [copy_id.to_string()],
                |r| r.get(0),
            )?;
            if has_row == 0 {
                return material::materialize(tx, &format!("note:{original}"));
            }
        }
    }
    // every candidate the witnesses have not closed needs its state:
    // open rows carry theirs; anything else needs the replay
    let mut states: HashMap<String, Note> = HashMap::new();
    let mut needs_full = false;
    let open_rows: std::collections::HashMap<String, String> = {
        let mut stmt =
            tx.prepare("SELECT loser_op, state_json FROM conflict_copies WHERE original_id = ?1")?;
        let out: Vec<(String, String)> = stmt
            .query_map([original.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        out.into_iter().collect()
    };
    for loser in candidates {
        let Some(json) = open_rows.get(&rows[loser].op_id.to_string()) else {
            needs_full = true;
            break;
        };
        let mut note = Note::from_json(
            &serde_json::from_str::<serde_json::Value>(json)
                .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?,
        )
        .map_err(|e| CellError::Db(format!("corrupt conflict state: {e}")))?;
        // the stored state has no body: the loser's payload is its home
        if let Some(facts) = rows[loser].body.as_ref() {
            note.body = facts.text.clone();
        }
        states.insert(rows[loser].hash.clone(), note);
    }
    if needs_full {
        return material::materialize(tx, &format!("note:{original}"));
    }
    let applied_hashes: std::collections::HashSet<String> = rows
        .iter()
        .filter(|r| r.body.is_some())
        .map(|r| r.hash.clone())
        .collect();
    after_replay(tx, original, &applied_hashes, &states)
}

/// One conflict of a cell, as `conflicts()` reports it (D3).
#[derive(Debug, Clone)]
pub struct ConflictInfo {
    /// The copy's note id (deterministic, a function of the loser).
    pub copy_id: Ulid,
    /// The original the copy lost against.
    pub original_id: Ulid,
    /// Who wrote the losing body.
    pub loser_actor: String,
    /// Human or agent — the class that lost.
    pub loser_class: ActorClass,
    /// The loser's HLC, the copy's own birth time.
    pub loser_at: String,
    /// An agent's copy is born in quarantine, in the queue.
    pub quarantined: bool,
    /// Inside the cap's window of three: a notes row, readable with
    /// `get` like every note. The others stay open and appear when a
    /// visible one closes.
    pub visible: bool,
}

/// Every OPEN conflict copy of a cell.
///
/// # Errors
/// [`CellError::Db`] on SQL failure.
pub fn conflicts(tx: &Transaction<'_>) -> Result<Vec<ConflictInfo>, CellError> {
    let mut stmt = tx.prepare(
        "SELECT copy_id, original_id, loser_actor, loser_hlc, open, visible FROM conflict_copies
         WHERE open = 1 ORDER BY original_id, loser_hlc, loser_op",
    )?;
    let out: Vec<ConflictInfo> = stmt
        .query_map([], |r| {
            let copy_id: String = r.get(0)?;
            let original_id: String = r.get(1)?;
            let loser_actor: String = r.get(2)?;
            let loser_hlc: String = r.get(3)?;
            let open: i64 = r.get(4)?;
            let visible: i64 = r.get(5)?;
            Ok((copy_id, original_id, loser_actor, loser_hlc, open, visible))
        })?
        .filter_map(|row| row.ok())
        .filter_map(
            |(copy_id, original_id, loser_actor, loser_hlc, open, visible)| {
                if open == 0 {
                    return None;
                }
                let (Ok(copy_id), Ok(original_id)) =
                    (Ulid::from_string(&copy_id), Ulid::from_string(&original_id))
                else {
                    return None;
                };
                let at = crate::hlc::Hlc::parse(&loser_hlc)
                    .map(|h| {
                        let t = time::OffsetDateTime::UNIX_EPOCH
                            + time::Duration::milliseconds(h.physical_ms as i64);
                        crate::format::At(t).to_db_string()
                    })
                    .unwrap_or_else(|_| loser_hlc.clone());
                let loser_class = classify_actor(&loser_actor);
                Some(ConflictInfo {
                    copy_id,
                    original_id,
                    loser_class,
                    loser_at: at,
                    loser_actor,
                    // an agent's copy is born in quarantine, in the queue
                    quarantined: loser_class == ActorClass::Agent,
                    visible: visible != 0,
                })
            },
        )
        .collect();
    // the quarantine lives in the copy's own note state (derived)
    let mut with_status = Vec::with_capacity(out.len());
    for mut info in out {
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM notes WHERE id = ?1",
                [info.copy_id.to_string()],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        info.quarantined = status.as_deref() == Some("quarantined");
        with_status.push(info);
    }
    Ok(with_status)
}

/// One conflict copy IN FULL (v0.6.5, E5): everything [`conflicts`]
/// lists, plus the three texts a choice is about — the original's body,
/// the copy's body and the body at the fork between the loser and the
/// winner (`null` under the compaction cut). The quarantined copies
/// are readable HERE and only here: `get` stays the closed oracle, and
/// a quarantined note answers `null` to it.
#[derive(Debug, Clone)]
pub struct ConflictDetail {
    /// The copy's note id (deterministic, a function of the loser).
    pub copy_id: Ulid,
    /// The original the copy lost against.
    pub original_id: Ulid,
    /// Who wrote the losing body.
    pub loser_actor: String,
    /// Human or agent — the class that lost.
    pub loser_class: ActorClass,
    /// The loser's HLC, the copy's own birth time.
    pub loser_at: String,
    /// An agent's copy is born in quarantine, in the queue.
    pub quarantined: bool,
    /// Inside the cap's window of three: a notes row of its own.
    pub visible: bool,
    /// The ORIGINAL's body as it stands.
    pub original_body: Option<String>,
    /// The LOSER's body (the loser's payload is its home).
    pub copy_body: Option<String>,
    /// The body at the fork between the loser and the winner — what a
    /// three-column view shows in the middle.
    pub fork_body: Option<String>,
}

/// One conflict copy in full, by its id (v0.6.5, E5): the read for the
/// screen that asks the user to choose. A copy that is missing or
/// CLOSED answers [`CellError::NoteNotFound`]: a closed copy is no
/// longer a choice.
///
/// # Errors
/// [`CellError::Db`] on SQL failure; [`CellError::NoteNotFound`] when
/// the id is no OPEN conflict copy.
pub fn conflict(tx: &Transaction<'_>, copy_id: &Ulid) -> Result<ConflictDetail, CellError> {
    let row: Option<(String, String, String, String, i64, i64)> = tx
        .query_row(
            "SELECT copy_id, original_id, loser_actor, loser_hlc, open, visible
             FROM conflict_copies WHERE copy_id = ?1",
            [copy_id.to_string()],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((_, original_id, loser_actor, loser_hlc, open, visible)) = row else {
        return Err(CellError::NoteNotFound(copy_id.to_string()));
    };
    if open == 0 {
        // closed: no longer a choice
        return Err(CellError::NoteNotFound(copy_id.to_string()));
    }
    let original = Ulid::from_string(&original_id)
        .map_err(|_| CellError::Db(format!("bad conflict original {original_id}")))?;
    let loser_op: String = tx.query_row(
        "SELECT loser_op FROM conflict_copies WHERE copy_id = ?1",
        [copy_id.to_string()],
        |r| r.get(0),
    )?;
    // the original's body as it stands
    let original_body = ops::read_note_row(tx, &original)?.map(|row| row.note.body);
    // the loser's body: its payload is the home (a patch's set.body, a
    // put's note.body); the copy's own notes row is the fallback
    let copy_body: Option<String> = {
        let from_payload: Option<String> = tx
            .query_row(
                "SELECT json_extract(p.body, '$.set.body') FROM op_payloads p
                 WHERE p.op_id = ?1 AND json_extract(p.body, '$.set.body') IS NOT NULL",
                [&loser_op],
                |r| r.get(0),
            )
            .optional()?;
        match from_payload {
            Some(b) => Some(b),
            None => tx
                .query_row(
                    "SELECT json_extract(p.body, '$.note.body') FROM op_payloads p
                     WHERE p.op_id = ?1",
                    [&loser_op],
                    |r| r.get(0),
                )
                .optional()?,
        }
    };
    // the fork's body, from the same derivation the set speaks
    let fork_body = {
        let rows = scan(tx, &original)?;
        let loser = rows.iter().position(|r| r.op_id.to_string() == loser_op);
        let applied: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.body.is_some())
            .map(|(i, _)| i)
            .collect();
        let (winner, _) = winner_and_candidates(&rows, &applied);
        match (loser, winner) {
            (Some(l), Some(w)) => {
                let fork = nearest_common_ancestor(&rows, l, w);
                body_at_fork(&rows, fork)
            }
            _ => None,
        }
    };
    let at = crate::hlc::Hlc::parse(&loser_hlc)
        .map(|h| {
            let t = time::OffsetDateTime::UNIX_EPOCH
                + time::Duration::milliseconds(h.physical_ms as i64);
            crate::format::At(t).to_db_string()
        })
        .unwrap_or_else(|_| loser_hlc.clone());
    let loser_class = classify_actor(&loser_actor);
    // the quarantine lives in the copy's own note state (derived)
    let status: Option<String> = tx
        .query_row(
            "SELECT status FROM notes WHERE id = ?1",
            [copy_id.to_string()],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(ConflictDetail {
        copy_id: *copy_id,
        original_id: original,
        loser_actor,
        loser_class,
        loser_at: at,
        quarantined: status.as_deref() == Some("quarantined"),
        visible: visible != 0,
        original_body,
        copy_body,
        fork_body,
    })
}

/// (v0.6.4, tests only) Does the SET hold a closure witness for the
/// conflict copy of one loser op — recomputed from the log alone, the
/// same predicate the derivation applies? The monotonicity property
/// uses this as its judge: a copy that was open and is no longer must
/// have one.
///
/// # Errors
/// [`CellError::Db`] on SQL failure; [`CellError::NoteNotFound`] when
/// the op is no loser of any derivation.
pub fn witness_for_test(
    tx: &Transaction<'_>,
    original: &Ulid,
    loser_op: &Ulid,
) -> Result<bool, CellError> {
    let rows = scan(tx, original)?;
    let loser = rows
        .iter()
        .position(|r| r.op_id == *loser_op)
        .ok_or_else(|| CellError::NoteNotFound(loser_op.to_string()))?;
    let applied: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.body.is_some())
        .map(|(i, _)| i)
        .collect();
    let (winner, _) = winner_and_candidates(&rows, &applied);
    if winner.is_none() {
        return Ok(false);
    }
    let copy_id = material::conflict_copy_id(&rows[loser].hash, hlc_ms_of(&rows[loser].hlc));
    let ancestry = Ancestry::of(&rows);
    has_witness(tx, &rows, &ancestry, loser, &applied, &copy_id)
}

/// (v0.6.4, tests only) Does the CURRENT set still DESERVE a copy for
/// this loser — is it an applied body write that is neither the winner
/// nor one of its ancestors, with its filters passing? The monotonicity
/// property's judge: a copy that goes may close unwitnessed when the
/// set no longer deserves it (the purge, the lapide, a graph an
/// out-of-order arrival completed — the loser became the winner); a
/// DESERVED copy that goes without a witness is a defect.
///
/// # Errors
/// [`CellError::Db`] on SQL failure; [`CellError::NoteNotFound`] when
/// the op belongs to no derivation of the note.
pub fn deserved_for_test(
    tx: &Transaction<'_>,
    original: &Ulid,
    loser_op: &Ulid,
) -> Result<bool, CellError> {
    let rows = scan(tx, original)?;
    let loser = rows
        .iter()
        .position(|r| r.op_id == *loser_op)
        .ok_or_else(|| CellError::NoteNotFound(loser_op.to_string()))?;
    let Some(facts) = rows[loser].body.as_ref() else {
        // held by the lapide, or its payload is gone: not a tip at all
        return Ok(false);
    };
    let applied: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.body.is_some())
        .map(|(i, _)| i)
        .collect();
    let (winner, _) = winner_and_candidates(&rows, &applied);
    let Some(winner) = winner else {
        return Ok(false);
    };
    if loser == winner {
        return Ok(false);
    }
    let winner_past: std::collections::HashSet<usize> =
        ancestors_of(&rows, winner).into_iter().collect();
    if winner_past.contains(&loser) {
        // the winner's author had seen it: no copy, the text lives on
        return Ok(false);
    }
    // the filters (D1.5)
    if rows[winner]
        .body
        .as_ref()
        .is_some_and(|w| w.text == facts.text)
    {
        return Ok(false);
    }
    if let Some(fork) = nearest_common_ancestor(&rows, loser, winner)
        && body_at_fork(&rows, Some(fork)).as_deref() == Some(facts.text.as_str())
    {
        return Ok(false);
    }
    Ok(true)
}

/// The copy-id hook: an operation touched a COPY's id (a modification,
/// the resolving delete, a decision on its queue row) — the ORIGINAL's
/// derivation must see the witness.
pub(crate) fn rederive_original_of(tx: &Transaction<'_>, copy: &Ulid) -> Result<(), CellError> {
    let original: Option<String> = tx
        .query_row(
            "SELECT original_id FROM conflict_copies WHERE copy_id = ?1",
            [copy.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let original = match original {
        Some(original) => Ulid::from_string(&original)
            .map_err(|_| CellError::Db(format!("bad conflict original {original}")))?,
        // an ORPHAN copy op: the birth row does not exist HERE and the
        // note itself does not either (the op parked on a missing note —
        // a copy this device never derived, its loser was the winner
        // somewhere else). Only then is the walk over the cell's body
        // writes worth its cost (the id is blake3 of the loser, not
        // invertible): gated on the missing row, a fresh note never
        // pays it. Found by the oracle, seed 15192944291106605994
        // v0.6.6 (F3): a PURGED note's row is absent by the purge, not by
        // orphanhood — and that walk is provably empty for it. If it were
        // a copy, its birth row would still stand above (the purge's own
        // copy-walk deletes the birth rows of the copies it TAKES, not
        // the purged id's own) and the first branch would handle it; an
        // original's id cannot be minted from a loser's hash. Without the
        // gate every purge paid the whole-cell walk: 63 ms on the
        // 10 000-note probe, the bulk of the +46% the phase had to
        // attribute. No outcome moves — the hunt re-proves it.
        None if note_row_is_absent(tx, copy)? && !material::note_purged_in_set(tx, copy)? => {
            match original_of_copy(tx, copy)? {
                Some(original) => original,
                None => return Ok(()),
            }
        }
        None => return Ok(()),
    };
    rederive_note(tx, &original)
}

/// Does this note id have a notes row? (the orphan gate)
fn note_row_is_absent(tx: &Transaction<'_>, id: &Ulid) -> Result<bool, CellError> {
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM notes WHERE id = ?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    Ok(n == 0)
}

/// Which note's body writes mint this copy id, when no derivation row
/// says so.
fn original_of_copy(tx: &Transaction<'_>, copy: &Ulid) -> Result<Option<Ulid>, CellError> {
    let notes: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT oo2.object FROM op_objects oo2
             JOIN ops o ON o.op_id = oo2.op_id
             WHERE o.kind IN ('put_note', 'patch_note') AND oo2.object LIKE 'note:%'",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };
    for object in notes {
        let Some(id) = object.strip_prefix("note:") else {
            continue;
        };
        let Ok(original) = Ulid::from_string(id) else {
            continue;
        };
        let rows = scan(tx, &original)?;
        for row in rows {
            if row.body.is_none() {
                continue;
            }
            if material::conflict_copy_id(&row.hash, hlc_ms_of(&row.hlc)) == *copy {
                return Ok(Some(original));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rigorous diff3: identical changes are ONE change (never an
    /// overlap), disjoint changes compose, overlapping changes refuse.
    #[test]
    fn the_diff3_is_rigorous() {
        // only one side changed
        assert_eq!(
            diff3_merge("a\nb\nc", "a\nX\nc", "a\nb\nc"),
            Some("a\nX\nc".into())
        );
        // disjoint changes from both sides compose
        assert_eq!(
            diff3_merge("a\nb\nc\nd", "a\nX\nc\nd", "a\nb\nc\nY"),
            Some("a\nX\nc\nY".into())
        );
        // identical changes are one change, not an overlap
        assert_eq!(
            diff3_merge("a\nb\nc", "a\nX\nc", "a\nX\nc"),
            Some("a\nX\nc".into())
        );
        // an insert at the same point from both sides, different content:
        // the overlap
        assert_eq!(diff3_merge("a\nb", "a\nX\nb", "a\nY\nb"), None);
        // the append case of the REPORT: both append at the tail, the
        // regions collide — a descendant is not, by text alone, a
        // containment (which is why the witness has a causal arm)
        assert_eq!(diff3_merge("a", "a\nx", "a\nx\ny"), None);
        // the containment that closes a copy: the loser's middle-line
        // change, again and verbatim, plus more
        assert_eq!(
            diff3_merge(
                "alpha\nbeta\ngamma",
                "alpha\nX\ngamma\ndelta",
                "alpha\nX\ngamma"
            ),
            Some("alpha\nX\ngamma\ndelta".into())
        );
        // both replace the same line differently: the overlap
        assert_eq!(diff3_merge("a\nb\nc", "a\nX\nc", "a\nY\nc"), None);
        // empty base, equal sides: identical changes, one change
        assert_eq!(diff3_merge("", "x", "x"), Some("x".into()));
    }

    /// The class of the author: the three human shapes, and the prudent
    /// default for everything else.
    #[test]
    fn the_class_of_the_actor() {
        assert_eq!(classify_actor("user"), ActorClass::Human);
        assert_eq!(classify_actor("homun:user"), ActorClass::Human);
        assert_eq!(classify_actor("user@mcp:claude-desktop"), ActorClass::Human);
        assert_eq!(classify_actor("homun:bot-7"), ActorClass::Agent);
        assert_eq!(classify_actor("homun:story"), ActorClass::Agent);
        assert_eq!(classify_actor("mcp:claude-desktop"), ActorClass::Agent);
        assert_eq!(classify_actor("reviewer:strict"), ActorClass::Agent);
        assert_eq!(classify_actor("device-3"), ActorClass::Agent);
        assert_eq!(classify_actor("qualcos'altro"), ActorClass::Agent);
        // and the human wins the key comparison
        let human: (ActorClass, String, String) = (
            ActorClass::Human,
            "01AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0000".into(),
            "0".into(),
        );
        let agent: (ActorClass, String, String) = (
            ActorClass::Agent,
            "01ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ".into(),
            "9".into(),
        );
        assert!(human > agent, "the class outranks the clock");
    }
}
