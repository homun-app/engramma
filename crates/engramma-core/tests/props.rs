// SPDX-License-Identifier: Apache-2.0

//! Property tests (§13.8): format round trip, replay determinism,
//! export→import equality, HLC monotonicity.

use std::path::Path;

use engramma_core::cell::ops::OpKind;
use engramma_core::format::yaml::YamlValue;
use engramma_core::format::{At, Lang, Note, NoteType, ParseMode, Validity};
use engramma_core::hlc::{Hlc, HlcClock};
use engramma_core::port::{export, import_cell};
use engramma_core::{Cell, Confidence, NotePatch, Status};
use proptest::prelude::*;
use time::OffsetDateTime;
use ulid::Ulid;

fn ts(secs: i64) -> At {
    At(OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs)
        .unwrap(/* test-only invariant: range checked by strategy */)
        .replace_nanosecond(0)
        .unwrap(/* test-only invariant: no nanos */))
}

fn ulid(n: u32) -> Ulid {
    // counter at the FRONT: the first 8 characters (used in export file
    // names) must differ between notes
    let mut s = format!("01P{n}");
    while s.len() < 26 {
        s.push('0');
    }
    Ulid::from_string(&s).unwrap(/* test-only: valid Crockford digits */)
}

fn any_tag() -> impl Strategy<Value = String> {
    (r"[a-z0-9_]{1,8}", proptest::option::of(r"[a-z0-9_]{1,8}")).prop_map(|(a, b)| match b {
        Some(b) => format!("{a}/{b}"),
        None => a,
    })
}

fn any_ext_value() -> impl Strategy<Value = YamlValue> {
    let leaf = prop_oneof![
        Just(YamlValue::Null),
        any::<bool>().prop_map(YamlValue::Bool),
        (-1000i64..1000).prop_map(YamlValue::Int),
        r"[a-zA-Z0-9 .,;:àèéìòù]{0,20}".prop_map(YamlValue::Str),
    ];
    leaf.prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..3).prop_map(YamlValue::Seq),
            proptest::collection::vec((r"x-[a-z]{1,6}".prop_map(String::from), inner), 0..3)
                .prop_map(YamlValue::Map),
        ]
    })
}

