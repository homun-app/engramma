// SPDX-License-Identifier: Apache-2.0

//! The pairing (v0.6, P3): two ways in, one confirmation. The TICKET
//! carries the inviter's iroh address and a 128-bit secret, base32 with a
//! version prefix, valid ten minutes and once. The SHORT CODE —
//! `7-tigre-lago`, a channel number plus two words of the fixed 2048-word
//! Italian list — meets at the blind mailbox and runs SPAKE2 (RFC 9382)
//! over the code, so the meeting point never learns it; one attempt per
//! code: a wrong guess burns the channel, and an active attacker has one
//! chance in 2048².
//!
//! Whichever way in, both sides end by printing the same SIX-DIGIT
//! verification code (SAS), derived from both transport keys and the
//! transcript, and the human confirms on both screens. Only then does the
//! `add` land.
//!
//! This module is the protocol's heart, transport-free: the iroh wiring
//! (P5) feeds bytes in and out. NOTE: the `spake2` crate has no
//! independent audit — its use is confined to this path.

use crate::words::WORDS;
use crate::{Error, Result};

/// The ticket's validity: ten minutes.
pub const TICKET_TTL_MS: u64 = 10 * 60 * 1000;

/// The ticket's version prefix.
const TICKET_PREFIX: &str = "engramma-invite-v1";

/// One invite ticket: where to find the inviter, and the shared secret.
#[derive(Debug, Clone)]
pub struct Ticket {
    /// The inviter's iroh node id (its Ed25519 transport public key), raw.
    pub node_id: [u8; 32],
    /// The relay URL of the inviter, if any.
    pub relay: String,
    /// Direct socket addresses, space-separated `ip:port`.
    pub addrs: Vec<String>,
    /// The 128-bit secret.
    pub secret: [u8; 16],
    /// When the ticket stops being valid (Unix ms).
    pub expires_at: u64,
}

impl Ticket {
    /// Mint a ticket for the next ten minutes.
    ///
    /// # Errors
    /// [`Error::Io`] when the system CSPRNG fails.
    pub fn mint(node_id: [u8; 32], relay: &str, addrs: &[String], now_ms: u64) -> Result<Self> {
        let mut secret = [0_u8; 16];
        getrandom::fill(&mut secret).map_err(std::io::Error::other)?;
        Ok(Self {
            node_id,
            relay: relay.into(),
            addrs: addrs.to_vec(),
            secret,
            expires_at: now_ms + TICKET_TTL_MS,
        })
    }

    /// The textual form: `PREFIX.base32(payload)`.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut payload = Vec::new();
        payload.extend_from_slice(&self.node_id);
        payload.extend_from_slice(&self.expires_at.to_be_bytes());
        payload.push(self.relay.len() as u8);
        payload.extend_from_slice(self.relay.as_bytes());
        payload.extend_from_slice(&(self.addrs.len() as u16).to_be_bytes());
        for a in &self.addrs {
            payload.push(a.len() as u8);
            payload.extend_from_slice(a.as_bytes());
        }
        payload.extend_from_slice(&self.secret);
        format!("{TICKET_PREFIX}.{}", base32(&payload))
    }

    /// Parse the textual form; the expiry is checked by the caller when it
    /// cares (the clock is the caller's).
    ///
    /// # Errors
    /// [`Error::Malformed`] when the ticket is not a ticket.
    pub fn decode(text: &str) -> Result<Self> {
        let rest = text
            .strip_prefix(&format!("{TICKET_PREFIX}."))
            .ok_or_else(|| Error::Malformed("not an engramma invite ticket".into()))?;
        let payload =
            base32_decode(rest).ok_or_else(|| Error::Malformed("bad base32 payload".into()))?;
        let mut p = payload.as_slice();
        fn take<'a>(p: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            if p.len() < n {
                return None;
            }
            let (head, tail) = p.split_at(n);
            *p = tail;
            Some(head)
        }
        let node_id: [u8; 32] = take(&mut p, 32)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Error::Malformed("ticket too short".into()))?;
        let expires_at = take(&mut p, 8)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_be_bytes)
            .ok_or_else(|| Error::Malformed("ticket misses expiry".into()))?;
        let rlen = take(&mut p, 1)
            .and_then(|b| b.first().copied())
            .ok_or_else(|| Error::Malformed("ticket misses relay".into()))?
            as usize;
        let relay = take(&mut p, rlen)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or_default()
            .to_string();
        let naddrs = take(&mut p, 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .ok_or_else(|| Error::Malformed("ticket misses addresses".into()))?
            as usize;
        let mut addrs = Vec::new();
        for _ in 0..naddrs {
            let alen = take(&mut p, 1)
                .and_then(|b| b.first().copied())
                .unwrap_or(0) as usize;
            let a = take(&mut p, alen)
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or_default()
                .to_string();
            addrs.push(a);
        }
        let secret: [u8; 16] = take(&mut p, 16)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Error::Malformed("ticket misses secret".into()))?;
        Ok(Self {
            node_id,
            relay,
            addrs,
            secret,
            expires_at,
        })
    }

    /// Is the ticket still valid?
    #[must_use]
    pub fn is_valid(&self, now_ms: u64) -> bool {
        now_ms < self.expires_at
    }
}

/// A short code: `7-tigre-lago`. The channel is the mailbox's meeting
/// point; the two words (11 bits each) are the SPAKE2 password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortCode {
    /// The channel number.
    pub channel: u16,
    /// The two words.
    pub words: [String; 2],
}

