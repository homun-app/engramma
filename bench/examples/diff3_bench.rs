// SPDX-License-Identifier: Apache-2.0

//! v0.6.5 (E3, R3): the cost of ONE rigorous diff3 at 5 000 and 20 000
//! lines, under `/usr/bin/time -l` for the RSS. The shapes are the
//! reviewer's: an append against a prepend over the same base, and the
//! unrelated worst case.

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    let shape = std::env::args().nth(2).unwrap_or_else(|| "r3".into());
    let base: String = (0..n)
        .map(|i| format!("riga {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (a, b) = match shape.as_str() {
        "unrelated" => {
            let other: String = (0..n)
                .map(|i| format!("diversa {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            (format!("{base}\nA"), other)
        }
        _ => {
            let mut aa = base.clone();
            aa.push_str("\nA");
            (aa, format!("L\n{base}"))
        }
    };
    let t = std::time::Instant::now();
    let merged = engramma_core::cell::conflicts::diff3_merge(&base, &a, &b);
    println!(
        "n={n} shape={shape} merged={} in {:?}",
        merged.is_some(),
        t.elapsed()
    );
}
