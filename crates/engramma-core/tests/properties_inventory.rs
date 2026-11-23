// SPDX-License-Identifier: Apache-2.0

//! v0.6.6 (F2): the machine-checked inventory of the suite's proptest
//! properties. A property removed from the sources (the v0.6.5 defect:
//! `434759c` replaced `no_copy_closes_without_a_witness` instead of
//! adding a judge next to it) must fail HERE, in a file the reviewer
//! reads — the same principle as an insta snapshot or a CI-compared
//! `--list`.
//!
//! The parser is deliberately simple and honest about what it sees: it
//! walks the integration tests of every crate (`crates/*/tests/**/*.rs`),
//! finds `proptest! {` blocks by counting braces, and records every
//! `fn name(` declared inside. What it does NOT see (declared in the
//! REPORT): properties made by macros other than `proptest!`, blocks
//! nested inside another macro invocation, and unbalanced braces inside
//! strings or comments of a block's body (every brace in this suite's
//! property bodies is balanced, format strings included).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// One inventory line, THREE columns (v0.6.14, N3): the test file from
/// the workspace root, the property name, and the modules the property
/// exercises (comma-separated paths from the root) — the map that says
/// whether a property is TOUCHED. The third column is mandatory: a
/// property without a map could never be judged touched, and would get
/// the short budget for ever.
fn parse_inventory_lines(text: &str) -> Vec<(String, String, Vec<String>)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut parts = l.split_whitespace();
            let path = parts
                .next()
                .unwrap_or_else(|| panic!("riga d'inventario malformata: «{l}»"));
            let name = parts
                .next()
                .unwrap_or_else(|| panic!("riga d'inventario senza proprietà: «{l}»"));
            let modules = parts.next().unwrap_or_else(|| {
                panic!(
                    "la proprietà «{path} {name}» non ha la mappa dei moduli (terza \
                         colonna di properties.txt, v0.6.14 N3)"
                )
            });
            let modules: Vec<String> = modules
                .split(',')
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .collect();
            (path.to_string(), name.to_string(), modules)
        })
        .collect()
}

/// The inventory as the two sets the gates compare: `(path, name)`.
fn parse_inventory(text: &str) -> BTreeSet<(String, String)> {
    parse_inventory_lines(text)
        .into_iter()
        .map(|(path, name, _)| (path, name))
        .collect()
}

