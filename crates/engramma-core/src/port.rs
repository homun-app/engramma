// SPDX-License-Identifier: Apache-2.0

//! Import/export: Engramma folders (lossless, §3/§7 of the format spec) and
//! plain Obsidian folders. Symbolic links are never followed and nothing
//! outside the given root is read.

use std::fs;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use ulid::Ulid;

use crate::cell::Cell;
use crate::error::{FormatError, PortError};
use crate::format::ParseMode;
use crate::format::{self, Note, Status};

/// Format version written into `engramma.json`.
pub const FORMAT_VERSION: u64 = 0;

/// Summary of an export.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSummary {
    /// Notes written (superseded included, tombstones never).
    pub written: usize,
    /// Notes skipped because tombstones.
    pub tombstones: usize,
    /// Files that could not be written.
    pub errors: Vec<String>,
}

/// Summary of an import.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSummary {
    /// Notes imported.
    pub imported: usize,
    /// Files skipped (hidden, non-markdown, tombstones, symlinks).
    pub skipped: Vec<String>,
    /// Failures with their reason.
    pub errors: Vec<String>,
    /// Format warnings (unknown keys, …).
    pub warnings: Vec<String>,
    /// v0.6.9 (I1): note ids the import SKIPPED because the cell holds a
    /// purge of them (an old export, a backup: the file comes back, the
    /// id does not). The import does not stop — the other files enter —
    /// and these are NAMED, not errored: the caller tells the user which
    /// files to drop or rewrite with a new id (`skippedPurged` in the
    /// CLI's JSON report).
    pub skipped_purged: Vec<String>,
}

/// Kebab-case, diacritics-folded slug for export file names.
pub fn slugify(title: &str) -> String {
    let folded = crate::text::strip_diacritics(&title.to_lowercase());
    let mut out = String::new();
    let mut dash = false;
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else if !out.is_empty() {
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    let mut out: String = out.chars().take(60).collect();
    if out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("note");
    }
    out
}

/// Export a cell as an Engramma folder (spec §7).
///
/// # Errors
/// [`PortError::Io`] on filesystem failures; [`PortError::Cell`] when notes
/// cannot be read.
pub fn export(cell: &Cell, dir: &Path) -> Result<ExportSummary, PortError> {
    fs::create_dir_all(dir)?;
    let mut summary = ExportSummary {
        written: 0,
        tombstones: 0,
        errors: Vec::new(),
    };
    // Deterministic order: read id-sorted, write type subfolders.
    let ids: Vec<String> = {
        let inner = cell.inner.lock().map_err(|_| {
            PortError::Cell(crate::error::CellError::Db("cell mutex poisoned".into()))
        })?;
        let mut stmt = inner
            .conn
            .prepare("SELECT id FROM notes ORDER BY id")
            .map_err(|e| PortError::Cell(crate::error::CellError::Db(e.to_string())))?;

        stmt.query_map([], |r| r.get(0))
            .map_err(|e| PortError::Cell(crate::error::CellError::Db(e.to_string())))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| PortError::Cell(crate::error::CellError::Db(e.to_string())))?
    };
    for id in ids {
        let ulid = Ulid::from_string(&id).map_err(|_| {
            PortError::Cell(crate::error::CellError::Db(format!("bad note id {id}")))
        })?;
        // v0.6.8 (H1): a bin's COPY answers NotFound — it follows its
        // lapide, and the folder says nothing of it (like a tombstone:
        // the copy skips, counted with the tombstones)
        let note = match cell.get(&ulid) {
            Ok(note) => note,
            Err(crate::error::CellError::NoteNotFound(_)) => {
                summary.tombstones += 1;
                continue;
            }
            Err(e) => return Err(PortError::Cell(e)),
        };
        if note.status == Status::Deleted {
            summary.tombstones += 1;
            continue;
        }
        let type_dir = dir.join(note.note_type.as_str());
        fs::create_dir_all(&type_dir)?;
        let base = format!(
            "{}-{}",
            slugify(note.display_title()),
            crate::id::id8(&ulid)
        );
        let text = note.serialize().map_err(|e| PortError::Note {
            path: type_dir.join(&base),
            source: e,
        })?;
        // Same slug and id8 are rare but possible: keep names unique.
        let mut name = format!("{base}.md");
        let mut n = 2;
        while type_dir.join(&name).exists() {
            name = format!("{base}-{n}.md");
            n += 1;
        }
        if let Err(e) = fs::write(type_dir.join(&name), text) {
            summary
                .errors
                .push(format!("{}: {e}", type_dir.join(name).display()));
        } else {
            summary.written += 1;
        }
    }
    let exported_at = OffsetDateTime::now_utc()
        .replace_millisecond(0)
        .unwrap_or(OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let manifest = serde_json::json!({
        "format_version": FORMAT_VERSION,
        "cell": cell.meta().map(|m| m.get("cell_name").cloned().unwrap_or_default()).unwrap_or_default(),
        "exported_at": exported_at,
        "note_count": summary.written,
    });
    fs::write(
        dir.join("engramma.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )?;
    Ok(summary)
}

/// A markdown file found while walking a folder.
struct Found {
    path: PathBuf,
    relative: String,
}

/// Walk a folder for `.md` files: no symlinks (never followed, reported as
/// skipped), hidden entries skipped, deterministic order.
fn walk_markdown(root: &Path, skipped: &mut Vec<String>) -> Result<Vec<Found>, PortError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)?;
        let mut names: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        names.sort();
        for path in names {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                skipped.push(format!("{}: symlink not followed", rel(root, &path)));
                continue;
            }
            if name.starts_with('.') {
                if meta.is_dir() {
                    skipped.push(format!("{}: hidden directory", rel(root, &path)));
                }
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(Found {
                    relative: rel(root, &path),
                    path,
                });
            }
        }
    }
    Ok(out)
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .to_string()
}