impl ShortCode {
    /// Generate a fresh code (random channel and words).
    ///
    /// # Errors
    /// [`Error::Io`] when the system CSPRNG fails.
    pub fn generate() -> Result<Self> {
        let mut buf = [0_u8; 6];
        getrandom::fill(&mut buf).map_err(std::io::Error::other)?;
        Ok(Self {
            channel: u16::from_be_bytes([buf[0], buf[1]]) % 9000 + 1,
            words: [
                // 11 bits per word: the top 8 of one byte and the top 3 of
                // the next, masked into the 2048-word list
                WORDS[((usize::from(buf[2]) << 3) | (usize::from(buf[3]) >> 5)) & 2047].to_string(),
                WORDS[((usize::from(buf[4]) << 3) | (usize::from(buf[5]) >> 5)) & 2047].to_string(),
            ],
        })
    }

    /// From explicit parts (tests, CLI parsing).
    #[must_use]
    pub fn from_parts(channel: u16, w1: &str, w2: &str) -> Self {
        Self {
            channel,
            words: [w1.into(), w2.into()],
        }
    }

    /// The SPAKE2 password: the whole code, normalized.
    #[must_use]
    pub fn password(&self) -> Vec<u8> {
        format!("{}-{}-{}", self.channel, self.words[0], self.words[1]).into_bytes()
    }

    /// The textual form.
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{}-{}-{}", self.channel, self.words[0], self.words[1])
    }

    /// Parse `7-tigre-lago`.
    ///
    /// # Errors
    /// [`Error::Malformed`] when the shape is wrong.
    pub fn decode(text: &str) -> Result<Self> {
        let parts: Vec<&str> = text.trim().split('-').collect();
        if parts.len() != 3 {
            return Err(Error::Malformed("a code is NUMBER-WORD-WORD".into()));
        }
        let channel: u16 = parts[0]
            .parse()
            .map_err(|_| Error::Malformed("the channel is a number".into()))?;
        let in_list = |w: &str| WORDS.contains(&w);
        if !in_list(parts[1]) || !in_list(parts[2]) {
            return Err(Error::Malformed(
                "the words must come from the pairing vocabulary".into(),
            ));
        }
        Ok(Self::from_parts(channel, parts[1], parts[2]))
    }
}

/// The SPAKE2 half of one side (RFC 9382, the Ed25519 group). The
/// identities are the protocol's own: "engramma/pair/a" and
/// "engramma/pair/b", so a PAKE exchange from any other protocol can never
/// be replayed here.
pub struct SpakeHalf {
    inner: spake2::Spake2<spake2::Ed25519Group>,
    transcript: Vec<u8>,
}

const ID_A: &[u8] = b"engramma/pair/a";
const ID_B: &[u8] = b"engramma/pair/b";

impl SpakeHalf {
    /// Start as side A (the inviter / the code's owner).
    #[must_use]
    pub fn begin_a(password: &[u8]) -> (Self, Vec<u8>) {
        let (inner, msg) = spake2::Spake2::<spake2::Ed25519Group>::start_a(
            &spake2::Password::new(password),
            &spake2::Identity::new(ID_A),
            &spake2::Identity::new(ID_B),
        );
        (
            Self {
                inner,
                transcript: msg.clone(),
            },
            msg,
        )
    }

    /// Start as side B (the joiner).
    #[must_use]
    pub fn begin_b(password: &[u8]) -> (Self, Vec<u8>) {
        let (inner, msg) = spake2::Spake2::<spake2::Ed25519Group>::start_b(
            &spake2::Password::new(password),
            &spake2::Identity::new(ID_A),
            &spake2::Identity::new(ID_B),
        );
        (
            Self {
                inner,
                transcript: msg.clone(),
            },
            msg,
        )
    }

    /// Consume the other side's message: back comes the shared secret.
    ///
    /// # Errors
    /// [`Error::Malformed`] when the other half's message does not fit the
    /// protocol (a wrong code lands here).
    pub fn finish(mut self, their_msg: &[u8]) -> Result<Vec<u8>> {
        self.transcript.extend_from_slice(their_msg);
        self.inner
            .finish(their_msg)
            .map_err(|e| Error::Malformed(format!("spake2: {e}")))
    }

    /// The bytes the SAS covers: both public messages, in this side's
    /// reading order (A sees A‖B, B sees A‖B too — the inviter starts).
    #[must_use]
    pub fn transcript(&self) -> &[u8] {
        &self.transcript
    }
}

/// The six-digit SAS: derived from the SHARED secret (which already binds
/// both transport keys through the callers' key confirmation), printed on
/// both screens, confirmed by the human.
#[must_use]
pub fn six_digits(shared: &[u8], transcript: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"engramma/sas/v1");
    hasher.update(shared);
    hasher.update(transcript);
    let digest = hasher.finalize();
    let n = u32::from_be_bytes([
        digest.as_bytes()[0],
        digest.as_bytes()[1],
        digest.as_bytes()[2],
        digest.as_bytes()[3],
    ]);
    format!("{:06}", n % 1_000_000)
}

/// The inviter's proof over the connection transcript: the ticket's
/// secret, MACed over the transcript, so the joiner authenticates the
/// inviter WITHOUT the secret ever crossing in the clear.
#[must_use]
pub fn transcript_mac(secret: &[u8; 16], transcript: &[u8]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"engramma/invite-mac/v1");
    hasher.update(secret);
    hasher.update(transcript);
    let mut out = [0_u8; 16];
    out.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    out
}

/// RFC 4648 base32, lowercase, no padding.
fn base32(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let mut buf: u64 = 0;
    let mut bits = 0_u32;
    for &b in data {
        buf = (buf << 8) | u64::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// Its inverse.
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = Vec::new();
    let mut buf: u64 = 0;
    let mut bits = 0_u32;
    for c in text.bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u64;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}