/// Every `crates/*/tests/**/*.rs` of the workspace, sorted — the same
/// order the inventory lists them in.
fn test_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(crates) = root.join("crates").read_dir().ok().map(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect::<Vec<_>>()
    }) else {
        return out;
    };
    for c in crates {
        let tests = c.join("tests");
        let mut stack = vec![tests];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = dir.read_dir() else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p
                    .extension()
                    .is_some_and(|x| x == "rs" || x == "proptest-regressions")
                {
                    // v0.6.12 (L2): the regression files too — the open
                    // defects' seed gate read ONLY the .rs sources, and no
                    // .proptest-regressions file ever reached it: the gate
                    // was vacuous since v0.6.9 (found while moving this
                    // phase's own seeds)
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// The properties one source file declares: every `fn` inside a
/// `proptest! { … }` block, blocks found by brace counting.
fn properties_of(src: &str) -> Vec<String> {
    without_test_attribute(src).into_iter().for_each(|name| {
        panic!("la proprietà `{name}` non ha `#[test]`: non è un test, non gira mai")
    });
    let mut found = Vec::new();
    // the FLOOR of each open proptest! block: the depth the line after
    // its opening brace sits at. A line is inside the block while its
    // starting depth is at or above the floor; the closing brace of the
    // block is the line that brings the depth back below it
    let mut floors: Vec<usize> = Vec::new();
    let mut depth = 0_usize;
    for line in src.lines() {
        // a `//`-comment line can NAME `proptest! {` (this file's own doc
        // does): it never opens one (block comments are a declared limit)
        if line.trim_start().starts_with("//") {
            continue;
        }
        if line.contains("proptest!") && line.contains('{') {
            floors.push(depth + line.matches('{').count() - line.matches('}').count());
        }
        // a `fn` name sits at the depth the line STARTS at — its own
        // braces come after the name
        let trimmed = line.trim_start();
        for kind in ["fn ", "async fn "] {
            if let Some(rest) = trimmed.strip_prefix(kind)
                && let Some(name) = rest.split(['(', ':']).next()
            {
                let name = name.trim();
                if !name.is_empty()
                    && floors.iter().any(|&f| depth >= f)
                    && !found.iter().any(|f| f == name)
                {
                    found.push(name.to_string());
                }
            }
        }
        depth = (depth + line.matches('{').count()).saturating_sub(line.matches('}').count());
        floors.retain(|&f| depth >= f);
    }
    found
}

/// v0.6.14 (N3): every property of a `proptest!` block carries `#[test]`
/// — the attribute that makes it a test. Without it the block mints a
/// plain function nobody calls, `cargo test` skips it in silence, and a
/// hunt records it green on zero cases: `export_import_equality` lost its
/// attribute on the 7th of November (`59092a6`, the attribute glued to
/// the end of a doc line) and `ports_matter_except_on_loopback_http`
/// never had one — nine phases of green on nothing. The attribute must
/// sit on its own line right above the `fn`, doc comments excluded.
fn without_test_attribute(src: &str) -> Vec<String> {
    let mut offenders = Vec::new();
    let mut floors: Vec<usize> = Vec::new();
    let mut depth = 0_usize;
    let mut previous = String::new();
    for line in src.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        if line.contains("proptest!") && line.contains('{') {
            floors.push(depth + line.matches('{').count() - line.matches('}').count());
        }
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("fn ")
            && let Some(name) = rest.split(['(', ':']).next()
        {
            let name = name.trim();
            if !name.is_empty()
                && floors.iter().any(|&f| depth >= f)
                && previous.trim() != "#[test]"
            {
                offenders.push(name.to_string());
            }
        }
        depth = (depth + line.matches('{').count()).saturating_sub(line.matches('}').count());
        floors.retain(|&f| depth >= f);
        previous = line.to_string();
    }
    offenders
}

#[test]
fn properties_inventory() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.ancestors().nth(2).expect(
        /* test-only */ "the workspace root sits two levels above engramma-core",
    );
    let inventory_text = std::fs::read_to_string(manifest.join("tests/properties.txt"))
        .expect(/* test-only */ "properties.txt");
    let inventory = parse_inventory(&inventory_text);

    let mut sources: BTreeSet<(String, String)> = BTreeSet::new();
    for file in test_sources(root) {
        let rel = file
            .strip_prefix(root)
            .expect(/* test-only */ "sources live under the workspace root")
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(&file).unwrap_or_else(|e| {
            panic!("lettura di {rel} fallita: {e}");
        });
        for name in properties_of(&text) {
            sources.insert((rel.clone(), name));
        }
    }

    let missing_from_inventory: Vec<_> = sources.difference(&inventory).collect();
    let missing_from_sources: Vec<_> = inventory.difference(&sources).collect();
    assert!(
        missing_from_inventory.is_empty() && missing_from_sources.is_empty(),
        "l'inventario delle proprietà non torna:\n  nei sorgenti ma NON nell'inventario (aggiungi la riga, o è una proprietà che il parser non vede — dichiara il limite): {missing_from_inventory:?}\n  nell'inventario ma NON nei sorgenti (la proprietà è stata tolta? v0.6.6, F2): {missing_from_sources:?}"
    );
}

