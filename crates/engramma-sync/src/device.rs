// SPDX-License-Identifier: Apache-2.0

//! The device identity (v0.6, P2): ONE seed on disk, THREE keys derived
//! from it, never the same key for two uses. `<root>/device.key` holds a
//! 32-byte random seed with 0600 permissions; HKDF-SHA256 with distinct
//! labels derives the iroh TRANSPORT key (Ed25519), the operation SIGNING
//! key (Ed25519) and the HPKE key (X25519). The device id IS the signing
//! public key: every signature names its author by construction.
//!
//! The seed never leaves the disk. The system keychain is a follow-up
//! (DESIGN § 10).

use ed25519_dalek::SigningKey;
use hkdf::Hkdf;
use sha2::Sha256;

use crate::Error;

/// The HKDF salt: fixed, public — the labels do the separating.
const HKDF_SALT: &[u8] = b"engramma/device/v1";
/// The label of the iroh transport key (an iroh node id is an Ed25519 key).
const LABEL_TRANSPORT: &[u8] = b"engramma/device/transport/v1";
/// The label of the operation signing key.
const LABEL_SIGNING: &[u8] = b"engramma/device/signing/v1";
/// The label of the HPKE (X25519) key.
const LABEL_HPKE: &[u8] = b"engramma/device/hpke/v1";

/// One device's keys, all derived from the one seed.
pub struct DeviceKey {
    transport: SigningKey,
    signing: SigningKey,
    // the DERIVED X25519 private key (DHKEM's DeriveKeyPair over the
    // HKDF-derived secret) — NOT the raw derived secret, which is only
    // the intermediate input
    hpke_secret: [u8; 32],
}

impl DeviceKey {
    /// Load `<root>/device.key`, or create it on first need (a 32-byte
    /// random seed, 0600). The directory is created if missing.
    ///
    /// # Errors
    /// [`Error::Io`] on filesystem failures, [`Error::BadKey`] when the
    /// file exists but does not hold 32 bytes.
    pub fn load_or_create(root: &std::path::Path) -> Result<Self, Error> {
        use std::io::{Read, Write};
        let path = root.join("device.key");
        if path.exists() {
            let mut seed = [0_u8; 32];
            let mut f = std::fs::File::open(&path)?;
            f.read_exact(&mut seed)?;
            return Ok(Self::from_seed(seed));
        }
        std::fs::create_dir_all(root)?;
        let mut seed = [0_u8; 32];
        // the seed is the device's whole identity: the OS CSPRNG, no
        // counters, no time
        getrandom::fill(&mut seed).map_err(std::io::Error::other)?;
        let mut f = std::fs::File::create(&path)?;
        f.write_all(&seed)?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self::from_seed(seed))
    }

    /// Derive everything from a known seed (tests).
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let derive = |label: &[u8]| -> [u8; 32] {
            let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), &seed);
            let mut okm = [0_u8; 32];
            hk.expand(label, &mut okm).expect(
                /* label and okm are fixed sizes */ "hkdf expand cannot fail",
            );
            okm
        };
        Self {
            transport: SigningKey::from_bytes(&derive(LABEL_TRANSPORT)),
            signing: SigningKey::from_bytes(&derive(LABEL_SIGNING)),
            hpke_secret: derive(LABEL_HPKE),
        }
    }

    /// The device id: the hex encoding of the SIGNING public key.
    #[must_use]
    pub fn device_id(&self) -> String {
        hex(&self.signing.verifying_key().to_bytes())
    }

    /// The public iroh transport key (Ed25519), as 32 raw bytes.
    #[must_use]
    pub fn transport_public(&self) -> [u8; 32] {
        self.transport.verifying_key().to_bytes()
    }

    /// The public signing key (Ed25519), as 32 raw bytes — this IS the id.
    #[must_use]
    pub fn signing_public(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// The HPKE (X25519) SECRET key, as 32 raw bytes (the holder opens
    /// its wraps with this; it never leaves the device).
    #[must_use]
    pub fn hpke_secret(&self) -> [u8; 32] {
        self.hpke_secret
    }

    /// The HPKE (X25519) public key, as 32 raw bytes.
    #[must_use]
    pub fn hpke_public(&self) -> [u8; 32] {
        let sk = x25519_dalek::StaticSecret::from(self.hpke_secret);
        x25519_dalek::PublicKey::from(&sk).to_bytes()
    }

    /// Sign a message with the operation signing key.
    #[must_use]
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        use ed25519_dalek::Signer;
        self.signing.sign(msg).to_bytes().to_vec()
    }

    /// Verify a signature against a signing public key.
    ///
    /// # Errors
    /// [`Error::BadSignature`] when the signature does not verify.
    pub fn verify(public: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<(), Error> {
        use ed25519_dalek::Verifier;
        let Ok(pk) = ed25519_dalek::VerifyingKey::from_bytes(public) else {
            return Err(Error::BadSignature);
        };
        let Ok(sig) = ed25519_dalek::Signature::from_slice(sig) else {
            return Err(Error::BadSignature);
        };
        pk.verify(msg, &sig).map_err(|_| Error::BadSignature)
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The registry log's node id (a fixed, distinct name: the registry is its
/// own small log, not a cell).
#[must_use]
pub fn registry_node() -> &'static str {
    "engramma-registry"
}