fn any_note() -> impl Strategy<Value = Note> {
    (
        0u32..1000,
        proptest::sample::select(Vec::from(NoteType::ALL)),
        proptest::option::of(r"[A-Za-zÀ-ÿ0-9 .,'-]{0,40}"),
        // body: printable, multi-line, with links and tags sprinkled
        proptest::collection::vec(
            prop_oneof![
                r"[a-zA-Zàèéìòù0-9 ,.;:'()-]{1,60}",
                Just("[[Milano]]".to_string()),
                Just("[[Fabio|F.]]".to_string()),
                Just("#casa/milano".to_string()),
                Just("#lavoro".to_string()),
            ],
            0..6,
        ),
        proptest::collection::vec(any_tag(), 0..3),
        proptest::option::of(r"[a-z]{2,10}"),
        r"[a-zA-Z0-9:/._-]{1,30}",
        proptest::sample::select(Vec::from([Confidence::Stated, Confidence::Inferred])),
        proptest::option::of((1000u32..2000, 1000u32..2000)),
        0u32..100,
        1000u64..2000,
        proptest::collection::vec(
            (r"x-[a-z]{1,6}".prop_map(String::from), any_ext_value()),
            0..2,
        ),
    )
        .prop_map(
            |(
                n,
                note_type,
                title,
                body_lines,
                tags,
                lang,
                source,
                confidence,
                validity,
                sup,
                t,
                ext,
            )| {
                let (vf, vt) = match validity {
                    Some((from, to)) => {
                        let f = Validity::parse(&format!(
                            "20{:02}-{:02}-{:02}",
                            20 + from / 300,
                            1 + from % 12,
                            1 + from % 28
                        ))
                        .unwrap_or_else(
                            |_| Validity::parse("2024-01-01").unwrap(/* fixed fallback */),
                        );
                        let t2 = Validity::parse(&format!(
                            "20{:02}-{:02}-{:02}",
                            20 + to / 300,
                            1 + to % 12,
                            1 + to % 28
                        ))
                        .unwrap_or_else(
                            |_| Validity::parse("2025-01-01").unwrap(/* fixed fallback */),
                        );
                        (Some(f), Some(t2))
                    }
                    None => (None, None),
                };
                Note {
                    id: ulid(n),
                    note_type,
                    title,
                    body: body_lines.join("\n"),
                    tags,
                    scope: None,
                    lang: lang.and_then(|l| Lang::parse(&l).ok()),
                    source,
                    confidence,
                    status: Status::Active,
                    valid_from: vf,
                    valid_to: vt,
                    supersedes: vec![ulid(sup)],
                    created_at: ts(t as i64),
                    updated_at: ts(t as i64),
                    extensions: ext,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `parse(serialize(n)) == n` for every generated note.
    #[test]
    fn format_round_trip(note in any_note()) {
        let text = note.serialize().unwrap(/* strategy keeps fields valid */);
        let parsed = engramma_core::format::parse(&text, ParseMode::Strict).unwrap(/* serialized notes are valid */);
        prop_assert_eq!(parsed.note, note.clone());
        // stability: serialize(parse(serialize(x))) is a fixed point
        let s1 = note.serialize().unwrap(/* as above */);
        let parsed2 = engramma_core::format::parse(&s1, ParseMode::Strict).unwrap(/* as above */);
        let s2 = parsed2.note.serialize().unwrap(/* as above */);
        prop_assert_eq!(s1, s2);
    }

    /// Replaying the log rebuilds exactly the same state.
    #[test]
    fn replay_equals_state(seed in any::<u64>()) {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("c.sqlite"), "p", "prop").unwrap(/* test-only */);
        run_random_ops(&cell, seed, ops_count(seed));
        let report = cell.verify().unwrap(/* test-only */);
        prop_assert!(report.ok(), "verify failed: {:?}", report.mismatch);
    }

    /// Export then import into a fresh cell gives the same notes and edges.
    #[test]
    fn export_import_equality(seed in any::<u64>()) {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("c.sqlite"), "p", "prop").unwrap(/* test-only */);
        run_random_ops(&cell, seed, ops_count(seed));
        // close open-ended state: delete nothing, just export
        let out = dir.path().join("exp");
        export(&cell, &out).unwrap(/* test-only */);
        let cell2 = Cell::create(&dir.path().join("c2.sqlite"), "p", "prop").unwrap(/* test-only */);
        let summary = import_cell(&cell2, &out, "prop").unwrap(/* export folders import cleanly */);
        prop_assert!(summary.errors.is_empty(), "{summary:?}");
        // compare notes and edges
        let dump = |c: &Cell, sql: &str| -> Vec<String> {
            c.debug_column(sql).unwrap(/* test-only */)
        };
        // Notes compared field by field through the API; `updated_at` is
        // excluded: importing a confirmed `external` note replays a `confirm`
        // op, which stamps its own (import-time) `updated_at`.
        let ids = dump(
            &cell,
            "SELECT id FROM notes WHERE status != 'deleted' ORDER BY id",
        );
        prop_assert_eq!(
            ids.len(),
            dump(&cell2, "SELECT id FROM notes WHERE status != 'deleted'").len()
        );
        for id in &ids {
            let a = cell
                .get(&Ulid::from_string(id).unwrap(/* from our own column */))
                .unwrap(/* test-only */);
            let b = cell2
                .get(&Ulid::from_string(id).unwrap(/* from our own column */))
                .unwrap(/* test-only */);
            prop_assert_eq!(a.note_type, b.note_type);
            prop_assert_eq!(a.title, b.title);
            prop_assert_eq!(a.body, b.body);
            prop_assert_eq!(a.tags, b.tags);
            prop_assert_eq!(a.scope, b.scope);
            prop_assert_eq!(a.lang, b.lang);
            prop_assert_eq!(a.source, b.source);
            prop_assert_eq!(a.confidence, b.confidence);
            prop_assert_eq!(a.status, b.status);
            prop_assert_eq!(a.valid_from, b.valid_from);
            prop_assert_eq!(a.valid_to, b.valid_to);
            prop_assert_eq!(a.supersedes, b.supersedes);
            prop_assert_eq!(a.created_at, b.created_at);
            prop_assert_eq!(a.extensions, b.extensions);
        }
        // Edges of living notes: tombstones stay out of the export, so their
        // (deleted-status) edges are not part of the exported state.
        let edge_sql = "SELECT e.id || e.src || e.rel || e.dst FROM edges e                         JOIN notes n ON e.note_id = n.id AND n.status != 'deleted'                         ORDER BY e.id";
        let edges1 = dump(&cell, edge_sql);
        let edges2 = dump(&cell2, edge_sql);
        prop_assert_eq!(edges1, edges2);
    }

    /// The HLC is monotonic under arbitrary physical-clock readings,
    /// including jumps backwards.
    #[test]
    fn hlc_monotonic(clock_readings in proptest::collection::vec(0u64..1_000_000, 1..100)) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        let t = Arc::new(AtomicU64::new(0));
        let tc = Arc::clone(&t);
        let mut clock = HlcClock::new("n1", move || tc.load(Ordering::Relaxed));
        let mut prev: Option<Hlc> = None;
        for r in clock_readings {
            t.store(r, Ordering::Relaxed);
            let h = clock.tick();
            if let Some(p) = prev {
                prop_assert!(h > p, "hlc went backwards: {p:?} -> {h:?}");
            }
            prev = Some(h);
        }
    }
}

