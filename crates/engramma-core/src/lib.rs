// SPDX-License-Identifier: Apache-2.0

//! Engramma core: note format, cells, change log, search (v0.1).

pub mod cell;
pub mod error;
pub mod format;
pub mod hlc;
pub mod id;
pub mod port;
pub mod search;
pub mod text;

pub use error::{CellError, FormatError, PortError, SearchError};
pub use port::{export, import_cell, import_obsidian};
pub use format::{Confidence, Lang, Note, NoteType, ParseMode, Status, Validity};
pub use cell::{Cell, NotePatch, VerifyReport};
pub use search::{Explain, Filters, Hit, Query, Weights};
pub use hlc::{Hlc, HlcClock};

#[cfg(test)]
mod setup_checks {
    /// The bundled SQLite must have FTS5 (the text index depends on it).
    #[test]
    fn bundled_sqlite_has_fts5() {
        let c = rusqlite::Connection::open_in_memory().unwrap(/* test-only */);
        c.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(x, tokenize='unicode61 remove_diacritics 2');",
        )
        .unwrap(/* test-only */);
    }
}
