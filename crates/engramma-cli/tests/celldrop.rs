// SPDX-License-Identifier: Apache-2.0

//! v0.4.3 (P0.1): `cell drop` forgets the cell and ITS OWN files — the
//! database, the WAL/shm and the compaction snapshots of the EXACT compact
//! format `<cell>-snap-<digits>.sqlite`. A neighboring cell whose name
//! merely looks like a snapshot (`storia-snap-archivio`) stays whole; a
//! real snapshot is never listed by `cell list`; a cell CANNOT be created
//! with a snapshot-shaped name (reserved: `x-snap-7` would ride with x's
//! drop and never be listed).

use std::process::Command;

use engramma_core::cell::CompactOptions;
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, Status};

fn note(n: u16, body: &str) -> Note {
    let t = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* test-only */);
    Note {
        id: ulid::Ulid::from_string(&format!("01{:024}", n)).unwrap(/* test-only */),
        note_type: NoteType::Fact,
        title: Some(format!("nota {n}")),
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::It),
        source: "user".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: At(t),
        updated_at: At(t),
        extensions: vec![],
    }
}

struct Fixture {
    /// Keeps the temp folder alive for the Fixture's life (never read).
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("cells")).unwrap(/* test-only */);
        Fixture { _dir: dir, root }
    }

    fn cell(&self, name: &str, bodies: &[&str]) -> std::path::PathBuf {
        let path = self.root.join("cells").join(format!("{name}.sqlite"));
        let cell = Cell::create(&path, name, "t").unwrap(/* test-only */);
        for (i, body) in bodies.iter().enumerate() {
            cell.put_note(&note((i + 1) as u16, body), "t")
                .unwrap(/* test-only */);
        }
        drop(cell);
        path
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_engramma"))
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap(/* test-only */)
    }
}

/// Test 1: the neighbor whose name merely LOOKS like a snapshot survives a
/// drop of the cell it seems to belong to, whole and re-readable.
#[test]
fn cell_drop_spares_the_snapshot_named_neighbor() {
    let fx = Fixture::new();
    fx.cell("storia", &["la nota di storia"]);
    fx.cell("storia-snap-archivio", &["la nota dell'archivio"]);
    fx.cell("storia2", &["la nota di storia2"]);

    let out = fx.run(&["cell", "drop", "storia", "--reason", "prova"]);
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let cells = fx.root.join("cells");
    let left: Vec<String> = std::fs::read_dir(&cells)
        .unwrap(/* test-only */)
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !left.iter().any(|n| n.starts_with("storia.sqlite")),
        "storia and its sidecars must go: {left:?}"
    );
    let archivio = cells.join("storia-snap-archivio.sqlite");
    assert!(archivio.is_file(), "the neighbor went too: {left:?}");
    // and it re-reads WHOLE (the data survived, not a RAM ghost)
    let cell = Cell::open(&archivio).unwrap(/* test-only */);
    assert_eq!(cell.note_ids_all().unwrap(/* test-only */).len(), 1);
    drop(cell);
    assert!(cells.join("storia2.sqlite").is_file(), "{left:?}");
}

/// Test 2: a REAL snapshot goes with its cell, and `cell list` never lists
/// it (a cell cannot have that name anymore, so that shape is always a
/// snapshot).
#[test]
fn cell_list_skips_the_real_snapshots_and_they_go_with_their_cell() {
    let fx = Fixture::new();
    let storia = fx.cell("storia", &["prima", "seconda", "terza"]);
    let cell = Cell::open(&storia).unwrap(/* test-only */);
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let report = cell
        .compact(head - 1, &CompactOptions { keep_snapshot: true })
        .unwrap(/* test-only */);
    drop(cell);
    let snap = report
        .snapshot
        .expect(/* test-only */ "keep_snapshot writes a file");
    assert!(snap.is_file(), "{}", snap.display());
    let snap_name = snap.file_name().unwrap(/* test-only */).to_string_lossy().to_string();
    assert!(snap_name.starts_with("storia-snap-"), "{snap_name}");

    // `cell list` says storia, not storia-snap-<head>
    let out = fx.run(&["cell", "list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("\"storia\""), "{stdout}");
    assert!(
        !stdout.contains("storia-snap-"),
        "the snapshot is listed: {stdout}"
    );

    // and the drop takes the snapshot with its cell
    let out = fx.run(&["cell", "drop", "storia", "--reason", "prova"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!snap.exists(), "the snapshot survived its cell's drop");
    assert!(
        !fx.root.join("cells").join("storia.sqlite").exists(),
        "the cell itself must go"
    );
}

/// Test 3: the snapshot-shaped NAME is reserved at creation — a cell called
/// `x-snap-7` would be indistinguishable from a snapshot of `x`. The
/// non-digit suffix stays valid (`storia-snap-archivio` is a legal cell).
#[test]
fn snapshot_shaped_cell_names_are_refused_at_creation() {
    let fx = Fixture::new();
    let out = fx.run(&["cell", "create", "x-snap-7"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !fx.root.join("cells").join("x-snap-7.sqlite").exists(),
        "the reserved cell was created"
    );
    let out = fx.run(&["cell", "create", "storia-snap-archivio"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Test 4: the honest bill — `compact`'s footprint counts the cell's OWN
/// snapshots, not a neighboring cell that happens to be named like one
/// (dir_footprint feeds CompactReport.bytes_before). The neighbor is made
/// MUCH bigger than storia's whole file, so the two bills cannot be
/// confused: with the prefix match storia's bill carried the neighbor too.
#[test]
fn compact_footprint_counts_only_the_cells_own_snapshots() {
    let fx = Fixture::new();
    let storia = fx.cell("storia", &["prima"]);
    let big: Vec<String> = (0..600)
        .map(|i| format!("nota lunga numero {i} dell'archivio con molto corpo segreto {i}"))
        .collect();
    let refs: Vec<&str> = big.iter().map(|s| s.as_str()).collect();
    let archivio = fx.cell("storia-snap-archivio", &refs);
    let archivio_len = std::fs::metadata(&archivio).unwrap(/* test-only */).len();
    let storia_len = std::fs::metadata(&storia).unwrap(/* test-only */).len();
    assert!(
        archivio_len > storia_len * 2,
        "the fixture must separate the bills: storia {storia_len}, archivio {archivio_len}"
    );
    let cell = Cell::open(&storia).unwrap(/* test-only */);
    let head: i64 = cell
        .debug_column("SELECT CAST(MAX(seq) AS TEXT) FROM ops")
        .unwrap(/* test-only */)[0]
        .parse()
        .unwrap(/* test-only */);
    let report = cell
        .compact(head - 1, &CompactOptions { keep_snapshot: true })
        .unwrap(/* test-only */);
    drop(cell);
    assert!(
        report.bytes_before < archivio_len,
        "storia's bill {} carries the neighbor's {archivio_len} bytes",
        report.bytes_before
    );
}
