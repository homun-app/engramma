// SPDX-License-Identifier: Apache-2.0

//! v0.4.4 (P1/P2) → v0.4.5 (P2): the RSS profile of the builtin embedder
//! in a bare Rust process — the reference measurement behind the mimalloc
//! choice and the Linux probe (`VmRSS`/`VmHWM`). Loads Granite 97M, embeds
//! 64 notes (half ~600 tokens), prints RSS after the load, after the
//! work, 2 s and 10 s later.
//!
//! v0.4.5 (P2): one run is no longer enough — the verification's 540 MB
//! estimate was a lucky single run. `--matrix N` spawns N SINGLE-RUN
//! child processes (`--csv`) of this same binary — the allocator travels
//! with the binary — and prints median/min/max of: load time, RSS after
//! the load, RSS after the work, RSS after 10 s, the PEAK (macOS: the
//! `Physical footprint (peak)` of `vmmap --summary` on the live child;
//! Linux: `VmHWM`), and the embed time. `ENGRAMMA_EMBED_THREADS` and
//! `ENGRAMMA_EMBED_TOKEN_BUDGET` pass through to the children.
//!
//! ```text
//! cargo run -p engramma-embed --release --example ram -- --matrix 5
//! cargo run -p engramma-embed --release --example ram_mimalloc -- --matrix 5
//! ```
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
    // /proc reports VmRSS in kB
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
        .map(|kb| kb / 1024)
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

/// `948.6M` / `1.2G` / `512K` / bytes → MiB (macOS vmmap tokens only).
#[cfg(target_os = "macos")]
fn size_to_mib(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, mult) = match s.as_bytes().last()? {
        b'G' | b'g' => (&s[..s.len() - 1], 1024.0),
        b'M' | b'm' => (&s[..s.len() - 1], 1.0),
        b'K' | b'k' => (&s[..s.len() - 1], 1.0 / 1024.0),
        _ => (s, 1.0 / 1024.0 / 1024.0),
    };
    num.trim().parse::<f64>().ok().map(|v| v * mult)
}

/// The process's own peak physical footprint, MiB. Linux: `VmHWM`;
/// macOS: `vmmap --summary` on the LIVE process (the counter dies with
/// it) — darwin 27 prints the peak as its OWN line,
/// «Physical footprint (peak):    948.6M».
fn peak_mib() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
        let kb = line.split_whitespace().nth(1)?.parse::<f64>().ok()?;
        Some(kb / 1024.0)
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("vmmap")
            .args(["--summary", &std::process::id().to_string()])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("Physical footprint (peak):") else {
                continue;
            };
            let token = rest.split_whitespace().next()?;
            return size_to_mib(token);
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
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

/// One workload: 64 notes, half long. Returns (embed seconds, vectors).
fn workload(builtin: &Builtin) -> (f64, usize) {
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
    (started.elapsed().as_secs_f64(), vectors.len())
}

