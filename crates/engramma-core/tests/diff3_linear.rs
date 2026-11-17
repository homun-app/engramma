// SPDX-License-Identifier: Apache-2.0

//! v0.6.5 (E3): the diff3 is LINEAR in space and bounded in cost. The
//! red is the reviewer's R3 — a 20 000-line containment costs seconds
//! and gigabytes under the quadratic table — and the property keeps the
//! new alignment deciding EXACTLY as the v0.6.4 one on short texts,
//! repeats and all.

use engramma_core::cell::conflicts::diff3_merge;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// the v0.6.4 reference, frozen here: the quadratic table the fleet ran
// with for the whole phase — the equality property judges against it
// ---------------------------------------------------------------------------

fn lcs_matches_v064(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
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
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn merge_chunk_v064<'a>(
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

fn diff3_merge_v064(base: &str, a: &str, b: &str) -> Option<String> {
    let fb: Vec<&str> = base.split('\n').collect();
    let fa: Vec<&str> = a.split('\n').collect();
    let fbb: Vec<&str> = b.split('\n').collect();
    let ma = lcs_matches_v064(&fb, &fa);
    let mb = lcs_matches_v064(&fb, &fbb);
    let mut out: Vec<&str> = Vec::new();
    let mut bi = 0_usize;
    let mut ai = 0_usize;
    let mut ci = 0_usize;
    let mut ia = 0_usize;
    let mut ib = 0_usize;
    loop {
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
            return merge_chunk_v064(&fb[bi..], &fa[ai..], &fbb[ci..], &mut out)
                .then(|| out.join("\n"));
        };
        if !merge_chunk_v064(&fb[bi..k], &fa[ai..sa], &fbb[ci..sb], &mut out) {
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

/// The reviewer's R3 shape at 20 000 lines: one merge must be a matter
/// of milliseconds, not the 1,4 s / 1,68 GB the quadratic table cost —
/// a regression tripwire far above what Myers actually takes.
#[test]
fn the_twenty_thousand_line_diff3_stays_fast() {
    let n = 20_000_usize;
    let base: String = (0..n).map(|i| format!("riga {i}")).collect::<Vec<_>>().join("\n");
    let mut a = base.clone();
    a.push_str("\nA");
    let mut l = String::from("L\n");
    l.push_str(&base);
    let started = std::time::Instant::now();
    let merged = diff3_merge(&base, &a, &l);
    let elapsed = started.elapsed();
    assert!(merged.is_some(), "the append and the prepend compose");
    assert_eq!(
        merged.as_deref(),
        Some(format!("L\n{base}\nA").as_str()),
        "the merge is the prepend plus the append"
    );
    assert!(
        elapsed.as_millis() < 500,
        "a 20 000-line merge took {elapsed:?} — the alignment is not linear"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]
    #[test]
    fn the_new_diff3_decides_as_the_v064_one(
        base in aligned_text(),
        a in aligned_text(),
        b in aligned_text(),
    ) {
        let mine = diff3_merge(&base, &a, &b);
        let theirs = diff3_merge_v064(&base, &a, &b);
        prop_assert_eq!(
            mine, theirs,
            "the linear alignment changed the merge: {:?} vs {:?} vs {:?}",
            base, a, b
        );
    }
}

/// A short text over a small vocabulary with heavy repeats and blank
/// lines — the alignment's ties live here, not in prose.
fn aligned_text() -> impl Strategy<Value = String> {
    (
        proptest::collection::vec(
            proptest::string::string_regex("[ xabAB\n]{0,3}").unwrap(/* static */),
            0..=40,
        ),
        0..=2_u8,
    )
        .prop_map(|(mut lines, tail)| {
            if lines.is_empty() {
                return String::new();
            }
            let mut joined = lines.join("\n");
            for _ in 0..tail {
                joined.push('\n');
            }
            joined
        })
}

/// The edge cases the property's shapes may not reach: empty bodies,
/// the final newline, a single line, identical sides, one side empty.
#[test]
fn the_diff3_edge_cases_hold() {
    // empty base, equal sides: identical changes are one change
    assert_eq!(diff3_merge("", "x", "x"), Some("x".into()));
    // one side is the base itself: the other side's word wins whole
    assert_eq!(diff3_merge("a\nb", "a\nb", "a\nX\nb"), Some("a\nX\nb".into()));
    assert_eq!(diff3_merge("a\nb", "a\nX\nb", "a\nb"), Some("a\nX\nb".into()));
    // both empty against an empty base
    assert_eq!(diff3_merge("", "", ""), Some("".into()));
    // a side empty, the base not: a deletion from one side only
    assert_eq!(diff3_merge("a\nb", "", "a\nb"), Some("".into()));
    assert_eq!(diff3_merge("a\nb", "a\nb", ""), Some("".into()));
    // both delete differently: the overlap
    assert_eq!(diff3_merge("a\nb\nc", "a\nc", "a\nb"), None);
    // the final newline is a line of its own
    assert_eq!(diff3_merge("a\n", "a\nx\n", "a\n"), Some("a\nx\n".into()));
    assert_eq!(diff3_merge("a\n", "a\n", "a\nx\n"), Some("a\nx\n".into()));
    // both append the same final empty line: one change
    assert_eq!(diff3_merge("a", "a\n", "a\n"), Some("a\n".into()));
    // both append a different final line: the overlap
    assert_eq!(diff3_merge("a", "a\nx", "a\ny"), None);
    // a single common line: each side adds its own copy around it — the
    // changes compose, they never overlap
    assert_eq!(diff3_merge("m", "a\nm", "m\na"), Some("a\nm\na".into()));
}

/// (E3, measurement — `--ignored`): the R3 numbers for the REPORT, at
/// 5 000 and 20 000 lines. The RSS the REPORT quotes comes from the
/// same shape under `/usr/bin/time -l` (bench/examples/diff3_bench.rs).
#[test]
#[ignore = "a measurement, not an assertion"]
fn r3_cost_at_5k_and_20k() {
    for n in [5_000_usize, 20_000] {
        let base: String = (0..n).map(|i| format!("riga {i}")).collect::<Vec<_>>().join("\n");
        let mut a = base.clone();
        a.push_str("\nA");
        let mut l = String::from("L\n");
        l.push_str(&base);
        let t = std::time::Instant::now();
        let r = diff3_merge(&base, &a, &l);
        eprintln!("R3 n={n} merge ok={} in {:?}", r.is_some(), t.elapsed());
        // the worst honest shape too: two unrelated 5 000-line texts
        let other: String = (0..n).map(|i| format!("diversa {i}")).collect::<Vec<_>>().join("\n");
        let t = std::time::Instant::now();
        let r = diff3_merge(&base, &other, &l);
        eprintln!("R3 n={n} unrelated merge ok={} in {:?}", r.is_some(), t.elapsed());
    }
}
