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
        let note = cell.get(&ulid).map_err(PortError::Cell)?;
        if note.status == Status::Deleted {
            summary.tombstones += 1;
            continue;
        }
        let type_dir = dir.join(note.note_type.as_str());
        fs::create_dir_all(&type_dir)?;
        let base = format!("{}-{}", slugify(note.display_title()), &id[..8]);
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
    };
    let files = walk_markdown(dir, &mut summary.skipped)?;
    for f in files {
        let text = fs::read_to_string(&f.path)?;
        match format::parse(&text, ParseMode::Strict) {
            Ok(parsed) => {
                if parsed.note.status == Status::Deleted {
                    summary.skipped.push(format!("{}: tombstone", f.relative));
                    continue;
                }
                let mut note = parsed.note.clone();
                // `put_note` always births `external` notes in quarantine;
                // a file with `external` + another status describes a note
                // that was confirmed later: rebuild that with `confirm` so the
                // log keeps the invariant.
                let needs_confirm = note.confidence == crate::format::Confidence::External
                    && note.status != Status::Quarantined;
                if note.confidence == crate::format::Confidence::External {
                    note.status = Status::Quarantined;
                }
                match cell.put_note(&note, actor) {
                    Ok(()) => {
                        if needs_confirm && let Err(e) = cell.confirm(&note.id, actor) {
                            summary.errors.push(format!("{}: {e}", f.relative));
                            continue;
                        }
                        summary.imported += 1;
                    }
                    Err(e) => summary.errors.push(format!("{}: {e}", f.relative)),
                }
                summary.warnings.extend(parsed.warnings);
            }
            Err(e) => summary.errors.push(format!("{}: {e}", f.relative)),
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
    };
    let files = walk_markdown(dir, &mut summary.skipped)?;
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
        let parsed = format::parse(&text, ParseMode::Import);
        let note = match parsed {
            Ok(p) => p.note,
            Err(FormatError::NoFrontmatter) | Err(FormatError::MissingField(_)) => Note {
                id: crate::id::new_ulid().map_err(PortError::Cell)?,
                note_type: crate::format::NoteType::Note,
                title: None,
                body: text,
                tags: vec![],
                scope: None,
                lang: None,
                source: format!("import:obsidian/{}", f.relative.replace('\\', "/")),
                confidence: crate::format::Confidence::Stated,
                status: Status::Active,
                valid_from: None,
                valid_to: None,
                supersedes: vec![],
                created_at: crate::format::At(mtime),
                updated_at: crate::format::At(mtime),
                extensions: vec![],
            },
            Err(e) => {
                summary.errors.push(format!("{}: {e}", f.relative));
                continue;
            }
        };
        match cell.put_note(&note, actor) {
            Ok(()) => summary.imported += 1,
            Err(e) => summary.errors.push(format!("{}: {e}", f.relative)),
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
        assert!(out.join("person").join("fabio-01000000.md").is_file());

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
}
