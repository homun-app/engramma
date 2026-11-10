// SPDX-License-Identifier: Apache-2.0

//! v0.4.4 (P1/P2): the RSS profile of the builtin embedder in a bare Rust
//! process — the reference measurement behind the mimalloc choice, and the
//! Linux probe (`VmRSS`/`VmHWM`). Loads Granite 97M, embeds 64 notes (half
//! ~600 tokens), prints RSS after the load, after the work, 2 s and 10 s
//! later.
//!
//! `cargo run -p engramma-embed --release --example ram` (system allocator);
//! `--example ram_mimalloc` for the same run under mimalloc.
//!
//! No new engine API and no I/O beyond the model cache: it exists to be
//! measured.

use std::thread;
use std::time::Duration;

use engramma_core::Embedder;
use engramma_core::embed::TextKind;
use engramma_embed::Builtin;

#[cfg(target_os = "linux")]
fn rss_mib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn rss_mib() -> Option<u64> {
    // macOS: the same mach call `footprint` uses, via sysctl — not exposed
    // without bindings, so shell out to vm_stat-free `ps` (RSS column).
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim().parse::<u64>().ok().map(|kb| kb / 1024)
}

fn show(label: &str) {
    if let Some(mib) = rss_mib() {
        println!("{label:34} {mib:6} MB");
    }
}

const WORDS: [&str; 25] = [
    "ricorda",
    "milano",
    "trasloco",
    "lavoro",
    "giulia",
    "marco",
    "progetto",
    "condominio",
    "bici",
    "treno",
    "ricerca",
    "memoria",
    "quarantena",
    "istruzioni",
    "riunione",
    "martedì",
    "novembre",
    "preventivo",
    "cucina",
    "affitto",
    "lavori",
    "cantina",
    "soppalco",
    "libreria",
    "tramonto",
];

/// ~600 tokens of distinct-enough memory text (the rss.js script's shape).
fn long_body(i: u64) -> String {
    let mut out = Vec::with_capacity(470);
    let mut s = i.wrapping_mul(7919);
    for _ in 0..470 {
        s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        out.push(WORDS[usize::try_from(s % 25).unwrap_or(0)]);
    }
    format!("nota lunga {i}: {}", out.join(" "))
}

pub fn run() {
    let root = engramma_embed::spec::model_root();
    let started = std::time::Instant::now();
    let builtin = Builtin::load_named(&root, "granite-embedding-97m-multilingual-r2")
        .unwrap_or_else(|e| panic!("model load: {e}"));
    println!("model loaded in {:.1} s", started.elapsed().as_secs_f64());
    show("dopo il carico");

    let mut texts: Vec<String> = Vec::with_capacity(64);
    for i in 0..64_u64 {
        if i % 2 == 0 {
            texts.push(long_body(i));
        } else {
            texts.push(format!("nota corta {i}: una cosa da ricordare."));
        }
    }
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let vectors = builtin
        .embed(&refs, TextKind::Document)
        .unwrap_or_else(|e| panic!("embed: {e}"));
    println!(
        "embed di 64 note in {:.1} s ({} vettori)",
        started.elapsed().as_secs_f64(),
        vectors.len()
    );
    show("dopo il lavoro");
    thread::sleep(Duration::from_secs(2));
    show("2 s dopo");
    thread::sleep(Duration::from_secs(8));
    show("10 s dopo");
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in status.lines() {
            if line.starts_with("VmHWM:") || line.starts_with("VmPeak:") {
                println!("{line}");
            }
        }
    }
}
