//! Engramma core: note format, cells, change log, search. (v0.1 in progress)

#[cfg(test)]
mod setup_checks {
    /// The bundled SQLite must have FTS5 (the text index depends on it).
    #[test]
    fn bundled_sqlite_has_fts5() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x, tokenize='unicode61 remove_diacritics 2');").unwrap();
    }
}
