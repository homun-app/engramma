// SPDX-License-Identifier: Apache-2.0

//! v0.5.2 (P2/P3): `oauth.db` — the whole OAuth state of the data root:
//! clients (CIMD cache and dynamic registrations), authorization requests
//! waiting for a human, one-shot codes, grants, access and refresh tokens,
//! and the event log the CLI shows.
//!
//! It is a SEPARATE file from the cells on purpose: these are credentials,
//! not memory. They must not ride the v0.6 replication, and their revocation
//! has to be immediate and local (DESIGN § 8.6). Of every secret only
//! `blake3` is stored; identifiers a human must retype (the approval code)
//! or that live in a URL (the request handle) stay as they are.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::OptionalExtension;
use time::OffsetDateTime;

use super::mint;

/// The one schema this code speaks. A file with a NEWER schema is refused,
/// exactly like the cells: a future engramma may have opened this root.
pub const SCHEMA_VERSION: i64 = 1;

/// The scopes this server knows. Unknown scopes in a request (ChatGPT asks
/// for `openid email profile`) are ignored, not refused — we do not speak
/// OIDC; the token answer carries what was REALLY granted.
pub const SCOPE_READ: &str = "memory:read";
pub const SCOPE_PROPOSE: &str = "memory:propose";

/// Everything that can go wrong in the store; the surfaces above turn
/// these into their own words.
#[derive(Debug)]
pub enum StoreError {
    /// A schema newer than this code.
    SchemaTooNew(i64),
    /// SQL or I/O trouble.
    Db(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::SchemaTooNew(v) => write!(
                f,
                "oauth.db speaks schema v{v}, this engramma only knows v{SCHEMA_VERSION} \
                 (a newer engramma opened this root)"
            ),
            StoreError::Db(e) => write!(f, "oauth.db: {e}"),
        }
    }
}

type Result<T> = std::result::Result<T, StoreError>;

fn db(e: impl std::fmt::Display) -> StoreError {
    StoreError::Db(e.to_string())
}

/// One raw `clients` row, before parsing.
type ClientRaw = (String, Option<String>, i64, Option<i64>, String);
/// One raw `requests` row, before parsing.
type RequestRaw = (
    String,         // handle
    String,         // approval_code
    String,         // client_id
    Option<String>, // client_name
    String,         // redirect_uri
    String,         // scope
    Option<String>, // resource
    String,         // code_challenge
    Option<String>, // state
    String,         // offered_cells
    String,         // offered_write
    String,         // status
    Option<String>, // grant_id
    i64,            // created_at
    i64,            // expires_at
);
/// One raw `codes` row, before parsing.
type CodeRaw = (
    bool,
    i64,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
);
/// One raw access `tokens` row, before parsing.
type AccessRaw = (bool, i64, String, String, String);
/// One raw refresh `tokens` row, before parsing.
type RefreshRaw = (bool, i64, String, String, String, String);
/// One raw full `grants` row, before parsing.
type GrantRaw = (
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    i64,
);

/// One registered client, whatever registered it.
#[derive(Debug, Clone)]
pub struct ClientRecord {
    /// For CIMD, the client_id URL; for DCR, the minted identifier.
    pub client_id: String,
    /// How this client became known.
    pub kind: ClientKind,
    /// Every registered redirect URI (already validated).
    pub redirect_uris: Vec<String>,
    /// The declared name (hostile text, shown escaped).
    pub name: Option<String>,
    pub created_at: i64,
}

/// How a client became known to us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientKind {
    /// Client ID Metadata Document, fetched and cached until this instant.
    Cimd { cache_until: i64 },
    /// RFC 7591 dynamic registration (`--allow-dcr`), public client.
    Dcr,
}

/// One authorization request, as `/authorize` recorded it.
#[derive(Debug, Clone)]
pub struct AuthRequest {
    /// The waiting page's URL handle (256 random bits, not a secret).
    pub handle: String,
    /// The 8 characters the human retypes (`XXXX-XXXX`, not a secret).
    pub approval_code: String,
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    /// The `resource` the client named, when any.
    pub resource: Option<String>,
    /// S256 code challenge.
    pub code_challenge: String,
    /// The client's `state`, echoed back on every redirect.
    pub state: Option<String>,
    /// What the SERVER process offers (its `--cells`): the grant can only
    /// restrict this, and the approval happens in another process.
    pub offered_cells: Vec<String>,
    /// The server process's `--write` cells (the propose ceiling).
    pub offered_write: Vec<String>,
    pub status: RequestStatus,
    pub grant_id: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// Where an authorization request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStatus {
    /// Waiting for the human.
    Pending,
    /// Approved on this computer: the grant exists.
    Approved,
    /// Refused on this computer.
    Denied,
}

impl RequestStatus {
    fn as_str(&self) -> &'static str {
        match self {
            RequestStatus::Pending => "pending",
            RequestStatus::Approved => "approved",
            RequestStatus::Denied => "denied",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "pending" => RequestStatus::Pending,
            "approved" => RequestStatus::Approved,
            "denied" => RequestStatus::Denied,
            other => return Err(db(format!("bad request status `{other}`"))),
        })
    }
}

/// One grant: a labeled bundle of what one connected client may do.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub id: String,
    /// The label chosen at approval: proposals sign as `mcp:<label>`.
    pub label: String,
    pub client_id: String,
    /// The cells this grant may see (already intersected at approval).
    pub cells: Vec<String>,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    /// created_at + 90 days: no refresh may live past this.
    pub absolute_expiry: i64,
}

/// What a consumed authorization code proved.
#[derive(Debug, Clone)]
pub struct CodeRecord {
    pub grant_id: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub resource: Option<String>,
    pub code_challenge: String,
    /// The revocation family shared by every token born from this code.
    pub family: String,
}

/// Why a token did not pass.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenCheck {
    /// Valid: the grant and the scopes it carries.
    Valid {
        grant: Grant,
        scopes: Vec<String>,
    },
    Unknown,
    Expired,
    Revoked,
    /// The token is fine but was minted for another audience.
    WrongAudience,
}

