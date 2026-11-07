// SPDX-License-Identifier: Apache-2.0

//! Engramma markdown view (DESIGN §4.1, v0.3): a two-way sync between a cell
//! and a folder of markdown files.
//!
//! The sync is a **scan-based reconciliation**, deterministic and testable:
//! [`Vault::sync_once`] walks the folder, compares every file with the note it
//! is bound to (the `id` in the header binds them, never the path) and with
//! what the last reconciliation saw, and applies the table of §4.1.
//! File-system events are only a hint to run a scan (`watch`): watchers lose
//! events, scans do not.
//!
//! In a conflict the cell wins and the file's version is saved as a
//! `<name>.conflitto-<timestamp>.md` copy, which the scanner ignores. A guard
//! stops mass deletions (an unmounted folder looks like "every file gone").

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use engramma_core::format::ParseMode;
use engramma_core::{Cell, CellError, Confidence, Note, Status};
use thiserror::Error;
use time::OffsetDateTime;
use ulid::Ulid;

/// More than this many vanished files in one reconciliation, and nothing is
/// deleted (an unmounted or moved folder), unless forced.
pub const MASS_DELETE_FILES: usize = 20;
/// …or more than this fraction of the living notes.
pub const MASS_DELETE_FRACTION: f64 = 0.10;

/// Errors of the markdown view.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VaultError {
    /// The cell failed.
    #[error("cell: {0}")]
    Cell(#[from] CellError),
    /// Filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The export or a serialization failed.
    #[error("port: {0}")]
    Port(#[from] engramma_core::PortError),
    /// A note could not be serialized back to its file.
    #[error("format: {0}")]
    Format(#[from] engramma_core::FormatError),
    /// Too many files would vanish: nothing was deleted.
    #[error(
        "la riconciliazione vorrebbe cancellare {would_delete} file su {known} \
         (soglia: {MASS_DELETE_FILES} file o il {pct:.0}% delle note): \
         cartella smontata o spostata? --allow-deletions per procedere"
    )]
    MassDelete {
        /// Files that would be deleted.
        would_delete: usize,
        /// Files known to the last reconciliation.
        known: usize,
        /// The enforced percentage.
        pct: f64,
    },
}

/// What one reconciliation did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SyncReport {
    /// Files turned into new notes (they had no `id`).
    pub created: Vec<String>,
    /// Notes updated from a changed file.
    pub updated_from_file: Vec<String>,
    /// Files rewritten because the note changed.
    pub updated_files: Vec<String>,
    /// Conflicts: the cell won, the file's version is a `.conflitto` copy.
    pub conflicts: Vec<String>,
    /// Notes deleted because their file was gone.
    pub deleted: Vec<String>,
    /// Files renamed or moved: same id, new path.
    pub moved: Vec<String>,
    /// Files skipped with their reason (bad header, unknown note…): the cell
    /// is untouched for them, the next reconciliation retries.
    pub errors: Vec<String>,
    /// Non-fatal warnings.
    pub warnings: Vec<String>,
}

impl SyncReport {
    /// Nothing happened at all (the fixed point).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.created.is_empty()
            && self.updated_from_file.is_empty()
            && self.updated_files.is_empty()
            && self.conflicts.is_empty()
            && self.deleted.is_empty()
            && self.moved.is_empty()
            && self.errors.is_empty()
            && self.warnings.is_empty()
    }
}

/// An open markdown view over one cell and one folder.
pub struct Vault {
    cell: Cell,
    root: PathBuf,
}

/// One file found by the scan.
struct Scanned {
    path: String,
    abs: PathBuf,
    hash: String,
    mtime: i64,
    size: i64,
    text: String,
}