/// Close the validity of superseded notes left open: `valid_to` becomes the
/// `valid_from` of the first note that cites them in `supersedes` (spec §1.8).
/// Notes without a citing note stay open (the warning already said it).
fn close_superseded_validity(notes: &mut [&mut Note]) {
    let mut closing: std::collections::HashMap<ulid::Ulid, crate::format::Validity> =
        std::collections::HashMap::new();
    for n in notes.iter() {
        if let Some(vf) = n.valid_from {
            for old in &n.supersedes {
                closing.entry(*old).or_insert(vf);
            }
        }
    }
    for n in notes.iter_mut() {
        if n.status == Status::Superseded
            && n.valid_to.is_none()
            && let Some(vf) = closing.get(&n.id)
        {
            n.valid_to = Some(*vf);
        }
    }
}

/// Import an Engramma export folder into a cell: the same notes and the same
/// relations come out (used by the export→import property test).
///
/// # Errors
/// [`PortError::NotAnExport`] without `engramma.json`; see also
/// [`PortError::Note`] for unparsable files.
pub fn import_cell(cell: &Cell, dir: &Path, actor: &str) -> Result<ImportSummary, PortError> {
    let manifest = dir.join("engramma.json");
    if !manifest.is_file() {
        return Err(PortError::NotAnExport(format!(
            "{} is missing engramma.json",
            dir.display()
        )));
    }
    let mut summary = ImportSummary {
        imported: 0,
        skipped: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        skipped_purged: Vec::new(),
    };
    let files = walk_markdown(dir, &mut summary.skipped)?;
    // Phase 1: parse everything (the note that closes a superseded one may
    // come later in the walk).
    let mut parsed_notes: Vec<(Note, Vec<String>, String)> = Vec::new();
    for f in files {
        let text = fs::read_to_string(&f.path)?;
        match format::parse(&text, ParseMode::Strict) {
            Ok(parsed) => {
                if parsed.note.status == Status::Deleted {
                    summary.skipped.push(format!("{}: tombstone", f.relative));
                    continue;
                }
                summary.warnings.extend(parsed.warnings.clone());
                parsed_notes.push((parsed.note, parsed.warnings, f.relative));
            }
            Err(e) => summary.errors.push(format!("{}: {e}", f.relative)),
        }
    }
    // Phase 2: close open superseded validities, then write.
    {
        let mut notes: Vec<&mut Note> = parsed_notes.iter_mut().map(|(n, _, _)| n).collect();
        close_superseded_validity(&mut notes);
    }
    for (mut note, _warnings, relative) in parsed_notes {
        // `put_note` always births `external` notes in quarantine; a file
        // with `external` + another status describes a note that was
        // confirmed later: rebuild that with `confirm` so the log keeps the
        // invariant.
        let needs_confirm = note.confidence == crate::format::Confidence::External
            && note.status != Status::Quarantined;
        if note.confidence == crate::format::Confidence::External {
            note.status = Status::Quarantined;
        }
        match cell.put_note(&note, actor) {
            Ok(()) => {
                if needs_confirm && let Err(e) = cell.confirm(&note.id, actor) {
                    summary.errors.push(format!("{relative}: {e}"));
                    continue;
                }
                summary.imported += 1;
            }
            // v0.6.9 (I1): the id this cell forgot does not come back —
            // not even from an old export. The import says so, names the
            // id, and moves on: the OTHER files enter
            Err(crate::CellError::NotePurged(id)) => {
                summary.skipped_purged.push(id.clone());
                summary
                    .skipped
                    .push(format!("{relative}: purged id {id} (forgotten forever)"));
            }
            Err(e) => summary.errors.push(format!("{relative}: {e}")),
        }
    }
    Ok(summary)
}