/// The single-run child: human output by default, one CSV line with
/// `--csv` (load_s, load_mb, work_mb, ten_s_mb, peak_mb, embed_s).
/// `--load-only` skips the workload — the pure load peak of v0.4.5 (P2.3).
fn single(csv: bool, load_only: bool) {
    let root = engramma_embed::spec::model_root();
    let started = std::time::Instant::now();
    let builtin = Builtin::load_named(&root, "granite-embedding-97m-multilingual-r2")
        .unwrap_or_else(|e| panic!("model load: {e}"));
    let load_s = started.elapsed().as_secs_f64();
    let load_mb = rss_mib().unwrap_or(0) as f64;
    if !csv {
        println!("model loaded in {load_s:.1} s");
        show("dopo il carico");
    }
    if load_only {
        let peak = peak_mib().unwrap_or(0.0);
        if csv {
            println!("{load_s:.2},{load_mb:.0},{load_mb:.0},{load_mb:.0},{peak:.0},0.00");
        } else {
            println!("picco (footprint massimo)      {peak:6.0} MB");
        }
        return;
    }

    let (embed_s, n) = workload(&builtin);
    let work_mb = rss_mib().unwrap_or(0) as f64;
    if !csv {
        println!("embed di 64 note in {embed_s:.1} s ({n} vettori)");
        show("dopo il lavoro");
    }
    thread::sleep(Duration::from_secs(2));
    let two_mb = rss_mib().unwrap_or(0) as f64;
    if !csv {
        show("2 s dopo");
    }
    thread::sleep(Duration::from_secs(8));
    let ten_mb = rss_mib().unwrap_or(0) as f64;
    if csv {
        let peak = peak_mib().unwrap_or(0.0);
        println!("{load_s:.2},{load_mb:.0},{work_mb:.0},{ten_mb:.0},{peak:.0},{embed_s:.2}");
        let _ = two_mb;
        return;
    }
    show("10 s dopo");
    if let Some(peak) = peak_mib() {
        println!("picco (footprint massimo)      {peak:6.0} MB");
    }
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

/// The `--matrix N` parent: N children in SEPARATE processes of this
/// same binary, then median/min/max per column.
fn matrix(runs: usize, load_only: bool) {
    let exe = std::env::current_exe().unwrap_or_else(|e| panic!("current_exe: {e}"));
    let mut rows: Vec<[f64; 6]> = Vec::with_capacity(runs);
    for i in 1..=runs {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--csv");
        if load_only {
            cmd.arg("--load-only");
        }
        let out = cmd.output().unwrap_or_else(|e| panic!("child {i}: {e}"));
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(line) = text.lines().find(|l| l.contains(',')) else {
            eprintln!("child {i} produced no CSV line:\n{text}");
            std::process::exit(1);
        };
        if !out.status.success() {
            eprintln!(
                "child {i} failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::process::exit(1);
        }
        let vals: Vec<f64> = line
            .split(',')
            .map(|v| v.trim().parse::<f64>().unwrap_or(f64::NAN))
            .collect();
        if vals.len() != 6 || vals.iter().any(|v| v.is_nan()) {
            eprintln!("child {i}: bad CSV line `{line}`");
            std::process::exit(1);
        }
        let row: [f64; 6] = [vals[0], vals[1], vals[2], vals[3], vals[4], vals[5]];
        let [l_s, l_mb, w_mb, t_mb, p_mb, e_s] = row;
        println!(
            "corsa {i}/{runs}: load {l_s:.1} s | dopo carico {l_mb:.0} MB | dopo lavoro {w_mb:.0} MB | +10 s {t_mb:.0} MB | picco {p_mb:.0} MB | embed {e_s:.2} s"
        );
        rows.push(row);
    }
    let median_of = |col: usize| {
        let mut v: Vec<f64> = rows.iter().map(|r| r[col]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = v.len();
        if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        }
    };
    let cols = [
        "carico (s)",
        "dopo carico (MB)",
        "dopo lavoro (MB)",
        "+10 s (MB)",
        "picco (MB)",
        "embed (s)",
    ];
    println!("# {runs} corse in processi separati — mediana / min / max");
    for (c, name) in cols.iter().enumerate() {
        let v: Vec<f64> = rows.iter().map(|r| r[c]).collect();
        let min = v.iter().copied().fold(f64::INFINITY, f64::min);
        let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        println!("{name:20} {:8.1} / {min:.1} / {max:.1}", median_of(c));
    }
}

pub fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let load_only = args.iter().any(|a| a == "--load-only");
    if let Some(pos) = args.iter().position(|a| a == "--matrix") {
        let runs = args
            .get(pos + 1)
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| (1..=100).contains(&n))
            .unwrap_or(5);
        matrix(runs, load_only);
        return;
    }
    let csv = args.iter().any(|a| a == "--csv");
    let known = ["--csv", "--load-only"];
    if (!csv && !load_only && !args.is_empty()) || args.iter().any(|a| !known.contains(&a.as_str()))
    {
        eprintln!(
            "ram: unknown args {args:?} (want nothing, --csv, --load-only or \
             --matrix [N] [--load-only]; ENGRAMMA_EMBED_THREADS and \
             ENGRAMMA_EMBED_TOKEN_BUDGET apply)"
        );
        std::process::exit(2);
    }
    single(csv, load_only);
}