impl Vault {
    /// Open the view; the folder may not exist yet ([`Vault::init`] creates
    /// and fills it).
    ///
    /// # Errors
    /// [`VaultError::Io`] when the path exists and is not a directory.
    pub fn open(cell: Cell, dir: &Path) -> Result<Self, VaultError> {
        if dir.exists() && !dir.is_dir() {
            return Err(VaultError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a directory", dir.display()),
            )));
        }
        Ok(Vault {
            cell,
            root: dir.to_path_buf(),
        })
    }

    /// The vault folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bound cell.
    #[must_use]
    pub fn cell(&self) -> &Cell {
        &self.cell
    }

    /// Export every living note into the folder and record what the scan
    /// sees: the starting point of the reconciliation.
    ///
    /// # Errors
    /// [`VaultError::Io`] on filesystem failures, [`VaultError::Cell`] when
    /// the export reads the cell.
    pub fn init(&self) -> Result<SyncReport, VaultError> {
        std::fs::create_dir_all(&self.root)?;
        self.bind_root()?;
        let out = engramma_core::port::export(&self.cell, &self.root)?;
        let mut report = SyncReport::default();
        report.warnings.extend(out.errors);
        for file in self.scan()? {
            if let Some(id) = id_of(&file.text)
                && let Ok(note) = self.cell.get(&Ulid::from_string(&id).unwrap_or_default())
            {
                self.record(&file, &note.id, note.status)?;
            }
        }
        Ok(report)
    }

    /// The forgetting side of a purge in THIS view (v0.3.2, P1.6): the
    /// bound file of the note and every `.conflitto` copy of it go, then
    /// their rows. The core never writes into the view — this is the one
    /// place that does, called by the CLI's `purge` for every registered
    /// root. A view that cannot be reached (unmounted folder, permissions)
    /// is an error: the caller reports `pending: ["vault"]` and the next
    /// `sync` finishes the work.
    ///
    /// # Errors
    /// [`VaultError::Io`] when the view is unreachable or a removal fails;
    /// [`VaultError::Cell`] on cell failures.
    pub fn forget_note(&self, id: &Ulid) -> Result<(), VaultError> {
        if !self.root.exists() {
            return Err(VaultError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("la vista {} non è raggiungibile", self.root.display()),
            )));
        }
        let gone = |p: &Path| -> Result<(), VaultError> {
            match std::fs::remove_file(p) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(VaultError::Io(e)),
            }
        };
        // the bound file, when this note has one
        if let Some(row) = self
            .cell
            .vault_files()?
            .into_iter()
            .find(|r| r.note_id == id.to_string())
        {
            gone(&self.root.join(&row.path))?;
            self.cell.vault_file_delete(&row.path)?;
        }
        // every conflict copy of the note
        for rel in self.cell.vault_conflicts_of(id)? {
            gone(&self.root.join(&rel))?;
            self.cell.vault_conflict_delete(&rel)?;
        }
        Ok(())
    }

    /// Bind this folder in the cell (`vault_roots`, v0.3.2, P1.6): the
    /// canonical absolute path, so a purge reaches every view without being
    /// told again. Called by every `init`/`sync`.
    fn bind_root(&self) -> Result<(), VaultError> {
        if let Ok(canon) = std::fs::canonicalize(&self.root) {
            self.cell.vault_root_put(&canon.to_string_lossy())?;
        }
        Ok(())
    }

    /// Walk the folder for markdown files: `.obsidian/`, `.trash/` and hidden
    /// entries are skipped, symlinks are never followed, `.conflitto-*.md`
    /// copies are ignored (the import rules).
    fn scan(&self) -> Result<Vec<Scanned>, VaultError> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            entries.sort();
            for path in entries {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let meta = std::fs::symlink_metadata(&path)?;
                if meta.file_type().is_symlink() || name.starts_with('.') {
                    continue;
                }
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "md") || name.contains(".conflitto-") {
                    continue;
                }
                out.push(self.snapshot(&path)?);
            }
        }
        Ok(out)
    }

    /// The export-style path for a note: `<type>/<slug>-<id8>.md` (with the
    /// same -2 disambiguation `port::export` uses).
    fn file_for(&self, note: &Note) -> Result<PathBuf, VaultError> {
        let dir = self.root.join(note.note_type.as_str());
        std::fs::create_dir_all(&dir)?;
        let base = format!(
            "{}-{}",
            engramma_core::port::slugify(note.display_title()),
            engramma_core::id::id8(&note.id)
        );
        let mut name = format!("{base}.md");
        let mut n = 2;
        while let Ok(text) = std::fs::read_to_string(dir.join(&name))
            && id_of(&text).as_deref() != Some(note.id.to_string().as_str())
        {
            // same slug and id8 are rare; keep names unique like the export
            name = format!("{base}-{n}.md");
            n += 1;
            if n > 50 {
                break;
            }
        }
        Ok(dir.join(name))
    }

    fn snapshot(&self, abs: &Path) -> Result<Scanned, VaultError> {
        let text = std::fs::read_to_string(abs)?;
        let meta = std::fs::symlink_metadata(abs)?;
        Ok(Scanned {
            path: abs
                .strip_prefix(&self.root)
                .unwrap_or(abs)
                .to_string_lossy()
                .replace('\\', "/"),
            abs: abs.to_path_buf(),
            hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
            mtime: mtime_of(&meta),
            size: i64::try_from(meta.len()).unwrap_or(0),
            text,
        })
    }

    /// Record what the scan saw for one note (or forget it).
    fn record(&self, file: &Scanned, id: &Ulid, status: Status) -> Result<(), VaultError> {
        if status == Status::Deleted {
            self.cell.vault_file_delete(&file.path)?;
            return Ok(());
        }
        self.cell.vault_file_put(&engramma_core::VaultFile {
            path: file.path.clone(),
            note_id: id.to_string(),
            file_hash: file.hash.clone(),
            note_version: self.cell.note_version(id).unwrap_or(0),
            mtime: file.mtime,
            size: file.size,
        })?;
        Ok(())
    }

    /// One reconciliation. Deterministic: the same folder and the same cell
    /// produce the same report and the same end state.
    ///
    /// # Errors
    /// [`VaultError::MassDelete`] when the deletion guard fires (nothing was
    /// deleted); single-file failures ride the report instead.
    pub fn sync_once(&self, allow_deletions: bool) -> Result<SyncReport, VaultError> {
        let mut report = SyncReport::default();
        let scanned = self.scan()?;
        let known: BTreeMap<String, engramma_core::VaultFile> = self
            .cell
            .vault_files()?
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect();
        let scanned_ids: BTreeSet<String> = scanned.iter().filter_map(|f| id_of(&f.text)).collect();
        let scanned_paths: BTreeSet<&str> = scanned.iter().map(|f| f.path.as_str()).collect();

        // 1. the deletion guard — but a file that merely MOVED is not gone:
        //    its id still scans under another path
        let gone: Vec<&engramma_core::VaultFile> = known
            .values()
            .filter(|f| {
                !scanned_paths.contains(f.path.as_str()) && !scanned_ids.contains(&f.note_id)
            })
            .collect();
        if !gone.is_empty() && !allow_deletions {
            let living = self.living_notes()?;
            let by_count = gone.len() > MASS_DELETE_FILES;
            let by_fraction =
                living > 0 && gone.len() > (MASS_DELETE_FRACTION * living as f64) as usize;
            if by_count || by_fraction {
                return Err(VaultError::MassDelete {
                    would_delete: gone.len(),
                    known: known.len(),
                    pct: MASS_DELETE_FRACTION * 100.0,
                });
            }
        }

        // 2. per-file reconciliation
        for file in &scanned {
            self.reconcile_file(file, &known, &scanned_paths, &mut report)?;
        }

        // 3. living notes no file is bound to: the cell moved first — write
        //    their file (the export naming) and record it. The bindings phase
        //    2 created in THIS sync count (v0.3.1, P1.6): a note imported
        //    from its own file is already bound, and a second export copy
        //    would give it two files to bounce between.
        let bound_now: BTreeMap<String, String> = self
            .cell
            .vault_files()?
            .into_iter()
            .map(|f| (f.note_id, f.path))
            .collect();
        for id in self.cell.note_ids()? {
            if bound_now.contains_key(&id.to_string()) {
                continue;
            }
            let note = match self.cell.get(&id) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let target = self.file_for(&note)?;
            self.write_file(&target, &note)?;
            let fresh = self.snapshot(&target)?;
            self.record(&fresh, &id, note.status)?;
            report.updated_files.push(fresh.path.clone());
        }

        // 4. deletions (the guard has already agreed)
        for row in gone {
            if let Ok(id) = Ulid::from_string(&row.note_id)
                && self.cell.get(&id).is_ok()
            {
                self.cell.delete_note(&id, "vault")?;
                report.deleted.push(row.path.clone());
            }
            self.cell.vault_file_delete(&row.path)?;
        }

        // 5. v0.3.2 (P1.6): finish a purge's work — the `.conflitto` copies
        //    of notes that no longer exist (a purge that could not reach
        //    this view left them, with their rows). Copies of LIVING notes
        //    stay: they are the user's safety copies, and a tombstone is
        //    not a purge.
        for (path, note_id) in self.cell.vault_conflicts()? {
            let Ok(id) = Ulid::from_string(&note_id) else {
                self.cell.vault_conflict_delete(&path)?;
                continue;
            };
            if self.cell.get(&id).is_ok() {
                continue; // living (or tombstoned): the copy stays
            }
            let abs = self.root.join(&path);
            match std::fs::remove_file(&abs) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    report.errors.push(format!("{path}: {e}"));
                    continue;
                }
            }
            self.cell.vault_conflict_delete(&path)?;
        }

        // and this folder is one of the cell's views now
        self.bind_root()?;
        Ok(report)
    }

    fn living_notes(&self) -> Result<usize, VaultError> {
        Ok(self
            .cell
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM notes WHERE status != 'deleted'")
            .ok()
            .and_then(|v| v.first().cloned())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    fn reconcile_file(
        &self,
        file: &Scanned,
        known: &BTreeMap<String, engramma_core::VaultFile>,
        scanned_paths: &BTreeSet<&str>,
        report: &mut SyncReport,
    ) -> Result<(), VaultError> {
        let Some(header_id) = id_of(&file.text) else {
            return self.import_new(file, report);
        };
        let Ok(note_id) = Ulid::from_string(&header_id) else {
            report.errors.push(format!(
                "{}: l'id nell'intestazione non è un ULID",
                file.path
            ));
            return Ok(());
        };
        let parsed = match engramma_core::format::parse(&file.text, ParseMode::Import) {
            Ok(p) => p,
            Err(e) => {
                report.errors.push(format!("{}: {e}", file.path));
                return Ok(());
            }
        };
        // v0.3.2 (P1.8): a file's warnings ride only with the read that
        // earned them — a quiet view must report nothing, or the fixed
        // point would never be silent (unknown keys warned at EVERY sync).
        let row_early = known.get(&file.path);
        let file_changed_early = row_early.is_none_or(|r| r.file_hash != file.hash);
        if file_changed_early {
            report.warnings.extend(parsed.warnings);
        }
        let mut file_note = parsed.note;
        if file_note.id != note_id {
            report.errors.push(format!(
                "{}: l'id dell'intestazione ({header_id}) non concorda con la nota ({})",
                file.path, file_note.id
            ));
            return Ok(());
        }

        // the live note: a tombstone or a purged note is "not there"
        let live = match self.cell.get(&note_id) {
            Ok(n) if n.status == Status::Deleted => None,
            Ok(n) => Some(n),
            Err(_) => None,
        };
        let Some(live_note) = live else {
            if let Some(old) = known.values().find(|r| r.note_id == header_id) {
                // the note was purged or tombstoned elsewhere: the file goes
                // with it (purge deletes the file of its note; a tombstone
                // is the cell saying the note is gone)
                std::fs::remove_file(&file.abs)?;
                self.cell.vault_file_delete(&old.path)?;
                report.deleted.push(file.path.clone());
            } else {
                report.warnings.push(format!(
                    "{}: la nota {header_id} non è in questa cella, file lasciato stare",
                    file.path
                ));
            }
            return Ok(());
        };

        // a moved or renamed file: same id, new path. But when the BOUND
        // file is still on disk this is a second COPY of the same note, not
        // a move: the binding wins (one file per note is structural,
        // v0.3.1), the duplicate is reported and left alone — re-binding it
        // would bounce every edit between the two copies forever.
        if !known.contains_key(&file.path)
            && let Some(old) = known.values().find(|r| r.note_id == header_id)
        {
            if scanned_paths.contains(old.path.as_str()) {
                report.warnings.push(format!(
                    "{}: seconda copia della nota {} ({} è quella legata), lasciata stare",
                    file.path,
                    engramma_core::id::id8(&note_id),
                    old.path
                ));
                return Ok(());
            }
            self.cell.vault_file_delete(&old.path)?;
            report.moved.push(format!("{} ← {}", file.path, old.path));
        }

        // the cell's rule: a file cannot promote an external note to active
        if file_note.confidence == Confidence::External && file_note.status == Status::Active {
            file_note.status = Status::Quarantined;
            report.warnings.push(format!(
                "{}: una nota external nasce in quarantena (`confirm` la promuove)",
                file.path
            ));
        }

        let row = known.get(&file.path);
        let file_changed = row.is_none_or(|r| r.file_hash != file.hash);
        let version_now = self.cell.note_version(&note_id).unwrap_or(0);
        let cell_changed = row.is_none_or(|r| r.note_version != version_now);

        match (file_changed, cell_changed) {
            (false, false) => {
                if row.is_none() {
                    // first sight of a file that already matches the note
                    self.record(file, &note_id, live_note.status)?;
                }
            }
            (false, true) => {
                // the cell moved: rewrite the file atomically
                self.write_file(&file.abs, &live_note)?;
                let fresh = self.snapshot(&file.abs)?;
                self.record(&fresh, &note_id, live_note.status)?;
                report.updated_files.push(file.path.clone());
            }
            (true, false) => {
                // the file moved: apply it to the cell
                if let Err(e) = self.cell.put_note(&file_note, "vault") {
                    report.errors.push(format!("{}: {e}", file.path));
                    return Ok(());
                }
                self.record(file, &note_id, file_note.status)?;
                report.updated_from_file.push(file.path.clone());
            }
            (true, true) => {
                // conflict: the cell wins, the file's version becomes a copy
                let stamp = OffsetDateTime::now_utc()
                    .format(
                        &time::format_description::parse_borrowed::<2>(
                            "[year]-[month]-[day]T[hour]_[minute]_[second]",
                        )
                        .unwrap_or_default(),
                    )
                    .unwrap_or_default();
                let conflict = file.abs.with_file_name(format!(
                    "{}.conflitto-{stamp}.md",
                    file.abs
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                ));
                std::fs::rename(&file.abs, &conflict)?;
                // v0.3.2 (P1.6): the scanner ignores this copy forever —
                // register it, or no purge would ever take it away
                let conflict_rel = conflict
                    .strip_prefix(&self.root)
                    .unwrap_or(&conflict)
                    .to_string_lossy()
                    .replace('\\', "/");
                self.cell.vault_conflict_put(&conflict_rel, &note_id)?;
                self.write_file(&file.abs, &live_note)?;
                let fresh = self.snapshot(&file.abs)?;
                self.record(&fresh, &note_id, live_note.status)?;
                report.conflicts.push(file.path.clone());
            }
        }
        Ok(())
    }

    /// A file without an `id` becomes a note; the file is then rewritten WITH
    /// its id (the Obsidian import rules: `source: import:obsidian/<rel>`).
    /// v0.3.1 (P1.5): a header that is not Engramma's is READ (title, tags,
    /// aliases, a valid type, dates) and the rewrite MERGES it — one `---`
    /// block with the user's fields kept, the body after the header.
    fn import_new(&self, file: &Scanned, report: &mut SyncReport) -> Result<(), VaultError> {
        let parsed = engramma_core::format::parse(&file.text, ParseMode::Import);
        let mut note = match parsed {
            Ok(p) => p.note,
            Err(engramma_core::FormatError::NoFrontmatter)
            | Err(engramma_core::FormatError::MissingField(_)) => {
                let stem = file
                    .abs
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let mtime = OffsetDateTime::from_unix_timestamp(file.mtime)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc())
                    .replace_millisecond(0)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc());
                let p = engramma_core::format::parse_obsidian(
                    &file.text,
                    &stem,
                    engramma_core::format::At(mtime),
                    Ulid::nil(),
                )
                .map_err(VaultError::Format)?;
                report.warnings.extend(p.warnings.clone());
                p.note
            }
            Err(e) => {
                report.errors.push(format!("{}: {e}", file.path));
                return Ok(());
            }
        };
        if note.id != Ulid::nil() {
            report.warnings.push(format!(
                "{}: intestazione valida senza campo id visibile, uso {}",
                file.path, note.id
            ));
        } else {
            note.id = engramma_core::id::new_ulid()?;
        }
        note.source = format!("import:obsidian/{}", file.path);
        note.confidence = Confidence::Stated;
        note.status = Status::Active;
        if let Err(e) = self.cell.put_note(&note, "vault") {
            report.errors.push(format!("{}: {e}", file.path));
            return Ok(());
        }
        self.write_file(&file.abs, &note)?;
        let fresh = self.snapshot(&file.abs)?;
        self.record(&fresh, &note.id, note.status)?;
        report.created.push(file.path.clone());
        Ok(())
    }

    /// Atomic write: a temporary in the same folder, then rename.
    fn write_file(&self, target: &Path, note: &Note) -> Result<(), VaultError> {
        let text = note.serialize()?;
        let tmp = target.with_file_name(format!(
            ".{}-{}.tmp",
            engramma_core::id::id8(&engramma_core::id::new_ulid()?),
            target
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, target)?;
        Ok(())
    }
}

