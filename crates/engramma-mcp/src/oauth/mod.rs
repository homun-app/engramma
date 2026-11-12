// SPDX-License-Identifier: Apache-2.0

//! v0.5.2 (P2/P3): the OAuth 2.1 machinery of `engramma mcp --http`.
//!
//! The MCP server is a resource server (RFC 9728 metadata, bearer tokens
//! bound to one audience) and engramma itself is the authorization server:
//! a public client with PKCE S256, clients discovered through their Client
//! ID Metadata Document (or, with `--allow-dcr`, RFC 7591), and — the
//! point of the whole design — NO grant exists until a human approves it
//! on the computer where engramma runs, by retyping the code the user's
//! own browser shows (the device-flow pattern, RFC 8628 § 5.4). Nothing
//! guessable protects anything: what protects the memory is that the
//! approval surface is local.

pub mod cimd;
pub mod mint;
pub mod net;
pub mod store;

pub use cimd::{CimdError, CimdFetch, fetch};
pub use mint::{
    APPROVAL_ALPHABET, approval_code, handle, hash_hex, opaque_token, pkce_s256, pkce_verify,
};
pub use net::{RedirectUri, is_public_ip, parse_redirect_uri, redirect_matches, redirect_uri_ok};
pub use store::{
    AuthRequest, ClientKind, ClientRecord, CodeRecord, CodeUse, Grant, OAuthStore, RefreshUse,
    RequestStatus, Rotation, SCHEMA_VERSION, SCOPE_PROPOSE, SCOPE_READ, StoreError, TokenCheck,
};