/// How many ops a property case runs: 40–60, mixed by seed.
fn ops_count(seed: u64) -> u16 {
    u16::try_from(40 + (seed % 21)).unwrap_or(40)
}

/// A deterministic pseudo-random op sequence (xorshift). The target note is
/// drawn at random among the known ones — pinning it to `notes.first()` let
/// one delete silence almost every later op — and every OpKind gets a chance
/// to fire. Returns how many ops of each kind **succeeded**.
fn run_random_ops(cell: &Cell, seed: u64, n: u16) -> std::collections::HashMap<OpKind, usize> {
    let mut s = seed | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut notes: Vec<Ulid> = Vec::new();
    let mut quarantined: Vec<Ulid> = Vec::new(); // non-external, via set_status
    let mut counts: std::collections::HashMap<OpKind, usize> = std::collections::HashMap::new();
    let draw = |pool: &[Ulid], rnd: u64| -> Option<Ulid> {
        if pool.is_empty() {
            None
        } else {
            pool.get((rnd % pool.len() as u64) as usize).copied()
        }
    };
    for i in 0..n {
        let choice = next() % 11;
        let fresh = any_note_of(next(), i);
        match choice {
            0..=2 => {
                if cell.put_note(&fresh, "prop").is_ok() {
                    notes.push(fresh.id);
                    *counts.entry(OpKind::PutNote).or_insert(0) += 1;
                }
            }
            3 => {
                if let Some(id) = draw(&notes, next()) {
                    let patch = NotePatch {
                        body: Some(format!("corpo {}", next() % 100)),
                        ..Default::default()
                    };
                    if cell.patch_note(&id, &patch, "prop").is_ok() {
                        *counts.entry(OpKind::PatchNote).or_insert(0) += 1;
                    }
                }
            }
            4 => {
                if let Some(id) = draw(&notes, next()) {
                    let mut new = any_note_of(next(), i + 1000);
                    new.supersedes = vec![id];
                    if cell.supersede(&id, &new, None, "prop").is_ok() {
                        notes.push(new.id);
                        *counts.entry(OpKind::Supersede).or_insert(0) += 1;
                    }
                }
            }
            5 => {
                if let Some(id) = draw(&notes, next())
                    && cell.delete_note(&id, "prop").is_ok()
                {
                    *counts.entry(OpKind::DeleteNote).or_insert(0) += 1;
                }
            }
            6 => {
                let mut ext = any_note_of(next(), i + 2000);
                ext.confidence = Confidence::External;
                ext.status = Status::Quarantined;
                ext.source = "import:web".into();
                if cell.put_note(&ext, "prop").is_ok() {
                    *counts.entry(OpKind::PutNote).or_insert(0) += 1;
                    if cell.confirm(&ext.id, "prop").is_ok() {
                        notes.push(ext.id);
                        *counts.entry(OpKind::Confirm).or_insert(0) += 1;
                    }
                }
            }
            7 => {
                let edge = engramma_core::cell::EdgeInput {
                    src: "Alfa".into(),
                    rel: "rel_x".into(),
                    dst: "Beta".into(),
                    note_id: None,
                    valid_from: None,
                    valid_to: None,
                    confidence: Confidence::Stated,
                };
                if let Ok(edge_id) = cell.link(&edge, "prop") {
                    *counts.entry(OpKind::Link).or_insert(0) += 1;
                    if cell.unlink(&edge_id, "prop").is_ok() {
                        *counts.entry(OpKind::Unlink).or_insert(0) += 1;
                    }
                }
            }
            8 => {
                // restore a version that exists, on a random note
                if let Some(id) = draw(&notes, next())
                    && let Ok(history) = cell.history(&id)
                    && !history.is_empty()
                {
                    let v = history[(next() as usize) % history.len()].version;
                    if cell.restore(&id, v, "prop").is_ok() {
                        *counts.entry(OpKind::Restore).or_insert(0) += 1;
                    }
                }
            }
            9 => {
                // quarantine a random non-external note (never via put)
                if let Some(id) = draw(&notes, next()) {
                    let is_external = cell
                        .get(&id)
                        .map(|n| n.confidence == Confidence::External)
                        .unwrap_or(true);
                    let already = quarantined.contains(&id);
                    if !is_external
                        && !already
                        && cell.set_status(&id, Status::Quarantined, "prop").is_ok()
                    {
                        quarantined.push(id);
                        *counts.entry(OpKind::SetStatus).or_insert(0) += 1;
                    }
                }
            }
            _ => {
                // release a quarantined (non-external) note back to active
                if let Some(id) = quarantined.pop() {
                    let restored = cell.set_status(&id, Status::Active, "prop").is_ok();
                    if restored {
                        *counts.entry(OpKind::SetStatus).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    counts
}

#[test]
fn random_ops_cover_every_kind() {
    // 256 seeds; count the SUCCEEDED ops per kind: each must fire at least
    // 20 times, so no branch of the generator silently rots.
    let mut totals = std::collections::HashMap::<OpKind, usize>::new();
    for seed in 0..256u64 {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("c.sqlite"), "p", "prop").unwrap(/* t */);
        // the cell's own creation is the init op
        *totals.entry(OpKind::Init).or_insert(0) += 1;
        for (kind, n) in run_random_ops(&cell, seed, ops_count(seed)) {
            *totals.entry(kind).or_insert(0) += n;
        }
    }
    let mut missing = Vec::new();
    for kind in OpKind::ALL {
        let got = totals.get(&kind).copied().unwrap_or(0);
        if got < 20 {
            missing.push(format!("{kind:?}: {got}"));
        }
    }
    assert!(
        missing.is_empty(),
        "op kinds under 20 successes over 256 seeds: {}",
        missing.join(", ")
    );
}

fn any_note_of(seed: u64, i: u16) -> Note {
    let bodies = [
        "Nota su [[Milano]] e [[Fabio]].",
        "Fatto con #tag/uno e [[Roma|capitale]].",
        "Preferenza: caffè amaro.",
        "## Relations\n- lives_in:: [[Roma]]\n",
        "## Relations\n- works_at:: [[Acme]] (from 2020-01-01 to 2024-12-31)\n",
    ];
    let types = [
        NoteType::Fact,
        NoteType::Person,
        NoteType::Preference,
        NoteType::Note,
    ];
    let idx = (seed % 5) as usize;
    let t = (seed % 4) as usize;
    let mut note = Note {
        id: {
            let mut s = format!("01P{}{}", seed % 100_000, i);
            while s.len() < 26 {
                s.push('0');
            }
            Ulid::from_string(&s).unwrap(/* valid digits */)
        },
        note_type: types[t],
        title: Some(format!("Titolo {i} {}", seed % 7)),
        body: bodies[idx].to_string(),
        tags: vec!["gen/prop".into()],
        scope: None,
        lang: Some(Lang::It),
        source: "prop".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: ts((seed % 10_000) as i64),
        updated_at: ts((seed % 10_000) as i64),
        extensions: vec![("x-s".into(), YamlValue::Int((seed % 100) as i64))],
    };
    if note.note_type == NoteType::Person {
        note.title = Some(format!("Persona{}", seed % 9));
    }
    note
}

#[test]
fn spec_examples_valid_and_invalid() {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/examples");
    let valid = spec.join("valid");
    let mut n = 0;
    for entry in std::fs::read_dir(&valid).unwrap(/* test-only */) {
        let entry = entry.unwrap(/* test-only */);
        let text = std::fs::read_to_string(entry.path()).unwrap(/* test-only */);
        engramma_core::format::parse(&text, ParseMode::Strict)
            .unwrap_or_else(|e| panic!("{} should be valid: {e}", entry.path().display()));
        n += 1;
    }
    assert!(n >= 4, "expected at least 4 valid examples, found {n}");

    let invalid = spec.join("invalid");
    let mut m = 0;
    for entry in std::fs::read_dir(&invalid).unwrap(/* test-only */) {
        let entry = entry.unwrap(/* test-only */);
        let text = std::fs::read_to_string(entry.path()).unwrap(/* test-only */);
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            engramma_core::format::parse(&text, ParseMode::Strict).is_err(),
            "{} should be invalid",
            name
        );
        m += 1;
    }
    assert!(m >= 8, "expected at least 8 invalid examples, found {m}");
}