/// v0.6.14 (N3): every property has its module map, and every path of
/// the map exists — the hunt reads this map to tell a TOUCHED property
/// (a module it exercises changed since the phase's base, or its own
/// test file), and a dead path would make a property look untouched for
/// ever while the code it judges moves under it.
#[test]
fn every_property_has_its_module_map() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .ancestors()
        .nth(2)
        .expect(/* test-only */ "the workspace root");
    let inventory_text = std::fs::read_to_string(manifest.join("tests/properties.txt"))
        .expect(/* test-only */ "properties.txt");
    let mut missing: Vec<String> = Vec::new();
    for (path, name, modules) in parse_inventory_lines(&inventory_text) {
        if modules.is_empty() {
            missing.push(format!(
                "«{path} {name}»: la mappa dei moduli è vuota (terza colonna di properties.txt)"
            ));
        }
        // the test file IS the property's oracle: it belongs to the map
        if !root.join(&path).is_file() {
            missing.push(format!("«{path} {name}»: il file del test non esiste"));
        }
        for m in &modules {
            if !root.join(m).is_file() {
                missing.push(format!(
                    "«{path} {name}»: la mappa nomina «{m}», che non esiste"
                ));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "la mappa proprietà → moduli non torna: {missing:#?}"
    );
}

/// One open-defect line: `path-from-the-workspace-root property_name
/// seed[, seed…] # the REPORT reference` (the `#` part is free text,
/// the seed field may hold more than one seed, comma-separated).
fn parse_open_defects(text: &str) -> Vec<(String, String, Vec<String>, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (head, reference) = l.split_once('#').unwrap_or((l, ""));
            let mut parts = head.split_whitespace();
            let path = parts
                .next()
                .unwrap_or_else(|| panic!("riga dei difetti aperti malformata: «{l}»"));
            let name = parts
                .next()
                .unwrap_or_else(|| panic!("riga dei difetti aperti senza proprietà: «{l}»"));
            let seeds = parts
                .next()
                .map(|s| s.split(',').map(str::trim).map(str::to_string).collect())
                .unwrap_or_default();
            (
                path.to_string(),
                name.to_string(),
                seeds,
                reference.trim().to_string(),
            )
        })
        .collect()
}

/// v0.6.9 (I3): the OPEN DEFECTS file is the hunt's other gate. A defect
/// named there must name a property that EXISTS in the inventory (a
/// typo would silently stop the hunt from judging that property), and
/// the line must carry at least one seed and a REPORT reference (the
/// judgement «declared defect or new one» lives in the REPORT, written
/// by hand — the file only points at it).
#[test]
fn open_defects_reference_real_properties() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let defects_path = manifest.join("tests/open-defects.txt");
    let defects_text =
        std::fs::read_to_string(&defects_path).expect(/* test-only */ "open-defects.txt");
    let inventory = parse_inventory(
        &std::fs::read_to_string(manifest.join("tests/properties.txt"))
            .expect(/* test-only */ "properties.txt"),
    );
    let defects = parse_open_defects(&defects_text);
    assert!(
        !defects.is_empty(),
        "an empty open-defects file means NO defects are open: if that is \
         the truth, delete the file and this test with it"
    );
    for (path, name, seeds, reference) in &defects {
        assert!(
            inventory.contains(&(path.clone(), name.clone())),
            "the open defect names «{path} {name}», which is NOT in the \
             inventory: a typo would make the hunt judge nothing"
        );
        assert!(
            !seeds.is_empty(),
            "the open defect «{path} {name}» carries no seed: the \
             reproduction is the defect's proof"
        );
        assert!(
            !reference.is_empty(),
            "the open defect «{path} {name}» carries no REPORT reference"
        );
    }
}

/// v0.6.9 (I3): the open defects' seeds stay OUT of the regression
/// files. A seed in `.proptest-regressions` makes every suite run fall
/// on a declared defect — the v0.6.8 lesson (the reviewer took the
/// `1207…` seed out by hand). The hunt says it checked; this test keeps
/// it checked between hunts. Declared limit: the seed must appear in
/// its decimal form (proptest writes «shrinks to seed = N» beside the
/// `cc` digest it replays); a hand-added `cc` line with the digest only
/// and no seed comment is invisible here.
#[test]
fn open_defect_seeds_stay_out_of_the_regression_files() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .ancestors()
        .nth(2)
        .expect(/* test-only */ "the workspace root");
    let defects = parse_open_defects(
        &std::fs::read_to_string(manifest.join("tests/open-defects.txt"))
            .expect(/* test-only */ "open-defects.txt"),
    );
    let mut offenders: Vec<String> = Vec::new();
    for file in test_sources(root) {
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".proptest-regressions") {
            continue;
        }
        let rel = file
            .strip_prefix(root)
            .expect(/* test-only */ "under the root")
            .display()
            .to_string();
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for (_, property, seeds, _) in &defects {
            for seed in seeds {
                if text.contains(seed.as_str()) {
                    offenders.push(format!("{rel}: the seed of «{property}»"));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "open-defect seeds inside the regression files (every suite run \
         would fall on a declared defect): {offenders:?}"
    );
}