/// Import a plain Obsidian folder: `.md` files without an Engramma header get
/// one (`id`, `type: note`, `source: import:obsidian/<relative path>`,
/// `confidence: stated`, timestamps from the file); files that already carry a
/// valid header are imported as Engramma notes.
///
/// # Errors
/// [`PortError::Io`] on filesystem failures.
pub fn import_obsidian(cell: &Cell, dir: &Path, actor: &str) -> Result<ImportSummary, PortError> {
    let mut summary = ImportSummary {
        imported: 0,
        skipped: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        skipped_purged: Vec::new(),
    };
    let files = walk_markdown(dir, &mut summary.skipped)?;
    // Phase 1: parse everything (an Engramma-headed note may supersede one
    // parsed earlier).
    let mut notes: Vec<(Note, String)> = Vec::new();
    for f in files {
        let text = fs::read_to_string(&f.path)?;
        let mtime = fs::metadata(&f.path)?
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| OffsetDateTime::from_unix_timestamp(d.as_secs() as i64))
            .transpose()
            .map_err(|e| PortError::NotAnExport(format!("bad mtime: {e}")))?
            .unwrap_or_else(OffsetDateTime::now_utc);
        let mtime = mtime.replace_millisecond(0).unwrap_or(mtime);
        // v0.3.1 (P1.5): a header that is not Engramma's is READ (title,
        // tags, aliases, type, dates), not swallowed into the body — the
        // note keeps the user's fields and the body after the header.
        let parsed = format::parse(&text, ParseMode::Import);
        let mut note = match parsed {
            Ok(p) => {
                summary.warnings.extend(p.warnings);
                p.note
            }
            Err(FormatError::NoFrontmatter) | Err(FormatError::MissingField(_)) => {
                let stem = f
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let p = format::parse_obsidian(
                    &text,
                    &stem,
                    crate::format::At(mtime),
                    crate::id::new_ulid().map_err(PortError::Cell)?,
                )
                .map_err(|e| PortError::Note {
                    path: f.path.clone(),
                    source: e,
                })?;
                summary.warnings.extend(p.warnings);
                let mut note = p.note;
                note.source = format!("import:obsidian/{}", f.relative.replace('\\', "/"));
                note.confidence = crate::format::Confidence::Stated;
                note.status = Status::Active;
                note
            }
            Err(e) => {
                summary.errors.push(format!("{}: {e}", f.relative));
                continue;
            }
        };
        let _ = &mut note; // (a full Engramma header keeps its own source)
        notes.push((note, f.relative));
    }
    // Phase 2: close open superseded validities, then write.
    {
        let mut refs: Vec<&mut Note> = notes.iter_mut().map(|(n, _)| n).collect();
        close_superseded_validity(&mut refs);
    }
    for (note, relative) in notes {
        match cell.put_note(&note, actor) {
            Ok(()) => summary.imported += 1,
            // v0.6.9 (I1): same word as the folder import — the purged id
            // is named and skipped, the other files enter
            Err(crate::CellError::NotePurged(id)) => {
                summary.skipped_purged.push(id.clone());
                summary
                    .skipped
                    .push(format!("{relative}: purged id {id} (forgotten forever)"));
            }
            Err(e) => summary.errors.push(format!("{relative}: {e}")),
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::NoteType;
    use crate::search::Query;

    fn at(day: u32) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(day) * 86_400)
            .unwrap(/* test-only */)
    }

    fn note(id: u16, title: &str, body: &str) -> Note {
        Note {
            id: Ulid::from_string(&format!("01{:024}", id)).unwrap(/* test-only */),
            note_type: NoteType::Person,
            title: Some(title.into()),
            body: body.into(),
            tags: vec!["casa/milano".into()],
            scope: None,
            lang: Some(crate::format::Lang::It),
            source: "user".into(),
            confidence: crate::format::Confidence::Stated,
            status: Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: crate::format::At(at(1)),
            updated_at: crate::format::At(at(1)),
            extensions: vec![("x-k".into(), crate::format::yaml::YamlValue::Int(7))],
        }
    }

    #[test]
    fn slugify_is_ascii_kebab() {
        assert_eq!(slugify("Fabio abita a Milano!"), "fabio-abita-a-milano");
        assert_eq!(slugify("Città del mare — 2026"), "citta-del-mare-2026");
        assert_eq!(slugify("???"), "note");
        assert!(slugify("Grande progetto lungoooooooooooo").len() <= 60);
    }

    #[test]
    fn export_then_import_reproduces_state() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("c.sqlite"), "famiglia", "t")
            .unwrap(/* test-only */);
        let fabio = note(
            1,
            "Fabio",
            "Fabio.\n\n## Relations\n- partner_of:: [[Giulia]]\n",
        );
        let giulia = note(2, "Giulia", "Giulia vive a [[Milano]]. #casa/milano");
        cell.put_note(&fabio, "t").unwrap(/* test-only */);
        cell.put_note(&giulia, "t").unwrap(/* test-only */);
        let mut dead = note(3, "Da cancellare", "x");
        dead.note_type = NoteType::Note;
        cell.put_note(&dead, "t").unwrap(/* test-only */);
        cell.delete_note(&dead.id, "t").unwrap(/* test-only */);

        let out = dir.path().join("export");
        let s = export(&cell, &out).unwrap(/* test-only */);
        assert_eq!(s.written, 2);
        assert_eq!(s.tombstones, 1);
        assert!(out.join("engramma.json").is_file());
        // the short id is the ULID's random suffix, not its timestamp prefix
        assert!(out.join("person").join("fabio-00000001.md").is_file());

        let cell2 = Cell::create(&dir.path().join("c2.sqlite"), "famiglia", "t")
            .unwrap(/* test-only */);
        let s2 = import_cell(&cell2, &out, "t").unwrap(/* test-only */);
        assert_eq!(s2.imported, 2);
        assert!(s2.errors.is_empty(), "{s2:?}");
        // same notes
        assert_eq!(cell2.get(&fabio.id).unwrap(/* test-only */), fabio);
        assert_eq!(cell2.get(&giulia.id).unwrap(/* test-only */), giulia);
        // same relations (edges by id are deterministic)
        let e1: Vec<(String, String, String)> = {
            let inner = cell.inner.lock().unwrap(/* test-only */);
            query_edges(&inner.conn)
        };
        let e2: Vec<(String, String, String)> = {
            let inner = cell2.inner.lock().unwrap(/* test-only */);
            query_edges(&inner.conn)
        };
        assert_eq!(e1, e2);
    }

    fn query_edges(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
        let mut stmt = conn
            .prepare("SELECT id, src, dst FROM edges ORDER BY id")
            .unwrap(/* test-only */);
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .unwrap(/* test-only */);
        rows.flatten().collect()
    }

    #[test]
    fn import_closes_superseded_validity_at_the_citing_note() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let src = dir.path().join("vault");
        fs::create_dir_all(&src).unwrap(/* test-only */);
        // an old superseded note left open…
        let old = b"---\nid: 01AAAAAAAAAAAAAAAAAAAAAAAA\ntype: preference\n\
                    title: Fabio vegetariano\nlang: it\nsource: user\n\
                    confidence: stated\nstatus: superseded\nvalid_from: 2020-01-06\n\
                    created_at: 2026-01-12T08:00:00Z\nupdated_at: 2026-01-12T08:00:00Z\n\
                    ---\n[[Fabio]] era vegetariano.\n";
        // …and the note that replaced it, starting 2026-10-01 (parsed after:
        // the closing note may come later in the walk)
        let new = b"---\nid: 01BBBBBBBBBBBBBBBBBBBBBBBB\ntype: preference\n\
                    title: Fabio onnivoro\nlang: it\nsource: user\n\
                    confidence: stated\nstatus: active\nvalid_from: 2026-10-01\n\
                    supersedes: [01AAAAAAAAAAAAAAAAAAAAAAAA]\n\
                    created_at: 2026-10-01T22:00:00Z\nupdated_at: 2026-10-01T22:00:00Z\n\
                    ---\n[[Fabio]] mangia di nuovo carne.\n";
        fs::write(src.join("zz-old.md"), old).unwrap(/* t */);
        fs::write(src.join("new.md"), new).unwrap(/* t */);

        let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t")
            .unwrap(/* test-only */);
        let s = import_obsidian(&cell, &src, "t").unwrap(/* test-only */);
        assert_eq!(s.imported, 2, "{s:?}");
        assert!(
            s.warnings
                .iter()
                .any(|w| w.contains("superseded") && w.contains("valid_to")),
            "the open validity must be a warning: {s:?}"
        );
        let old_id = ulid::Ulid::from_string("01AAAAAAAAAAAAAAAAAAAAAAAA")
            .unwrap(/* test-only */);
        let back = cell.get(&old_id).unwrap(/* test-only */);
        assert_eq!(
            back.valid_to.map(|v| v.to_db_string()),
            Some("2026-10-01".to_string()),
            "validity must close where the citing note begins"
        );
        // an Engramma folder (strict) refuses the same file instead
        let exp = dir.path().join("exp");
        fs::create_dir_all(exp.join("preference")).unwrap(/* test-only */);
        fs::write(exp.join("preference").join("old.md"), old).unwrap(/* t */);
        fs::write(
            exp.join("engramma.json"),
            r#"{"format_version":0,"cell":"t","exported_at":"2026-11-05T00:00:00Z","note_count":1}"#,
        )
        .unwrap(/* t */);
        let cell2 = Cell::create(&dir.path().join("c2.sqlite"), "t", "t")
            .unwrap(/* test-only */);
        let s2 = import_cell(&cell2, &exp, "t").unwrap(/* test-only */);
        assert!(
            s2.errors
                .iter()
                .any(|e| e.contains("superseded") && e.contains("valid_to")),
            "strict import must reject it: {s2:?}"
        );
    }

    #[test]
    fn obsidian_import_with_hidden_and_symlinks() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join(".obsidian")).unwrap(/* test-only */);
        fs::create_dir_all(vault.join("notes")).unwrap(/* test-only */);
        fs::write(vault.join(".obsidian/config.json"), "{}").unwrap(/* test-only */);
        fs::write(vault.join("notes/idea.txt"), "not markdown").unwrap(/* test-only */);
        fs::write(
            vault.join("appunti.md"),
            "# Titolo\n\nAppunto su [[Milano]].\n",
        )
        .unwrap(/* test-only */);
        // a symlink: must be skipped, never followed
        #[cfg(unix)]
        std::os::unix::fs::symlink(vault.join("appunti.md"), vault.join("link.md"))
            .unwrap(/* test-only */);

        let cell = Cell::create(&dir.path().join("c.sqlite"), "import", "t")
            .unwrap(/* test-only */);
        let s = import_obsidian(&cell, &vault, "t").unwrap(/* test-only */);
        assert_eq!(s.imported, 1, "{s:?}");
        assert!(s.skipped.iter().any(|x| x.contains(".obsidian")));
        #[cfg(unix)]
        assert!(s.skipped.iter().any(|x| x.contains("link.md")));
        let hits = cell.search_with_now(&Query::text("appunto"), at(2))
            .unwrap(/* test-only */);
        assert_eq!(hits.len(), 1);
        let n = &hits[0].note;
        assert!(n.source.starts_with("import:obsidian/appunti.md"));
        assert_eq!(n.note_type, NoteType::Note);
    }
    /// v0.3.2 (P1.7): a plain file without a header takes the FILE NAME as
    /// its title (as Obsidian displays it) — the v0.3.1 fallback only fired
    /// when a header existed.
    #[test]
    fn obsidian_import_titles_plain_files_by_name() {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let vault = dir.path().join("v");
        std::fs::create_dir_all(&vault).unwrap(/* test-only */);
        std::fs::write(vault.join("Mele rosse.md"), "Mi piacciono.\n")
            .unwrap(/* test-only */);
        let cell = Cell::create(&dir.path().join("c.sqlite"), "t", "t")
            .unwrap(/* test-only */);
        let s = import_obsidian(&cell, &vault, "t").unwrap(/* test-only */);
        assert_eq!(s.imported, 1, "{s:?}");
        let ids = cell.note_ids().unwrap(/* test-only */);
        assert_eq!(ids.len(), 1);
        let note = cell.get(&ids[0]).unwrap(/* test-only */);
        assert_eq!(note.title.as_deref(), Some("Mele rosse"), "{note:?}");
        assert_eq!(note.body, "Mi piacciono.\n");
    }
}
