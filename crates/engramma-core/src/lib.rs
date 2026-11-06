// SPDX-License-Identifier: Apache-2.0

//! Engramma core: note format, cells, change log, search, embedding
//! contract (v0.2).

pub mod cell;
pub mod embed;
pub mod error;
pub mod format;
pub mod graph;
pub mod hlc;
pub mod id;
pub mod port;
pub mod search;
pub mod text;
pub mod vector;

pub use cell::{Cell, NotePatch, VaultFile, VerifyReport};
pub use cell::{EmbedPendingReport, ModelCoverage, ModelRow, ModelState};
pub use embed::{Embedder, TextKind};
pub use error::{CellError, EmbedError, FormatError, PortError, SearchError};
pub use format::{Confidence, Lang, Note, NoteType, ParseMode, Status, Validity};
pub use hlc::{Hlc, HlcClock};
pub use port::{export, import_cell, import_obsidian};
pub use search::{
    CosNorm, Explain, Filters, Fusion, Hit, Mode, Query, SearchOutcome, Semantic, Weights,
};
pub use vector::{FlatI8, IndexHit, IndexRow, VectorIndex};

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