/// A running watch: filesystem events (debounced) trigger a scan, a periodic
/// full scan is the safety net, and the handle stops it cleanly.
pub struct WatchHandle {
    stop: std::sync::mpsc::Sender<()>,
    joined: std::thread::JoinHandle<()>,
}

impl WatchHandle {
    /// Signal the loop to stop and wait for the last reconciliation to end.
    ///
    /// # Errors
    /// [`VaultError::Io`]-shaped join failure is flattened to `Ok` on cancel;
    /// a panic in the loop surfaces as an io error.
    pub fn stop(self) -> Result<(), VaultError> {
        let _ = self.stop.send(());
        self.joined
            .join()
            .map_err(|_| std::io::Error::other("the watch loop panicked"))
            .map_err(VaultError::Io)
    }
}

impl Vault {
    /// Watch the folder: events (debounced by `debounce_ms`, predefinito
    /// 500) trigger one reconciliation each, and a full scan every `period`
    /// catches what the watcher missed. Returns immediately; stop with
    /// [`WatchHandle::stop`].
    ///
    /// # Errors
    /// [`VaultError::Io`] when the folder cannot be watched.
    pub fn watch(
        &self,
        debounce_ms: u64,
        period: std::time::Duration,
    ) -> Result<WatchHandle, VaultError> {
        use notify::RecursiveMode;
        use notify_debouncer_full::new_debouncer;

        std::fs::create_dir_all(&self.root)?;
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(
            std::time::Duration::from_millis(debounce_ms.max(1)),
            None,
            event_tx,
        )
        .map_err(|e| std::io::Error::other(format!("watcher: {e}")))?;
        debouncer
            .watch(&self.root, RecursiveMode::Recursive)
            .map_err(|e| std::io::Error::other(format!("watch: {e}")))?;
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let vault = Vault {
            cell: self.cell.clone(),
            root: self.root.clone(),
        };
        let joined = std::thread::spawn(move || {
            let debouncer = &mut debouncer; // keep the watcher alive
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match event_rx.recv_timeout(period) {
                    Ok(_events) => {
                        // the debouncer already waited: reconcile now
                        let _ = vault.sync_once(false);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // the safety net: watchers lose events
                        let _ = vault.sync_once(false);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            let _ = debouncer; // dropped here: watcher unsubscribed
        });
        Ok(WatchHandle {
            stop: stop_tx,
            joined,
        })
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The `id:` field of a frontmatter, without a full parse (the real parse
/// follows and decides).
fn id_of(text: &str) -> Option<String> {
    let rest = text.strip_prefix("---\n")?;
    for line in rest.lines() {
        if line.trim() == "---" {
            break;
        }
        if let Some(v) = line.trim().strip_prefix("id:") {
            let v = v.trim();
            return Some(v.trim_matches(['\'', '"']).to_string());
        }
    }
    None
}
