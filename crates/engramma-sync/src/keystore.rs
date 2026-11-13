// SPDX-License-Identifier: Apache-2.0

//! The cell keys a data-root holds (v0.6, P4/P5): each shared cell's
//! epoch keys, and the wraps that carry them to the other devices. Lives
//! in `devices.db` beside the registry — the same root of trust.

use rusqlite::{Connection, OptionalExtension};

use crate::crypto::EpochKey;
use crate::{Error, Result};

/// The key store: a handle onto the devices.db connection.
pub struct KeyStore<'a> {
    conn: &'a Connection,
}

impl<'a> KeyStore<'a> {
    /// Wrap the registry's connection.
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Create epoch 0 for a cell: the sharer mints the key.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn create_epoch0(&self, sync_id: &[u8; 32]) -> Result<EpochKey> {
        let key = EpochKey::generate()?;
        self.conn.execute(
            "INSERT OR REPLACE INTO cell_keys (sync_id, epoch, key, wrapped_for, wrapped, created_at)
             VALUES (?1, 0, ?2, NULL, NULL, ?3)",
            rusqlite::params![sync_id, key.0, now_iso()],
        )?;
        Ok(key)
    }

    /// Open epoch k+1 for a cell (a device was removed): everyone gets a
    /// new key the removed device will never see.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn rotate(&self, sync_id: &[u8; 32]) -> Result<(u64, EpochKey)> {
        let next: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(epoch), -1) + 1 FROM cell_keys WHERE sync_id = ?1",
                [sync_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let key = EpochKey::generate()?;
        self.conn.execute(
            "INSERT OR REPLACE INTO cell_keys (sync_id, epoch, key, wrapped_for, wrapped, created_at)
             VALUES (?1, ?2, ?3, NULL, NULL, ?4)",
            rusqlite::params![sync_id, next, key.0, now_iso()],
        )?;
        Ok((u64::try_from(next).unwrap_or(0), key))
    }

    /// Store a wrap received for this device.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn store_wrap(
        &self,
        sync_id: &[u8; 32],
        epoch: u64,
        for_device: &str,
        wrapped: &[u8],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO cell_keys (sync_id, epoch, key, wrapped_for, wrapped, created_at)
             VALUES (?1, ?2, x'', ?3, ?4, ?5)",
            rusqlite::params![sync_id, epoch_i64(epoch), for_device, wrapped, now_iso()],
        )?;
        Ok(())
    }

    /// Store an unwrapped key this device received (or created).
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn store_key(&self, sync_id: &[u8; 32], epoch: u64, key: &EpochKey) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO cell_keys (sync_id, epoch, key, wrapped_for, wrapped, created_at)
             VALUES (?1, ?2, ?3, NULL, NULL, ?4)",
            rusqlite::params![sync_id, epoch_i64(epoch), key.0, now_iso()],
        )?;
        Ok(())
    }

    /// The key of an epoch, when this device holds it.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn key_of(&self, sync_id: &[u8; 32], epoch: u64) -> Result<Option<EpochKey>> {
        let row: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT key FROM cell_keys WHERE sync_id = ?1 AND epoch = ?2 AND wrapped_for IS NULL",
                rusqlite::params![sync_id, epoch_i64(epoch)],
                |r| r.get(0),
            )
            .optional()?;
        let Some(k) = row else { return Ok(None) };
        let mut bytes = [0_u8; 32];
        if k.len() != 32 {
            return Err(Error::Malformed("epoch key must be 32 bytes".into()));
        }
        bytes.copy_from_slice(&k);
        Ok(Some(EpochKey(bytes)))
    }

    /// The wraps this device must send to a peer for an epoch.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn wraps_for(&self, sync_id: &[u8; 32], epoch: u64) -> Result<Vec<(String, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT wrapped_for, wrapped FROM cell_keys
             WHERE sync_id = ?1 AND epoch = ?2 AND wrapped_for IS NOT NULL",
        )?;
        let rows = stmt.query_map(rusqlite::params![sync_id, epoch_i64(epoch)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every epoch this device holds a key for, newest first.
    ///
    /// # Errors
    /// [`Error::Db`] on SQL failure.
    pub fn epochs_of(&self, sync_id: &[u8; 32]) -> Result<Vec<u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT epoch FROM cell_keys WHERE sync_id = ?1 ORDER BY epoch DESC",
        )?;
        let rows = stmt.query_map([sync_id], |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(u64::try_from(row?).unwrap_or(0));
        }
        Ok(out)
    }
}

fn epoch_i64(epoch: u64) -> i64 {
    i64::try_from(epoch).unwrap_or(0)
}

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .replace_millisecond(0)
        .unwrap_or(time::OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
