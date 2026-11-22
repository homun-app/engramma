// SPDX-License-Identifier: Apache-2.0

//! v0.6.4 (D5, measurement — `--ignored`): what a DURABLE conflict copy
//! costs, and what a conflicted fleet costs against a quiet one.
//!
//! 1. **Bytes per copy**, from `dbstat`, for a 1 KB note and a 10 KB
//!    note: the cell is measured whole, then with N open copies, and
//!    the difference divides by N.
//! 2. **A fleet of 10 000 notes** with offline conflicts on the 1% and
//!    the 10% of the notes, against the same fleet with no conflicts:
//!    the file's growth in percent, and the time of ONE sync pass
//!    (everything the writers hold, delivered to a fresh copy).
//!
//! The numbers go in the REPORT; there is no assertion to fail — this
//! is a measure, not a promise.

use engramma_core::cell::ops::Op;
use engramma_core::format::{At, Note, NoteType};
use engramma_core::{Cell, Confidence, NotePatch, Status};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use time::OffsetDateTime;
use ulid::Ulid;

const K: u64 = 1_800_600_000_000;

fn now() -> At {
    At(OffsetDateTime::from_unix_timestamp(1_810_000_000).unwrap(/* fixed */))
}

fn note(id: u32, body: &str) -> Note {
    Note {
        id: Ulid::from_string(&format!("01{}{:08}", "0".repeat(16), id))
            .unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some("titolo della nota".into()),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: None,
        source: "bench".into(),
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

/// A cell whose clock reads a wall the bench MOVES between writes, so
/// the HLCs follow the script (who writes first, who is concurrent).
fn bench_cell(name: &str, dir: &std::path::Path) -> (Cell, Arc<AtomicU64>) {
    let cell = Cell::create(&dir.join(format!("{name}.sqlite")), "bench", "bench").unwrap(/* t */);
    let wall = Arc::new(AtomicU64::new(K));
    let w = Arc::clone(&wall);
    cell.inject_clock(engramma_core::hlc::HlcClock::new(
        format!("{name}000000000x"),
        move || w.load(Ordering::SeqCst),
    ))
    .unwrap(/* t */);
    (cell, wall)
}

/// The database's whole size, page by page (`dbstat`), in bytes.
fn db_bytes(cell: &Cell) -> u64 {
    cell.debug_column("SELECT CAST(SUM(pgsize) AS TEXT) FROM dbstat")
        .unwrap_or_default()
        .first()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or_default()
}

/// The cell file's size on disk (the WAL checkpointed first).
fn file_bytes(cell: &Cell) -> u64 {
    cell.debug_column("PRAGMA wal_checkpoint(TRUNCATE)").ok();
    std::fs::metadata(cell.file_path())
        .map(|m| m.len())
        .unwrap_or_default()
}

fn body_of(note_size: usize) -> String {
    let filler = "riga di corpo della nota del banco, lunga abbastanza da contare. ";
    let mut b = String::new();
    while b.len() < note_size {
        b.push_str(filler);
        b.push('\n');
    }
    b
}

/// v0.6.12 (L3): our own name, so a killed run's leftovers can be swept
/// (see tests/common/mod.rs).
fn test_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("engramma-test-")
        .tempdir()
        .expect(/* test-only */ "a fresh folder")
}

/// Measure one copy's bytes: a quiet cell, then the same cell with `n`
/// open copies, the difference over `n`.
#[test]
#[ignore = "a measurement, not an assertion"]
fn bytes_per_copy_1k_and_10k() {
    for (tag, note_size) in [("1 KB", 1_024_usize), ("10 KB", 10_240)] {
        let dir = test_dir();
        let (cell, wall) = bench_cell("m", dir.path());
        let body = body_of(note_size);
        let n = note(1, &body);
        let id = n.id;
        wall.store(K + 1, Ordering::SeqCst);
        cell.put_note(&n, "user").unwrap(/* t */);
        wall.store(K + 2, Ordering::SeqCst);
        // the human's word — the winner every copy will lose against
        let mut winner_body = body.clone();
        winner_body.push_str("e la frase della mano umana");
        cell.patch_note(
            &id,
            &NotePatch { body: Some(winner_body), ..Default::default() },
            "user",
        )
        .unwrap(/* t */);
        let before = db_bytes(&cell);

        // N concurrent agent writes, one per FRESH device (each took the
        // put and wrote from it — the same base the human winner had):
        // all lose to the human, all open their copy
        let copies = 20_u32;
        let put: Vec<Op> = cell
            .ops()
            .unwrap(/* t */)
            .into_iter()
            .filter(|o| o.kind.as_str() == "put_note")
            .collect();
        let mut losers: Vec<Op> = Vec::new();
        for i in 0..copies {
            let (dev, dev_wall) = bench_cell(&format!("d{i}"), dir.path());
            dev.ingest_op(&put[0]).unwrap(/* t */);
            dev_wall.store(K + 3 + u64::from(i), Ordering::SeqCst);
            let mut loser_body = body.clone();
            loser_body.push_str(&format!("e la mano dell'agente {i}"));
            dev.patch_note(
                &id,
                &NotePatch { body: Some(loser_body), ..Default::default() },
                "homun:bot-1",
            )
            .unwrap(/* t */);
            losers.push(
                dev.ops()
                    .unwrap(/* t */)
                    .into_iter()
                    .find(|o| o.kind.as_str() == "patch_note")
                    .expect(/* it just wrote one */ "the loser"),
            );
        }
        for op in &losers {
            cell.ingest_op(op).unwrap(/* t */);
        }
        let after = db_bytes(&cell);
        let per_copy = (after.saturating_sub(before)) / u64::from(copies);
        let open = cell
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE open = 1")
            .unwrap_or_default();
        eprintln!(
            "D5 {tag}: nota di {note_size} B — dbstat {before} → {after} B, \
             {} copie aperte, {} B per copia",
            open.first().map(String::as_str).unwrap_or("?"),
            per_copy
        );
    }
}

/// The fleet bench: 10 000 notes, offline conflicts on the 0%, 1% and
/// 10%, against the same fleet quiet. File growth and the time of one
/// full pass into a fresh copy.
#[test]
#[ignore = "a measurement, not an assertion"]
fn fleet_ten_thousand_notes_one_and_ten_percent() {
    let mut quiet_size = 0_u64;
    for (tag, conflicted) in [("0%", 0_u32), ("1%", 100), ("10%", 1000)] {
        let dir = test_dir();
        let (a, wall_a) = bench_cell("a", dir.path());
        let (b, wall_b) = bench_cell("b", dir.path());
        let mut ids: Vec<Ulid> = Vec::new();
        for i in 1..=10_000_u32 {
            let n = note(
                i,
                &format!("nota {i} del banco: il corpo di righe bastevoli"),
            );
            ids.push(n.id);
            a.put_note(&n, "user").unwrap(/* t */);
        }
        // the agent's concurrent word on `conflicted` notes, from the
        // same base (the writer B took A's put, then wrote without
        // seeing anything else)
        for op in a.ops().unwrap(/* t */).iter().skip(1) {
            b.ingest_op(op).unwrap(/* t */);
        }
        for (i, id) in ids.iter().enumerate() {
            if (i as u32) < conflicted {
                wall_b.store(K + 1 + i as u64, Ordering::SeqCst);
                b.patch_note(
                    id,
                    &NotePatch {
                        body: Some(format!(
                            "nota {} riscritta dall'agente in concorrenza, con qualche parola in più",
                            i + 1
                        )),
                        ..Default::default()
                    },
                    "homun:bot-1",
                )
                .unwrap(/* t */);
            }
        }
        // A's own word on the same notes — the human, older than the
        // agent's: the agent loses every conflict (D2), the copies open
        for (i, id) in ids.iter().enumerate() {
            if (i as u32) < conflicted {
                wall_a.store(K + 5_000 + i as u64, Ordering::SeqCst);
                a.patch_note(
                    id,
                    &NotePatch {
                        body: Some(format!(
                            "nota {} ritoccata dalla mano umana, che l'agente non aveva visto",
                            i + 1
                        )),
                        ..Default::default()
                    },
                    "user",
                )
                .unwrap(/* t */);
            }
        }
        let _ = wall_a;
        // one full pass into a fresh copy: everything both hold
        let (fresh, _fw) = bench_cell("f", dir.path());
        let started = Instant::now();
        let mut union: Vec<Op> = Vec::new();
        for op in a.ops().unwrap(/* t */).into_iter().skip(1) {
            union.push(op);
        }
        let held: std::collections::HashSet<String> =
            union.iter().map(|o| o.content_hash.clone()).collect();
        for op in b.ops().unwrap(/* t */) {
            if !held.contains(&op.content_hash) {
                union.push(op);
            }
        }
        union.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
        for op in &union {
            fresh.ingest_op(op).unwrap(/* t */);
        }
        let pass = started.elapsed();
        let size = file_bytes(&fresh);
        let copies = fresh
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE open = 1")
            .unwrap_or_default();
        let growth = if quiet_size == 0 {
            None
        } else {
            Some(100.0 * (size as f64 / quiet_size as f64 - 1.0))
        };
        eprintln!(
            "D5 flotta {tag}: file {size} B ({} B/nota{}, copie aperte {}, \
             passata {:.2} s)",
            size / 10_000,
            growth
                .map(|g| format!(", +{g:.1}% sul quieto"))
                .unwrap_or_default(),
            copies.first().map(String::as_str).unwrap_or("?"),
            pass.as_secs_f64()
        );
        if quiet_size == 0 {
            quiet_size = size;
        }
    }
}

/// v0.6.5 (E3, measurement — `--ignored`): the rederivation cost on a
/// note with TEN open copies and FIFTY subsequent body writes — the
/// product (losers × subsequent writes) the containment loop pays at
/// every pass. The number goes in the REPORT; the containment cache
/// exists only if this demands it.
#[test]
#[ignore = "a measurement, not an assertion"]
fn rederivation_with_ten_open_copies_and_fifty_writes() {
    let dir = test_dir();
    let (cell, wall) = bench_cell("r", dir.path());
    let body = "prima riga\nil corpo condiviso della nota del banco\nterza riga";
    let n = note(41, body);
    let id = n.id;
    wall.store(K + 1, Ordering::SeqCst);
    cell.put_note(&n, "user").unwrap(/* t */);
    let put: Vec<Op> = cell
        .ops()
        .unwrap(/* t */)
        .into_iter()
        .filter(|o| o.kind.as_str() == "put_note")
        .collect();
    // ten concurrent losers, one per fresh device (each saw only the put)
    let mut losers: Vec<Op> = Vec::new();
    for i in 0..10_u32 {
        let (dev, dev_wall) = bench_cell(&format!("l{i}"), dir.path());
        dev.ingest_op(&put[0]).unwrap(/* t */);
        dev_wall.store(K + 2 + u64::from(i), Ordering::SeqCst);
        dev.patch_note(
            &id,
            &NotePatch {
                body: Some(format!("prima riga\nla mano dell'agente {i}\nterza riga")),
                ..Default::default()
            },
            "homun:bot-1",
        )
        .unwrap(/* t */);
        losers.push(
            dev.ops()
                .unwrap(/* t */)
                .into_iter()
                .find(|o| o.kind.as_str() == "patch_note")
                .expect(/* it just wrote one */ "the loser"),
        );
    }
    for op in &losers {
        cell.ingest_op(op).unwrap(/* t */);
    }
    // the human winner: concurrent to every loser — written on a device
    // that saw ONLY the put (the older human hand that wins, D2)
    let (h, hw) = bench_cell("h", dir.path());
    h.ingest_op(&put[0]).unwrap(/* t */);
    hw.store(K + 40, Ordering::SeqCst);
    let human = {
        h.patch_note(
            &id,
            &NotePatch {
                body: Some("prima riga\nla mano umana che vince\nterza riga".into()),
                ..Default::default()
            },
            "user",
        )
        .unwrap(/* t */);
        h.ops()
            .unwrap(/* t */)
            .into_iter()
            .find(|o| o.kind.as_str() == "patch_note")
            .expect(/* it just wrote one */ "the human winner")
    };
    cell.ingest_op(&human).unwrap(/* t */);
    let open = cell
        .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE open = 1")
        .unwrap_or_default();
    eprintln!(
        "E3 banco: copie aperte {}",
        open.first().cloned().unwrap_or_default()
    );
    // fifty SEQUENTIAL body writes on the winner's line, each applied here
    for i in 0..50_u32 {
        wall.store(K + 100 + u64::from(i), Ordering::SeqCst);
        cell.patch_note(
            &id,
            &NotePatch {
                body: Some(format!("prima riga\nla mano umana che vince, volta {i}\nterza riga")),
                ..Default::default()
            },
            "user",
        )
        .unwrap(/* t */);
    }
    // the measure: ONE more local write — the derivation it triggers
    let measure_once = |tag: &str| {
        wall.store(K + 500 + (tag.len() as u64), Ordering::SeqCst);
        let started = Instant::now();
        cell.patch_note(
            &id,
            &NotePatch {
                body: Some("prima riga\nla mano umana che vince, l'ultima\nterza riga".into()),
                ..Default::default()
            },
            "user",
        )
        .unwrap(/* t */);
        eprintln!(
            "E3 banco: una riderivazione, {tag}: {:?}",
            started.elapsed()
        );
    };
    measure_once("10 copie e 50 scritture");
    for i in 50..100_u32 {
        wall.store(K + 100 + u64::from(i), Ordering::SeqCst);
        cell.patch_note(
            &id,
            &NotePatch {
                body: Some(format!("prima riga\nla mano umana che vince, volta {i}\nterza riga")),
                ..Default::default()
            },
            "user",
        )
        .unwrap(/* t */);
    }
    measure_once("10 copie e 100 scritture");
    // and the whole-history rebuild, for scale
    let all: Vec<Op> = cell.ops().unwrap(/* t */);
    let (fresh, _fw) = bench_cell("f2", dir.path());
    let started = Instant::now();
    for op in &all {
        fresh.ingest_op(op).unwrap(/* t */);
    }
    eprintln!(
        "E3 banco: la flotta intera rigiocata da zero: {:?}",
        started.elapsed()
    );
}

/// v0.6.5 (E6, measurement — `--ignored`): the cost of the copies ALONE.
/// The conflicted fleet and the sequential one write THE SAME operations
/// — a put and two body patches per note — but the sequential one
/// delivers A's write to B BEFORE B writes: no conflict, no copy. The
/// difference between the two files is the copy's cost, and only it.
/// Table by table (`dbstat`), for a 70 B, a 1 KB and a 10 KB note; the
/// saving that passes a fifth of the copy's cost, with the numbers
/// before and after.
#[test]
#[ignore = "a measurement, not an assertion"]
fn d5_the_copies_alone_table_by_table() {
    for (tag, note_size) in [("70 B", 70_usize), ("1 KB", 1_024), ("10 KB", 10_240)] {
        let body = body_of(note_size);
        // --- the conflicted fleet: put, then two concurrent patches
        let dir = test_dir();
        let (a, wall_a) = bench_cell("a", dir.path());
        let (b, wall_b) = bench_cell("b", dir.path());
        let n = note(61, &body);
        let id = n.id;
        wall_a.store(K + 1, Ordering::SeqCst);
        a.put_note(&n, "user").unwrap(/* t */);
        for op in a.ops().unwrap(/* t */).iter().skip(1) {
            b.ingest_op(op).unwrap(/* t */);
        }
        let mut winner_body = body.clone();
        winner_body.push_str("\ne la frase della mano umana");
        wall_a.store(K + 2, Ordering::SeqCst);
        a.patch_note(
            &id,
            &NotePatch { body: Some(winner_body.clone()), ..Default::default() },
            "user",
        )
        .unwrap(/* t */);
        let mut loser_body = body.clone();
        loser_body.push_str("\ne la mano dell'agente");
        wall_b.store(K + 3, Ordering::SeqCst);
        b.patch_note(
            &id,
            &NotePatch { body: Some(loser_body), ..Default::default() },
            "homun:bot-1",
        )
        .unwrap(/* t */);
        // --- the sequential fleet: the SAME ops, but B sees A's first
        let (a2, wall_a2) = bench_cell("a2", dir.path());
        let (b2, wall_b2) = bench_cell("b2", dir.path());
        wall_a2.store(K + 1, Ordering::SeqCst);
        a2.put_note(&n, "user").unwrap(/* t */);
        for op in a2.ops().unwrap(/* t */).iter().skip(1) {
            b2.ingest_op(op).unwrap(/* t */);
        }
        wall_a2.store(K + 2, Ordering::SeqCst);
        a2.patch_note(
            &id,
            &NotePatch { body: Some(winner_body.clone()), ..Default::default() },
            "user",
        )
        .unwrap(/* t */);
        for op in a2.ops().unwrap(/* t */).iter().skip(2) {
            b2.ingest_op(op).unwrap(/* t */);
        }
        wall_b2.store(K + 3, Ordering::SeqCst);
        b2.patch_note(
            &id,
            &NotePatch {
                body: Some(format!("{}\ne la mano dell'agente, dopo", body)),
                ..Default::default()
            },
            "homun:bot-1",
        )
        .unwrap(/* t */);
        // both settle into a fresh copy of the union, in total order
        let union_of = |x: &Cell, y: &Cell| -> Vec<Op> {
            let mut u: Vec<Op> = x.ops().unwrap(/* t */);
            u.extend(y.ops().unwrap(/* t */));
            u.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
            let mut seen = std::collections::BTreeMap::new();
            for op in u {
                seen.insert(op.op_id.to_string(), op);
            }
            seen.into_values().collect()
        };
        let (conflicted, _fc) = bench_cell("fc", dir.path());
        for op in &union_of(&a, &b) {
            conflicted.ingest_op(op).unwrap(/* t */);
        }
        let (sequential, _fs) = bench_cell("fs", dir.path());
        for op in &union_of(&a2, &b2) {
            sequential.ingest_op(op).unwrap(/* t */);
        }
        let copies = conflicted
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE open = 1")
            .unwrap_or_default();
        let sequential_copies = sequential
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies")
            .unwrap_or_default();
        let tables_of = |cell: &Cell| -> Vec<(String, u64)> {
            cell.debug_column(
                "SELECT name || char(1) || SUM(pgsize) FROM dbstat GROUP BY name ORDER BY name",
            )
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let (nb, bytes) = row.split_once('\u{1}')?;
                Some((nb.to_string(), bytes.parse().ok()?))
            })
            .collect()
        };
        let ct = tables_of(&conflicted);
        let st = tables_of(&sequential);
        let total_c: u64 = ct.iter().map(|(_, v)| v).sum();
        let total_s: u64 = st.iter().map(|(_, v)| v).sum();
        eprintln!(
            "D6 {tag}: copie {} (la sequenziale {})",
            copies.first().map(String::as_str).unwrap_or("?"),
            sequential_copies.first().map(String::as_str).unwrap_or("?")
        );
        eprintln!(
            "D6 {tag}: dbstat tot {total_c} B (sequenziale {total_s} B, differenza {} B = la copia)",
            total_c.saturating_sub(total_s)
        );
        for (name, bytes_c) in &ct {
            let bytes_s = st
                .iter()
                .find(|(nb, _)| nb == name)
                .map(|(_, v)| *v)
                .unwrap_or(0);
            if *bytes_c != bytes_s {
                eprintln!(
                    "D6 {tag}: tabella {name}: {bytes_s} → {bytes_c} B (+{})",
                    bytes_c.saturating_sub(bytes_s)
                );
            }
        }
        // the saving that passes a fifth of the copy's cost: the body
        // back in the derivation row, before and after
        let state_len = state_len_with_body(&conflicted);
        let lean: u64 = conflicted
            .debug_column("SELECT CAST(SUM(LENGTH(state_json)) AS TEXT) FROM conflict_copies")
            .unwrap_or_default()
            .first()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        eprintln!(
            "D6 {tag}: la riga di derivazione è {lean} B senza corpo, ~{state_len} B col corpo (il risparmio D5)"
        );
    }
}