/// The OAuth store. One SQLite connection behind a mutex: the volume is a
/// few requests a minute, correctness before concurrency.
#[derive(Clone)]
pub struct OAuthStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl OAuthStore {
    /// Open (or create) `<root>/oauth.db`. New files are created with 0600
    /// permissions: this is a credentials file.
    ///
    /// # Errors
    /// [`StoreError::SchemaTooNew`] when the file speaks a newer schema.
    pub fn open(path: &Path) -> Result<Self> {
        let fresh = !path.exists();
        let conn = if fresh {
            // create with 0600 from the very first byte
            let mut opts = rusqlite::OpenFlags::default();
            opts.insert(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE);
            opts.insert(rusqlite::OpenFlags::SQLITE_OPEN_CREATE);
            let conn = rusqlite::Connection::open_with_flags(path, opts).map_err(db)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(db)?;
            }
            conn
        } else {
            rusqlite::Connection::open(path).map_err(db)?
        };
        let mut conn = conn;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db)?;
        if fresh {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(db)?;
            tx.execute_batch(SCHEMA).map_err(db)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                [SCHEMA_VERSION.to_string()],
            )
            .map_err(db)?;
            tx.commit().map_err(db)?;
        }
        // v0.5.3 (P3.1): the negative CIMD cache — additive and idempotent,
        // unversioned like the cells' fts5vocab view: existing roots gain it
        // on their next open, no older binary is refused.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cimd_refused (
               client_id TEXT PRIMARY KEY,
               until     INTEGER NOT NULL,
               reason    TEXT NOT NULL
             );",
        )
        .map_err(db)?;
        let found: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(db)?
            .unwrap_or(0);
        if found > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew(found));
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(
        &self,
    ) -> std::result::Result<std::sync::MutexGuard<'_, rusqlite::Connection>, StoreError> {
        self.conn.lock().map_err(|_| db("store lock poisoned"))
    }

    // -- events -----------------------------------------------------------

    /// Record one event (never a secret, never note text).
    pub fn event(&self, now: i64, kind: &str, detail: &str) {
        let Ok(conn) = self.lock() else { return };
        let _ = conn.execute(
            "INSERT INTO events (at, kind, detail) VALUES (?1, ?2, ?3)",
            rusqlite::params![now, kind, detail],
        );
    }

    /// The whole event log, oldest first.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn events(&self) -> Result<Vec<(i64, String, String)>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT at, kind, detail FROM events ORDER BY seq")
            .map_err(db)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(db)?
            .flatten()
            .collect();
        Ok(rows)
    }

    // -- clients ----------------------------------------------------------

    /// Cache a validated CIMD client document.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn cimd_put(&self, client_id: &str, rec: &ClientRecord, cache_until: i64) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO clients (client_id, kind, redirect_uris, name, created_at, cache_until)
             VALUES (?1, 'cimd', ?2, ?3, ?4, ?5)
             ON CONFLICT(client_id) DO UPDATE SET
               redirect_uris = excluded.redirect_uris,
               name = excluded.name,
               cache_until = excluded.cache_until",
            rusqlite::params![
                client_id,
                serde_json::to_string(&rec.redirect_uris).unwrap_or_default(),
                rec.name,
                rec.created_at,
                cache_until
            ],
        )
        .map_err(db)?;
        Ok(())
    }

    /// The cached CIMD document for a client, when the cache still holds.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn cimd_get(&self, client_id: &str, now: i64) -> Result<Option<ClientRecord>> {
        let conn = self.lock()?;
        let row: Option<(String, Option<String>, i64)> = conn
            .query_row(
                "SELECT redirect_uris, name, created_at FROM clients
                 WHERE client_id = ?1 AND kind = 'cimd' AND cache_until > ?2",
                rusqlite::params![client_id, now],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(db)?;
        let Some((uris, name, created_at)) = row else {
            return Ok(None);
        };
        Ok(Some(ClientRecord {
            client_id: client_id.to_string(),
            kind: ClientKind::Cimd { cache_until: 0 },
            redirect_uris: serde_json::from_str(&uris).unwrap_or_default(),
            name,
            created_at,
        }))
    }

    /// How many DCR clients are registered and not yet used (the cap is 20).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn dcr_unused_count(&self) -> Result<usize> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clients WHERE kind = 'dcr' AND used_at IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(db)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Register a DCR client; the caller minted the client_id.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn dcr_put(&self, rec: &ClientRecord) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO clients (client_id, kind, redirect_uris, name, created_at, cache_until)
             VALUES (?1, 'dcr', ?2, ?3, ?4, NULL)",
            rusqlite::params![
                rec.client_id,
                serde_json::to_string(&rec.redirect_uris).unwrap_or_default(),
                rec.name,
                rec.created_at
            ],
        )
        .map_err(db)?;
        Ok(())
    }

    /// Look a client up (CIMD cache or DCR), fresh or not. Marks a DCR
    /// client as used (the 24 h expiry of unused ones stops applying).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn client_get(&self, client_id: &str, now: i64) -> Result<Option<ClientRecord>> {
        let conn = self.lock()?;
        let row: Option<ClientRaw> = conn
            .query_row(
                "SELECT redirect_uris, name, created_at, cache_until, kind FROM clients
                 WHERE client_id = ?1",
                [client_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(db)?;
        let Some((uris, name, created_at, cache_until, kind)) = row else {
            return Ok(None);
        };
        let kind = if kind == "dcr" {
            // v0.5.3 (P3.6), the lifecycle of a dynamic registration:
            // - MAI usato (used_at NULL): scade dopo 24 h;
            // - USATO: resta finché ha un grant vivo — la registrazione era
            //   un mezzo per il collegamento, e con tutti i collegamenti
            //   morti non c'è più niente da servire.
            let state: Option<(bool, bool)> = conn
                .query_row(
                    "SELECT used_at IS NOT NULL,
                            NOT EXISTS (SELECT 1 FROM grants
                              WHERE client_id = ?1 AND revoked_at IS NULL AND absolute_expiry > ?2)
                     FROM clients WHERE client_id = ?1",
                    rusqlite::params![client_id, now],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(db)?;
            match state {
                // never used and older than a day: gone
                Some((false, _)) => {
                    let expired: bool = conn
                        .query_row(
                            "SELECT created_at < ?2 FROM clients WHERE client_id = ?1",
                            rusqlite::params![client_id, now - 24 * 3600],
                            |r| r.get(0),
                        )
                        .unwrap_or(true);
                    if expired {
                        let _ =
                            conn.execute("DELETE FROM clients WHERE client_id = ?1", [client_id]);
                        return Ok(None);
                    }
                }
                // used, and every grant of its is dead: gone
                Some((true, true)) => {
                    let _ = conn.execute("DELETE FROM clients WHERE client_id = ?1", [client_id]);
                    return Ok(None);
                }
                _ => {}
            }
            let _ = conn.execute(
                "UPDATE clients SET used_at = ?2 WHERE client_id = ?1 AND used_at IS NULL",
                rusqlite::params![client_id, now],
            );
            ClientKind::Dcr
        } else {
            ClientKind::Cimd {
                cache_until: cache_until.unwrap_or(0),
            }
        };
        Ok(Some(ClientRecord {
            client_id: client_id.to_string(),
            kind,
            redirect_uris: serde_json::from_str(&uris).unwrap_or_default(),
            name,
            created_at,
        }))
    }

    /// v0.5.3 (P3.1): is this client_id in the negative CIMD cache (a
    /// document that was refused or unreachable, for the next five
    /// minutes)? The REASON comes back for the event log — the wire stays
    /// generic.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn cimd_refused_get(&self, client_id: &str, now: i64) -> Result<Option<String>> {
        let conn = self.lock()?;
        let reason: Option<String> = conn
            .query_row(
                "SELECT reason FROM cimd_refused WHERE client_id = ?1 AND until > ?2",
                rusqlite::params![client_id, now],
                |r| r.get(0),
            )
            .optional()
            .map_err(db)?;
        Ok(reason)
    }

    /// v0.5.3 (P3.1): park a refused/unreachable client_id for five
    /// minutes: whoever knows the /authorize URL must not keep the process
    /// downloading, one refusal per URL per window is enough.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn cimd_refused_put(&self, client_id: &str, until: i64, reason: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO cimd_refused (client_id, until, reason)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(client_id) DO UPDATE SET until = ?2, reason = ?3",
            rusqlite::params![client_id, until, reason],
        )
        .map_err(db)?;
        Ok(())
    }

    // -- authorization requests --------------------------------------------

    /// Record a fresh pending request. The caller minted handle and
    /// approval code; the ceiling of 5 pending requests is the caller's
    /// to check first ([`OAuthStore::pending_count`]).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn request_insert(&self, req: &AuthRequest) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO requests
               (handle, approval_code, client_id, client_name, redirect_uri, scope,
                resource, code_challenge, state, offered_cells, offered_write,
                status, grant_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            rusqlite::params![
                req.handle,
                req.approval_code,
                req.client_id,
                req.client_name,
                req.redirect_uri,
                serde_json::to_string(&req.scopes).unwrap_or_default(),
                req.resource,
                req.code_challenge,
                req.state,
                serde_json::to_string(&req.offered_cells).unwrap_or_default(),
                serde_json::to_string(&req.offered_write).unwrap_or_default(),
                req.status.as_str(),
                req.grant_id,
                req.created_at,
                req.expires_at
            ],
        )
        .map_err(db)?;
        // opportunistic cleaning: requests past their life are refused, not deleted;
        // expired codes and long-dead tokens can go
        let _ = conn.execute(
            "DELETE FROM codes WHERE expires_at < ?1",
            [now_minus(7 * 24 * 3600)],
        );
        Ok(())
    }

    /// The pending requests, oldest first (the CLI's `oauth pending`).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn requests_pending(&self, now: i64) -> Result<Vec<AuthRequest>> {
        self.requests_where("status = 'pending'", now)
    }

    /// How many requests are waiting right now (the global ceiling of 10,
    /// v0.5.3 P3.1).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn pending_count(&self, now: i64) -> Result<usize> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM requests WHERE status = 'pending' AND expires_at > ?1",
                [now],
                |r| r.get(0),
            )
            .map_err(db)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// v0.5.3 (P3.1): how many requests THIS client has waiting — the
    /// per-client ceiling is 2, so one client alone can never fill the
    /// queue (a spammer with many domains still can: declared in DESIGN).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn pending_count_of(&self, client_id: &str, now: i64) -> Result<usize> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM requests
                 WHERE status = 'pending' AND expires_at > ?1 AND client_id = ?2",
                rusqlite::params![now, client_id],
                |r| r.get(0),
            )
            .map_err(db)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    fn requests_where(&self, cond: &str, now: i64) -> Result<Vec<AuthRequest>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT handle, approval_code, client_id, client_name, redirect_uri, scope,
                        resource, code_challenge, state, offered_cells, offered_write,
                        status, grant_id, created_at, expires_at
                 FROM requests WHERE {cond} ORDER BY created_at"
            ))
            .map_err(db)?;
        let raw = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, i64>(13)?,
                    r.get::<_, i64>(14)?,
                ))
            })
            .map_err(db)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db)?
            .into_iter()
            // a pending request lives until its EXPIRY (field 14; verifica
            // v0.5.2: the filter read field 13, the creation time, and
            // `oauth pending` never listed anything)
            .filter(|r| r.14 > now || r.11 != "pending")
            .collect::<Vec<_>>();
        drop(stmt);
        let mut out = Vec::new();
        for r in raw {
            let (
                handle,
                code,
                client_id,
                client_name,
                redirect_uri,
                scope,
                resource,
                challenge,
                state,
                offered_cells,
                offered_write,
                status,
                grant_id,
                created_at,
                expires_at,
            ) = r;
            out.push(AuthRequest {
                handle,
                approval_code: code,
                client_id,
                client_name,
                redirect_uri,
                scopes: serde_json::from_str(&scope).unwrap_or_default(),
                resource,
                code_challenge: challenge,
                state,
                offered_cells: serde_json::from_str(&offered_cells).unwrap_or_default(),
                offered_write: serde_json::from_str(&offered_write).unwrap_or_default(),
                status: RequestStatus::parse(&status)?,
                grant_id,
                created_at,
                expires_at,
            });
        }
        Ok(out)
    }

    /// One request by its approval code (what the human types).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn request_by_code(&self, code: &str) -> Result<Option<AuthRequest>> {
        let conn = self.lock()?;
        let handle: Option<String> = conn
            .query_row(
                "SELECT handle FROM requests WHERE approval_code = ?1",
                [code],
                |r| r.get(0),
            )
            .optional()
            .map_err(db)?;
        drop(conn);
        let Some(handle) = handle else {
            return Ok(None);
        };
        self.request_by_handle(&handle)
    }

    /// One request by its waiting-page handle.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn request_by_handle(&self, handle: &str) -> Result<Option<AuthRequest>> {
        let conn = self.lock()?;
        let row: Option<RequestRaw> = conn
            .query_row(
                "SELECT handle, approval_code, client_id, client_name, redirect_uri, scope,
                        resource, code_challenge, state, offered_cells, offered_write,
                        status, grant_id, created_at, expires_at
                 FROM requests WHERE handle = ?1",
                [handle],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                        r.get(12)?,
                        r.get(13)?,
                        r.get(14)?,
                    ))
                },
            )
            .optional()
            .map_err(db)?;
        drop(conn);
        let Some((
            handle2,
            code,
            client_id,
            client_name,
            redirect_uri,
            scope,
            resource,
            challenge,
            state,
            offered_cells,
            offered_write,
            status,
            grant_id,
            created_at,
            expires_at,
        )) = row
        else {
            return Ok(None);
        };
        Ok(Some(AuthRequest {
            handle: handle2,
            approval_code: code,
            client_id,
            client_name,
            redirect_uri,
            scopes: serde_json::from_str(&scope).unwrap_or_default(),
            resource,
            code_challenge: challenge,
            state,
            offered_cells: serde_json::from_str(&offered_cells).unwrap_or_default(),
            offered_write: serde_json::from_str(&offered_write).unwrap_or_default(),
            status: RequestStatus::parse(&status)?,
            grant_id,
            created_at,
            expires_at,
        }))
    }

    /// Approve a pending request: mint the grant (label, cells, scopes —
    /// only what the process offers) and mark the request. The grant's
    /// absolute life is 90 days.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn request_approve(
        &self,
        approval_code: &str,
        label: &str,
        cells: &[String],
        scopes: &[String],
        now: i64,
    ) -> Result<Option<Grant>> {
        let Some(mut req) = self.request_by_code(approval_code)? else {
            return Ok(None);
        };
        if req.status != RequestStatus::Pending || req.expires_at <= now {
            return Ok(None);
        }
        // a grant past its 90 days is dead even if nobody revoked it: it
        // gives its label back before the new one takes it
        {
            let conn = self.lock()?;
            conn.execute(
                "UPDATE grants SET revoked_at = ?2
                 WHERE label = ?1 AND revoked_at IS NULL AND absolute_expiry <= ?2",
                rusqlite::params![label, now],
            )
            .map_err(db)?;
        }
        let grant = Grant {
            id: engramma_core::id::new_ulid()
                .map(|u| u.to_string())
                .map_err(|e| db(e.to_string()))?,
            label: label.to_string(),
            client_id: req.client_id.clone(),
            cells: cells.to_vec(),
            scopes: scopes.to_vec(),
            created_at: now,
            last_used_at: None,
            revoked_at: None,
            absolute_expiry: now + 90 * 24 * 3600,
        };
        {
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO grants (id, label, client_id, cells, scopes, created_at, absolute_expiry)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    grant.id,
                    grant.label,
                    grant.client_id,
                    serde_json::to_string(&grant.cells).unwrap_or_default(),
                    serde_json::to_string(&grant.scopes).unwrap_or_default(),
                    now,
                    grant.absolute_expiry
                ],
            )
            .map_err(db)?;
            conn.execute(
                "UPDATE requests SET status = 'approved', grant_id = ?2
                 WHERE approval_code = ?1 AND status = 'pending'",
                rusqlite::params![approval_code, grant.id],
            )
            .map_err(db)?;
        }
        req.status = RequestStatus::Approved;
        req.grant_id = Some(grant.id.clone());
        self.event(
            now,
            "grant_approved",
            &format!("label={} client={}", grant.label, host_of(&grant.client_id)),
        );
        Ok(Some(grant))
    }

    /// Refuse a pending request.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn request_deny(&self, approval_code: &str, now: i64) -> Result<bool> {
        let n = {
            let conn = self.lock()?;
            conn.execute(
                "UPDATE requests SET status = 'denied' WHERE approval_code = ?1 AND status = 'pending'",
                [approval_code],
            )
            .map_err(db)?
        };
        if n > 0 {
            self.event(now, "request_denied", &format!("code={approval_code}"));
        }
        Ok(n > 0)
    }

    /// v0.5.3 (P3.1): refuse EVERY pending request (`engramma oauth deny
    /// --all`) — the flood answer. Returns how many went. The waiting
    /// pages redirect each browser back with `access_denied`.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn request_deny_all(&self, now: i64) -> Result<usize> {
        let n = {
            let conn = self.lock()?;
            conn.execute(
                "UPDATE requests SET status = 'denied'
                 WHERE status = 'pending' AND expires_at > ?1",
                [now],
            )
            .map_err(db)?
        };
        if n > 0 {
            self.event(now, "request_denied", &format!("--all: {n} richieste"));
        }
        Ok(n)
    }

    /// Mint the one-shot authorization code of an approved request, the
    /// first time the waiting page asks. `None` when the handle is unknown,
    /// the request was never approved, or the code was already minted (the
    /// browser that missed the redirect starts over from the app).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn code_mint(
        &self,
        handle: &str,
        canonical_resource: &str,
        now: i64,
    ) -> Result<Option<(String, CodeRecord, i64)>> {
        let Some(req) = self.request_by_handle(handle)? else {
            return Ok(None);
        };
        if req.status != RequestStatus::Approved || req.expires_at <= now {
            return Ok(None);
        }
        let Some(grant_id) = req.grant_id.clone() else {
            return Ok(None);
        };
        let code = mint::opaque_token("eng_code_").map_err(|e| db(e.to_string()))?;
        let family = engramma_core::id::new_ulid()
            .map(|u| u.to_string())
            .map_err(|e| db(e.to_string()))?;
        let expires = now + 60;
        let code_hash = mint::hash_hex(&code);
        let record = CodeRecord {
            grant_id,
            client_id: req.client_id,
            redirect_uri: req.redirect_uri,
            scopes: req.scopes,
            // the resource the code binds: what the client asked for, or
            // our canonical one when it asked for none
            resource: Some(
                req.resource
                    .unwrap_or_else(|| canonical_resource.to_string()),
            ),
            code_challenge: req.code_challenge,
            family,
        };
        {
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO codes (code_hash, handle, client_id, redirect_uri, scope, resource,
                                    code_challenge, grant_id, family, issued_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    code_hash,
                    handle,
                    record.client_id,
                    record.redirect_uri,
                    serde_json::to_string(&record.scopes).unwrap_or_default(),
                    record.resource,
                    record.code_challenge,
                    record.grant_id,
                    record.family,
                    now,
                    expires
                ],
            )
            .map_err(db)?;
        }
        Ok(Some((code, record, expires)))
    }

    /// Whether this handle's code was already minted (the waiting page
    /// says «already completed» instead of redirecting again).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn code_already_minted(&self, handle: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM codes WHERE handle = ?1",
                [handle],
                |r| r.get(0),
            )
            .map_err(db)?;
        Ok(n > 0)
    }

    /// Consume an authorization code: the first use returns what it proved,
    /// a SECOND use trips the alarm (the caller revokes the family).
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn code_consume(
        &self,
        code: &str,
        now: i64,
    ) -> Result<std::result::Result<CodeRecord, CodeUse>> {
        let hash = mint::hash_hex(code);
        let conn = self.lock()?;
        let row: Option<CodeRaw> = conn
            .query_row(
                "SELECT used, expires_at, client_id, redirect_uri, scope, resource,
                        code_challenge, grant_id, family
                 FROM codes WHERE code_hash = ?1",
                [hash.as_str()],
                |r| {
                    Ok((
                        r.get::<_, bool>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .optional()
            .map_err(db)?;
        let Some((
            used,
            expires_at,
            client_id,
            redirect_uri,
            scope,
            resource,
            code_challenge,
            grant_id,
            family,
        )) = row
        else {
            return Ok(Err(CodeUse::Unknown));
        };
        if used {
            return Ok(Err(CodeUse::Reused { family }));
        }
        if expires_at <= now {
            return Ok(Err(CodeUse::Expired));
        }
        conn.execute(
            "UPDATE codes SET used = 1 WHERE code_hash = ?1",
            [hash.as_str()],
        )
        .map_err(db)?;
        Ok(Ok(CodeRecord {
            grant_id,
            client_id,
            redirect_uri,
            scopes: serde_json::from_str(&scope).unwrap_or_default(),
            resource,
            code_challenge,
            family,
        }))
    }

    // -- tokens ------------------------------------------------------------

    /// Issue an access token (3600 s) for a grant.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures; the token minting itself.
    pub fn access_issue(
        &self,
        grant: &Grant,
        scopes: &[String],
        audience: &str,
        family: &str,
        now: i64,
    ) -> Result<(String, i64)> {
        let token = mint::opaque_token("eng_at_").map_err(|e| db(e.to_string()))?;
        let expires = now + 3600;
        self.token_insert(
            &token, "access", grant, scopes, audience, family, now, expires,
        )?;
        Ok((token, expires))
    }

    /// Issue a refresh token: 30 days, rolling.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn refresh_issue(
        &self,
        grant: &Grant,
        scopes: &[String],
        audience: &str,
        family: &str,
        now: i64,
    ) -> Result<(String, i64)> {
        let token = mint::opaque_token("eng_rt_").map_err(|e| db(e.to_string()))?;
        let expires = (now + 30 * 24 * 3600).min(grant.absolute_expiry);
        self.token_insert(
            &token, "refresh", grant, scopes, audience, family, now, expires,
        )?;
        Ok((token, expires))
    }

    // one insert, all its parts: the count is the API's convenience
    #[allow(clippy::too_many_arguments)]
    fn token_insert(
        &self,
        token: &str,
        kind: &str,
        grant: &Grant,
        scopes: &[String],
        audience: &str,
        family: &str,
        now: i64,
        expires: i64,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO tokens (id, kind, token_hash, grant_id, scopes, audience, family,
                                 issued_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                engramma_core::id::new_ulid()
                    .map(|u| u.to_string())
                    .map_err(|e| db(e.to_string()))?,
                kind,
                mint::hash_hex(token),
                grant.id,
                serde_json::to_string(scopes).unwrap_or_default(),
                audience,
                family,
                now,
                expires
            ],
        )
        .map_err(db)?;
        Ok(())
    }

    /// Validate an access token: unknown, expired, revoked, wrong audience
    /// or valid with its grant.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn access_check(&self, token: &str, audience: &str, now: i64) -> Result<TokenCheck> {
        let hash = mint::hash_hex(token);
        let conn = self.lock()?;
        let row: Option<AccessRaw> = conn
            .query_row(
                "SELECT revoked, expires_at, grant_id, scopes, audience
                 FROM tokens WHERE token_hash = ?1 AND kind = 'access'",
                [hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(db)?;
        drop(conn);
        let Some((revoked, expires_at, grant_id, scopes, token_audience)) = row else {
            return Ok(TokenCheck::Unknown);
        };
        if token_audience != audience {
            return Ok(TokenCheck::WrongAudience);
        }
        if revoked {
            return Ok(TokenCheck::Revoked);
        }
        if expires_at <= now {
            return Ok(TokenCheck::Expired);
        }
        let Some(grant) = self.grant(&grant_id, now)? else {
            return Ok(TokenCheck::Revoked);
        };
        let _ = self.touch_grant(&grant_id, now);
        Ok(TokenCheck::Valid {
            grant,
            scopes: serde_json::from_str(&scopes).unwrap_or_default(),
        })
    }

    /// Rotate a refresh token. A reused (already rotated) token revokes the
    /// whole family — the grant included — before answering `invalid_grant`.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn refresh_rotate(
        &self,
        refresh: &str,
        scopes_wanted: &[String],
        audience: &str,
        now: i64,
    ) -> Result<std::result::Result<Rotation, RefreshUse>> {
        let hash = mint::hash_hex(refresh);
        let row: Option<RefreshRaw> = {
            let conn = self.lock()?;
            conn.query_row(
                "SELECT revoked, expires_at, grant_id, scopes, audience, family
                 FROM tokens WHERE token_hash = ?1 AND kind = 'refresh'",
                [hash.as_str()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(db)?
        };
        let Some((revoked, expires_at, grant_id, scopes, token_audience, family)) = row else {
            return Ok(Err(RefreshUse::Unknown));
        };
        if token_audience != audience {
            return Ok(Err(RefreshUse::Unknown));
        }
        if revoked {
            // REUSE: the rotated token came back. OAuth 2.1: assume theft,
            // kill everything this chain ever issued.
            self.revoke_family(&family, now)?;
            self.event(now, "refresh_reused", &format!("grant={}", grant_id));
            return Ok(Err(RefreshUse::Reused));
        }
        if expires_at <= now {
            return Ok(Err(RefreshUse::Expired));
        }
        let Some(grant) = self.grant(&grant_id, now)? else {
            return Ok(Err(RefreshUse::Unknown));
        };
        // the rotation may only keep or NARROW the scopes
        let scopes: Vec<String> = if scopes_wanted.is_empty() {
            serde_json::from_str(&scopes).unwrap_or_default()
        } else {
            let current: Vec<String> = serde_json::from_str(&scopes).unwrap_or_default();
            scopes_wanted
                .iter()
                .filter(|s| current.contains(s))
                .cloned()
                .collect()
        };
        // the claim is ONE conditional step: of two concurrent uses only
        // one flips `revoked` 0 → 1; the loser is a reuse, exactly like a
        // late replay (verifica v0.5.2: the read above and this write were
        // two lock sections, and both racers rotated)
        let claimed = {
            let conn = self.lock()?;
            conn.execute(
                "UPDATE tokens SET revoked = 1, last_used_at = ?2
                 WHERE token_hash = ?1 AND revoked = 0",
                rusqlite::params![hash, now],
            )
            .map_err(db)?
        };
        if claimed == 0 {
            self.revoke_family(&family, now)?;
            self.event(now, "refresh_reused", &format!("grant={grant_id}"));
            return Ok(Err(RefreshUse::Reused));
        }
        let (access, access_exp) = self.access_issue(&grant, &scopes, audience, &family, now)?;
        let (new_refresh, refresh_exp) =
            self.refresh_issue(&grant, &scopes, audience, &family, now)?;
        Ok(Ok(Rotation {
            grant,
            scopes,
            access,
            access_exp,
            refresh: new_refresh,
            refresh_exp,
        }))
    }

    /// Revoke one token by its string (RFC 7009: unknown is not an error).
    /// v0.5.3 (P3.4): revoking a REFRESH takes the whole family with it —
    /// every access token born from the same authorization code (RFC 7009
    /// § 2.1); revoking an ACCESS touches that token alone, and the client
    /// keeps refreshing.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn token_revoke(&self, token: &str, now: i64) -> Result<()> {
        let hash = mint::hash_hex(token);
        let found: Option<(String, String)> = {
            let conn = self.lock()?;
            conn.query_row(
                "SELECT kind, family FROM tokens WHERE token_hash = ?1",
                [hash.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db)?
        };
        if let Some((kind, family)) = found {
            let n = if kind == "refresh" {
                let conn = self.lock()?;
                conn.execute(
                    "UPDATE tokens SET revoked = 1 WHERE family = ?1",
                    [family.as_str()],
                )
                .map_err(db)?
            } else {
                let conn = self.lock()?;
                conn.execute(
                    "UPDATE tokens SET revoked = 1 WHERE token_hash = ?1",
                    [hash.as_str()],
                )
                .map_err(db)?
            };
            let what = if kind == "refresh" {
                format!("by /revoke: refresh → family ({n} token)")
            } else {
                "by /revoke: access (solo lui)".to_string()
            };
            self.event(now, "token_revoked", &what);
        }
        Ok(())
    }

    /// Revoke every token of a family and its grant.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn revoke_family(&self, family: &str, now: i64) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("UPDATE tokens SET revoked = 1 WHERE family = ?1", [family])
            .map_err(db)?;
        conn.execute(
            "UPDATE grants SET revoked_at = ?2 WHERE id IN
               (SELECT DISTINCT grant_id FROM tokens WHERE family = ?1)",
            rusqlite::params![family, now],
        )
        .map_err(db)?;
        Ok(())
    }

    // -- grants ------------------------------------------------------------

    /// One grant, when it lives and is not revoked.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn grant(&self, id: &str, now: i64) -> Result<Option<Grant>> {
        let conn = self.lock()?;
        let row: Option<GrantRaw> = conn
            .query_row(
                "SELECT id, label, client_id, cells, scopes, created_at, last_used_at, revoked_at, absolute_expiry
                 FROM grants WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
            )
            .optional()
            .map_err(db)?;
        drop(conn);
        let Some((
            id,
            label,
            client_id,
            cells,
            scopes,
            created_at,
            last_used_at,
            revoked_at,
            absolute_expiry,
        )) = row
        else {
            return Ok(None);
        };
        if revoked_at.is_some() || absolute_expiry <= now {
            return Ok(None);
        }
        Ok(Some(Grant {
            id,
            label,
            client_id,
            cells: serde_json::from_str(&cells).unwrap_or_default(),
            scopes: serde_json::from_str(&scopes).unwrap_or_default(),
            created_at,
            last_used_at,
            revoked_at,
            absolute_expiry,
        }))
    }

    /// Every grant (alive or not) with its scopes, for the CLI.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn grants(&self) -> Result<Vec<Grant>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, label, client_id, cells, scopes, created_at, last_used_at, revoked_at,
                        absolute_expiry FROM grants ORDER BY created_at",
            )
            .map_err(db)?;
        let raw = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, i64>(8)?,
                ))
            })
            .map_err(db)?
            .flatten()
            .collect::<Vec<_>>();
        drop(stmt);
        Ok(raw
            .into_iter()
            .map(
                |(
                    id,
                    label,
                    client_id,
                    cells,
                    scopes,
                    created_at,
                    last_used_at,
                    revoked_at,
                    absolute_expiry,
                )| Grant {
                    id,
                    label,
                    client_id,
                    cells: serde_json::from_str(&cells).unwrap_or_default(),
                    scopes: serde_json::from_str(&scopes).unwrap_or_default(),
                    created_at,
                    last_used_at,
                    revoked_at,
                    absolute_expiry,
                },
            )
            .collect())
    }

    /// Revoke a grant (or all): every token of it dies at the NEXT request.
    /// # Errors
    /// [`StoreError::Db`] on SQL failures.
    pub fn grant_revoke(&self, id: Option<&str>, now: i64) -> Result<usize> {
        let conn = self.lock()?;
        let n = match id {
            Some(id) => conn
                .execute(
                    "UPDATE grants SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                    rusqlite::params![id, now],
                )
                .map_err(db)?,
            None => conn
                .execute(
                    "UPDATE grants SET revoked_at = ?1 WHERE revoked_at IS NULL",
                    [now],
                )
                .map_err(db)?,
        };
        drop(conn);
        self.event(now, "grant_revoked", id.unwrap_or("*"));
        Ok(n)
    }

    fn touch_grant(&self, id: &str, now: i64) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE grants SET last_used_at = ?2 WHERE id = ?1",
            rusqlite::params![id, now],
        )
        .map_err(db)?;
        Ok(())
    }
}

