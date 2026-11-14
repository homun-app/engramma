// SPDX-License-Identifier: Apache-2.0

//! The RELAY of every endpoint (R2, v0.6.1): one function builds all the
//! endpoints of the sync — the session road, the pairing road, the blind
//! node — in one of three ways:
//!
//! - **default**: n0's public relays (`RelayMode::Default`);
//! - **`--relay <url>`**: one relay of one's own (`RelayMode::Custom`);
//! - **`--no-relay`**: direct connections only (`RelayMode::Disabled`).
//!
//! pkarr stays OFF in all three ways (the N0 preset would publish and
//! resolve addresses on n0.computer's DNS: it would hand a third party the
//! id and the addresses; the choice is documented as off in `docs/sync.md`).
//! The dial tries the direct paths first and the relay after — that is
//! iroh's own behavior, nothing to reinvent here.
//!
//! The choice is SAVED in the root (`devices.db`, meta `relay`), so `sync
//! serve` does not ask for it at every start; a flag given explicitly wins
//! over what is saved and replaces it.

use iroh::Endpoint;

use crate::device::DeviceKey;
use crate::registry::DeviceRegistry;
use crate::{Error, Result};

/// How an endpoint reaches the world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayChoice {
    /// n0's public relays (the default of the spec).
    Default,
    /// One relay of one's own.
    Own(String),
    /// Direct connections only.
    Disabled,
}

impl RelayChoice {
    /// The saved form (`devices.db`, meta `relay`).
    #[must_use]
    pub fn as_saved(&self) -> String {
        match self {
            Self::Default => "default".into(),
            Self::Own(url) => format!("own:{url}"),
            Self::Disabled => "none".into(),
        }
    }

    /// Parse the saved form.
    #[must_use]
    pub fn parse_saved(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "none" => Some(Self::Disabled),
            other => other.strip_prefix("own:").map(|url| Self::Own(url.into())),
        }
    }

    /// The relay URL of the ticket and the address book, when there is one
    /// to name: the own relay's, or `None` for the default (each endpoint
    /// names the n0 relay it actually rides) and for no-relay.
    #[must_use]
    pub fn ticket_relay(&self) -> Option<String> {
        match self {
            Self::Own(url) => Some(url.clone()),
            _ => None,
        }
    }
}

/// Read the root's saved choice; the default when nothing is saved.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn saved_choice(root: &std::path::Path) -> Result<RelayChoice> {
    let reg = DeviceRegistry::open(root)?;
    saved_choice_of(reg.conn())
}

fn saved_choice_of(conn: &rusqlite::Connection) -> Result<RelayChoice> {
    let saved: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'relay'", [], |r| {
            r.get(0)
        })
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(saved
        .as_deref()
        .and_then(RelayChoice::parse_saved)
        .unwrap_or(RelayChoice::Default))
}

/// Save the choice in the root (an explicit flag: it wins and replaces).
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn save_choice(root: &std::path::Path, choice: &RelayChoice) -> Result<()> {
    let reg = DeviceRegistry::open(root)?;
    reg.conn().execute(
        "INSERT INTO meta (key, value) VALUES ('relay', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [choice.as_saved()],
    )?;
    Ok(())
}

/// Resolve the effective choice: an explicit flag wins over what is saved
/// (and replaces it); without a flag, the saved one; without anything, the
/// public relays of n0.
///
/// # Errors
/// [`Error::Db`] on SQL failure.
pub fn resolve(root: &std::path::Path, flag: Option<&RelayChoice>) -> Result<RelayChoice> {
    match flag {
        Some(choice) => {
            save_choice(root, choice)?;
            Ok(choice.clone())
        }
        None => saved_choice(root),
    }
}

/// The one endpoint builder of the sync road (R2): every endpoint — the
/// session, the pairing, the blind node — comes from here, with the same
/// transport key, its own ALPN and the same three relay ways. pkarr stays
/// off in all of them.
///
/// # Errors
/// [`Error::Io`] when iroh cannot bind; [`Error::Malformed`] on a bad
/// own-relay URL.
pub async fn bind(
    key: &DeviceKey,
    bind_addr: &str,
    alpn: &[u8],
    choice: &RelayChoice,
) -> Result<Endpoint> {
    build_endpoint(key, bind_addr, alpn, choice, false).await
}

/// The relay tests' way: the same builder with the IP transports cleared —
/// only the relay can carry the traffic, which is how a test proves the
/// session really rode it.
///
/// # Errors
/// [`Error::Io`] when iroh cannot bind; [`Error::Malformed`] on a bad
/// own-relay URL.
pub async fn bind_relay_only(
    key: &DeviceKey,
    bind_addr: &str,
    alpn: &[u8],
    choice: &RelayChoice,
) -> Result<Endpoint> {
    build_endpoint(key, bind_addr, alpn, choice, true).await
}

async fn build_endpoint(
    key: &DeviceKey,
    bind_addr: &str,
    alpn: &[u8],
    choice: &RelayChoice,
    no_ip: bool,
) -> Result<Endpoint> {
    let secret = iroh::SecretKey::from_bytes(&key.transport_seed());
    // presets::Minimal, not N0: the N0 family wires pkarr's DNS publish
    // and resolve — off in every way, as the threat model promises
    let mut builder = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(secret)
        .alpns(vec![alpn.to_vec()])
        .bind_addr(bind_addr)
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    let relay_mode = match choice {
        RelayChoice::Default => iroh::RelayMode::Default,
        RelayChoice::Disabled => iroh::RelayMode::Disabled,
        RelayChoice::Own(url) => {
            let parsed: iroh::RelayUrl = url
                .parse()
                .map_err(|e| Error::Malformed(format!("relay non valido `{url}`: {e}")))?;
            iroh::RelayMode::Custom(iroh::RelayMap::from_iter([parsed]))
        }
    };
    builder = builder.relay_mode(relay_mode);
    if no_ip {
        builder = builder.clear_ip_transports();
    }
    builder = test_tls(builder);
    builder
        .bind()
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))
}

/// In test builds the relay is iroh's local one, whose certificate is
/// self-signed: the endpoint must trust it (iroh gates the insecure
/// verifier behind its `test-utils` feature, which this crate forwards).
#[cfg(feature = "test-utils")]
fn test_tls(builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
    builder.ca_tls_config(iroh::tls::CaTlsConfig::insecure_skip_verify())
}

#[cfg(not(feature = "test-utils"))]
fn test_tls(builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
    builder
}
