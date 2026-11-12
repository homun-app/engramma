// SPDX-License-Identifier: Apache-2.0

//! v0.5.2 (P3): the minting helpers — opaque tokens, authorization codes,
//! request handles, approval codes — and the PKCE check. Every SECRET is
//! 256 random bits (ring); every comparison of a secret goes through
//! digests, never through the strings.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SecureRandom as _;

/// The alphabet of the approval code: no ambiguous glyphs (no 0/O, 1/I/L).
pub const APPROVAL_ALPHABET: &[u8; 31] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// 32 random bytes, or an error from the system RNG (which never fails in
/// practice on our platforms; the caller decides what that means).
pub fn random_bytes() -> Result<[u8; 32], std::io::Error> {
    let mut b = [0_u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(b)
}

/// `eng_at_…` / `eng_rt_…` / `eng_code_…`: a prefix plus 256 random bits,
/// base64url. Opaque: what the token MEANS lives in `oauth.db`, keyed by
/// its hash.
pub fn opaque_token(prefix: &str) -> Result<String, std::io::Error> {
    Ok(format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(random_bytes()?)
    ))
}

/// The 256-bit request handle of the waiting page (URL-safe, hex).
pub fn handle() -> Result<String, std::io::Error> {
    Ok(random_bytes()?.iter().map(|b| format!("{b:02x}")).collect())
}

/// The 8-character approval code the human retypes on the computer where
/// engramma runs (RFC 8628 § 5.4: what the browser shows, the local
/// terminal asks for). Random over the unambiguous alphabet.
pub fn approval_code() -> Result<String, std::io::Error> {
    let bytes = random_bytes()?;
    let code: String = bytes
        .iter()
        .map(|b| APPROVAL_ALPHABET[usize::from(*b) % APPROVAL_ALPHABET.len()] as char)
        .take(8)
        .collect();
    // shown as XXXX-XXXX
    Ok(format!("{}-{}", &code[..4], &code[4..]))
}

/// What the human typed, back to the stored `XXXX-XXXX` shape: case and
/// the dash (or spaces) do not matter — the code is read off a page and
/// retyped, and it is no secret to begin with.
#[must_use]
pub fn normalize_approval_code(typed: &str) -> String {
    let bare: String = typed
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if bare.len() == 8 && bare.is_ascii() {
        format!("{}-{}", &bare[..4], &bare[4..])
    } else {
        typed.to_string()
    }
}

/// blake3 of a secret, hex — the only form ever stored or compared.
pub fn hash_hex(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

/// `BASE64URL(SHA256(verifier))`, unpadded — the S256 code challenge.
pub fn pkce_s256(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// Verify an S256 challenge against a verifier, comparing through blake3
/// digests (P0's rule: secrets never compare as raw strings). ring's own
/// `constant_time` helper is deprecated in 0.17 — and its docs say it made
/// no promises anyway — so the digest-then-compare pattern it is.
pub fn pkce_verify(challenge: &str, verifier: &str) -> bool {
    let computed = pkce_s256(verifier);
    blake3::hash(challenge.as_bytes()) == blake3::hash(computed.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_codes_come_from_the_unambiguous_alphabet() {
        for _ in 0..32 {
            let c = approval_code().unwrap(/* test-only */);
            assert_eq!(c.len(), 9, "XXXX-XXXX");
            assert_eq!(c.as_bytes()[4], b'-');
            for ch in c.chars().filter(|c| *c != '-') {
                assert!(APPROVAL_ALPHABET.contains(&(ch as u8)), "{c}");
            }
        }
    }

    #[test]
    fn tokens_are_long_opaque_and_prefixed() {
        let t = opaque_token("eng_at_").unwrap(/* test-only */);
        assert!(t.starts_with("eng_at_"));
        assert!(t.len() > 40, "256 bits of base64url: {t}");
        assert_ne!(t, opaque_token("eng_at_").unwrap(/* test-only */));
    }

    #[test]
    fn pkce_s256_matches_the_rfc_vector() {
        // RFC 7636 appendix B: verifier dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(pkce_verify(
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            verifier
        ));
        assert!(!pkce_verify(
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            "wrong"
        ));
    }
}