/// What a rotated refresh gives back.
#[derive(Debug, Clone)]
pub struct Rotation {
    pub grant: Grant,
    pub scopes: Vec<String>,
    pub access: String,
    pub access_exp: i64,
    pub refresh: String,
    pub refresh_exp: i64,
}

/// Why a code did not pass.
#[derive(Debug, Clone)]
pub enum CodeUse {
    /// Already consumed once: the caller revokes the family.
    Reused {
        family: String,
    },
    Unknown,
    Expired,
}

/// Why a refresh did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshUse {
    /// Already rotated once: the family (and grant) are gone.
    Reused,
    Unknown,
    Expired,
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn now_minus(secs: i64) -> i64 {
    now() - secs
}

/// The host of a client_id URL, for the event log (never a secret).
fn host_of(client_id: &str) -> &str {
    client_id
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(client_id)
        .split(['/', ':'])
        .next()
        .unwrap_or(client_id)
}

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE clients (
  client_id    TEXT PRIMARY KEY,
  kind         TEXT NOT NULL CHECK (kind IN ('cimd', 'dcr')),
  redirect_uris TEXT NOT NULL,
  name         TEXT,
  created_at   INTEGER NOT NULL,
  cache_until  INTEGER,
  used_at      INTEGER
);
CREATE TABLE requests (
  handle        TEXT PRIMARY KEY,
  approval_code TEXT NOT NULL UNIQUE,
  client_id     TEXT NOT NULL,
  client_name   TEXT,
  redirect_uri  TEXT NOT NULL,
  scope         TEXT NOT NULL,
  resource      TEXT,
  code_challenge TEXT NOT NULL,
  state         TEXT,
  offered_cells TEXT NOT NULL,
  offered_write TEXT NOT NULL,
  status        TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'denied')),
  grant_id      TEXT,
  created_at    INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL
);
CREATE TABLE codes (
  code_hash     TEXT PRIMARY KEY,
  handle        TEXT NOT NULL,
  client_id     TEXT NOT NULL,
  redirect_uri  TEXT NOT NULL,
  scope         TEXT NOT NULL,
  resource      TEXT,
  code_challenge TEXT NOT NULL,
  grant_id      TEXT NOT NULL,
  family        TEXT NOT NULL,
  issued_at     INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL,
  used          INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE grants (
  id          TEXT PRIMARY KEY,
  -- unique among LIVE grants only (the index below): a revoked or
  -- expired connection gives its label back for the reconnection
  label       TEXT NOT NULL,
  client_id   TEXT NOT NULL,
  cells       TEXT NOT NULL,
  scopes      TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at  INTEGER,
  absolute_expiry INTEGER NOT NULL
);
CREATE UNIQUE INDEX grants_live_label ON grants(label) WHERE revoked_at IS NULL;
CREATE TABLE tokens (
  id         TEXT PRIMARY KEY,
  kind       TEXT NOT NULL CHECK (kind IN ('access', 'refresh')),
  token_hash TEXT NOT NULL UNIQUE,
  grant_id   TEXT NOT NULL,
  scopes     TEXT NOT NULL,
  audience   TEXT NOT NULL,
  family     TEXT NOT NULL,
  issued_at  INTEGER NOT NULL,
  expires_at  INTEGER NOT NULL,
  revoked    INTEGER NOT NULL DEFAULT 0,
  last_used_at INTEGER
);
CREATE TABLE events (
  seq    INTEGER PRIMARY KEY AUTOINCREMENT,
  at     INTEGER NOT NULL,
  kind   TEXT NOT NULL,
  detail TEXT NOT NULL
);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, OAuthStore) {
        let dir = tempfile::tempdir().unwrap(/* test-only */);
        let s = OAuthStore::open(&dir.path().join("oauth.db")).unwrap(/* test-only */);
        (dir, s)
    }

    #[test]
    fn the_file_is_created_with_tight_permissions() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let (d, _s) = store();
            let mode = std::fs::metadata(d.path().join("oauth.db"))
                .unwrap(/* test-only */)
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "credentials file");
        }
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let (d, s) = store();
        drop(s);
        let conn = rusqlite::Connection::open(d.path().join("oauth.db")).unwrap(/* test-only */);
        conn.execute(
            "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
            [],
        )
        .unwrap(/* test-only */);
        drop(conn);
        match OAuthStore::open(&d.path().join("oauth.db")) {
            Err(StoreError::SchemaTooNew(99)) => {}
            Err(other) => panic!("expected SchemaTooNew, got {other}"),
            Ok(_) => panic!("expected SchemaTooNew, got an open store"),
        }
    }

    #[test]
    fn a_code_is_one_shot_and_its_reuse_is_reported() {
        let (_d, s) = store();
        let now = 1_000_i64;
        s.request_insert(&AuthRequest {
            handle: "h1".into(),
            approval_code: "ABCD-EFGH".into(),
            client_id: "https://chatgpt.com/oauth/client.json".into(),
            client_name: Some("ChatGPT".into()),
            redirect_uri: "https://chatgpt.com/connector_platform_oauth_redirect".into(),
            scopes: vec![SCOPE_READ.into()],
            resource: None,
            code_challenge: "challenge".into(),
            state: Some("st".into()),
            offered_cells: vec!["default".into()],
            offered_write: vec![],
            status: RequestStatus::Pending,
            grant_id: None,
            created_at: now,
            expires_at: now + 600,
        })
        .unwrap(/* t */);
        s.request_approve(
            "ABCD-EFGH",
            "prova",
            &["default".to_string()],
            &[SCOPE_READ.to_string()],
            now,
        )
        .unwrap(/* t */);
        let (code, _rec, _exp) =
            s.code_mint("h1", "https://mem.example/mcp", now).unwrap(/* t */).expect("minted");
        let first = s.code_consume(&code, now).unwrap(/* t */);
        assert!(first.is_ok());
        let second = s.code_consume(&code, now).unwrap(/* t */);
        assert!(
            matches!(second, Err(CodeUse::Reused { .. })),
            "the second use names the theft"
        );
    }

    fn seed_client_grant(s: &OAuthStore, client_id: &str, now: i64) {
        let conn = s.conn.lock().unwrap(/* test-only */);
        conn.execute(
            "INSERT INTO grants (id, label, client_id, cells, scopes, created_at, absolute_expiry)
             VALUES ('g-usato', 'usato', ?1, '[]', '[\"memory:read\"]', ?2, ?3)",
            rusqlite::params![client_id, now, now + 90 * 24 * 3600],
        )
        .unwrap(/* test-only */);
        drop(conn);
    }

    fn seed_grant(s: &OAuthStore, id: &str, label: &str, now: i64) -> Grant {
        let grant = Grant {
            id: id.into(),
            label: label.into(),
            client_id: "https://claude.ai".into(),
            cells: vec![],
            scopes: vec![SCOPE_READ.into()],
            created_at: now,
            last_used_at: None,
            revoked_at: None,
            absolute_expiry: now + 90 * 24 * 3600,
        };
        let conn = s.conn.lock().unwrap(/* test-only */);
        conn.execute(
            "INSERT INTO grants (id, label, client_id, cells, scopes, created_at, absolute_expiry)
             VALUES (?1, ?2, ?3, '[]', ?4, ?5, ?6)",
            rusqlite::params![
                grant.id,
                grant.label,
                grant.client_id,
                serde_json::to_string(&grant.scopes).unwrap_or_default(),
                now,
                grant.absolute_expiry
            ],
        )
        .unwrap(/* test-only */);
        drop(conn);
        grant
    }

    #[test]
    fn a_reused_refresh_kills_the_whole_grant() {
        let (_d, s) = store();
        let now = 1_000_i64;
        let grant = seed_grant(&s, "g1", "prova", now);
        let (rt, _) = s.refresh_issue(&grant, &[SCOPE_READ.into()], "https://mem.example", "fam1", now)
            .unwrap(/* t */);
        let first = s
            .refresh_rotate(&rt, &[], "https://mem.example", now)
            .unwrap(/* t */);
        assert!(first.is_ok());
        // the OLD refresh again: theft
        let second = s
            .refresh_rotate(&rt, &[], "https://mem.example", now)
            .unwrap(/* t */);
        assert!(matches!(second, Err(RefreshUse::Reused)));
        // the NEW access token from the rotation is dead too
        let rotation = first.unwrap(/* t */);
        assert_eq!(
            s.access_check(&rotation.access, "https://mem.example", now)
                .unwrap(/* t */),
            TokenCheck::Revoked
        );
        // and the grant
        assert!(s.grant("g1", now).unwrap(/* t */).is_none());
    }

    #[test]
    fn parallel_rotations_of_one_refresh_cannot_both_win() {
        // verifica v0.5.2: the read and the «mark rotated» were two lock
        // sections — two concurrent refreshes both saw `revoked = 0` and
        // both rotated, forking the chain with no theft alarm
        for round in 0..20 {
            let (_d, s) = store();
            let s = std::sync::Arc::new(s);
            let now = 1_000_i64;
            let grant = seed_grant(&s, "g1", "prova", now);
            let (rt, _) = s
                .refresh_issue(&grant, &[SCOPE_READ.into()], "https://mem.example", "fam1", now)
                .unwrap(/* t */);
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let s = std::sync::Arc::clone(&s);
                    let b = std::sync::Arc::clone(&barrier);
                    let rt = rt.clone();
                    std::thread::spawn(move || {
                        b.wait();
                        s.refresh_rotate(&rt, &[], "https://mem.example", now)
                            .unwrap(/* t */)
                            .is_ok()
                    })
                })
                .collect();
            let won = handles
                .into_iter()
                .map(|h| h.join().unwrap(/* t */))
                .filter(|ok| *ok)
                .count();
            assert!(
                won <= 1,
                "round {round}: {won} rotations of one refresh won"
            );
        }
    }

    #[test]
    fn a_minted_code_is_never_minted_again_even_after_it_expires() {
        // verifica v0.5.2: «already minted» looked only at LIVE codes, so a
        // refresh of the waiting page after 60 s minted a second code
        let (_d, s) = store();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        s.request_insert(&AuthRequest {
            handle: "h1".into(),
            approval_code: "ABCD-EFGH".into(),
            client_id: "https://chatgpt.com/oauth/client.json".into(),
            client_name: None,
            redirect_uri: "https://chatgpt.com/connector_platform_oauth_redirect".into(),
            scopes: vec![SCOPE_READ.into()],
            resource: None,
            code_challenge: "challenge".into(),
            state: None,
            offered_cells: vec!["default".into()],
            offered_write: vec![],
            status: RequestStatus::Pending,
            grant_id: None,
            created_at: now - 120,
            expires_at: now + 480,
        })
        .unwrap(/* t */);
        s.request_approve(
            "ABCD-EFGH",
            "prova",
            &["default".to_string()],
            &[SCOPE_READ.to_string()],
            now - 120,
        )
        .unwrap(/* t */);
        // minted two minutes ago: its 60 s are over
        let minted = s
            .code_mint("h1", "https://mem.example/mcp", now - 120)
            .unwrap(/* t */);
        assert!(minted.is_some());
        assert!(
            s.code_already_minted("h1").unwrap(/* t */),
            "an expired code was still minted once"
        );
    }

    /// v0.5.3 (P3.4): revoking a REFRESH takes the whole family (every
    /// access token of the same authorization, RFC 7009 § 2.1); revoking
    /// an ACCESS touches it alone and the client keeps refreshing.
    #[test]
    fn revoke_of_a_refresh_kills_the_family_of_an_access_only_it() {
        let (_d, s) = store();
        let now = 1_000_i64;
        let grant = seed_grant(&s, "g1", "prova", now);
        let (at1, _) = s
            .access_issue(&grant, &[SCOPE_READ.into()], "https://mem.example", "fam1", now)
            .unwrap(/* t */);
        let (at2, _) = s
            .access_issue(&grant, &[SCOPE_READ.into()], "https://mem.example", "fam1", now)
            .unwrap(/* t */);
        let (rt, _) = s
            .refresh_issue(&grant, &[SCOPE_READ.into()], "https://mem.example", "fam1", now)
            .unwrap(/* t */);
        // an access token alone: only it dies, the refresh still rotates
        s.token_revoke(&at1, now).unwrap(/* t */);
        assert_eq!(
            s.access_check(&at1, "https://mem.example", now).unwrap(/* t */),
            TokenCheck::Revoked
        );
        assert!(matches!(
            s.access_check(&at2, "https://mem.example", now).unwrap(/* t */),
            TokenCheck::Valid { .. }
        ));
        assert!(s.refresh_rotate(&rt, &[], "https://mem.example", now).unwrap(/* t */).is_ok());
        // a refresh token: the whole family goes with it
        s.token_revoke(&rt, now).unwrap(/* t */);
        assert_eq!(
            s.access_check(&at2, "https://mem.example", now).unwrap(/* t */),
            TokenCheck::Revoked,
            "the access of the same family is revoked with the refresh"
        );
        assert!(matches!(
            s.refresh_rotate(&rt, &[], "https://mem.example", now).unwrap(/* t */),
            Err(RefreshUse::Reused) | Err(RefreshUse::Unknown)
        ));
    }

    /// v0.5.3 (P3.6): the DCR lifecycle, owed by the v0.5.2 REPORT — a
    /// never-used registration dies at 24 h; a used one lives exactly as
    /// long as a grant of its does. Clock injected (`now` is a parameter).
    #[test]
    fn the_dcr_client_lifecycle_is_24h_unused_and_alive_while_granted() {
        use crate::oauth::store::ClientKind;
        let (_d, s) = store();
        let t0 = 1_000_i64;
        let rec = |id: &str, created: i64| ClientRecord {
            client_id: id.into(),
            kind: ClientKind::Dcr,
            redirect_uris: vec!["https://x.example/cb".into()],
            name: Some("X".into()),
            created_at: created,
        };
        s.dcr_put(&rec("nuovo", t0)).unwrap(/* t */);
        s.dcr_put(&rec("vecchio", t0)).unwrap(/* t */);
        // fresh and never used: alive, and the first use marks it
        assert!(s.client_get("nuovo", t0 + 100).unwrap(/* t */).is_some());
        // never used and older than a day: gone
        assert!(s.client_get("vecchio", t0 + 24 * 3600 + 1).unwrap(/* t */).is_none());
        // a used one WITH a live grant: alive until the grant dies
        s.dcr_put(&rec("usato", t0)).unwrap(/* t */);
        let _ = s.client_get("usato", t0 + 100).unwrap(/* t */).expect("first use");
        seed_client_grant(&s, "usato", t0 + 200);
        assert!(
            s.client_get("usato", t0 + 3 * 24 * 3600).unwrap(/* t */).is_some(),
            "a used client outlives the 24 h while its grant lives"
        );
        // the grant dies (revoked): the client goes with it
        s.grant_revoke(Some("g-usato"), t0 + 4 * 24 * 3600).unwrap(/* t */);
        assert!(
            s.client_get("usato", t0 + 4 * 24 * 3600).unwrap(/* t */).is_none(),
            "no live grant left: the registration is gone"
        );
    }

    /// v0.5.3 (P3.1): the negative CIMD cache and the pending ceilings.
    #[test]
    fn the_negative_cimd_cache_and_the_pending_ceilings() {
        let (_d, s) = store();
        let t0 = 1_000_i64;
        assert!(s.cimd_refused_get("https://x.example/c", t0).unwrap(/* t */).is_none());
        s.cimd_refused_put("https://x.example/c", t0 + 300, "fetch: dns").unwrap(/* t */);
        assert_eq!(
            s.cimd_refused_get("https://x.example/c", t0 + 299).unwrap(/* t */).as_deref(),
            Some("fetch: dns")
        );
        assert!(
            s.cimd_refused_get("https://x.example/c", t0 + 301).unwrap(/* t */).is_none(),
            "five minutes, then it may be tried again"
        );
        // the pending ceilings: 2 per client, 10 in all
        for i in 0..12 {
            let client = if i < 4 {
                "https://a.example/c"
            } else {
                "https://b.example/c"
            };
            s.request_insert(&AuthRequest {
                handle: format!("h{i}"),
                approval_code: format!("AAAA-{i:04}"),
                client_id: client.into(),
                client_name: None,
                redirect_uri: "https://x.example/cb".into(),
                scopes: vec![SCOPE_READ.into()],
                resource: None,
                code_challenge: "c".into(),
                state: None,
                offered_cells: vec![],
                offered_write: vec![],
                status: RequestStatus::Pending,
                grant_id: None,
                created_at: t0,
                expires_at: t0 + 600,
            })
            .unwrap(/* t */);
        }
        assert_eq!(
            s.pending_count_of("https://a.example/c", t0).unwrap(/* t */),
            4
        );
        assert_eq!(s.pending_count(t0).unwrap(/* t */), 12);
        // deny --all clears every pending request
        assert_eq!(s.request_deny_all(t0).unwrap(/* t */), 12);
        assert_eq!(s.pending_count(t0).unwrap(/* t */), 0);
    }

    #[test]
    fn an_expired_code_answers_invalid_and_not_theft() {
        let (_d, s) = store();
        // a realistic clock: request_insert GCs long-expired codes, and
        // toy timestamps would be swept away by that very sweep
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        // two requests, two approvals, two codes: a code is one-shot, so
        // the timely use and the late one cannot be the same one
        let mut codes = Vec::new();
        for (h, c) in [("h1", "AAAA-BBBB"), ("h2", "CCCC-DDDD")] {
            s.request_insert(&AuthRequest {
                handle: h.into(),
                approval_code: c.into(),
                client_id: "https://chatgpt.com/oauth/client.json".into(),
                client_name: None,
                redirect_uri: "https://chatgpt.com/connector_platform_oauth_redirect".into(),
                scopes: vec![SCOPE_READ.into()],
                resource: None,
                code_challenge: "challenge".into(),
                state: None,
                offered_cells: vec!["default".into()],
                offered_write: vec![],
                status: RequestStatus::Pending,
                grant_id: None,
                created_at: now,
                expires_at: now + 600,
            })
            .unwrap(/* t */);
            s.request_approve(
                c,
                &format!("prova-{h}"),
                &["default".to_string()],
                &[SCOPE_READ.to_string()],
                now,
            )
            .unwrap(/* t */)
            .expect("approved");
            let (code, _rec, _exp) = s
                .code_mint(h, "https://mem.example/mcp", now)
                .unwrap(/* t */)
                .expect("minted");
            codes.push(code);
        }
        // the late use is EXPIRED, not a theft alarm
        let got = s.code_consume(&codes[0], now + 3600).unwrap(/* t */);
        assert!(
            matches!(&got, Err(CodeUse::Expired)),
            "expected Expired, got {got:?}"
        );
        // the timely use passes
        assert!(s.code_consume(&codes[1], now + 30).unwrap(/* t */).is_ok());
    }
}