/// The stored derivation rows' length with the body back in, computed
/// from the rows themselves (the counterfactual of the D5 saving).
fn state_len_with_body(cell: &Cell) -> u64 {
    let rows: Vec<String> = cell
        .debug_column(
            "SELECT state_json || char(1) || COALESCE(json_extract(p.body, '$.set.body'), '') \
             FROM conflict_copies cc LEFT JOIN op_payloads p ON p.op_id = cc.loser_op",
        )
        .unwrap_or_default();
    rows.into_iter()
        .map(|row| {
            row.split_once('\u{1}')
                .map(|(sj, b)| sj.len() as u64 + b.len() as u64 + 3)
                .unwrap_or(0)
        })
        .sum()
}

/// v0.6.5 (E6, measurement — `--ignored`): the file-growth threshold. A
/// fleet of 10 000 notes with the 10% conflicted, against the SAME
/// fleet sequential: the derived rows of the copies may grow the file
/// by at most the 15% — the number the REPORT carries.
#[test]
#[ignore = "a measurement, not an assertion"]
fn d6_ten_percent_conflicted_against_sequential() {
    let n_notes = 10_000_u32;
    let conflicted_n = 1_000_u32;
    let body = body_of(70);
    let mut sizes: Vec<(u64, f64)> = Vec::new();
    for sequential_flavor in [false, true] {
        let dir = test_dir();
        let (a, wall_a) = bench_cell("a", dir.path());
        let (b, wall_b) = bench_cell("b", dir.path());
        let mut ids: Vec<Ulid> = Vec::new();
        for i in 1..=n_notes {
            let n = note(i, &format!("nota {i} del banco: {body}"));
            ids.push(n.id);
            wall_a.store(K + i as u64, Ordering::SeqCst);
            a.put_note(&n, "user").unwrap(/* t */);
        }
        for op in a.ops().unwrap(/* t */).iter().skip(1) {
            b.ingest_op(op).unwrap(/* t */);
        }
        for (i, id) in ids.iter().enumerate() {
            if (i as u32) >= conflicted_n {
                break;
            }
            wall_a.store(K + 100_000 + i as u64, Ordering::SeqCst);
            a.patch_note(
                id,
                &NotePatch {
                    body: Some(format!("nota {} ritoccata dalla mano umana", i + 1)),
                    ..Default::default()
                },
                "user",
            )
            .unwrap(/* t */);
            if sequential_flavor {
                // B sees A's word BEFORE writing its own: no conflict
                let patch = a
                    .ops()
                    .unwrap(/* t */)
                    .into_iter()
                    .rev()
                    .find(|o| o.kind.as_str() == "patch_note")
                    .expect(/* just written */ "the human patch");
                b.ingest_op(&patch).unwrap(/* t */);
            }
            wall_b.store(K + 200_000 + i as u64, Ordering::SeqCst);
            b.patch_note(
                id,
                &NotePatch {
                    body: Some(format!(
                        "nota {} riscritta dall'agente{}",
                        i + 1,
                        if sequential_flavor { ", dopo" } else { ", in concorrenza" }
                    )),
                    ..Default::default()
                },
                "homun:bot-1",
            )
            .unwrap(/* t */);
        }
        let (fresh, _fw) = bench_cell("f", dir.path());
        let mut union: Vec<Op> = a.ops().unwrap(/* t */);
        let held: std::collections::HashSet<String> =
            union.iter().map(|o| o.content_hash.clone()).collect();
        for op in b.ops().unwrap(/* t */) {
            if !held.contains(&op.content_hash) {
                union.push(op);
            }
        }
        union.sort_by(|p, q| p.hlc.cmp(&q.hlc).then(p.op_id.cmp(&q.op_id)));
        let started = Instant::now();
        for op in &union {
            fresh.ingest_op(op).unwrap(/* t */);
        }
        let pass = started.elapsed();
        let size = file_bytes(&fresh);
        let copies = fresh
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM conflict_copies WHERE open = 1")
            .unwrap_or_default();
        eprintln!(
            "D6 flotta {} ({} conflitti): file {size} B ({} B/nota), copie {}, passata {:.2} s",
            if sequential_flavor {
                "sequenziale"
            } else {
                "in conflitto"
            },
            if sequential_flavor { 0 } else { conflicted_n },
            size / u64::from(n_notes),
            copies.first().map(String::as_str).unwrap_or("?"),
            pass.as_secs_f64()
        );
        sizes.push((size, pass.as_secs_f64()));
    }
    if sizes.len() == 2 {
        let growth = 100.0 * (sizes[0].0 as f64 / sizes[1].0 as f64 - 1.0);
        eprintln!("D6 soglia: crescita al 10% di note in conflitto: {growth:.1}% (soglia 15%)");
    }
}
